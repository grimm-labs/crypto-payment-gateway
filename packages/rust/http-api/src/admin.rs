use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use chrono::Utc;
use axum::routing::{delete, get, patch, post};
use axum::{Extension, Json, Router};
use config::{PASSWORD_MAX_LEN, PASSWORD_MIN_LEN};
use db::{ApiRequestFilter, ApiRequestRow, AuditActor, StaffUpdate, StaffUserRow};
use domain::{
    ApiSurface, StaffRole, StatusClass, TrafficWindow, WebhookDeliveryStatus, WebhookEventType,
};
use jobs::{retrigger_delivery, retrigger_failed_webhooks, Actor, AppState, RetriggerError};
use redis::AsyncCommands;
use serde::{Deserialize, Deserializer};
use serde_json::{json, Value};

use crate::middleware::{require_admin, require_jwt, ClientIp, CurrentStaff};
use crate::session::{self, set_cookie_header};
use crate::withdrawals as w;
use crate::{
    create_deposit_address, get_balances, get_deposit_address, get_lightning_payment,
    get_wallet_balances, list_deposit_addresses, list_lightning_payments, rate_limit,
    retry_deposit_webhook, ApiError, ListQuery,
};

/// Everything the dashboard calls lives under `/api/admin/v1`. Only
/// setup/login/refresh/logout are public (rate-limited; refresh and logout
/// read the HttpOnly cookie and need the CSRF header); the rest requires
/// the acting staff's JWT, and anything that moves funds, changes access,
/// replays webhooks or reads the wallet history requires the ADMIN role.
pub fn admin_router(state: &AppState) -> Router<AppState> {
    let public = Router::new()
        .route("/setup", get(get_setup).post(post_setup))
        .route("/auth/login", post(login))
        .route("/auth/login/totp", post(login_totp))
        .route("/auth/refresh", post(refresh))
        .route("/auth/logout", post(logout));

    let admin_only = Router::new()
        .route("/staff", get(list_staff).post(create_staff))
        .route("/staff/{id}", patch(update_staff))
        .route("/staff/{id}/totp/reset", post(reset_staff_totp))
        .route("/api-keys", get(list_api_keys).post(create_api_key))
        .route("/api-keys/{id}", delete(revoke_api_key).patch(update_api_key))
        .route("/settings", get(get_settings).patch(update_settings))
        .route(
            "/webhook-endpoints",
            get(list_webhook_endpoints).post(create_webhook_endpoint),
        )
        .route(
            "/webhook-endpoints/{id}",
            patch(update_webhook_endpoint).delete(delete_webhook_endpoint),
        )
        .route("/webhooks/retry-failed", post(retry_failed))
        .route(
            "/webhook-deliveries/{id}/retry",
            post(retry_webhook_delivery),
        )
        .route(
            "/deposits/{deposit_id}/webhooks/retry",
            post(retry_deposit_webhook),
        )
        .route(
            "/lightning/payments",
            get(list_lightning_payments).post(w::create_lightning_payout),
        )
        .route("/lightning/payments/quote", post(w::quote_lightning_payout))
        .route("/lightning/payments/{id}", get(get_lightning_payment))
        .route("/payout-requests", get(w::list_payout_requests))
        .route("/audit-log", get(list_audit_log))
        // Withdrawals: ADMIN (TOTP checked per action).
        .route("/withdrawals", post(w::create_withdrawal))
        .route("/withdrawals/quote", post(w::quote_withdrawal))
        .route("/withdrawals/{id}/approve", post(w::approve_withdrawal))
        .route("/withdrawals/{id}/reject", post(w::reject_withdrawal))
        .route("/withdrawals/{id}/cancel", post(w::cancel_withdrawal))
        .route("/withdrawals/{id}/bump", post(w::bump_withdrawal))
        .route("/withdrawals/{id}/abandon", post(w::abandon_withdrawal))
        .route("/address-book", post(w::create_address_book_entry))
        .route("/address-book/{id}", delete(w::delete_address_book_entry))
        .route("/sweeps/run", post(w::run_sweeps))
        .route("/outflows/unfreeze", post(w::unfreeze_outflows))
        .route("/settings/assets", patch(w::update_asset_settings))
        .route_layer(axum::middleware::from_fn(require_admin));

    let authed = Router::new()
        .route("/auth/logout-all", post(logout_all))
        .route("/auth/me", get(me))
        .route("/auth/password", post(change_password))
        .route("/auth/totp/setup", post(totp_setup))
        .route("/auth/totp/enable", post(totp_enable))
        .route("/auth/totp/disable", post(totp_disable))
        .route("/webhooks", get(list_webhooks))
        .route("/stats", get(stats))
        .route("/stats/deposits-timeseries", get(deposits_timeseries))
        .route("/traffic/overview", get(traffic_overview))
        .route("/traffic/requests", get(traffic_requests))
        .route(
            "/deposit-addresses",
            post(create_deposit_address).get(list_deposit_addresses),
        )
        .route("/deposit-addresses/{id}", get(get_deposit_address))
        .route("/balances", get(get_balances))
        .route("/wallet-balances", get(get_wallet_balances))
        // Read-only withdrawal views for every staff member; any staff
        // member may freeze outflows (no TOTP: speed matters).
        .route("/withdrawals", get(w::list_withdrawals))
        .route("/withdrawals/{id}", get(w::get_withdrawal))
        .route("/address-book", get(w::list_address_book))
        .route("/treasury", get(w::get_treasury))
        .route("/sweeps", get(w::list_sweeps))
        .route("/alerts", get(w::list_alerts))
        .route("/outflows", get(w::get_outflows))
        .route("/outflows/freeze", post(w::freeze_outflows))
        .route("/settings/assets", get(w::get_asset_settings))
        .merge(admin_only)
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            require_jwt,
        ));

    public.merge(authed)
}

pub(crate) fn staff_actor(staff: &CurrentStaff, ip: &ClientIp) -> Actor {
    Actor {
        audit: AuditActor::Staff(staff.id.clone()),
        ip: ip.0.clone(),
    }
}

/// A session-opening response: JSON body + the refresh cookie.
fn session_response(status: StatusCode, issued: session::IssuedSession) -> Response {
    (status, set_cookie_header(issued.cookie), Json(issued.body)).into_response()
}

async fn get_setup(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let count = state.db.count_staff().await?;
    Ok(Json(json!({ "needs_setup": count == 0 })))
}

#[derive(Debug, Deserialize)]
struct SetupRequest {
    email: String,
    password: String,
    first_name: String,
    last_name: String,
    setup_token: Option<String>,
}

/// First-admin bootstrap. Needs the one-time setup token printed in the
/// API logs at boot (or `SETUP_TOKEN`): an attacker reaching a fresh
/// instance first can no longer claim it. Every cheap check runs before the
/// argon2 hash, so the public route cannot be used to burn CPU.
async fn post_setup(
    State(state): State<AppState>,
    client_ip: ClientIp,
    Json(body): Json<SetupRequest>,
) -> Result<Response, ApiError> {
    rate_limit::hit(&state, rate_limit::SETUP_PER_IP, client_ip.bucket()).await?;
    if state.db.count_staff().await? > 0 {
        return Err(ApiError::conflict("Setup already completed"));
    }
    let token_ok = match (&state.setup_token, body.setup_token.as_deref()) {
        (Some(expected), Some(given)) => {
            auth::constant_time_eq_hex(&auth::sha256_hex(expected), &auth::sha256_hex(given.trim()))
        }
        _ => false,
    };
    if !token_ok {
        return Err(ApiError::forbidden("Invalid setup token"));
    }
    let email = validate_email(&body.email)?;
    let (first_name, last_name) = validate_names(&body.first_name, &body.last_name)?;
    validate_password(&body.password)?;

    let password_hash =
        auth::hash_password(&body.password).map_err(|e| ApiError::internal(e.to_string()))?;
    let Some(staff) = state
        .db
        .create_first_admin(&email, &first_name, &last_name, &password_hash)
        .await?
    else {
        return Err(ApiError::conflict("Setup already completed"));
    };

    jobs::audit::record(
        &state,
        &Actor {
            audit: AuditActor::Staff(staff.id.clone()),
            ip: client_ip.0.clone(),
        },
        "setup.completed",
        Some(("staff", &staff.id)),
        json!({ "email": staff.email }),
    )
    .await;
    let issued = session::open_session(&state, &staff).await?;
    Ok(session_response(StatusCode::CREATED, issued))
}

#[derive(Debug, Deserialize)]
struct LoginRequest {
    email: String,
    password: String,
}

