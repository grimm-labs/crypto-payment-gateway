//! Internal transfers (docs/projects-spec.md §9, §11, §12): the admin
//! routes (`POST /transfers` with TOTP and an `Idempotency-Key`, the
//! lists) and the merchant's read-only view of the transfers its project
//! is a side of.

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::{Extension, Json};
use db::{OwnerFilter, TransferFilter};
use domain::{parse_amount_raw, parse_sats, Network, Token};
use jobs::transfers::{transfer_json, TransferInput};
use jobs::AppState;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::admin::{require_step_up, staff_actor};
use crate::middleware::{ClientIp, CurrentApiKey, CurrentStaff};
use crate::projects::{owner_filter_param, parse_owner_field, require_any_project};
use crate::withdrawals::idempotency_key;
use crate::{rate_limit, ApiError};

#[derive(Debug, Deserialize)]
pub(crate) struct CreateTransferRequest {
    from_project_id: String,
    to_project_id: String,
    network: String,
    token: String,
    /// Decimal string in token units.
    amount: Option<String>,
    /// Integer sats — bitcoin/lightning only.
    amount_sats: Option<String>,
    note: String,
    totp_code: Option<String>,
}

/// Shape checks (no I/O); the rules of §9 run in `jobs::transfers`.
pub(crate) fn validate_transfer_request(body: &CreateTransferRequest) -> Result<TransferInput, ApiError> {
    let from = parse_owner_field(&body.from_project_id)?;
    let to = parse_owner_field(&body.to_project_id)?;
    let network = Network::parse_api(&body.network).map_err(|_| ApiError::bad_request("Invalid network"))?;
    let token = Token::parse_api(&body.token).map_err(|_| ApiError::bad_request("Invalid token"))?;
    let btc_network = matches!(network, Network::Bitcoin | Network::Lightning);
    let amount_raw = match (body.amount.as_deref(), body.amount_sats.as_deref()) {
        (Some(_), Some(_)) => return Err(ApiError::bad_request("Send amount or amount_sats, not both")),
        (None, Some(sats)) if btc_network => {
            u128::from(parse_sats(sats).map_err(|_| ApiError::bad_request("Invalid amount_sats (integer string)"))?)
        }
        (None, Some(_)) => return Err(ApiError::bad_request("amount_sats is only accepted on bitcoin/lightning")),
        (Some(amount), None) => parse_amount_raw(amount.trim(), token).map_err(|_| {
            ApiError::new(
                StatusCode::BAD_REQUEST,
                format!(
                    "Invalid amount: a decimal string greater than 0 with at most {} decimals",
                    token.decimals()
                ),
            )
        })?,
        (None, None) => return Err(ApiError::bad_request("amount is required")),
    };
    Ok(TransferInput {
        from,
        to,
        network,
        token,
        amount_raw,
        note: body.note.clone(),
    })
}

/// `POST /transfers` (ADMIN + TOTP, `Idempotency-Key`): `201`, or `200` on a
/// replay by the same staff member with the same parameters.
pub(crate) async fn create_transfer(
    State(state): State<AppState>,
    Extension(staff): Extension<CurrentStaff>,
    client_ip: ClientIp,
    headers: HeaderMap,
    Json(body): Json<CreateTransferRequest>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let key = idempotency_key(&headers)?;
    let input = validate_transfer_request(&body)?;
    // The pure rules first (422), so a malformed request never burns a
    // single-use code.
    jobs::transfers::validate_transfer(&input).map_err(jobs::withdrawals::WithdrawalError::from)?;
    require_any_project(&state).await?;
    rate_limit::hit(&state, rate_limit::TRANSFERS, &staff.id).await?;
    require_step_up(&state, &staff.id, body.totp_code.as_deref()).await?;
    let created = jobs::transfers::create_transfer(&state, &staff_actor(&staff, &client_ip), &staff.id, &key, &input).await?;
    let status = if created.replayed { StatusCode::OK } else { StatusCode::CREATED };
    Ok((status, Json(transfer_json(&created.row)?)))
}

#[derive(Debug, Deserialize)]
pub(crate) struct TransferListQuery {
    /// Staff: `<uuid>` (either side) or `merchant`.
    project_id: Option<String>,
    network: Option<String>,
    token: Option<String>,
    limit: Option<i64>,
    offset: Option<i64>,
}

fn list_filter(query: &TransferListQuery, owner: OwnerFilter) -> Result<TransferFilter, ApiError> {
    Ok(TransferFilter {
        owner,
        network: query
            .network
            .as_deref()
            .map(Network::parse_api)
            .transpose()
            .map_err(|_| ApiError::bad_request("Invalid network"))?,
        token: query
            .token
            .as_deref()
            .map(Token::parse_api)
            .transpose()
            .map_err(|_| ApiError::bad_request("Invalid token"))?,
    })
}

