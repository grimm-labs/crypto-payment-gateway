//! Withdrawal HTTP surface (spec §7.13): merchant `/api/v1/withdrawals`,
//! the admin routes (withdrawals, approval, address book, treasury, sweeps,
//! outflow freeze, asset settings), and the deprecated Lightning payout
//! alias.

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use chrono::Utc;
use config::{
    ADDRESS_BOOK_COOLING_OFF_SECS, ADDRESS_BOOK_LABEL_MAX_CHARS, WITHDRAWAL_COMMENT_MAX_CHARS,
    WITHDRAWAL_METADATA_MAX_BYTES, WITHDRAWAL_REFERENCE_MAX_CHARS,
};
use db::{
    AddressBookEntryRow, AssetSettingsRow, AssetSettingsUpdate, NewAddressBookEntry, OutboundTxRow,
    OwnerFilter, SweepRow, WithdrawalFilter, WithdrawalRow,
};
use domain::{
    is_withdrawable, parse_amount_raw, parse_sats, raw_u128_to_string, AlertType, DestinationKind,
    Network, Owner, StaffRole, SweepStatus, Token, WithdrawalStatus,
};
use jobs::freeze::FreezeError;
use jobs::withdrawals::{
    Canceller, DestinationInput, Requester, RuleError, WithdrawalError, WithdrawalInput, WithdrawalQuote,
};
use jobs::AppState;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::admin::{require_step_up, staff_actor};
use crate::middleware::{ClientIp, CurrentApiKey, CurrentStaff};
use crate::projects::{owner_filter_param, parse_owner_field, require_any_project};
use crate::{actor, rate_limit, ApiError};

impl From<WithdrawalError> for ApiError {
    fn from(err: WithdrawalError) -> Self {
        match err {
            WithdrawalError::Rule(RuleError::Forbidden(message)) => ApiError::new(StatusCode::FORBIDDEN, message),
            WithdrawalError::Rule(refused) => ApiError::unprocessable(refused.to_string()),
            WithdrawalError::BadRequest(message) => ApiError::new(StatusCode::BAD_REQUEST, message),
            WithdrawalError::NotFound => ApiError::not_found("Withdrawal not found".into()),
            WithdrawalError::Conflict(message) => ApiError::new(StatusCode::CONFLICT, message),
            WithdrawalError::KeyReused => {
                ApiError::unprocessable("Idempotency-Key was already used with different parameters".into())
            }
            WithdrawalError::Frozen => ApiError::service_unavailable("Outflows are frozen".into()),
            WithdrawalError::NetworkDisabled(message) => ApiError::service_unavailable(message),
            WithdrawalError::Provider(err) => {
                // The provider detail (and its URL-free error) stays in the logs.
                tracing::warn!("Withdrawal provider call failed: {err:#}");
                ApiError::bad_gateway("Could not reach the network provider")
            }
            WithdrawalError::Internal(err) => ApiError::internal(format!("{err:#}")),
        }
    }
}

/// Body of `POST /withdrawals` and `/withdrawals/quote`.
#[derive(Debug, Deserialize)]
pub(crate) struct WithdrawalRequest {
    network: String,
    token: String,
    /// Decimal string in token units.
    amount: Option<String>,
    /// Integer string — bitcoin/lightning only.
    amount_sats: Option<String>,
    destination: Option<String>,
    address_book_entry_id: Option<String>,
    reference: Option<String>,
    /// Lightning-address comment.
    comment: Option<String>,
    metadata: Option<Value>,
    /// Staff only: fresh TOTP code (create).
    totp_code: Option<String>,
    /// Staff only: the owner debited — a project UUID or `"merchant"`
    /// (spec §8.1). A merchant key's withdrawals belong to its project.
    project_id: Option<String>,
}

/// Shape checks of a withdrawal request (everything that needs no I/O).
/// `merchant`: the caller is a merchant key, whose body may not name an
/// owner.
pub(crate) fn validate_withdrawal_request(body: &WithdrawalRequest, merchant: bool) -> Result<WithdrawalInput, ApiError> {
    let owner = match body.project_id.as_deref() {
        Some(_) if merchant => {
            return Err(ApiError::unprocessable(
                "project_id is not accepted from a merchant key: its withdrawals belong to its project".into(),
            ))
        }
        Some(value) => Some(parse_owner_field(value)?),
        None => None,
    };
    let network = Network::parse_api(&body.network).map_err(|_| ApiError::bad_request("Invalid network"))?;
    let token = Token::parse_api(&body.token).map_err(|_| ApiError::bad_request("Invalid token"))?;
    if !is_withdrawable(network, token) {
        return Err(ApiError::bad_request("This token cannot be withdrawn on this network"));
    }
    let btc_network = matches!(network, Network::Bitcoin | Network::Lightning);
    let amount_raw = match (body.amount.as_deref(), body.amount_sats.as_deref()) {
        (Some(_), Some(_)) => return Err(ApiError::bad_request("Send amount or amount_sats, not both")),
        (None, Some(sats)) if btc_network => u128::from(
            parse_sats(sats).map_err(|_| ApiError::bad_request("Invalid amount_sats (integer string)"))?,
        ),
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
    let destination = match (body.destination.as_deref(), body.address_book_entry_id.as_deref()) {
        (Some(destination), None) if !destination.trim().is_empty() => DestinationInput::Raw(destination.trim().to_string()),
        (None, Some(id)) if !id.trim().is_empty() => DestinationInput::BookEntry(id.trim().to_string()),
        _ => return Err(ApiError::bad_request("Send exactly one of destination and address_book_entry_id")),
    };
    let reference = body.reference.as_deref().map(str::trim).filter(|r| !r.is_empty());
    if reference.is_some_and(|r| r.chars().count() > WITHDRAWAL_REFERENCE_MAX_CHARS) {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            format!("reference must be at most {WITHDRAWAL_REFERENCE_MAX_CHARS} characters"),
        ));
    }
    let comment = body.comment.as_deref().map(str::trim).filter(|c| !c.is_empty());
    if comment.is_some() && network != Network::Lightning {
        return Err(ApiError::bad_request("comment is only accepted on lightning"));
    }
    if comment.is_some_and(|c| c.chars().count() > WITHDRAWAL_COMMENT_MAX_CHARS) {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            format!("comment must be at most {WITHDRAWAL_COMMENT_MAX_CHARS} characters"),
        ));
    }
    let metadata = body.metadata.clone().unwrap_or_else(|| json!({}));
    if !metadata.is_object() {
        return Err(ApiError::bad_request("metadata must be a JSON object"));
    }
    if serde_json::to_vec(&metadata)?.len() > WITHDRAWAL_METADATA_MAX_BYTES {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            format!("metadata must be at most {WITHDRAWAL_METADATA_MAX_BYTES} bytes"),
        ));
    }
    Ok(WithdrawalInput {
        network,
        token,
        amount_raw,
        destination,
        reference: reference.map(str::to_string),
        comment: comment.map(str::to_string),
        metadata,
        id_is_idempotency_key: false,
        owner,
    })
}

/// The canonical UUID of the required `Idempotency-Key` header.
pub(crate) fn idempotency_key(headers: &HeaderMap) -> Result<String, ApiError> {
    let raw = headers
        .get("idempotency-key")
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| ApiError::bad_request("Idempotency-Key header is required (UUID)"))?;
    uuid::Uuid::parse_str(raw.trim())
        .map(|uuid| uuid.hyphenated().to_string())
        .map_err(|_| ApiError::bad_request("Idempotency-Key must be a UUID"))
}