async fn login(
    State(state): State<AppState>,
    client_ip: ClientIp,
    Json(body): Json<LoginRequest>,
) -> Result<Response, ApiError> {
    // Per client and per targeted account: spraying many accounts from one
    // IP and hammering one account from many IPs are both bounded.
    rate_limit::hit(&state, rate_limit::LOGIN_PER_IP, client_ip.bucket()).await?;
    let email = body.email.trim().to_lowercase();
    rate_limit::hit(&state, rate_limit::LOGIN_PER_ACCOUNT, &email).await?;
    if body.password.len() > PASSWORD_MAX_LEN * 4 {
        return Err(ApiError::unauthorized("Invalid email or password"));
    }

    let staff = state.db.find_staff_by_email(&email).await?;
    // Unknown accounts are verified against a dummy hash: same argon2 cost,
    // so the timing does not reveal whether the email exists.
    let verified = auth::verify_password_constant_time(
        &body.password,
        staff.as_ref().map(|staff| staff.password_hash.as_str()),
    );
    let Some(staff) = staff.filter(|_| verified) else {
        return Err(ApiError::unauthorized("Invalid email or password"));
    };

    // Second factor: hand out a short-lived single-use MFA token instead of
    // the session — /auth/login/totp exchanges it plus a valid code. The
    // deactivation notice is deliberately only shown once every credential
    // (password AND code) has been proven, so it leaks nothing to guessers;
    // for a 2FA account it therefore comes from /auth/login/totp instead.
    if staff.totp_enabled() {
        let (mfa_token, mfa_hash) = auth::generate_refresh_token();
        let key = state.config.redis_key(&format!("auth:mfa:{mfa_hash}"));
        let mut redis = state.redis.clone();
        let _: () = redis.set_ex(key, &staff.id, MFA_TOKEN_TTL_SECS).await?;
        return Ok(Json(json!({
            "mfa_required": true,
            "mfa_token": mfa_token,
        }))
        .into_response());
    }

    if !staff.is_active {
        return Err(ApiError::forbidden(
            "Your account is not active — please contact an administrator",
        ));
    }

    let issued = session::open_session(&state, &staff).await?;
    Ok(session_response(StatusCode::OK, issued))
}

/// Lifetime of the password-proven login challenge.
const MFA_TOKEN_TTL_SECS: u64 = 300;
/// Wrong codes tolerated per MFA token before it is destroyed.
const MFA_MAX_ATTEMPTS: i64 = 5;
/// Anti-replay memory; must outlive the ±1-period skew window.
const TOTP_REPLAY_TTL_SECS: u64 = 120;

#[derive(Debug, Deserialize)]
struct LoginTotpRequest {
    mfa_token: String,
    code: String,
}

async fn login_totp(
    State(state): State<AppState>,
    client_ip: ClientIp,
    Json(body): Json<LoginTotpRequest>,
) -> Result<Response, ApiError> {
    rate_limit::hit(&state, rate_limit::LOGIN_PER_IP, client_ip.bucket()).await?;
    let hash = auth::sha256_hex(&body.mfa_token);
    let key = state.config.redis_key(&format!("auth:mfa:{hash}"));
    let attempts_key = state.config.redis_key(&format!("auth:mfa:attempts:{hash}"));
    let mut redis = state.redis.clone();

    let staff_id: Option<String> = redis.get(&key).await?;
    let Some(staff_id) = staff_id else {
        return Err(ApiError::unauthorized("Invalid or expired login session"));
    };

    let attempts: i64 = redis.incr(&attempts_key, 1).await?;
    if attempts == 1 {
        let _: () = redis.expire(&attempts_key, MFA_TOKEN_TTL_SECS as i64).await?;
    }
    if attempts > MFA_MAX_ATTEMPTS {
        let _: () = redis.del(&key).await?;
        let _: () = redis.del(&attempts_key).await?;
        return Err(ApiError::unauthorized(
            "Too many incorrect codes — sign in again",
        ));
    }

    let Some(staff) = state.db.find_staff_by_id(&staff_id).await? else {
        return Err(ApiError::unauthorized("Unknown staff user"));
    };
    let Some(secret) = staff.totp_secret.as_deref().filter(|_| staff.totp_enabled()) else {
        return Err(ApiError::unauthorized("Invalid or expired login session"));
    };

    // A wrong code does NOT consume the token: a typo shouldn't force the
    // password step again. The per-token counter above and the per-account
    // failure budget (across every MFA token) bound brute force.
    rate_limit::check_failures(&state, rate_limit::TOTP_FAILURES, &staff.id).await?;
    if verify_totp_code(&state, &staff.id, secret, &body.code)
        .await?
        .is_none()
    {
        rate_limit::record_failure(&state, rate_limit::TOTP_FAILURES, &staff.id).await?;
        return Err(ApiError::unauthorized("Invalid authentication code"));
    }

    let _: () = redis.del(&key).await?;
    let _: () = redis.del(&attempts_key).await?;

    // Both factors are proven — only now reveal that the account is blocked.
    if !staff.is_active {
        return Err(ApiError::forbidden(
            "Your account is not active — please contact an administrator",
        ));
    }

    let issued = session::open_session(&state, &staff).await?;
    Ok(session_response(StatusCode::OK, issued))
}

/// Verify a TOTP code with replay protection: a code is only accepted for a
/// timestep strictly newer than the last one used by this staff member.
/// Returns the matched counter, or `None` on wrong/reused codes.
async fn verify_totp_code(
    state: &AppState,
    staff_id: &str,
    secret: &str,
    code: &str,
) -> Result<Option<i64>, ApiError> {
    let Some(counter) = auth::verify_totp(secret, code.trim(), Utc::now().timestamp()) else {
        return Ok(None);
    };
    let key = state.config.redis_key(&format!("auth:totp:last:{staff_id}"));
    let mut redis = state.redis.clone();
    let last: Option<i64> = redis.get(&key).await?;
    if last.is_some_and(|last| counter <= last) {
        return Ok(None);
    }
    let _: () = redis.set_ex(key, counter, TOTP_REPLAY_TTL_SECS).await?;
    Ok(Some(counter))
}

/// Rotate the refresh cookie. A failure clears the cookie.
async fn refresh(
    State(state): State<AppState>,
    client_ip: ClientIp,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    session::require_csrf_header(&headers)?;
    rate_limit::hit(&state, rate_limit::REFRESH_PER_IP, client_ip.bucket()).await?;
    let result = match session::refresh_token_from(&headers) {
        Some(token) => session::refresh_session(&state, &token).await,
        None => Err(ApiError::unauthorized("No session")),
    };
    match result {
        Ok(issued) => Ok(session_response(StatusCode::OK, issued)),
        Err(err) if err.status == StatusCode::UNAUTHORIZED => {
            let mut response = err.into_response();
            response
                .headers_mut()
                .insert(axum::http::header::SET_COOKIE, session::clear_cookie(&state));
            Ok(response)
        }
        Err(err) => Err(err),
    }
}

/// Public on purpose: logging out must work with an expired access token
/// (the old JWT-guarded route went through a refresh first and left the
/// rotated session alive). Always 200, always clears the cookie.
async fn logout(State(state): State<AppState>, headers: HeaderMap) -> Result<Response, ApiError> {
    session::require_csrf_header(&headers)?;
    if let Some(token) = session::refresh_token_from(&headers) {
        session::revoke_session_of_token(&state, &token).await?;
    }
    Ok((
        set_cookie_header(session::clear_cookie(&state)),
        Json(json!({ "status": "ok" })),
    )
        .into_response())
}

/// Revoke every session of the current staff member, this one included.
async fn logout_all(
    State(state): State<AppState>,
    Extension(current): Extension<CurrentStaff>,
    client_ip: ClientIp,
) -> Result<Response, ApiError> {
    session::revoke_all_sessions(&state, &current.id).await?;
    jobs::audit::record(
        &state,
        &staff_actor(&current, &client_ip),
        "auth.sessions_revoked",
        Some(("staff", &current.id)),
        json!({ "scope": "self" }),
    )
    .await;
    Ok((
        set_cookie_header(session::clear_cookie(&state)),
        Json(json!({ "status": "ok" })),
    )
        .into_response())
}

#[derive(Debug, Deserialize)]
struct ChangePasswordRequest {
    current_password: String,
    new_password: String,
    totp_code: Option<String>,
}

/// Self-service password change: needs the current password (and a TOTP
/// code when 2FA is on); every session is revoked and a fresh one opened.
async fn change_password(
    State(state): State<AppState>,
    Extension(current): Extension<CurrentStaff>,
    client_ip: ClientIp,
    Json(body): Json<ChangePasswordRequest>,
) -> Result<Response, ApiError> {
    let Some(staff) = state.db.find_staff_by_id(&current.id).await? else {
        return Err(ApiError::unauthorized("Unknown staff user"));
    };
    verify_current_password(&state, &staff, &body.current_password).await?;
    if staff.totp_enabled() {
        require_step_up(&state, &staff.id, body.totp_code.as_deref()).await?;
    }
    validate_password(&body.new_password)?;
    let hash =
        auth::hash_password(&body.new_password).map_err(|e| ApiError::internal(e.to_string()))?;
    state.db.set_staff_password(&staff.id, &hash).await?;
    session::revoke_all_sessions(&state, &staff.id).await?;
    jobs::audit::record(
        &state,
        &staff_actor(&current, &client_ip),
        "auth.password_changed",
        Some(("staff", &staff.id)),
        json!({}),
    )
    .await;
    let issued = session::open_session(&state, &staff).await?;
    Ok(session_response(StatusCode::OK, issued))
}

/// 400 (never 401 — the dashboard treats 401 as an expired session) on a
/// wrong current password, with a per-account failure budget so a stolen
/// session cannot brute-force it.
async fn verify_current_password(
    state: &AppState,
    staff: &StaffUserRow,
    password: &str,
) -> Result<(), ApiError> {
    rate_limit::check_failures(state, rate_limit::PASSWORD_FAILURES, &staff.id).await?;
    if password.len() > PASSWORD_MAX_LEN * 4 || !auth::verify_password(password, &staff.password_hash) {
        rate_limit::record_failure(state, rate_limit::PASSWORD_FAILURES, &staff.id).await?;
        return Err(ApiError::bad_request("Current password is incorrect"));
    }
    Ok(())
}

