use std::env;
use std::path::PathBuf;

use anyhow::{Context, Result};
use ipnet::IpNet;
use serde::Deserialize;

pub const APP_NAME: &str = "stablecoin-payment-service";
pub const WEBHOOK_MAX_ATTEMPTS: u32 = 5;
/// Gaps between webhook attempts, indexed by `attempts - 1` after a failed
/// attempt. Geometric ratio 3; the gaps sum to 3600s so the 5th and final
/// attempt starts ~1h after the 1st. Length must stay WEBHOOK_MAX_ATTEMPTS - 1.
pub const WEBHOOK_RETRY_DELAYS_SECS: [u64; 4] = [90, 270, 810, 2430];
const _: () = assert!(WEBHOOK_RETRY_DELAYS_SECS.len() == WEBHOOK_MAX_ATTEMPTS as usize - 1);
pub const WALLET_BALANCE_SYNC_INTERVAL_SECS: u64 = 60 * 60;
pub const ADDRESS_REQUEST_DELAY_MS: u64 = 300;
/// Default payment-address expiry when the DB setting is absent; the value
/// itself lives in `app_settings` and is managed from the dashboard.
pub const DEPOSIT_EXPIRY_DEFAULT_MINUTES: i64 = 60;
pub const DEPOSIT_EXPIRY_MAX_MINUTES: i64 = 1440;
/// Allowed API-key lifetimes (1/3/6 months, 1 year); no expiry when omitted.
pub const API_KEY_EXPIRY_ALLOWED_DAYS: [i64; 4] = [30, 90, 180, 365];
/// Expired keys are rejected at lookup time immediately; this sweep only
/// materializes the EXPIRED status for listings and stats.
pub const API_KEY_EXPIRY_SWEEP_INTERVAL_SECS: u64 = 300;
/// Default issuer shown by authenticator apps for staff TOTP enrollments;
/// admins can override it from the dashboard Settings (`app_settings`).
pub const TOTP_ISSUER: &str = "Stablecoin Payment Gateway";

// API traffic log (`api_requests`). Requests are recorded through a bounded
// in-memory queue and batch-inserted by a background writer so the request
// path never waits on Postgres; when the queue is full, events are dropped
// (and counted) rather than applying back-pressure.
pub const API_TRAFFIC_QUEUE_CAPACITY: usize = 10_000;
pub const API_TRAFFIC_FLUSH_INTERVAL_MS: u64 = 1_000;
pub const API_TRAFFIC_BATCH_MAX: usize = 500;
pub const API_TRAFFIC_PURGE_INTERVAL_SECS: u64 = 60 * 60;
/// Rows deleted per purge statement — keeps each DELETE short-lived.
pub const API_TRAFFIC_PURGE_BATCH: i64 = 10_000;
/// Default retention of the traffic log when the DB setting is absent; the
/// value itself lives in `app_settings` (`0` = keep forever) and is managed
/// from the dashboard.
pub const API_TRAFFIC_RETENTION_DEFAULT_DAYS: i64 = 730;
pub const API_TRAFFIC_RETENTION_MAX_DAYS: i64 = 3650;
const _: () = assert!(API_TRAFFIC_BATCH_MAX > 0 && API_TRAFFIC_BATCH_MAX <= API_TRAFFIC_QUEUE_CAPACITY);
const _: () = assert!(API_TRAFFIC_FLUSH_INTERVAL_MS > 0 && API_TRAFFIC_PURGE_BATCH > 0);
const _: () = assert!(
    API_TRAFFIC_RETENTION_DEFAULT_DAYS >= 1
        && API_TRAFFIC_RETENTION_DEFAULT_DAYS <= API_TRAFFIC_RETENTION_MAX_DAYS
);

/// Payout webhooks catch-up: at boot, wallet sends that finished while the
/// gateway was down are listed from the persisted watermark minus this margin
/// (deliveries are deduplicated, so the overlap is harmless). The SDK lists
/// by *creation* time, so the margin also covers sends that stayed pending
/// for a long while. The watermark is refreshed on this interval while the
/// gateway runs.
pub const LIGHTNING_PAYOUT_CATCH_UP_MARGIN_SECS: u64 = 24 * 60 * 60;
pub const LIGHTNING_PAYOUT_WATERMARK_INTERVAL_SECS: u64 = 5 * 60;

