use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use chain::ChainClients;
use config::{
    AppConfig, ADDRESS_REQUEST_DELAY_MS, API_KEY_EXPIRY_SWEEP_INTERVAL_SECS,
    WALLET_BALANCE_SYNC_INTERVAL_SECS, WEBHOOK_MAX_ATTEMPTS, WEBHOOK_RETRY_DELAYS_SECS,
};
use db::Db;
use db::DepositAddressRow;
use domain::{
    format_amount, raw_to_decimal, raw_to_decimal_string, required_confirmations,
    DetectedTransfer, DepositAddressStatus, DepositStatus, Network, Token, WebhookDeliveryStatus,
    WebhookEventType, is_amount_within_tolerance,
};
use lightning_client::{LightningClient, ReceivedPayment};
use redis::aio::ConnectionManager;
use redis::AsyncCommands;
use serde::{Deserialize, Serialize};
use tracing::{debug, error, info, warn};
use wallet::WalletService;

pub mod traffic;
pub use traffic::TrafficRecorder;

/// Legacy LIST queue, only drained at startup (pre-backoff deploys).
const LEGACY_WEBHOOK_LIST_KEY: &str = "queue:webhook-delivery";
/// ZSET scored by next-attempt epoch seconds. New suffix on purpose: a Redis
/// key cannot change type in place (WRONGTYPE against the old LIST).
const WEBHOOK_ZSET_KEY: &str = "zset:webhook-delivery";
const WALLET_BALANCES_KEY: &str = "wallet-balances";

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<AppConfig>,
    pub db: Db,
    pub wallet: WalletService,
    pub chain: ChainClients,
    pub redis: ConnectionManager,
    /// Non-blocking producer side of the API traffic log.
    pub traffic: TrafficRecorder,
    /// Breez SDK client; `None` = Lightning disabled (no key, or connect
    /// failed at boot).
    pub lightning: Option<LightningClient>,
}

impl AppState {
    pub async fn redis_key(&self, suffix: &str) -> String {
        self.config.redis_key(suffix)
    }

    /// Lightning is served by the Breez client, every other network by a
    /// chain provider.
    pub fn is_network_enabled(&self, network: Network) -> bool {
        match network {
            Network::Lightning => self.lightning.is_some(),
            _ => self.chain.is_network_available(network),
        }
    }

    pub fn network_disabled_message(&self, network: Network) -> &'static str {
        match network {
            Network::Lightning if self.config.breez_api_key.is_some() => {
                "Breez SDK not connected"
            }
            _ => self.chain.missing_key_message(network),
        }
    }
}

pub async fn process_detected_transfer(state: &AppState, transfer: DetectedTransfer) -> Result<()> {
    let address = match state.db.find_deposit_address(&transfer.deposit_address_id).await? {
        Some(a) => a,
        None => return Ok(()),
    };

    if address.status != DepositAddressStatus::Pending.as_db() {
        return Ok(());
    }

    let network = Network::from_db(&address.network)?;
    let token = Token::from_db(&address.token)?;
    let required = required_confirmations(
        network,
        state.config.tron_confirmations,
        state.config.eth_confirmations,
        state.config.btc_confirmations,
    );

    if let Some(existing) = state
        .db
        .find_deposit_by_address_and_tx(&transfer.deposit_address_id, &transfer.tx_hash)
        .await?
    {
        if existing.status == DepositStatus::Confirmed.as_db() {
            return Ok(());
        }
    }

    let amount = raw_to_decimal(&transfer.amount_raw, token)?;
    let amount_str = amount.normalize().to_string();
    let is_confirmed = transfer.confirmations >= required;

    if !is_amount_within_tolerance(
        &amount_str,
        &address.expected_amount.to_string(),
        state.config.amount_tolerance_percent,
    ) {
        warn!(
            "Deposit amount {amount_str} outside tolerance for address {}",
            address.id
        );
        return Ok(());
    }

    let status = if is_confirmed {
        DepositStatus::Confirmed
    } else {
        DepositStatus::Detected
    };

    let deposit = state
        .db
        .upsert_deposit_and_maybe_pay(
            &transfer.deposit_address_id,
            &transfer.tx_hash,
            amount,
            &transfer.amount_raw,
            transfer.confirmations,
            status,
            is_confirmed,
        )
        .await?;

    // Fan out to every active endpoint subscribed to the PAID event; no
    // endpoints configured is a no-op.
    if is_confirmed {
        for endpoint in state
            .db
            .active_webhook_endpoints_for_event(WebhookEventType::Paid)
            .await?
        {
            if !state.db.has_delivered_webhook(&deposit.id, &endpoint.id).await? {
                enqueue_webhook(state, WebhookEventType::Paid, &deposit.id, &endpoint.id).await?;
            }
        }
    }

    Ok(())
}

/// Map a settled Lightning payment to its gateway invoice row. `None` for
/// invoices the gateway did not issue (the Breez wallet may receive other
/// funds), rows that are not PENDING Lightning invoices, and payment ids that
/// could not be stored safely. The tx hash is the SDK payment id and a
/// settled payment is final (1 confirmation); `process_detected_transfer`
/// upserts on (address, tx) so seeing the same payment twice is a no-op.
pub fn lightning_transfer(
    payment: &ReceivedPayment,
    row: Option<&DepositAddressRow>,
) -> Option<DetectedTransfer> {
    let row = row?;
    if row.network != Network::Lightning.as_db()
        || row.status != DepositAddressStatus::Pending.as_db()
    {
        return None;
    }
    // Stored as `deposits.txHash`; ids never reach the webhook queue (which
    // keys on our UUIDs), but keep `|` out so no future member encoding can
    // be ambiguous.
    if payment.payment_id.is_empty() || payment.payment_id.contains('|') {
        warn!(
            "Ignoring Lightning payment with unexpected id shape for address {}",
            row.id
        );
        return None;
    }
    Some(DetectedTransfer {
        deposit_address_id: row.id.clone(),
        tx_hash: payment.payment_id.clone(),
        amount_raw: payment.amount_sats.to_string(),
        confirmations: 1,
    })
}

