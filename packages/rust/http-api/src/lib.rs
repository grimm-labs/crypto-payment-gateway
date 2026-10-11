use std::collections::HashMap;
use std::time::Duration as StdDuration;

use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{header, HeaderMap, HeaderName, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use chrono::{DateTime, Duration, Utc};
use config::{
    DEPOSIT_EXPIRY_MAX_MINUTES, DEPOSIT_EXPIRY_MIN_MINUTES, MAX_PENDING_ADDRESSES_PER_API_KEY,
    USDT_MAX_EXPECTED_AMOUNT,
};
use db::{AuditActor, DepositAddressRow, WebhookDeliveryRow};
use domain::{
    decimal_to_raw, format_amount, parse_sats, raw_to_decimal, raw_to_decimal_string,
    round_to_token, supports,
    DepositAddressStatus, Network, Token, WebhookDeliveryStatus, WebhookEventType, BTC_MAX_SATS,
};
use jobs::{
    get_cached_wallet_balances, get_live_balances, lightning_payment_entry,
    lightning_payment_history, notify_address_event, retrigger_failed_deposit_webhooks,
    retrigger_webhook, Actor, AppState, LightningError, LightningHistoryEntry, PayoutCommand,
    PayoutError, RetriggerError,
};
use middleware::{ClientIp, CurrentApiKey, CurrentStaff};
use rust_decimal::Decimal;
use axum::http::HeaderValue;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tower_http::cors::{AllowOrigin, CorsLayer};
use tower_http::set_header::SetResponseHeaderLayer;
use tower_http::timeout::TimeoutLayer;
use tower_http::trace::TraceLayer;

mod admin;
mod middleware;
mod rate_limit;
mod session;
mod traffic;

const READINESS_TIMEOUT: StdDuration = StdDuration::from_secs(2);
/// Whole-request bound; above the payout call's own timeout.
const REQUEST_TIMEOUT: StdDuration = StdDuration::from_secs(config::PAYOUT_TIMEOUT_SECS + 15);
/// JSON bodies are small; axum's implicit 2 MiB is far more than needed.
const BODY_LIMIT_BYTES: usize = 64 * 1024;
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
        .route("/lightning/payments/quote", post(quote_lightning_payout))
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
        tracing::warn!("JWT_SECRET is not set — /api/admin/v1 and the dashboard are disabled");
    }

    // Inside the traffic layer, so a timed-out request is still recorded.
    let app = app
        .layer(DefaultBodyLimit::max(BODY_LIMIT_BYTES))
        .layer(TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            REQUEST_TIMEOUT,
        ));
    let mut app = app;

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

    // JSON API: nothing may be sniffed, framed or cached (tokens, keys,
    // payment data).
    let mut app = app
        .layer(SetResponseHeaderLayer::if_not_present(
            header::X_CONTENT_TYPE_OPTIONS,
            HeaderValue::from_static("nosniff"),
        ))
        .layer(SetResponseHeaderLayer::if_not_present(
            header::X_FRAME_OPTIONS,
            HeaderValue::from_static("DENY"),
        ))
        .layer(SetResponseHeaderLayer::if_not_present(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static("default-src 'none'; frame-ancestors 'none'"),
        ))
        .layer(SetResponseHeaderLayer::if_not_present(
            header::REFERRER_POLICY,
            HeaderValue::from_static("no-referrer"),
        ))
        .layer(SetResponseHeaderLayer::if_not_present(
            header::CACHE_CONTROL,
            HeaderValue::from_static("no-store"),
        ));
    if !state.config.is_development() {
        app = app.layer(SetResponseHeaderLayer::if_not_present(
            header::STRICT_TRANSPORT_SECURITY,
            HeaderValue::from_static("max-age=31536000; includeSubDomains"),
        ));
    }

    let app = app.layer(TraceLayer::new_for_http());
    let app = if state.config.cors_allowed_origins.is_empty() {
        app
    } else {
        app.layer(cors_layer(&state.config.cors_allowed_origins))
    };
    app.with_state(state)
}

