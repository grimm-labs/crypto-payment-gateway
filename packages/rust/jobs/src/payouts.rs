//! Lightning payouts with guard rails: per-payout and rolling-24 h limits
//! (`app_settings`), an Idempotency-Key bound to the caller and the
//! parameters (`payout_requests`), a bounded call time and an audit trail.

use std::time::Duration;

use config::{PAYOUT_STALE_PENDING_SECS, PAYOUT_TIMEOUT_SECS};
use db::{NewPayoutRequest, PayoutReservation};
use lightning_client::{PaymentDirection, PayoutQuote, WalletPayment};
use serde_json::json;
use tracing::{error, info, warn};

use crate::audit::{self, Actor};
use crate::AppState;

/// A validated payout request (address shape, amount and key format are
/// checked by the HTTP layer).
#[derive(Debug, Clone)]
pub struct PayoutCommand {
    pub ln_address: String,
    pub amount_sats: u64,
    pub comment: Option<String>,
    /// Canonical UUID — also the SDK payment id of the send.
    pub idempotency_key: String,
    /// Exactly one of the two.
    pub api_key_id: Option<String>,
    pub staff_id: Option<String>,
}

#[derive(Debug)]
pub enum PayoutError {
    /// Lightning is not configured.
    Unavailable,
    /// A limit is set to 0.
    PayoutsDisabled,
    PerPayoutLimit { max_sats: u64 },
    DailyLimit { used_sats: u64, max_sats: u64 },
    InProgress,
    /// The key was already used by another caller or with other parameters.
    KeyReused,
    /// The SDK refused or failed (detail logged, never returned).
    Failed,
    Internal(anyhow::Error),
}

impl From<anyhow::Error> for PayoutError {
    fn from(value: anyhow::Error) -> Self {
        Self::Internal(value)
    }
}

#[derive(Debug, Clone)]
pub struct PayoutOutcome {
    pub payment: WalletPayment,
    /// `true` when the key had already been sent: nothing new was sent.
    pub replayed: bool,
}

/// SHA-256 of the normalized parameters — what a replayed key must match.
pub fn payout_params_hash(ln_address: &str, amount_sats: u64, comment: Option<&str>) -> String {
    auth::sha256_hex(&format!(
        "{}\n{amount_sats}\n{}",
        ln_address.trim().to_ascii_lowercase(),
        comment.unwrap_or_default()
    ))
}