/// Event-push path: settle the gateway invoice a received payment paid.
pub async fn handle_lightning_payment(state: &AppState, payment: ReceivedPayment) -> Result<()> {
    let row = state.db.find_deposit_address_by_address(&payment.invoice).await?;
    let Some(transfer) = lightning_transfer(&payment, row.as_ref()) else {
        debug!("Lightning payment {} matches no pending invoice", payment.payment_id);
        return Ok(());
    };
    info!(
        "Lightning payment {} settles address {}",
        payment.payment_id, transfer.deposit_address_id
    );
    process_detected_transfer(state, transfer).await
}

/// Reconciliation path (events missed across restarts): match settled
/// payments since the oldest pending invoice against the pending rows.
async fn reconcile_lightning(state: &AppState, pending: &[DepositAddressRow]) -> Result<()> {
    let Some(lightning) = &state.lightning else {
        return Ok(());
    };
    let Some(oldest) = pending.iter().map(|a| a.created_at).min() else {
        return Ok(());
    };
    let since = u64::try_from(oldest.timestamp()).unwrap_or(0);
    let by_invoice: HashMap<&str, &DepositAddressRow> =
        pending.iter().map(|a| (a.address.as_str(), a)).collect();

    for payment in lightning.list_received_since(since).await? {
        let Some(transfer) =
            lightning_transfer(&payment, by_invoice.get(payment.invoice.as_str()).copied())
        else {
            continue;
        };
        info!(
            "Lightning reconciliation: payment {} settles address {}",
            payment.payment_id, transfer.deposit_address_id
        );
        if let Err(err) = process_detected_transfer(state, transfer).await {
            error!("process Lightning payment {} error: {err:#}", payment.payment_id);
        }
    }
    Ok(())
}

/// Fan an address lifecycle event (PENDING at creation, EXPIRED at expiry)
/// out to every active endpoint subscribed to it. Returns the number of
/// deliveries queued; none subscribed is a no-op.
pub async fn notify_address_event(
    state: &AppState,
    address_id: &str,
    event: WebhookEventType,
) -> Result<usize> {
    let endpoints = state.db.active_webhook_endpoints_for_event(event).await?;
    for endpoint in &endpoints {
        enqueue_webhook(state, event, address_id, &endpoint.id).await?;
    }
    Ok(endpoints.len())
}

/// Expire past-deadline addresses and queue the EXPIRED webhook for each
/// newly expired one. Returns the number of addresses expired.
pub async fn expire_addresses_and_notify(state: &AppState) -> Result<u64> {
    let expired_ids = state.db.expire_stale_addresses().await?;
    for address_id in &expired_ids {
        if let Err(err) =
            notify_address_event(state, address_id, WebhookEventType::Expired).await
        {
            warn!("Failed to queue EXPIRED webhook for address {address_id}: {err:#}");
        }
    }
    Ok(expired_ids.len() as u64)
}

/// ZSET member for a scheduled delivery. PAID keeps the historical 2-part
/// `"{deposit_id}|{endpoint_id}"` shape; address-scoped events are
/// `"{EVENT}|{address_id}|{endpoint_id}"`. Ids are UUIDs (`[0-9a-f-]`), so
/// `|` can never appear inside them.
pub fn encode_queue_member(
    event: WebhookEventType,
    subject_id: &str,
    endpoint_id: &str,
) -> String {
    match event {
        WebhookEventType::Paid => format!("{subject_id}|{endpoint_id}"),
        other => format!("{}|{subject_id}|{endpoint_id}", other.as_db()),
    }
}

/// `(event, subject_id, endpoint_id)` — subject is a deposit id for PAID and
/// a deposit-address id otherwise. `None` for legacy members written before
/// per-endpoint deliveries (a bare deposit id) — the webhook loop fans those
/// out to all active PAID endpoints.
pub fn decode_queue_member(member: &str) -> Option<(WebhookEventType, &str, &str)> {
    let parts: Vec<&str> = member.split('|').collect();
    match parts.as_slice() {
        [deposit, endpoint] if !deposit.is_empty() && !endpoint.is_empty() => {
            Some((WebhookEventType::Paid, deposit, endpoint))
        }
        [event, subject, endpoint] if !subject.is_empty() && !endpoint.is_empty() => {
            let event = WebhookEventType::from_db(event).ok()?;
            Some((event, subject, endpoint))
        }
        _ => None,
    }
}

/// Schedule a delivery attempt for "now". ZSET members are unique, so
/// re-enqueueing a key that already has a scheduled retry simply moves it
/// forward instead of adding a duplicate.
pub async fn enqueue_webhook(
    state: &AppState,
    event: WebhookEventType,
    subject_id: &str,
    endpoint_id: &str,
) -> Result<()> {
    let key = state.config.redis_key(WEBHOOK_ZSET_KEY);
    let mut redis = state.redis.clone();
    let _: () = redis
        .zadd(
            key,
            encode_queue_member(event, subject_id, endpoint_id),
            chrono::Utc::now().timestamp(),
        )
        .await?;
    Ok(())
}