pub(crate) fn key_requester(key: &CurrentApiKey) -> Requester {
    Requester::ApiKey {
        id: key.id.clone(),
        project_id: key.project_id.clone(),
        policy: key.withdrawal_policy,
        tokens: key.withdrawal_tokens.clone(),
        networks: key.withdrawal_networks.clone(),
    }
}

pub(crate) fn staff_requester(staff: &CurrentStaff) -> Requester {
    Requester::Staff { id: staff.id.clone(), role: staff.role }
}

fn render(raw: u128, token: Token) -> String {
    raw_u128_to_string(raw, token)
}

/// The `Withdrawal` response shape.
pub(crate) fn withdrawal_json(row: &WithdrawalRow) -> Result<Value, ApiError> {
    let token = row.token()?;
    let fee_token = row.fee_token()?;
    Ok(json!({
        "id": row.id,
        // The owner: a project, or null for the merchant funds.
        "project_id": row.project_id,
        "status": row.status()?.as_api(),
        "network": row.network()?.as_api(),
        "token": token.as_api(),
        "amount": render(row.amount_raw.0, token),
        "amount_raw": row.amount_raw.0.to_string(),
        "fee_estimate": row.fee_estimate_raw.map(|f| render(f.0, fee_token)),
        "fee_estimate_raw": row.fee_estimate_raw.map(|f| f.0.to_string()),
        "fee": row.fee_raw.map(|f| render(f.0, fee_token)),
        "fee_raw": row.fee_raw.map(|f| f.0.to_string()),
        "fee_token": fee_token.as_api(),
        "destination": row.destination,
        "destination_kind": row.destination_kind()?.as_api(),
        "address_book_entry_id": row.address_book_entry_id,
        "approval_required": row.approval_required,
        "approved_by": row.approved_by_id,
        "approved_at": row.approved_at.map(|t| t.to_rfc3339()),
        "approval_expires_at": row.approval_expires_at.map(|t| t.to_rfc3339()),
        "self_approval_not_before": row.self_approval_not_before.map(|t| t.to_rfc3339()),
        "cancel_reason": row.cancel_reason,
        "tx_hash": row.tx_hash,
        "confirmations": row.confirmations,
        "failure_reason": row.failure_reason,
        "reference": row.reference,
        "comment": row.comment,
        "metadata": row.metadata,
        "requested_by": jobs::withdrawals::requested_by(row),
        "created_at": row.created_at.to_rfc3339(),
        "updated_at": row.updated_at.to_rfc3339(),
        "broadcast_at": row.broadcast_at.map(|t| t.to_rfc3339()),
        "confirmed_at": row.confirmed_at.map(|t| t.to_rfc3339()),
    }))
}

fn outbound_json(tx: &OutboundTxRow) -> Value {
    json!({
        "id": tx.id,
        "purpose": domain::OutboundPurpose::from_db(&tx.purpose).map(|p| p.as_api()).unwrap_or("withdrawal"),
        "tx_hash": tx.tx_hash,
        "status": domain::OutboundTxStatus::from_db(&tx.status).map(|s| s.as_api()).unwrap_or("signed"),
        "from_address": tx.from_address,
        "to_address": tx.to_address,
        "nonce": tx.nonce,
        "fee_raw": tx.fee_raw.map(|f| f.0.to_string()),
        "replaces_id": tx.replaces_id,
        "block_number": tx.block_number,
        "confirmations": tx.confirmations,
        "last_error": tx.last_error,
        "created_at": tx.created_at.to_rfc3339(),
        "broadcast_at": tx.broadcast_at.map(|t| t.to_rfc3339()),
        "confirmed_at": tx.confirmed_at.map(|t| t.to_rfc3339()),
    })
}

fn quote_json(quote: &WithdrawalQuote) -> Value {
    json!({
        "network": quote.network.as_api(),
        "token": quote.token.as_api(),
        "amount": render(quote.amount_raw, quote.token),
        "amount_raw": quote.amount_raw.to_string(),
        "fee_estimate": render(quote.fee_estimate_raw, quote.fee_token),
        "fee_estimate_raw": quote.fee_estimate_raw.to_string(),
        "fee_token": quote.fee_token.as_api(),
        "approval_required": quote.approval_required,
        "destination": quote.destination,
        "destination_kind": quote.destination_kind.as_api(),
        "address_book_entry_id": quote.address_book_entry_id,
        "destination_acceptable": quote.destination_refusal.is_none(),
        "destination_refusal": quote.destination_refusal.as_ref().map(|r| r.to_string()),
        "available": quote.available_raw.map(|raw| render(raw, quote.token)),
        "available_raw": quote.available_raw.map(|raw| raw.to_string()),
        "fee_available": quote.fee_available_raw.map(|raw| render(raw, quote.fee_token)),
        "fee_available_raw": quote.fee_available_raw.map(|raw| raw.to_string()),
    })
}

fn caller_id(api_key: Option<&CurrentApiKey>, staff: Option<&CurrentStaff>) -> String {
    api_key
        .map(|key| key.id.clone())
        .or_else(|| staff.map(|staff| staff.id.clone()))
        .unwrap_or_default()
}

/// `POST /withdrawals` (merchant key or ADMIN + TOTP): 202 new, 200 replay.
pub(crate) async fn create_withdrawal(
    State(state): State<AppState>,
    api_key: Option<Extension<CurrentApiKey>>,
    staff: Option<Extension<CurrentStaff>>,
    client_ip: ClientIp,
    headers: HeaderMap,
    Json(body): Json<WithdrawalRequest>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let key = idempotency_key(&headers)?;
    let input = validate_withdrawal_request(&body, api_key.is_some())?;
    rate_limit::hit(&state, rate_limit::WITHDRAWALS, &caller_id(api_key.as_deref(), staff.as_deref())).await?;
    let requester = match (api_key.as_deref(), staff.as_deref()) {
        (Some(key), _) => key_requester(key),
        (None, Some(staff)) => {
            if staff.role != StaffRole::Admin {
                return Err(ApiError::forbidden("Admin role required"));
            }
            require_any_project(&state).await?;
            require_step_up(&state, &staff.id, body.totp_code.as_deref()).await?;
            staff_requester(staff)
        }
        (None, None) => return Err(ApiError::unauthorized("Not authenticated")),
    };
    let actor = actor(api_key.as_deref(), staff.as_deref(), &client_ip);
    let created = jobs::withdrawals::create_withdrawal(&state, &actor, &requester, &key, &input).await?;
    let status = if created.replayed { StatusCode::OK } else { StatusCode::ACCEPTED };
    Ok((status, Json(withdrawal_json(&created.row)?)))
}

/// `POST /withdrawals/quote`: same validation, nothing reserved.
pub(crate) async fn quote_withdrawal(
    State(state): State<AppState>,
    api_key: Option<Extension<CurrentApiKey>>,
    staff: Option<Extension<CurrentStaff>>,
    Json(body): Json<WithdrawalRequest>,
) -> Result<Json<Value>, ApiError> {
    let input = validate_withdrawal_request(&body, api_key.is_some())?;
    rate_limit::hit(&state, rate_limit::WITHDRAWALS, &caller_id(api_key.as_deref(), staff.as_deref())).await?;
    let requester = match (api_key.as_deref(), staff.as_deref()) {
        (Some(key), _) => key_requester(key),
        (None, Some(staff)) => staff_requester(staff),
        (None, None) => return Err(ApiError::unauthorized("Not authenticated")),
    };
    let quote = jobs::withdrawals::quote_withdrawal(&state, &requester, &input).await?;
    Ok(Json(quote_json(&quote)))
}