/// Step-up authentication for sensitive actions (withdrawals and their
/// approval, withdrawal grants, asset limits, address book, staff
/// management): a fresh TOTP code of the
/// acting staff member. Admins without 2FA cannot perform them.
pub(crate) async fn require_step_up(
    state: &AppState,
    staff_id: &str,
    code: Option<&str>,
) -> Result<(), ApiError> {
    let Some(staff) = state.db.find_staff_by_id(staff_id).await? else {
        return Err(ApiError::unauthorized("Unknown staff user"));
    };
    let Some(secret) = staff.totp_secret.as_deref().filter(|_| staff.totp_enabled()) else {
        return Err(ApiError::forbidden(
            "Two-factor authentication must be enabled on your account to perform this action",
        ));
    };
    let Some(code) = code.map(str::trim).filter(|code| !code.is_empty()) else {
        return Err(ApiError::bad_request("totp_code is required for this action"));
    };
    rate_limit::check_failures(state, rate_limit::TOTP_FAILURES, &staff.id).await?;
    if verify_totp_code(state, &staff.id, secret, code).await?.is_none() {
        rate_limit::record_failure(state, rate_limit::TOTP_FAILURES, &staff.id).await?;
        return Err(ApiError::bad_request("Invalid authentication code"));
    }
    Ok(())
}

async fn me(Extension(staff): Extension<CurrentStaff>, State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let Some(row) = state.db.find_staff_by_id(&staff.id).await? else {
        return Err(ApiError::unauthorized("Unknown staff user"));
    };
    Ok(Json(staff_to_json(&row)))
}

/// Pending (generated but unconfirmed) TOTP secrets live only in Redis, so
/// abandoned setups expire on their own and nothing half-enrolled hits the DB.
const TOTP_PENDING_TTL_SECS: u64 = 600;

#[derive(Debug, Deserialize)]
struct PasswordConfirmation {
    password: String,
}

/// Enrolling needs the password: a stolen session alone must not be able to
/// bind the attacker's authenticator to the account (#60).
async fn totp_setup(
    Extension(current): Extension<CurrentStaff>,
    State(state): State<AppState>,
    Json(body): Json<PasswordConfirmation>,
) -> Result<Json<Value>, ApiError> {
    let Some(staff) = state.db.find_staff_by_id(&current.id).await? else {
        return Err(ApiError::unauthorized("Unknown staff user"));
    };
    verify_current_password(&state, &staff, &body.password)
        .await
        .map_err(|err| match err.status {
            StatusCode::BAD_REQUEST => ApiError::bad_request("Password is incorrect"),
            _ => err,
        })?;
    if staff.totp_enabled() {
        return Err(ApiError::conflict(
            "Two-factor authentication is already enabled",
        ));
    }

    let secret = auth::generate_totp_secret();
    let key = state.config.redis_key(&format!("auth:totp:pending:{}", staff.id));
    let mut redis = state.redis.clone();
    let _: () = redis.set_ex(key, &secret, TOTP_PENDING_TTL_SECS).await?;

    let issuer = state.db.totp_issuer().await?;
    let otpauth_url = auth::totp_otpauth_url(&secret, &staff.email, &issuer);
    Ok(Json(json!({
        "secret": secret,
        "otpauth_url": otpauth_url,
    })))
}

#[derive(Debug, Deserialize)]
struct TotpCodeRequest {
    code: String,
}

async fn totp_enable(
    Extension(current): Extension<CurrentStaff>,
    State(state): State<AppState>,
    client_ip: ClientIp,
    Json(body): Json<TotpCodeRequest>,
) -> Result<Json<Value>, ApiError> {
    let Some(staff) = state.db.find_staff_by_id(&current.id).await? else {
        return Err(ApiError::unauthorized("Unknown staff user"));
    };
    if staff.totp_enabled() {
        return Err(ApiError::conflict(
            "Two-factor authentication is already enabled",
        ));
    }

    let key = state.config.redis_key(&format!("auth:totp:pending:{}", staff.id));
    let mut redis = state.redis.clone();
    let secret: Option<String> = redis.get(&key).await?;
    let Some(secret) = secret else {
        return Err(ApiError::bad_request(
            "No pending 2FA setup — generate a QR code first",
        ));
    };

    // 400, never 401: on authed routes the dashboard treats 401 as an
    // expired session and would log the user out over a typo.
    rate_limit::check_failures(&state, rate_limit::TOTP_FAILURES, &staff.id).await?;
    if verify_totp_code(&state, &staff.id, &secret, &body.code)
        .await?
        .is_none()
    {
        rate_limit::record_failure(&state, rate_limit::TOTP_FAILURES, &staff.id).await?;
        return Err(ApiError::bad_request("Invalid authentication code"));
    }

    let Some(row) = state.db.enable_staff_totp(&staff.id, &secret).await? else {
        return Err(ApiError::unauthorized("Unknown staff user"));
    };
    let _: () = redis.del(&key).await?;
    jobs::audit::record(
        &state,
        &staff_actor(&current, &client_ip),
        "auth.totp_enabled",
        Some(("staff", &row.id)),
        json!({}),
    )
    .await;
    Ok(Json(staff_to_json(&row)))
}

#[derive(Debug, Deserialize)]
struct TotpDisableRequest {
    password: String,
    code: String,
}

/// Needs both the password and a code (rate-limited): a stolen session
/// cannot strip the second factor.
async fn totp_disable(
    Extension(current): Extension<CurrentStaff>,
    State(state): State<AppState>,
    client_ip: ClientIp,
    Json(body): Json<TotpDisableRequest>,
) -> Result<Json<Value>, ApiError> {
    let Some(staff) = state.db.find_staff_by_id(&current.id).await? else {
        return Err(ApiError::unauthorized("Unknown staff user"));
    };
    if !staff.totp_enabled() {
        return Err(ApiError::bad_request(
            "Two-factor authentication is not enabled",
        ));
    }
    verify_current_password(&state, &staff, &body.password)
        .await
        .map_err(|err| match err.status {
            StatusCode::BAD_REQUEST => ApiError::bad_request("Password is incorrect"),
            _ => err,
        })?;
    require_step_up(&state, &staff.id, Some(&body.code))
        .await
        .map_err(|err| match err.status {
            StatusCode::BAD_REQUEST => ApiError::bad_request("Invalid authentication code"),
            _ => err,
        })?;

    let Some(row) = state.db.clear_staff_totp(&staff.id).await? else {
        return Err(ApiError::unauthorized("Unknown staff user"));
    };
    jobs::audit::record(
        &state,
        &staff_actor(&current, &client_ip),
        "auth.totp_disabled",
        Some(("staff", &row.id)),
        json!({}),
    )
    .await;
    Ok(Json(staff_to_json(&row)))
}

async fn list_staff(
    State(state): State<AppState>,
    Query(query): Query<ListQuery>,
) -> Result<Json<Value>, ApiError> {
    let limit = query.limit.unwrap_or(50).clamp(1, 100);
    let offset = query.offset.unwrap_or(0).max(0);
    let (rows, total) = state.db.list_staff(limit, offset).await?;
    Ok(Json(json!({
        "data": rows.iter().map(staff_to_json).collect::<Vec<_>>(),
        "total": total,
        "limit": limit,
        "offset": offset,
    })))
}

#[derive(Debug, Deserialize)]
struct CreateStaffRequest {
    email: String,
    password: String,
    first_name: String,
    last_name: String,
    role: String,
    totp_code: Option<String>,
}

async fn create_staff(
    State(state): State<AppState>,
    Extension(current): Extension<CurrentStaff>,
    client_ip: ClientIp,
    Json(body): Json<CreateStaffRequest>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    require_step_up(&state, &current.id, body.totp_code.as_deref()).await?;
    let email = validate_email(&body.email)?;
    let (first_name, last_name) = validate_names(&body.first_name, &body.last_name)?;
    validate_password(&body.password)?;
    let role = StaffRole::parse_api(&body.role)
        .map_err(|_| ApiError::bad_request("Invalid role: expected admin or operator"))?;

    if state.db.find_staff_by_email(&email).await?.is_some() {
        return Err(ApiError::conflict("A staff user with this email already exists"));
    }

    let password_hash =
        auth::hash_password(&body.password).map_err(|e| ApiError::internal(e.to_string()))?;
    let row = state
        .db
        .create_staff_user(&email, &first_name, &last_name, &password_hash, role)
        .await?;
    jobs::audit::record(
        &state,
        &staff_actor(&current, &client_ip),
        "staff.created",
        Some(("staff", &row.id)),
        json!({ "email": row.email, "role": role.as_api() }),
    )
    .await;
    Ok((StatusCode::CREATED, Json(staff_to_json(&row))))
}

#[derive(Debug, Deserialize)]
struct UpdateStaffRequest {
    first_name: Option<String>,
    last_name: Option<String>,
    role: Option<String>,
    is_active: Option<bool>,
    password: Option<String>,
    /// Required when `role`, `is_active` or `password` is present.
    totp_code: Option<String>,
}