async fn list(state: &AppState, filter: &TransferFilter, query: &TransferListQuery) -> Result<Json<Value>, ApiError> {
    let limit = query.limit.unwrap_or(50).clamp(1, 100);
    let offset = query.offset.unwrap_or(0).max(0);
    let (rows, total) = state.db.list_internal_transfers(filter, limit, offset).await?;
    let data = rows.iter().map(transfer_json).collect::<Result<Vec<_>, _>>()?;
    Ok(Json(json!({ "data": data, "total": total, "limit": limit, "offset": offset })))
}

/// `GET /transfers` (any staff).
pub(crate) async fn list_transfers(
    State(state): State<AppState>,
    Query(query): Query<TransferListQuery>,
) -> Result<Json<Value>, ApiError> {
    let filter = list_filter(&query, owner_filter_param(query.project_id.as_deref())?)?;
    list(&state, &filter, &query).await
}

/// `GET /transfers/{id}` (any staff).
pub(crate) async fn get_transfer(State(state): State<AppState>, Path(id): Path<String>) -> Result<Json<Value>, ApiError> {
    let row = state
        .db
        .find_internal_transfer(&id)
        .await?
        .ok_or_else(|| ApiError::not_found(format!("Transfer not found: {id}")))?;
    Ok(Json(transfer_json(&row)?))
}

/// The merchant's scope: its project's transfers (either side). A legacy
/// key without a project is a side of none.
fn merchant_project(key: &CurrentApiKey) -> Result<&str, ApiError> {
    key.project_id
        .as_deref()
        .ok_or_else(|| ApiError::conflict("This API key belongs to no project"))
}

/// `GET /api/v1/transfers` (read-only, invariant 2).
pub(crate) async fn merchant_list_transfers(
    State(state): State<AppState>,
    Extension(key): Extension<CurrentApiKey>,
    Query(query): Query<TransferListQuery>,
) -> Result<Json<Value>, ApiError> {
    let project_id = merchant_project(&key)?.to_string();
    let filter = list_filter(&query, OwnerFilter::Project(project_id))?;
    list(&state, &filter, &query).await
}

/// `GET /api/v1/transfers/{id}`: 404 unless the project is a side of it.
pub(crate) async fn merchant_get_transfer(
    State(state): State<AppState>,
    Extension(key): Extension<CurrentApiKey>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let project_id = merchant_project(&key)?;
    let row = state
        .db
        .find_internal_transfer_for_project(&id, project_id)
        .await?
        .ok_or_else(|| ApiError::not_found(format!("Transfer not found: {id}")))?;
    Ok(Json(transfer_json(&row)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::Owner;

    fn request(body: Value) -> CreateTransferRequest {
        serde_json::from_value(body).expect("valid shape")
    }

    #[test]
    fn transfer_bodies_take_owners_and_amounts_per_network() {
        let p = "6f1c7a8e-2f43-4d55-9a8b-1c2d3e4f5a6b";
        let input = validate_transfer_request(&request(json!({
            "from_project_id": p, "to_project_id": "merchant", "network": "bitcoin", "token": "BTC",
            "amount_sats": "1500", "note": "back to the merchant"
        })))
        .unwrap();
        assert_eq!(input.from, Owner::Project(p.into()));
        assert_eq!(input.to, Owner::Merchant);
        assert_eq!(input.amount_raw, 1_500);
        let usdt = validate_transfer_request(&request(json!({
            "from_project_id": "merchant", "to_project_id": p, "network": "tron", "token": "USDT",
            "amount": "25.5", "note": "n"
        })))
        .unwrap();
        assert_eq!(usdt.amount_raw, 25_500_000);
        for bad in [
            json!({ "from_project_id": "shop", "to_project_id": p, "network": "tron", "token": "USDT", "amount": "1", "note": "n" }),
            json!({ "from_project_id": p, "to_project_id": "merchant", "network": "tron", "token": "USDT", "amount_sats": "1", "note": "n" }),
            json!({ "from_project_id": p, "to_project_id": "merchant", "network": "bitcoin", "token": "BTC", "amount": "1", "amount_sats": "1", "note": "n" }),
            json!({ "from_project_id": p, "to_project_id": "merchant", "network": "tron", "token": "USDT", "note": "n" }),
        ] {
            assert!(validate_transfer_request(&request(bad.clone())).is_err(), "{bad}");
        }
    }
}