/// Gap before the next attempt once `attempts` HTTP attempts have been made.
/// `None` when the retry budget is exhausted (or `attempts` is out of range).
pub fn next_retry_delay_secs(attempts: i32) -> Option<u64> {
    if attempts < 1 {
        return None;
    }
    WEBHOOK_RETRY_DELAYS_SECS.get(attempts as usize - 1).copied()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryOutcome {
    /// 2xx received now, or the row was already DELIVERED.
    Delivered,
    /// Attempt failed with retry budget left; `attempts` made so far.
    RetryAt { attempts: i32 },
    /// Attempt failed and the budget is exhausted; automatic retries stop.
    Failed,
    /// Nothing sent (target endpoint inactive or deleted).
    Skipped,
}

#[derive(Debug)]
pub enum RetriggerError {
    DepositNotFound,
    DepositNotConfirmed,
    DeliveryNotFound,
    /// Target endpoint is inactive or no longer subscribed to the event.
    EndpointNotEligible,
    NoActiveEndpoints,
    Other(anyhow::Error),
}

impl From<anyhow::Error> for RetriggerError {
    fn from(value: anyhow::Error) -> Self {
        Self::Other(value)
    }
}

/// Manual replay for the dashboard: for every active PAID-subscribed
/// endpoint, reset the delivery row to PENDING / 0 attempts, then schedule an
/// immediate attempt. Never touches `deposits` or `deposit_addresses`.
pub async fn retrigger_webhook(state: &AppState, deposit_id: &str) -> Result<(), RetriggerError> {
    let deposit = state
        .db
        .find_deposit(deposit_id)
        .await?
        .ok_or(RetriggerError::DepositNotFound)?;
    let endpoints = state
        .db
        .active_webhook_endpoints_for_event(WebhookEventType::Paid)
        .await?;
    if endpoints.is_empty() {
        return Err(RetriggerError::NoActiveEndpoints);
    }
    if deposit.status != DepositStatus::Confirmed.as_db() {
        return Err(RetriggerError::DepositNotConfirmed);
    }

    for endpoint in &endpoints {
        state.db.reset_webhook_delivery(&deposit.id, &endpoint.id).await?;
        enqueue_webhook(state, WebhookEventType::Paid, &deposit.id, &endpoint.id).await?;
    }
    info!(
        "Webhook retriggered for deposit {deposit_id} towards {} endpoint(s)",
        endpoints.len()
    );
    Ok(())
}

/// Manual replay of one delivery row (any event type): reset it to PENDING /
/// 0 attempts and schedule an immediate attempt. Legacy rows (no endpoint)
/// fall back to the per-deposit replay towards all PAID endpoints.
pub async fn retrigger_delivery(state: &AppState, delivery_id: &str) -> Result<(), RetriggerError> {
    let delivery = state
        .db
        .find_webhook_delivery_by_id(delivery_id)
        .await?
        .ok_or(RetriggerError::DeliveryNotFound)?;
    let event = WebhookEventType::from_db(&delivery.event_type)
        .map_err(|e| RetriggerError::Other(e.into()))?;

    let Some(endpoint_id) = delivery.webhook_endpoint_id.clone() else {
        let deposit_id = delivery.deposit_id.ok_or(RetriggerError::DeliveryNotFound)?;
        return retrigger_webhook(state, &deposit_id).await;
    };

    let endpoint = state
        .db
        .find_webhook_endpoint(&endpoint_id)
        .await?
        .ok_or(RetriggerError::EndpointNotEligible)?;
    if !endpoint.is_active || !endpoint.accepts_event(event) {
        return Err(RetriggerError::EndpointNotEligible);
    }

    let subject_id = match event {
        WebhookEventType::Paid => {
            let deposit_id = delivery.deposit_id.ok_or(RetriggerError::DeliveryNotFound)?;
            let deposit = state
                .db
                .find_deposit(&deposit_id)
                .await?
                .ok_or(RetriggerError::DepositNotFound)?;
            if deposit.status != DepositStatus::Confirmed.as_db() {
                return Err(RetriggerError::DepositNotConfirmed);
            }
            deposit_id
        }
        _ => delivery.deposit_address_id.clone(),
    };

    state.db.reset_webhook_delivery_by_id(&delivery.id).await?;
    enqueue_webhook(state, event, &subject_id, &endpoint_id).await?;
    info!("Webhook delivery {delivery_id} retriggered ({} event)", event.as_api());
    Ok(())
}

/// Bulk manual replay: re-queue every delivery key whose authoritative
/// delivery is FAILED; legacy rows (no endpoint) fan out to all active
/// PAID-subscribed endpoints. Individual failures are logged and skipped;
/// returns the number of keys successfully re-queued.
pub async fn retrigger_failed_webhooks(state: &AppState) -> Result<usize, RetriggerError> {
    let endpoints = state.db.active_webhook_endpoints().await?;
    if endpoints.is_empty() {
        return Err(RetriggerError::NoActiveEndpoints);
    }
    let paid_endpoints: Vec<&str> = endpoints
        .iter()
        .filter(|e| e.accepts_event(WebhookEventType::Paid))
        .map(|e| e.id.as_str())
        .collect();

    let targets = state.db.failed_webhook_targets().await?;
    let mut retried = 0usize;
    for target in &targets {
        let Ok(event) = WebhookEventType::from_db(&target.event_type) else {
            warn!("Bulk webhook retry skipped unknown event type {}", target.event_type);
            continue;
        };
        let subject_id = match event {
            WebhookEventType::Paid => match target.deposit_id.as_deref() {
                Some(id) => id,
                None => continue,
            },
            _ => target.deposit_address_id.as_str(),
        };
        let target_endpoints: Vec<&str> = match &target.webhook_endpoint_id {
            Some(id) => vec![id.as_str()],
            // Legacy failure — replay towards every active PAID endpoint.
            None => paid_endpoints.clone(),
        };
        for endpoint_id in target_endpoints {
            let result: Result<()> = async {
                match event {
                    WebhookEventType::Paid => {
                        state.db.reset_webhook_delivery(subject_id, endpoint_id).await?;
                    }
                    _ => {
                        let row = state
                            .db
                            .get_or_create_address_webhook_delivery(subject_id, event, endpoint_id)
                            .await?;
                        state.db.reset_webhook_delivery_by_id(&row.id).await?;
                    }
                }
                enqueue_webhook(state, event, subject_id, endpoint_id).await
            }
            .await;
            match result {
                Ok(()) => retried += 1,
                Err(err) => {
                    warn!("Bulk webhook retry skipped {} {subject_id} endpoint {endpoint_id}: {err:#}", event.as_api())
                }
            }
        }
    }
    info!("Bulk webhook retry: {retried} delivery target(s) re-queued");
    Ok(retried)
}

/// Deliver one queued webhook. `subject_id` is a deposit id for PAID and a
/// deposit-address id for PENDING/EXPIRED.
pub async fn deliver_webhook(
    state: &AppState,
    event: WebhookEventType,
    subject_id: &str,
    endpoint_id: &str,
) -> Result<DeliveryOutcome> {
    // Reload the endpoint on every attempt so a deactivation or an event
    // unsubscription takes effect immediately: the queued member is consumed
    // without being rescheduled.
    let endpoint = match state.db.find_webhook_endpoint(endpoint_id).await? {
        Some(endpoint) if endpoint.is_active && endpoint.accepts_event(event) => endpoint,
        _ => {
            warn!(
                "Webhook delivery skipped for {} {subject_id} — endpoint {endpoint_id} is inactive, gone or unsubscribed",
                event.as_api()
            );
            return Ok(DeliveryOutcome::Skipped);
        }
    };
    let webhook_url = &endpoint.url;
    let subject_label = match event {
        WebhookEventType::Paid => format!("deposit {subject_id}"),
        _ => format!("address {subject_id} ({})", event.as_api()),
    };

    let (delivery, payload) = match event {
        WebhookEventType::Paid => {
            let Some(deposit) = state.db.find_deposit(subject_id).await? else {
                anyhow::bail!("Deposit not found: {subject_id}");
            };
            let Some(address) = state.db.find_deposit_address(&deposit.deposit_address_id).await?
            else {
                anyhow::bail!("Deposit address not found for deposit {subject_id}");
            };
            let delivery = state
                .db
                .get_or_create_webhook_delivery(subject_id, endpoint_id)
                .await?;
            let network = Network::from_db(&address.network)?;
            let token = Token::from_db(&address.token)?;
            // Historical shape — `data.deposit_id` is deposit_addresses.id
            // on purpose (see AGENTS.md); existing receivers depend on it.
            let payload = serde_json::json!({
                "event_type": event.payload_event_type(),
                "data": {
                    "deposit_id": deposit.deposit_address_id,
                    "address": address.address,
                    "network": network.as_api(),
                    "token": address.token,
                    "amount": format_amount(deposit.amount, token),
                    "amount_raw": deposit.amount_raw,
                    "tx_hash": deposit.tx_hash,
                    "confirmations": deposit.confirmations,
                    "status": "confirmed",
                },
            });
            (delivery, payload)
        }
        WebhookEventType::Pending | WebhookEventType::Expired => {
            let Some(address) = state.db.find_deposit_address(subject_id).await? else {
                anyhow::bail!("Deposit address not found: {subject_id}");
            };
            let delivery = state
                .db
                .get_or_create_address_webhook_delivery(subject_id, event, endpoint_id)
                .await?;
            (delivery, address_event_payload(event, &address)?)
        }
    };

    if delivery.status == WebhookDeliveryStatus::Delivered.as_db() {
        return Ok(DeliveryOutcome::Delivered);
    }

    // Serialize exactly once: the signature must cover the exact bytes sent.
    let body = serde_json::to_vec(&payload)?;
    let client = reqwest::Client::new();
    let mut request = client
        .post(webhook_url)
        .timeout(Duration::from_secs(10))
        .header("Content-Type", "application/json");
    if let Some(secret) = endpoint.secret.as_deref().filter(|s| !s.is_empty()) {
        request = request.header("X-Webhook-Signature", auth::webhook_signature(secret, &body));
    }
    match request.body(body).send().await {
        Ok(resp) => {
            let status = resp.status().as_u16() as i32;
            if resp.status().is_success() {
                state
                    .db
                    .update_webhook_delivery(
                        &delivery.id,
                        WebhookDeliveryStatus::Delivered,
                        delivery.attempts + 1,
                        Some(status),
                        None,
                        Some(payload.clone()),
                    )
                    .await?;
                info!("Webhook delivered for {subject_label}");
                Ok(DeliveryOutcome::Delivered)
            } else {
                let attempts = delivery.attempts + 1;
                let final_status = if attempts >= WEBHOOK_MAX_ATTEMPTS as i32 {
                    WebhookDeliveryStatus::Failed
                } else {
                    WebhookDeliveryStatus::Pending
                };
                state
                    .db
                    .update_webhook_delivery(
                        &delivery.id,
                        final_status,
                        attempts,
                        Some(status),
                        Some(format!("HTTP {status}")),
                        Some(payload.clone()),
                    )
                    .await?;
                if attempts < WEBHOOK_MAX_ATTEMPTS as i32 {
                    warn!("Webhook HTTP {status} for {subject_label} (attempt {attempts})");
                    return Ok(DeliveryOutcome::RetryAt { attempts });
                }
                error!("Webhook delivery failed after {attempts} attempts for {subject_label}");
                Ok(DeliveryOutcome::Failed)
            }
        }
        Err(err) => {
            let attempts = delivery.attempts + 1;
            let final_status = if attempts >= WEBHOOK_MAX_ATTEMPTS as i32 {
                WebhookDeliveryStatus::Failed
            } else {
                WebhookDeliveryStatus::Pending
            };
            state
                .db
                .update_webhook_delivery(
                    &delivery.id,
                    final_status,
                    attempts,
                    None,
                    Some(err.to_string()),
                    Some(payload.clone()),
                )
                .await?;
            if attempts < WEBHOOK_MAX_ATTEMPTS as i32 {
                warn!("Webhook error for {subject_label} (attempt {attempts}): {err}");
                return Ok(DeliveryOutcome::RetryAt { attempts });
            }
            error!("Webhook delivery failed after {attempts} attempts for {subject_label}");
            Ok(DeliveryOutcome::Failed)
        }
    }
}

/// Payload of an address-scoped event. Mirrors the PAID payload's naming:
/// `data.deposit_id` is deposit_addresses.id.
fn address_event_payload(
    event: WebhookEventType,
    address: &DepositAddressRow,
) -> Result<serde_json::Value> {
    let network = Network::from_db(&address.network)?;
    Ok(serde_json::json!({
        "event_type": event.payload_event_type(),
        "data": {
            "deposit_id": address.id,
            "address": address.address,
            "network": network.as_api(),
            "token": address.token,
            "expected_amount": address.expected_amount.normalize().to_string(),
            "status": event.as_api(),
            "expires_at": address.expires_at.to_rfc3339(),
            "created_at": address.created_at.to_rfc3339(),
        },
    }))
}

/// `traffic_rx` is the consumer side of `state.traffic`; it is dropped when
/// the traffic log is disabled, which turns every `record` into a no-op.
pub fn spawn_workers(
    state: AppState,
    traffic_rx: tokio::sync::mpsc::Receiver<db::ApiTrafficEvent>,
) {
    if state.config.api_traffic_enabled {
        let db = state.db.clone();
        tokio::spawn(async move { traffic::api_traffic_writer_loop(db, traffic_rx).await });
        let s0 = state.clone();
        tokio::spawn(async move { traffic::api_traffic_purge_loop(s0).await });
    } else {
        drop(traffic_rx);
        info!("API_TRAFFIC_ENABLED=false — API traffic log disabled");
    }

    let poll_secs = state.config.polling_interval_seconds.max(1);
    for network in [Network::Tron, Network::Ethereum, Network::Solana, Network::Bitcoin] {
        let s = state.clone();
        tokio::spawn(async move { monitor_loop(s, network, poll_secs).await });
    }
    if let Some(lightning) = state.lightning.clone() {
        let s = state.clone();
        tokio::spawn(async move { lightning_event_loop(s, lightning).await });
        let s = state.clone();
        tokio::spawn(async move { monitor_loop(s, Network::Lightning, poll_secs).await });
    }
    let s4 = state.clone();
    tokio::spawn(async move { webhook_loop(s4).await });
    let s5 = state.clone();
    tokio::spawn(async move { wallet_balance_sync_loop(s5).await });
    let s6 = state.clone();
    tokio::spawn(async move { api_key_expiry_loop(s6).await });
}

/// Materialize EXPIRED statuses for past-deadline API keys. Runs once right
/// at boot, then every `API_KEY_EXPIRY_SWEEP_INTERVAL_SECS`. Expired keys are
/// already rejected at lookup time — this keeps listings and stats honest.
async fn api_key_expiry_loop(state: AppState) {
    let mut interval =
        tokio::time::interval(Duration::from_secs(API_KEY_EXPIRY_SWEEP_INTERVAL_SECS));
    loop {
        interval.tick().await;
        match state.db.expire_stale_api_keys().await {
            Ok(0) => {}
            Ok(expired) => info!("Expired {expired} api key(s)"),
            Err(err) => error!("api key expiry sweep error: {err:#}"),
        }
    }
}

/// Primary Lightning detection: SDK `PaymentSucceeded` events. The SDK
/// callback only enqueues; settlement runs here, off the SDK dispatch.
async fn lightning_event_loop(state: AppState, lightning: LightningClient) {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<ReceivedPayment>();
    let listener_id = lightning
        .subscribe(move |payment| {
            let _ = tx.send(payment);
        })
        .await;
    info!("Lightning payment listener registered ({listener_id})");
    while let Some(payment) = rx.recv().await {
        let payment_id = payment.payment_id.clone();
        if let Err(err) = handle_lightning_payment(&state, payment).await {
            // The reconciliation loop picks it up on its next tick.
            error!("Lightning payment {payment_id} processing error: {err:#}");
        }
    }
}

async fn monitor_loop(state: AppState, network: Network, poll_secs: u64) {
    let mut interval = tokio::time::interval(Duration::from_secs(poll_secs));
    loop {
        interval.tick().await;
        if let Err(err) = poll_network(&state, network).await {
            error!("{network:?} monitor error: {err:#}");
        }
    }
}

async fn poll_network(state: &AppState, network: Network) -> Result<()> {
    if !state.is_network_enabled(network) {
        debug!(
            "Skipping {:?} monitor — {}",
            network,
            state.network_disabled_message(network)
        );
        return Ok(());
    }

    let _ = expire_addresses_and_notify(state).await?;
    let addresses = state.db.pending_addresses(network).await?;
    if addresses.is_empty() {
        return Ok(());
    }
    if network == Network::Lightning {
        return reconcile_lightning(state, &addresses).await;
    }

    for (i, address) in addresses.iter().enumerate() {
        if i > 0 {
            tokio::time::sleep(Duration::from_millis(ADDRESS_REQUEST_DELAY_MS)).await;
        }
        let transfers = match network {
            Network::Tron => state.chain.poll_tron_address(&address.address).await,
            Network::Ethereum => state.chain.poll_ethereum_address(&address.address).await,
            Network::Solana => state.chain.poll_solana_address(&address.address).await,
            Network::Bitcoin => state.chain.poll_bitcoin_address(&address.address).await,
            // Reconciled above from the Breez payment list, not per address.
            Network::Lightning => Ok(vec![]),
        };

        match transfers {
            Ok(transfers) => {
                for t in transfers {
                    if let Err(err) = process_detected_transfer(
                        state,
                        DetectedTransfer {
                            deposit_address_id: address.id.clone(),
                            tx_hash: t.tx_hash,
                            amount_raw: t.amount_raw,
                            confirmations: t.confirmations,
                        },
                    )
                    .await
                    {
                        error!("process transfer error for {}: {err:#}", address.id);
                    }
                }
            }
            Err(err) => {
                error!("{:?} monitor error for {}: {err:#}", network, address.id);
            }
        }
    }
    Ok(())
}

async fn webhook_loop(state: AppState) {
    if let Err(err) = drain_legacy_webhook_queue(&state).await {
        warn!("legacy webhook queue drain failed: {err:#}");
    }

    let key = state.config.redis_key(WEBHOOK_ZSET_KEY);
    let mut interval = tokio::time::interval(Duration::from_secs(2));
    loop {
        interval.tick().await;
        let mut redis = state.redis.clone();
        let now = chrono::Utc::now().timestamp();

        let due: Vec<String> = match redis
            .zrangebyscore_limit(&key, "-inf", now, 0, 1)
            .await
        {
            Ok(v) => v,
            Err(err) => {
                error!("webhook queue read error: {err}");
                continue;
            }
        };
        let Some(member) = due.into_iter().next() else {
            continue;
        };
        if let Err(err) = redis.zrem::<_, _, ()>(&key, &member).await {
            error!("webhook queue remove error: {err}");
            continue;
        }

        let Some((event, subject_id, endpoint_id)) = decode_queue_member(&member) else {
            // Legacy member (bare deposit id, pre-multi-endpoint deploy):
            // fan it out to the current active PAID-subscribed endpoints.
            match state
                .db
                .active_webhook_endpoints_for_event(WebhookEventType::Paid)
                .await
            {
                Ok(endpoints) if endpoints.is_empty() => {
                    warn!("Dropping legacy webhook queue member {member} — no active endpoints");
                }
                Ok(endpoints) => {
                    info!("Fanning out legacy webhook queue member {member} to {} endpoint(s)", endpoints.len());
                    for endpoint in endpoints {
                        if let Err(err) =
                            enqueue_webhook(&state, WebhookEventType::Paid, &member, &endpoint.id)
                                .await
                        {
                            error!("legacy member fan-out error for {member}: {err:#}");
                        }
                    }
                }
                Err(err) => {
                    warn!("legacy member fan-out read error for {member}, rescheduling: {err:#}");
                    let score = now + WEBHOOK_RETRY_DELAYS_SECS[0] as i64;
                    let _: Result<(), _> = redis.zadd(&key, &member, score).await;
                }
            }
            continue;
        };

        match deliver_webhook(&state, event, subject_id, endpoint_id).await {
            Ok(DeliveryOutcome::RetryAt { attempts }) => {
                if let Some(delay) = next_retry_delay_secs(attempts) {
                    let score = now + delay as i64;
                    if let Err(err) = redis.zadd::<_, _, _, ()>(&key, &member, score).await {
                        error!("webhook retry schedule error for {member}: {err}");
                    } else {
                        info!("Webhook attempt {} for {} {subject_id} (endpoint {endpoint_id}) rescheduled in {delay}s", attempts + 1, event.as_api());
                    }
                }
            }
            Ok(_) => {}
            Err(err) => {
                // Infrastructure error (DB unreachable, row missing): retry
                // after the first backoff gap rather than spinning.
                warn!("webhook delivery error for {member}, rescheduling: {err:#}");
                let score = now + WEBHOOK_RETRY_DELAYS_SECS[0] as i64;
                let _: Result<(), _> = redis.zadd(&key, &member, score).await;
            }
        }
    }
}

/// One-shot migration: move ids from the pre-backoff LIST queue into the
/// scheduled ZSET so in-flight webhooks survive the deploy.
async fn drain_legacy_webhook_queue(state: &AppState) -> Result<()> {
    let list_key = state.config.redis_key(LEGACY_WEBHOOK_LIST_KEY);
    let zset_key = state.config.redis_key(WEBHOOK_ZSET_KEY);
    let mut redis = state.redis.clone();
    let mut drained = 0u32;
    loop {
        let deposit_id: Option<String> = redis.rpop(&list_key, None).await?;
        let Some(deposit_id) = deposit_id else { break };
        let _: () = redis
            .zadd(&zset_key, &deposit_id, chrono::Utc::now().timestamp())
            .await?;
        drained += 1;
    }
    if drained > 0 {
        info!("Drained {drained} webhook id(s) from the legacy list queue");
    }
    Ok(())
}

async fn wallet_balance_sync_loop(state: AppState) {
    // run once shortly after boot, then hourly
    tokio::time::sleep(Duration::from_secs(5)).await;
    loop {
        if let Err(err) = sync_wallet_balances(&state).await {
            error!("wallet balance sync error: {err:#}");
        }
        tokio::time::sleep(Duration::from_secs(WALLET_BALANCE_SYNC_INTERVAL_SECS)).await;
    }
}

/// Raw totals per token. Each network carries a single token, so sats can
/// never leak into the USDT figure (and vice versa).
#[derive(Debug, Default)]
pub struct TokenTotals {
    pub usdt_raw: u128,
    pub btc_raw: u128,
}

impl TokenTotals {
    pub fn total_usdt(&self) -> String {
        raw_to_decimal_string(&self.usdt_raw.to_string(), Token::Usdt)
            .unwrap_or_else(|_| "0".into())
    }

    pub fn total_btc(&self) -> String {
        raw_to_decimal_string(&self.btc_raw.to_string(), Token::Btc)
            .unwrap_or_else(|_| "0".into())
    }
}

pub fn token_totals<'a>(entries: impl IntoIterator<Item = (Token, &'a str)>) -> TokenTotals {
    let mut totals = TokenTotals::default();
    for (token, amount_raw) in entries {
        let raw = amount_raw.parse::<u128>().unwrap_or(0);
        match token {
            Token::Usdt => totals.usdt_raw += raw,
            Token::Btc => totals.btc_raw += raw,
        }
    }
    totals
}