#[derive(Debug, Deserialize)]
pub(crate) struct WithdrawalListQuery {
    status: Option<String>,
    network: Option<String>,
    token: Option<String>,
    api_key_id: Option<String>,
    /// Staff: `<uuid>` or `merchant`.
    project_id: Option<String>,
    limit: Option<i64>,
    offset: Option<i64>,
}

/// `GET /withdrawals`: a merchant key sees its project's (spec §8.5) — a
/// legacy key without a project, its own.
pub(crate) async fn list_withdrawals(
    State(state): State<AppState>,
    api_key: Option<Extension<CurrentApiKey>>,
    Query(query): Query<WithdrawalListQuery>,
) -> Result<Json<Value>, ApiError> {
    let filter = WithdrawalFilter {
        status: query
            .status
            .as_deref()
            .map(WithdrawalStatus::parse_api)
            .transpose()
            .map_err(|_| ApiError::bad_request("Invalid status"))?,
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
        api_key_id: match api_key.as_deref() {
            Some(key) if key.project_id.is_none() => Some(key.id.clone()),
            Some(_) => None,
            None => query.api_key_id.clone().filter(|id| !id.is_empty()),
        },
        owner: match api_key.as_deref() {
            Some(key) => crate::merchant_scope(Some(key)),
            None => owner_filter_param(query.project_id.as_deref())?,
        },
    };
    let limit = query.limit.unwrap_or(50).clamp(1, 100);
    let offset = query.offset.unwrap_or(0).max(0);
    let (rows, total) = state.db.list_withdrawals(&filter, limit, offset).await?;
    let data = rows.iter().map(withdrawal_json).collect::<Result<Vec<_>, _>>()?;
    Ok(Json(json!({ "data": data, "total": total, "limit": limit, "offset": offset })))
}

/// Invariant 2: a merchant key reads its project's withdrawals through a
/// query scoped on the project; another project's withdrawal is as absent
/// as a missing one. A legacy key (no project) keeps seeing its own.
async fn find_visible(state: &AppState, api_key: Option<&CurrentApiKey>, id: &str) -> Result<WithdrawalRow, ApiError> {
    let not_found = || ApiError::not_found(format!("Withdrawal not found: {id}"));
    match api_key {
        Some(key) => {
            let filter = WithdrawalFilter {
                api_key_id: key.project_id.is_none().then(|| key.id.clone()),
                owner: crate::merchant_scope(Some(key)),
                ..Default::default()
            };
            state.db.find_withdrawal_visible(id, &filter).await?.ok_or_else(not_found)
        }
        None => state.db.find_withdrawal(id).await?.ok_or_else(not_found),
    }
}

/// `GET /withdrawals/{id}`; staff also get the transaction timeline.
pub(crate) async fn get_withdrawal(
    State(state): State<AppState>,
    api_key: Option<Extension<CurrentApiKey>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let row = find_visible(&state, api_key.as_deref(), &id).await?;
    let mut body = withdrawal_json(&row)?;
    if api_key.is_none() {
        let txs = state.db.outbound_txs_for_withdrawal(&id).await?;
        body["transactions"] = json!(txs.iter().map(outbound_json).collect::<Vec<_>>());
    }
    Ok(Json(body))
}

#[derive(Debug, Deserialize, Default)]
pub(crate) struct StepUpBody {
    totp_code: Option<String>,
}

/// `POST /withdrawals/{id}/cancel`: the requesting key (no body needed),
/// or an ADMIN + TOTP.
pub(crate) async fn cancel_withdrawal(
    State(state): State<AppState>,
    api_key: Option<Extension<CurrentApiKey>>,
    staff: Option<Extension<CurrentStaff>>,
    client_ip: ClientIp,
    Path(id): Path<String>,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    let canceller = match (api_key.as_deref(), staff.as_deref()) {
        (Some(key), _) => Canceller::ApiKey {
            id: key.id.clone(),
            project_id: key.project_id.clone(),
        },
        (None, Some(staff)) => {
            let body: StepUpBody = if body.is_empty() {
                StepUpBody::default()
            } else {
                serde_json::from_slice(&body).map_err(|_| ApiError::bad_request("Invalid JSON body"))?
            };
            require_step_up(&state, &staff.id, body.totp_code.as_deref()).await?;
            Canceller::Admin(staff.id.clone())
        }
        (None, None) => return Err(ApiError::unauthorized("Not authenticated")),
    };
    let actor = actor(api_key.as_deref(), staff.as_deref(), &client_ip);
    let row = jobs::withdrawals::cancel_withdrawal(&state, &actor, &canceller, &id).await?;
    Ok(Json(withdrawal_json(&row)?))
}

/// Admin actions on one withdrawal, each with a fresh TOTP code.
pub(crate) async fn approve_withdrawal(
    State(state): State<AppState>,
    Extension(staff): Extension<CurrentStaff>,
    client_ip: ClientIp,
    Path(id): Path<String>,
    Json(body): Json<StepUpBody>,
) -> Result<Json<Value>, ApiError> {
    require_step_up(&state, &staff.id, body.totp_code.as_deref()).await?;
    let row = jobs::withdrawals::approve_withdrawal(&state, &staff_actor(&staff, &client_ip), &staff.id, &id).await?;
    Ok(Json(withdrawal_json(&row)?))
}

pub(crate) async fn reject_withdrawal(
    State(state): State<AppState>,
    Extension(staff): Extension<CurrentStaff>,
    client_ip: ClientIp,
    Path(id): Path<String>,
    Json(body): Json<StepUpBody>,
) -> Result<Json<Value>, ApiError> {
    require_step_up(&state, &staff.id, body.totp_code.as_deref()).await?;
    let row = jobs::withdrawals::reject_withdrawal(&state, &staff_actor(&staff, &client_ip), &staff.id, &id).await?;
    Ok(Json(withdrawal_json(&row)?))
}

pub(crate) async fn bump_withdrawal(
    State(state): State<AppState>,
    Extension(staff): Extension<CurrentStaff>,
    client_ip: ClientIp,
    Path(id): Path<String>,
    Json(body): Json<StepUpBody>,
) -> Result<Json<Value>, ApiError> {
    require_step_up(&state, &staff.id, body.totp_code.as_deref()).await?;
    let row = jobs::withdrawals::bump_withdrawal(&state, &staff_actor(&staff, &client_ip), &id).await?;
    Ok(Json(withdrawal_json(&row)?))
}

/// Abandon a STUCK withdrawal — only once the network proves none of its
/// transactions can land.
pub(crate) async fn abandon_withdrawal(
    State(state): State<AppState>,
    Extension(staff): Extension<CurrentStaff>,
    client_ip: ClientIp,
    Path(id): Path<String>,
    Json(body): Json<StepUpBody>,
) -> Result<Json<Value>, ApiError> {
    require_step_up(&state, &staff.id, body.totp_code.as_deref()).await?;
    let row = state
        .db
        .find_withdrawal(&id)
        .await?
        .ok_or_else(|| ApiError::not_found(format!("Withdrawal not found: {id}")))?;
    if row.status()? != WithdrawalStatus::Stuck {
        return Err(ApiError::conflict("Only a stuck withdrawal can be abandoned"));
    }
    if !jobs::outbound::abandon(&state, &staff_actor(&staff, &client_ip), &row).await? {
        return Err(ApiError::conflict(
            "The network does not prove yet that this withdrawal can no longer land — bump it, or retry later",
        ));
    }
    let row = state.db.find_withdrawal(&id).await?.ok_or_else(|| ApiError::not_found(format!("Withdrawal not found: {id}")))?;
    Ok(Json(withdrawal_json(&row)?))
}

