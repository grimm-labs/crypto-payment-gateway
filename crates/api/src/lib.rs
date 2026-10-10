use std::collections::HashMap;
use std::time::Duration as StdDuration;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, Duration, Utc};
use db::{DepositAddressRow, WebhookDeliveryRow};
use domain::{
    decimal_to_raw, format_amount, parse_sats, raw_to_decimal, round_to_token, supports,
    DepositAddressStatus, Network, Token, WebhookDeliveryStatus, WebhookEventType,
};
use jobs::{
    expire_addresses_and_notify, get_cached_wallet_balances, get_live_balances,
    notify_address_event, retrigger_webhook, AppState, RetriggerError,
};
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
            let sats = parse_sats(raw).map_err(|_| {
                ApiError::bad_request(
                    "Invalid expected_amount_sats (integer string, 1 to 2100000000000000)",
                )
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
            return Err(ApiError::bad_request(
                "description must be at most 128 characters",
            ));
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
    fn expected_amount_raw_uses_token_base_units() {
        let now = Utc::now();
        let row = |network: Network, amount: Decimal| DepositAddressRow {
            id: "a".into(),
            network: network.as_db().into(),
            token: network.token().as_db().into(),
            address: "addr".into(),
            derivation_index: 0,
            expected_amount: amount,
            status: "PENDING".into(),
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