/// Optional safety net for setups where the dashboard is not served behind
/// the same origin (dev proxy / nginx cover the normal cases). Origins are
/// validated at startup (`config`).
fn cors_layer(origins: &[String]) -> CorsLayer {
    let list: Vec<HeaderValue> = origins
        .iter()
        .filter_map(|origin| HeaderValue::from_str(origin).ok())
        .collect();
    CorsLayer::new()
        .allow_origin(AllowOrigin::list(list))
        .allow_methods([Method::GET, Method::POST, Method::PATCH, Method::DELETE])
        .allow_headers([
            header::CONTENT_TYPE,
            header::AUTHORIZATION,
            HeaderName::from_static("x-api-key"),
            HeaderName::from_static("idempotency-key"),
            HeaderName::from_static(session::CSRF_HEADER),
        ])
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
        Ok(Err(err)) => {
            // Detail stays in the logs — /readyz is public.
            tracing::warn!("Postgres readiness check failed: {err:#}");
            "error".into()
        }
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
        Ok(Err(err)) => {
            tracing::warn!("Redis readiness check failed: {err}");
            "error".into()
        }
        Err(_) => "timeout".into(),
    }
}

/// The audit actor of a request (merchant key or staff member).
fn actor(api_key: Option<&CurrentApiKey>, staff: Option<&CurrentStaff>, ip: &ClientIp) -> Actor {
    let audit = match (api_key, staff) {
        (Some(key), _) => AuditActor::ApiKey(key.id.clone()),
        (None, Some(staff)) => AuditActor::Staff(staff.id.clone()),
        (None, None) => AuditActor::System,
    };
    Actor {
        audit,
        ip: ip.0.clone(),
    }
}

