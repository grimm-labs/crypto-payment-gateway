//! Lightning withdrawals through the Breez SDK: nothing is signed locally.
//! A withdrawal moves QUEUED → BROADCAST *before* it is handed to the SDK
//! (from then on it can no longer be cancelled or failed by a re-check: a
//! payment may be in flight). The SDK idempotency key is the withdrawal id,
//! so a retry after a timeout or a crash returns the stored send instead of
//! paying twice. Final outcomes come from the wallet events and from a
//! reconciliation pass.

use std::time::Duration;

use anyhow::Result;
use chrono::Utc;
use config::PAYOUT_TIMEOUT_SECS;
use db::{WithdrawalRow, WithdrawalUpdate};
use domain::{DestinationKind, Network, WebhookEventType, WithdrawalEvent, WithdrawalStatus};
use lightning_client::{FeeAboveCap, LightningClient, PaymentDirection, WalletPayment, WalletPaymentStatus};
use serde_json::json;
use tracing::{info, warn};

use super::{fail_claim, transition};
use crate::audit::{self, Actor};
use crate::withdrawals::{notify_withdrawal, recheck_before_signing};
use crate::{freeze, AppState};

/// A BROADCAST withdrawal the wallet has no payment for after this long
/// (the process died between the status change and the SDK call, or the
/// SDK never registered it) is handed to the SDK again — same key.
const RESEND_AFTER_SECS: i64 = 3 * PAYOUT_TIMEOUT_SECS as i64;

/// Send the oldest QUEUED Lightning withdrawal. `false` when there was
/// nothing (or it must wait).
pub async fn execute_next(state: &AppState) -> Result<bool> {
    let Some(lightning) = state.lightning.clone() else {
        return Ok(false);
    };
    let Some(mut claim) = state.db.claim_queued_withdrawal(Network::Lightning, &[]).await? else {
        return Ok(false);
    };
    let row = claim.row().clone();
    if let Err(reason) = recheck_before_signing(state, &row).await? {
        fail_claim(state, claim, reason).await?;
        return Ok(true);
    }
    if freeze::outflows_frozen(state).await {
        return Ok(false);
    }
    let to = domain::next_withdrawal_status(row.status()?, WithdrawalEvent::Broadcast, true)?;
    claim
        .transition(to, &WithdrawalUpdate {
            tx_hash: Some(row.id.clone()),
            broadcast_now: true,
            ..Default::default()
        })
        .await?
        .ok_or_else(|| anyhow::anyhow!("withdrawal {} moved under its claim", row.id))?;
    let row = claim.commit().await?;
    let Some(_guard) = state.db.try_advisory_lock(&format!("lightning-send:{}", row.id)).await? else {
        return Ok(true);
    };
    send(state, &lightning, &row).await?;
    Ok(true)
}

/// Hand a BROADCAST withdrawal to the SDK (again: same idempotency key).
async fn send(state: &AppState, lightning: &LightningClient, row: &WithdrawalRow) -> Result<()> {
    let amount = u64::try_from(row.amount_raw.0)?;
    let cap = row.fee_cap_raw.map(|c| u64::try_from(c.0).unwrap_or(u64::MAX));
    let call = async {
        match row.destination_kind()? {
            DestinationKind::Bolt11 => lightning.pay_bolt11(&row.destination, amount, &row.id, cap).await,
            _ => {
                lightning
                    .pay_lightning_address(&row.destination, amount, row.comment.clone(), Some(row.id.clone()), cap)
                    .await
            }
        }
    };
    let payment = match tokio::time::timeout(Duration::from_secs(PAYOUT_TIMEOUT_SECS), call).await {
        // Outcome unknown: reconciled by `track` (same key on a retry).
        Err(_) => {
            warn!("Lightning withdrawal {} timed out — reconciled later", row.id);
            return Ok(());
        }
        // Refused at the quote: nothing left.
        Ok(Err(err)) if err.downcast_ref::<FeeAboveCap>().is_some() => {
            warn!("Lightning withdrawal {}: {err:#}", row.id);
            return fail_unsent(state, row, "fee_too_high").await;
        }
        Ok(Err(err)) => {
            // The SDK detail stays in the logs. Did anything leave?
            warn!("Lightning withdrawal {} send failed: {err:#}", row.id);
            match lightning.get_payment(&row.id).await {
                Ok(Some(payment)) if payment.direction == PaymentDirection::Send => payment,
                Ok(_) => return fail_unsent(state, row, "payment_failed").await,
                Err(_) => return Ok(()),
            }
        }
        Ok(Ok(payment)) => payment,
    };
    accepted(state, row, &payment).await
}

