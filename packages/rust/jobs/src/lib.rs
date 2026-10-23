use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use chain::ChainClients;
use config::{
    AppConfig, ADDRESS_EXPIRY_GRACE_SECS, ADDRESS_REQUEST_DELAY_MS,
    API_KEY_EXPIRY_SWEEP_INTERVAL_SECS, DETECTED_DEPOSIT_HOLD_SECS,
    LIGHTNING_PAYOUT_CATCH_UP_MARGIN_SECS, LIGHTNING_PAYOUT_WATERMARK_INTERVAL_SECS,
    LIVE_BALANCES_CACHE_SECS, WALLET_BALANCE_SYNC_INTERVAL_SECS, WEBHOOK_CLAIM_LEASE_SECS,
    WEBHOOK_DELIVERY_CONCURRENCY, WEBHOOK_MAX_ATTEMPTS, WEBHOOK_RECONCILE_BATCH,
    WEBHOOK_RECONCILE_INTERVAL_SECS, WEBHOOK_RECONCILE_MAX_AGE_SECS, WEBHOOK_RETRY_DELAYS_SECS,
};
use db::Db;
use db::DepositAddressRow;
use domain::{
    decimal_to_raw, format_amount, raw_to_decimal, raw_to_decimal_string, required_confirmations,
    DetectedTransfer, DepositAddressStatus, DepositStatus, Network, Token,
    WebhookDeliveryStatus, WebhookEventType, WebhookSubject,
};
use lightning_client::{
    LightningClient, LightningEvent, PaymentDirection, ReceivedPayment, WalletPayment,
    WalletPaymentStatus,
};
use redis::aio::ConnectionManager;
use redis::AsyncCommands;
use serde::{Deserialize, Serialize};
use tracing::{debug, error, info, warn};
use wallet::WalletService;

pub mod alerts;
pub mod audit;
pub mod fees;
pub mod freeze;
pub mod ledger;
pub mod outbound;
pub mod payouts;
pub mod projects;
pub mod sweeper;
pub mod traffic;
pub mod transfers;
pub mod treasury;
pub mod webhook_client;
pub mod webhooks;
pub mod withdrawals;
pub use audit::Actor;
pub use payouts::{legacy_payout_input, PAYOUT_ALIAS_SUNSET};
pub use traffic::TrafficRecorder;
pub use webhook_client::{check_webhook_host_resolves_publicly, validate_webhook_url, WebhookClient};

/// Legacy LIST queue, only drained at startup (pre-backoff deploys).
const LEGACY_WEBHOOK_LIST_KEY: &str = "queue:webhook-delivery";
/// ZSET scored by next-attempt epoch seconds. New suffix on purpose: a Redis
/// key cannot change type in place (WRONGTYPE against the old LIST).
const WEBHOOK_ZSET_KEY: &str = "zset:webhook-delivery";
/// Members claimed by a worker, scored by lease expiry: a worker that dies
/// mid-delivery leaves its member here, and it goes back to the queue once
/// the lease expires (at-least-once). A member is never claimed twice at
/// the same time, even across overlapping containers during a deploy.
const WEBHOOK_INFLIGHT_KEY: &str = "zset:webhook-inflight";
/// Cached `/balances` result and the lock of whoever recomputes it.
const LIVE_BALANCES_KEY: &str = "live-balances";
const LIVE_BALANCES_LOCK_KEY: &str = "live-balances:lock";
const WALLET_BALANCES_KEY: &str = "wallet-balances";
/// Unix seconds up to which finished wallet sends have been notified.
const PAYOUT_WATERMARK_KEY: &str = "lightning:payout-watermark";

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
    /// Outgoing webhook client (SSRF-hardened outside development).
    pub webhooks: WebhookClient,
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

/// Record a transfer seen on a watched address (ADR 0005). Every non-zero
/// transfer is a deposit, `DETECTED` then `CONFIRMED`, whatever its amount
/// — but one included in a block older than the payment (beyond the
/// clock-skew margin, `domain::predates_payment`) is not: never recorded,
/// counted or announced:
/// the payment is the confirmed total of its address, paid from
/// `100 − AMOUNT_TOLERANCE_PERCENT` % of the expected amount. `paid` fans
/// out once, when this call set the address PAID; `late` when it confirmed
/// a late deposit (first seen once the address had left PENDING).
pub async fn process_detected_transfer(state: &AppState, transfer: DetectedTransfer) -> Result<()> {
    let address = match state.db.find_deposit_address(&transfer.deposit_address_id).await? {
        Some(a) => a,
        None => return Ok(()),
    };

    let network = Network::from_db(&address.network)?;
    let token = Token::from_db(&address.token)?;
    let status = DepositAddressStatus::from_db(&address.status)?;
    let required = required_confirmations(
        network,
        state.config.tron_confirmations,
        state.config.eth_confirmations,
        state.config.btc_confirmations,
    );

    let existing = state
        .db
        .find_deposit_by_address_and_tx(&transfer.deposit_address_id, &transfer.tx_hash)
        .await?;
    match &existing {
        Some(existing) if existing.status == DepositStatus::Confirmed.as_db() => return Ok(()),
        Some(_) => {}
        // Fast path of the rule the recording transaction applies again
        // under the address lock.
        None => {
            // Funds the address held before the payment existed (a reused
            // index, a restored derivation counter): never its deposits.
            if domain::predates_payment(transfer.block_time, address.created_at.timestamp()) {
                debug!(
                    "Transfer {} to address {} is older than the payment (block at {:?}, created at {}) — not a deposit",
                    transfer.tx_hash,
                    address.id,
                    transfer.block_time,
                    address.created_at.timestamp()
                );
                return Ok(());
            }
            let watch_open = address.watch_until > chrono::Utc::now();
            if domain::admit_transfer(network, status, watch_open) == domain::TransferAdmission::Ignore {
                debug!(
                    "Transfer {} to address {} after its late watch — left to the sweeps",
                    transfer.tx_hash, address.id
                );
                return Ok(());
            }
        }
    }

    let amount = raw_to_decimal(&transfer.amount_raw, token)?;
    // Zero-value transfers ("address poisoning") never settle anything.
    if amount.is_zero() {
        debug!("Ignoring zero-amount transfer {} to address {}", transfer.tx_hash, address.id);
        return Ok(());
    }
    // A gas top-up of a sweep reaches the address from the treasury: the
    // gateway's own money, never a deposit.
    if existing.is_none() && state.db.is_gateway_outbound_tx(network, &transfer.tx_hash).await? {
        debug!("Ignoring the gateway's own transaction {} to address {}", transfer.tx_hash, address.id);
        return Ok(());
    }
    let is_confirmed = transfer.confirmations >= required;
    let deposit_status = if is_confirmed {
        DepositStatus::Confirmed
    } else {
        DepositStatus::Detected
    };

    let Some(recorded) = state
        .db
        .upsert_deposit_and_maybe_pay(
            &transfer.deposit_address_id,
            &transfer.tx_hash,
            amount,
            &transfer.amount_raw,
            transfer.confirmations,
            deposit_status,
            state.config.amount_tolerance_percent,
            projects::projects_exist(state).await?,
        )
        .await?
    else {
        return Ok(());
    };
    if recorded.deposit.late && existing.is_none() {
        info!(
            "Late deposit {} of {} {} to address {} ({})",
            recorded.deposit.id,
            amount.normalize(),
            token.as_api(),
            address.id,
            status.as_api()
        );
    }

    // The delivery rows of the address owner's endpoints subscribed to the
    // event were written with the status change (spec §10.1, §4.5): queue
    // them now — the reconciliation catches up a lost one.
    if let Some(event) = recorded.event {
        if event == WebhookEventType::Paid {
            info!("Address {} paid by its confirmed total", address.id);
        }
        queue_deliveries(state, event, &recorded.deposit.id, &recorded.deliveries).await;
    }

    Ok(())
}

/// Queue the deliveries a status change wrote (the fast path, after its
/// commit). A failed `ZADD` is only logged: the row is PENDING in the
/// database and `reconcile_webhook_deliveries` queues it again.
pub async fn queue_deliveries(state: &AppState, event: WebhookEventType, subject_id: &str, endpoints: &[String]) {
    for endpoint_id in endpoints {
        if let Err(err) = enqueue_webhook(state, event, subject_id, endpoint_id).await {
            warn!(
                "Failed to queue the {} webhook of {subject_id} for endpoint {endpoint_id} — the reconciliation will: {err:#}",
                event.as_api()
            );
        }
    }
}