async fn update_staff(
    State(state): State<AppState>,
    Extension(current): Extension<CurrentStaff>,
    client_ip: ClientIp,
    Path(id): Path<String>,
    Json(body): Json<UpdateStaffRequest>,
) -> Result<Json<Value>, ApiError> {
    let Some(target) = state.db.find_staff_by_id(&id).await? else {
        return Err(ApiError::not_found(format!("Staff user not found: {id}")));
    };

    let role = body
        .role
        .as_deref()
        .map(StaffRole::parse_api)
        .transpose()
        .map_err(|_| ApiError::bad_request("Invalid role: expected admin or operator"))?;

    if target.id == current.id && body.is_active == Some(false) {
        return Err(ApiError::bad_request("You cannot deactivate your own account"));
    }

    // Only non-admin staff can be deactivated; demote an admin first.
    if body.is_active == Some(false) && target.role == StaffRole::Admin.as_db() {
        return Err(ApiError::bad_request(
            "Admin accounts cannot be deactivated — demote them to operator first",
        ));
    }

    let sensitive = body.role.is_some() || body.is_active.is_some() || body.password.is_some();
    if sensitive {
        require_step_up(&state, &current.id, body.totp_code.as_deref()).await?;
    }

    let password_hash = match body.password.as_deref() {
        Some(password) => {
            validate_password(password)?;
            Some(auth::hash_password(password).map_err(|e| ApiError::internal(e.to_string()))?)
        }
        None => None,
    };

    let (first_name, last_name) = (
        body.first_name.as_deref().map(str::trim),
        body.last_name.as_deref().map(str::trim),
    );
    if first_name == Some("") || last_name == Some("") {
        return Err(ApiError::bad_request("Names cannot be empty"));
    }

    // The last-active-admin rule is checked atomically with the write.
    let row = match state
        .db
        .update_staff(
            &id,
            first_name,
            last_name,
            role,
            body.is_active,
            password_hash.as_deref(),
        )
        .await?
    {
        StaffUpdate::Updated(row) => *row,
        StaffUpdate::NotFound => {
            return Err(ApiError::not_found(format!("Staff user not found: {id}")))
        }
        StaffUpdate::LastActiveAdmin => {
            return Err(ApiError::conflict(
                "Cannot demote or deactivate the last active admin",
            ))
        }
    };

    // Blocking must be immediate: every session of the blocked staff is
    // revoked (access tokens included — they are checked against their
    // session), and so are their API keys (revocation is permanent —
    // reactivating the account does not resurrect them). A password set by
    // an admin also ends the member's sessions.
    if body.is_active == Some(false) {
        session::revoke_all_sessions(&state, &row.id).await?;
        state.db.revoke_api_keys_created_by(&row.id).await?;
    } else if password_hash.is_some() {
        session::revoke_all_sessions(&state, &row.id).await?;
    }

    jobs::audit::record(
        &state,
        &staff_actor(&current, &client_ip),
        "staff.updated",
        Some(("staff", &row.id)),
        json!({
            "role": role.map(|r| r.as_api()),
            "is_active": body.is_active,
            "password_changed": password_hash.is_some(),
            "name_changed": first_name.is_some() || last_name.is_some(),
        }),
    )
    .await;
    Ok(Json(staff_to_json(&row)))
}

#[derive(Debug, Deserialize)]
struct StepUpRequest {
    totp_code: Option<String>,
}

/// Recovery path for a lost authenticator: an admin removes another staff
/// member's 2FA. Their sessions are revoked too — a reset usually means the
/// device is lost or compromised. Admins cannot reset their own 2FA here
/// (that would bypass the code-required disable on the Account page).
async fn reset_staff_totp(
    Extension(current): Extension<CurrentStaff>,
    State(state): State<AppState>,
    client_ip: ClientIp,
    Path(id): Path<String>,
    Json(body): Json<StepUpRequest>,
) -> Result<Json<Value>, ApiError> {
    if id == current.id {
        return Err(ApiError::bad_request(
            "Use the Account page to disable your own 2FA",
        ));
    }
    require_step_up(&state, &current.id, body.totp_code.as_deref()).await?;
    let Some(staff) = state.db.find_staff_by_id(&id).await? else {
        return Err(ApiError::not_found(format!("Staff user not found: {id}")));
    };
    if !staff.totp_enabled() {
        return Err(ApiError::bad_request(
            "Two-factor authentication is not enabled for this user",
        ));
    }

    let Some(row) = state.db.clear_staff_totp(&id).await? else {
        return Err(ApiError::not_found(format!("Staff user not found: {id}")));
    };
    session::revoke_all_sessions(&state, &row.id).await?;
    jobs::audit::record(
        &state,
        &staff_actor(&current, &client_ip),
        "staff.totp_reset",
        Some(("staff", &row.id)),
        json!({}),
    )
    .await;
    Ok(Json(staff_to_json(&row)))
}

/// Display status: a key past its deadline reads `expired` even before the
/// periodic sweep has materialized the DB status.
fn api_key_display_status(status: &str, expires_at: Option<chrono::DateTime<chrono::Utc>>) -> &'static str {
    match domain::ApiKeyStatus::from_db(status) {
        Ok(domain::ApiKeyStatus::Active)
            if expires_at.is_some_and(|deadline| deadline <= chrono::Utc::now()) =>
        {
            domain::ApiKeyStatus::Expired.as_api()
        }
        Ok(status) => status.as_api(),
        Err(_) => "active",
    }
}

async fn list_api_keys(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let rows = state.db.list_api_keys().await?;
    let data = rows
        .iter()
        .map(|k| {
            json!({
                "id": k.id,
                "name": k.name,
                "prefix": k.prefix,
                "status": api_key_display_status(&k.status, k.expires_at),
                "created_by": k.created_by_name,
                "last_used_at": k.last_used_at.map(|t| t.to_rfc3339()),
                "revoked_at": k.revoked_at.map(|t| t.to_rfc3339()),
                "expires_at": k.expires_at.map(|t| t.to_rfc3339()),
                "withdrawal_policy": domain::WithdrawalPolicy::from_db(&k.withdrawal_policy).map(|p| p.as_api()).unwrap_or("none"),
                "withdrawal_tokens": k.withdrawal_tokens,
                "withdrawal_networks": k.withdrawal_networks.as_ref().map(|networks| {
                    networks.iter().filter_map(|n| domain::Network::from_db(n).ok()).map(|n| n.as_api()).collect::<Vec<_>>()
                }),
                "withdrawal_limits": limits_json(&k.withdrawal_limits),
                "created_at": k.created_at.to_rfc3339(),
            })
        })
        .collect::<Vec<_>>();
    Ok(Json(json!({ "data": data })))
}

/// `[{token, max_per_withdrawal_raw, daily_max_raw}]` rendered with the
/// token amounts next to the raw ones.
fn limits_json(limits: &Value) -> Value {
    let rows = limits.as_array().cloned().unwrap_or_default();
    Value::Array(
        rows.into_iter()
            .map(|row| {
                let token = row["token"].as_str().and_then(|t| domain::Token::from_db(t).ok());
                let render = |raw: &Value| {
                    match (token, raw.as_str().and_then(|r| r.parse::<u128>().ok())) {
                        (Some(token), Some(raw)) => json!(domain::raw_u128_to_string(raw, token)),
                        _ => Value::Null,
                    }
                };
                json!({
                    "token": token.map(|t| t.as_api()),
                    "max_per_withdrawal": render(&row["max_per_withdrawal_raw"]),
                    "max_per_withdrawal_raw": row["max_per_withdrawal_raw"],
                    "daily_max": render(&row["daily_max_raw"]),
                    "daily_max_raw": row["daily_max_raw"],
                })
            })
            .collect(),
    )
}

#[derive(Debug, Deserialize)]
struct CreateApiKeyRequest {
    name: String,
    /// Days until expiry — one of 30/90/180/365; absent or null = no expiry
    /// (a key that may withdraw must expire). New keys cannot withdraw: the
    /// grant is set afterwards (`PATCH /api-keys/{id}`, TOTP).
    expires_in_days: Option<i64>,
}

/// Longest API key name.
const API_KEY_NAME_MAX_CHARS: usize = 100;

async fn create_api_key(
    State(state): State<AppState>,
    Extension(current): Extension<CurrentStaff>,
    client_ip: ClientIp,
    Json(body): Json<CreateApiKeyRequest>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let name = body.name.trim();
    if name.is_empty() || name.chars().count() > API_KEY_NAME_MAX_CHARS {
        return Err(ApiError::bad_request("Name is required (at most 100 characters)"));
    }
    let expires_at = match body.expires_in_days {
        None => None,
        Some(days) if config::API_KEY_EXPIRY_ALLOWED_DAYS.contains(&days) => {
            Some(Utc::now() + chrono::Duration::days(days))
        }
        Some(_) => {
            return Err(ApiError::bad_request(
                "expires_in_days must be one of 30, 90, 180, 365 (omit for no expiry)",
            ))
        }
    };

    let tag = state.db.api_key_prefix().await?;
    let material = auth::generate_api_key(tag.as_deref());
    let row = state
        .db
        .create_api_key(
            name,
            &material.prefix,
            &material.sha256_hex,
            &current.id,
            expires_at,
        )
        .await?;

    jobs::audit::record(
        &state,
        &staff_actor(&current, &client_ip),
        "api_key.created",
        Some(("api_key", &row.id)),
        json!({
            "name": row.name,
            "prefix": row.prefix,
            "expires_at": row.expires_at.map(|t| t.to_rfc3339()),
        }),
    )
    .await;

    // `key` is shown exactly once — only its hash is stored.
    Ok((
        StatusCode::CREATED,
        Json(json!({
            "id": row.id,
            "name": row.name,
            "prefix": row.prefix,
            "key": material.full_key,
            "status": api_key_display_status(&row.status, row.expires_at),
            "expires_at": row.expires_at.map(|t| t.to_rfc3339()),
            "withdrawal_policy": "none",
            "withdrawal_tokens": Vec::<String>::new(),
            "withdrawal_networks": Value::Null,
            "withdrawal_limits": Vec::<Value>::new(),
            "created_at": row.created_at.to_rfc3339(),
        })),
    ))
}