async fn create_deposit_address(
    State(state): State<AppState>,
    api_key: Option<Extension<CurrentApiKey>>,
    Json(body): Json<CreateDepositAddressRequest>,
) -> Result<(StatusCode, Json<DepositAddressResponse>), ApiError> {
    let request = validate_create_request(&body)?;
    let network = request.network;
    // An address nobody polls would never see its payment (#66).
    if !state.is_network_enabled(network) {
        return Err(ApiError::service_unavailable(format!(
            "{} is not configured",
            network.as_api()
        )));
    }

    let now = Utc::now();
    let expires_at = match body.expires_at.as_deref() {
        Some(exp) => {
            let expires_at = chrono::DateTime::parse_from_rfc3339(exp)
                .map(|d| d.with_timezone(&Utc))
                .map_err(|_| ApiError::bad_request("Invalid expires_at"))?;
            validate_expires_at(expires_at, now)?
        }
        None => {
            let expiry_minutes = state.db.deposit_expiry_minutes().await?;
            now + Duration::minutes(expiry_minutes)
        }
    };

    // Each open payment costs a provider call per polling cycle: bound
    // what one merchant key can open.
    if let Some(key) = api_key.as_deref() {
        rate_limit::hit(&state, rate_limit::DEPOSIT_ADDRESS_CREATION, &key.id).await?;
        let open = state.db.count_pending_addresses_for_api_key(&key.id).await?;
        if open >= MAX_PENDING_ADDRESSES_PER_API_KEY {
            return Err(ApiError::new(
                StatusCode::TOO_MANY_REQUESTS,
                format!(
                    "Too many open payments for this API key (max {MAX_PENDING_ADDRESSES_PER_API_KEY})"
                ),
            ));
        }
    }

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
            api_key.as_deref().map(|key| key.id.as_str()),
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
                ApiError::new(
                    StatusCode::BAD_REQUEST,
                    format!("Invalid expected_amount_sats (integer string, 1 to {BTC_MAX_SATS})"),
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
            let amount = round_to_token(amount, token);
            // 0 would be settled by any zero-value transfer (address
            // poisoning); huge values overflow DECIMAL(24,8).
            if amount <= Decimal::ZERO || amount > Decimal::from(USDT_MAX_EXPECTED_AMOUNT) {
                return Err(ApiError::new(
                    StatusCode::BAD_REQUEST,
                    format!(
                        "expected_amount must be greater than 0 and at most {USDT_MAX_EXPECTED_AMOUNT}"
                    ),
                ));
            }
            (amount, None)
        }
    };

    if let Some(description) = &body.description {
        if network != Network::Lightning {
            return Err(ApiError::bad_request("description is only accepted on lightning"));
        }
        if description.chars().count() > INVOICE_DESCRIPTION_MAX_CHARS {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                format!("description must be at most {INVOICE_DESCRIPTION_MAX_CHARS} characters"),
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

/// A client-chosen deadline must leave time to pay and not keep the payment
/// polled for days.
fn validate_expires_at(
    expires_at: DateTime<Utc>,
    now: DateTime<Utc>,
) -> Result<DateTime<Utc>, ApiError> {
    let min = now + Duration::minutes(DEPOSIT_EXPIRY_MIN_MINUTES);
    let max = now + Duration::minutes(DEPOSIT_EXPIRY_MAX_MINUTES);
    if expires_at < min || expires_at > max {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            format!(
                "expires_at must be between {DEPOSIT_EXPIRY_MIN_MINUTES} and {DEPOSIT_EXPIRY_MAX_MINUTES} minutes from now"
            ),
        ));
    }
    Ok(expires_at)
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
    // Expiry is the monitor loops' job (after a last poll), never a side
    // effect of listing.
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
    api_key: Option<Extension<CurrentApiKey>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    // Endpoint URLs (and errors quoting them) are admin data: a merchant
    // key never sees them (#51).
    let merchant = api_key.is_some();
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
                        .map(|delivery| webhook_to_json(delivery, merchant))
                        .unwrap_or(Value::Null);
                    let webhooks: Vec<Value> = entries
                        .iter()
                        .filter_map(|delivery| {
                            let endpoint_id = delivery.webhook_endpoint_id.as_ref()?;
                            let mut entry = webhook_to_json(delivery, merchant);
                            if let Some(entry_obj) = entry.as_object_mut() {
                                entry_obj.insert("endpoint_id".into(), json!(endpoint_id));
                                if !merchant {
                                    entry_obj.insert(
                                        "endpoint_url".into(),
                                        json!(endpoint_urls.get(endpoint_id)),
                                    );
                                }
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

/// `merchant`: drop `last_error` (it can quote the endpoint).
fn webhook_to_json(delivery: &WebhookDeliveryRow, merchant: bool) -> Value {
    let mut entry = json!({
        "status": WebhookDeliveryStatus::from_db(&delivery.status).map(|s| s.as_api()).unwrap_or("pending"),
        "attempts": delivery.attempts,
        "last_response": delivery.last_response,
        "last_error": delivery.last_error,
        "updated_at": delivery.updated_at.to_rfc3339(),
    });
    if merchant {
        if let Some(object) = entry.as_object_mut() {
            object.remove("last_error");
        }
    }
    entry
}

/// Merchant keys may only re-queue deliveries that FAILED (never resend a
/// delivered `deposit_confirmed`), at most once a minute per deposit.
/// Staff (ADMIN-only route) replay every endpoint, delivered ones included.
async fn retry_deposit_webhook(
    State(state): State<AppState>,
    api_key: Option<Extension<CurrentApiKey>>,
    staff: Option<Extension<CurrentStaff>>,
    client_ip: ClientIp,
    Path(deposit_id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let result = match api_key.as_deref() {
        Some(_) => {
            rate_limit::hit(&state, rate_limit::WEBHOOK_REPLAY_PER_DEPOSIT, &deposit_id).await?;
            retrigger_failed_deposit_webhooks(&state, &deposit_id).await
        }
        None => retrigger_webhook(&state, &deposit_id).await.map(|()| 0),
    };
    match result {
        Ok(retried) => {
            jobs::audit::record(
                &state,
                &actor(api_key.as_deref(), staff.as_deref(), &client_ip),
                "webhook.retried",
                Some(("deposit", &deposit_id)),
                json!({ "scope": if api_key.is_some() { "failed" } else { "all" } }),
            )
            .await;
            let mut body = json!({ "status": "queued", "deposit_id": deposit_id });
            if api_key.is_some() {
                body["retried"] = json!(retried);
            }
            Ok(Json(body))
        }
        Err(RetriggerError::DepositNotFound) => Err(ApiError::not_found(format!(
            "Deposit not found: {deposit_id}"
        ))),
        Err(RetriggerError::DepositNotConfirmed) => {
            Err(ApiError::bad_request("Deposit is not confirmed"))
        }
        Err(RetriggerError::NoActiveEndpoints) => Err(ApiError::service_unavailable(
            "No active webhook endpoints configured".into(),
        )),
        Err(RetriggerError::NothingToRetry) => {
            Err(ApiError::conflict("No failed webhook delivery to retry"))
        }
        Err(RetriggerError::Other(err)) => Err(ApiError::internal(format!("{err:#}"))),
        Err(err) => Err(ApiError::internal(format!("{err:?}"))),
    }
}

/// Served from a 5-minute cache, recomputed in the background: the first
/// call (or the first after expiry) answers 503 with `Retry-After`.
async fn get_balances(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    match get_live_balances(&state).await? {
        Some(balances) => Ok(Json(serde_json::to_value(balances)?)),
        None => Err(ApiError::service_unavailable(
            "Balances are being computed — retry shortly".into(),
        )
        .with_retry_after(15)),
    }
}

/// The Lightning wallet history exposes every payee and payer of the
/// (possibly shared) wallet: merchant keys need `can_send`; on the admin
/// surface the routes are ADMIN-only.
fn authorize_lightning_read(api_key: Option<&CurrentApiKey>) -> Result<(), ApiError> {
    match api_key {
        Some(key) if !key.can_send => Err(ApiError::forbidden(
            "This API key is not allowed to access the Lightning wallet",
        )),
        _ => Ok(()),
    }
}

/// The whole Lightning wallet history (every direction and status, funds
/// unrelated to gateway invoices included), newest first. Paged by
/// `has_more`: the SDK reports no total.
async fn list_lightning_payments(
    State(state): State<AppState>,
    api_key: Option<Extension<CurrentApiKey>>,
    Query(query): Query<PageQuery>,
) -> Result<Json<Value>, ApiError> {
    authorize_lightning_read(api_key.as_deref())?;
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
    /// Staff only: fresh TOTP code (step-up).
    totp_code: Option<String>,
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
    let amount_sats = parse_sats(&body.amount_sats).map_err(|_| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            format!("Invalid amount_sats (integer string, 1 to {BTC_MAX_SATS})"),
        )
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

/// Domain suffixes that only resolve inside a private network (or never).
const LOCAL_TLDS: [&str; 13] = [
    "local", "localhost", "internal", "lan", "home", "localdomain", "arpa", "test", "invalid",
    "example", "onion", "corp", "intranet",
];

/// `user@domain` (LUD-16): one `@`, a user part of letters, digits and
/// `-_.` (what the SDK accepts), and a public DNS name — the SDK fetches
/// `https://domain/.well-known/lnurlp/user`, so IP literals and local names
/// are refused to keep the gateway from probing its own network. Everything
/// else the SDK could parse (LNURL, invoices, on-chain addresses) is refused
/// before reaching it.
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
            .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c));
    let labels: Vec<&str> = domain.split('.').collect();
    let labels_ok = labels.len() >= 2
        && labels.iter().all(|label| {
            !label.is_empty()
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        });
    // A public TLD is alphabetic — this also rules out IPv4 literals.
    let tld = labels.last().copied().unwrap_or_default().to_ascii_lowercase();
    let tld_ok = tld.len() >= 2
        && tld.chars().all(|c| c.is_ascii_alphabetic())
        && !LOCAL_TLDS.contains(&tld.as_str());
    user_ok && labels_ok && tld_ok
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

/// The SDK fetches `https://<domain>/.well-known/lnurlp/<user>`: a public
/// name that resolves to an internal address would make the gateway probe
/// its own network (#65). Best effort (the SDK resolves again), skipped in
/// development.
async fn ensure_public_ln_domain(state: &AppState, ln_address: &str) -> Result<(), ApiError> {
    if state.config.is_development() {
        return Ok(());
    }
    let host = ln_address.rsplit_once('@').map(|(_, d)| d).unwrap_or_default();
    let resolved: Vec<std::net::SocketAddr> = tokio::net::lookup_host((host, 443))
        .await
        .map_err(|_| ApiError::bad_request("Lightning address domain does not resolve"))?
        .collect();
    if resolved.is_empty() || !resolved.iter().all(|addr| domain::is_public_ip(addr.ip())) {
        return Err(ApiError::bad_request(
            "Lightning address domain must resolve to public addresses only",
        ));
    }
    Ok(())
}

fn payout_error(err: PayoutError, quote: bool) -> ApiError {
    match err {
        PayoutError::Unavailable => {
            ApiError::service_unavailable("Lightning is not configured".into())
        }
        PayoutError::PayoutsDisabled => {
            ApiError::unprocessable("Payouts are disabled (limit set to 0)".into())
        }
        PayoutError::PerPayoutLimit { max_sats } => ApiError::unprocessable(format!(
            "Amount exceeds the per-payout limit of {max_sats} sats"
        )),
        PayoutError::DailyLimit {
            used_sats,
            max_sats,
        } => ApiError::unprocessable(format!(
            "Daily payout limit reached: {used_sats} of {max_sats} sats already sent in the last 24 h"
        )),
        PayoutError::InProgress => {
            ApiError::conflict("A payout with this Idempotency-Key is already in progress")
        }
        // Also covers a key used by another caller — without saying so.
        PayoutError::KeyReused => ApiError::unprocessable(
            "Idempotency-Key was already used with different parameters".into(),
        ),
        // Generic on purpose: the SDK detail stays server-side.
        PayoutError::Failed if quote => ApiError::bad_gateway("Could not get a quote"),
        PayoutError::Failed => ApiError::bad_gateway("Payment failed"),
        PayoutError::Internal(err) => ApiError::internal(format!("{err:#}")),
    }
}

/// Pay a Lightning address from the wallet, once per `Idempotency-Key`
/// (required). Answers 202 as soon as the send is accepted (usually
/// `pending`), 200 for a replayed key; the outcome follows as a payout
/// webhook and through `GET /lightning/payments/{id}`. Staff need a fresh
/// TOTP code.
async fn create_lightning_payout(
    State(state): State<AppState>,
    api_key: Option<Extension<CurrentApiKey>>,
    staff: Option<Extension<CurrentStaff>>,
    client_ip: ClientIp,
    headers: HeaderMap,
    Json(body): Json<PayoutRequest>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    authorize_payout(api_key.as_deref(), staff.as_deref())?;
    let idempotency_key = headers
        .get("idempotency-key")
        .map(|value| value.to_str().unwrap_or_default())
        .ok_or_else(|| ApiError::bad_request("Idempotency-Key header is required (UUID)"))?;
    let payout = validate_payout_request(&body, Some(idempotency_key))?;
    let caller_id = api_key
        .as_deref()
        .map(|key| key.id.clone())
        .or_else(|| staff.as_deref().map(|staff| staff.id.clone()))
        .unwrap_or_default();
    rate_limit::hit(&state, rate_limit::PAYOUTS, &caller_id).await?;
    if let (None, Some(staff)) = (api_key.as_deref(), staff.as_deref()) {
        admin::require_step_up(&state, &staff.id, body.totp_code.as_deref()).await?;
    }
    ensure_public_ln_domain(&state, &payout.ln_address).await?;

    let command = PayoutCommand {
        ln_address: payout.ln_address,
        amount_sats: payout.amount_sats,
        comment: payout.comment,
        idempotency_key: payout
            .idempotency_key
            .ok_or_else(|| ApiError::bad_request("Idempotency-Key header is required (UUID)"))?,
        api_key_id: api_key.as_deref().map(|key| key.id.clone()),
        staff_id: match api_key.as_deref() {
            Some(_) => None,
            None => staff.as_deref().map(|staff| staff.id.clone()),
        },
    };
    let actor = actor(api_key.as_deref(), staff.as_deref(), &client_ip);
    let outcome = jobs::request_payout(&state, &actor, &command)
        .await
        .map_err(|err| payout_error(err, false))?;
    let status = if outcome.replayed {
        StatusCode::OK
    } else {
        StatusCode::ACCEPTED
    };
    Ok((
        status,
        Json(lightning_history_json(&LightningHistoryEntry {
            payment: outcome.payment,
            deposit_address_id: None,
        })),
    ))
}

#[derive(Debug, Deserialize)]
struct PayoutQuoteRequest {
    ln_address: String,
    amount_sats: String,
    comment: Option<String>,
}

/// What a payout would cost (the recipient's invoice + fee), without
/// paying — the dashboard's confirmation step.
async fn quote_lightning_payout(
    State(state): State<AppState>,
    api_key: Option<Extension<CurrentApiKey>>,
    staff: Option<Extension<CurrentStaff>>,
    Json(body): Json<PayoutQuoteRequest>,
) -> Result<Json<Value>, ApiError> {
    authorize_payout(api_key.as_deref(), staff.as_deref())?;
    let payout = validate_payout_request(
        &PayoutRequest {
            ln_address: body.ln_address,
            amount_sats: body.amount_sats,
            comment: body.comment,
            totp_code: None,
        },
        None,
    )?;
    let caller_id = api_key
        .as_deref()
        .map(|key| key.id.clone())
        .or_else(|| staff.as_deref().map(|staff| staff.id.clone()))
        .unwrap_or_default();
    rate_limit::hit(&state, rate_limit::PAYOUTS, &caller_id).await?;
    ensure_public_ln_domain(&state, &payout.ln_address).await?;
    let quote = jobs::quote_payout(&state, &payout.ln_address, payout.amount_sats, payout.comment)
        .await
        .map_err(|err| payout_error(err, true))?;
    Ok(Json(json!({
        "ln_address": payout.ln_address,
        "amount_sats": quote.amount_sats.to_string(),
        "fees_sats": quote.fees_sats.to_string(),
        "total_sats": quote.amount_sats.saturating_add(quote.fees_sats).to_string(),
    })))
}

/// One wallet payment (any direction) — e.g. a payout's current status.
async fn get_lightning_payment(
    State(state): State<AppState>,
    api_key: Option<Extension<CurrentApiKey>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    authorize_lightning_read(api_key.as_deref())?;
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
    /// Sent as `Retry-After` (429 / 503).
    retry_after: Option<u64>,
}

impl ApiError {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
            retry_after: None,
        }
    }
    fn with_retry_after(mut self, secs: u64) -> Self {
        self.retry_after = Some(secs);
        self
    }
    fn not_found(message: String) -> Self {
        Self::new(StatusCode::NOT_FOUND, message)
    }
    fn bad_request(message: &str) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }
    fn unauthorized(message: &str) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, message)
    }
    fn forbidden(message: &str) -> Self {
        Self::new(StatusCode::FORBIDDEN, message)
    }
    fn conflict(message: &str) -> Self {
        Self::new(StatusCode::CONFLICT, message)
    }
    fn unprocessable(message: String) -> Self {
        Self::new(StatusCode::UNPROCESSABLE_ENTITY, message)
    }
    /// The detail (driver errors, constraint names…) is logged, never sent:
    /// clients only get a generic message.
    fn internal(detail: String) -> Self {
        tracing::error!("Internal error: {detail}");
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "Internal server error")
    }
    fn bad_gateway(message: &str) -> Self {
        Self::new(StatusCode::BAD_GATEWAY, message)
    }
    fn service_unavailable(message: String) -> Self {
        Self::new(StatusCode::SERVICE_UNAVAILABLE, message)
    }
}