pub const USDT_ETH: &str = "0xdAC17F958D2ee523a2206206994597C13D831ec7";
pub const USDT_TRON: &str = "TR7NHqjeKQxGTCi8q8ZY4pL8otSzgjLj6t";
pub const USDT_SOLANA: &str = "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB";

#[derive(Debug, Clone, Deserialize)]
pub struct AppConfig {
    pub port: u16,
    pub app_env: String,
    pub log_level: String,
    pub wallet_mnemonic: String,
    pub database_url: String,
    pub redis_url: String,
    pub redis_prefix: String,
    pub polling_interval_seconds: u64,
    pub eth_confirmations: i32,
    pub tron_confirmations: i32,
    pub amount_tolerance_percent: f64,
    pub trongrid_api_key: Option<String>,
    pub alchemy_api_key: Option<String>,
    pub helius_api_key: Option<String>,
    pub trongrid_base_url: String,
    pub eth_rpc_base_url: String,
    pub helius_rpc_base_url: String,
    pub helius_api_base_url: String,
    pub jwt_secret: Option<String>,
    pub access_token_ttl_minutes: i64,
    pub refresh_token_ttl_days: i64,
    pub cors_allowed_origins: Option<String>,
    /// Kill-switch for the API traffic log (middleware + writer + purge).
    pub api_traffic_enabled: bool,
    /// Esplora REST base URL; `None` disables the Bitcoin network.
    pub bitcoin_esplora_base_url: Option<String>,
    pub btc_confirmations: i32,
    /// Breez SDK Spark API key; `None` disables Lightning.
    pub breez_api_key: Option<String>,
    /// Absolute path of the Breez SDK local storage (must persist).
    pub breez_storage_dir: String,
    /// Reverse proxies whose `X-Forwarded-For` is trusted (empty = none).
    pub trusted_proxies: Vec<IpNet>,
}

impl AppConfig {
    pub fn from_env() -> Result<Self> {
        load_dotenv();
        Self::from_lookup(|key| env::var(key).ok())
    }

    /// Build the config from any variable source (`from_env` passes the
    /// process environment; tests pass a map).
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let vars = Vars(&lookup);
        let app_env = vars.raw_or("APP_ENV", "development");
        let wallet_mnemonic = vars.required("WALLET_MNEMONIC")?;
        let database_url = vars.required("DATABASE_URL")?;
        let redis_url = vars.required("REDIS_URL")?;

        Ok(Self {
            port: vars.parse_or("PORT", 3000),
            log_level: vars.raw_or("LOG_LEVEL", "info"),
            redis_prefix: format!("{APP_NAME}-{app_env}"),
            app_env,
            wallet_mnemonic,
            database_url,
            redis_url,
            polling_interval_seconds: vars.parse_or("POLLING_INTERVAL_SECONDS", 10),
            eth_confirmations: vars.parse_or("ETH_CONFIRMATIONS", 12),
            tron_confirmations: vars.parse_or("TRON_CONFIRMATIONS", 19),
            amount_tolerance_percent: vars.parse_or("AMOUNT_TOLERANCE_PERCENT", 1.0),
            trongrid_api_key: vars.optional("TRONGRID_API_KEY"),
            alchemy_api_key: vars.optional("ALCHEMY_API_KEY"),
            helius_api_key: vars.optional("HELIUS_API_KEY"),
            trongrid_base_url: vars.string_or("TRONGRID_BASE_URL", "https://api.trongrid.io"),
            eth_rpc_base_url: vars
                .string_or("ETH_RPC_BASE_URL", "https://eth-mainnet.g.alchemy.com/v2"),
            helius_rpc_base_url: vars
                .string_or("HELIUS_RPC_BASE_URL", "https://mainnet.helius-rpc.com"),
            helius_api_base_url: vars.string_or("HELIUS_API_BASE_URL", "https://api.helius.xyz"),
            jwt_secret: vars.optional("JWT_SECRET").filter(|s| s.len() >= 32),
            access_token_ttl_minutes: vars.parse_or("ACCESS_TOKEN_TTL_MINUTES", 15),
            refresh_token_ttl_days: vars.parse_or("REFRESH_TOKEN_TTL_DAYS", 30),
            cors_allowed_origins: vars.optional("CORS_ALLOWED_ORIGINS"),
            api_traffic_enabled: vars.parse_or("API_TRAFFIC_ENABLED", true),
            bitcoin_esplora_base_url: vars.optional("BITCOIN_ESPLORA_BASE_URL"),
            btc_confirmations: vars.parse_or("BTC_CONFIRMATIONS", 3),
            breez_api_key: vars.optional("BREEZ_API_KEY"),
            breez_storage_dir: absolute_path(&vars.string_or("BREEZ_STORAGE_DIR", "./.breez-data"))?,
            trusted_proxies: parse_cidr_list(&vars.optional("TRUSTED_PROXIES").unwrap_or_default())?,
        })
    }

    pub fn redis_key(&self, suffix: &str) -> String {
        format!("{}:{}", self.redis_prefix, suffix)
    }

    pub fn eth_rpc_url(&self, api_key: &str) -> String {
        format!(
            "{}/{}",
            self.eth_rpc_base_url.trim_end_matches('/'),
            api_key
        )
    }

    pub fn helius_rpc_url(&self, api_key: &str) -> String {
        format!(
            "{}/?api-key={api_key}",
            self.helius_rpc_base_url.trim_end_matches('/')
        )
    }

    pub fn helius_transactions_url(&self, api_key: &str) -> String {
        format!(
            "{}/v0/transactions/?api-key={api_key}",
            self.helius_api_base_url.trim_end_matches('/')
        )
    }

    pub fn trongrid_url(&self, path: &str) -> String {
        let base = self.trongrid_base_url.trim_end_matches('/');
        let path = path.trim_start_matches('/');
        format!("{base}/{path}")
    }

    /// `None` when Bitcoin is disabled (no Esplora base URL).
    pub fn esplora_url(&self, path: &str) -> Option<String> {
        let base = self.bitcoin_esplora_base_url.as_deref()?.trim_end_matches('/');
        let path = path.trim_start_matches('/');
        Some(format!("{base}/{path}"))
    }
}