// ---------------------------------------------------------------- address book

fn entry_json(entry: &AddressBookEntryRow) -> Value {
    let now = Utc::now();
    json!({
        "id": entry.id,
        // The owner: a project, or null for a merchant entry.
        "project_id": entry.project_id,
        "network": Network::from_db(&entry.network).map(|n| n.as_api()).unwrap_or("unknown"),
        "destination_kind": DestinationKind::from_db(&entry.destination_kind).map(|k| k.as_api()).unwrap_or("address"),
        "destination": entry.destination,
        "label": entry.label,
        "is_cold_storage": entry.is_cold_storage,
        "active": entry.is_active(now),
        "active_at": entry.active_at.to_rfc3339(),
        "created_by": entry.created_by_name,
        "created_at": entry.created_at.to_rfc3339(),
    })
}

/// Merchant view: the active entries, for keys allowed to withdraw.
pub(crate) async fn merchant_address_book(
    State(state): State<AppState>,
    Extension(key): Extension<CurrentApiKey>,
) -> Result<Json<Value>, ApiError> {
    if key.withdrawal_policy == domain::WithdrawalPolicy::None {
        return Err(ApiError::forbidden("This API key is not allowed to withdraw"));
    }
    // The key's project's entries (a legacy key without a project sees none
    // of the project entries: only the merchant's, as before projects).
    let owner = match &key.project_id {
        Some(id) => OwnerFilter::Project(id.clone()),
        None => OwnerFilter::Merchant,
    };
    let entries = state.db.list_address_book_entries(None, true, &owner).await?;
    Ok(Json(json!({
        "data": entries.iter().map(|e| json!({
            "id": e.id,
            "network": Network::from_db(&e.network).map(|n| n.as_api()).unwrap_or("unknown"),
            "destination": e.destination,
            "label": e.label,
        })).collect::<Vec<_>>(),
    })))
}

#[derive(Debug, Deserialize)]
pub(crate) struct AddressBookQuery {
    network: Option<String>,
    /// `<uuid>` or `merchant`.
    project_id: Option<String>,
}

pub(crate) async fn list_address_book(
    State(state): State<AppState>,
    Query(query): Query<AddressBookQuery>,
) -> Result<Json<Value>, ApiError> {
    let network = query
        .network
        .as_deref()
        .map(Network::parse_api)
        .transpose()
        .map_err(|_| ApiError::bad_request("Invalid network"))?;
    let owner = owner_filter_param(query.project_id.as_deref())?;
    let entries = state.db.list_address_book_entries(network, false, &owner).await?;
    Ok(Json(json!({ "data": entries.iter().map(entry_json).collect::<Vec<_>>() })))
}

#[derive(Debug, Deserialize)]
pub(crate) struct CreateEntryRequest {
    network: String,
    destination: String,
    label: String,
    #[serde(default)]
    is_cold_storage: bool,
    totp_code: Option<String>,
    /// The owner: a project UUID or `"merchant"` (required).
    project_id: Option<String>,
}

/// New entries are usable after a 24 h cooling-off; adding one raises an
/// `address_book_entry_added` alert.
pub(crate) async fn create_address_book_entry(
    State(state): State<AppState>,
    Extension(staff): Extension<CurrentStaff>,
    client_ip: ClientIp,
    Json(body): Json<CreateEntryRequest>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    require_any_project(&state).await?;
    let owner = parse_owner_field(
        body.project_id
            .as_deref()
            .ok_or_else(|| ApiError::unprocessable("project_id is required: a project UUID or \"merchant\"".into()))?,
    )?;
    if let Owner::Project(id) = &owner {
        if state.db.find_project(id).await?.is_none() {
            return Err(ApiError::unprocessable("Unknown project".into()));
        }
    }
    require_step_up(&state, &staff.id, body.totp_code.as_deref()).await?;
    let network = Network::parse_api(&body.network).map_err(|_| ApiError::bad_request("Invalid network"))?;
    let label = body.label.trim();
    if label.is_empty() || label.chars().count() > ADDRESS_BOOK_LABEL_MAX_CHARS {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            format!("label is required (at most {ADDRESS_BOOK_LABEL_MAX_CHARS} characters)"),
        ));
    }
    let kind = match network {
        Network::Lightning => match chain::destination::lightning_destination_kind(body.destination.trim()) {
            DestinationKind::LnAddress => DestinationKind::LnAddress,
            _ => return Err(ApiError::bad_request("Lightning book entries are Lightning addresses (BOLT11 invoices are single-use)")),
        },
        _ => DestinationKind::Address,
    };
    // Amount rules (dust, invoice amount) are checked per withdrawal.
    let destination = chain::destination::normalize_destination(
        network,
        kind,
        body.destination.trim(),
        u128::from(u64::MAX),
        &jobs::withdrawals::resolve::rules(&state),
    )
    .map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, e.to_string()))?;
    if jobs::withdrawals::resolve::is_gateway_destination(&state, network, &destination).await? {
        return Err(ApiError::bad_request("Destinations never point at the gateway's own addresses"));
    }
    let entry = state
        .db
        .create_address_book_entry(&NewAddressBookEntry {
            network,
            destination_kind: kind,
            destination: &destination,
            label,
            is_cold_storage: body.is_cold_storage,
            created_by_id: &staff.id,
            active_at: Utc::now() + chrono::Duration::seconds(ADDRESS_BOOK_COOLING_OFF_SECS),
            owner: owner.clone(),
        })
        .await?
        .ok_or_else(|| ApiError::conflict("This destination is already in the address book of this owner"))?;
    jobs::audit::record(
        &state,
        &staff_actor(&staff, &client_ip),
        "address_book.created",
        Some(("address_book_entry", &entry.id)),
        json!({ "network": network.as_api(), "destination": destination, "label": label, "is_cold_storage": body.is_cold_storage, "project_id": entry.project_id }),
    )
    .await;
    if let Err(err) = jobs::alerts::raise(
        &state,
        AlertType::AddressBookEntryAdded,
        Some(network),
        None,
        Some(&entry.id),
        json!({ "entry_id": entry.id, "destination": destination, "label": label, "active_at": entry.active_at.to_rfc3339() }),
    )
    .await
    {
        tracing::error!("address-book alert failed: {err:#}");
    }
    Ok((StatusCode::CREATED, Json(entry_json(&entry))))
}

pub(crate) async fn delete_address_book_entry(
    State(state): State<AppState>,
    Extension(staff): Extension<CurrentStaff>,
    client_ip: ClientIp,
    Path(id): Path<String>,
    Json(body): Json<StepUpBody>,
) -> Result<Json<Value>, ApiError> {
    require_step_up(&state, &staff.id, body.totp_code.as_deref()).await?;
    let entry = state
        .db
        .delete_address_book_entry(&id, &staff.id)
        .await?
        .ok_or_else(|| ApiError::not_found(format!("Address-book entry not found: {id}")))?;
    jobs::audit::record(
        &state,
        &staff_actor(&staff, &client_ip),
        "address_book.deleted",
        Some(("address_book_entry", &id)),
        json!({ "destination": entry.destination }),
    )
    .await;
    Ok(Json(json!({ "status": "deleted", "id": id })))
}

