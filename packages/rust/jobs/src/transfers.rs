//! Internal transfers (docs/projects-spec.md §9): an admin's ledger move of
//! one asset on one network between two owners. Nothing moves on chain.
//! Recorded under the per-asset advisory lock the withdrawal admission
//! uses (invariant 3), idempotent per `Idempotency-Key`, append-only
//! (invariant 6), notified to both sides (invariant 5).

use anyhow::Result;
use db::{InternalTransferRow, NewInternalTransfer};
use domain::{raw_u128_to_string, Network, Owner, Token, WebhookEventType};
use serde_json::{json, Value};
use tracing::{error, info};

use crate::audit::{self, Actor};
use crate::withdrawals::{RuleError, WithdrawalError};
use crate::{enqueue_webhook, ledger, webhooks, AppState};

/// A transfer request that passed the HTTP-level shape checks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferInput {
    pub from: Owner,
    pub to: Owner,
    pub network: Network,
    pub token: Token,
    pub amount_raw: u128,
    pub note: String,
}

/// Longest note.
pub const NOTE_MAX_CHARS: usize = 200;

/// The pure rules (spec §9): a payment pair, two different owners, a
/// positive amount, a note of 1–200 characters.
pub fn validate_transfer(input: &TransferInput) -> Result<(), RuleError> {
    if !domain::supports(input.network, input.token) {
        return Err(RuleError::Refused {
            code: "unsupported_asset",
            message: format!(
                "{} on {} is not a payment pair: projects never hold it",
                input.token.as_api(),
                input.network.as_api()
            ),
        });
    }
    if input.from == input.to {
        return Err(RuleError::Refused {
            code: "same_owner",
            message: "A transfer needs two different owners".into(),
        });
    }
    if input.amount_raw == 0 {
        return Err(RuleError::Refused {
            code: "invalid_amount",
            message: "amount must be greater than 0".into(),
        });
    }
    let note_len = input.note.trim().chars().count();
    if note_len == 0 || note_len > NOTE_MAX_CHARS {
        return Err(RuleError::Refused {
            code: "invalid_note",
            message: format!("note is required (at most {NOTE_MAX_CHARS} characters)"),
        });
    }
    Ok(())
}

fn owner_key(owner: &Owner) -> &str {
    owner.project_id().unwrap_or(domain::MERCHANT_OWNER)
}

/// SHA-256 of the normalized parameters — what a replayed key must match.
pub fn params_hash(input: &TransferInput) -> String {
    auth::sha256_hex(&format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        owner_key(&input.from),
        owner_key(&input.to),
        input.network.as_db(),
        input.token.as_db(),
        input.amount_raw,
        input.note.trim()
    ))
}

#[derive(Debug, Clone)]
pub struct CreatedTransfer {
    pub row: InternalTransferRow,
    /// Same key, staff member and parameters: the stored transfer.
    pub replayed: bool,
}

fn insufficient(code: &'static str, message: &str) -> WithdrawalError {
    WithdrawalError::Rule(RuleError::Refused {
        code,
        message: message.into(),
    })
}

/// Record a transfer (spec §9): validation, the project or merchant-funds
/// check inside the `withdrawal-asset:{TOKEN}` lock, the row, the ledger
/// cache, the audit entry and `transfer_completed` to both sides.
pub async fn create_transfer(
    state: &AppState,
    actor: &Actor,
    staff_id: &str,
    idempotency_key: &str,
    input: &TransferInput,
) -> Result<CreatedTransfer, WithdrawalError> {
    validate_transfer(input)?;
    let note = input.note.trim();
    let hash = params_hash(input);
    for owner in [&input.from, &input.to] {
        if let Owner::Project(id) = owner {
            if state.db.find_project(id).await?.is_none() {
                return Err(WithdrawalError::BadRequest("Unknown project".into()));
            }
        }
    }

    let mut admission = state.db.begin_withdrawal_admission(&[input.token]).await?;
    if let Some(existing) = db::transfers::find_internal_transfer_by_key(admission.conn(), idempotency_key).await? {
        if existing.staff_id != staff_id || existing.params_hash != hash {
            return Err(WithdrawalError::KeyReused);
        }
        return Ok(CreatedTransfer { row: existing, replayed: true });
    }
    match &input.from {
        Owner::Project(project_id) => {
            let balance = ledger::project_balance_on(admission.conn(), project_id, input.network, input.token).await?;
            if !balance.covers(input.amount_raw) {
                return Err(insufficient(
                    "project_insufficient_funds",
                    "Insufficient project balance for this transfer",
                ));
            }
        }
        Owner::Merchant => {
            // Fresh holdings (cache ≤ 30 s, spec §9) minus every project balance.
            let merchant = ledger::merchant_funds(state, admission.conn(), input.network, input.token).await?;
            let Some(funds) = merchant.merchant_funds else {
                return Err(WithdrawalError::NetworkDisabled("Holdings unavailable, retry later".into()));
            };
            if i128::try_from(input.amount_raw).map(|amount| funds < amount).unwrap_or(true) {
                return Err(insufficient(
                    "merchant_insufficient_funds",
                    "Insufficient merchant funds for this transfer",
                ));
            }
        }
    }
    let row = db::transfers::insert_internal_transfer(
        admission.conn(),
        &NewInternalTransfer {
            idempotency_key,
            params_hash: &hash,
            network: input.network,
            token: input.token,
            amount_raw: input.amount_raw,
            from: input.from.clone(),
            to: input.to.clone(),
            note,
            staff_id,
        },
    )
    .await?;
    admission.commit().await?;
    ledger::invalidate_cache(state).await;

    info!(
        "Internal transfer {}: {} {} on {} from {} to {}",
        row.id,
        raw_u128_to_string(input.amount_raw, input.token),
        input.token.as_api(),
        input.network.as_api(),
        owner_key(&input.from),
        owner_key(&input.to)
    );
    audit::record(
        state,
        actor,
        "transfer.created",
        Some(("internal_transfer", &row.id)),
        json!({
            "network": input.network.as_api(),
            "token": input.token.as_api(),
            "amount_raw": input.amount_raw.to_string(),
            "from": owner_json(&input.from),
            "to": owner_json(&input.to),
            "note": note,
        }),
    )
    .await;
    notify_transfer(state, &row).await;
    Ok(CreatedTransfer { row, replayed: false })
}

