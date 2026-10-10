use std::collections::HashMap;
use std::time::Duration as StdDuration;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use chrono::{DateTime, Duration, Utc};
use db::{DepositAddressRow, WebhookDeliveryRow};
use domain::{
    decimal_to_raw, format_amount, parse_sats, raw_to_decimal, raw_to_decimal_string,
    round_to_token, supports,
    DepositAddressStatus, Network, Token, WebhookDeliveryStatus, WebhookEventType, BTC_MAX_SATS,
};
use jobs::{
    expire_addresses_and_notify, get_cached_wallet_balances, get_live_balances,
    lightning_payment_entry, lightning_payment_history, notify_address_event, retrigger_webhook,
    send_payout, AppState, LightningError, LightningHistoryEntry, RetriggerError,
};
use middleware::{CurrentApiKey, CurrentStaff};
use rust_decimal::Decimal;
use axum::http::HeaderValue;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tower_http::cors::{AllowOrigin, Any, CorsLayer};
use tower_http::trace::TraceLayer;

mod admin;
mod middleware;
mod traffic;

const READINESS_TIMEOUT: StdDuration = StdDuration::from_secs(2);
/// BOLT11 memo cap for `description` (characters).
const INVOICE_DESCRIPTION_MAX_CHARS: usize = 128;

pub fn router(state: AppState) -> Router {
    let api_v1 = Router::new()
        .route(
            "/deposit-addresses",
            post(create_deposit_address).get(list_deposit_addresses),
        )
        .route("/deposit-addresses/{id}", get(get_deposit_address))
        .route(
            "/deposits/{deposit_id}/webhooks/retry",
            post(retry_deposit_webhook),
        )
        .route("/balances", get(get_balances))
        .route("/wallet-balances", get(get_wallet_balances))
        .route(
            "/lightning/payments",
            get(list_lightning_payments).post(create_lightning_payout),
        )
        .route("/lightning/payments/{id}", get(get_lightning_payment))
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            middleware::require_api_key,
        ));

    let mut app = Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .nest("/api/v1", api_v1);

    if state.config.jwt_secret.is_some() {
        app = app.nest("/api/admin/v1", admin::admin_router(&state));
    } else {
        tracing::warn!(
            "JWT_SECRET is not set (min 32 chars) — /api/admin/v1 and the dashboard are disabled"
        );
    }

    // Traffic log goes on after both `nest`s and outside the auth guards
    // (which are `route_layer`s inside the nested routers): it then sees the
    // matched route template, the final status of rejected requests, and the
    // identity the guards copy into the response extensions. `.layer` rather
    // than `.route_layer` so `/api/...` 404s are counted too; non-API paths
    // are filtered out by `ApiSurface::from_path`.
    if state.config.api_traffic_enabled {
        app = app.layer(axum::middleware::from_fn_with_state(
            traffic::TrafficContext::new(
                state.traffic.clone(),
                state.config.trusted_proxies.clone(),
            ),
            traffic::record_traffic,
        ));
    }

    let app = app.layer(TraceLayer::new_for_http());
    let app = match state.config.cors_allowed_origins.as_deref() {
        Some(origins) => app.layer(cors_layer(origins)),
        None => app,
    };
    app.with_state(state)
}

/// Optional safety net for setups where the dashboard is not served behind
/// the same origin (dev proxy / nginx cover the normal cases).
fn cors_layer(origins: &str) -> CorsLayer {
    let list: Vec<HeaderValue> = origins
        .split(',')
        .filter_map(|origin| origin.trim().parse().ok())
        .collect();
    CorsLayer::new()
        .allow_origin(AllowOrigin::list(list))
        .allow_methods(Any)
        .allow_headers(Any)
}

async fn healthz() -> Json<Value> {
    Json(json!({ "status": "OK" }))
}

async fn readyz(State(state): State<AppState>) -> impl IntoResponse {
    let (postgres, redis, lightning) = tokio::join!(
        check_postgres(&state),
        check_redis(&state),
        check_lightning(&state)
    );

    let ready = postgres == "ok" && redis == "ok";
    let status = if ready {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };

    (
        status,
        Json(json!({
            "status": if ready { "ready" } else { "not_ready" },
            "checks": {
                "postgres": postgres,
                "redis": redis,
                // Informational: Lightning being down must not take the
                // other networks out of rotation.
                "lightning": lightning,
            }
        })),
    )
}

async fn check_postgres(state: &AppState) -> String {
    match tokio::time::timeout(READINESS_TIMEOUT, state.db.ping()).await {
        Ok(Ok(())) => "ok".into(),
        Ok(Err(e)) => format!("error: {e:#}"),
        Err(_) => "timeout".into(),
    }
}

async fn check_lightning(state: &AppState) -> &'static str {
    let Some(lightning) = &state.lightning else {
        return "disabled";
    };
    match tokio::time::timeout(READINESS_TIMEOUT, lightning.balance_sats(false)).await {
        Ok(Ok(_)) => "ok",
        Ok(Err(err)) => {
            // SDK detail stays in the logs — /readyz is public.
            tracing::warn!("Lightning readiness check failed: {err:#}");
            "error"
        }
        Err(_) => "timeout",
    }
}