/// Networks whose balance is the sum over derived addresses.
const ADDRESS_BALANCE_NETWORKS: [Network; 4] =
    [Network::Tron, Network::Ethereum, Network::Solana, Network::Bitcoin];

async fn fetch_address_balance(state: &AppState, network: Network, address: &str) -> Result<u128> {
    match network {
        Network::Bitcoin => state.chain.fetch_btc_balance(address).await,
        _ => state.chain.fetch_usdt_balance(network, address).await,
    }
}

fn format_raw(raw: u128, token: Token) -> String {
    raw_to_decimal_string(&raw.to_string(), token).unwrap_or_else(|_| "0".into())
}

pub async fn sync_wallet_balances(state: &AppState) -> Result<()> {
    if !state.chain.has_any_network_available() && state.lightning.is_none() {
        debug!("Skipping wallet balance sync — no blockchain API keys configured");
        return Ok(());
    }

    info!("Starting wallet HD balance sync");
    let mut networks = Vec::new();
    for network in ADDRESS_BALANCE_NETWORKS {
        networks.push(sync_network(state, network).await?);
    }
    networks.push(lightning_wallet_balance(state).await);

    let totals = token_totals(
        networks
            .iter()
            .filter_map(|n| Token::parse_api(&n.token).ok().map(|t| (t, n.amount_raw.as_str()))),
    );
    let payload = CachedWalletBalances {
        total_usdt: totals.total_usdt(),
        total_usdt_raw: totals.usdt_raw.to_string(),
        total_btc: totals.total_btc(),
        total_btc_raw: totals.btc_raw.to_string(),
        networks,
        updated_at: chrono::Utc::now().to_rfc3339(),
    };

    let key = state.config.redis_key(WALLET_BALANCES_KEY);
    let mut redis = state.redis.clone();
    let _: () = redis.set(key, serde_json::to_string(&payload)?).await?;
    info!(
        "Wallet HD balance sync completed — total USDT: {}, total BTC: {}",
        payload.total_usdt, payload.total_btc
    );
    Ok(())
}