/// `{ "type": "project" | "merchant", "id": <uuid> | null }`.
pub fn owner_json(owner: &Owner) -> Value {
    json!({ "type": owner.kind().as_api(), "id": owner.project_id() })
}

/// Queue `transfer_completed` for the endpoints of both sides, once per
/// endpoint. A queueing failure is logged: the transfer already exists.
async fn notify_transfer(state: &AppState, row: &InternalTransferRow) {
    let event = WebhookEventType::TransferCompleted;
    let owners = webhooks::SubjectOwners::Both(row.from_owner(), row.to_owner());
    let result: Result<()> = async {
        for endpoint in webhooks::endpoints_for(state, event, &owners).await? {
            if state
                .db
                .find_subject_webhook_delivery(event, &row.id, &endpoint.id)
                .await?
                .is_some()
            {
                continue;
            }
            enqueue_webhook(state, event, &row.id, &endpoint.id).await?;
        }
        Ok(())
    }
    .await;
    if let Err(err) = result {
        error!("Failed to queue transfer_completed for transfer {}: {err:#}", row.id);
    }
}

/// Webhook body of `transfer_completed` (spec §10.3).
pub fn transfer_payload(row: &InternalTransferRow) -> Result<Value> {
    Ok(json!({
        "event_type": WebhookEventType::TransferCompleted.payload_event_type(),
        "data": transfer_json(row)?,
    }))
}

/// The API and payload shape of a transfer.
pub fn transfer_json(row: &InternalTransferRow) -> Result<Value> {
    let token = row.token()?;
    Ok(json!({
        "transfer_id": row.id,
        "id": row.id,
        "network": row.network()?.as_api(),
        "token": token.as_api(),
        "amount": raw_u128_to_string(row.amount_raw.0, token),
        "amount_raw": row.amount_raw.0.to_string(),
        "from": owner_json(&row.from_owner()),
        "to": owner_json(&row.to_owner()),
        "note": row.note,
        "created_at": row.created_at.to_rfc3339(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(from: Owner, to: Owner, network: Network, token: Token, amount: u128, note: &str) -> TransferInput {
        TransferInput { from, to, network, token, amount_raw: amount, note: note.into() }
    }

    #[test]
    fn transfers_need_a_payment_pair_two_owners_an_amount_and_a_note() {
        let p1 = Owner::Project("p1".into());
        let p2 = Owner::Project("p2".into());
        assert!(validate_transfer(&input(p1.clone(), p2.clone(), Network::Bitcoin, Token::Btc, 1, "ok")).is_ok());
        assert!(validate_transfer(&input(p1.clone(), Owner::Merchant, Network::Ethereum, Token::Eth, 1, "ok")).is_ok());
        assert_eq!(
            validate_transfer(&input(p1.clone(), p2.clone(), Network::Tron, Token::Trx, 1, "gas")).unwrap_err().code(),
            "unsupported_asset"
        );
        assert_eq!(
            validate_transfer(&input(p1.clone(), p1.clone(), Network::Tron, Token::Usdt, 1, "same")).unwrap_err().code(),
            "same_owner"
        );
        assert_eq!(
            validate_transfer(&input(Owner::Merchant, Owner::Merchant, Network::Tron, Token::Usdt, 1, "same")).unwrap_err().code(),
            "same_owner"
        );
        assert_eq!(
            validate_transfer(&input(p1.clone(), p2.clone(), Network::Tron, Token::Usdt, 0, "zero")).unwrap_err().code(),
            "invalid_amount"
        );
        assert_eq!(
            validate_transfer(&input(p1.clone(), p2.clone(), Network::Tron, Token::Usdt, 1, "   ")).unwrap_err().code(),
            "invalid_note"
        );
        assert_eq!(
            validate_transfer(&input(p1, p2, Network::Tron, Token::Usdt, 1, &"n".repeat(201))).unwrap_err().code(),
            "invalid_note"
        );
    }

    #[test]
    fn the_params_hash_covers_every_field_and_normalizes_the_note() {
        let base = input(Owner::Project("p1".into()), Owner::Merchant, Network::Tron, Token::Usdt, 10, "note");
        let same = input(Owner::Project("p1".into()), Owner::Merchant, Network::Tron, Token::Usdt, 10, "  note ");
        assert_eq!(params_hash(&base), params_hash(&same));
        let mut other = base.clone();
        other.amount_raw = 11;
        assert_ne!(params_hash(&base), params_hash(&other));
        let mut swapped = base.clone();
        std::mem::swap(&mut swapped.from, &mut swapped.to);
        assert_ne!(params_hash(&base), params_hash(&swapped));
        let mut renamed = base.clone();
        renamed.note = "other".into();
        assert_ne!(params_hash(&base), params_hash(&renamed));
    }

    #[test]
    fn owners_render_as_type_and_id() {
        assert_eq!(owner_json(&Owner::Merchant), json!({ "type": "merchant", "id": null }));
        assert_eq!(owner_json(&Owner::Project("p1".into())), json!({ "type": "project", "id": "p1" }));
    }
}