async fn check_redis(state: &AppState) -> String {
    let mut redis = state.redis.clone();
    match tokio::time::timeout(
        READINESS_TIMEOUT,
        redis::cmd("PING").query_async::<String>(&mut redis),
    )
    .await
    {
        Ok(Ok(_)) => "ok".into(),
        Ok(Err(e)) => format!("error: {e}"),
        Err(_) => "timeout".into(),
    }
}

async fn create_deposit_address(
    State(state): State<AppState>,
    Json(body): Json<CreateDepositAddressRequest>,
) -> Result<(StatusCode, Json<DepositAddressResponse>), ApiError> {
    let request = validate_create_request(&body)?;
    let network = request.network;

    let expires_at = if let Some(exp) = body.expires_at {
        chrono::DateTime::parse_from_rfc3339(&exp)
            .map(|d| d.with_timezone(&Utc))
            .map_err(|_| ApiError::bad_request("Invalid expires_at"))?
    } else {
        let expiry_minutes = state.db.deposit_expiry_minutes().await?;
        Utc::now() + Duration::minutes(expiry_minutes)
    };

    // Lightning rows carry no key material; the index is allocated per
    // network all the same, for uniformity.
    let derivation_index = state.db.next_derivation_index(network).await?;
    let address = match network {
        Network::Lightning => {
            let Some(lightning) = &state.lightning else {
                return Err(ApiError::service_unavailable(
                    "Lightning is not configured".into(),
                ));
            };
            let expiry_secs = invoice_expiry_secs(expires_at, Utc::now())?;
            let sats = request
                .expected_sats
                .ok_or_else(|| ApiError::internal("missing sats amount".into()))?;
            let invoice = lightning
                .create_invoice(
                    sats,
                    request.description.as_deref().unwrap_or_default(),
                    expiry_secs,
                )
                .await
                .map_err(|err| {
                    // The SDK detail stays server-side.
                    tracing::error!("Lightning invoice creation failed: {err:#}");
                    ApiError::internal("Failed to create invoice".into())
                })?;
            invoice.payment_request
        }
        _ => state
            .wallet
            .derive_address(network, derivation_index as u32)
            .map_err(|e| ApiError::internal(e.to_string()))?,
    };

    let metadata = body.metadata.unwrap_or_else(|| json!({}));
    let row = state
        .db
        .create_deposit_address(
            network,
            request.token,
            &address,
            derivation_index,
            request.expected_amount,
            expires_at,
            metadata,
        )
        .await?;

    // The address is created PENDING — fan out the PENDING event to the
    // subscribed endpoints. A queueing failure must not fail the creation.
    if let Err(err) = notify_address_event(&state, &row.id, WebhookEventType::Pending).await {
        tracing::warn!("Failed to queue PENDING webhook for address {}: {err:#}", row.id);
    }

    Ok((StatusCode::CREATED, Json(to_response(&row)?)))
}

/// A create request that passed validation.
#[derive(Debug)]
struct ValidCreateRequest {
    network: Network,
    token: Token,
    /// In token units (BTC for bitcoin/lightning), rounded to its decimals.
    expected_amount: Decimal,
    /// Set on BTC networks only.
    expected_sats: Option<u64>,
    /// Lightning only: the BOLT11 memo.
    description: Option<String>,
}

/// BTC networks take an integer `expected_amount_sats` and refuse
/// `expected_amount` (so `1500` is never ambiguous between sats and BTC);
/// USDT networks keep the decimal `expected_amount`.
fn validate_create_request(
    body: &CreateDepositAddressRequest,
) -> Result<ValidCreateRequest, ApiError> {
    let network = Network::parse_api(&body.network).map_err(ApiError::from)?;
    let token = Token::parse_api(&body.token).map_err(ApiError::from)?;
    if !supports(network, token) {
        return Err(ApiError::bad_request("Token not supported on this network"));
    }

    let (expected_amount, expected_sats) = match token {
        Token::Btc => {
            if body.expected_amount.is_some() {
                return Err(ApiError::bad_request(
                    "expected_amount is not accepted on bitcoin/lightning — use expected_amount_sats",
                ));
            }
            let Some(raw) = body.expected_amount_sats.as_deref() else {
                return Err(ApiError::bad_request(
                    "expected_amount_sats is required on bitcoin/lightning",
                ));
            };
            let sats = parse_sats(raw).map_err(|_| ApiError {
                status: StatusCode::BAD_REQUEST,
                message: format!(
                    "Invalid expected_amount_sats (integer string, 1 to {BTC_MAX_SATS})"
                ),
            })?;
            let amount = raw_to_decimal(&sats.to_string(), token).map_err(ApiError::from)?;
            (amount, Some(sats))
        }
        Token::Usdt => {
            if body.expected_amount_sats.is_some() {
                return Err(ApiError::bad_request(
                    "expected_amount_sats is only accepted on bitcoin/lightning",
                ));
            }
            let amount: Decimal = body
                .expected_amount
                .as_deref()
                .and_then(|v| v.parse().ok())
                .ok_or_else(|| ApiError::bad_request("Invalid expected_amount"))?;
            (round_to_token(amount, token), None)
        }
    };

    if let Some(description) = &body.description {
        if network != Network::Lightning {
            return Err(ApiError::bad_request("description is only accepted on lightning"));
        }
        if description.chars().count() > INVOICE_DESCRIPTION_MAX_CHARS {
            return Err(ApiError {
                status: StatusCode::BAD_REQUEST,
                message: format!(
                    "description must be at most {INVOICE_DESCRIPTION_MAX_CHARS} characters"
                ),
            });
        }
    }

    Ok(ValidCreateRequest {
        network,
        token,
        expected_amount,
        expected_sats,
        description: body.description.clone(),
    })
}

