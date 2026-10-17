use std::collections::HashMap;
use std::time::Duration as StdDuration;

use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{header, HeaderName, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use chrono::{DateTime, Duration, Utc};
use config::{
    DEPOSIT_EXPIRY_MAX_MINUTES, DEPOSIT_EXPIRY_MIN_MINUTES, ETH_MAX_EXPECTED_AMOUNT,
    MAX_PENDING_ADDRESSES_PER_API_KEY, USDT_MAX_EXPECTED_AMOUNT,
};
use db::{AuditActor, DepositAddressRow, OwnerFilter, WebhookDeliveryRow};
use domain::{
    decimal_to_raw, format_amount, parse_sats, raw_to_decimal, raw_to_decimal_string,
    round_to_token, supports,
    DepositAddressStatus, Network, Token, WebhookDeliveryStatus, WebhookEventType, BTC_MAX_SATS,
};
use jobs::{
    get_cached_wallet_balances, get_live_balances, lightning_payment_entry,
    lightning_payment_history, notify_address_event, retrigger_failed_deposit_webhooks,
    retrigger_webhook, Actor, AppState, LightningError, LightningHistoryEntry, RetriggerError,
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
mod projects;
mod rate_limit;
mod session;
#[cfg(test)]
mod test_support;
mod traffic;
mod withdrawals;

const READINESS_TIMEOUT: StdDuration = StdDuration::from_secs(2);
/// Whole-request bound; above the payout call's own timeout.
const REQUEST_TIMEOUT: StdDuration = StdDuration::from_secs(config::PAYOUT_TIMEOUT_SECS + 15);
/// JSON bodies are small; axum's implicit 2 MiB is far more than needed.
const BODY_LIMIT_BYTES: usize = 64 * 1024;
/// BOLT11 memo cap for `description` (characters).
const INVOICE_DESCRIPTION_MAX_CHARS: usize = 128;

/// The merchant surface (`/api/v1`): scoped to the calling key's project.
/// Custody figures (`/balances`, `/wallet-balances`) and the Lightning
/// wallet history are staff-only (spec §11): they are not mounted here.
fn merchant_router(state: &AppState) -> Router<AppState> {
    Router::new()
        .route(
            "/deposit-addresses",
            post(create_deposit_address).get(list_deposit_addresses),
        )
        .route("/deposit-addresses/{id}", get(get_deposit_address))
        .route(
            "/deposits/{deposit_id}/webhooks/retry",
            post(retry_deposit_webhook),
        )
        .route(
            "/withdrawals",
            post(withdrawals::create_withdrawal).get(withdrawals::list_withdrawals),
        )
        .route("/withdrawals/quote", post(withdrawals::quote_withdrawal))
        .route("/withdrawals/{id}", get(withdrawals::get_withdrawal))
        .route("/withdrawals/{id}/cancel", post(withdrawals::cancel_withdrawal))
        .route("/address-book", get(withdrawals::merchant_address_book))
        // Deprecated alias of POST /withdrawals (Lightning addresses).
        .route("/lightning/payments", post(withdrawals::create_lightning_payout))
        .route("/lightning/payments/quote", post(withdrawals::quote_lightning_payout))
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            middleware::require_api_key,
        ))
        // Added after the guard, so it answers everyone: the wallet history
        // (GET) moved to the staff surface — 404 like a path that never
        // existed, not 405.
        .route("/lightning/payments", get(merchant_route_removed))
}

/// A route taken out of the merchant surface answers like a missing one.
async fn merchant_route_removed() -> ApiError {
    ApiError::not_found("Not found".into())
}

/// How a merchant read is scoped (invariant 2): the key's project; a legacy
/// key without a project keeps the unscoped behaviour of before projects.
/// Staff reads are never scoped by this (they filter explicitly).
pub(crate) fn merchant_scope(api_key: Option<&CurrentApiKey>) -> OwnerFilter {
    match api_key.and_then(|key| key.project_id.as_deref()) {
        Some(project_id) => OwnerFilter::Project(project_id.to_string()),
        None => OwnerFilter::Any,
    }
}

