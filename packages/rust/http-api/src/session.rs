//! Staff sessions: refresh-token families in Redis, carried by an HttpOnly
//! cookie.
//!
//! - A login opens a *session* (`auth:session:{sid}`) with an absolute
//!   lifetime (`REFRESH_TOKEN_TTL_DAYS` from the login, never extended).
//! - Its current refresh token (`auth:refresh:{sha256}` → sid) is consumed
//!   atomically (`GETDEL`) by every refresh and replaced. Two concurrent
//!   refreshes cannot both win.
//! - A rotated token is remembered (`auth:refresh:rotated:{sha256}`): if it
//!   is presented again after a short grace (two tabs racing on the same
//!   cookie), it was stolen — the whole session is revoked.
//! - Access tokens carry the sid and `require_jwt` checks the session still
//!   exists, so logout / password change / deactivation take effect at once.
//! - `auth:staff-sessions:{staff_id}` indexes a member's sessions (revoke
//!   all, no SCAN).

use axum::http::header::{COOKIE, SET_COOKIE};
use axum::http::{HeaderMap, HeaderValue};
use chrono::Utc;
use db::StaffUserRow;
use domain::StaffRole;
use jobs::AppState;
use redis::AsyncCommands;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::ApiError;

pub const REFRESH_COOKIE: &str = "cpg_refresh";
/// Only the auth routes ever receive the cookie.
pub const REFRESH_COOKIE_PATH: &str = "/api/admin/v1/auth";
/// Cookie-authenticated routes require this header: a cross-site form or
/// simple request cannot set it (SameSite=Strict is the first line).
pub const CSRF_HEADER: &str = "x-csrf-protection";
/// A rotated token presented again within this window is a benign race
/// between tabs, not a theft.
const REUSE_GRACE_SECS: i64 = 30;

#[derive(Debug, Serialize, Deserialize)]
struct SessionRecord {
    staff_id: String,
    /// Unix seconds; the session dies at `created_at + lifetime`.
    created_at: i64,
    expires_at: i64,
    /// SHA-256 of the current refresh token.
    refresh_hash: String,
}

fn session_key(state: &AppState, sid: &str) -> String {
    state.config.redis_key(&format!("auth:session:{sid}"))
}

fn refresh_key(state: &AppState, hash: &str) -> String {
    state.config.redis_key(&format!("auth:refresh:{hash}"))
}

fn rotated_key(state: &AppState, hash: &str) -> String {
    state.config.redis_key(&format!("auth:refresh:rotated:{hash}"))
}

fn staff_sessions_key(state: &AppState, staff_id: &str) -> String {
    state.config.redis_key(&format!("auth:staff-sessions:{staff_id}"))
}

/// A freshly issued session: the JSON body (`access_token` + `staff`) and
/// the `Set-Cookie` header carrying the refresh token.
pub struct IssuedSession {
    pub body: Value,
    pub cookie: HeaderValue,
}

/// Open a new session for `staff` (login, setup, password change).
pub async fn open_session(state: &AppState, staff: &StaffUserRow) -> Result<IssuedSession, ApiError> {
    let sid = auth::generate_session_id();
    let now = Utc::now().timestamp();
    let lifetime = state.config.refresh_token_ttl_days * 86_400;
    let (token, hash) = auth::generate_refresh_token();
    let record = SessionRecord {
        staff_id: staff.id.clone(),
        created_at: now,
        expires_at: now + lifetime,
        refresh_hash: hash.clone(),
    };
    let ttl = lifetime as u64;
    let mut redis = state.redis.clone();
    let _: () = redis
        .set_ex(session_key(state, &sid), serde_json::to_string(&record)?, ttl)
        .await?;
    let _: () = redis.set_ex(refresh_key(state, &hash), &sid, ttl).await?;
    let index = staff_sessions_key(state, &staff.id);
    let _: () = redis.sadd(&index, &sid).await?;
    let _: () = redis.expire(&index, lifetime).await?;
    issue(state, staff, &sid, &token, lifetime)
}