/// BOLT11 expiry matching the gateway expiry, so both lapse together.
fn invoice_expiry_secs(expires_at: DateTime<Utc>, now: DateTime<Utc>) -> Result<u32, ApiError> {
    let secs = (expires_at - now).num_seconds();
    if secs <= 0 {
        return Err(ApiError::bad_request("expires_at must be in the future"));
    }
    Ok(u32::try_from(secs).unwrap_or(u32::MAX))
}

async fn list_deposit_addresses(
    State(state): State<AppState>,
    Query(query): Query<ListQuery>,
) -> Result<Json<ListResponse>, ApiError> {
    let _ = expire_addresses_and_notify(&state).await?;

    let status = query
        .status
        .as_deref()
        .map(DepositAddressStatus::parse_api)
        .transpose()
        .map_err(ApiError::from)?;
    let network = query
        .network
        .as_deref()
        .map(Network::parse_api)
        .transpose()
        .map_err(ApiError::from)?;

    let limit = query.limit.unwrap_or(50).clamp(1, 100);
    let offset = query.offset.unwrap_or(0).max(0);

    let (rows, total) = state
        .db
        .list_deposit_addresses(status, network, limit, offset)
        .await?;
    let data = rows
        .iter()
        .map(to_response)
        .collect::<Result<Vec<_>, _>>()?;

    Ok(Json(ListResponse {
        data,
        total,
        limit,
        offset,
    }))
}

async fn get_deposit_address(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let Some(row) = state.db.find_deposit_address(&id).await? else {
        return Err(ApiError::not_found(format!(
            "Deposit address not found: {id}"
        )));
    };
    let deposits = state.db.list_deposits_for_address(&id).await?;
    let deposit_ids: Vec<String> = deposits.iter().map(|d| d.id.clone()).collect();
    // Rows come back newest-first; keep the first one per (deposit, endpoint)
    // pair (the authoritative delivery, same convention as the jobs crate).
    let mut deliveries: HashMap<(String, Option<String>), WebhookDeliveryRow> = HashMap::new();
    for delivery in state
        .db
        .find_webhook_deliveries_for_deposits(&deposit_ids)
        .await?
    {
        // Rows fetched by deposit id always carry one; keep the guard for
        // type honesty (address-scoped deliveries have no deposit).
        let Some(deposit_id) = delivery.deposit_id.clone() else {
            continue;
        };
        deliveries
            .entry((deposit_id, delivery.webhook_endpoint_id.clone()))
            .or_insert(delivery);
    }
    let endpoint_urls: HashMap<String, String> = state
        .db
        .list_webhook_endpoints()
        .await?
        .into_iter()
        .map(|e| (e.id, e.url))
        .collect();

    let token = Token::from_db(&row.token).map_err(ApiError::from)?;
    let mut base = serde_json::to_value(to_response(&row)?)?;
    if let Some(obj) = base.as_object_mut() {
        obj.insert("metadata".into(), row.metadata);
        obj.insert(
            "deposits".into(),
            json!(deposits
                .into_iter()
                .map(|d| {
                    let mut entries: Vec<&WebhookDeliveryRow> = deliveries
                        .iter()
                        .filter(|((deposit_id, _), _)| deposit_id == &d.id)
                        .map(|(_, delivery)| delivery)
                        .collect();
                    entries.sort_by_key(|delivery| std::cmp::Reverse(delivery.updated_at));

                    // `webhook` keeps the pre-multi-endpoint shape (most
                    // recently updated delivery, legacy rows included).
                    let webhook = entries
                        .first()
                        .map(|delivery| webhook_to_json(delivery))
                        .unwrap_or(Value::Null);
                    let webhooks: Vec<Value> = entries
                        .iter()
                        .filter_map(|delivery| {
                            let endpoint_id = delivery.webhook_endpoint_id.as_ref()?;
                            let mut entry = webhook_to_json(delivery);
                            if let Some(entry_obj) = entry.as_object_mut() {
                                entry_obj.insert("endpoint_id".into(), json!(endpoint_id));
                                entry_obj.insert(
                                    "endpoint_url".into(),
                                    json!(endpoint_urls.get(endpoint_id)),
                                );
                            }
                            Some(entry)
                        })
                        .collect();

                    json!({
                        "id": d.id,
                        "tx_hash": d.tx_hash,
                        "amount": format_amount(d.amount, token),
                        "amount_raw": d.amount_raw,
                        "confirmations": d.confirmations,
                        "status": domain::DepositStatus::from_db(&d.status).map(|s| s.as_api()).unwrap_or("detected"),
                        "confirmed_at": d.confirmed_at.map(|t| t.to_rfc3339()),
                        "webhook": webhook,
                        "webhooks": webhooks,
                    })
                })
                .collect::<Vec<_>>()),
        );
    }
    Ok(Json(base))
}