impl From<domain::DomainError> for ApiError {
    fn from(value: domain::DomainError) -> Self {
        Self::not_found(value.to_string())
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(value: anyhow::Error) -> Self {
        Self::internal(format!("{value:#}"))
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
        let mut response = (self.status, body).into_response();
        if let Some(secs) = self.retry_after {
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from(secs));
        }
        response
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
            // The SDK only accepts [a-z0-9-_.] in the user part.
            "alice+tips@example.com",
            // No IP literals or local names: the SDK would fetch them
            // (blind SSRF from the gateway host).
            "alice@127.0.0.1",
            "alice@10.0.0.5",
            "alice@169.254.169.254",
            "alice@printer.local",
            "alice@db.internal",
            "alice@example.localhost",
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
        let staff = |role| CurrentStaff { id: "s".into(), role };

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
    fn usdt_amounts_must_be_positive_and_bounded() {
        for amount in ["0", "-5", "0.0000001", "1000000000000.01"] {
            let err = rejected(json!({
                "network": "tron", "token": "USDT", "expected_amount": amount
            }));
            assert_eq!(err.status, StatusCode::BAD_REQUEST, "{amount}");
        }
    }

    #[test]
    fn expires_at_is_bounded() {
        let now = Utc::now();
        assert!(validate_expires_at(now + Duration::minutes(30), now).is_ok());
        for bad in [
            now - Duration::minutes(5),
            now + Duration::seconds(30),
            now + Duration::minutes(1441),
            now + Duration::days(365 * 7000),
        ] {
            let err = validate_expires_at(bad, now).unwrap_err();
            assert_eq!(err.status, StatusCode::BAD_REQUEST);
        }
    }

    #[test]
    fn lightning_history_needs_a_sending_key() {
        assert!(authorize_lightning_read(None).is_ok());
        let key = |can_send| CurrentApiKey { id: "k".into(), can_send };
        assert!(authorize_lightning_read(Some(&key(true))).is_ok());
        assert_eq!(
            authorize_lightning_read(Some(&key(false))).unwrap_err().status,
            StatusCode::FORBIDDEN
        );
    }

    #[test]
    fn payout_errors_map_to_client_safe_statuses() {
        let status = |err| payout_error(err, false).status;
        assert_eq!(status(PayoutError::PerPayoutLimit { max_sats: 1 }), StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(status(PayoutError::DailyLimit { used_sats: 1, max_sats: 2 }), StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(status(PayoutError::PayoutsDisabled), StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(status(PayoutError::KeyReused), StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(status(PayoutError::InProgress), StatusCode::CONFLICT);
        assert_eq!(status(PayoutError::Failed), StatusCode::BAD_GATEWAY);
        let internal = payout_error(PayoutError::Internal(anyhow::anyhow!("pg: relation x")), false);
        assert_eq!(internal.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(internal.message, "Internal server error");
    }

    #[test]
    fn internal_errors_never_leak_details() {
        let err = ApiError::from(anyhow::anyhow!("duplicate key value violates unique constraint \"x\""));
        assert_eq!(err.message, "Internal server error");
    }

    #[test]
    fn local_lightning_domains_are_refused() {
        for address in ["alice@node.test", "alice@1.0.0.10.in-addr.arpa", "alice@x.onion", "alice@pay.corp"] {
            assert!(!is_lightning_address(address), "{address}");
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
            status: DepositAddressStatus::Pending.as_db().into(),
            expires_at: now,
            metadata: json!({}),
            api_key_id: None,
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