/// Rotate the refresh token presented in the cookie. Every failure is a
/// 401 (the caller clears the cookie).
pub async fn refresh_session(state: &AppState, token: &str) -> Result<IssuedSession, ApiError> {
    let invalid = || ApiError::unauthorized("Invalid refresh token");
    let hash = auth::sha256_hex(token);
    let mut redis = state.redis.clone();

    // GET + DEL atomically (GETDEL needs Redis 6.2; a script works everywhere).
    let sid: Option<String> = redis::Script::new(
        "local v = redis.call('GET', KEYS[1]) if v then redis.call('DEL', KEYS[1]) end return v",
    )
    .key(refresh_key(state, &hash))
    .invoke_async(&mut redis)
    .await?;
    let Some(sid) = sid else {
        // Not current: a token rotated earlier is being replayed.
        let rotated: Option<String> = redis.get(rotated_key(state, &hash)).await?;
        if let Some((sid, rotated_at)) = rotated.as_deref().and_then(|v| v.split_once('|')) {
            let rotated_at: i64 = rotated_at.parse().unwrap_or(0);
            if Utc::now().timestamp() - rotated_at > REUSE_GRACE_SECS {
                tracing::warn!("Refresh token reuse detected — revoking session {sid}");
                revoke_session(state, sid).await?;
            }
        }
        return Err(invalid());
    };

    let raw: Option<String> = redis.get(session_key(state, &sid)).await?;
    let Some(mut record) = raw.and_then(|raw| serde_json::from_str::<SessionRecord>(&raw).ok())
    else {
        return Err(invalid());
    };
    if record.refresh_hash != hash {
        revoke_session(state, &sid).await?;
        return Err(invalid());
    }
    let remaining = record.expires_at - Utc::now().timestamp();
    if remaining <= 0 {
        revoke_session(state, &sid).await?;
        return Err(invalid());
    }

    let Some(staff) = state.db.find_staff_by_id(&record.staff_id).await? else {
        revoke_session(state, &sid).await?;
        return Err(invalid());
    };
    if !staff.is_active {
        revoke_all_sessions(state, &staff.id).await?;
        return Err(ApiError::unauthorized("Account is deactivated"));
    }

    let (token, new_hash) = auth::generate_refresh_token();
    record.refresh_hash = new_hash.clone();
    let ttl = remaining as u64;
    let _: () = redis
        .set_ex(session_key(state, &sid), serde_json::to_string(&record)?, ttl)
        .await?;
    let _: () = redis.set_ex(refresh_key(state, &new_hash), &sid, ttl).await?;
    let _: () = redis
        .set_ex(
            rotated_key(state, &hash),
            format!("{sid}|{}", Utc::now().timestamp()),
            ttl,
        )
        .await?;
    issue(state, &staff, &sid, &token, remaining)
}

/// Whether the session an access token belongs to is still alive.
pub async fn session_is_active(state: &AppState, sid: &str) -> Result<bool, ApiError> {
    let mut redis = state.redis.clone();
    Ok(redis.exists(session_key(state, sid)).await?)
}

/// Kill one session (logout of the presented cookie).
pub async fn revoke_session(state: &AppState, sid: &str) -> Result<(), ApiError> {
    let mut redis = state.redis.clone();
    let raw: Option<String> = redis.get(session_key(state, sid)).await?;
    if let Some(record) = raw.and_then(|raw| serde_json::from_str::<SessionRecord>(&raw).ok()) {
        let _: () = redis.del(refresh_key(state, &record.refresh_hash)).await?;
        let _: () = redis
            .srem(staff_sessions_key(state, &record.staff_id), sid)
            .await?;
    }
    let _: () = redis.del(session_key(state, sid)).await?;
    Ok(())
}

/// Kill the session a refresh token belongs to (logout).
pub async fn revoke_session_of_token(state: &AppState, token: &str) -> Result<(), ApiError> {
    let hash = auth::sha256_hex(token);
    let mut redis = state.redis.clone();
    let sid: Option<String> = redis.get(refresh_key(state, &hash)).await?;
    if let Some(sid) = sid {
        revoke_session(state, &sid).await?;
    }
    Ok(())
}

/// Kill every session of `staff_id` (deactivation, password change, 2FA
/// reset, "sign out everywhere").
pub async fn revoke_all_sessions(state: &AppState, staff_id: &str) -> Result<(), ApiError> {
    let index = staff_sessions_key(state, staff_id);
    let mut redis = state.redis.clone();
    let sids: Vec<String> = redis.smembers(&index).await?;
    for sid in sids {
        revoke_session(state, &sid).await?;
    }
    let _: () = redis.del(&index).await?;
    Ok(())
}