fn webhook_to_json(delivery: &WebhookDeliveryRow) -> Value {
    json!({
        "status": WebhookDeliveryStatus::from_db(&delivery.status).map(|s| s.as_api()).unwrap_or("pending"),
        "attempts": delivery.attempts,
        "last_response": delivery.last_response,
        "last_error": delivery.last_error,
        "updated_at": delivery.updated_at.to_rfc3339(),
    })
}

async fn retry_deposit_webhook(
    State(state): State<AppState>,
    Path(deposit_id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    match retrigger_webhook(&state, &deposit_id).await {
        Ok(()) => Ok(Json(json!({
            "status": "queued",
            "deposit_id": deposit_id,
        }))),
        Err(RetriggerError::DepositNotFound) => Err(ApiError::not_found(format!(
            "Deposit not found: {deposit_id}"
        ))),
        Err(RetriggerError::DepositNotConfirmed) => {
            Err(ApiError::bad_request("Deposit is not confirmed"))
        }
        Err(RetriggerError::NoActiveEndpoints) => Err(ApiError::service_unavailable(
            "No active webhook endpoints configured".into(),
        )),
        Err(RetriggerError::Other(err)) => Err(ApiError::internal(err.to_string())),
        Err(err) => Err(ApiError::internal(format!("{err:?}"))),
    }
}

async fn get_balances(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let resp = get_live_balances(&state).await?;
    Ok(Json(serde_json::to_value(resp)?))
}

/// The whole Lightning wallet history (every direction and status, funds
/// unrelated to gateway invoices included), newest first. Paged by
/// `has_more`: the SDK reports no total.
async fn list_lightning_payments(
    State(state): State<AppState>,
    Query(query): Query<PageQuery>,
) -> Result<Json<Value>, ApiError> {
    let (limit, offset) = history_paging(&query);
    match lightning_payment_history(&state, offset, limit).await {
        Ok(Some(page)) => Ok(Json(json!({
            "data": page.entries.iter().map(lightning_history_json).collect::<Vec<_>>(),
            "limit": limit,
            "offset": offset,
            "has_more": page.has_more,
        }))),
        Ok(None) => Err(ApiError::service_unavailable(
            "Lightning is not configured".into(),
        )),
        Err(err) => {
            // The SDK detail stays server-side.
            tracing::error!("Lightning history listing failed: {err:#}");
            Err(ApiError::internal("Failed to list Lightning payments".into()))
        }
    }
}

/// Longest Lightning address accepted (local part + `@` + domain).
const LN_ADDRESS_MAX_LEN: usize = 320;

#[derive(Debug, Deserialize)]
struct PayoutRequest {
    /// `user@domain` — the only payout destination (no LNURL strings or
    /// invoices).
    ln_address: String,
    /// Integer string, sats; fees come on top.
    amount_sats: String,
    /// Sent to the recipient; its maximum length is the recipient's.
    comment: Option<String>,
}

#[derive(Debug)]
struct ValidPayout {
    ln_address: String,
    amount_sats: u64,
    comment: Option<String>,
    /// Canonical (lowercase, hyphenated) UUID, as the SDK requires.
    idempotency_key: Option<String>,
}

fn validate_payout_request(
    body: &PayoutRequest,
    idempotency_key: Option<&str>,
) -> Result<ValidPayout, ApiError> {
    let ln_address = body.ln_address.trim();
    if !is_lightning_address(ln_address) {
        return Err(ApiError::bad_request(
            "ln_address must be a Lightning address (user@domain)",
        ));
    }
    let amount_sats = parse_sats(&body.amount_sats).map_err(|_| ApiError {
        status: StatusCode::BAD_REQUEST,
        message: format!("Invalid amount_sats (integer string, 1 to {BTC_MAX_SATS})"),
    })?;
    let idempotency_key = idempotency_key
        .map(|key| {
            uuid::Uuid::parse_str(key.trim())
                .map(|uuid| uuid.hyphenated().to_string())
                .map_err(|_| ApiError::bad_request("Idempotency-Key must be a UUID"))
        })
        .transpose()?;
    Ok(ValidPayout {
        ln_address: ln_address.to_string(),
        amount_sats,
        comment: body.comment.clone().filter(|c| !c.trim().is_empty()),
        idempotency_key,
    })
}

/// `user@domain` (LUD-16): one `@`, a local part of letters, digits and
/// `-_.+`, a dotted DNS name. Everything else the SDK could parse (LNURL,
/// invoices, on-chain addresses) is refused before reaching it.
fn is_lightning_address(value: &str) -> bool {
    if value.len() > LN_ADDRESS_MAX_LEN {
        return false;
    }
    let Some((user, domain)) = value.split_once('@') else {
        return false;
    };
    let user_ok = !user.is_empty()
        && user
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_.+".contains(c));
    let domain_ok = domain.contains('.')
        && !domain.starts_with(['.', '-'])
        && !domain.ends_with(['.', '-'])
        && domain.chars().all(|c| c.is_ascii_alphanumeric() || "-.".contains(c));
    user_ok && domain_ok
}

/// Payouts: a merchant key granted `can_send`, or ADMIN staff.
fn authorize_payout(
    api_key: Option<&CurrentApiKey>,
    staff: Option<&CurrentStaff>,
) -> Result<(), ApiError> {
    match (api_key, staff) {
        (Some(key), _) if key.can_send => Ok(()),
        (Some(_), _) => Err(ApiError::forbidden(
            "This API key is not allowed to send payments",
        )),
        (None, Some(staff)) if staff.role == domain::StaffRole::Admin => Ok(()),
        (None, Some(_)) => Err(ApiError::forbidden("Admin role required")),
        (None, None) => Err(ApiError::unauthorized("Not authenticated")),
    }
}

/// Pay a Lightning address from the wallet. Answers as soon as the send is
/// accepted (usually `pending`); the outcome follows as a payout webhook and
/// through `GET /lightning/payments/{id}`.
async fn create_lightning_payout(
    State(state): State<AppState>,
    api_key: Option<Extension<CurrentApiKey>>,
    staff: Option<Extension<CurrentStaff>>,
    headers: HeaderMap,
    Json(body): Json<PayoutRequest>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    authorize_payout(api_key.as_deref(), staff.as_deref())?;
    let idempotency_key = headers
        .get("idempotency-key")
        .map(|value| value.to_str().unwrap_or_default());
    let payout = validate_payout_request(&body, idempotency_key)?;

    match send_payout(
        &state,
        &payout.ln_address,
        payout.amount_sats,
        payout.comment,
        payout.idempotency_key,
    )
    .await
    {
        Ok(payment) => Ok((
            StatusCode::ACCEPTED,
            Json(lightning_history_json(&LightningHistoryEntry {
                payment,
                deposit_address_id: None,
            })),
        )),
        Err(LightningError::Disabled) => Err(ApiError::service_unavailable(
            "Lightning is not configured".into(),
        )),
        Err(LightningError::Failed(err)) => {
            // Generic on purpose: the SDK detail stays server-side.
            tracing::warn!("Lightning payout to {} failed: {err:#}", payout.ln_address);
            Err(ApiError::bad_gateway("Payment failed"))
        }
    }
}

/// One wallet payment (any direction) — e.g. a payout's current status.
async fn get_lightning_payment(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    match lightning_payment_entry(&state, &id).await {
        Ok(Some(entry)) => Ok(Json(lightning_history_json(&entry))),
        Ok(None) => Err(ApiError::not_found(format!("Lightning payment not found: {id}"))),
        Err(LightningError::Disabled) => Err(ApiError::service_unavailable(
            "Lightning is not configured".into(),
        )),
        Err(LightningError::Failed(err)) => {
            tracing::error!("Lightning payment lookup failed: {err:#}");
            Err(ApiError::internal("Failed to read Lightning payment".into()))
        }
    }
}

/// `(limit, offset)`: limit defaults to 50, clamped to 1–100.
fn history_paging(query: &PageQuery) -> (u32, u32) {
    let limit = query.limit.unwrap_or(50).clamp(1, 100) as u32;
    let offset = u32::try_from(query.offset.unwrap_or(0).max(0)).unwrap_or(u32::MAX);
    (limit, offset)
}

fn lightning_history_json(entry: &LightningHistoryEntry) -> Value {
    let payment = &entry.payment;
    let sats = |value: u128| value.to_string();
    json!({
        "id": payment.id,
        "direction": payment.direction.as_api(),
        "status": payment.status.as_api(),
        "amount": raw_to_decimal_string(&sats(payment.amount_sats), Token::Btc)
            .unwrap_or_else(|_| "0".into()),
        "amount_sats": sats(payment.amount_sats),
        "fees_sats": sats(payment.fees_sats),
        "method": payment.method,
        "invoice": payment.invoice,
        "description": payment.description,
        "tx_id": payment.tx_id,
        "ln_address": payment.ln_address,
        "comment": payment.comment,
        "created_at": i64::try_from(payment.timestamp)
            .ok()
            .and_then(|ts| DateTime::from_timestamp(ts, 0))
            .map(|dt| dt.to_rfc3339()),
        // deposit_addresses.id of the gateway payment this settled, if any.
        "deposit_address_id": entry.deposit_address_id,
    })
}

async fn get_wallet_balances(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let resp = get_cached_wallet_balances(&state).await?;
    Ok(Json(serde_json::to_value(resp)?))
}

fn to_response(row: &DepositAddressRow) -> Result<DepositAddressResponse, ApiError> {
    let network = Network::from_db(&row.network).map_err(ApiError::from)?;
    let token = Token::from_db(&row.token).map_err(ApiError::from)?;
    let status = DepositAddressStatus::from_db(&row.status).map_err(ApiError::from)?;
    Ok(DepositAddressResponse {
        id: row.id.clone(),
        address: row.address.clone(),
        network: network.as_api().to_string(),
        token: token.as_api().to_string(),
        status: status.as_api().to_string(),
        expected_amount: row.expected_amount.normalize().to_string(),
        expected_amount_raw: decimal_to_raw(row.expected_amount, token),
        expires_at: row.expires_at.to_rfc3339(),
        created_at: row.created_at.to_rfc3339(),
    })
}

#[derive(Debug, Deserialize)]
struct CreateDepositAddressRequest {
    network: String,
    token: String,
    /// Decimal string — USDT networks only.
    expected_amount: Option<String>,
    /// Integer string in sats — bitcoin/lightning only.
    expected_amount_sats: Option<String>,
    expires_at: Option<String>,
    metadata: Option<Value>,
    /// Lightning only: BOLT11 memo (≤ 128 chars).
    description: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PageQuery {
    limit: Option<i64>,
    offset: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct ListQuery {
    status: Option<String>,
    network: Option<String>,
    limit: Option<i64>,
    offset: Option<i64>,
}

#[derive(Debug, Serialize)]
struct DepositAddressResponse {
    id: String,
    address: String,
    network: String,
    token: String,
    status: String,
    expected_amount: String,
    /// Base units: sats for BTC, 10^-6 for USDT.
    expected_amount_raw: String,
    expires_at: String,
    created_at: String,
}

#[derive(Debug, Serialize)]
struct ListResponse {
    data: Vec<DepositAddressResponse>,
    total: i64,
    limit: i64,
    offset: i64,
}

#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn not_found(message: String) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message,
        }
    }
    fn bad_request(message: &str) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
        }
    }
    fn unauthorized(message: &str) -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            message: message.into(),
        }
    }
    fn forbidden(message: &str) -> Self {
        Self {
            status: StatusCode::FORBIDDEN,
            message: message.into(),
        }
    }
    fn conflict(message: &str) -> Self {
        Self {
            status: StatusCode::CONFLICT,
            message: message.into(),
        }
    }
    fn internal(message: String) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message,
        }
    }
    fn bad_gateway(message: &str) -> Self {
        Self {
            status: StatusCode::BAD_GATEWAY,
            message: message.into(),
        }
    }
    fn service_unavailable(message: String) -> Self {
        Self {
            status: StatusCode::SERVICE_UNAVAILABLE,
            message,
        }
    }
}

