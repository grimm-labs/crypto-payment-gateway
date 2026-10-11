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
    WEBHOOK_DELIVERY_CONCURRENCY, WEBHOOK_MAX_ATTEMPTS, WEBHOOK_RETRY_DELAYS_SECS,
};
use db::Db;
use db::DepositAddressRow;
use domain::{
    format_amount, raw_to_decimal, raw_to_decimal_string, required_confirmations,
    DetectedTransfer, DepositAddressStatus, DepositStatus, Network, Token, WebhookDeliveryStatus,
    WebhookEventType, is_amount_within_tolerance,
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

pub mod audit;
pub mod payouts;
pub mod traffic;
pub mod webhook_client;
pub use audit::Actor;
pub use payouts::{quote_payout, request_payout, PayoutCommand, PayoutError, PayoutOutcome};
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
    /// First-admin bootstrap token, set at boot while no staff exists.
    pub setup_token: Option<Arc<String>>,
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
    // Zero-value transfers ("address poisoning") never settle anything.
    if amount.is_zero() {
        debug!("Ignoring zero-amount transfer {} to address {}", transfer.tx_hash, address.id);
        return Ok(());
    }
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
    let mut queued = 0;
    for endpoint in state.db.active_webhook_endpoints_for_event(event).await? {
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

/// Expire `network`'s addresses whose deadline (plus the grace period) has
/// passed — never one with a DETECTED deposit still confirming — and queue
/// the EXPIRED webhook for each newly expired one. Called by the network's
/// monitor loop right *after* a polling pass, so every address gets a last
/// poll before it expires. Returns the number of addresses expired.
/// `polled`: see `Db::expire_stale_addresses`.
pub async fn expire_addresses_and_notify(
    state: &AppState,
    network: Network,
    polled: Option<&[String]>,
) -> Result<u64> {
    let expired_ids = state
        .db
        .expire_stale_addresses(
            network,
            ADDRESS_EXPIRY_GRACE_SECS,
            DETECTED_DEPOSIT_HOLD_SECS,
            polled,
        )
        .await?;
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

/// Merchant replay of a confirmed deposit's webhooks: only the deliveries
/// that ended FAILED, and only towards endpoints still active and
/// subscribed to PAID — a delivered webhook is never sent again from the
/// merchant API. Returns the number of deliveries re-queued.
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
    let eligible: Vec<String> = state
        .db
        .active_webhook_endpoints_for_event(WebhookEventType::Paid)
        .await?
        .into_iter()
        .map(|endpoint| endpoint.id)
        .collect();
    let mut retried = 0;
    for delivery in state.db.failed_paid_deliveries(&deposit.id).await? {
        let Some(endpoint_id) = delivery.webhook_endpoint_id.as_deref() else {
            continue;
        };
        if !eligible.iter().any(|id| id == endpoint_id) {
            continue;
        }
        state.db.reset_webhook_delivery_by_id(&delivery.id).await?;
        enqueue_webhook(state, WebhookEventType::Paid, &deposit.id, endpoint_id).await?;
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
        event if event.is_payout() => delivery
            .lightning_payment_id
            .clone()
            .ok_or(RetriggerError::DeliveryNotFound)?,
        _ => delivery
            .deposit_address_id
            .clone()
            .ok_or(RetriggerError::DeliveryNotFound)?,
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
            event if event.is_payout() => match target.lightning_payment_id.as_deref() {
                Some(id) => id,
                None => continue,
            },
            _ => match target.deposit_address_id.as_deref() {
                Some(id) => id,
                None => continue,
            },
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
                    event if event.is_payout() => {
                        let row = state
                            .db
                            .get_or_create_payout_webhook_delivery(subject_id, event, endpoint_id)
                            .await?;
                        state.db.reset_webhook_delivery_by_id(&row.id).await?;
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

/// Deliver one queued webhook. `subject_id` is a deposit id for PAID, a
/// deposit-address id for PENDING/EXPIRED and a Lightning payment id for
/// payout events.
pub async fn deliver_webhook(
    state: &AppState,
    event: WebhookEventType,
    subject_id: &str,
    endpoint_id: &str,
) -> Result<DeliveryOutcome> {
    // Reload the endpoint on every attempt so a deactivation or an event
    // unsubscription takes effect immediately: the queued member is consumed
    // without being rescheduled, and its PENDING row is closed.
    let endpoint = match state.db.find_webhook_endpoint(endpoint_id).await? {
        Some(endpoint) if endpoint.is_active && endpoint.accepts_event(event) => endpoint,
        _ => {
            warn!(
                "Webhook delivery skipped for {} {subject_id} — endpoint {endpoint_id} is inactive, gone or unsubscribed",
                event.as_api()
            );
            close_undeliverable(
                state,
                event,
                subject_id,
                endpoint_id,
                "Endpoint inactive, unsubscribed or deleted",
            )
            .await?;
            return Ok(DeliveryOutcome::Skipped);
        }
    };
    let subject_label = match event {
        WebhookEventType::Paid => format!("deposit {subject_id}"),
        event if event.is_payout() => format!("payout {subject_id} ({})", event.as_api()),
        _ => format!("address {subject_id} ({})", event.as_api()),
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
        WebhookEventType::Paid => {
            let Some(deposit) = state.db.find_deposit(subject_id).await? else {
                return Ok(None);
            };
            let Some(address) = state.db.find_deposit_address(&deposit.deposit_address_id).await?
            else {
                return Ok(None);
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
                    "token": token.as_api(),
                    "amount": format_amount(deposit.amount, token),
                    "amount_raw": deposit.amount_raw,
                    "tx_hash": deposit.tx_hash,
                    "confirmations": deposit.confirmations,
                    "status": "confirmed",
                },
            });
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
            Ok(Some((delivery, address_event_payload(event, &address)?, occurred_at)))
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
    let row = match event {
        WebhookEventType::Paid => state.db.find_webhook_delivery(subject_id, endpoint_id).await?,
        event if event.is_payout() => {
            state
                .db
                .find_payout_webhook_delivery(subject_id, event, endpoint_id)
                .await?
        }
        event => {
            state
                .db
                .find_address_webhook_delivery(subject_id, event, endpoint_id)
                .await?
        }
    };
    if let Some(row) = row {
        state.db.fail_pending_webhook_delivery(&row.id, reason).await?;
    }
    Ok(())
}

/// Payload of an address-scoped event. Mirrors the PAID payload's naming:
/// `data.deposit_id` is deposit_addresses.id.
fn address_event_payload(
    event: WebhookEventType,
    address: &DepositAddressRow,
) -> Result<serde_json::Value> {
    let network = Network::from_db(&address.network)?;
    let token = Token::from_db(&address.token)?;
    Ok(serde_json::json!({
        "event_type": event.payload_event_type(),
        "data": {
            "deposit_id": address.id,
            "address": address.address,
            "network": network.as_api(),
            "token": token.as_api(),
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
    loop {
        interval.tick().await;
        if let Err(err) = poll_network(&state, network).await {
            error!("{network:?} monitor error: {err:#}");
        }
    }
}

/// Poll `network`'s open payments, then expire the stale ones: the sweep
/// runs only after the poll, so a payment always gets a last look before
/// it expires (no boot-time race either — the first pass of every loop
/// polls before it expires anything).
async fn poll_network(state: &AppState, network: Network) -> Result<()> {
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

    let addresses = state.db.pending_addresses(network).await?;
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
            Network::Ethereum => state.chain.poll_ethereum_address(&address.address).await,
            Network::Solana => state.chain.poll_solana_address(&address.address).await,
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
            // Pre-multi-endpoint member: fan it out to the current active
            // PAID-subscribed endpoints (as decodable members).
            match state
                .db
                .active_webhook_endpoints_for_event(WebhookEventType::Paid)
                .await
            {
                Ok(endpoints) => {
                    if endpoints.is_empty() {
                        warn!("Dropping legacy webhook queue member {deposit_id} — no active endpoints");
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

/// Raw totals per token. Each network carries a single token, so sats can
/// never leak into the USDT figure (and vice versa).
#[derive(Debug, Default)]
pub struct TokenTotals {
    pub usdt_raw: u128,
    pub btc_raw: u128,
}

impl TokenTotals {
    pub fn total_usdt(&self) -> String {
        format_raw(self.usdt_raw, Token::Usdt)
    }

    pub fn total_btc(&self) -> String {
        format_raw(self.btc_raw, Token::Btc)
    }
}

/// Sum `(token, amount_raw)` entries as reported in balance payloads;
/// unknown tokens and unparsable amounts count as zero.
pub fn token_totals<'a>(entries: impl IntoIterator<Item = (&'a str, &'a str)>) -> TokenTotals {
    let mut totals = TokenTotals::default();
    for (token, amount_raw) in entries {
        let raw = amount_raw.parse::<u128>().unwrap_or(0);
        match Token::parse_api(token) {
            // A hostile provider must not overflow the totals.
            Ok(Token::Usdt) => totals.usdt_raw = totals.usdt_raw.saturating_add(raw),
            Ok(Token::Btc) => totals.btc_raw = totals.btc_raw.saturating_add(raw),
            Err(_) => {}
        }
    }
    totals
}

/// Networks served by a chain provider, polled per derived address
/// (Lightning goes through the Breez client instead).
const CHAIN_NETWORKS: [Network; 4] =
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
    for network in CHAIN_NETWORKS {
        networks.push(sync_network(state, network).await?);
    }
    networks.push(lightning_wallet_balance(state).await);

    let totals = token_totals(networks.iter().map(|n| (n.token.as_str(), n.amount_raw.as_str())));
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
            Ok(bal) => total_raw = total_raw.saturating_add(bal),
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
struct LightningBalance {
    amount: String,
    amount_raw: String,
    available: bool,
    message: Option<String>,
}

async fn lightning_balance(state: &AppState, ensure_synced: bool) -> LightningBalance {
    let mut balance = LightningBalance {
        amount: "0".into(),
        amount_raw: "0".into(),
        available: false,
        message: None,
    };
    let Some(lightning) = &state.lightning else {
        balance.message = Some(state.network_disabled_message(Network::Lightning).to_string());
        return balance;
    };
    match lightning.balance_sats(ensure_synced).await {
        Ok(sats) => {
            balance.amount = format_raw(u128::from(sats), Token::Btc);
            balance.amount_raw = sats.to_string();
            balance.available = true;
        }
        Err(err) => {
            warn!("Failed to fetch Lightning wallet balance: {err:#}");
            balance.message = Some("Failed to read the Lightning wallet balance".into());
        }
    }
    balance
}

async fn lightning_wallet_balance(state: &AppState) -> NetworkWalletBalance {
    // The hourly sync can afford a fresh figure.
    let balance = lightning_balance(state, true).await;
    NetworkWalletBalance {
        amount: balance.amount,
        amount_raw: balance.amount_raw,
        available: balance.available,
        message: balance.message,
        ..empty_wallet_network(Network::Lightning)
    }
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

/// Live balances of every issued address. Querying all of them costs one
/// provider call per address, so the result is cached for
/// `LIVE_BALANCES_CACHE_SECS` and recomputed in the background by a single
/// caller (Redis lock). `None` = being computed, ask again shortly.
pub async fn get_live_balances(state: &AppState) -> Result<Option<BalancesResponse>> {
    let cache_key = state.config.redis_key(LIVE_BALANCES_KEY);
    let lock_key = state.config.redis_key(LIVE_BALANCES_LOCK_KEY);
    let mut redis = state.redis.clone();
    let cached: Option<String> = redis.get(&cache_key).await?;
    if let Some(cached) = cached {
        return Ok(Some(serde_json::from_str(&cached)?));
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
    for network in CHAIN_NETWORKS {
        networks.push(live_network_balance(state, network).await?);
    }
    networks.push(live_lightning_balance(state).await);

    let totals = token_totals(networks.iter().map(|n| (n.token.as_str(), n.amount_raw.as_str())));
    Ok(BalancesResponse {
        total_usdt: totals.total_usdt(),
        total_btc: totals.total_btc(),
        networks,
        updated_at: chrono::Utc::now().to_rfc3339(),
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
    for (i, address) in addresses.iter().enumerate() {
        if i > 0 {
            tokio::time::sleep(Duration::from_millis(ADDRESS_REQUEST_DELAY_MS)).await;
        }
        match fetch_address_balance(state, network, address).await {
            Ok(bal) => total_raw = total_raw.saturating_add(bal),
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
    let balance = lightning_balance(state, false).await;
    NetworkBalance {
        network: Network::Lightning.as_api().into(),
        token: Token::Btc.as_api().into(),
        amount: balance.amount,
        amount_raw: balance.amount_raw,
        addresses_checked: 0,
        available: balance.available,
        message: balance.message,
    }
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

#[derive(Debug, Serialize, Deserialize)]
pub struct BalancesResponse {
    pub total_usdt: String,
    /// Sum of the bitcoin + lightning entries; never folded into USDT.
    pub total_btc: String,
    pub networks: Vec<NetworkBalance>,
    /// When this (cached) figure was computed.
    pub updated_at: String,
}

#[derive(Debug, Serialize, Deserialize)]
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
    use lightning_client::{PaymentDirection, ReceivedPayment, WalletPaymentStatus};

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
            api_key_id: None,
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
