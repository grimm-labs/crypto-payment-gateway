use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Context;
use http_api::router;
use chain::ChainClients;
use config::{AppConfig, API_TRAFFIC_QUEUE_CAPACITY};
use db::Db;
use jobs::{spawn_workers, AppState, TrafficRecorder};
use lightning_client::{LightningClient, LightningConfig};
use redis::aio::ConnectionManager;
use tracing::info;
use tracing_subscriber::EnvFilter;
use wallet::WalletService;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = Arc::new(AppConfig::from_env()?);

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_new(format!("{},gateway=info,api=info,jobs=info", config.log_level))
                .unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        .compact()
        .init();

    if config.trongrid_api_key.is_none() {
        tracing::warn!("TRONGRID_API_KEY is not set — Tron monitoring disabled");
    }
    if config.alchemy_api_key.is_none() {
        tracing::warn!("ALCHEMY_API_KEY is not set — Ethereum monitoring disabled");
    }
    if config.helius_api_key.is_none() {
        tracing::warn!("HELIUS_API_KEY is not set — Solana monitoring disabled");
    }
    if config.bitcoin_esplora_base_url.is_none() {
        tracing::warn!("BITCOIN_ESPLORA_BASE_URL is not set — Bitcoin monitoring disabled");
    }

    let wallet = WalletService::from_mnemonic(&config.wallet_mnemonic)
        .context("Invalid WALLET_MNEMONIC")?;
    info!("Wallet mnemonic validated successfully");

    let db = Db::connect(&config.database_url).await?;
    if let Err(err) = db.migrate().await {
        tracing::warn!("sqlx migrate skipped or failed (schema may already exist): {err:#}");
    }

    match db.active_webhook_endpoints().await {
        Ok(endpoints) if endpoints.is_empty() => tracing::warn!(
            "No active webhook endpoints configured — outgoing webhooks are disabled until one is added in the dashboard (Settings)"
        ),
        Ok(endpoints) => info!("{} active webhook endpoint(s) configured", endpoints.len()),
        Err(err) => tracing::warn!("Could not read webhook endpoints: {err:#}"),
    }

    let redis_client = redis::Client::open(config.redis_url.as_str())
        .context("Invalid REDIS_URL")?;
    let redis = ConnectionManager::new(redis_client)
        .await
        .context("Failed to connect to Redis")?;

    let chain = ChainClients::new(config.clone());
    let lightning = connect_lightning(&config).await;
    let (traffic, traffic_rx) = TrafficRecorder::channel(API_TRAFFIC_QUEUE_CAPACITY);
    let state = AppState {
        config: config.clone(),
        db,
        wallet,
        chain,
        redis,
        traffic,
        lightning: lightning.clone(),
    };

    // Before any expiry sweep (monitor loops, list endpoint): settle invoices
    // paid while the gateway was down, even if their deadline has passed.
    if let Err(err) = jobs::reconcile_lightning_backlog(&state).await {
        tracing::warn!("Boot Lightning reconciliation failed: {err:#}");
    }

    spawn_workers(state.clone(), traffic_rx);

    let app = router(state);
    let addr = format!("0.0.0.0:{}", config.port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    info!("Listening on {addr}");
    // ConnectInfo gives the traffic log a peer address when no proxy header
    // is present.
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await?;

    // Close the Breez SDK so its SQLite storage is left consistent.
    if let Some(lightning) = lightning {
        info!("Disconnecting Breez SDK");
        if let Err(err) = lightning.disconnect().await {
            tracing::error!("Breez SDK disconnect failed: {err:#}");
        }
    }
    info!("Shutdown complete");
    Ok(())
}

/// `None` (Lightning disabled) when `BREEZ_API_KEY` is unset or the SDK
/// cannot connect — never fatal: the other networks keep running.
async fn connect_lightning(config: &AppConfig) -> Option<LightningClient> {
    let Some(api_key) = config.breez_api_key.clone() else {
        tracing::warn!("BREEZ_API_KEY is not set — Lightning disabled");
        return None;
    };
    match LightningClient::connect(LightningConfig {
        api_key,
        mnemonic: config.wallet_mnemonic.clone(),
        storage_dir: config.breez_storage_dir.clone(),
    })
    .await
    {
        Ok(client) => {
            info!("Lightning enabled (Breez storage: {})", config.breez_storage_dir);
            Some(client)
        }
        Err(err) => {
            tracing::error!("Breez SDK connection failed — Lightning disabled: {err:#}");
            None
        }
    }
}

/// Resolves on SIGINT (Ctrl-C) or SIGTERM (docker stop).
async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(err) = tokio::signal::ctrl_c().await {
            tracing::error!("Failed to listen for Ctrl-C: {err}");
            std::future::pending::<()>().await;
        }
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(err) => {
                tracing::error!("Failed to listen for SIGTERM: {err}");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    info!("Shutdown signal received");
}