/// Load `.env` from cwd / parents, then from the Cargo workspace root.
fn load_dotenv() {
    if dotenvy::dotenv().is_ok() {
        return;
    }

    // crates/config → workspace root/.env (works even if cwd is elsewhere)
    let workspace_env = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../.env");
    if dotenvy::from_path(&workspace_env).is_ok() {
        return;
    }

    if let Ok(canonical) = workspace_env.canonicalize() {
        let _ = dotenvy::from_path(canonical);
    }
}

struct Vars<'a, F: Fn(&str) -> Option<String>>(&'a F);

impl<F: Fn(&str) -> Option<String>> Vars<'_, F> {
    fn required(&self, key: &str) -> Result<String> {
        (self.0)(key).with_context(|| {
            format!(
                "{key} must be defined in environment variables (check .env at the project root)"
            )
        })
    }

    fn optional(&self, key: &str) -> Option<String> {
        (self.0)(key)
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    }

    /// Untrimmed, kept as-is when set (even empty) — `APP_ENV` feeds the
    /// Redis key prefix, which must not shift under existing deployments.
    fn raw_or(&self, key: &str, default: &str) -> String {
        (self.0)(key).unwrap_or_else(|| default.to_string())
    }

    fn string_or(&self, key: &str, default: &str) -> String {
        self.optional(key).unwrap_or_else(|| default.to_string())
    }

    fn parse_or<T: std::str::FromStr>(&self, key: &str, default: T) -> T {
        (self.0)(key)
            .and_then(|v| v.parse().ok())
            .unwrap_or(default)
    }
}

/// Resolve against the current directory without requiring the path to
/// exist — the Breez SDK breaks on a relative storage dir once the working
/// directory changes.
fn absolute_path(path: &str) -> Result<String> {
    let absolute = std::path::absolute(path)
        .with_context(|| format!("Cannot resolve BREEZ_STORAGE_DIR {path}"))?;
    Ok(absolute.to_string_lossy().into_owned())
}