/// Map a settled Lightning payment to its gateway invoice row. `None` for
/// invoices the gateway did not issue (the Breez wallet may receive other
/// funds) and rows that are not PENDING Lightning invoices. The tx hash is
/// the SDK payment id and a settled payment is final (1 confirmation);
/// `process_detected_transfer` upserts on (address, tx) so seeing the same
/// payment twice is a no-op.
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
    // Stored as `deposits.txHash`. SDK ids are Spark transfer UUIDs and never
    // enter webhook queue members (those key on deposits.id), so an odd shape
    // is flagged but the settled payment is still credited.
    if payment.payment_id.is_empty() || payment.payment_id.contains('|') {
        error!(
            "Lightning payment for address {} has an unexpected id shape {:?}",
            row.id, payment.payment_id
        );
    }
    Some(DetectedTransfer {
        deposit_address_id: row.id.clone(),
        tx_hash: payment.payment_id.clone(),
        amount_raw: payment.amount_sats.to_string(),
        confirmations: 1,
        // A fixed-amount invoice issued for this payment: never older.
        block_time: None,
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

/// Reconciliation matching: the transfers that settled `payments` make for
/// the `pending` invoices (unknown invoices are skipped).
pub fn reconciled_transfers(
    payments: &[ReceivedPayment],
    pending: &[DepositAddressRow],
) -> Vec<DetectedTransfer> {
    let by_invoice: HashMap<&str, &DepositAddressRow> =
        pending.iter().map(|a| (a.address.as_str(), a)).collect();
    payments
        .iter()
        .filter_map(|payment| {
            lightning_transfer(payment, by_invoice.get(payment.invoice.as_str()).copied())
        })
        .collect()
}

/// Reconciliation path (events missed across restarts): match settled
/// payments since the oldest of `pending` against those invoices.
async fn reconcile_lightning(state: &AppState, pending: &[DepositAddressRow]) -> Result<()> {
    let Some(lightning) = &state.lightning else {
        return Ok(());
    };
    let Some(oldest) = pending.iter().map(|a| a.created_at).min() else {
        return Ok(());
    };
    let since = u64::try_from(oldest.timestamp()).unwrap_or(0);
    let payments = lightning.list_received_since(since).await?;
    for transfer in reconciled_transfers(&payments, pending) {
        info!(
            "Lightning reconciliation: payment {} settles address {}",
            transfer.tx_hash, transfer.deposit_address_id
        );
        let tx_hash = transfer.tx_hash.clone();
        if let Err(err) = process_detected_transfer(state, transfer).await {
            error!("process Lightning payment {tx_hash} error: {err:#}");
        }
    }
    Ok(())
}

/// A wallet history entry, linked to the gateway payment (deposit address)
/// whose invoice it settled — `None` for funds unrelated to the gateway.
#[derive(Debug, Clone)]
pub struct LightningHistoryEntry {
    pub payment: WalletPayment,
    pub deposit_address_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct LightningHistoryPage {
    pub entries: Vec<LightningHistoryEntry>,
    pub has_more: bool,
}

/// Link each payment to its gateway invoice (`invoices`: BOLT11 → address
/// id), keeping the history order.
pub fn link_history(
    payments: Vec<WalletPayment>,
    invoices: &HashMap<String, String>,
) -> Vec<LightningHistoryEntry> {
    payments
        .into_iter()
        .map(|payment| {
            let deposit_address_id = payment
                .invoice
                .as_ref()
                .and_then(|invoice| invoices.get(invoice))
                .cloned();
            LightningHistoryEntry {
                payment,
                deposit_address_id,
            }
        })
        .collect()
}

/// A page of the whole Lightning wallet history, newest first. `None` when
/// Lightning is disabled.
pub async fn lightning_payment_history(
    state: &AppState,
    offset: u32,
    limit: u32,
) -> Result<Option<LightningHistoryPage>> {
    let Some(lightning) = &state.lightning else {
        return Ok(None);
    };
    let page = lightning.list_payments(offset, limit).await?;
    let invoices: Vec<String> = page
        .payments
        .iter()
        .filter_map(|p| p.invoice.clone())
        .collect();
    let links: HashMap<String, String> = state
        .db
        .lightning_invoice_ids(&invoices)
        .await?
        .into_iter()
        .collect();
    Ok(Some(LightningHistoryPage {
        entries: link_history(page.payments, &links),
        has_more: page.has_more,
    }))
}

/// Why a Lightning wallet operation could not be served.
#[derive(Debug)]
pub enum LightningError {
    /// Lightning is not configured (no key, or the SDK did not connect).
    Disabled,
    /// The SDK (or the DB) failed; the detail is for the logs only.
    Failed(anyhow::Error),
}

impl From<anyhow::Error> for LightningError {
    fn from(value: anyhow::Error) -> Self {
        Self::Failed(value)
    }
}

/// One wallet payment (any direction) linked to its gateway invoice, if any.
pub async fn lightning_payment_entry(
    state: &AppState,
    payment_id: &str,
) -> Result<Option<LightningHistoryEntry>, LightningError> {
    let lightning = state.lightning.as_ref().ok_or(LightningError::Disabled)?;
    let Some(payment) = lightning.get_payment(payment_id).await? else {
        return Ok(None);
    };
    let invoices: Vec<String> = payment.invoice.iter().cloned().collect();
    let links: HashMap<String, String> = state
        .db
        .lightning_invoice_ids(&invoices)
        .await?
        .into_iter()
        .collect();
    Ok(link_history(vec![payment], &links).pop())
}

/// The payout webhook a finished wallet send triggers; `None` while it is
/// still pending, and for anything that is not a send.
pub fn payout_event_for(payment: &WalletPayment) -> Option<WebhookEventType> {
    if payment.direction != PaymentDirection::Send {
        return None;
    }
    match payment.status {
        WalletPaymentStatus::Completed => Some(WebhookEventType::PayoutSucceeded),
        WalletPaymentStatus::Failed => Some(WebhookEventType::PayoutFailed),
        WalletPaymentStatus::Pending => None,
    }
}

/// Where the boot catch-up starts: the watermark minus a safety margin.
/// `None` on first boot (no watermark yet): history is never replayed.
pub fn payout_catch_up_from(watermark: Option<u64>) -> Option<u64> {
    watermark.map(|ts| ts.saturating_sub(LIGHTNING_PAYOUT_CATCH_UP_MARGIN_SECS))
}

/// Queue the payout webhook of a finished send for every subscribed
/// endpoint — every wallet send counts, whoever made it (the wallet may be
/// shared). Keys already queued or delivered are skipped, so events,
/// SDK replays and the boot catch-up never double-notify.
pub async fn notify_payout(state: &AppState, payment: &WalletPayment) -> Result<usize> {
    let Some(event) = payout_event_for(payment) else {
        return Ok(0);
    };
    // `|` separates queue member fields: such an id could never be decoded.
    if payment.id.is_empty() || payment.id.contains('|') {
        error!("Payout webhook skipped — unexpected payment id shape {:?}", payment.id);
        return Ok(0);
    }
    // Gateway withdrawals (and legacy payout requests) are reported by
    // their own webhooks: `payout_*` is for sends the gateway did not make.
    if state.db.is_gateway_send(&payment.id).await? {
        return Ok(0);
    }
    let mut queued = 0;
    for endpoint in webhooks::endpoints_for(state, event, &webhooks::SubjectOwners::merchant()).await? {
        if state
            .db
            .find_payout_webhook_delivery(&payment.id, event, &endpoint.id)
            .await?
            .is_some()
        {
            continue;
        }
        enqueue_webhook(state, event, &payment.id, &endpoint.id).await?;
        queued += 1;
    }
    Ok(queued)
}

/// Webhook body of a payout event.
pub fn payout_payload(event: WebhookEventType, payment: &WalletPayment) -> serde_json::Value {
    serde_json::json!({
        "event_type": event.payload_event_type(),
        "data": {
            "payment_id": payment.id,
            "status": payment.status.as_api(),
            "network": Network::Lightning.as_api(),
            "token": Token::Btc.as_api(),
            "amount": format_raw(payment.amount_sats, Token::Btc),
            "amount_sats": payment.amount_sats.to_string(),
            "fees_sats": payment.fees_sats.to_string(),
            "method": payment.method,
            "ln_address": payment.ln_address,
            "comment": payment.comment,
            "invoice": payment.invoice,
            "created_at": i64::try_from(payment.timestamp)
                .ok()
                .and_then(|ts| chrono::DateTime::from_timestamp(ts, 0))
                .map(|dt| dt.to_rfc3339()),
        },
    })
}

/// Boot-time catch-up: notify sends that finished while the gateway was
/// down, then move the watermark to now. First boot only sets it.
async fn catch_up_payouts(state: &AppState, lightning: &LightningClient) -> Result<()> {
    let key = state.config.redis_key(PAYOUT_WATERMARK_KEY);
    let mut redis = state.redis.clone();
    let watermark: Option<u64> = redis.get(&key).await?;
    let now = u64::try_from(chrono::Utc::now().timestamp()).unwrap_or(0);
    if watermark.is_none() {
        warn!(
            "No payout watermark — first boot or lost Redis data: payout webhooks start from now"
        );
    }
    if let Some(from) = payout_catch_up_from(watermark) {
        let mut queued = 0;
        for payment in lightning.list_sends_since(from).await? {
            queued += notify_payout(state, &payment).await?;
        }
        if queued > 0 {
            info!("Payout catch-up queued {queued} webhook(s)");
        }
    }
    let _: () = redis.set(&key, now).await?;
    Ok(())
}

/// Expire `network`'s addresses whose deadline (plus the grace period) has
/// passed — never one with a DETECTED deposit still confirming — and queue
/// the EXPIRED deliveries written with each (spec §4.5). Called by the
/// network's monitor loop right *after* a polling pass, so every address
/// gets a last poll before it expires. Returns the number of addresses
/// expired. `polled`: see `Db::expire_stale_addresses`.
pub async fn expire_addresses_and_notify(
    state: &AppState,
    network: Network,
    polled: Option<&[String]>,
) -> Result<u64> {
    let expired = state
        .db
        .expire_stale_addresses(
            network,
            ADDRESS_EXPIRY_GRACE_SECS,
            DETECTED_DEPOSIT_HOLD_SECS,
            polled,
            projects::projects_exist(state).await?,
        )
        .await?;
    for address in &expired {
        queue_deliveries(state, WebhookEventType::Expired, &address.id, &address.deliveries).await;
    }
    Ok(expired.len() as u64)
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

/// What a webhook queue member turned out to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueMember<'a> {
    Delivery {
        event: WebhookEventType,
        subject_id: &'a str,
        endpoint_id: &'a str,
    },
    /// Bare deposit id written before per-endpoint deliveries.
    Legacy(&'a str),
    /// Undecodable: dropped (re-queueing it would loop forever).
    Malformed,
}

pub fn classify_queue_member(member: &str) -> QueueMember<'_> {
    if let Some((event, subject_id, endpoint_id)) = decode_queue_member(member) {
        return QueueMember::Delivery {
            event,
            subject_id,
            endpoint_id,
        };
    }
    if !member.is_empty() && !member.contains('|') && !member.chars().any(char::is_whitespace) {
        QueueMember::Legacy(member)
    } else {
        QueueMember::Malformed
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
    /// No FAILED delivery to replay (merchant replays never resend a
    /// delivered webhook).
    NothingToRetry,
    /// A confirmed deposit whose confirmation fired no event (a partial
    /// transfer, ADR 0005), or not the event asked for.
    NoDepositEvent,
    Other(anyhow::Error),
}

impl From<anyhow::Error> for RetriggerError {
    fn from(value: anyhow::Error) -> Self {
        Self::Other(value)
    }
}

/// The event a deposit's confirmation fired (`deposits."webhookEvent"`):
/// `paid` for the deposit that paid its address — only that one —, `late`
/// for one confirmed once its address had left PENDING, none for a partial
/// transfer. A replay sends that event and nothing else.
pub fn deposit_replay_event(deposit: &db::DepositRow) -> Option<WebhookEventType> {
    deposit
        .webhook_event
        .as_deref()
        .and_then(|event| WebhookEventType::from_db(event).ok())
        .filter(|event| event.subject() == WebhookSubject::Deposit)
}

/// Manual replay for the dashboard: for every active endpoint of the
/// deposit's owner subscribed to the event its confirmation fired
/// (`deposit_replay_event`; none → `NoDepositEvent`), reset the delivery
/// row to PENDING / 0 attempts, then schedule an immediate attempt. Never
/// touches `deposits` or `deposit_addresses`.
pub async fn retrigger_webhook(state: &AppState, deposit_id: &str) -> Result<(), RetriggerError> {
    let deposit = state
        .db
        .find_deposit(deposit_id)
        .await?
        .ok_or(RetriggerError::DepositNotFound)?;
    if deposit.status != DepositStatus::Confirmed.as_db() {
        return Err(RetriggerError::DepositNotConfirmed);
    }
    let event = deposit_replay_event(&deposit).ok_or(RetriggerError::NoDepositEvent)?;
    let endpoints = webhooks::endpoints_for_subject(state, event, &deposit.id)
        .await?
        .ok_or(RetriggerError::DepositNotFound)?;
    if endpoints.is_empty() {
        return Err(RetriggerError::NoActiveEndpoints);
    }

    for endpoint in &endpoints {
        state.db.reset_deposit_webhook_delivery(event, &deposit.id, &endpoint.id).await?;
        enqueue_webhook(state, event, &deposit.id, &endpoint.id).await?;
    }
    info!(
        "Webhook retriggered for deposit {deposit_id} ({}) towards {} endpoint(s)",
        event.as_api(),
        endpoints.len()
    );
    Ok(())
}

/// Merchant replay of a confirmed deposit's webhooks: only the deliveries
/// that ended FAILED, and only towards endpoints still active and
/// subscribed to their event — a delivered webhook is never sent again from
/// the merchant API. Returns the number of deliveries re-queued.
pub async fn retrigger_failed_deposit_webhooks(
    state: &AppState,
    deposit_id: &str,
) -> Result<usize, RetriggerError> {
    let deposit = state
        .db
        .find_deposit(deposit_id)
        .await?
        .ok_or(RetriggerError::DepositNotFound)?;
    if deposit.status != DepositStatus::Confirmed.as_db() {
        return Err(RetriggerError::DepositNotConfirmed);
    }
    let mut retried = 0;
    // The deposit owner's endpoints only (spec §10.1), per event.
    let mut eligible: Vec<(WebhookEventType, Vec<String>)> = Vec::new();
    for delivery in state.db.failed_deposit_deliveries(&deposit.id).await? {
        let Some(endpoint_id) = delivery.webhook_endpoint_id.as_deref() else {
            continue;
        };
        let Ok(event) = WebhookEventType::from_db(&delivery.event_type) else {
            continue;
        };
        if !eligible.iter().any(|(known, _)| *known == event) {
            let endpoints = webhooks::endpoints_for_subject(state, event, &deposit.id)
                .await?
                .ok_or(RetriggerError::DepositNotFound)?;
            eligible.push((event, endpoints.into_iter().map(|endpoint| endpoint.id).collect()));
        }
        let allowed = eligible
            .iter()
            .any(|(known, ids)| *known == event && ids.iter().any(|id| id == endpoint_id));
        if !allowed {
            continue;
        }
        state.db.reset_webhook_delivery_by_id(&delivery.id).await?;
        enqueue_webhook(state, event, &deposit.id, endpoint_id).await?;
        retried += 1;
    }
    if retried == 0 {
        return Err(RetriggerError::NothingToRetry);
    }
    info!("Merchant replay re-queued {retried} failed webhook(s) for deposit {deposit_id}");
    Ok(retried)
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

    let subject_id = match event.subject() {
        WebhookSubject::Deposit => {
            let deposit_id = delivery.deposit_id.ok_or(RetriggerError::DeliveryNotFound)?;
            let deposit = state
                .db
                .find_deposit(&deposit_id)
                .await?
                .ok_or(RetriggerError::DepositNotFound)?;
            if deposit.status != DepositStatus::Confirmed.as_db() {
                return Err(RetriggerError::DepositNotConfirmed);
            }
            Some(deposit_id)
        }
        WebhookSubject::LightningPayment => delivery.lightning_payment_id.clone(),
        WebhookSubject::Withdrawal => delivery.withdrawal_id.clone(),
        WebhookSubject::Alert => delivery.alert_id.clone(),
        WebhookSubject::Transfer => delivery.transfer_id.clone(),
        WebhookSubject::DepositAddress => delivery.deposit_address_id.clone(),
    }
    .ok_or(RetriggerError::DeliveryNotFound)?;
    // Active, subscribed and owned by the subject's owner (spec §10.1).
    match webhooks::check_endpoint(state, &endpoint_id, event, &subject_id).await? {
        Ok(_) => {}
        Err(webhooks::Ineligible::SubjectGone) => return Err(RetriggerError::DeliveryNotFound),
        Err(_) => return Err(RetriggerError::EndpointNotEligible),
    }

    state.db.reset_webhook_delivery_by_id(&delivery.id).await?;
    enqueue_webhook(state, event, &subject_id, &endpoint_id).await?;
    info!("Webhook delivery {delivery_id} retriggered ({} event)", event.as_api());
    Ok(())
}

/// Bulk manual replay: re-queue every delivery key whose authoritative
/// delivery is FAILED, towards endpoints still active, subscribed and owned
/// by the subject's owner (spec §10.1); legacy rows (no endpoint) fan out
/// to the owner's PAID endpoints. Individual failures are logged and
/// skipped; returns the number of keys successfully re-queued.
pub async fn retrigger_failed_webhooks(state: &AppState) -> Result<usize, RetriggerError> {
    if state.db.active_webhook_endpoints().await?.is_empty() {
        return Err(RetriggerError::NoActiveEndpoints);
    }

    let targets = state.db.failed_webhook_targets().await?;
    let mut retried = 0usize;
    for target in &targets {
        let Ok(event) = WebhookEventType::from_db(&target.event_type) else {
            warn!("Bulk webhook retry skipped unknown event type {}", target.event_type);
            continue;
        };
        let Some(subject_id) = target.subject_id(event) else {
            continue;
        };
        let target_endpoints: Vec<String> = match &target.webhook_endpoint_id {
            Some(id) => match webhooks::check_endpoint(state, id, event, subject_id).await? {
                Ok(_) => vec![id.clone()],
                Err(reason) => {
                    debug!("Bulk webhook retry skipped {} {subject_id} endpoint {id}: {reason:?}", event.as_api());
                    continue;
                }
            },
            // Legacy failure — replay towards the owner's active PAID endpoints.
            None => webhooks::endpoints_for_subject(state, event, subject_id)
                .await?
                .unwrap_or_default()
                .into_iter()
                .map(|e| e.id)
                .collect(),
        };
        for endpoint_id in target_endpoints.iter().map(String::as_str) {
            let result: Result<()> = async {
                if event.subject() == WebhookSubject::Deposit {
                    state.db.reset_deposit_webhook_delivery(event, subject_id, endpoint_id).await?;
                } else {
                    let row = state
                        .db
                        .get_or_create_subject_webhook_delivery(event, subject_id, endpoint_id)
                        .await?;
                    state.db.reset_webhook_delivery_by_id(&row.id).await?;
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

/// Deliver one queued webhook. `subject_id` is a deposit id for PAID and
/// LATE, a deposit-address id for PENDING/EXPIRED, a Lightning payment id for
/// payout events, a withdrawal id for withdrawal events and an alert id for
/// alert events (`WebhookEventType::subject`).
pub async fn deliver_webhook(
    state: &AppState,
    event: WebhookEventType,
    subject_id: &str,
    endpoint_id: &str,
) -> Result<DeliveryOutcome> {
    // Re-check the endpoint on every attempt — active, subscribed and owned
    // by the subject's owner (spec §10.1) — so a deactivation, an
    // unsubscription or a mismatch takes effect immediately: the queued
    // member is consumed without being rescheduled, and its PENDING row is
    // closed.
    let endpoint = match webhooks::check_endpoint(state, endpoint_id, event, subject_id).await? {
        Ok(endpoint) => endpoint,
        Err(reason) => {
            let message = match reason {
                webhooks::Ineligible::EndpointUnavailable => "Endpoint inactive, unsubscribed or deleted",
                webhooks::Ineligible::OwnerMismatch => "Endpoint belongs to another owner",
                webhooks::Ineligible::SubjectGone => "Subject not found",
            };
            warn!(
                "Webhook delivery skipped for {} {subject_id} — endpoint {endpoint_id}: {message}",
                event.as_api()
            );
            close_undeliverable(state, event, subject_id, endpoint_id, message).await?;
            return Ok(DeliveryOutcome::Skipped);
        }
    };
    let subject_label = match event.subject() {
        WebhookSubject::Deposit => format!("deposit {subject_id}"),
        WebhookSubject::LightningPayment => format!("payout {subject_id} ({})", event.as_api()),
        WebhookSubject::Withdrawal => format!("withdrawal {subject_id} ({})", event.as_api()),
        WebhookSubject::Alert => format!("alert {subject_id} ({})", event.as_api()),
        WebhookSubject::Transfer => format!("transfer {subject_id}"),
        WebhookSubject::DepositAddress => format!("address {subject_id} ({})", event.as_api()),
    };

    let Some((delivery, mut payload, occurred_at)) =
        prepare_delivery(state, event, subject_id, endpoint_id).await?
    else {
        // The subject is gone (or Lightning is off): retrying forever would
        // only spin. Payout webhooks are re-queued by the boot catch-up.
        error!("Webhook delivery dropped for {subject_label} — subject not found");
        close_undeliverable(state, event, subject_id, endpoint_id, "Subject not found").await?;
        return Ok(DeliveryOutcome::Skipped);
    };

    if delivery.status == WebhookDeliveryStatus::Delivered.as_db() {
        return Ok(DeliveryOutcome::Delivered);
    }
    // Only a manual retry reopens a FAILED row, and it resets it to PENDING
    // before queueing: a member met for a FAILED row is stale (e.g. queued
    // again by the reconciliation from a read taken just before the last
    // attempt failed) and must not add an attempt past the budget.
    if delivery.status == WebhookDeliveryStatus::Failed.as_db() {
        debug!("Webhook delivery {} is closed (failed) — stale queue member dropped", delivery.id);
        return Ok(DeliveryOutcome::Failed);
    }

    // Envelope: a stable id receivers deduplicate on (the delivery row is
    // reused across retries) and when the event happened (deliveries are
    // not ordered).
    if let Some(object) = payload.as_object_mut() {
        object.insert("event_id".into(), serde_json::json!(delivery.id));
        object.insert("occurred_at".into(), serde_json::json!(occurred_at.to_rfc3339()));
    }

    // Serialize exactly once: the signature must cover the exact bytes sent.
    let body = serde_json::to_vec(&payload)?;
    let request = match state.webhooks.post(&endpoint.url) {
        Ok(request) => request,
        Err(reason) => {
            // Stored before the URL rules were tightened: never sent.
            warn!("Webhook to endpoint {endpoint_id} not sent: {reason}");
            return record_failed_attempt(state, &delivery, None, reason, payload, &subject_label)
                .await;
        }
    };
    let timestamp = chrono::Utc::now().timestamp();
    let mut request = request
        .header("Content-Type", "application/json")
        .header("X-Webhook-Id", &delivery.id)
        .header("X-Webhook-Timestamp", timestamp.to_string());
    if let Some(secret) = endpoint.secret.as_deref().filter(|s| !s.is_empty()) {
        request = request
            .header("X-Webhook-Signature", auth::webhook_signature(secret, &body))
            .header(
                "X-Webhook-Signature-V2",
                auth::webhook_signature_v2(secret, timestamp, &body),
            );
    }
    match request.body(body).send().await {
        // Redirects are not followed: a 3xx lands in the failure branch.
        Ok(resp) if resp.status().is_success() => {
            let status = resp.status().as_u16() as i32;
            state
                .db
                .update_webhook_delivery(
                    &delivery.id,
                    WebhookDeliveryStatus::Delivered,
                    delivery.attempts + 1,
                    Some(status),
                    None,
                    Some(payload),
                )
                .await?;
            info!("Webhook delivered for {subject_label}");
            Ok(DeliveryOutcome::Delivered)
        }
        Ok(resp) => {
            let status = resp.status().as_u16() as i32;
            record_failed_attempt(
                state,
                &delivery,
                Some(status),
                format!("HTTP {status}"),
                payload,
                &subject_label,
            )
            .await
        }
        // `without_url`: the endpoint URL (query tokens…) must not land in
        // `lastError`, which the dashboard displays.
        Err(err) => {
            let error = err.without_url().to_string();
            record_failed_attempt(state, &delivery, None, error, payload, &subject_label).await
        }
    }
}

/// The delivery row, its payload and when the event happened — `None` when
/// the subject no longer exists.
async fn prepare_delivery(
    state: &AppState,
    event: WebhookEventType,
    subject_id: &str,
    endpoint_id: &str,
) -> Result<Option<(db::WebhookDeliveryRow, serde_json::Value, chrono::DateTime<chrono::Utc>)>> {
    match event {
        WebhookEventType::Paid | WebhookEventType::Late => {
            let Some(deposit) = state.db.find_deposit(subject_id).await? else {
                return Ok(None);
            };
            let Some(address) = state.db.find_deposit_address(&deposit.deposit_address_id).await?
            else {
                return Ok(None);
            };
            let delivery = state
                .db
                .get_or_create_deposit_webhook_delivery(event, subject_id, endpoint_id)
                .await?;
            let received_raw = received_snapshot(state, &delivery, &address.id).await?;
            let payload = deposit_event_payload(event, &address, &deposit, received_raw)?;
            let occurred_at = deposit.confirmed_at.unwrap_or(deposit.detected_at);
            Ok(Some((delivery, payload, occurred_at)))
        }
        WebhookEventType::Pending | WebhookEventType::Expired => {
            let Some(address) = state.db.find_deposit_address(subject_id).await? else {
                return Ok(None);
            };
            let delivery = state
                .db
                .get_or_create_address_webhook_delivery(subject_id, event, endpoint_id)
                .await?;
            let occurred_at = match event {
                WebhookEventType::Pending => address.created_at,
                _ => address.updated_at,
            };
            let received_raw = match event {
                WebhookEventType::Expired => Some(received_snapshot(state, &delivery, &address.id).await?),
                _ => None,
            };
            Ok(Some((delivery, address_event_payload(event, &address, received_raw)?, occurred_at)))
        }
        WebhookEventType::PayoutSucceeded | WebhookEventType::PayoutFailed => {
            let Some(lightning) = &state.lightning else {
                // Dropped rather than retried forever: once Lightning is back,
                // the boot catch-up re-queues it.
                warn!("Payout webhook {subject_id} dropped — Lightning is not configured");
                return Ok(None);
            };
            let Some(payment) = lightning.get_payment(subject_id).await? else {
                return Ok(None);
            };
            let delivery = state
                .db
                .get_or_create_payout_webhook_delivery(subject_id, event, endpoint_id)
                .await?;
            let occurred_at = i64::try_from(payment.timestamp)
                .ok()
                .and_then(|ts| chrono::DateTime::from_timestamp(ts, 0))
                .unwrap_or_else(chrono::Utc::now);
            Ok(Some((delivery, payout_payload(event, &payment), occurred_at)))
        }
        event if event.subject() == WebhookSubject::Withdrawal => {
            let Some(row) = state.db.find_withdrawal(subject_id).await? else {
                return Ok(None);
            };
            let delivery = state
                .db
                .get_or_create_subject_webhook_delivery(event, subject_id, endpoint_id)
                .await?;
            let occurred_at = match event {
                WebhookEventType::WithdrawalPendingApproval => row.created_at,
                WebhookEventType::WithdrawalBroadcast => row.broadcast_at.unwrap_or(row.updated_at),
                WebhookEventType::WithdrawalConfirmed => row.confirmed_at.unwrap_or(row.updated_at),
                _ => row.updated_at,
            };
            Ok(Some((delivery, withdrawals::withdrawal_payload(event, &row)?, occurred_at)))
        }
        WebhookEventType::TransferCompleted => {
            let Some(row) = state.db.find_internal_transfer(subject_id).await? else {
                return Ok(None);
            };
            let delivery = state
                .db
                .get_or_create_subject_webhook_delivery(event, subject_id, endpoint_id)
                .await?;
            Ok(Some((delivery, transfers::transfer_payload(&row)?, row.created_at)))
        }
        _ => {
            let Some(alert) = state.db.find_alert(subject_id).await? else {
                return Ok(None);
            };
            let delivery = state
                .db
                .get_or_create_subject_webhook_delivery(event, subject_id, endpoint_id)
                .await?;
            Ok(Some((delivery, alerts::alert_payload(&alert)?, alert.created_at)))
        }
    }
}

/// The `received_amount_raw` a delivery sends on every attempt: the
/// snapshot taken when its row was written (in the status transaction), or —
/// for a row without one — the confirmed total now, stored for the next
/// attempts.
async fn received_snapshot(state: &AppState, delivery: &db::WebhookDeliveryRow, address_id: &str) -> Result<u128> {
    match delivery.received_raw {
        Some(snapshot) => Ok(snapshot.0),
        None => {
            let now = state.db.confirmed_total_raw(address_id).await?;
            state.db.snapshot_received_raw(&delivery.id, now).await
        }
    }
}

/// One failed HTTP attempt: PENDING with a retry scheduled while budget
/// remains, FAILED afterwards. Only the last error is kept.
async fn record_failed_attempt(
    state: &AppState,
    delivery: &db::WebhookDeliveryRow,
    status: Option<i32>,
    error: String,
    payload: serde_json::Value,
    subject_label: &str,
) -> Result<DeliveryOutcome> {
    let attempts = delivery.attempts + 1;
    let exhausted = attempts >= WEBHOOK_MAX_ATTEMPTS as i32;
    state
        .db
        .update_webhook_delivery(
            &delivery.id,
            if exhausted {
                WebhookDeliveryStatus::Failed
            } else {
                WebhookDeliveryStatus::Pending
            },
            attempts,
            status,
            Some(error.clone()),
            Some(payload),
        )
        .await?;
    if exhausted {
        error!("Webhook delivery failed after {attempts} attempts for {subject_label}");
        return Ok(DeliveryOutcome::Failed);
    }
    warn!("Webhook attempt {attempts} failed for {subject_label}: {error}");
    Ok(DeliveryOutcome::RetryAt { attempts })
}

/// Close the existing PENDING row of a delivery that will never be sent.
async fn close_undeliverable(
    state: &AppState,
    event: WebhookEventType,
    subject_id: &str,
    endpoint_id: &str,
    reason: &str,
) -> Result<()> {
    let row = match event.subject() {
        WebhookSubject::Deposit => {
            state
                .db
                .find_deposit_webhook_delivery(event, subject_id, endpoint_id)
                .await?
        }
        _ => {
            state
                .db
                .find_subject_webhook_delivery(event, subject_id, endpoint_id)
                .await?
        }
    };
    if let Some(row) = row {
        state.db.fail_pending_webhook_delivery(&row.id, reason).await?;
    }
    Ok(())
}

/// Payload of a deposit event: `paid` (`deposit_confirmed`, the historical
/// shape — `data.deposit_id` is deposit_addresses.id on purpose, see
/// AGENTS.md; existing receivers depend on it) and `late` (`deposit_late`,
/// the same plus `address_status`). Both carry the expected amount and the
/// confirmed total of the address in base units (ADR 0005).
pub fn deposit_event_payload(
    event: WebhookEventType,
    address: &DepositAddressRow,
    deposit: &db::DepositRow,
    received_raw: u128,
) -> Result<serde_json::Value> {
    let network = Network::from_db(&address.network)?;
    let token = Token::from_db(&address.token)?;
    let mut payload = serde_json::json!({
        "event_type": event.payload_event_type(),
        "data": {
            "deposit_id": deposit.deposit_address_id,
            "project_id": address.project_id,
            "address": address.address,
            "network": network.as_api(),
            "token": token.as_api(),
            "amount": format_amount(deposit.amount, token),
            "amount_raw": deposit.amount_raw,
            "tx_hash": deposit.tx_hash,
            "confirmations": deposit.confirmations,
            "status": "confirmed",
            "expected_amount_raw": decimal_to_raw(address.expected_amount, token),
            "received_amount_raw": received_raw.to_string(),
        },
    });
    if event == WebhookEventType::Late {
        let status = DepositAddressStatus::from_db(&address.status)?;
        payload["data"]["address_status"] = serde_json::json!(status.as_api());
    }
    Ok(payload)
}

/// Payload of an address-scoped event. Mirrors the PAID payload's naming:
/// `data.deposit_id` is deposit_addresses.id. `expired` also carries the
/// confirmed total (`received_raw`, a partial payment).
fn address_event_payload(
    event: WebhookEventType,
    address: &DepositAddressRow,
    received_raw: Option<u128>,
) -> Result<serde_json::Value> {
    let network = Network::from_db(&address.network)?;
    let token = Token::from_db(&address.token)?;
    let mut payload = serde_json::json!({
        "event_type": event.payload_event_type(),
        "data": {
            "deposit_id": address.id,
            "project_id": address.project_id,
            "address": address.address,
            "network": network.as_api(),
            "token": token.as_api(),
            "expected_amount": address.expected_amount.normalize().to_string(),
            "status": event.as_api(),
            "expires_at": address.expires_at.to_rfc3339(),
            "created_at": address.created_at.to_rfc3339(),
        },
    });
    if let Some(received_raw) = received_raw {
        payload["data"]["received_amount_raw"] = serde_json::json!(received_raw.to_string());
    }
    Ok(payload)
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

    // One loop per network, enabled or not: a disabled network still gets
    // its (hard-stop) expiry sweep.
    let poll_secs = state.config.polling_interval_seconds.max(1);
    for network in Network::ALL {
        let s = state.clone();
        tokio::spawn(async move { monitor_loop(s, network, poll_secs).await });
    }
    if let Some(lightning) = state.lightning.clone() {
        let s = state.clone();
        tokio::spawn(async move { lightning_event_loop(s, lightning).await });
    }
    let s4 = state.clone();
    tokio::spawn(async move { webhook_loop(s4).await });
    let s9 = state.clone();
    tokio::spawn(async move { webhook_reconciliation_loop(s9).await });
    let s5 = state.clone();
    tokio::spawn(async move { wallet_balance_sync_loop(s5).await });
    let s6 = state.clone();
    tokio::spawn(async move { api_key_expiry_loop(s6).await });

    // Withdrawals, sweeps and treasuries.
    for network in Network::ALL {
        let s = state.clone();
        tokio::spawn(async move { outbound::outbound_loop(s, network).await });
    }
    for network in sweeper::SWEPT_NETWORKS {
        let s = state.clone();
        tokio::spawn(async move { sweeper::sweep_loop(s, network).await });
    }
    let s7 = state.clone();
    tokio::spawn(async move { treasury::treasury_monitor_loop(s7).await });
    let s8 = state.clone();
    tokio::spawn(async move { withdrawals::approval_expiry_loop(s8).await });
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

/// Lightning wallet events: received payments settle gateway invoices
/// (primary detection), finished sends trigger payout webhooks. The SDK
/// callback only enqueues; the work runs here, off the SDK dispatch.
async fn lightning_event_loop(state: AppState, lightning: LightningClient) {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<LightningEvent>();
    let listener_id = lightning
        .subscribe(move |event| {
            let _ = tx.send(event);
        })
        .await;
    info!("Lightning payment listener registered ({listener_id})");

    // Subscribed first, so nothing finishing during the catch-up is missed.
    let caught_up = match catch_up_payouts(&state, &lightning).await {
        Ok(()) => true,
        Err(err) => {
            warn!("Payout webhook catch-up failed, retrying on the next watermark tick: {err:#}");
            false
        }
    };
    let watermark_state = state.clone();
    let watermark_client = lightning.clone();
    tokio::spawn(async move {
        payout_watermark_loop(watermark_state, watermark_client, caught_up).await
    });

    while let Some(event) = rx.recv().await {
        match event {
            LightningEvent::Received(payment) => {
                let payment_id = payment.payment_id.clone();
                if let Err(err) = handle_lightning_payment(&state, payment).await {
                    // The reconciliation loop picks it up on its next tick.
                    error!("Lightning payment {payment_id} processing error: {err:#}");
                }
            }
            LightningEvent::SendFinished(payment) => {
                if let Err(err) = outbound::lightning::on_send_finished(&state, &payment).await {
                    // The reconciliation pass settles it on its next tick.
                    error!("Lightning withdrawal update error for {}: {err:#}", payment.id);
                }
                if let Err(err) = notify_payout(&state, &payment).await {
                    // The next boot catch-up picks it up.
                    error!("Payout webhook queueing error for {}: {err:#}", payment.id);
                }
            }
        }
    }
}

/// Keep the payout watermark close to now while running, so a restart only
/// re-lists the downtime (plus the margin). Until the boot catch-up has
/// succeeded the watermark is left alone and the catch-up retried — moving
/// it first would lose the downtime for good.
async fn payout_watermark_loop(state: AppState, lightning: LightningClient, mut caught_up: bool) {
    let key = state.config.redis_key(PAYOUT_WATERMARK_KEY);
    let mut interval =
        tokio::time::interval(Duration::from_secs(LIGHTNING_PAYOUT_WATERMARK_INTERVAL_SECS));
    interval.tick().await;
    loop {
        interval.tick().await;
        if !caught_up {
            match catch_up_payouts(&state, &lightning).await {
                Ok(()) => caught_up = true,
                Err(err) => warn!("Payout webhook catch-up retry failed: {err:#}"),
            }
            continue;
        }
        let now = chrono::Utc::now().timestamp().max(0);
        let mut redis = state.redis.clone();
        if let Err(err) = redis.set::<_, _, ()>(&key, now).await {
            warn!("Payout watermark update failed: {err}");
        }
    }
}

async fn monitor_loop(state: AppState, network: Network, poll_secs: u64) {
    let mut interval = tokio::time::interval(Duration::from_secs(poll_secs));
    // A pass longer than the interval must not trigger a burst of passes.
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let late_every = Duration::from_secs(state.config.late_watch_poll_seconds);
    let mut last_late_pass: Option<std::time::Instant> = None;
    loop {
        interval.tick().await;
        let include_late = late_pass_due(last_late_pass, std::time::Instant::now(), late_every);
        if include_late {
            last_late_pass = Some(std::time::Instant::now());
        }
        if let Err(err) = poll_network(&state, network, include_late).await {
            error!("{network:?} monitor error: {err:#}");
        }
    }
}

/// Whether a monitor pass also polls the addresses in their late watch:
/// the first pass, then once `every` (`LATE_WATCH_POLL_SECONDS`) has passed
/// since the last one that did. Open payments are polled every pass.
pub fn late_pass_due(last: Option<std::time::Instant>, now: std::time::Instant, every: Duration) -> bool {
    last.is_none_or(|last| now.saturating_duration_since(last) >= every)
}

/// Poll `network`'s watched payments (the open ones, then those in their
/// late watch — ADR 0005), then expire the stale ones: the sweep runs only
/// after the poll, so a payment always gets a last look before
/// it expires (no boot-time race either — the first pass of every loop
/// polls before it expires anything).
/// `include_late`: also the paid and expired addresses in their late watch
/// (`late_pass_due`).
async fn poll_network(state: &AppState, network: Network, include_late: bool) -> Result<()> {
    if !state.is_network_enabled(network) {
        debug!(
            "Skipping {:?} monitor — {}",
            network,
            state.network_disabled_message(network)
        );
        // Nothing can be polled: only the hard stop applies, so payments
        // made while the network was down are still reconciled once it is
        // back (within the hold window).
        expire_addresses_and_notify(state, network, Some(&[])).await?;
        return Ok(());
    }

    let addresses = state.db.watched_addresses(network, include_late).await?;
    if network == Network::Lightning {
        // A failed reconciliation returns early: nothing expires unpolled.
        reconcile_lightning(state, &addresses).await?;
        expire_addresses_and_notify(state, network, None).await?;
        return Ok(());
    }

    let mut polled = Vec::with_capacity(addresses.len());
    for (i, address) in addresses.iter().enumerate() {
        if i > 0 {
            tokio::time::sleep(Duration::from_millis(ADDRESS_REQUEST_DELAY_MS)).await;
        }
        let transfers = match network {
            Network::Tron => state.chain.poll_tron_address(&address.address).await,
            // Native ETH and USDT payments share the network: dispatch on
            // the token the address was issued for.
            Network::Ethereum if address.token == Token::Eth.as_db() => {
                state.chain.poll_ethereum_native(&address.address).await
            }
            Network::Ethereum => state.chain.poll_ethereum_address(&address.address).await,
            // New rows watch the owner's token account (spec §4.4).
            Network::Solana => state.chain.poll_solana_address(address.watched_account()).await,
            Network::Bitcoin => state.chain.poll_bitcoin_address(&address.address).await,
            // Reconciled above from the Breez payment list, not per address.
            Network::Lightning => Ok(vec![]),
        };

        match transfers {
            Ok(transfers) => {
                let mut processed = true;
                for t in transfers {
                    if let Err(err) = process_detected_transfer(
                        state,
                        DetectedTransfer {
                            deposit_address_id: address.id.clone(),
                            tx_hash: t.tx_hash,
                            amount_raw: t.amount_raw,
                            confirmations: t.confirmations,
                            block_time: t.block_time,
                        },
                    )
                    .await
                    {
                        processed = false;
                        error!("process transfer error for {}: {err:#}", address.id);
                    }
                }
                if processed {
                    polled.push(address.id.clone());
                }
            }
            Err(err) => {
                error!("{:?} monitor error for {}: {err:#}", network, address.id);
            }
        }
    }
    expire_addresses_and_notify(state, network, Some(&polled)).await?;
    Ok(())
}

/// Claim up to ARGV[3] due members: each moves from the queue to the
/// in-flight set (score = lease expiry). A member still leased elsewhere
/// is left in the queue — it is never delivered twice concurrently.
const CLAIM_SCRIPT: &str = r"
local due = redis.call('ZRANGEBYSCORE', KEYS[1], '-inf', ARGV[1], 'LIMIT', 0, tonumber(ARGV[3]) * 4)
local claimed = {}
for _, member in ipairs(due) do
  if #claimed >= tonumber(ARGV[3]) then break end
  local lease = redis.call('ZSCORE', KEYS[2], member)
  if (not lease) or tonumber(lease) <= tonumber(ARGV[1]) then
    redis.call('ZREM', KEYS[1], member)
    redis.call('ZADD', KEYS[2], ARGV[2], member)
    table.insert(claimed, member)
  end
end
return claimed
";

/// Release a claimed member; ARGV[2] (when not empty) reschedules it, keeping
/// an earlier score if the member was re-enqueued meanwhile.
const RELEASE_SCRIPT: &str = r"
redis.call('ZREM', KEYS[2], ARGV[1])
if ARGV[2] ~= '' then
  local current = redis.call('ZSCORE', KEYS[1], ARGV[1])
  if (not current) or tonumber(current) > tonumber(ARGV[2]) then
    redis.call('ZADD', KEYS[1], ARGV[2], ARGV[1])
  end
end
return 1
";

/// Members whose lease expired (worker died mid-delivery) go back to the
/// queue, due now.
const REQUEUE_EXPIRED_SCRIPT: &str = r"
local expired = redis.call('ZRANGEBYSCORE', KEYS[2], '-inf', ARGV[1])
for _, member in ipairs(expired) do
  redis.call('ZREM', KEYS[2], member)
  local current = redis.call('ZSCORE', KEYS[1], member)
  if (not current) or tonumber(current) > tonumber(ARGV[1]) then
    redis.call('ZADD', KEYS[1], ARGV[1], member)
  end
end
return #expired
";

/// Queue again the members ARGV lists (member, score pairs) that are
/// neither scheduled nor in flight — a `ZADD NX` that also skips a member a
/// worker holds. Returns how many were added.
const REQUEUE_MISSING_SCRIPT: &str = r"
local added = 0
for i = 1, #ARGV, 2 do
  local member = ARGV[i]
  if (not redis.call('ZSCORE', KEYS[1], member)) and (not redis.call('ZSCORE', KEYS[2], member)) then
    redis.call('ZADD', KEYS[1], ARGV[i + 1], member)
    added = added + 1
  end
end
return added
";

/// When the next attempt of a PENDING delivery is due (unix seconds): at
/// once before its first attempt (or after a manual reset), else the retry
/// gap after its last failed attempt (`updatedAt`).
pub fn delivery_due_at(attempts: i32, updated_at: chrono::DateTime<chrono::Utc>) -> i64 {
    let gap = next_retry_delay_secs(attempts).unwrap_or(0);
    updated_at.timestamp().saturating_add(i64::try_from(gap).unwrap_or(i64::MAX))
}

/// The queue member and score of a PENDING delivery whose next attempt is
/// due at `now`; `None` while it is not due, or for a row whose event is
/// unknown or whose subject is missing.
pub fn reconciled_member(row: &db::PendingDeliveryRow, now: i64) -> Option<(String, i64)> {
    let event = WebhookEventType::from_db(&row.event_type).ok()?;
    let subject_id = row.subject_id(event)?;
    let due = delivery_due_at(row.attempts, row.updated_at);
    (due <= now).then(|| (encode_queue_member(event, subject_id, &row.webhook_endpoint_id), due))
}

/// The reconciliation of the webhook queue (spec §4.5): every PENDING
/// delivery whose next attempt is due and that has no member in the queue
/// or in flight is queued again (touched in the last
/// `WEBHOOK_RECONCILE_MAX_AGE_SECS`: older ones are history). Deliveries are written in the transaction
/// of their status change and queued after its commit; this catches what a
/// crash or a failed `ZADD` in between lost. A stale read is harmless: a
/// member queued for a row delivered or failed meanwhile is dropped by
/// `deliver_webhook` without any request. Returns the members queued.
pub async fn reconcile_webhook_deliveries(state: &AppState) -> Result<usize> {
    let queue = state.config.redis_key(WEBHOOK_ZSET_KEY);
    let inflight = state.config.redis_key(WEBHOOK_INFLIGHT_KEY);
    let script = redis::Script::new(REQUEUE_MISSING_SCRIPT);
    let mut redis = state.redis.clone();
    let mut after: Option<String> = None;
    let mut queued = 0usize;
    loop {
        let page = state
            .db
            .pending_deliveries_page(after.as_deref(), WEBHOOK_RECONCILE_MAX_AGE_SECS, WEBHOOK_RECONCILE_BATCH)
            .await?;
        let Some(last) = page.last() else { break };
        after = Some(last.id.clone());
        let now = chrono::Utc::now().timestamp();
        let members: Vec<(String, i64)> = page.iter().filter_map(|row| reconciled_member(row, now)).collect();
        if !members.is_empty() {
            let mut invocation = script.key(&queue);
            invocation.key(&inflight);
            for (member, score) in &members {
                invocation.arg(member).arg(*score);
            }
            let added: i64 = invocation.invoke_async(&mut redis).await?;
            queued += usize::try_from(added).unwrap_or(0);
        }
        if page.len() < WEBHOOK_RECONCILE_BATCH as usize {
            break;
        }
    }
    Ok(queued)
}

/// Every `WEBHOOK_RECONCILE_INTERVAL_SECS`, `reconcile_webhook_deliveries`.
async fn webhook_reconciliation_loop(state: AppState) {
    let mut interval = tokio::time::interval(Duration::from_secs(WEBHOOK_RECONCILE_INTERVAL_SECS));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        interval.tick().await;
        match reconcile_webhook_deliveries(&state).await {
            Ok(0) => {}
            Ok(queued) => warn!("Webhook reconciliation queued {queued} lost delivery(ies) again"),
            Err(err) => error!("webhook reconciliation error: {err:#}"),
        }
    }
}

async fn webhook_loop(state: AppState) {
    if let Err(err) = drain_legacy_webhook_queue(&state).await {
        warn!("legacy webhook queue drain failed: {err:#}");
    }

    let queue = state.config.redis_key(WEBHOOK_ZSET_KEY);
    let inflight = state.config.redis_key(WEBHOOK_INFLIGHT_KEY);
    let claim = redis::Script::new(CLAIM_SCRIPT);
    let release = redis::Script::new(RELEASE_SCRIPT);
    let requeue = redis::Script::new(REQUEUE_EXPIRED_SCRIPT);
    let mut interval = tokio::time::interval(Duration::from_secs(1));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        interval.tick().await;
        let mut redis = state.redis.clone();
        let now = chrono::Utc::now().timestamp();

        match requeue
            .key(&queue)
            .key(&inflight)
            .arg(now)
            .invoke_async::<i64>(&mut redis)
            .await
        {
            Ok(0) => {}
            Ok(count) => warn!("{count} webhook delivery lease(s) expired — re-queued"),
            Err(err) => {
                error!("webhook lease recovery error: {err}");
                continue;
            }
        }

        let claimed: Vec<String> = match claim
            .key(&queue)
            .key(&inflight)
            .arg(now)
            .arg(now + WEBHOOK_CLAIM_LEASE_SECS)
            .arg(WEBHOOK_DELIVERY_CONCURRENCY)
            .invoke_async(&mut redis)
            .await
        {
            Ok(claimed) => claimed,
            Err(err) => {
                error!("webhook queue claim error: {err}");
                continue;
            }
        };
        if claimed.is_empty() {
            continue;
        }

        let mut deliveries = tokio::task::JoinSet::new();
        for member in claimed {
            let state = state.clone();
            deliveries.spawn(async move {
                let next = process_queue_member(&state, &member).await;
                (member, next)
            });
        }
        while let Some(joined) = deliveries.join_next().await {
            // A panicked task keeps its lease: the member comes back later.
            let Ok((member, next)) = joined else { continue };
            let next = next.map(|score| score.to_string()).unwrap_or_default();
            if let Err(err) = release
                .key(&queue)
                .key(&inflight)
                .arg(&member)
                .arg(next)
                .invoke_async::<i64>(&mut redis)
                .await
            {
                error!("webhook queue release error for {member}: {err}");
            }
        }
    }
}

/// Deliver one claimed member. Returns when to try it again (`None` = done).
async fn process_queue_member(state: &AppState, member: &str) -> Option<i64> {
    let retry_after = |secs: u64| chrono::Utc::now().timestamp() + secs as i64;
    match classify_queue_member(member) {
        QueueMember::Malformed => {
            error!("Dropping malformed webhook queue member {member:?}");
            None
        }
        QueueMember::Legacy(deposit_id) => {
            // Pre-multi-endpoint member: fan it out to the deposit owner's
            // active PAID-subscribed endpoints (as decodable members).
            match webhooks::endpoints_for_subject(state, WebhookEventType::Paid, deposit_id).await {
                Ok(endpoints) => {
                    let endpoints = endpoints.unwrap_or_default();
                    if endpoints.is_empty() {
                        warn!("Dropping legacy webhook queue member {deposit_id} — no endpoint to notify");
                    }
                    for endpoint in endpoints {
                        if let Err(err) =
                            enqueue_webhook(state, WebhookEventType::Paid, deposit_id, &endpoint.id).await
                        {
                            error!("legacy member fan-out error for {deposit_id}: {err:#}");
                        }
                    }
                    None
                }
                Err(err) => {
                    warn!("legacy member fan-out read error for {deposit_id}, rescheduling: {err:#}");
                    Some(retry_after(WEBHOOK_RETRY_DELAYS_SECS[0]))
                }
            }
        }
        QueueMember::Delivery {
            event,
            subject_id,
            endpoint_id,
        } => match deliver_webhook(state, event, subject_id, endpoint_id).await {
            Ok(DeliveryOutcome::RetryAt { attempts }) => {
                let delay = next_retry_delay_secs(attempts)?;
                info!(
                    "Webhook attempt {} for {} {subject_id} (endpoint {endpoint_id}) rescheduled in {delay}s",
                    attempts + 1,
                    event.as_api()
                );
                Some(retry_after(delay))
            }
            Ok(_) => None,
            Err(err) => {
                // Infrastructure error (DB unreachable…): retry after the
                // first backoff gap rather than spinning.
                warn!("webhook delivery error for {member}, rescheduling: {err:#}");
                Some(retry_after(WEBHOOK_RETRY_DELAYS_SECS[0]))
            }
        },
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

/// Raw totals per token. Each entry carries one token, so sats never leak
/// into the USDT figure, nor wei into anything else.
#[derive(Debug, Default)]
pub struct TokenTotals {
    pub usdt_raw: u128,
    pub btc_raw: u128,
    pub eth_raw: u128,
    pub trx_raw: u128,
    pub sol_raw: u128,
}

impl TokenTotals {
    pub fn total_usdt(&self) -> String {
        format_raw(self.usdt_raw, Token::Usdt)
    }

    pub fn total_btc(&self) -> String {
        format_raw(self.btc_raw, Token::Btc)
    }

    pub fn total_eth(&self) -> String {
        format_raw(self.eth_raw, Token::Eth)
    }

    pub fn total_trx(&self) -> String {
        format_raw(self.trx_raw, Token::Trx)
    }

    pub fn total_sol(&self) -> String {
        format_raw(self.sol_raw, Token::Sol)
    }
}

/// Sum `(token, amount_raw)` entries as reported in balance payloads;
/// unknown tokens and unparsable amounts count as zero.
pub fn token_totals<'a>(entries: impl IntoIterator<Item = (&'a str, &'a str)>) -> TokenTotals {
    let mut totals = TokenTotals::default();
    for (token, amount_raw) in entries {
        let raw = amount_raw.parse::<u128>().unwrap_or(0);
        // A hostile provider must not overflow the totals.
        let total = match Token::parse_api(token) {
            Ok(Token::Usdt) => &mut totals.usdt_raw,
            Ok(Token::Btc) => &mut totals.btc_raw,
            Ok(Token::Eth) => &mut totals.eth_raw,
            Ok(Token::Trx) => &mut totals.trx_raw,
            Ok(Token::Sol) => &mut totals.sol_raw,
            Err(_) => continue,
        };
        *total = total.saturating_add(raw);
    }
    totals
}

/// The (network, token) entries of the balance views: every payment token,
/// plus the gas assets held only by the treasuries (TRX, SOL).
fn balance_entries() -> Vec<(Network, Token)> {
    let mut entries = Vec::new();
    for network in [Network::Tron, Network::Ethereum, Network::Solana, Network::Bitcoin, Network::Lightning] {
        for token in network.withdrawable_tokens() {
            entries.push((network, *token));
        }
    }
    entries
}

/// Deposit addresses hold payment tokens; gas assets sit on treasuries.
fn held_by_deposits(network: Network, token: Token) -> bool {
    domain::supports(network, token) && network != Network::Lightning
}

fn format_raw(raw: u128, token: Token) -> String {
    raw_to_decimal_string(&raw.to_string(), token).unwrap_or_else(|_| domain::raw_u128_to_string(raw, token))
}

/// The treasury's latest balance of (network, token) — `None` on error
/// (logged), 0 when unconfigured.
async fn treasury_balance_now(state: &AppState, network: Network, token: Token) -> Option<u128> {
    let result = match network {
        Network::Lightning => match &state.lightning {
            Some(lightning) => lightning.balance_sats(false).await.map(u128::from),
            None => Ok(0),
        },
        _ => match treasury::treasury_token_holder(state, network, token) {
            Ok(holder) => state.chain.fetch_balance(network, token, &holder).await,
            Err(err) => Err(err),
        },
    };
    match result {
        Ok(balance) => Some(balance),
        Err(err) => {
            warn!("Treasury balance of {} {} unreadable: {err:#}", network.as_api(), token.as_api());
            None
        }
    }
}

pub async fn sync_wallet_balances(state: &AppState) -> Result<()> {
    if !state.chain.has_any_network_available() && state.lightning.is_none() {
        debug!("Skipping wallet balance sync — no blockchain API keys configured");
        return Ok(());
    }

    info!("Starting wallet HD balance sync");
    let mut networks = Vec::new();
    for (network, token) in balance_entries() {
        networks.push(sync_network(state, network, token).await?);
    }

    let totals = token_totals(networks.iter().map(|n| (n.token.as_str(), n.amount_raw.as_str())));
    let payload = CachedWalletBalances {
        total_usdt: totals.total_usdt(),
        total_usdt_raw: totals.usdt_raw.to_string(),
        total_btc: totals.total_btc(),
        total_btc_raw: totals.btc_raw.to_string(),
        total_eth: totals.total_eth(),
        total_trx: totals.total_trx(),
        total_sol: totals.total_sol(),
        networks,
        updated_at: chrono::Utc::now().to_rfc3339(),
    };

    let key = state.config.redis_key(WALLET_BALANCES_KEY);
    let mut redis = state.redis.clone();
    let _: () = redis.set(key, serde_json::to_string(&payload)?).await?;
    info!(
        "Wallet HD balance sync completed — total USDT: {}, total BTC: {}, total ETH: {}",
        payload.total_usdt, payload.total_btc, payload.total_eth
    );
    Ok(())
}

/// Every derived index of `network` (whether issued for this token or not)
/// plus the treasury.
async fn sync_network(state: &AppState, network: Network, token: Token) -> Result<NetworkWalletBalance> {
    if !state.is_network_enabled(network) {
        return Ok(NetworkWalletBalance {
            message: Some(state.network_disabled_message(network).to_string()),
            ..empty_wallet_network(network, token)
        });
    }

    let mut deposits_raw: u128 = 0;
    let mut indices_scanned = 0;
    if held_by_deposits(network, token) {
        let max_index = state.db.max_derivation_index(network).await?;
        for index in 0..=max_index {
            if index > 0 {
                tokio::time::sleep(Duration::from_millis(ADDRESS_REQUEST_DELAY_MS)).await;
            }
            let address = state.wallet.derive_address(network, index as u32)?;
            match state.chain.fetch_balance(network, token, &address).await {
                Ok(bal) => deposits_raw = deposits_raw.saturating_add(bal),
                Err(err) => warn!(
                    "Failed to fetch wallet {} balance for {} index {index}: {err:#}",
                    token.as_api(),
                    network.as_api()
                ),
            }
        }
        indices_scanned = max_index + 1;
    }
    let treasury_raw = treasury_balance_now(state, network, token).await;
    let total = deposits_raw.saturating_add(treasury_raw.unwrap_or(0));
    Ok(NetworkWalletBalance {
        network: network.as_api().into(),
        token: token.as_api().into(),
        amount: format_raw(total, token),
        amount_raw: total.to_string(),
        treasury_amount: treasury_raw.map(|raw| format_raw(raw, token)),
        treasury_amount_raw: treasury_raw.map(|raw| raw.to_string()),
        deposits_amount: format_raw(deposits_raw, token),
        deposits_amount_raw: deposits_raw.to_string(),
        indices_scanned,
        available: true,
        message: treasury_raw.is_none().then(|| "Treasury balance unavailable".to_string()),
    })
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
            total_eth: cached.total_eth,
            total_trx: cached.total_trx,
            total_sol: cached.total_sol,
            networks: cached.networks,
            updated_at: Some(cached.updated_at),
        });
    }
    Ok(WalletBalancesResponse {
        total_usdt: "0".into(),
        total_btc: "0".into(),
        total_eth: "0".into(),
        total_trx: "0".into(),
        total_sol: "0".into(),
        networks: balance_entries()
            .into_iter()
            .map(|(network, token)| empty_wallet_network(network, token))
            .collect(),
        updated_at: None,
    })
}

/// Live balances of every issued address and of the treasuries. Querying
/// all of them costs one provider call per address, so the result is
/// cached for `LIVE_BALANCES_CACHE_SECS` and recomputed in the background
/// by a single caller (Redis lock). `None` = being computed, ask again
/// shortly.
pub async fn get_live_balances(state: &AppState) -> Result<Option<BalancesResponse>> {
    let cache_key = state.config.redis_key(LIVE_BALANCES_KEY);
    let lock_key = state.config.redis_key(LIVE_BALANCES_LOCK_KEY);
    let mut redis = state.redis.clone();
    let cached: Option<String> = redis.get(&cache_key).await?;
    if let Some(cached) = cached {
        // A payload cached by the previous release (one entry per network)
        // is recomputed rather than served half-filled.
        if let Ok(balances) = serde_json::from_str::<BalancesResponse>(&cached) {
            return Ok(Some(balances));
        }
    }

    // The lock outlives any sane computation; it is released when done.
    let acquired: Option<String> = redis::cmd("SET")
        .arg(&lock_key)
        .arg(1)
        .arg("NX")
        .arg("EX")
        .arg(LIVE_BALANCES_CACHE_SECS * 4)
        .query_async(&mut redis)
        .await?;
    if acquired.is_some() {
        let state = state.clone();
        tokio::spawn(async move {
            let mut redis = state.redis.clone();
            match compute_live_balances(&state).await {
                Ok(balances) => match serde_json::to_string(&balances) {
                    Ok(json) => {
                        if let Err(err) = redis
                            .set_ex::<_, _, ()>(&cache_key, json, LIVE_BALANCES_CACHE_SECS)
                            .await
                        {
                            error!("live balances cache write failed: {err}");
                        }
                    }
                    Err(err) => error!("live balances serialization failed: {err}"),
                },
                Err(err) => error!("live balances computation failed: {err:#}"),
            }
            let _: Result<(), _> = redis.del(&lock_key).await;
        });
    }
    Ok(None)
}

async fn compute_live_balances(state: &AppState) -> Result<BalancesResponse> {
    let mut networks = Vec::new();
    for (network, token) in balance_entries() {
        networks.push(live_network_balance(state, network, token).await?);
    }

    let totals = token_totals(networks.iter().map(|n| (n.token.as_str(), n.amount_raw.as_str())));
    Ok(BalancesResponse {
        total_usdt: totals.total_usdt(),
        total_btc: totals.total_btc(),
        total_eth: totals.total_eth(),
        total_trx: totals.total_trx(),
        total_sol: totals.total_sol(),
        networks,
        updated_at: chrono::Utc::now().to_rfc3339(),
    })
}

/// Treasury + unswept deposits of (network, token).
async fn live_network_balance(state: &AppState, network: Network, token: Token) -> Result<NetworkBalance> {
    if !state.is_network_enabled(network) {
        return Ok(NetworkBalance {
            message: Some(state.network_disabled_message(network).to_string()),
            ..empty_network_balance(network, token)
        });
    }

    let mut deposits_raw: u128 = 0;
    let mut checked = 0;
    if held_by_deposits(network, token) {
        let addresses = state.db.addresses_for_network(network).await?;
        for (i, address) in addresses.iter().enumerate() {
            if i > 0 {
                tokio::time::sleep(Duration::from_millis(ADDRESS_REQUEST_DELAY_MS)).await;
            }
            match state.chain.fetch_balance(network, token, address).await {
                Ok(bal) => deposits_raw = deposits_raw.saturating_add(bal),
                Err(err) => warn!(
                    "Failed to fetch {} {} balance for {address}: {err:#}",
                    network.as_api(),
                    token.as_api()
                ),
            }
        }
        checked = addresses.len() as i32;
    }
    let treasury_raw = treasury_balance_now(state, network, token).await;
    let total = deposits_raw.saturating_add(treasury_raw.unwrap_or(0));
    Ok(NetworkBalance {
        network: network.as_api().into(),
        token: token.as_api().into(),
        amount: format_raw(total, token),
        amount_raw: total.to_string(),
        treasury_amount: treasury_raw.map(|raw| format_raw(raw, token)),
        treasury_amount_raw: treasury_raw.map(|raw| raw.to_string()),
        deposits_amount: format_raw(deposits_raw, token),
        deposits_amount_raw: deposits_raw.to_string(),
        addresses_checked: checked,
        available: true,
        message: treasury_raw.is_none().then(|| "Treasury balance unavailable".to_string()),
    })
}

fn empty_wallet_network(network: Network, token: Token) -> NetworkWalletBalance {
    NetworkWalletBalance {
        network: network.as_api().into(),
        token: token.as_api().into(),
        amount: "0".into(),
        amount_raw: "0".into(),
        treasury_amount: None,
        treasury_amount_raw: None,
        deposits_amount: "0".into(),
        deposits_amount_raw: "0".into(),
        indices_scanned: 0,
        available: false,
        message: None,
    }
}

fn empty_network_balance(network: Network, token: Token) -> NetworkBalance {
    NetworkBalance {
        network: network.as_api().into(),
        token: token.as_api().into(),
        amount: "0".into(),
        amount_raw: "0".into(),
        treasury_amount: None,
        treasury_amount_raw: None,
        deposits_amount: "0".into(),
        deposits_amount_raw: "0".into(),
        addresses_checked: 0,
        available: false,
        message: None,
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CachedWalletBalances {
    pub total_usdt: String,
    pub total_usdt_raw: String,
    /// Defaults keep payloads cached before BTC/ETH support readable.
    #[serde(default = "zero_string")]
    pub total_btc: String,
    #[serde(default = "zero_string")]
    pub total_btc_raw: String,
    #[serde(default = "zero_string")]
    pub total_eth: String,
    #[serde(default = "zero_string")]
    pub total_trx: String,
    #[serde(default = "zero_string")]
    pub total_sol: String,
    pub networks: Vec<NetworkWalletBalance>,
    pub updated_at: String,
}

fn zero_string() -> String {
    "0".into()
}

/// One (network, token): treasury + deposit addresses.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkWalletBalance {
    pub network: String,
    pub token: String,
    pub amount: String,
    pub amount_raw: String,
    /// `None` when unreadable (or cached before treasuries existed).
    #[serde(default)]
    pub treasury_amount: Option<String>,
    #[serde(default)]
    pub treasury_amount_raw: Option<String>,
    #[serde(default = "zero_string")]
    pub deposits_amount: String,
    #[serde(default = "zero_string")]
    pub deposits_amount_raw: String,
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
    pub total_eth: String,
    /// Gas reserves (treasuries).
    pub total_trx: String,
    pub total_sol: String,
    pub networks: Vec<NetworkWalletBalance>,
    pub updated_at: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct BalancesResponse {
    pub total_usdt: String,
    /// Sum of the bitcoin + lightning entries; never folded into USDT.
    pub total_btc: String,
    pub total_eth: String,
    /// Gas reserves (treasuries).
    pub total_trx: String,
    pub total_sol: String,
    pub networks: Vec<NetworkBalance>,
    /// When this (cached) figure was computed.
    pub updated_at: String,
}

/// One (network, token): treasury + unswept deposit addresses.
#[derive(Debug, Serialize, Deserialize)]
pub struct NetworkBalance {
    pub network: String,
    pub token: String,
    pub amount: String,
    pub amount_raw: String,
    pub treasury_amount: Option<String>,
    pub treasury_amount_raw: Option<String>,
    pub deposits_amount: String,
    pub deposits_amount_raw: String,
    pub addresses_checked: i32,
    pub available: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use lightning_client::{PaymentDirection, ReceivedPayment, WalletPaymentStatus};

    fn invoice_row(network: Network, status: DepositAddressStatus) -> DepositAddressRow {
        let now = chrono::Utc::now();
        DepositAddressRow {
            id: "addr-1".into(),
            network: network.as_db().into(),
            token: network.payment_tokens()[0].as_db().into(),
            address: "lnbc1500u1invoice".into(),
            derivation_index: 0,
            expected_amount: rust_decimal::Decimal::new(150_000, 8),
            status: status.as_db().into(),
            expires_at: now,
            metadata: serde_json::json!({}),
            api_key_id: None,
            project_id: Some("p1".into()),
            btc_received_raw: None,
            watch_until: now,
            token_account: None,
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
    fn unexpected_payment_id_shapes_are_still_credited() {
        // SDK ids are Spark transfer UUIDs and never reach the webhook queue
        // (members key on deposits.id): an odd shape is logged, never dropped
        // — dropping would leave a settled payment uncredited.
        let row = invoice_row(Network::Lightning, DepositAddressStatus::Pending);
        let transfer = lightning_transfer(&settled("pay|abc"), Some(&row)).expect("credited");
        assert_eq!(transfer.tx_hash, "pay|abc");
    }

    fn other_invoice(id: &str, address: &str) -> DepositAddressRow {
        DepositAddressRow {
            id: id.into(),
            address: address.into(),
            ..invoice_row(Network::Lightning, DepositAddressStatus::Pending)
        }
    }

    #[test]
    fn reconciliation_matches_only_pending_invoices() {
        let pending = vec![
            invoice_row(Network::Lightning, DepositAddressStatus::Pending),
            other_invoice("addr-2", "lnbc2unpaid"),
        ];
        let payments = vec![
            settled("pay-abc"),
            ReceivedPayment {
                invoice: "lnbc9unrelated".into(),
                payment_id: "pay-unrelated".into(),
                amount_sats: 1,
            },
        ];
        let transfers = reconciled_transfers(&payments, &pending);
        assert_eq!(transfers.len(), 1);
        assert_eq!(transfers[0].deposit_address_id, "addr-1");
        assert_eq!(transfers[0].tx_hash, "pay-abc");
    }

    #[test]
    fn reconciliation_and_event_share_the_upsert_key() {
        // The event push and the reconciliation poll can both see a payment,
        // and the listing can repeat it: every sighting must produce the same
        // (address, tx) key, which the per-address tx-hash upsert in
        // `process_detected_transfer` turns into a no-op after the first.
        let row = invoice_row(Network::Lightning, DepositAddressStatus::Pending);
        let from_event = lightning_transfer(&settled("pay-abc"), Some(&row)).unwrap();
        let from_poll =
            reconciled_transfers(&[settled("pay-abc"), settled("pay-abc")], std::slice::from_ref(&row));
        assert_eq!(from_poll.len(), 2);
        for transfer in from_poll {
            assert_eq!(
                (&transfer.deposit_address_id, &transfer.tx_hash, &transfer.amount_raw),
                (&from_event.deposit_address_id, &from_event.tx_hash, &from_event.amount_raw)
            );
        }
    }

    fn history_payment(id: &str, invoice: Option<&str>) -> WalletPayment {
        WalletPayment {
            id: id.into(),
            direction: PaymentDirection::Receive,
            status: WalletPaymentStatus::Completed,
            amount_sats: 1_000,
            fees_sats: 0,
            timestamp: 1_700_000_000,
            method: "lightning".into(),
            invoice: invoice.map(Into::into),
            description: None,
            tx_id: None,
            ln_address: None,
            comment: None,
        }
    }

    #[test]
    fn history_links_gateway_invoices_to_their_payment() {
        let invoices: HashMap<String, String> =
            [("lnbc1gateway".to_string(), "addr-1".to_string())].into();
        let entries = link_history(
            vec![
                history_payment("p1", Some("lnbc1gateway")),
                history_payment("p2", Some("lnbc1elsewhere")),
                history_payment("p3", None),
            ],
            &invoices,
        );
        let linked: Vec<(&str, Option<&str>)> = entries
            .iter()
            .map(|e| (e.payment.id.as_str(), e.deposit_address_id.as_deref()))
            .collect();
        // Order (newest first) is preserved; only gateway invoices link.
        assert_eq!(linked, vec![("p1", Some("addr-1")), ("p2", None), ("p3", None)]);
    }

    fn sent(status: WalletPaymentStatus) -> WalletPayment {
        WalletPayment {
            direction: PaymentDirection::Send,
            status,
            amount_sats: 150_000,
            fees_sats: 3,
            ln_address: Some("alice@example.com".into()),
            comment: Some("thanks".into()),
            invoice: Some("lnbc1500u1recipient".into()),
            ..history_payment("6f1c7a8e-2f43-4d55-9a8b-1c2d3e4f5a6b", None)
        }
    }

    #[test]
    fn payout_events_follow_the_final_send_status() {
        assert_eq!(
            payout_event_for(&sent(WalletPaymentStatus::Completed)),
            Some(WebhookEventType::PayoutSucceeded)
        );
        assert_eq!(
            payout_event_for(&sent(WalletPaymentStatus::Failed)),
            Some(WebhookEventType::PayoutFailed)
        );
        assert_eq!(payout_event_for(&sent(WalletPaymentStatus::Pending)), None);
        // A received payment is never a payout.
        let received = WalletPayment {
            direction: PaymentDirection::Receive,
            ..sent(WalletPaymentStatus::Completed)
        };
        assert_eq!(payout_event_for(&received), None);
    }

    #[test]
    fn payout_catch_up_starts_before_the_watermark_and_never_on_first_boot() {
        // First boot: nothing to catch up (no history replay).
        assert_eq!(payout_catch_up_from(None), None);
        assert_eq!(
            payout_catch_up_from(Some(1_700_003_600)),
            Some(1_700_003_600 - config::LIGHTNING_PAYOUT_CATCH_UP_MARGIN_SECS)
        );
        assert_eq!(payout_catch_up_from(Some(10)), Some(0));
    }

    #[test]
    fn payout_queue_members_roundtrip() {
        for event in [WebhookEventType::PayoutSucceeded, WebhookEventType::PayoutFailed] {
            let member =
                encode_queue_member(event, "6f1c7a8e-2f43-4d55-9a8b-1c2d3e4f5a6b", "ep-456");
            assert_eq!(
                member,
                format!("{}|6f1c7a8e-2f43-4d55-9a8b-1c2d3e4f5a6b|ep-456", event.as_db())
            );
            assert_eq!(
                decode_queue_member(&member),
                Some((event, "6f1c7a8e-2f43-4d55-9a8b-1c2d3e4f5a6b", "ep-456"))
            );
        }
    }

    #[test]
    fn payout_payload_reports_the_send() {
        let payload =
            payout_payload(WebhookEventType::PayoutSucceeded, &sent(WalletPaymentStatus::Completed));
        assert_eq!(
            payload,
            serde_json::json!({
                "event_type": "payout_succeeded",
                "data": {
                    "payment_id": "6f1c7a8e-2f43-4d55-9a8b-1c2d3e4f5a6b",
                    "status": "completed",
                    "network": "lightning",
                    "token": "BTC",
                    "amount": "0.0015",
                    "amount_sats": "150000",
                    "fees_sats": "3",
                    "method": "lightning",
                    "ln_address": "alice@example.com",
                    "comment": "thanks",
                    "invoice": "lnbc1500u1recipient",
                    "created_at": "2023-11-14T22:13:20+00:00",
                },
            })
        );
    }

    #[test]
    fn totals_keep_every_asset_apart() {
        let totals = token_totals([
            ("ETH", "50000000000000000"),
            ("ETH", "1"),
            ("TRX", "1500000"),
            ("SOL", "100000000"),
            ("USDT", "1"),
        ]);
        assert_eq!(totals.total_eth(), "0.050000000000000001");
        assert_eq!(totals.total_trx(), "1.5");
        assert_eq!(totals.total_sol(), "0.1");
        assert_eq!(totals.total_usdt(), "0.000001");
        assert_eq!(totals.btc_raw, 0);
    }

    #[test]
    fn balance_entries_cover_every_withdrawable_asset() {
        let entries = balance_entries();
        assert!(entries.contains(&(Network::Ethereum, Token::Usdt)));
        assert!(entries.contains(&(Network::Ethereum, Token::Eth)));
        assert!(entries.contains(&(Network::Tron, Token::Trx)));
        assert!(entries.contains(&(Network::Solana, Token::Sol)));
        assert!(entries.contains(&(Network::Lightning, Token::Btc)));
        // Gas assets sit on treasuries only.
        assert!(!held_by_deposits(Network::Tron, Token::Trx));
        assert!(held_by_deposits(Network::Ethereum, Token::Eth));
        assert!(!held_by_deposits(Network::Lightning, Token::Btc));
    }

    #[test]
    fn totals_never_mix_sats_into_usdt() {
        let totals = token_totals([
            ("USDT", "25000000"),
            ("BTC", "150000"),
            ("USDT", "1"),
            ("BTC", "50000"),
            ("USDT", "not-a-number"),
            ("DOGE", "999"),
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

    fn deposit_row(late: bool, status: DepositStatus) -> db::DepositRow {
        let now = chrono::Utc::now();
        db::DepositRow {
            id: "dep-1".into(),
            deposit_address_id: "addr-1".into(),
            tx_hash: "tx-1".into(),
            amount: rust_decimal::Decimal::new(357, 2),
            amount_raw: "3570000".into(),
            confirmations: 19,
            status: status.as_db().into(),
            late,
            webhook_event: None,
            detected_at: now,
            confirmed_at: Some(now),
        }
    }

    fn usdt_address(status: DepositAddressStatus) -> DepositAddressRow {
        DepositAddressRow {
            network: Network::Tron.as_db().into(),
            token: Token::Usdt.as_db().into(),
            address: "TAddr".into(),
            expected_amount: rust_decimal::Decimal::new(857, 2),
            ..invoice_row(Network::Tron, status)
        }
    }

    #[test]
    fn deposit_payloads_carry_the_amounts_and_the_late_address_status() {
        let address = usdt_address(DepositAddressStatus::Paid);
        let paid = deposit_event_payload(
            WebhookEventType::Paid,
            &address,
            &deposit_row(false, DepositStatus::Confirmed),
            8_570_000,
        )
        .unwrap();
        assert_eq!(
            paid,
            serde_json::json!({
                "event_type": "deposit_confirmed",
                "data": {
                    "deposit_id": "addr-1",
                    "project_id": "p1",
                    "address": "TAddr",
                    "network": "tron",
                    "token": "USDT",
                    "amount": "3.570000",
                    "amount_raw": "3570000",
                    "tx_hash": "tx-1",
                    "confirmations": 19,
                    "status": "confirmed",
                    "expected_amount_raw": "8570000",
                    "received_amount_raw": "8570000",
                },
            })
        );

        for (status, api) in [(DepositAddressStatus::Paid, "paid"), (DepositAddressStatus::Expired, "expired")] {
            let late = deposit_event_payload(
                WebhookEventType::Late,
                &usdt_address(status),
                &deposit_row(true, DepositStatus::Confirmed),
                12_140_000,
            )
            .unwrap();
            assert_eq!(late["event_type"], "deposit_late");
            assert_eq!(late["data"]["address_status"], api);
            assert_eq!(late["data"]["received_amount_raw"], "12140000");
            assert_eq!(late["data"]["deposit_id"], "addr-1");
            assert_eq!(late["data"]["status"], "confirmed");
        }
        assert!(paid["data"].get("address_status").is_none());
    }

    #[test]
    fn the_expired_payload_reports_what_arrived() {
        let address = usdt_address(DepositAddressStatus::Expired);
        let expired = address_event_payload(WebhookEventType::Expired, &address, Some(3_000_000)).unwrap();
        assert_eq!(expired["event_type"], "payment_expired");
        assert_eq!(expired["data"]["received_amount_raw"], "3000000");
        assert_eq!(expired["data"]["expected_amount"], "8.57");
        let pending = address_event_payload(WebhookEventType::Pending, &address, None).unwrap();
        assert!(pending["data"].get("received_amount_raw").is_none());
    }

    #[test]
    fn a_replay_sends_the_event_the_confirmation_fired_and_nothing_else() {
        let with = |event: Option<&str>| db::DepositRow {
            webhook_event: event.map(str::to_string),
            ..deposit_row(false, DepositStatus::Confirmed)
        };
        // The deposit that paid its address: `paid`.
        assert_eq!(deposit_replay_event(&with(Some("PAID"))), Some(WebhookEventType::Paid));
        // Confirmed once the address had left PENDING (late, or detected
        // before another transfer paid it): `late`.
        assert_eq!(deposit_replay_event(&with(Some("LATE"))), Some(WebhookEventType::Late));
        // Another deposit of a paid address (a partial transfer): nothing —
        // never a second `paid` with a new event id.
        assert_eq!(deposit_replay_event(&with(None)), None);
        assert_eq!(deposit_replay_event(&with(Some("EXPIRED"))), None);
        assert_eq!(deposit_replay_event(&with(Some("BOGUS"))), None);
    }

    #[test]
    fn late_watch_addresses_are_polled_at_their_own_cadence() {
        let every = Duration::from_secs(300);
        let start = std::time::Instant::now();
        // The first pass polls everything.
        assert!(late_pass_due(None, start, every));
        // Then open payments only, until 5 minutes have passed.
        assert!(!late_pass_due(Some(start), start + Duration::from_secs(10), every));
        assert!(!late_pass_due(Some(start), start + Duration::from_secs(299), every));
        assert!(late_pass_due(Some(start), start + Duration::from_secs(300), every));
        // A clock that does not move forward never panics.
        assert!(!late_pass_due(Some(start + Duration::from_secs(5)), start, every));
    }

    #[test]
    fn late_queue_members_carry_their_event_and_deposit() {
        let member = encode_queue_member(WebhookEventType::Late, "dep-123", "ep-456");
        assert_eq!(member, "LATE|dep-123|ep-456");
        assert_eq!(decode_queue_member(&member), Some((WebhookEventType::Late, "dep-123", "ep-456")));
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
    fn queue_members_are_classified_without_looping_on_garbage() {
        assert_eq!(
            classify_queue_member("dep-1|ep-1"),
            QueueMember::Delivery {
                event: WebhookEventType::Paid,
                subject_id: "dep-1",
                endpoint_id: "ep-1"
            }
        );
        assert_eq!(classify_queue_member("a1b2c3-uuid"), QueueMember::Legacy("a1b2c3-uuid"));
        // Anything with a separator that does not decode is dropped, never
        // fanned out again (that re-enqueued an undecodable member forever).
        for member in ["", "|", "dep|", "BOGUS|addr|ep", "a|b|c|d", "with space"] {
            assert_eq!(classify_queue_member(member), QueueMember::Malformed, "{member:?}");
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

    fn pending_row(event: &str, attempts: i32, updated_at: chrono::DateTime<chrono::Utc>) -> db::PendingDeliveryRow {
        db::PendingDeliveryRow {
            id: "row-1".into(),
            event_type: event.into(),
            deposit_id: Some("dep-1".into()),
            deposit_address_id: Some("addr-1".into()),
            lightning_payment_id: None,
            withdrawal_id: None,
            alert_id: None,
            transfer_id: None,
            webhook_endpoint_id: "ep-1".into(),
            attempts,
            updated_at,
        }
    }

    #[test]
    fn a_delivery_is_due_at_once_then_after_its_retry_gap() {
        let at = chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap();
        assert_eq!(delivery_due_at(0, at), 1_700_000_000);
        assert_eq!(delivery_due_at(1, at), 1_700_000_090);
        assert_eq!(delivery_due_at(4, at), 1_700_002_430);
        // Out of budget (a FAILED row would not be read): due at once.
        assert_eq!(delivery_due_at(5, at), 1_700_000_000);
    }

    #[test]
    fn a_pending_delivery_missing_from_the_queue_is_queued_again_once_due() {
        let at = chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap();
        let now = at.timestamp();
        // A `paid` row written with the status change, never queued.
        assert_eq!(
            reconciled_member(&pending_row("PAID", 0, at), now),
            Some(("dep-1|ep-1".to_string(), now))
        );
        assert_eq!(
            reconciled_member(&pending_row("LATE", 0, at), now),
            Some(("LATE|dep-1|ep-1".to_string(), now))
        );
        assert_eq!(
            reconciled_member(&pending_row("EXPIRED", 0, at), now),
            Some(("EXPIRED|addr-1|ep-1".to_string(), now))
        );
        // A failed attempt: not before its retry is due.
        assert_eq!(reconciled_member(&pending_row("PAID", 1, at), now + 89), None);
        assert_eq!(
            reconciled_member(&pending_row("PAID", 1, at), now + 90),
            Some(("dep-1|ep-1".to_string(), now + 90))
        );
        // Unknown event, missing subject: left alone.
        assert_eq!(reconciled_member(&pending_row("BOGUS", 0, at), now), None);
        let mut withdrawal = pending_row("WITHDRAWAL_CONFIRMED", 0, at);
        assert_eq!(reconciled_member(&withdrawal, now), None);
        withdrawal.withdrawal_id = Some("w-1".into());
        assert_eq!(
            reconciled_member(&withdrawal, now),
            Some(("WITHDRAWAL_CONFIRMED|w-1|ep-1".to_string(), now))
        );
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