// -------------------------------------------------------------- treasury & co

pub(crate) async fn get_treasury(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let rows = jobs::treasury::treasury_view(&state).await?;
    let data: Vec<Value> = rows
        .iter()
        .map(|row| {
            let token = Token::parse_api(row.token).unwrap_or(Token::Usdt);
            let fmt = |raw: &Option<String>| raw.as_deref().and_then(|r| r.parse::<u128>().ok()).map(|r| render(r, token));
            let mut value = serde_json::to_value(row).unwrap_or(Value::Null);
            if let Some(object) = value.as_object_mut() {
                object.insert("confirmed".into(), json!(fmt(&row.confirmed_raw)));
                object.insert("available_amount".into(), json!(fmt(&row.available_raw)));
                object.insert("reserved".into(), json!(fmt(&Some(row.reserved_raw.clone()))));
                object.insert("unswept".into(), json!(fmt(&Some(row.unswept_raw.clone()))));
                object.insert("unattributed".into(), json!(fmt(&Some(row.unattributed_raw.clone()))));
                object.insert("hot_wallet_cap".into(), json!(fmt(&row.hot_wallet_cap_raw)));
                object.insert("gas_floor".into(), json!(fmt(&row.gas_floor_raw)));
                object.insert("gas_alert_below".into(), json!(fmt(&row.gas_alert_below_raw)));
            }
            value
        })
        .collect();
    Ok(Json(json!({
        "data": data,
        "outflows_frozen": jobs::freeze::outflows_frozen(&state).await,
    })))
}

fn sweep_json(sweep: &SweepRow) -> Value {
    let token = Token::from_db(&sweep.token).unwrap_or(Token::Usdt);
    let fee_token = Network::from_db(&sweep.network).map(|n| n.fee_token()).unwrap_or(token);
    json!({
        "id": sweep.id,
        "network": Network::from_db(&sweep.network).map(|n| n.as_api()).unwrap_or("unknown"),
        "token": token.as_api(),
        "deposit_address_id": sweep.deposit_address_id,
        "from_address": sweep.from_address,
        "amount": render(sweep.amount_raw.0, token),
        "amount_raw": sweep.amount_raw.0.to_string(),
        "credited": render(sweep.credited_raw.0, token),
        "unattributed": render(sweep.unattributed_raw.0, token),
        "unattributed_raw": sweep.unattributed_raw.0.to_string(),
        "fee": sweep.fee_raw.map(|f| render(f.0, fee_token)),
        "fee_token": fee_token.as_api(),
        "status": SweepStatus::from_db(&sweep.status).map(|s| s.as_api()).unwrap_or("planned"),
        "last_error": sweep.last_error,
        "created_at": sweep.created_at.to_rfc3339(),
        "updated_at": sweep.updated_at.to_rfc3339(),
    })
}

#[derive(Debug, Deserialize)]
pub(crate) struct SweepListQuery {
    network: Option<String>,
    status: Option<String>,
    limit: Option<i64>,
    offset: Option<i64>,
}

pub(crate) async fn list_sweeps(
    State(state): State<AppState>,
    Query(query): Query<SweepListQuery>,
) -> Result<Json<Value>, ApiError> {
    let network = query
        .network
        .as_deref()
        .map(Network::parse_api)
        .transpose()
        .map_err(|_| ApiError::bad_request("Invalid network"))?;
    let status = query
        .status
        .as_deref()
        .map(SweepStatus::parse_api)
        .transpose()
        .map_err(|_| ApiError::bad_request("Invalid status"))?;
    let limit = query.limit.unwrap_or(50).clamp(1, 100);
    let offset = query.offset.unwrap_or(0).max(0);
    let (rows, total) = state.db.list_sweeps(network, status, limit, offset).await?;
    Ok(Json(json!({
        "data": rows.iter().map(sweep_json).collect::<Vec<_>>(),
        "total": total,
        "limit": limit,
        "offset": offset,
    })))
}

#[derive(Debug, Deserialize)]
pub(crate) struct RunSweepsRequest {
    network: Option<String>,
    totp_code: Option<String>,
}

/// Plan sweeps now (they are then executed by the engine).
pub(crate) async fn run_sweeps(
    State(state): State<AppState>,
    Extension(staff): Extension<CurrentStaff>,
    client_ip: ClientIp,
    Json(body): Json<RunSweepsRequest>,
) -> Result<Json<Value>, ApiError> {
    require_step_up(&state, &staff.id, body.totp_code.as_deref()).await?;
    if jobs::freeze::outflows_frozen(&state).await {
        return Err(ApiError::service_unavailable("Outflows are frozen".into()));
    }
    let networks: Vec<Network> = match body.network.as_deref() {
        Some(network) => vec![Network::parse_api(network).map_err(|_| ApiError::bad_request("Invalid network"))?],
        None => jobs::sweeper::SWEPT_NETWORKS.to_vec(),
    };
    let mut planned = 0;
    for network in &networks {
        if !state.is_network_enabled(*network) {
            continue;
        }
        planned += jobs::sweeper::plan(&state, *network)
            .await
            .map_err(|err| ApiError::internal(format!("{err:#}")))?;
    }
    jobs::audit::record(
        &state,
        &staff_actor(&staff, &client_ip),
        "sweeps.run",
        None,
        json!({ "networks": networks.iter().map(|n| n.as_api()).collect::<Vec<_>>(), "planned": planned }),
    )
    .await;
    Ok(Json(json!({ "planned": planned })))
}

pub(crate) async fn get_outflows(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    Ok(Json(json!({
        "frozen": jobs::freeze::outflows_frozen(&state).await,
        "forced_by_environment": state.config.outflows_frozen,
    })))
}

/// Any staff member may freeze — no TOTP: speed matters more.
pub(crate) async fn freeze_outflows(
    State(state): State<AppState>,
    Extension(staff): Extension<CurrentStaff>,
    client_ip: ClientIp,
) -> Result<Json<Value>, ApiError> {
    jobs::freeze::set_outflows_frozen(&state, &staff_actor(&staff, &client_ip), true)
        .await
        .map_err(freeze_error)?;
    Ok(Json(json!({ "frozen": true })))
}

pub(crate) async fn unfreeze_outflows(
    State(state): State<AppState>,
    Extension(staff): Extension<CurrentStaff>,
    client_ip: ClientIp,
    Json(body): Json<StepUpBody>,
) -> Result<Json<Value>, ApiError> {
    require_step_up(&state, &staff.id, body.totp_code.as_deref()).await?;
    jobs::freeze::set_outflows_frozen(&state, &staff_actor(&staff, &client_ip), false)
        .await
        .map_err(freeze_error)?;
    Ok(Json(json!({ "frozen": false })))
}