pub fn router(state: AppState) -> Router {
    let api_v1 = merchant_router(&state);

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
    // The owner (spec §5): the calling key's project, or the staff
    // member's explicit choice — required once projects exist.
    let project_id = match api_key.as_deref() {
        Some(key) => {
            if body.project_id.is_some() {
                return Err(ApiError::unprocessable(
                    "project_id is not accepted from a merchant key: its payments belong to its project".into(),
                ));
            }
            key.project_id.clone()
        }
        None => {
            projects::require_any_project(&state).await?;
            let id = projects::parse_project_id_field(body.project_id.as_deref())?;
            if state.db.find_project(&id).await?.is_none() {
                return Err(ApiError::unprocessable("Unknown project".into()));
            }
            Some(id)
        }
    };
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
            project_id.as_deref(),
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
/// USDT and ETH take the decimal `expected_amount` (rounded to the token's
/// decimals: 6 for USDT, 18 for ETH).
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
        Token::Usdt | Token::Eth => {
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
            let max = if token == Token::Eth {
                ETH_MAX_EXPECTED_AMOUNT
            } else {
                USDT_MAX_EXPECTED_AMOUNT
            };
            // 0 would be settled by any zero-value transfer (address
            // poisoning); huge values overflow DECIMAL(38,18).
            if amount <= Decimal::ZERO || amount > Decimal::from(max) {
                return Err(ApiError::new(
                    StatusCode::BAD_REQUEST,
                    format!("expected_amount must be greater than 0 and at most {max}"),
                ));
            }
            (amount, None)
        }
        // Gas assets are never payments (`supports` refused them above).
        Token::Trx | Token::Sol => {
            return Err(ApiError::bad_request("Token not supported on this network"))
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
    api_key: Option<Extension<CurrentApiKey>>,
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
    // Staff filter on the owner; a merchant key is scoped by its project.
    let owner = match api_key.as_deref() {
        Some(key) => merchant_scope(Some(key)),
        None => projects::owner_filter_param(query.project_id.as_deref())?,
    };

    let (rows, total) = state
        .db
        .list_deposit_addresses(status, network, &owner, limit, offset)
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
    // key never sees them (#51). A key reads through a query scoped on its
    // project (invariant 2): another project's payment is a 404.
    let merchant = api_key.is_some();
    let row = match api_key.as_deref() {
        Some(key) => state.db.find_deposit_address_owned(&id, &merchant_scope(Some(key))).await?,
        None => state.db.find_deposit_address(&id).await?,
    };
    let Some(row) = row else {
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
        .list_webhook_endpoints(&OwnerFilter::Any)
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
        Some(key) => {
            // Another project's deposit does not exist for this key
            // (invariant 2), checked in the query before anything else.
            if state.db.find_deposit_owned(&deposit_id, &merchant_scope(Some(key))).await?.is_none() {
                return Err(ApiError::not_found(format!("Deposit not found: {deposit_id}")));
            }
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
/// (possibly shared) wallet: merchant keys need a withdrawal grant for BTC
/// on Lightning; on the admin surface the routes are ADMIN-only.
fn authorize_lightning_read(api_key: Option<&CurrentApiKey>) -> Result<(), ApiError> {
    match api_key {
        Some(key)
            if jobs::withdrawals::rules::authorize(
                &withdrawals::key_requester(key),
                Network::Lightning,
                Token::Btc,
            )
            .is_err() =>
        {
            Err(ApiError::forbidden(
                "This API key is not allowed to access the Lightning wallet",
            ))
        }
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
        project_id: row.project_id.clone(),
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
    /// Staff only: the owning project (required once projects exist).
    project_id: Option<String>,
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
    /// Staff only: `<uuid>` (or `merchant` for legacy rows).
    project_id: Option<String>,
    limit: Option<i64>,
    offset: Option<i64>,
}

#[derive(Debug, Serialize)]
struct DepositAddressResponse {
    id: String,
    /// The owning project; null only on payments from before projects.
    project_id: Option<String>,
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
    fn lightning_history_needs_a_btc_lightning_grant() {
        use domain::WithdrawalPolicy;
        assert!(authorize_lightning_read(None).is_ok());
        let key = |policy, tokens: Vec<Token>, networks: Option<Vec<Network>>| CurrentApiKey {
            id: "k".into(),
            project_id: Some("p1".into()),
            withdrawal_policy: policy,
            withdrawal_tokens: tokens,
            withdrawal_networks: networks,
        };
        // The migrated `canSend` grant.
        assert!(authorize_lightning_read(Some(&key(WithdrawalPolicy::Open, vec![Token::Btc], Some(vec![Network::Lightning])))).is_ok());
        for denied in [
            key(WithdrawalPolicy::None, vec![Token::Btc], None),
            key(WithdrawalPolicy::Open, vec![Token::Usdt], None),
            key(WithdrawalPolicy::Book, vec![Token::Btc], Some(vec![Network::Bitcoin])),
        ] {
            assert_eq!(authorize_lightning_read(Some(&denied)).unwrap_err().status, StatusCode::FORBIDDEN);
        }
    }

    #[test]
    fn native_eth_payments_take_a_decimal_amount_with_18_decimals() {
        let valid = validate_create_request(&request(json!({
            "network": "ethereum", "token": "ETH", "expected_amount": "0.050000000000000000001"
        })))
        .unwrap();
        assert_eq!(valid.token, Token::Eth);
        assert_eq!(valid.expected_amount, dec!(0.05));
        assert_eq!(decimal_to_raw(valid.expected_amount, Token::Eth), "50000000000000000");
        for (amount, ok) in [("100000", true), ("100000.000000000000000001", false), ("0", false)] {
            let result = validate_create_request(&request(json!({
                "network": "ethereum", "token": "ETH", "expected_amount": amount
            })));
            assert_eq!(result.is_ok(), ok, "{amount}");
        }
        // ETH on Ethereum only; gas assets never.
        for (network, token) in [("tron", "ETH"), ("tron", "TRX"), ("solana", "SOL"), ("bitcoin", "ETH")] {
            let err = rejected(json!({ "network": network, "token": token, "expected_amount": "1" }));
            assert_eq!(err.status, StatusCode::BAD_REQUEST, "{network}/{token}");
        }
        assert!(rejected(json!({ "network": "ethereum", "token": "ETH", "expected_amount_sats": "1" })).message.contains("expected_amount_sats"));
    }

    #[test]
    fn merchant_reads_are_scoped_by_the_key_project() {
        use domain::WithdrawalPolicy;
        let key = |project_id: Option<&str>| CurrentApiKey {
            id: "k".into(),
            project_id: project_id.map(str::to_string),
            withdrawal_policy: WithdrawalPolicy::None,
            withdrawal_tokens: vec![],
            withdrawal_networks: None,
        };
        assert_eq!(merchant_scope(Some(&key(Some("p1")))), OwnerFilter::Project("p1".into()));
        // A legacy key before the first project: unscoped, as before.
        assert_eq!(merchant_scope(Some(&key(None))), OwnerFilter::Any);
        // Staff requests carry no key: never scoped here.
        assert_eq!(merchant_scope(None), OwnerFilter::Any);
    }

    #[test]
    fn internal_errors_never_leak_details() {
        let err = ApiError::from(anyhow::anyhow!("duplicate key value violates unique constraint \"x\""));
        assert_eq!(err.message, "Internal server error");
    }

    #[test]
    fn expected_amount_raw_uses_token_base_units() {
        let now = Utc::now();
        let row = |network: Network, amount: Decimal| DepositAddressRow {
            id: "a".into(),
            network: network.as_db().into(),
            token: network.payment_tokens()[0].as_db().into(),
            address: "addr".into(),
            derivation_index: 0,
            expected_amount: amount,
            status: DepositAddressStatus::Pending.as_db().into(),
            expires_at: now,
            metadata: json!({}),
            api_key_id: None,
            project_id: None,
            btc_received_raw: None,
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

        let mut eth = row(Network::Ethereum, dec!(0.050000000000000000));
        eth.token = Token::Eth.as_db().into();
        let eth = to_response(&eth).unwrap();
        assert_eq!(eth.token, "ETH");
        assert_eq!(eth.expected_amount, "0.05");
        assert_eq!(eth.expected_amount_raw, "50000000000000000");
    }
}