#[derive(Debug, Deserialize)]
struct WithdrawalLimitRequest {
    token: String,
    /// Token amounts (decimal strings); `0` disables.
    max_per_withdrawal: String,
    daily_max: String,
}

#[derive(Debug, Deserialize)]
struct UpdateApiKeyRequest {
    /// `none` / `book` / `open`.
    withdrawal_policy: String,
    /// Subset of USDT, BTC, ETH.
    #[serde(default)]
    withdrawal_tokens: Vec<String>,
    /// `null` = every network of the allowed tokens.
    #[serde(default)]
    withdrawal_networks: Option<Vec<String>>,
    /// Mandatory for every token of an `open` key.
    #[serde(default)]
    withdrawal_limits: Vec<WithdrawalLimitRequest>,
    /// Required when granting or raising anything (only a downgrade to
    /// `none` needs none).
    totp_code: Option<String>,
}

/// Validate a withdrawal grant (no I/O).
fn validate_grant(body: &UpdateApiKeyRequest) -> Result<db::WithdrawalGrant, ApiError> {
    use domain::{Network, Token, WithdrawalPolicy};
    let policy = WithdrawalPolicy::parse_api(&body.withdrawal_policy)
        .map_err(|_| ApiError::bad_request("withdrawal_policy must be none, book or open"))?;
    if policy == WithdrawalPolicy::None {
        return Ok(db::WithdrawalGrant { policy, tokens: vec![], networks: None, limits: vec![] });
    }
    let mut tokens = Vec::new();
    for token in &body.withdrawal_tokens {
        let token = Token::parse_api(token).map_err(|_| ApiError::bad_request("Invalid withdrawal token"))?;
        if token.is_gas_only() {
            return Err(ApiError::bad_request("Gas assets (TRX, SOL) can only be withdrawn by an admin"));
        }
        if !tokens.contains(&token) {
            tokens.push(token);
        }
    }
    if tokens.is_empty() {
        return Err(ApiError::bad_request("withdrawal_tokens is required (USDT, BTC, ETH)"));
    }
    let networks = match &body.withdrawal_networks {
        None => None,
        Some(list) => {
            let mut networks = Vec::new();
            for network in list {
                let network = Network::parse_api(network).map_err(|_| ApiError::bad_request("Invalid withdrawal network"))?;
                if !tokens.iter().any(|t| domain::is_withdrawable(network, *t)) {
                    return Err(ApiError::new(
                        StatusCode::BAD_REQUEST,
                        format!("None of the allowed tokens is withdrawn on {}", network.as_api()),
                    ));
                }
                if !networks.contains(&network) {
                    networks.push(network);
                }
            }
            if networks.is_empty() {
                return Err(ApiError::bad_request("withdrawal_networks cannot be empty (null = all)"));
            }
            Some(networks)
        }
    };
    let mut limits = Vec::new();
    for limit in &body.withdrawal_limits {
        let token = Token::parse_api(&limit.token).map_err(|_| ApiError::bad_request("Invalid limit token"))?;
        if !tokens.contains(&token) {
            return Err(ApiError::bad_request("A limit names a token the key may not withdraw"));
        }
        if limits.iter().any(|(t, _, _)| *t == token) {
            return Err(ApiError::bad_request("One limit per token"));
        }
        limits.push((
            token,
            w::parse_limit(&limit.max_per_withdrawal, token)?,
            w::parse_limit(&limit.daily_max, token)?,
        ));
    }
    if policy == WithdrawalPolicy::Open {
        if let Some(missing) = tokens.iter().find(|t| !limits.iter().any(|(l, _, _)| l == *t)) {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                format!("An open key needs a {} limit", missing.as_api()),
            ));
        }
    }
    Ok(db::WithdrawalGrant { policy, tokens, networks, limits })
}

/// Set a key's withdrawal grant. Anything but a downgrade to `none` is a
/// step-up action, and only keys with an expiry may withdraw.
async fn update_api_key(
    State(state): State<AppState>,
    Extension(current): Extension<CurrentStaff>,
    client_ip: ClientIp,
    Path(id): Path<String>,
    Json(body): Json<UpdateApiKeyRequest>,
) -> Result<Json<Value>, ApiError> {
    let Some(key) = state.db.find_api_key(&id).await? else {
        return Err(ApiError::not_found(format!("API key not found: {id}")));
    };
    let grant = validate_grant(&body)?;
    if grant.policy != domain::WithdrawalPolicy::None {
        if key.expires_at.is_none() {
            return Err(ApiError::bad_request(
                "Only keys with an expiry can be allowed to withdraw",
            ));
        }
        require_step_up(&state, &current.id, body.totp_code.as_deref()).await?;
    }
    let before_limits = state.db.api_key_withdrawal_limits(&id).await?;
    if !state.db.set_api_key_withdrawal_grant(&id, &grant).await? {
        return Err(ApiError::not_found(format!("API key not found: {id}")));
    }
    let tokens: Vec<&str> = grant.tokens.iter().map(|t| t.as_api()).collect();
    let networks: Option<Vec<&str>> = grant.networks.as_ref().map(|n| n.iter().map(|n| n.as_api()).collect());
    let limits: Vec<Value> = grant
        .limits
        .iter()
        .map(|(token, max, daily)| json!({ "token": token.as_api(), "max_per_withdrawal_raw": max.to_string(), "daily_max_raw": daily.to_string() }))
        .collect();
    jobs::audit::record(
        &state,
        &staff_actor(&current, &client_ip),
        "api_key.withdrawal_policy_updated",
        Some(("api_key", &id)),
        json!({
            "policy": { "from": key.withdrawal_policy().as_api(), "to": grant.policy.as_api() },
            "tokens": tokens,
            "networks": networks,
            "limits": limits,
            "previous_limits": before_limits.iter().map(|l| json!({ "token": l.token, "max_per_withdrawal_raw": l.max_per_withdrawal_raw.0.to_string(), "daily_max_raw": l.daily_max_raw.0.to_string() })).collect::<Vec<_>>(),
        }),
    )
    .await;
    Ok(Json(json!({
        "id": id,
        "withdrawal_policy": grant.policy.as_api(),
        "withdrawal_tokens": tokens,
        "withdrawal_networks": networks,
        "withdrawal_limits": limits,
    })))
}

async fn revoke_api_key(
    State(state): State<AppState>,
    Extension(current): Extension<CurrentStaff>,
    client_ip: ClientIp,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    if !state.db.revoke_api_key(&id).await? {
        return Err(ApiError::not_found(format!(
            "API key not found or already revoked: {id}"
        )));
    }
    jobs::audit::record(
        &state,
        &staff_actor(&current, &client_ip),
        "api_key.revoked",
        Some(("api_key", &id)),
        json!({}),
    )
    .await;
    Ok(Json(json!({ "status": "revoked", "id": id })))
}

async fn get_settings(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    settings_json(&state).await
}

/// Partial update: only the provided fields are validated and written.
#[derive(Debug, Deserialize)]
struct UpdateSettingsRequest {
    deposit_expiry_minutes: Option<i64>,
    api_key_prefix: Option<String>,
    totp_issuer: Option<String>,
    /// Absent = untouched; explicit `null` = keep forever.
    #[serde(default, deserialize_with = "deserialize_some")]
    api_traffic_retention_days: Option<Option<i64>>,
}

/// Distinguishes a missing JSON field (`None`) from an explicit `null`
/// (`Some(None)`) — serde collapses both to `None` on a plain `Option`.
pub(crate) fn deserialize_some<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}

/// Keep the otpauth label reasonable — authenticator apps truncate long ones.
const TOTP_ISSUER_MAX_LEN: usize = 40;