impl From<domain::DomainError> for ApiError {
    fn from(value: domain::DomainError) -> Self {
        Self::not_found(value.to_string())
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(value: anyhow::Error) -> Self {
        Self::internal(value.to_string())
    }
}

impl From<serde_json::Error> for ApiError {
    fn from(value: serde_json::Error) -> Self {
        Self::internal(value.to_string())
    }
}

impl From<redis::RedisError> for ApiError {
    fn from(value: redis::RedisError) -> Self {
        Self::internal(value.to_string())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = Json(json!({
            "statusCode": self.status.as_u16(),
            "message": self.message,
        }));
        (self.status, body).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn request(body: Value) -> CreateDepositAddressRequest {
        serde_json::from_value(body).expect("valid request shape")
    }

    fn rejected(body: Value) -> ApiError {
        validate_create_request(&request(body)).expect_err("must be rejected")
    }

    #[test]
    fn bitcoin_takes_the_amount_in_sats() {
        let valid = validate_create_request(&request(json!({
            "network": "bitcoin", "token": "BTC", "expected_amount_sats": "150000"
        })))
        .unwrap();
        assert_eq!(valid.network, Network::Bitcoin);
        assert_eq!(valid.token, Token::Btc);
        assert_eq!(valid.expected_sats, Some(150_000));
        assert_eq!(valid.expected_amount, dec!(0.0015));
    }

    #[test]
    fn missing_sats_on_bitcoin_is_a_bad_request() {
        let err = rejected(json!({ "network": "bitcoin", "token": "BTC" }));
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
        assert!(err.message.contains("expected_amount_sats"), "{}", err.message);
    }

    #[test]
    fn expected_amount_is_rejected_on_btc_networks() {
        // `1500` must never be ambiguous between sats and BTC.
        for network in ["bitcoin", "lightning"] {
            let err = rejected(json!({
                "network": network, "token": "BTC",
                "expected_amount": "0.0015", "expected_amount_sats": "150000"
            }));
            assert_eq!(err.status, StatusCode::BAD_REQUEST, "{network}");
        }
    }

    #[test]
    fn sats_on_a_usdt_network_is_a_bad_request() {
        let err = rejected(json!({
            "network": "tron", "token": "USDT",
            "expected_amount": "25", "expected_amount_sats": "150000"
        }));
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
        assert!(err.message.contains("expected_amount_sats"), "{}", err.message);
    }

    #[test]
    fn invalid_sats_are_bad_requests() {
        for sats in ["0", "-1", "1.5", "abc", "", "2100000000000001", "1e5"] {
            let err = rejected(json!({
                "network": "lightning", "token": "BTC", "expected_amount_sats": sats
            }));
            assert_eq!(err.status, StatusCode::BAD_REQUEST, "{sats}");
        }
    }

    #[test]
    fn usdt_networks_are_unchanged() {
        let valid = validate_create_request(&request(json!({
            "network": "tron", "token": "usdt", "expected_amount": "25.50"
        })))
        .unwrap();
        assert_eq!(valid.network, Network::Tron);
        assert_eq!(valid.token, Token::Usdt);
        assert_eq!(valid.expected_amount, dec!(25.50));
        assert_eq!(valid.expected_sats, None);

        let err = rejected(json!({ "network": "ethereum", "token": "USDT" }));
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
        assert_eq!(err.message, "Invalid expected_amount");
    }

    #[test]
    fn usdt_amounts_keep_six_decimals() {
        // Same rounding the former DECIMAL(18,6) column applied.
        let valid = validate_create_request(&request(json!({
            "network": "solana", "token": "USDT", "expected_amount": "1.12345678"
        })))
        .unwrap();
        assert_eq!(valid.expected_amount, dec!(1.123457));
    }

    #[test]
    fn token_must_match_the_network() {
        for (network, token) in [("tron", "BTC"), ("bitcoin", "USDT"), ("lightning", "USDT")] {
            let err = rejected(json!({
                "network": network, "token": token,
                "expected_amount": "1", "expected_amount_sats": "1"
            }));
            assert_eq!(err.status, StatusCode::BAD_REQUEST, "{network}/{token}");
            assert_eq!(err.message, "Token not supported on this network");
        }
    }

    #[test]
    fn description_is_capped_at_128_chars() {
        let ok = validate_create_request(&request(json!({
            "network": "lightning", "token": "BTC", "expected_amount_sats": "1000",
            "description": "é".repeat(128)
        })))
        .unwrap();
        assert_eq!(ok.description.as_deref().map(|d| d.chars().count()), Some(128));

        let err = rejected(json!({
            "network": "lightning", "token": "BTC", "expected_amount_sats": "1000",
            "description": "x".repeat(129)
        }));
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
        assert!(err.message.contains("128"), "{}", err.message);
    }

    #[test]
    fn description_is_lightning_only() {
        let err = rejected(json!({
            "network": "bitcoin", "token": "BTC", "expected_amount_sats": "1000",
            "description": "order #123"
        }));
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn invoice_expiry_matches_the_gateway_expiry() {
        let now = Utc::now();
        assert_eq!(invoice_expiry_secs(now + Duration::minutes(60), now).unwrap(), 3600);
        assert_eq!(
            invoice_expiry_secs(now + Duration::days(365 * 200), now).unwrap(),
            u32::MAX
        );
        for past in [now, now - Duration::seconds(1)] {
            let err = invoice_expiry_secs(past, now).unwrap_err();
            assert_eq!(err.status, StatusCode::BAD_REQUEST);
        }
    }

    #[test]
    fn lightning_history_entry_renders_btc_amounts_and_link() {
        use lightning_client::{PaymentDirection, WalletPayment, WalletPaymentStatus};
        let entry = jobs::LightningHistoryEntry {
            payment: WalletPayment {
                id: "pay-1".into(),
                direction: PaymentDirection::Receive,
                status: WalletPaymentStatus::Completed,
                amount_sats: 150_000,
                fees_sats: 2,
                timestamp: 1_700_000_000,
                method: "lightning".into(),
                invoice: Some("lnbc1500u1invoice".into()),
                description: Some("order #123".into()),
                tx_id: None,
                ln_address: None,
                comment: None,
            },
            deposit_address_id: Some("addr-1".into()),
        };
        assert_eq!(
            lightning_history_json(&entry),
            json!({
                "id": "pay-1",
                "direction": "receive",
                "status": "completed",
                "amount": "0.0015",
                "amount_sats": "150000",
                "fees_sats": "2",
                "method": "lightning",
                "invoice": "lnbc1500u1invoice",
                "description": "order #123",
                "tx_id": null,
                "ln_address": null,
                "comment": null,
                "created_at": "2023-11-14T22:13:20+00:00",
                "deposit_address_id": "addr-1",
            })
        );
    }

    #[test]
    fn history_paging_defaults_and_bounds() {
        let page = |limit: Option<i64>, offset: Option<i64>| {
            history_paging(&PageQuery { limit, offset })
        };
        assert_eq!(page(None, None), (50, 0));
        assert_eq!(page(Some(500), Some(-3)), (100, 0));
        assert_eq!(page(Some(0), Some(40)), (1, 40));
        assert_eq!(page(Some(20), Some(i64::MAX)), (20, u32::MAX));
    }

    fn payout(body: Value) -> PayoutRequest {
        serde_json::from_value(body).expect("valid payout shape")
    }

    #[test]
    fn payout_request_accepts_a_lightning_address_and_sats() {
        let valid = validate_payout_request(
            &payout(json!({
                "ln_address": " Alice@Example.com ", "amount_sats": "150000", "comment": "thanks"
            })),
            Some("6F1C7A8E-2F43-4D55-9A8B-1C2D3E4F5A6B"),
        )
        .unwrap();
        assert_eq!(valid.ln_address, "Alice@Example.com");
        assert_eq!(valid.amount_sats, 150_000);
        assert_eq!(valid.comment.as_deref(), Some("thanks"));
        // The SDK requires a canonical UUID.
        assert_eq!(
            valid.idempotency_key.as_deref(),
            Some("6f1c7a8e-2f43-4d55-9a8b-1c2d3e4f5a6b")
        );

        let no_key = validate_payout_request(
            &payout(json!({ "ln_address": "alice@example.com", "amount_sats": "1" })),
            None,
        )
        .unwrap();
        assert_eq!(no_key.idempotency_key, None);
        assert_eq!(no_key.comment, None);
    }

    #[test]
    fn payout_request_only_takes_user_at_domain() {
        for address in [
            "alice",
            "@example.com",
            "alice@",
            "alice@localhost",
            "alice@@example.com",
            "a b@example.com",
            "lnurl1dp68gurn8ghj7um9wfmxjcm99e3k7mf0v9cxj0m385ekvcenxc6r2c35xvukxefcv5mkvv34x5ekzd3ev56nyd3hxqurzepexejxxepnxscrvwfnv9nxzcn9xq6xyefhvgcxxcmyxymnserxfq5fns",
            "lnbc1500n1pjexample",
        ] {
            let err = validate_payout_request(
                &payout(json!({ "ln_address": address, "amount_sats": "1000" })),
                None,
            )
            .expect_err(address);
            assert_eq!(err.status, StatusCode::BAD_REQUEST, "{address}");
        }
        let too_long = format!("{}@example.com", "a".repeat(400));
        assert!(validate_payout_request(
            &payout(json!({ "ln_address": too_long, "amount_sats": "1000" })),
            None,
        )
        .is_err());
    }

    #[test]
    fn payout_request_rejects_bad_amounts_and_keys() {
        for sats in ["0", "-1", "1.5", "abc", ""] {
            let err = validate_payout_request(
                &payout(json!({ "ln_address": "alice@example.com", "amount_sats": sats })),
                None,
            )
            .expect_err(sats);
            assert_eq!(err.status, StatusCode::BAD_REQUEST, "{sats}");
        }
        let err = validate_payout_request(
            &payout(json!({ "ln_address": "alice@example.com", "amount_sats": "1000" })),
            Some("order-123"),
        )
        .unwrap_err();
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
        assert!(err.message.contains("Idempotency-Key"), "{}", err.message);
    }

    #[test]
    fn payouts_need_a_permitted_key_or_an_admin() {
        use crate::middleware::{CurrentApiKey, CurrentStaff};
        let key = |can_send| CurrentApiKey { id: "k".into(), can_send };
        let staff = |role| CurrentStaff { id: "s".into(), email: "s@example.com".into(), role };

        assert!(authorize_payout(Some(&key(true)), None).is_ok());
        let err = authorize_payout(Some(&key(false)), None).unwrap_err();
        assert_eq!(err.status, StatusCode::FORBIDDEN);

        assert!(authorize_payout(None, Some(&staff(domain::StaffRole::Admin))).is_ok());
        let err = authorize_payout(None, Some(&staff(domain::StaffRole::Operator))).unwrap_err();
        assert_eq!(err.status, StatusCode::FORBIDDEN);

        let err = authorize_payout(None, None).unwrap_err();
        assert_eq!(err.status, StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn expected_amount_raw_uses_token_base_units() {
        let now = Utc::now();
        let row = |network: Network, amount: Decimal| DepositAddressRow {
            id: "a".into(),
            network: network.as_db().into(),
            token: network.token().as_db().into(),
            address: "addr".into(),
            derivation_index: 0,
            expected_amount: amount,
            status: DepositAddressStatus::Pending.as_db().into(),
            expires_at: now,
            metadata: json!({}),
            created_at: now,
            updated_at: now,
        };
        let btc = to_response(&row(Network::Lightning, dec!(0.00150000))).unwrap();
        assert_eq!(btc.expected_amount, "0.0015");
        assert_eq!(btc.expected_amount_raw, "150000");
        assert_eq!(btc.network, "lightning");
        assert_eq!(btc.token, "BTC");

        let usdt = to_response(&row(Network::Tron, dec!(25.50000000))).unwrap();
        assert_eq!(usdt.expected_amount, "25.5");
        assert_eq!(usdt.expected_amount_raw, "25500000");
    }
}
