use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Context;
use http_api::router;
use chain::ChainClients;
use config::{AppConfig, API_TRAFFIC_QUEUE_CAPACITY};
use db::Db;
use jobs::{spawn_workers, AppState, TrafficRecorder, WebhookClient};
use lightning_client::{LightningClient, LightningConfig};
use redis::aio::ConnectionManager;
use tracing::info;
use tracing_subscriber::EnvFilter;
use wallet::{WalletChains, WalletService};

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
    if config.btc_confirmations < config::BTC_CONFIRMATIONS_RECOMMENDED {
        tracing::warn!(
            "BTC_CONFIRMATIONS={} is below the recommended {} — replace-by-fee double spends get easier",
            config.btc_confirmations,
            config::BTC_CONFIRMATIONS_RECOMMENDED
        );
    }
    if config.insecure_transport() {
        if config.is_development() {
            tracing::warn!("Redis/Postgres without verified TLS (tolerated in development)");
        } else {
            tracing::warn!(
                "ALLOW_INSECURE_TRANSPORT=true — Redis/Postgres are reached WITHOUT verified TLS: staff sessions, the webhook queue and payment data travel in clear. Only acceptable on a private network."
            );
        }
    }
    if config::is_public_test_mnemonic(&config.wallet_mnemonic) {
        tracing::warn!(
            "WALLET_MNEMONIC is a public BIP39 test vector — never send real funds to these addresses"
        );
    }
    if config.chain.is_testnet() {
        tracing::warn!(
            "CHAIN_MODE=testnet — Sepolia, Nile, devnet, testnet4 and Spark regtest; never point this instance at a mainnet database"
        );
    }
    if config.outflows_frozen {
        tracing::warn!("OUTFLOWS_FROZEN=true — no withdrawal, sweep or gas top-up leaves the gateway");
    }

    let wallet = WalletService::with_chains(
        &config.wallet_mnemonic,
        WalletChains {
            bitcoin_testnet: config.chain.is_testnet(),
            solana_usdt_mint: config.chain.usdt_solana_mint.clone(),
        },
    )
    .context("Invalid WALLET_MNEMONIC")?;
    info!("Wallet mnemonic validated successfully");
    // Public addresses: the operator funds the gas reserves there.
    for network in jobs::treasury::TREASURY_NETWORKS {
        if let Ok(address) = wallet.treasury_address(network) {
            info!("{} treasury: {address}", network.as_api());
        }
    }

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

    let mut chain = ChainClients::new(config.clone());
    // A provider answering for another chain (testnet node, wrong URL,
    // tampered endpoint) would make up "confirmed" payments: that network
    // stays disabled. A transient failure (timeout, 429) only warns.
    for (network, result) in chain.verify_networks().await {
        match result {
            Ok(()) => info!(
                "{} provider verified on {}",
                network.as_api(),
                config.chain.mode.as_str()
            ),
            Err(err) if err.downcast_ref::<chain::WrongChainError>().is_some() => {
                tracing::error!("{} disabled — {err:#}", network.as_api());
                chain.disable_network(network);
            }
            Err(err) => tracing::warn!(
                "{} mainnet check could not complete (kept enabled): {err:#}",
                network.as_api()
            ),
        }
    }

    let lightning = connect_lightning(&config).await;
    let (traffic, traffic_rx) = TrafficRecorder::channel(API_TRAFFIC_QUEUE_CAPACITY);
    // http:// and private receivers are only accepted in development.
    let webhooks = WebhookClient::new(config.is_development())?;
    let setup_token = setup_token(&config, &db).await?;
    let state = AppState {
        config: config.clone(),
        db,
        wallet,
        chain,
        redis,
        traffic,
        lightning: lightning.clone(),
        webhooks,
        setup_token,
    };

    // No boot-time reconciliation is needed before serving: every monitor
    // loop (Lightning included) polls its open payments before its first
    // expiry sweep, and nothing else expires payments.
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

/// The one-time token `POST /setup` requires while no staff exists:
/// `SETUP_TOKEN` when set, otherwise a random one printed once in the logs
/// (the operator reads it there). `None` once an admin exists.
async fn setup_token(config: &AppConfig, db: &Db) -> anyhow::Result<Option<Arc<String>>> {
    if config.jwt_secret.is_none() || db.count_staff().await? > 0 {
        return Ok(None);
    }
    if let Some(token) = &config.setup_token {
        info!("No staff account yet — complete the dashboard setup with SETUP_TOKEN");
        return Ok(Some(Arc::new(token.clone())));
    }
    let token = auth::generate_session_id();
    tracing::warn!(
        "No staff account yet — complete the dashboard setup with this one-time setup token: {token}"
    );
    Ok(Some(Arc::new(token)))
}

/// `None` (Lightning disabled) when `BREEZ_API_KEY` is unset or the SDK
/// cannot connect — never fatal: the other networks keep running.
async fn connect_lightning(config: &AppConfig) -> Option<LightningClient> {
    let Some(api_key) = config.breez_api_key.clone() else {
        tracing::warn!("BREEZ_API_KEY is not set — Lightning disabled");
        return None;
    };
    // On mainnet a public mnemonic would put the Spark wallet in everyone's
    // hands (testnet mode connects to Spark regtest).
    if !config.chain.is_testnet() && config::is_public_test_mnemonic(&config.wallet_mnemonic) {
        tracing::error!(
            "WALLET_MNEMONIC is a public test vector — Lightning disabled (use your own mnemonic)"
        );
        return None;
    }
    match LightningClient::connect(LightningConfig {
        api_key,
        mnemonic: config.wallet_mnemonic.clone(),
        storage_dir: config.breez_storage_dir.clone(),
        testnet: config.chain.is_testnet(),
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