fn issue(
    state: &AppState,
    staff: &StaffUserRow,
    sid: &str,
    refresh_token: &str,
    max_age_secs: i64,
) -> Result<IssuedSession, ApiError> {
    let Some(secret) = state.config.jwt_secret.as_deref() else {
        return Err(ApiError::service_unavailable(
            "JWT_SECRET is not configured".into(),
        ));
    };
    let role = StaffRole::from_db(&staff.role).map_err(|e| ApiError::internal(e.to_string()))?;
    let access_token = auth::issue_access_token(
        secret,
        &staff.id,
        &staff.email,
        role,
        sid,
        state.config.access_token_ttl_minutes,
    )
    .map_err(|e| ApiError::internal(e.to_string()))?;
    Ok(IssuedSession {
        body: json!({
            "access_token": access_token,
            "staff": crate::admin::staff_to_json(staff),
        }),
        cookie: refresh_cookie(refresh_token, max_age_secs, !state.config.is_development()),
    })
}

fn refresh_cookie(value: &str, max_age_secs: i64, secure: bool) -> HeaderValue {
    let secure = if secure { "; Secure" } else { "" };
    HeaderValue::from_str(&format!(
        "{REFRESH_COOKIE}={value}; Path={REFRESH_COOKIE_PATH}; Max-Age={}; HttpOnly; SameSite=Strict{secure}",
        max_age_secs.max(0)
    ))
    .expect("cookie value is hex")
}

/// `Set-Cookie` that deletes the refresh cookie.
pub fn clear_cookie(state: &AppState) -> HeaderValue {
    refresh_cookie("", 0, !state.config.is_development())
}

/// The refresh token from the request cookies, if any.
pub fn refresh_token_from(headers: &HeaderMap) -> Option<String> {
    headers
        .get_all(COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(name, _)| *name == REFRESH_COOKIE)
        .map(|(_, value)| value.trim().to_string())
        .filter(|value| !value.is_empty() && value.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// 403 unless the CSRF header is present.
pub fn require_csrf_header(headers: &HeaderMap) -> Result<(), ApiError> {
    if headers.get(CSRF_HEADER).is_some() {
        Ok(())
    } else {
        Err(ApiError::forbidden("Missing CSRF header"))
    }
}

pub fn set_cookie_header(cookie: HeaderValue) -> [(axum::http::HeaderName, HeaderValue); 1] {
    [(SET_COOKIE, cookie)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refresh_cookie_is_http_only_strict_and_scoped() {
        let cookie = refresh_cookie("abc123", 3600, true);
        let cookie = cookie.to_str().unwrap();
        assert_eq!(
            cookie,
            "cpg_refresh=abc123; Path=/api/admin/v1/auth; Max-Age=3600; HttpOnly; SameSite=Strict; Secure"
        );
        let dev = refresh_cookie("abc123", 3600, false);
        assert!(!dev.to_str().unwrap().contains("Secure"));
        assert!(refresh_cookie("", -5, true).to_str().unwrap().contains("Max-Age=0"));
    }

    #[test]
    fn refresh_token_is_read_from_the_cookie_header_only_when_well_formed() {
        let mut headers = HeaderMap::new();
        headers.append(COOKIE, HeaderValue::from_static("theme=dark; cpg_refresh=deadbeef01"));
        assert_eq!(refresh_token_from(&headers).as_deref(), Some("deadbeef01"));

        let mut headers = HeaderMap::new();
        headers.append(COOKIE, HeaderValue::from_static("cpg_refresh=not hex!"));
        assert_eq!(refresh_token_from(&headers), None);
        assert_eq!(refresh_token_from(&HeaderMap::new()), None);
    }

    #[test]
    fn csrf_header_is_required() {
        assert!(require_csrf_header(&HeaderMap::new()).is_err());
        let mut headers = HeaderMap::new();
        headers.insert(CSRF_HEADER, HeaderValue::from_static("1"));
        assert!(require_csrf_header(&headers).is_ok());
    }
}