fn freeze_error(err: FreezeError) -> ApiError {
    match err {
        FreezeError::ForcedByEnvironment => ApiError::conflict("OUTFLOWS_FROZEN=true forces the freeze: change the environment to lift it"),
        FreezeError::Internal(err) => ApiError::internal(format!("{err:#}")),
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct PageQuery {
    limit: Option<i64>,
    offset: Option<i64>,
}

pub(crate) async fn list_alerts(
    State(state): State<AppState>,
    Query(query): Query<PageQuery>,
) -> Result<Json<Value>, ApiError> {
    let limit = query.limit.unwrap_or(50).clamp(1, 100);
    let offset = query.offset.unwrap_or(0).max(0);
    let (rows, total) = state.db.list_alerts(limit, offset).await?;
    let data: Vec<Value> = rows
        .iter()
        .map(|row| {
            let mut payload = jobs::alerts::alert_payload(row).map(|p| p["data"].clone()).unwrap_or(Value::Null);
            payload["created_at"] = json!(row.created_at.to_rfc3339());
            payload
        })
        .collect();
    Ok(Json(json!({ "data": data, "total": total, "limit": limit, "offset": offset })))
}

/// Legacy Lightning payout requests (read-only history).
pub(crate) async fn list_payout_requests(
    State(state): State<AppState>,
    Query(query): Query<PageQuery>,
) -> Result<Json<Value>, ApiError> {
    let limit = query.limit.unwrap_or(50).clamp(1, 100);
    let offset = query.offset.unwrap_or(0).max(0);
    let (rows, total) = state.db.list_payout_requests(limit, offset).await?;
    let data: Vec<Value> = rows
        .iter()
        .map(|row| {
            json!({
                "id": row.id,
                "ln_address": row.ln_address,
                "amount_sats": row.amount_sats.to_string(),
                "status": row.status.to_lowercase(),
                "payment_id": row.payment_id,
                "requested_by": match (&row.api_key_id, &row.staff_id) {
                    (Some(id), _) => json!({ "type": "api_key", "id": id }),
                    (None, Some(id)) => json!({ "type": "staff", "id": id }),
                    _ => Value::Null,
                },
                "created_at": row.created_at.to_rfc3339(),
            })
        })
        .collect();
    Ok(Json(json!({ "data": data, "total": total, "limit": limit, "offset": offset })))
}

// ---------------------------------------------------------------- asset settings

/// Decimal token amount (`"0"` allowed: disables) → base units.
pub(crate) fn parse_limit(value: &str, token: Token) -> Result<u128, ApiError> {
    let value = value.trim();
    if value == "0" {
        return Ok(0);
    }
    parse_amount_raw(value, token).map_err(|_| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            format!("Invalid {} amount: {value}", token.as_api()),
        )
    })
}

fn asset_json(row: &AssetSettingsRow) -> Value {
    let token = row.token().unwrap_or(Token::Usdt);
    let both = |raw: u128| json!({ "amount": render(raw, token), "raw": raw.to_string() });
    let optional = |raw: Option<db::RawAmount>| raw.map(|r| both(r.0)).unwrap_or(Value::Null);
    json!({
        "token": token.as_api(),
        "max_per_withdrawal": both(row.max_per_withdrawal_raw.0),
        "daily_max": both(row.daily_max_raw.0),
        "approval_threshold": both(row.approval_threshold_raw.0),
        "hot_wallet_cap": optional(row.hot_wallet_cap_raw),
        "gas_reserve_floor": optional(row.gas_reserve_floor_raw),
        "gas_alert_below": optional(row.gas_alert_below_raw),
        "is_gas_asset": Network::ALL.iter().any(|n| n.gas_token() == Some(token) && *n != Network::Bitcoin),
        "updated_at": row.updated_at.to_rfc3339(),
    })
}

async fn assets_json(state: &AppState) -> Result<Value, ApiError> {
    let assets = state.db.list_asset_settings().await?;
    let sweeps = state.db.list_sweep_settings().await?;
    Ok(json!({
        "assets": assets.iter().map(asset_json).collect::<Vec<_>>(),
        "sweeps": sweeps.iter().map(|s| {
            let token = Token::from_db(&s.token).unwrap_or(Token::Usdt);
            json!({
                "network": Network::from_db(&s.network).map(|n| n.as_api()).unwrap_or("unknown"),
                "token": token.as_api(),
                "minimum": render(s.minimum_raw.0, token),
                "minimum_raw": s.minimum_raw.0.to_string(),
                "enabled": s.enabled,
            })
        }).collect::<Vec<_>>(),
    }))
}

pub(crate) async fn get_asset_settings(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    Ok(Json(assets_json(&state).await?))
}

/// One asset's changes (token amounts as decimal strings; `null` clears
/// the optional ones).
#[derive(Debug, Deserialize, Default)]
pub(crate) struct AssetPatch {
    max_per_withdrawal: Option<String>,
    daily_max: Option<String>,
    approval_threshold: Option<String>,
    #[serde(default, deserialize_with = "crate::admin::deserialize_some")]
    hot_wallet_cap: Option<Option<String>>,
    #[serde(default, deserialize_with = "crate::admin::deserialize_some")]
    gas_reserve_floor: Option<Option<String>>,
    #[serde(default, deserialize_with = "crate::admin::deserialize_some")]
    gas_alert_below: Option<Option<String>>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct SweepPatch {
    network: String,
    token: String,
    minimum: Option<String>,
    enabled: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct AssetSettingsRequest {
    #[serde(default)]
    assets: std::collections::BTreeMap<String, AssetPatch>,
    #[serde(default)]
    sweeps: Vec<SweepPatch>,
    totp_code: Option<String>,
}

/// ADMIN + TOTP: limits, thresholds, caps, gas floors, sweep minimums.
pub(crate) async fn update_asset_settings(
    State(state): State<AppState>,
    Extension(staff): Extension<CurrentStaff>,
    client_ip: ClientIp,
    Json(body): Json<AssetSettingsRequest>,
) -> Result<Json<Value>, ApiError> {
    require_step_up(&state, &staff.id, body.totp_code.as_deref()).await?;
    // Validate everything before writing anything.
    let mut updates = Vec::new();
    for (token, patch) in &body.assets {
        let token = Token::parse_api(token).map_err(|_| ApiError::bad_request("Invalid token"))?;
        let opt = |value: &Option<Option<String>>| -> Result<Option<Option<u128>>, ApiError> {
            value
                .as_ref()
                .map(|inner| inner.as_deref().map(|v| parse_limit(v, token)).transpose())
                .transpose()
        };
        let update = AssetSettingsUpdate {
            max_per_withdrawal_raw: patch.max_per_withdrawal.as_deref().map(|v| parse_limit(v, token)).transpose()?,
            daily_max_raw: patch.daily_max.as_deref().map(|v| parse_limit(v, token)).transpose()?,
            approval_threshold_raw: patch.approval_threshold.as_deref().map(|v| parse_limit(v, token)).transpose()?,
            hot_wallet_cap_raw: opt(&patch.hot_wallet_cap)?,
            gas_reserve_floor_raw: opt(&patch.gas_reserve_floor)?,
            gas_alert_below_raw: opt(&patch.gas_alert_below)?,
        };
        updates.push((token, update));
    }
    let mut sweep_updates = Vec::new();
    for patch in &body.sweeps {
        let network = Network::parse_api(&patch.network).map_err(|_| ApiError::bad_request("Invalid network"))?;
        let token = Token::parse_api(&patch.token).map_err(|_| ApiError::bad_request("Invalid token"))?;
        let minimum = patch.minimum.as_deref().map(|v| parse_limit(v, token)).transpose()?;
        sweep_updates.push((network, token, minimum, patch.enabled));
    }
    let before = assets_json(&state).await?;
    for (token, update) in &updates {
        state.db.update_asset_settings(*token, update).await?;
    }
    for (network, token, minimum, enabled) in sweep_updates {
        if state.db.update_sweep_setting(network, token, minimum, enabled).await?.is_none() {
            return Err(ApiError::bad_request("This network and token are never swept"));
        }
    }
    let after = assets_json(&state).await?;
    jobs::audit::record(
        &state,
        &staff_actor(&staff, &client_ip),
        "settings.assets_updated",
        None,
        json!({ "before": before, "after": after }),
    )
    .await;
    Ok(Json(after))
}

// -------------------------------------------------------------- payout alias

/// Old payout body.
#[derive(Debug, Deserialize)]
pub(crate) struct PayoutRequest {
    ln_address: String,
    amount_sats: String,
    comment: Option<String>,
    totp_code: Option<String>,
}

fn deprecation_headers() -> [(HeaderName, HeaderValue); 2] {
    [
        (HeaderName::from_static("deprecation"), HeaderValue::from_static("true")),
        (HeaderName::from_static("sunset"), HeaderValue::from_static(jobs::PAYOUT_ALIAS_SUNSET)),
    ]
}

fn payout_input(body: &PayoutRequest) -> Result<WithdrawalInput, ApiError> {
    let ln_address = body.ln_address.trim();
    if !chain::destination::is_lightning_address(ln_address) {
        return Err(ApiError::bad_request("ln_address must be a Lightning address (user@domain)"));
    }
    let amount_sats = parse_sats(&body.amount_sats).map_err(|_| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            format!("Invalid amount_sats (integer string, 1 to {})", domain::BTC_MAX_SATS),
        )
    })?;
    Ok(jobs::legacy_payout_input(
        ln_address,
        amount_sats,
        body.comment.clone().filter(|c| !c.trim().is_empty()),
    ))
}