async fn update_settings(
    State(state): State<AppState>,
    Extension(current): Extension<CurrentStaff>,
    client_ip: ClientIp,
    Json(body): Json<UpdateSettingsRequest>,
) -> Result<Json<Value>, ApiError> {
    // Withdrawal limits live in `/settings/assets` (TOTP).
    let before = settings_json(&state).await?.0;

    if let Some(minutes) = body.deposit_expiry_minutes {
        if !(1..=config::DEPOSIT_EXPIRY_MAX_MINUTES).contains(&minutes) {
            return Err(ApiError::bad_request(
                "deposit_expiry_minutes must be between 1 and 1440",
            ));
        }
        state
            .db
            .set_setting(db::DEPOSIT_EXPIRY_SETTING, &minutes.to_string())
            .await?;
    }

    if let Some(prefix) = body.api_key_prefix.as_deref() {
        let prefix = prefix.trim();
        // Empty string clears the tag; otherwise 1..=5 alphanumeric chars
        // ('_' is the key-format separator, so it can never appear here).
        let valid = prefix.is_empty()
            || (prefix.len() <= auth::API_KEY_TAG_MAX_LEN
                && prefix.chars().all(|c| c.is_ascii_alphanumeric()));
        if !valid {
            return Err(ApiError::bad_request(
                "api_key_prefix must be 1-5 alphanumeric characters (empty to disable)",
            ));
        }
        state.db.set_setting(db::API_KEY_PREFIX_SETTING, prefix).await?;
    }

    if let Some(issuer) = body.totp_issuer.as_deref() {
        // Empty string resets to the code default; the stored value only
        // affects NEW enrollments (existing authenticator entries keep the
        // label they were created with).
        let issuer = issuer.trim();
        if issuer.chars().count() > TOTP_ISSUER_MAX_LEN {
            return Err(ApiError::bad_request(
                "totp_issuer must be at most 40 characters (empty to reset)",
            ));
        }
        state.db.set_setting(db::TOTP_ISSUER_SETTING, issuer).await?;
    }

    if let Some(retention) = body.api_traffic_retention_days {
        let value = match retention {
            None => db::API_TRAFFIC_RETENTION_FOREVER.to_string(),
            Some(days) if (1..=config::API_TRAFFIC_RETENTION_MAX_DAYS).contains(&days) => {
                days.to_string()
            }
            Some(_) => {
                return Err(ApiError::bad_request(
                    "api_traffic_retention_days must be between 1 and 3650, or null to keep forever",
                ))
            }
        };
        state
            .db
            .set_setting(db::API_TRAFFIC_RETENTION_SETTING, &value)
            .await?;
    }

    let after = settings_json(&state).await?;
    jobs::audit::record(
        &state,
        &staff_actor(&current, &client_ip),
        "settings.updated",
        None,
        json!({ "before": before, "after": after.0 }),
    )
    .await;
    Ok(after)
}

async fn settings_json(state: &AppState) -> Result<Json<Value>, ApiError> {
    let minutes = state.db.deposit_expiry_minutes().await?;
    let api_key_prefix = state.db.api_key_prefix().await?;
    let totp_issuer = state.db.totp_issuer().await?;
    let api_traffic_retention_days = state.db.api_traffic_retention_days().await?;
    Ok(Json(json!({
        "deposit_expiry_minutes": minutes,
        "api_key_prefix": api_key_prefix,
        "totp_issuer": totp_issuer,
        // null = indefinite (never purged).
        "api_traffic_retention_days": api_traffic_retention_days,
    })))
}

#[derive(Debug, Deserialize)]
struct TrafficOverviewQuery {
    surface: Option<String>,
    window: Option<String>,
    top_limit: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct TrafficRequestsQuery {
    surface: Option<String>,
    status: Option<String>,
    route: Option<String>,
    method: Option<String>,
    api_key_id: Option<String>,
    limit: Option<i64>,
    offset: Option<i64>,
}

/// `merchant` (default) / `admin` / `all` (= no filter).
fn parse_surface(value: Option<&str>) -> Result<Option<ApiSurface>, ApiError> {
    match value.map(str::trim) {
        None | Some("") => Ok(Some(ApiSurface::Merchant)),
        Some(v) if v.eq_ignore_ascii_case("all") => Ok(None),
        Some(v) => ApiSurface::parse_api(v)
            .map(Some)
            .map_err(|_| ApiError::bad_request("surface must be merchant, admin or all")),
    }
}

fn parse_window(value: Option<&str>) -> Result<TrafficWindow, ApiError> {
    match value.map(str::trim) {
        None | Some("") => Ok(TrafficWindow::M15),
        Some(v) => TrafficWindow::parse_api(v)
            .map_err(|_| ApiError::bad_request("window must be 15m, 1h or 24h")),
    }
}

fn ms(value: Option<f64>) -> Value {
    value.map(|v| json!((v * 100.0).round() / 100.0)).unwrap_or(Value::Null)
}

async fn traffic_overview(
    State(state): State<AppState>,
    Query(query): Query<TrafficOverviewQuery>,
) -> Result<Json<Value>, ApiError> {
    let surface = parse_surface(query.surface.as_deref())?;
    let window = parse_window(query.window.as_deref())?;
    let top_limit = query.top_limit.unwrap_or(10).clamp(1, 25);
    let secs = window.secs();

    let (summary, timeseries, top_routes, top_keys) = tokio::join!(
        state.db.traffic_summary(surface, secs),
        state.db.traffic_timeseries(surface, secs, window.bucket_secs()),
        state.db.traffic_top_routes(surface, secs, top_limit),
        state.db.traffic_top_api_keys(secs, top_limit),
    );
    let summary = summary?;
    let timeseries = timeseries?;
    let top_routes = top_routes?;
    // Keys only exist on the merchant surface; an admin-only view gets none.
    let top_keys = if surface == Some(ApiSurface::Admin) {
        Vec::new()
    } else {
        top_keys?
    };

    let errors = summary.client_errors + summary.server_errors;
    let error_rate = if summary.total > 0 {
        errors as f64 / summary.total as f64
    } else {
        0.0
    };
    let per_minute_avg = summary.total as f64 / (secs as f64 / 60.0);

    Ok(Json(json!({
        "surface": surface.map(|s| s.as_api()).unwrap_or("all"),
        "window": window.as_api(),
        "bucket_seconds": window.bucket_secs(),
        "summary": {
            "requests_total": summary.total,
            "requests_last_minute": summary.last_minute,
            "requests_per_minute_avg": (per_minute_avg * 100.0).round() / 100.0,
            "error_rate": (error_rate * 10_000.0).round() / 10_000.0,
            "client_errors": summary.client_errors,
            "server_errors": summary.server_errors,
            "p50_ms": ms(summary.p50_ms),
            "p95_ms": ms(summary.p95_ms),
            "active_keys": summary.active_keys,
            "dropped_events": state.traffic.dropped(),
        },
        "timeseries": timeseries.iter().map(|b| json!({
            "bucket": b.bucket.to_rfc3339(),
            "ok": b.ok,
            "client_error": b.client_error,
            "server_error": b.server_error,
            "p95_ms": ms(b.p95_ms),
        })).collect::<Vec<_>>(),
        "top_routes": top_routes.iter().map(|r| json!({
            "method": r.method,
            "route": r.route,
            "count": r.count,
            "client_errors": r.client_errors,
            "server_errors": r.server_errors,
            "p95_ms": ms(r.p95_ms),
        })).collect::<Vec<_>>(),
        "top_api_keys": top_keys.iter().map(|k| json!({
            // null = requests that carried no valid key.
            "api_key_id": k.api_key_id,
            "name": k.name,
            "prefix": k.prefix,
            "status": k.status.as_deref().map(|s| {
                domain::ApiKeyStatus::from_db(s).map(|s| s.as_api()).unwrap_or("active")
            }),
            "count": k.count,
            "client_errors": k.client_errors,
            "server_errors": k.server_errors,
            "p95_ms": ms(k.p95_ms),
            "last_seen_at": k.last_seen_at.to_rfc3339(),
        })).collect::<Vec<_>>(),
    })))
}

async fn traffic_requests(
    State(state): State<AppState>,
    Query(query): Query<TrafficRequestsQuery>,
) -> Result<Json<Value>, ApiError> {
    let status_class = match query.status.as_deref().map(str::trim) {
        None | Some("") | Some("all") => None,
        Some(v) => Some(
            StatusClass::parse_api(v)
                .map_err(|_| ApiError::bad_request("status must be 2xx, 3xx, 4xx or 5xx"))?,
        ),
    };
    let non_empty = |v: Option<String>| v.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    let filter = ApiRequestFilter {
        surface: parse_surface(query.surface.as_deref())?,
        status_class,
        route: non_empty(query.route),
        method: non_empty(query.method).map(|m| m.to_ascii_uppercase()),
        api_key_id: non_empty(query.api_key_id),
    };
    let limit = query.limit.unwrap_or(50).clamp(1, 100);
    let offset = query.offset.unwrap_or(0).max(0);

    let (rows, total) = state.db.list_api_requests(&filter, limit, offset).await?;
    Ok(Json(json!({
        "data": rows.iter().map(request_to_json).collect::<Vec<_>>(),
        "total": total,
        "limit": limit,
        "offset": offset,
    })))
}

fn request_to_json(r: &ApiRequestRow) -> Value {
    json!({
        "id": r.id,
        "surface": ApiSurface::from_db(&r.surface).map(|s| s.as_api()).unwrap_or("merchant"),
        "method": r.method,
        "route": r.route,
        "status": r.status,
        "duration_ms": (r.duration_ms * 100.0).round() / 100.0,
        "api_key": r.api_key_id.as_ref().map(|id| json!({
            "id": id,
            "name": r.api_key_name,
            "prefix": r.api_key_prefix,
        })),
        "staff": r.staff_id.as_ref().map(|id| json!({
            "id": id,
            "name": r.staff_name,
        })),
        "ip": r.ip,
        "created_at": r.created_at.to_rfc3339(),
    })
}

/// API values ("pending"/"paid"/"expired") of an endpoint's subscriptions.
fn events_to_api(events: &[String]) -> Vec<&'static str> {
    events
        .iter()
        .filter_map(|e| WebhookEventType::from_db(e).ok())
        .map(|e| e.as_api())
        .collect()
}