async fn sync_network(state: &AppState, network: Network) -> Result<NetworkWalletBalance> {
    let network_name = network.as_api().to_string();
    let token = network.token();
    if !state.is_network_enabled(network) {
        return Ok(NetworkWalletBalance {
            network: network_name,
            token: token.as_api().into(),
            amount: "0".into(),
            amount_raw: "0".into(),
            indices_scanned: 0,
            available: false,
            message: Some(state.network_disabled_message(network).to_string()),
        });
    }

    let max_index = state.db.max_derivation_index(network).await?;
    let mut total_raw: u128 = 0;
    for index in 0..=max_index {
        if index > 0 {
            tokio::time::sleep(Duration::from_millis(ADDRESS_REQUEST_DELAY_MS)).await;
        }
        let address = state.wallet.derive_address(network, index as u32)?;
        match fetch_address_balance(state, network, &address).await {
            Ok(bal) => total_raw += bal,
            Err(err) => warn!(
                "Failed to fetch wallet balance for {network_name} index {index}: {err:#}"
            ),
        }
    }

    Ok(NetworkWalletBalance {
        network: network_name,
        token: token.as_api().into(),
        amount: format_raw(total_raw, token),
        amount_raw: total_raw.to_string(),
        indices_scanned: max_index + 1,
        available: true,
        message: None,
    })
}