/// Old payout response shape, plus `withdrawal_id`. `id` is the wallet
/// payment id the send gets (the withdrawal id).
fn payout_json(row: &WithdrawalRow) -> Value {
    json!({
        "id": row.id,
        "withdrawal_id": row.id,
        "direction": "send",
        "status": match row.status().unwrap_or(WithdrawalStatus::Queued) {
            WithdrawalStatus::Confirmed => "completed",
            WithdrawalStatus::Failed | WithdrawalStatus::Cancelled => "failed",
            _ => "pending",
        },
        "amount": render(row.amount_raw.0, Token::Btc),
        "amount_sats": row.amount_raw.0.to_string(),
        "fees_sats": row.fee_raw.or(row.fee_estimate_raw).map(|f| f.0.to_string()).unwrap_or_else(|| "0".into()),
        "method": "lightning",
        "invoice": null,
        "description": null,
        "tx_id": null,
        "ln_address": row.destination,
        "comment": row.comment,
        "created_at": row.created_at.to_rfc3339(),
        "deposit_address_id": null,
    })
}

/// Deprecated alias of `POST /withdrawals` for Lightning addresses.
pub(crate) async fn create_lightning_payout(
    State(state): State<AppState>,
    api_key: Option<Extension<CurrentApiKey>>,
    staff: Option<Extension<CurrentStaff>>,
    client_ip: ClientIp,
    headers: HeaderMap,
    Json(body): Json<PayoutRequest>,
) -> Result<Response, ApiError> {
    let key = idempotency_key(&headers)?;
    let input = payout_input(&body)?;
    rate_limit::hit(&state, rate_limit::WITHDRAWALS, &caller_id(api_key.as_deref(), staff.as_deref())).await?;
    let requester = match (api_key.as_deref(), staff.as_deref()) {
        (Some(key), _) => key_requester(key),
        (None, Some(staff)) => {
            if staff.role != StaffRole::Admin {
                return Err(ApiError::forbidden("Admin role required"));
            }
            require_step_up(&state, &staff.id, body.totp_code.as_deref()).await?;
            staff_requester(staff)
        }
        (None, None) => return Err(ApiError::unauthorized("Not authenticated")),
    };
    // A retry of a payout made before the upgrade (Kamal overlap, client
    // retry): its SDK key was this Idempotency-Key — never a second send.
    if let Some(legacy) = state.db.find_payout_request_by_key(&key).await? {
        return legacy_payout_replay(&state, &legacy, &requester, &body).await;
    }
    let actor = actor(api_key.as_deref(), staff.as_deref(), &client_ip);
    let created = jobs::withdrawals::create_withdrawal(&state, &actor, &requester, &key, &input).await?;
    let status = if created.replayed { StatusCode::OK } else { StatusCode::ACCEPTED };
    Ok((status, deprecation_headers(), Json(payout_json(&created.row))).into_response())
}