/// The SDK holds the send: recorded, notified, settled if already final.
async fn accepted(state: &AppState, row: &WithdrawalRow, payment: &WalletPayment) -> Result<()> {
    let row = if row.tx_hash.as_deref() != Some(payment.id.as_str()) {
        let status = row.status()?;
        state
            .db
            .transition_withdrawal(&row.id, &[status], status, &WithdrawalUpdate {
                tx_hash: Some(payment.id.clone()),
                ..Default::default()
            })
            .await?
            .unwrap_or_else(|| row.clone())
    } else {
        row.clone()
    };
    info!("Lightning withdrawal {} accepted by the wallet ({})", row.id, payment.status.as_api());
    audit::record(state, &Actor::system(), "withdrawal.broadcast", Some(("withdrawal", &row.id)), json!({ "payment_id": payment.id })).await;
    // Once per endpoint, however often this runs.
    notify_withdrawal(state, &row, WebhookEventType::WithdrawalBroadcast).await;
    finalize(state, &row, payment).await
}

/// Nothing left the wallet: FAILED (its reservation released).
async fn fail_unsent(state: &AppState, row: &WithdrawalRow, reason: &'static str) -> Result<()> {
    let Some(row) = transition(state, row, WithdrawalEvent::Fail, &WithdrawalUpdate {
        failure_reason: Some(reason.to_string()),
        ..Default::default()
    })
    .await?
    else {
        return Ok(());
    };
    warn!("Withdrawal {} failed: {reason}", row.id);
    audit::record(state, &Actor::system(), "withdrawal.failed", Some(("withdrawal", &row.id)), json!({ "reason": reason })).await;
    notify_withdrawal(state, &row, WebhookEventType::WithdrawalFailed).await;
    Ok(())
}

/// Settle a BROADCAST Lightning withdrawal from its wallet payment.
pub async fn finalize(state: &AppState, row: &WithdrawalRow, payment: &WalletPayment) -> Result<()> {
    if row.status()? != WithdrawalStatus::Broadcast {
        return Ok(());
    }
    let (event, update, action, webhook) = match payment.status {
        WalletPaymentStatus::Pending => return Ok(()),
        WalletPaymentStatus::Completed => (
            WithdrawalEvent::Confirm,
            WithdrawalUpdate {
                fee_raw: Some(payment.fees_sats),
                confirmations: Some(1),
                confirmed_now: true,
                ..Default::default()
            },
            "withdrawal.confirmed",
            WebhookEventType::WithdrawalConfirmed,
        ),
        WalletPaymentStatus::Failed => (
            WithdrawalEvent::Fail,
            WithdrawalUpdate {
                fee_raw: Some(payment.fees_sats),
                failure_reason: Some("payment_failed".into()),
                ..Default::default()
            },
            "withdrawal.failed",
            WebhookEventType::WithdrawalFailed,
        ),
    };
    if let Some(row) = transition(state, row, event, &update).await? {
        info!("Lightning withdrawal {} {}", row.id, row.status);
        audit::record(state, &Actor::system(), action, Some(("withdrawal", &row.id)), json!({ "payment_id": payment.id, "fees_sats": payment.fees_sats.to_string() })).await;
        notify_withdrawal(state, &row, webhook).await;
    }
    Ok(())
}

/// Reconciliation: BROADCAST Lightning withdrawals whose final status the
/// event stream did not deliver.
pub async fn track(state: &AppState) -> Result<()> {
    let Some(lightning) = &state.lightning else {
        return Ok(());
    };
    for row in state.db.withdrawals_with_status(WithdrawalStatus::Broadcast, Some(Network::Lightning)).await? {
        let payment_id = row.tx_hash.clone().unwrap_or_else(|| row.id.clone());
        match lightning.get_payment(&payment_id).await {
            Ok(Some(payment)) => {
                // Accepted while its answer was lost (timeout): notified now.
                notify_withdrawal(state, &row, WebhookEventType::WithdrawalBroadcast).await;
                finalize(state, &row, &payment).await?
            }
            Ok(None) => {
                let since = row.broadcast_at.unwrap_or(row.updated_at);
                if Utc::now() - since < chrono::Duration::seconds(RESEND_AFTER_SECS) || freeze::outflows_frozen(state).await {
                    continue;
                }
                // One sender at a time across containers (Kamal overlap).
                let Some(_guard) = state.db.try_advisory_lock(&format!("lightning-send:{}", row.id)).await? else {
                    continue;
                };
                let Some(row) = state.db.find_withdrawal(&row.id).await? else {
                    continue;
                };
                if row.status()? != WithdrawalStatus::Broadcast {
                    continue;
                }
                warn!("Lightning withdrawal {} has no wallet payment — handed to the SDK again (same key)", row.id);
                send(state, lightning, &row).await?;
            }
            Err(err) => warn!("Lightning withdrawal {} status unreadable: {err:#}", row.id),
        }
    }
    Ok(())
}

/// Event path: a finished wallet send. `true` when it is a gateway
/// withdrawal (settled here, no `payout_*` webhook).
pub async fn on_send_finished(state: &AppState, payment: &WalletPayment) -> Result<bool> {
    if let Some(row) = state.db.find_withdrawal(&payment.id).await? {
        finalize(state, &row, payment).await?;
        return Ok(true);
    }
    state.db.is_gateway_send(&payment.id).await
}