/// The Breez/Spark wallet balance — a single entry, no derived addresses.
async fn lightning_wallet_balance(state: &AppState) -> NetworkWalletBalance {
    let mut entry = empty_wallet_network(Network::Lightning);
    let Some(lightning) = &state.lightning else {
        entry.message = Some(state.network_disabled_message(Network::Lightning).to_string());
        return entry;
    };
    match lightning.balance_sats(true).await {
        Ok(sats) => {
            entry.amount = format_raw(u128::from(sats), Token::Btc);
            entry.amount_raw = sats.to_string();
            entry.available = true;
        }
        Err(err) => {
            warn!("Failed to fetch Lightning wallet balance: {err:#}");
            entry.message = Some("Failed to read the Lightning wallet balance".into());
        }
    }
    entry
}

pub async fn get_cached_wallet_balances(state: &AppState) -> Result<WalletBalancesResponse> {
    let key = state.config.redis_key(WALLET_BALANCES_KEY);
    let mut redis = state.redis.clone();
    let raw: Option<String> = redis.get(key).await?;
    if let Some(raw) = raw {
        let cached: CachedWalletBalances = serde_json::from_str(&raw)?;
        return Ok(WalletBalancesResponse {
            total_usdt: cached.total_usdt,
            total_btc: cached.total_btc,
            networks: cached.networks,
            updated_at: Some(cached.updated_at),
        });
    }
    Ok(WalletBalancesResponse {
        total_usdt: "0".into(),
        total_btc: "0".into(),
        networks: Network::ALL.into_iter().map(empty_wallet_network).collect(),
        updated_at: None,
    })
}