/// Answer a key of the former payout flow from the wallet, like that flow
/// did: same caller and parameters only; nothing is ever sent again.
async fn legacy_payout_replay(
    state: &AppState,
    legacy: &db::PayoutRequestRow,
    requester: &jobs::withdrawals::rules::Requester,
    body: &PayoutRequest,
) -> Result<Response, ApiError> {
    let same_caller = legacy.api_key_id.as_deref() == requester.api_key_id() && legacy.staff_id.as_deref() == requester.staff_id();
    let same_params = legacy.ln_address.eq_ignore_ascii_case(body.ln_address.trim())
        && legacy.amount_sats.to_string() == body.amount_sats.trim();
    if !same_caller || !same_params {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "Idempotency-Key already used with other parameters",
        ));
    }
    let payment_id = legacy.payment_id.as_deref().unwrap_or(&legacy.idempotency_key);
    match crate::lightning_payment_entry(state, payment_id).await {
        Ok(Some(entry)) if entry.payment.direction == lightning_client::PaymentDirection::Send => {
            Ok((StatusCode::OK, deprecation_headers(), Json(crate::lightning_history_json(&entry))).into_response())
        }
        Ok(_) => Err(ApiError::new(
            StatusCode::CONFLICT,
            "This Idempotency-Key belongs to a payout from before the upgrade that the wallet does not hold (yet): retry later, or use a new key",
        )),
        Err(jobs::LightningError::Disabled) => Err(ApiError::service_unavailable("Lightning is not configured".into())),
        Err(jobs::LightningError::Failed(err)) => Err(ApiError::internal(format!("legacy payout lookup failed: {err:#}"))),
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct PayoutQuoteRequest {
    ln_address: String,
    amount_sats: String,
    comment: Option<String>,
}

pub(crate) async fn quote_lightning_payout(
    State(state): State<AppState>,
    api_key: Option<Extension<CurrentApiKey>>,
    staff: Option<Extension<CurrentStaff>>,
    Json(body): Json<PayoutQuoteRequest>,
) -> Result<Response, ApiError> {
    let input = payout_input(&PayoutRequest {
        ln_address: body.ln_address,
        amount_sats: body.amount_sats,
        comment: body.comment,
        totp_code: None,
    })?;
    rate_limit::hit(&state, rate_limit::WITHDRAWALS, &caller_id(api_key.as_deref(), staff.as_deref())).await?;
    let requester = match (api_key.as_deref(), staff.as_deref()) {
        (Some(key), _) => key_requester(key),
        (None, Some(staff)) => staff_requester(staff),
        (None, None) => return Err(ApiError::unauthorized("Not authenticated")),
    };
    let quote = jobs::withdrawals::quote_withdrawal(&state, &requester, &input).await?;
    let fees = quote.fee_estimate_raw;
    Ok((
        deprecation_headers(),
        Json(json!({
            "ln_address": quote.destination,
            "amount_sats": quote.amount_raw.to_string(),
            "fees_sats": fees.to_string(),
            "total_sats": quote.amount_raw.saturating_add(fees).to_string(),
        })),
    )
        .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(body: Value) -> WithdrawalRequest {
        serde_json::from_value(body).expect("valid shape")
    }

    /// Staff caller (a merchant key may not name an owner, see below).
    fn validate_withdrawal_request(body: &WithdrawalRequest) -> Result<WithdrawalInput, ApiError> {
        super::validate_withdrawal_request(body, false)
    }

    #[test]
    fn owners_come_from_staff_bodies_only() {
        let body = request(json!({
            "network": "tron", "token": "USDT", "amount": "1", "destination": "T",
            "project_id": "6f1c7a8e-2f43-4d55-9a8b-1c2d3e4f5a6b"
        }));
        assert_eq!(
            validate_withdrawal_request(&body).unwrap().owner,
            Some(Owner::Project("6f1c7a8e-2f43-4d55-9a8b-1c2d3e4f5a6b".into()))
        );
        let merchant = request(json!({
            "network": "tron", "token": "USDT", "amount": "1", "destination": "T", "project_id": "merchant"
        }));
        assert_eq!(validate_withdrawal_request(&merchant).unwrap().owner, Some(Owner::Merchant));
        // A merchant key's withdrawals belong to its project: no choice.
        assert_eq!(super::validate_withdrawal_request(&body, true).unwrap_err().status, StatusCode::UNPROCESSABLE_ENTITY);
        let none = request(json!({ "network": "tron", "token": "USDT", "amount": "1", "destination": "T" }));
        assert_eq!(super::validate_withdrawal_request(&none, true).unwrap().owner, None);
        let bad = request(json!({ "network": "tron", "token": "USDT", "amount": "1", "destination": "T", "project_id": "shop" }));
        assert_eq!(validate_withdrawal_request(&bad).unwrap_err().status, StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[test]
    fn amounts_follow_the_network() {
        let usdt = validate_withdrawal_request(&request(json!({
            "network": "tron", "token": "USDT", "amount": "25.5", "destination": "TR7NHqjeKQxGTCi8q8ZY4pL8otSzgjLj6t"
        })))
        .unwrap();
        assert_eq!(usdt.amount_raw, 25_500_000);
        let eth = validate_withdrawal_request(&request(json!({
            "network": "ethereum", "token": "ETH", "amount": "0.05", "destination": "0x5aAeb6053F3E94C9b9A09f33669435E7Ef1BeAed"
        })))
        .unwrap();
        assert_eq!(eth.amount_raw, 50_000_000_000_000_000);
        let sats = validate_withdrawal_request(&request(json!({
            "network": "lightning", "token": "BTC", "amount_sats": "150000", "destination": "alice@example.com", "comment": "hi"
        })))
        .unwrap();
        assert_eq!(sats.amount_raw, 150_000);
        assert_eq!(sats.comment.as_deref(), Some("hi"));
        // BTC networks also take a decimal BTC amount.
        let btc = validate_withdrawal_request(&request(json!({
            "network": "bitcoin", "token": "BTC", "amount": "0.0015", "destination": "bc1q"
        })))
        .unwrap();
        assert_eq!(btc.amount_raw, 150_000);
        for bad in [
            json!({ "network": "tron", "token": "USDT", "amount_sats": "1", "destination": "x" }),
            json!({ "network": "tron", "token": "USDT", "amount": "0", "destination": "x" }),
            json!({ "network": "tron", "token": "USDT", "amount": "1.0000001", "destination": "x" }),
            json!({ "network": "tron", "token": "USDT", "destination": "x" }),
            json!({ "network": "bitcoin", "token": "BTC", "amount": "1", "amount_sats": "1", "destination": "x" }),
        ] {
            assert_eq!(validate_withdrawal_request(&request(bad.clone())).unwrap_err().status, StatusCode::BAD_REQUEST, "{bad}");
        }
    }

    #[test]
    fn exactly_one_destination_and_bounded_extras() {
        let base = |extra: Value| {
            let mut body = json!({ "network": "lightning", "token": "BTC", "amount_sats": "1000" });
            body.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
            validate_withdrawal_request(&request(body))
        };
        assert!(base(json!({})).is_err());
        assert!(base(json!({ "destination": "a@b.co", "address_book_entry_id": "e1" })).is_err());
        assert!(matches!(
            base(json!({ "address_book_entry_id": "e1" })).unwrap().destination,
            DestinationInput::BookEntry(_)
        ));
        assert!(base(json!({ "destination": "a@b.co", "reference": "r".repeat(129) })).is_err());
        assert!(base(json!({ "destination": "a@b.co", "metadata": [1] })).is_err());
        assert!(base(json!({ "destination": "a@b.co", "metadata": { "k": "v".repeat(5000) } })).is_err());
        let not_lightning = validate_withdrawal_request(&request(json!({
            "network": "tron", "token": "USDT", "amount": "1", "destination": "T", "comment": "x"
        })));
        assert!(not_lightning.is_err());
    }

    #[test]
    fn gas_assets_only_on_their_network() {
        assert!(validate_withdrawal_request(&request(json!({
            "network": "tron", "token": "TRX", "amount": "10", "destination": "T"
        })))
        .is_ok());
        let err = validate_withdrawal_request(&request(json!({
            "network": "ethereum", "token": "TRX", "amount": "10", "destination": "0x"
        })))
        .unwrap_err();
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn idempotency_keys_are_canonical_uuids() {
        let mut headers = HeaderMap::new();
        assert!(idempotency_key(&headers).is_err());
        headers.insert("idempotency-key", HeaderValue::from_static("6F1C7A8E-2F43-4D55-9A8B-1C2D3E4F5A6B"));
        assert_eq!(idempotency_key(&headers).unwrap(), "6f1c7a8e-2f43-4d55-9a8b-1c2d3e4f5a6b");
        headers.insert("idempotency-key", HeaderValue::from_static("order-1"));
        assert!(idempotency_key(&headers).is_err());
    }

    #[test]
    fn withdrawal_errors_map_to_client_safe_statuses() {
        let status = |err: WithdrawalError| ApiError::from(err).status;
        assert_eq!(status(WithdrawalError::Rule(RuleError::Forbidden("x".into()))), StatusCode::FORBIDDEN);
        assert_eq!(
            status(WithdrawalError::Rule(RuleError::Refused { code: "insufficient_funds", message: "x".into() })),
            StatusCode::UNPROCESSABLE_ENTITY
        );
        assert_eq!(status(WithdrawalError::KeyReused), StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(status(WithdrawalError::Frozen), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(status(WithdrawalError::Conflict("x".into())), StatusCode::CONFLICT);
        let provider = ApiError::from(WithdrawalError::Provider(anyhow::anyhow!("https://key@provider/x")));
        assert_eq!(provider.status, StatusCode::BAD_GATEWAY);
        assert!(!provider.message.contains("provider/x"));
        let refused = ApiError::from(WithdrawalError::Rule(RuleError::Refused { code: "limit_exceeded", message: "Too much".into() }));
        assert_eq!(refused.message, "limit_exceeded: Too much");
    }

    #[test]
    fn limits_accept_zero_and_token_decimals() {
        assert_eq!(parse_limit("0", Token::Btc).unwrap(), 0);
        assert_eq!(parse_limit("0.01", Token::Btc).unwrap(), 1_000_000);
        assert_eq!(parse_limit("5", Token::Eth).unwrap(), 5 * 10u128.pow(18));
        assert!(parse_limit("-1", Token::Usdt).is_err());
        assert!(parse_limit("0.0000001", Token::Usdt).is_err());
    }
}