/// Comma-separated CIDRs; any invalid entry fails startup rather than being
/// silently ignored (a typo would otherwise trust nothing — or the wrong hop).
fn parse_cidr_list(raw: &str) -> Result<Vec<IpNet>> {
    raw.split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(|entry| {
            entry
                .parse::<IpNet>()
                .with_context(|| format!("TRUSTED_PROXIES contains an invalid CIDR: {entry}"))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    const REQUIRED: [(&str, &str); 3] = [
        ("WALLET_MNEMONIC", "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about"),
        ("DATABASE_URL", "postgresql://localhost/test"),
        ("REDIS_URL", "redis://localhost"),
    ];

    fn config_with(extra: &[(&str, &str)]) -> Result<AppConfig> {
        let vars: HashMap<String, String> = REQUIRED
            .iter()
            .chain(extra)
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        AppConfig::from_lookup(|key| vars.get(key).cloned())
    }

    #[test]
    fn bitcoin_and_lightning_defaults() {
        let config = config_with(&[]).unwrap();
        assert_eq!(config.btc_confirmations, 3);
        // Unset = the network is disabled.
        assert_eq!(config.bitcoin_esplora_base_url, None);
        assert_eq!(config.breez_api_key, None);
        assert!(config.trusted_proxies.is_empty());
    }

    #[test]
    fn blank_values_disable_like_unset() {
        let config = config_with(&[
            ("BITCOIN_ESPLORA_BASE_URL", "  "),
            ("BREEZ_API_KEY", ""),
            ("TRUSTED_PROXIES", " "),
        ])
        .unwrap();
        assert_eq!(config.bitcoin_esplora_base_url, None);
        assert_eq!(config.breez_api_key, None);
        assert!(config.trusted_proxies.is_empty());
    }

    #[test]
    fn bitcoin_and_lightning_overrides() {
        let config = config_with(&[
            ("BITCOIN_ESPLORA_BASE_URL", "https://mempool.space/api/"),
            ("BTC_CONFIRMATIONS", "6"),
            ("BREEZ_API_KEY", "breez-key"),
            ("BREEZ_STORAGE_DIR", "/var/lib/breez"),
        ])
        .unwrap();
        assert_eq!(
            config.bitcoin_esplora_base_url.as_deref(),
            Some("https://mempool.space/api/")
        );
        assert_eq!(config.esplora_url("blocks/tip/height").as_deref(), Some("https://mempool.space/api/blocks/tip/height"));
        assert_eq!(config.btc_confirmations, 6);
        assert_eq!(config.breez_api_key.as_deref(), Some("breez-key"));
        assert_eq!(config.breez_storage_dir, "/var/lib/breez");
    }

    #[test]
    fn breez_storage_dir_is_resolved_to_an_absolute_path() {
        let config = config_with(&[]).unwrap();
        let dir = std::path::Path::new(&config.breez_storage_dir);
        assert!(dir.is_absolute(), "{} must be absolute", config.breez_storage_dir);
        assert!(dir.ends_with(".breez-data"));

        let config = config_with(&[("BREEZ_STORAGE_DIR", "relative/breez")]).unwrap();
        let dir = std::path::Path::new(&config.breez_storage_dir);
        assert!(dir.is_absolute());
        assert!(dir.ends_with("relative/breez"));
    }

    #[test]
    fn trusted_proxies_parse_comma_separated_cidrs() {
        let config = config_with(&[("TRUSTED_PROXIES", "172.16.0.0/12, 10.0.0.0/8,::1/128")]).unwrap();
        let expected: Vec<ipnet::IpNet> = ["172.16.0.0/12", "10.0.0.0/8", "::1/128"]
            .iter()
            .map(|c| c.parse().unwrap())
            .collect();
        assert_eq!(config.trusted_proxies, expected);
    }

    #[test]
    fn invalid_trusted_proxy_fails_startup() {
        let err = config_with(&[("TRUSTED_PROXIES", "10.0.0.0/8,not-a-cidr")]).unwrap_err();
        assert!(format!("{err:#}").contains("not-a-cidr"));
    }

    #[test]
    fn missing_required_var_is_an_error() {
        let err = AppConfig::from_lookup(|_| None).unwrap_err();
        assert!(format!("{err:#}").contains("WALLET_MNEMONIC"));
    }

    #[test]
    fn webhook_retry_delays_span_one_hour() {
        assert_eq!(
            WEBHOOK_RETRY_DELAYS_SECS.len(),
            WEBHOOK_MAX_ATTEMPTS as usize - 1
        );
        assert_eq!(WEBHOOK_RETRY_DELAYS_SECS.iter().sum::<u64>(), 3600);
        assert!(WEBHOOK_RETRY_DELAYS_SECS.windows(2).all(|w| w[0] < w[1]));
    }
}