pub async fn get_live_balances(state: &AppState) -> Result<BalancesResponse> {
    let mut networks = Vec::new();
    for network in ADDRESS_BALANCE_NETWORKS {
        networks.push(live_network_balance(state, network).await?);
    }
    networks.push(live_lightning_balance(state).await);

    let totals = token_totals(
        networks
            .iter()
            .filter_map(|n| Token::parse_api(&n.token).ok().map(|t| (t, n.amount_raw.as_str()))),
    );
    Ok(BalancesResponse {
        total_usdt: totals.total_usdt(),
        total_btc: totals.total_btc(),
        networks,
    })
}

async fn live_network_balance(state: &AppState, network: Network) -> Result<NetworkBalance> {
    let network_name = network.as_api().to_string();
    let token = network.token();
    if !state.is_network_enabled(network) {
        return Ok(NetworkBalance {
            network: network_name,
            token: token.as_api().into(),
            amount: "0".into(),
            amount_raw: "0".into(),
            addresses_checked: 0,
            available: false,
            message: Some(state.network_disabled_message(network).to_string()),
        });
    }

    let addresses = state.db.addresses_for_network(network).await?;
    let mut total_raw: u128 = 0;
    for address in &addresses {
        match fetch_address_balance(state, network, address).await {
            Ok(bal) => total_raw += bal,
            Err(err) => warn!("Failed to fetch {network_name} balance for {address}: {err:#}"),
        }
    }

    Ok(NetworkBalance {
        network: network_name,
        token: token.as_api().into(),
        amount: format_raw(total_raw, token),
        amount_raw: total_raw.to_string(),
        addresses_checked: addresses.len() as i32,
        available: true,
        message: None,
    })
}

async fn live_lightning_balance(state: &AppState) -> NetworkBalance {
    let mut entry = NetworkBalance {
        network: Network::Lightning.as_api().into(),
        token: Token::Btc.as_api().into(),
        amount: "0".into(),
        amount_raw: "0".into(),
        addresses_checked: 0,
        available: false,
        message: None,
    };
    let Some(lightning) = &state.lightning else {
        entry.message = Some(state.network_disabled_message(Network::Lightning).to_string());
        return entry;
    };
    match lightning.balance_sats(false).await {
        Ok(sats) => {
            entry.amount = format_raw(u128::from(sats), Token::Btc);
            entry.amount_raw = sats.to_string();
            entry.available = true;
        }
        Err(err) => {
            warn!("Failed to fetch Lightning wallet balance: {err:#}");
            entry.message = Some("Failed to read the Lightning wallet balance".into());
        }
    }
    entry
}

