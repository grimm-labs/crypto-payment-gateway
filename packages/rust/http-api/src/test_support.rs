//! An `AppState` without infrastructure, for router tests: a Postgres pool
//! that never connects, a stub Redis answering the handful of commands the
//! guards use, the public test mnemonic and no provider key. Routes are
//! exercised with `tower::ServiceExt::oneshot`; nothing reaches the
//! database (every request stops at routing or at the auth guards).

use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use config::AppConfig;
use jobs::{AppState, TrafficRecorder, WebhookClient};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tower::ServiceExt;
use wallet::{WalletChains, WalletService};

const TEST_MNEMONIC: &str =
    "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";

/// A RESP2 stub: `GET` → nil, `INCR(BY)` → 1, anything else → `+OK`. Enough for
/// the rate limiter and the connection handshake; never a real store.
async fn stub_redis() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind stub redis");
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((socket, _)) = listener.accept().await else { break };
            tokio::spawn(async move {
                let (read, mut write) = socket.into_split();
                let mut reader = BufReader::new(read);
                let mut line = String::new();
                loop {
                    line.clear();
                    let Ok(n) = reader.read_line(&mut line).await else { break };
                    if n == 0 {
                        break;
                    }
                    let Some(count) = line.trim_end().strip_prefix('*').and_then(|c| c.parse::<usize>().ok()) else {
                        continue;
                    };
                    let mut args = Vec::with_capacity(count);
                    for _ in 0..count {
                        line.clear();
                        if reader.read_line(&mut line).await.is_err() {
                            return;
                        }
                        let len: usize = line.trim_end().strip_prefix('$').and_then(|l| l.parse().ok()).unwrap_or(0);
                        let mut buf = vec![0u8; len + 2];
                        if reader.read_exact(&mut buf).await.is_err() {
                            return;
                        }
                        args.push(String::from_utf8_lossy(&buf[..len]).to_string());
                    }
                    let reply = match args.first().map(|c| c.to_ascii_uppercase()).as_deref() {
                        Some("GET") => "$-1\r\n",
                        Some("INCR" | "INCRBY") => ":1\r\n",
                        _ => "+OK\r\n",
                    };
                    if write.write_all(reply.as_bytes()).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    port
}

pub(crate) async fn test_state() -> AppState {
    let redis_port = stub_redis().await;
    let vars: HashMap<&str, String> = HashMap::from([
        ("WALLET_MNEMONIC", TEST_MNEMONIC.to_string()),
        ("DATABASE_URL", "postgresql://postgres:postgres@127.0.0.1:1/never".to_string()),
        ("REDIS_URL", format!("redis://127.0.0.1:{redis_port}")),
        ("JWT_SECRET", "x".repeat(32)),
        ("APP_ENV", "development".to_string()),
    ]);
    let config = Arc::new(AppConfig::from_lookup(|key| vars.get(key).cloned()).expect("test config"));
    let wallet = WalletService::with_chains(
        TEST_MNEMONIC,
        WalletChains {
            bitcoin_testnet: false,
            solana_usdt_mint: config.chain.usdt_solana_mint.clone(),
        },
    )
    .expect("test wallet");
    let redis_client = redis::Client::open(config.redis_url.as_str()).expect("redis url");
    let redis = redis::aio::ConnectionManager::new(redis_client)
        .await
        .expect("stub redis connection");
    let (traffic, _rx) = TrafficRecorder::channel(64);
    AppState {
        config: config.clone(),
        db: db::Db::connect_lazy(&config.database_url).expect("lazy pool"),
        wallet,
        chain: chain::ChainClients::new(config),
        redis,
        traffic,
        lightning: None,
        webhooks: WebhookClient::new(true).expect("webhook client"),
    }
}

/// The status the full router answers `method path` with, without headers.
pub(crate) async fn status_of(state: &AppState, method: Method, path: &str) -> StatusCode {
    let request = Request::builder().method(method).uri(path).body(Body::empty()).unwrap();
    crate::router(state.clone()).oneshot(request).await.unwrap().status()
}

#[cfg(test)]
mod router_tests {
    use super::*;

    #[tokio::test]
    async fn custody_and_wallet_history_routes_left_the_merchant_surface() {
        let state = test_state().await;
        // Spec §11: 404 on /api/v1 — like a path that never existed.
        for path in [
            "/api/v1/balances",
            "/api/v1/wallet-balances",
            "/api/v1/lightning/payments",
            "/api/v1/lightning/payments/some-id",
        ] {
            assert_eq!(status_of(&state, Method::GET, path).await, StatusCode::NOT_FOUND, "{path}");
        }
        // Still served to staff (the JWT guard answers, so the route exists).
        for path in [
            "/api/admin/v1/balances",
            "/api/admin/v1/wallet-balances",
            "/api/admin/v1/lightning/payments",
            "/api/admin/v1/lightning/payments/some-id",
        ] {
            assert_eq!(status_of(&state, Method::GET, path).await, StatusCode::UNAUTHORIZED, "{path}");
        }
    }

    #[tokio::test]
    async fn the_merchant_surface_keeps_its_scoped_routes() {
        let state = test_state().await;
        // The key guard answers: the routes exist (401 = missing x-api-key).
        for (method, path) in [
            (Method::GET, "/api/v1/deposit-addresses"),
            (Method::POST, "/api/v1/deposit-addresses"),
            (Method::GET, "/api/v1/deposit-addresses/some-id"),
            (Method::POST, "/api/v1/deposits/some-id/webhooks/retry"),
            (Method::GET, "/api/v1/withdrawals"),
            (Method::POST, "/api/v1/withdrawals"),
            (Method::POST, "/api/v1/withdrawals/quote"),
            (Method::GET, "/api/v1/withdrawals/some-id"),
            (Method::POST, "/api/v1/withdrawals/some-id/cancel"),
            (Method::GET, "/api/v1/address-book"),
            // The deprecated payout alias stays.
            (Method::POST, "/api/v1/lightning/payments"),
            (Method::POST, "/api/v1/lightning/payments/quote"),
        ] {
            assert_eq!(status_of(&state, method.clone(), path).await, StatusCode::UNAUTHORIZED, "{method} {path}");
        }
        assert_eq!(status_of(&state, Method::GET, "/api/v1/nope").await, StatusCode::NOT_FOUND);
        assert_eq!(status_of(&state, Method::GET, "/healthz").await, StatusCode::OK);
    }
}