/// Parse and dedupe a request's `events` list; at least one valid event is
/// required.
fn parse_events(events: &[String]) -> Result<Vec<WebhookEventType>, ApiError> {
    let mut parsed: Vec<WebhookEventType> = Vec::new();
    for value in events {
        let event = WebhookEventType::parse_api(value).map_err(|_| {
            ApiError::new(
                StatusCode::BAD_REQUEST,
                format!(
                    "Invalid event: expected one of {}",
                    WebhookEventType::ALL.iter().map(|e| e.as_api()).collect::<Vec<_>>().join(", ")
                ),
            )
        })?;
        if !parsed.contains(&event) {
            parsed.push(event);
        }
    }
    if parsed.is_empty() {
        return Err(ApiError::bad_request("At least one event is required"));
    }
    Ok(parsed)
}

/// Validate a signing secret: 16–128 printable ASCII chars without
/// whitespace. Mandatory — deliveries are always signed (#9).
fn validate_webhook_secret(secret: &str) -> Result<String, ApiError> {
    let secret = secret.trim();
    if secret.is_empty() {
        return Err(ApiError::bad_request("The signing secret cannot be removed"));
    }
    let valid = (16..=128).contains(&secret.len())
        && secret.chars().all(|c| c.is_ascii_graphic());
    if !valid {
        return Err(ApiError::bad_request(
            "Webhook secret must be 16-128 printable ASCII characters without spaces",
        ));
    }
    Ok(secret.to_string())
}

/// URL rules of the delivery client (https, no credentials, public host —
/// relaxed in development) plus a DNS check, so the admin learns at once.
async fn validate_webhook_url(state: &AppState, url: &str) -> Result<String, ApiError> {
    let url = url.trim();
    if url.len() > 2048 {
        return Err(ApiError::bad_request("Webhook URL is too long"));
    }
    let allow_private = state.webhooks.allows_private();
    let parsed = jobs::validate_webhook_url(url, allow_private)
        .map_err(|reason| ApiError::new(StatusCode::BAD_REQUEST, reason))?;
    jobs::check_webhook_host_resolves_publicly(&parsed, allow_private)
        .await
        .map_err(|reason| ApiError::new(StatusCode::BAD_REQUEST, reason))?;
    Ok(url.to_string())
}

async fn list_webhook_endpoints(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let rows = state.db.list_webhook_endpoints().await?;
    let data = rows
        .iter()
        .map(|e| {
            json!({
                "id": e.id,
                "url": e.url,
                "is_active": e.is_active,
                "events": events_to_api(&e.events),
                "has_secret": e.has_secret,
                "created_by": e.created_by_name,
                "created_at": e.created_at.to_rfc3339(),
                "updated_at": e.updated_at.to_rfc3339(),
            })
        })
        .collect::<Vec<_>>();
    Ok(Json(json!({ "data": data })))
}

#[derive(Debug, Deserialize)]
struct CreateWebhookEndpointRequest {
    url: String,
    /// Subscribed events; absent = paid only (pre-subscription behavior).
    events: Option<Vec<String>>,
    /// Signing secret for X-Webhook-Signature(-V2) — required, write-only.
    secret: Option<String>,
}

async fn create_webhook_endpoint(
    State(state): State<AppState>,
    Extension(current): Extension<CurrentStaff>,
    client_ip: ClientIp,
    Json(body): Json<CreateWebhookEndpointRequest>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let events = match body.events.as_deref() {
        Some(events) => parse_events(events)?,
        None => vec![WebhookEventType::Paid],
    };
    let secret = validate_webhook_secret(body.secret.as_deref().unwrap_or_default())?;
    let url = validate_webhook_url(&state, &body.url).await?;
    let secret = Some(secret);

    if state.db.find_webhook_endpoint_by_url(&url).await?.is_some() {
        return Err(ApiError::conflict(
            "A webhook endpoint with this URL already exists",
        ));
    }

    let row = state
        .db
        .create_webhook_endpoint(&url, &events, secret.as_deref(), &current.id)
        .await?;
    jobs::audit::record(
        &state,
        &staff_actor(&current, &client_ip),
        "webhook_endpoint.created",
        Some(("webhook_endpoint", &row.id)),
        json!({ "url": row.url, "events": events_to_api(&row.events) }),
    )
    .await;
    Ok((
        StatusCode::CREATED,
        Json(json!({
            "id": row.id,
            "url": row.url,
            "is_active": row.is_active,
            "events": events_to_api(&row.events),
            "has_secret": row.has_secret(),
            "created_at": row.created_at.to_rfc3339(),
            "updated_at": row.updated_at.to_rfc3339(),
        })),
    ))
}

#[derive(Debug, Deserialize)]
struct UpdateWebhookEndpointRequest {
    is_active: Option<bool>,
    events: Option<Vec<String>>,
    /// Present = rotate (signing can no longer be removed).
    secret: Option<String>,
}

async fn update_webhook_endpoint(
    State(state): State<AppState>,
    Extension(current): Extension<CurrentStaff>,
    client_ip: ClientIp,
    Path(id): Path<String>,
    Json(body): Json<UpdateWebhookEndpointRequest>,
) -> Result<Json<Value>, ApiError> {
    if body.is_active.is_none() && body.events.is_none() && body.secret.is_none() {
        return Err(ApiError::bad_request("Nothing to update"));
    }
    // Validate everything before writing anything.
    let events = body.events.as_deref().map(parse_events).transpose()?;
    let secret = body.secret.as_deref().map(validate_webhook_secret).transpose()?;

    let mut row = None;
    if let Some(events) = events.as_deref() {
        row = state.db.set_webhook_endpoint_events(&id, events).await?;
        if row.is_none() {
            return Err(ApiError::not_found(format!(
                "Webhook endpoint not found: {id}"
            )));
        }
    }
    if let Some(secret) = secret.as_deref() {
        row = state
            .db
            .set_webhook_endpoint_secret(&id, Some(secret))
            .await?;
        if row.is_none() {
            return Err(ApiError::not_found(format!(
                "Webhook endpoint not found: {id}"
            )));
        }
    }
    if let Some(is_active) = body.is_active {
        row = state.db.set_webhook_endpoint_active(&id, is_active).await?;
    }
    let Some(row) = row else {
        return Err(ApiError::not_found(format!(
            "Webhook endpoint not found: {id}"
        )));
    };
    jobs::audit::record(
        &state,
        &staff_actor(&current, &client_ip),
        "webhook_endpoint.updated",
        Some(("webhook_endpoint", &row.id)),
        json!({
            "is_active": body.is_active,
            "events": events.as_ref().map(|e| e.iter().map(|e| e.as_api()).collect::<Vec<_>>()),
            "secret_rotated": secret.is_some(),
        }),
    )
    .await;
    Ok(Json(json!({
        "id": row.id,
        "url": row.url,
        "is_active": row.is_active,
        "events": events_to_api(&row.events),
        "has_secret": row.has_secret(),
        "updated_at": row.updated_at.to_rfc3339(),
    })))
}

async fn delete_webhook_endpoint(
    State(state): State<AppState>,
    Extension(current): Extension<CurrentStaff>,
    client_ip: ClientIp,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    match state.db.delete_webhook_endpoint_if_unused(&id).await? {
        None => Err(ApiError::not_found(format!(
            "Webhook endpoint not found: {id}"
        ))),
        Some(false) => Err(ApiError::conflict(
            "This endpoint has delivery history — deactivate it instead",
        )),
        Some(true) => {
            jobs::audit::record(
                &state,
                &staff_actor(&current, &client_ip),
                "webhook_endpoint.deleted",
                Some(("webhook_endpoint", &id)),
                json!({}),
            )
            .await;
            Ok(Json(json!({ "status": "deleted", "id": id })))
        }
    }
}

async fn list_webhooks(
    State(state): State<AppState>,
    Query(query): Query<ListQuery>,
) -> Result<Json<Value>, ApiError> {
    let status = query
        .status
        .as_deref()
        .map(WebhookDeliveryStatus::parse_api)
        .transpose()
        .map_err(ApiError::from)?;
    let limit = query.limit.unwrap_or(50).clamp(1, 100);
    let offset = query.offset.unwrap_or(0).max(0);

    let (rows, total) = state
        .db
        .list_webhook_deliveries_detailed(status, limit, offset)
        .await?;

    let data = rows
        .iter()
        .map(|w| {
            json!({
                "id": w.id,
                "event": WebhookEventType::from_db(&w.event_type).map(|e| e.as_api()).unwrap_or("paid"),
                // deposits.id — what the deposit retry endpoint expects.
                // NULL on pending/expired event deliveries (no deposit).
                "deposit_id": w.deposit_id,
                // deposit_addresses.id — what the webhook payload calls deposit_id.
                // NULL on payout deliveries.
                "deposit_address_id": w.deposit_address_id,
                // SDK payment id — payout deliveries only.
                "lightning_payment_id": w.lightning_payment_id,
                // Withdrawal / alert deliveries only.
                "withdrawal_id": w.withdrawal_id,
                "alert_id": w.alert_id,
                // NULL on deliveries from before per-endpoint tracking.
                "webhook_endpoint_id": w.webhook_endpoint_id,
                "endpoint_url": w.endpoint_url,
                "tx_hash": w.tx_hash,
                "address": w.address,
                "network": match (&w.network, &w.lightning_payment_id) {
                    (Some(network), _) => domain::Network::from_db(network).map(|n| n.as_api()).unwrap_or("unknown"),
                    (None, Some(_)) => domain::Network::Lightning.as_api(),
                    (None, None) => "unknown",
                },
                // Deposit amount for paid events, expected amount for address
                // events, the sent amount for payouts and withdrawals.
                "amount": w.amount.or(w.expected_amount)
                    .map(|a| a.normalize().to_string())
                    .or_else(|| w.payout_amount.clone())
                    .or_else(|| match (&w.withdrawal_amount_raw, &w.withdrawal_token) {
                        (Some(raw), Some(token)) => Some(domain::raw_u128_to_string(
                            raw.parse().unwrap_or(0),
                            domain::Token::from_db(token).unwrap_or(domain::Token::Usdt),
                        )),
                        _ => None,
                    }),
                "deposit_status": w.deposit_status.as_deref().map(|s| {
                    domain::DepositStatus::from_db(s).map(|s| s.as_api()).unwrap_or("detected")
                }),
                "status": WebhookDeliveryStatus::from_db(&w.status).map(|s| s.as_api()).unwrap_or("pending"),
                "attempts": w.attempts,
                "last_response": w.last_response,
                "last_error": w.last_error,
                // Body of the most recent attempt; null until one is made.
                "payload": w.last_payload,
                "created_at": w.created_at.to_rfc3339(),
                "updated_at": w.updated_at.to_rfc3339(),
            })
        })
        .collect::<Vec<_>>();

    Ok(Json(json!({
        "data": data,
        "total": total,
        "limit": limit,
        "offset": offset,
    })))
}

