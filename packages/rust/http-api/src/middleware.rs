use std::convert::Infallible;
use std::net::SocketAddr;

use axum::extract::{ConnectInfo, FromRequestParts, Request, State};
use axum::http::header::AUTHORIZATION;
use axum::http::request::Parts;
use axum::middleware::Next;
use axum::response::Response;
use domain::{Network, StaffRole, Token, WithdrawalPolicy};
use jobs::AppState;

use crate::{rate_limit, session, traffic, ApiError};

/// Authenticated staff member, inserted as a request extension by
/// [`require_jwt`]. Admin handlers act on behalf of this user. Also copied
/// into the *response* extensions so outer middleware (the traffic log) can
/// attribute the request after the handler ran.
#[derive(Debug, Clone)]
pub struct CurrentStaff {
    pub id: String,
    pub role: StaffRole,
}

/// Authenticated merchant API key, inserted into the request and response
/// extensions by [`require_api_key`]. Never carries the raw key.
#[derive(Debug, Clone)]
pub struct CurrentApiKey {
    pub id: String,
    /// The key's project: every merchant read and mutation is scoped to it.
    /// `None` only for a legacy key before the first project exists, which
    /// keeps the unscoped behaviour of before (spec §11, §13).
    pub project_id: Option<String>,
    /// What the key may withdraw to (`None` = no withdrawals).
    pub withdrawal_policy: WithdrawalPolicy,
    pub withdrawal_tokens: Vec<Token>,
    /// `None` = every network of the allowed tokens.
    pub withdrawal_networks: Option<Vec<Network>>,
}

/// The client IP, resolved like the traffic log does (forwarding headers
/// only from `TRUSTED_PROXIES`). `None` without a socket address (tests).
#[derive(Debug, Clone)]
pub struct ClientIp(pub Option<String>);

impl ClientIp {
    /// Rate-limit identity: the IP, or a shared bucket when unknown.
    pub fn bucket(&self) -> &str {
        self.0.as_deref().unwrap_or("unknown")
    }
}

impl FromRequestParts<AppState> for ClientIp {
    type Rejection = Infallible;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Infallible> {
        let peer = parts
            .extensions
            .get::<ConnectInfo<SocketAddr>>()
            .map(|info| info.0.ip());
        let header = |name: &str| parts.headers.get(name).and_then(|v| v.to_str().ok());
        Ok(Self(
            traffic::resolve_client_ip(
                peer,
                header("x-forwarded-for"),
                header("x-real-ip"),
                &state.config.trusted_proxies,
            )
            .map(|ip| ip.to_string()),
        ))
    }
}

/// Merchant-API guard: validates the `x-api-key` header against the hashed
/// `api_keys` table. Invalid keys are rate-limited per client IP, valid
/// ones per key.
pub async fn require_api_key(
    State(state): State<AppState>,
    client_ip: ClientIp,
    mut request: Request,
    next: Next,
) -> Result<Response, ApiError> {
    rate_limit::check_failures(&state, rate_limit::INVALID_API_KEY_PER_IP, client_ip.bucket())
        .await?;
    let key = request
        .headers()
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let row = match key.as_deref() {
        Some(key) => match auth::parse_api_key_prefix(key) {
            Some(prefix) => state
                .db
                .find_api_key_by_prefix(prefix)
                .await?
                .filter(|row| auth::constant_time_eq_hex(&auth::sha256_hex(key), &row.key_hash)),
            None => None,
        },
        None => None,
    };
    let Some(row) = row else {
        rate_limit::record_failure(&state, rate_limit::INVALID_API_KEY_PER_IP, client_ip.bucket())
            .await?;
        return Err(ApiError::unauthorized(if key.is_none() {
            "Missing x-api-key header"
        } else {
            "Invalid API key"
        }));
    };
    rate_limit::hit(&state, rate_limit::API_KEY_REQUESTS, &row.id).await?;

    let current = CurrentApiKey {
        id: row.id.clone(),
        project_id: row.project_id.clone(),
        withdrawal_policy: row.withdrawal_policy(),
        withdrawal_tokens: row
            .withdrawal_tokens
            .iter()
            .filter_map(|t| Token::from_db(t).ok())
            .collect(),
        withdrawal_networks: row
            .withdrawal_networks
            .as_ref()
            .map(|networks| networks.iter().filter_map(|n| Network::from_db(n).ok()).collect()),
    };

    // Fire-and-forget: never block the request on the usage timestamp.
    let db = state.db.clone();
    tokio::spawn(async move {
        if let Err(err) = db.touch_api_key_last_used(&row.id).await {
            tracing::warn!("Failed to update API key lastUsedAt: {err:#}");
        }
    });

    request.extensions_mut().insert(current.clone());
    let mut response = next.run(request).await;
    response.extensions_mut().insert(current);
    Ok(response)
}

/// Dashboard guard: validates the `Authorization: Bearer` access token,
/// checks its session is still alive (logout, password change and
/// deactivation revoke access tokens immediately) and loads the staff user.
pub async fn require_jwt(
    State(state): State<AppState>,
    mut request: Request,
    next: Next,
) -> Result<Response, ApiError> {
    let Some(secret) = state.config.jwt_secret.as_deref() else {
        return Err(ApiError::service_unavailable(
            "JWT_SECRET is not configured".into(),
        ));
    };

    let token = request
        .headers()
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or_else(|| ApiError::unauthorized("Missing bearer token"))?;

    let claims = auth::decode_access_token(secret, token)
        .map_err(|_| ApiError::unauthorized("Invalid or expired token"))?;
    if !session::session_is_active(&state, &claims.sid).await? {
        return Err(ApiError::unauthorized("Session expired"));
    }

    let Some(staff) = state.db.find_staff_by_id(&claims.sub).await? else {
        return Err(ApiError::unauthorized("Unknown staff user"));
    };
    if !staff.is_active {
        return Err(ApiError::unauthorized("Account is deactivated"));
    }
    let role = StaffRole::from_db(&staff.role)
        .map_err(|e| ApiError::internal(e.to_string()))?;

    let current = CurrentStaff { id: staff.id, role };
    request.extensions_mut().insert(current.clone());
    let mut response = next.run(request).await;
    response.extensions_mut().insert(current);
    Ok(response)
}

/// Must sit inside a [`require_jwt`] layer; rejects non-admin staff.
pub async fn require_admin(request: Request, next: Next) -> Result<Response, ApiError> {
    let staff = request
        .extensions()
        .get::<CurrentStaff>()
        .ok_or_else(|| ApiError::unauthorized("Not authenticated"))?;
    if staff.role != StaffRole::Admin {
        return Err(ApiError::forbidden("Admin role required"));
    }
    Ok(next.run(request).await)
}