fn empty_wallet_network(network: Network) -> NetworkWalletBalance {
    NetworkWalletBalance {
        network: network.as_api().into(),
        token: network.token().as_api().into(),
        amount: "0".into(),
        amount_raw: "0".into(),
        indices_scanned: 0,
        available: false,
        message: None,
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CachedWalletBalances {
    pub total_usdt: String,
    pub total_usdt_raw: String,
    /// Defaults keep payloads cached before BTC support readable.
    #[serde(default = "zero_string")]
    pub total_btc: String,
    #[serde(default = "zero_string")]
    pub total_btc_raw: String,
    pub networks: Vec<NetworkWalletBalance>,
    pub updated_at: String,
}

fn zero_string() -> String {
    "0".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkWalletBalance {
    pub network: String,
    pub token: String,
    pub amount: String,
    pub amount_raw: String,
    pub indices_scanned: i32,
    pub available: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct WalletBalancesResponse {
    pub total_usdt: String,
    /// Sum of the bitcoin + lightning entries; never folded into USDT.
    pub total_btc: String,
    pub networks: Vec<NetworkWalletBalance>,
    pub updated_at: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct BalancesResponse {
    pub total_usdt: String,
    /// Sum of the bitcoin + lightning entries; never folded into USDT.
    pub total_btc: String,
    pub networks: Vec<NetworkBalance>,
}

#[derive(Debug, Serialize)]
pub struct NetworkBalance {
    pub network: String,
    pub token: String,
    pub amount: String,
    pub amount_raw: String,
    pub addresses_checked: i32,
    pub available: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use lightning_client::ReceivedPayment;

    fn invoice_row(network: Network, status: DepositAddressStatus) -> DepositAddressRow {
        let now = chrono::Utc::now();
        DepositAddressRow {
            id: "addr-1".into(),
            network: network.as_db().into(),
            token: network.token().as_db().into(),
            address: "lnbc1500u1invoice".into(),
            derivation_index: 0,
            expected_amount: rust_decimal::Decimal::new(150_000, 8),
            status: status.as_db().into(),
            expires_at: now,
            metadata: serde_json::json!({}),
            created_at: now,
            updated_at: now,
        }
    }

    fn settled(payment_id: &str) -> ReceivedPayment {
        ReceivedPayment {
            invoice: "lnbc1500u1invoice".into(),
            payment_id: payment_id.into(),
            amount_sats: 150_000,
        }
    }

    #[test]
    fn settled_invoice_becomes_a_final_transfer_keyed_by_payment_id() {
        let row = invoice_row(Network::Lightning, DepositAddressStatus::Pending);
        let transfer = lightning_transfer(&settled("pay-abc"), Some(&row)).expect("matched");
        assert_eq!(transfer.deposit_address_id, "addr-1");
        assert_eq!(transfer.tx_hash, "pay-abc");
        assert_eq!(transfer.amount_raw, "150000");
        assert_eq!(transfer.confirmations, 1);
    }

    #[test]
    fn unknown_invoices_are_ignored() {
        // The Breez wallet may receive funds unrelated to any gateway invoice.
        assert!(lightning_transfer(&settled("pay-abc"), None).is_none());
    }

    #[test]
    fn only_pending_lightning_rows_are_matched() {
        for status in [DepositAddressStatus::Paid, DepositAddressStatus::Expired] {
            let row = invoice_row(Network::Lightning, status);
            assert!(lightning_transfer(&settled("pay-abc"), Some(&row)).is_none());
        }
        let row = invoice_row(Network::Bitcoin, DepositAddressStatus::Pending);
        assert!(lightning_transfer(&settled("pay-abc"), Some(&row)).is_none());
    }

    #[test]
    fn payment_ids_with_a_queue_separator_are_rejected_before_storage() {
        let row = invoice_row(Network::Lightning, DepositAddressStatus::Pending);
        assert!(lightning_transfer(&settled("pay|abc"), Some(&row)).is_none());
        assert!(lightning_transfer(&settled(""), Some(&row)).is_none());
    }

    #[test]
    fn event_and_reconciliation_yield_the_same_upsert_key() {
        // The event push and the reconciliation poll can both see the same
        // payment; they must produce the same (address, tx) key so the
        // per-address tx-hash upsert in `process_detected_transfer` makes the
        // second delivery a no-op (and the address is PAID by then anyway).
        let row = invoice_row(Network::Lightning, DepositAddressStatus::Pending);
        let from_event = lightning_transfer(&settled("pay-abc"), Some(&row)).unwrap();
        let from_poll = lightning_transfer(&settled("pay-abc"), Some(&row)).unwrap();
        assert_eq!(
            (from_event.deposit_address_id, from_event.tx_hash, from_event.amount_raw),
            (from_poll.deposit_address_id, from_poll.tx_hash, from_poll.amount_raw)
        );
    }

    #[test]
    fn totals_never_mix_sats_into_usdt() {
        let totals = token_totals([
            (Token::Usdt, "25000000"),
            (Token::Btc, "150000"),
            (Token::Usdt, "1"),
            (Token::Btc, "50000"),
            (Token::Usdt, "not-a-number"),
        ]);
        assert_eq!(totals.usdt_raw, 25_000_001);
        assert_eq!(totals.btc_raw, 200_000);
        assert_eq!(totals.total_usdt(), "25.000001");
        assert_eq!(totals.total_btc(), "0.002");
    }

    #[test]
    fn paid_queue_member_keeps_legacy_shape() {
        let member = encode_queue_member(WebhookEventType::Paid, "dep-123", "ep-456");
        assert_eq!(member, "dep-123|ep-456");
        assert_eq!(
            decode_queue_member(&member),
            Some((WebhookEventType::Paid, "dep-123", "ep-456"))
        );
    }

    #[test]
    fn address_event_queue_member_roundtrip() {
        for event in [WebhookEventType::Pending, WebhookEventType::Expired] {
            let member = encode_queue_member(event, "addr-123", "ep-456");
            assert_eq!(member, format!("{}|addr-123|ep-456", event.as_db()));
            assert_eq!(
                decode_queue_member(&member),
                Some((event, "addr-123", "ep-456"))
            );
        }
    }

    #[test]
    fn legacy_queue_member_is_detected() {
        assert_eq!(decode_queue_member("a1b2c3-uuid-without-separator"), None);
        assert_eq!(decode_queue_member(""), None);
        assert_eq!(decode_queue_member("|"), None);
        assert_eq!(decode_queue_member("dep|"), None);
        assert_eq!(decode_queue_member("|ep"), None);
        // Unknown event prefix in a 3-part member is not a valid new-style
        // member either.
        assert_eq!(decode_queue_member("BOGUS|addr|ep"), None);
        assert_eq!(decode_queue_member("a|b|c|d"), None);
    }

    #[test]
    fn retry_delays_follow_schedule() {
        assert_eq!(next_retry_delay_secs(1), Some(90));
        assert_eq!(next_retry_delay_secs(2), Some(270));
        assert_eq!(next_retry_delay_secs(3), Some(810));
        assert_eq!(next_retry_delay_secs(4), Some(2430));
    }

    #[test]
    fn retries_stop_at_max_attempts() {
        assert_eq!(next_retry_delay_secs(0), None);
        assert_eq!(next_retry_delay_secs(-1), None);
        assert_eq!(next_retry_delay_secs(WEBHOOK_MAX_ATTEMPTS as i32), None);
        assert_eq!(next_retry_delay_secs(WEBHOOK_MAX_ATTEMPTS as i32 + 1), None);
    }

    #[test]
    fn last_attempt_starts_one_hour_after_first() {
        let total: u64 = (1..WEBHOOK_MAX_ATTEMPTS as i32)
            .map(|attempts| next_retry_delay_secs(attempts).unwrap())
            .sum();
        assert_eq!(total, 3600);
    }
}