/// Manual replay of one delivery row — works for every event type, unlike
/// the historical per-deposit retry which is PAID-only.
async fn retry_webhook_delivery(
    State(state): State<AppState>,
    Extension(current): Extension<CurrentStaff>,
    client_ip: ClientIp,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    match retrigger_delivery(&state, &id).await {
        Ok(()) => {
            jobs::audit::record(
                &state,
                &staff_actor(&current, &client_ip),
                "webhook.retried",
                Some(("webhook_delivery", &id)),
                json!({}),
            )
            .await;
            Ok(Json(json!({ "status": "queued", "delivery_id": id })))
        }
        Err(RetriggerError::DeliveryNotFound) => Err(ApiError::not_found(format!(
            "Webhook delivery not found: {id}"
        ))),
        Err(RetriggerError::DepositNotFound) => {
            Err(ApiError::not_found("Deposit not found for this delivery".into()))
        }
        Err(RetriggerError::DepositNotConfirmed) => {
            Err(ApiError::bad_request("Deposit is not confirmed"))
        }
        Err(RetriggerError::EndpointNotEligible) => Err(ApiError::conflict(
            "Target endpoint is inactive or no longer subscribed to this event",
        )),
        Err(RetriggerError::NoActiveEndpoints) => Err(ApiError::service_unavailable(
            "No active webhook endpoints configured".into(),
        )),
        Err(RetriggerError::NothingToRetry) => {
            Err(ApiError::conflict("No failed webhook delivery to retry"))
        }
        Err(RetriggerError::Other(err)) => Err(ApiError::internal(format!("{err:#}"))),
    }
}

async fn retry_failed(
    State(state): State<AppState>,
    Extension(current): Extension<CurrentStaff>,
    client_ip: ClientIp,
) -> Result<Json<Value>, ApiError> {
    match retrigger_failed_webhooks(&state).await {
        Ok(retried) => {
            jobs::audit::record(
                &state,
                &staff_actor(&current, &client_ip),
                "webhook.retried",
                None,
                json!({ "scope": "all_failed", "retried": retried }),
            )
            .await;
            Ok(Json(json!({ "retried": retried })))
        }
        Err(RetriggerError::NoActiveEndpoints) => Err(ApiError::service_unavailable(
            "No active webhook endpoints configured".into(),
        )),
        Err(err) => Err(ApiError::internal(format!("{err:?}"))),
    }
}

async fn stats(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let s = state.db.admin_stats().await?;
    Ok(Json(json!({
        "addresses": {
            "pending": s.addresses_pending,
            "paid": s.addresses_paid,
            "expired": s.addresses_expired,
        },
        "deposits": {
            "detected": s.deposits_detected,
            "confirmed": s.deposits_confirmed,
        },
        "confirmed_volume": s.confirmed_volume.normalize().to_string(),
        "webhooks": {
            "pending": s.webhooks_pending,
            "delivered": s.webhooks_delivered,
            "failed": s.webhooks_failed,
        },
    })))
}

#[derive(Debug, Deserialize)]
struct TimeseriesQuery {
    days: Option<i32>,
}

async fn deposits_timeseries(
    State(state): State<AppState>,
    Query(query): Query<TimeseriesQuery>,
) -> Result<Json<Value>, ApiError> {
    let days = query.days.unwrap_or(90).clamp(1, 365);
    let rows = state.db.deposits_timeseries(days).await?;
    let data = rows
        .iter()
        .map(|r| {
            json!({
                "date": r.date,
                "count": r.count,
                "volume": r.volume.normalize().to_string(),
            })
        })
        .collect::<Vec<_>>();
    Ok(Json(json!({ "days": days, "data": data })))
}

#[derive(Debug, Deserialize)]
struct AuditLogQuery {
    action: Option<String>,
    limit: Option<i64>,
    offset: Option<i64>,
}

async fn list_audit_log(
    State(state): State<AppState>,
    Query(query): Query<AuditLogQuery>,
) -> Result<Json<Value>, ApiError> {
    let limit = query.limit.unwrap_or(50).clamp(1, 100);
    let offset = query.offset.unwrap_or(0).max(0);
    let action = query
        .action
        .as_deref()
        .map(str::trim)
        .filter(|a| !a.is_empty())
        .map(|a| a.chars().take(64).collect::<String>());
    let (rows, total) = state
        .db
        .list_audit_log(action.as_deref(), limit, offset)
        .await?;
    let data = rows
        .iter()
        .map(|row| {
            json!({
                "id": row.id,
                "actor_type": match row.actor_type.as_str() {
                    "STAFF" => "staff",
                    "API_KEY" => "api_key",
                    _ => "system",
                },
                "actor_id": row.actor_id,
                "actor_name": row.actor_name,
                "action": row.action,
                "target_type": row.target_type,
                "target_id": row.target_id,
                "details": row.details,
                "ip": row.ip,
                "created_at": row.created_at.to_rfc3339(),
            })
        })
        .collect::<Vec<_>>();
    Ok(Json(json!({ "data": data, "total": total, "limit": limit, "offset": offset })))
}

pub(crate) fn staff_to_json(row: &StaffUserRow) -> Value {
    json!({
        "id": row.id,
        "email": row.email,
        "first_name": row.first_name,
        "last_name": row.last_name,
        "role": StaffRole::from_db(&row.role).map(|r| r.as_api()).unwrap_or("operator"),
        "is_active": row.is_active,
        "totp_enabled": row.totp_enabled(),
        "created_at": row.created_at.to_rfc3339(),
    })
}

/// `local@domain.tld`: one `@`, no whitespace or control characters, a
/// dotted domain with non-empty labels, at most 254 characters.
fn validate_email(email: &str) -> Result<String, ApiError> {
    let email = email.trim().to_lowercase();
    let valid = email.len() <= 254
        && !email.chars().any(|c| c.is_whitespace() || c.is_control())
        && match email.split_once('@') {
            Some((local, domain)) => {
                !local.is_empty()
                    && !domain.contains('@')
                    && domain.contains('.')
                    && domain.split('.').all(|label| !label.is_empty())
            }
            None => false,
        };
    if !valid {
        return Err(ApiError::bad_request("Invalid email address"));
    }
    Ok(email)
}

fn validate_names(first_name: &str, last_name: &str) -> Result<(String, String), ApiError> {
    let (first_name, last_name) = (first_name.trim(), last_name.trim());
    if first_name.is_empty() || last_name.is_empty() {
        return Err(ApiError::bad_request("First name and last name are required"));
    }
    Ok((first_name.to_string(), last_name.to_string()))
}

/// 12–128 characters (the upper bound caps the argon2 work per attempt).
fn validate_password(password: &str) -> Result<(), ApiError> {
    let len = password.chars().count();
    if !(PASSWORD_MIN_LEN..=PASSWORD_MAX_LEN).contains(&len) {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            format!("Password must be between {PASSWORD_MIN_LEN} and {PASSWORD_MAX_LEN} characters"),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emails_need_a_dotted_domain() {
        assert_eq!(validate_email(" Alice@Example.COM ").unwrap(), "alice@example.com");
        for bad in ["ab@cd", "a@@b.c", "@b.c", "a@", "a b@c.d", "a@b..c", "a@.c"] {
            assert!(validate_email(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn passwords_are_bounded() {
        assert!(validate_password(&"x".repeat(11)).is_err());
        assert!(validate_password(&"x".repeat(12)).is_ok());
        assert!(validate_password(&"é".repeat(128)).is_ok());
        assert!(validate_password(&"x".repeat(129)).is_err());
    }

    #[test]
    fn webhook_secrets_are_mandatory() {
        assert!(validate_webhook_secret("  ").is_err());
        assert!(validate_webhook_secret("short").is_err());
        assert_eq!(validate_webhook_secret(&"s".repeat(16)).unwrap(), "s".repeat(16));
    }
}
