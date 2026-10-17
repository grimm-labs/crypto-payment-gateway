//! Fixed-window rate limits and failure counters, stored in Redis so they
//! hold across API instances (Kamal overlaps two containers during a
//! deploy). Errors talking to Redis fail closed (500): every limited route
//! needs Redis anyway.

use axum::http::StatusCode;
use chrono::Utc;
use jobs::AppState;
use redis::AsyncCommands;

use crate::ApiError;

/// A limit: at most `max` events per `window_secs`.
#[derive(Debug, Clone, Copy)]
pub struct Limit {
    pub scope: &'static str,
    pub max: i64,
    pub window_secs: i64,
}

// Credential endpoints (per client IP and per targeted account).
pub const LOGIN_PER_IP: Limit = Limit { scope: "login-ip", max: 20, window_secs: 15 * 60 };
pub const LOGIN_PER_ACCOUNT: Limit = Limit { scope: "login-account", max: 10, window_secs: 15 * 60 };
pub const SETUP_PER_IP: Limit = Limit { scope: "setup-ip", max: 10, window_secs: 60 * 60 };
pub const REFRESH_PER_IP: Limit = Limit { scope: "refresh-ip", max: 60, window_secs: 60 };
/// Wrong TOTP codes per staff member, across MFA tokens and step-up
/// checks (login, sensitive actions, 2FA management).
pub const TOTP_FAILURES: Limit = Limit { scope: "totp-fail", max: 10, window_secs: 15 * 60 };
/// Wrong current passwords on authenticated routes (2FA management,
/// password change) — a stolen session must not brute-force the password.
pub const PASSWORD_FAILURES: Limit = Limit { scope: "password-fail", max: 5, window_secs: 15 * 60 };

// Merchant API.
pub const API_KEY_REQUESTS: Limit = Limit { scope: "api-key", max: 600, window_secs: 60 };
pub const INVALID_API_KEY_PER_IP: Limit = Limit { scope: "api-key-invalid-ip", max: 30, window_secs: 60 };
pub const DEPOSIT_ADDRESS_CREATION: Limit = Limit { scope: "address-create", max: 120, window_secs: 60 };
/// Merchant webhook replays, per deposit.
pub const WEBHOOK_REPLAY_PER_DEPOSIT: Limit = Limit { scope: "webhook-replay", max: 1, window_secs: 60 };
/// Withdrawal creations and quotes (the Lightning payout alias included),
/// per caller.
pub const WITHDRAWALS: Limit = Limit { scope: "withdrawal", max: 10, window_secs: 60 };
/// Internal transfers, per staff member (spec §9).
pub const TRANSFERS: Limit = Limit { scope: "transfer", max: 10, window_secs: 60 };

fn window_key(state: &AppState, limit: Limit, id: &str, now: i64) -> (String, i64) {
    let window = now.div_euclid(limit.window_secs);
    let retry_after = (window + 1) * limit.window_secs - now;
    let id = auth::sha256_hex(id); // bounded key size, no raw emails in Redis
    (
        state.config.redis_key(&format!("ratelimit:{}:{id}:{window}", limit.scope)),
        retry_after.max(1),
    )
}

fn too_many(retry_after: i64) -> ApiError {
    ApiError::new(
        StatusCode::TOO_MANY_REQUESTS,
        format!("Too many requests — try again in {retry_after} s"),
    )
    .with_retry_after(retry_after as u64)
}

/// Count one event; 429 once the window holds more than `max`.
pub async fn hit(state: &AppState, limit: Limit, id: &str) -> Result<(), ApiError> {
    let (key, retry_after) = window_key(state, limit, id, Utc::now().timestamp());
    let mut redis = state.redis.clone();
    let count: i64 = redis.incr(&key, 1).await?;
    if count == 1 {
        let _: () = redis.expire(&key, limit.window_secs).await?;
    }
    if count > limit.max {
        return Err(too_many(retry_after));
    }
    Ok(())
}

/// 429 when `max` failures were already recorded in the current window
/// (checked *before* the attempt, so a blocked caller learns nothing).
pub async fn check_failures(state: &AppState, limit: Limit, id: &str) -> Result<(), ApiError> {
    let (key, retry_after) = window_key(state, limit, id, Utc::now().timestamp());
    let mut redis = state.redis.clone();
    let count: Option<i64> = redis.get(&key).await?;
    if count.unwrap_or(0) >= limit.max {
        return Err(too_many(retry_after));
    }
    Ok(())
}

pub async fn record_failure(state: &AppState, limit: Limit, id: &str) -> Result<(), ApiError> {
    let (key, _) = window_key(state, limit, id, Utc::now().timestamp());
    let mut redis = state.redis.clone();
    let count: i64 = redis.incr(&key, 1).await?;
    if count == 1 {
        let _: () = redis.expire(&key, limit.window_secs).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_are_sane() {
        for limit in [
            LOGIN_PER_IP,
            LOGIN_PER_ACCOUNT,
            SETUP_PER_IP,
            REFRESH_PER_IP,
            TOTP_FAILURES,
            PASSWORD_FAILURES,
            API_KEY_REQUESTS,
            INVALID_API_KEY_PER_IP,
            DEPOSIT_ADDRESS_CREATION,
            WEBHOOK_REPLAY_PER_DEPOSIT,
            WITHDRAWALS,
            TRANSFERS,
        ] {
            assert!(limit.max >= 1 && limit.window_secs >= 1, "{}", limit.scope);
        }
    }

    #[test]
    fn too_many_carries_retry_after() {
        let err = too_many(42);
        assert_eq!(err.status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(err.retry_after, Some(42));
        assert!(err.message.contains("42"));
    }
}
