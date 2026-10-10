//! API traffic log pipeline: the HTTP middleware pushes one
//! [`ApiTrafficEvent`] per request into a bounded channel (never awaiting),
//! `api_traffic_writer_loop` batch-inserts them, and
//! `api_traffic_purge_loop` enforces the admin-configured retention.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use config::{
    API_TRAFFIC_BATCH_MAX, API_TRAFFIC_FLUSH_INTERVAL_MS, API_TRAFFIC_PURGE_BATCH,
    API_TRAFFIC_PURGE_INTERVAL_SECS,
};
use db::{ApiTrafficEvent, Db};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crate::AppState;

/// Warn once per this many dropped events / failed flushes to avoid log
/// storms when Postgres is down or the queue is saturated.
const WARN_EVERY: u64 = 1_000;

/// Cheap-to-clone producer handle held in [`AppState`]. `record` never
/// blocks: when the queue is full (or the writer is gone) the event is
/// dropped and counted.
#[derive(Clone)]
pub struct TrafficRecorder {
    tx: mpsc::Sender<ApiTrafficEvent>,
    dropped: Arc<AtomicU64>,
}

impl TrafficRecorder {
    pub fn channel(capacity: usize) -> (Self, mpsc::Receiver<ApiTrafficEvent>) {
        let (tx, rx) = mpsc::channel(capacity.max(1));
        (
            Self {
                tx,
                dropped: Arc::new(AtomicU64::new(0)),
            },
            rx,
        )
    }

    pub fn record(&self, event: ApiTrafficEvent) {
        if let Err(err) = self.tx.try_send(event) {
            let dropped = self.dropped.fetch_add(1, Ordering::Relaxed) + 1;
            if dropped == 1 || dropped.is_multiple_of(WARN_EVERY) {
                warn!(
                    "API traffic event dropped ({dropped} total): {}",
                    match err {
                        mpsc::error::TrySendError::Full(_) => "queue full",
                        mpsc::error::TrySendError::Closed(_) => "writer stopped",
                    }
                );
            }
        }
    }

    /// Events dropped since boot (queue full / writer stopped).
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

/// Drain the channel into Postgres: flush every
/// `API_TRAFFIC_FLUSH_INTERVAL_MS` or as soon as `API_TRAFFIC_BATCH_MAX`
/// events are buffered. A failed insert is logged (rate-limited) and its
/// batch discarded — the traffic log must never back-pressure requests.
pub async fn api_traffic_writer_loop(db: Db, mut rx: mpsc::Receiver<ApiTrafficEvent>) {
    let mut ticker = tokio::time::interval(Duration::from_millis(API_TRAFFIC_FLUSH_INTERVAL_MS));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut batch: Vec<ApiTrafficEvent> = Vec::with_capacity(API_TRAFFIC_BATCH_MAX);
    let mut failures: u64 = 0;

    loop {
        tokio::select! {
            received = rx.recv() => match received {
                Some(event) => {
                    batch.push(event);
                    if batch.len() >= API_TRAFFIC_BATCH_MAX {
                        flush(&db, &mut batch, &mut failures).await;
                    }
                }
                None => {
                    flush(&db, &mut batch, &mut failures).await;
                    info!("API traffic writer stopped (all producers dropped)");
                    return;
                }
            },
            _ = ticker.tick() => {
                if !batch.is_empty() {
                    flush(&db, &mut batch, &mut failures).await;
                }
            }
        }
    }
}

async fn flush(db: &Db, batch: &mut Vec<ApiTrafficEvent>, failures: &mut u64) {
    if batch.is_empty() {
        return;
    }
    match db.insert_api_requests(batch).await {
        Ok(_) => {
            if *failures > 0 {
                info!("API traffic writer recovered after {failures} failed flush(es)");
                *failures = 0;
            }
        }
        Err(err) => {
            *failures += 1;
            if *failures == 1 || failures.is_multiple_of(WARN_EVERY) {
                warn!(
                    "Failed to persist {} API traffic event(s) (failure #{failures}, batch discarded): {err:#}",
                    batch.len()
                );
            }
        }
    }
    batch.clear();
}

/// Hourly retention sweep, first run at boot. Re-reads the admin setting on
/// every tick so a change applies without a restart; an indefinite retention
/// skips the sweep entirely.
pub async fn api_traffic_purge_loop(state: AppState) {
    let mut interval = tokio::time::interval(Duration::from_secs(API_TRAFFIC_PURGE_INTERVAL_SECS));
    loop {
        interval.tick().await;
        let days = match state.db.api_traffic_retention_days().await {
            Ok(Some(days)) => days,
            Ok(None) => {
                debug!("API traffic retention is indefinite — purge skipped");
                continue;
            }
            Err(err) => {
                error!("Could not read API traffic retention: {err:#}");
                continue;
            }
        };

        let mut purged: u64 = 0;
        loop {
            match state.db.purge_api_requests(days, API_TRAFFIC_PURGE_BATCH).await {
                Ok(0) => break,
                Ok(n) => purged += n,
                Err(err) => {
                    error!("API traffic purge error: {err:#}");
                    break;
                }
            }
        }
        if purged > 0 {
            info!("Purged {purged} API request(s) older than {days} day(s)");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use domain::ApiSurface;

    fn event() -> ApiTrafficEvent {
        ApiTrafficEvent {
            surface: ApiSurface::Merchant,
            method: "GET".into(),
            route: "/api/v1/balances".into(),
            status: 200,
            duration_ms: 1.5,
            api_key_id: None,
            staff_id: None,
            ip: None,
            created_at: Utc::now(),
        }
    }

    #[test]
    fn drops_when_queue_is_full_and_counts() {
        let (recorder, mut rx) = TrafficRecorder::channel(2);
        recorder.record(event());
        recorder.record(event());
        recorder.record(event());
        assert_eq!(recorder.dropped(), 1);
        assert!(rx.try_recv().is_ok());
        assert!(rx.try_recv().is_ok());
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn drops_when_writer_is_gone() {
        let (recorder, rx) = TrafficRecorder::channel(8);
        drop(rx);
        recorder.record(event());
        assert_eq!(recorder.dropped(), 1);
    }

    #[test]
    fn dropped_counter_is_shared_across_clones() {
        let (recorder, _rx) = TrafficRecorder::channel(1);
        let other = recorder.clone();
        recorder.record(event());
        other.record(event());
        assert_eq!(recorder.dropped(), 1);
        assert_eq!(other.dropped(), 1);
    }
}