/// Pay a Lightning address from the wallet, once per Idempotency-Key.
pub async fn request_payout(
    state: &AppState,
    actor: &Actor,
    command: &PayoutCommand,
) -> Result<PayoutOutcome, PayoutError> {
    let lightning = state.lightning.as_ref().ok_or(PayoutError::Unavailable)?;
    let params_hash =
        payout_params_hash(&command.ln_address, command.amount_sats, command.comment.as_deref());
    let max_sats = state.db.payout_max_sats().await?;
    let daily_max_sats = state.db.payout_daily_max_sats().await?;

    let reservation = state
        .db
        .reserve_payout(
            &NewPayoutRequest {
                idempotency_key: &command.idempotency_key,
                api_key_id: command.api_key_id.as_deref(),
                staff_id: command.staff_id.as_deref(),
                params_hash: &params_hash,
                ln_address: &command.ln_address,
                amount_sats: command.amount_sats,
                comment: command.comment.as_deref(),
            },
            max_sats,
            daily_max_sats,
            PAYOUT_STALE_PENDING_SECS,
        )
        .await?;

    let request = match reservation {
        PayoutReservation::Reserved(row) => row,
        PayoutReservation::AlreadySent(row) => {
            let payment_id = row.payment_id.unwrap_or(row.idempotency_key);
            let payment = lightning
                .get_payment(&payment_id)
                .await?
                .filter(|payment| payment.direction == PaymentDirection::Send)
                .ok_or_else(|| {
                    PayoutError::Internal(anyhow::anyhow!(
                        "payout request {} is SENT but the wallet has no send {payment_id}",
                        row.id
                    ))
                })?;
            return Ok(PayoutOutcome {
                payment,
                replayed: true,
            });
        }
        PayoutReservation::InProgress => return Err(PayoutError::InProgress),
        PayoutReservation::CallerMismatch | PayoutReservation::ParamsMismatch => {
            return Err(PayoutError::KeyReused)
        }
        PayoutReservation::Disabled => return Err(PayoutError::PayoutsDisabled),
        PayoutReservation::PerPayoutLimit { max_sats } => {
            return Err(PayoutError::PerPayoutLimit { max_sats })
        }
        PayoutReservation::DailyLimit {
            used_sats,
            max_sats,
        } => {
            warn!(
                "Lightning payout refused: daily limit ({used_sats} + {} > {max_sats} sats)",
                command.amount_sats
            );
            return Err(PayoutError::DailyLimit {
                used_sats,
                max_sats,
            });
        }
    };

    audit::record(
        state,
        actor,
        "payout.requested",
        Some(("payout_request", &request.id)),
        json!({
            "ln_address": command.ln_address,
            "amount_sats": command.amount_sats.to_string(),
            "idempotency_key": command.idempotency_key,
        }),
    )
    .await;

    let send = lightning.pay_lightning_address(
        &command.ln_address,
        command.amount_sats,
        command.comment.clone(),
        Some(command.idempotency_key.clone()),
    );
    match tokio::time::timeout(Duration::from_secs(PAYOUT_TIMEOUT_SECS), send).await {
        Ok(Ok(payment)) => {
            if let Err(err) = state.db.mark_payout_sent(&request.id, &payment.id).await {
                error!("Payout {} sent but not recorded: {err:#}", payment.id);
            }
            info!(
                "Lightning payout {} accepted: {} sats to {} ({}), request {} by {:?}",
                payment.id,
                command.amount_sats,
                command.ln_address,
                payment.status.as_api(),
                request.id,
                actor.audit
            );
            audit::record(
                state,
                actor,
                "payout.sent",
                Some(("lightning_payment", &payment.id)),
                json!({
                    "payout_request_id": request.id,
                    "ln_address": command.ln_address,
                    "amount_sats": command.amount_sats.to_string(),
                    "fees_sats": payment.fees_sats.to_string(),
                    "status": payment.status.as_api(),
                }),
            )
            .await;
            Ok(PayoutOutcome {
                payment,
                replayed: false,
            })
        }
        Ok(Err(err)) => {
            // The SDK detail stays in the logs.
            warn!(
                "Lightning payout request {} to {} failed: {err:#}",
                request.id, command.ln_address
            );
            if let Err(err) = state.db.mark_payout_failed(&request.id, "Payment failed").await {
                error!("Failed payout {} not recorded: {err:#}", request.id);
            }
            audit::record(
                state,
                actor,
                "payout.failed",
                Some(("payout_request", &request.id)),
                json!({ "ln_address": command.ln_address, "amount_sats": command.amount_sats.to_string() }),
            )
            .await;
            Err(PayoutError::Failed)
        }
        Err(_) => {
            // Outcome unknown: the request stays PENDING (still counted in
            // the limits); once stale, the same key may be retried — the SDK
            // answers a sent key from storage instead of paying twice.
            error!(
                "Lightning payout request {} timed out after {PAYOUT_TIMEOUT_SECS}s — outcome unknown",
                request.id
            );
            Err(PayoutError::Failed)
        }
    }
}

/// The recipient's invoice and fee for a payout, without paying. Enforces
/// the per-payout limit so the caller learns about it before confirming.
pub async fn quote_payout(
    state: &AppState,
    ln_address: &str,
    amount_sats: u64,
    comment: Option<String>,
) -> Result<PayoutQuote, PayoutError> {
    let lightning = state.lightning.as_ref().ok_or(PayoutError::Unavailable)?;
    let max_sats = state.db.payout_max_sats().await?;
    let daily_max_sats = state.db.payout_daily_max_sats().await?;
    if max_sats == 0 || daily_max_sats == 0 {
        return Err(PayoutError::PayoutsDisabled);
    }
    if amount_sats > max_sats {
        return Err(PayoutError::PerPayoutLimit { max_sats });
    }
    let quote = lightning.quote_lightning_address(ln_address, amount_sats, comment);
    match tokio::time::timeout(Duration::from_secs(PAYOUT_TIMEOUT_SECS), quote).await {
        Ok(Ok(quote)) => Ok(quote),
        Ok(Err(err)) => {
            warn!("Lightning payout quote for {ln_address} failed: {err:#}");
            Err(PayoutError::Failed)
        }
        Err(_) => {
            warn!("Lightning payout quote for {ln_address} timed out");
            Err(PayoutError::Failed)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn params_hash_normalizes_the_address_but_not_the_amount_or_comment() {
        let base = payout_params_hash("Alice@Example.com", 1_000, Some("thanks"));
        assert_eq!(base, payout_params_hash(" alice@example.com ", 1_000, Some("thanks")));
        assert_ne!(base, payout_params_hash("alice@example.com", 1_001, Some("thanks")));
        assert_ne!(base, payout_params_hash("alice@example.com", 1_000, Some("thanks!")));
        assert_ne!(base, payout_params_hash("bob@example.com", 1_000, Some("thanks")));
        assert_ne!(
            payout_params_hash("alice@example.com", 1_000, None),
            payout_params_hash("alice@example.com", 1_000, Some("\n"))
        );
    }
}
