use std::env;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use ipnet::IpNet;
use zeroize::Zeroize;

/// Namespace of every Redis key (`{REDIS_NAMESPACE}-{APP_ENV}:{suffix}`).
/// Historical value from the project's former name (stablecoin payment
/// gateway) — never change it: the webhook queue, staff sessions, the payout
/// watermark and the balance cache all live under this prefix.
pub const REDIS_NAMESPACE: &str = "stablecoin-payment-service";
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
/// A client-supplied `expires_at` must fall in
/// [now + DEPOSIT_EXPIRY_MIN_MINUTES, now + DEPOSIT_EXPIRY_MAX_MINUTES].
pub const DEPOSIT_EXPIRY_MIN_MINUTES: i64 = 1;
/// A payment keeps being polled this long after its deadline before it is
/// expired: a transfer broadcast just before the deadline (or a Lightning
/// event racing the sweep) is still credited instead of being abandoned.
pub const ADDRESS_EXPIRY_GRACE_SECS: i64 = 10 * 60;
/// An address with a DETECTED deposit is never expired while the deposit
/// waits for its confirmations — at most this long after its deadline (a
/// mempool transaction replaced by RBF would otherwise pin it forever).
pub const DETECTED_DEPOSIT_HOLD_SECS: i64 = 24 * 60 * 60;
/// Open (PENDING) payments a single merchant API key may hold at once —
/// bounds the per-cycle polling work a key can impose.
pub const MAX_PENDING_ADDRESSES_PER_API_KEY: i64 = 1_000;
/// Largest USDT `expected_amount` accepted (the column is DECIMAL(24,8)).
pub const USDT_MAX_EXPECTED_AMOUNT: i64 = 1_000_000_000_000;
/// `/balances` queries every issued address on every network: its result
/// is cached this long and recomputed by one caller at a time.
pub const LIVE_BALANCES_CACHE_SECS: u64 = 5 * 60;
/// Allowed API-key lifetimes (1/3/6 months, 1 year); no expiry when omitted.
pub const API_KEY_EXPIRY_ALLOWED_DAYS: [i64; 4] = [30, 90, 180, 365];
/// Expired keys are rejected at lookup time immediately; this sweep only
/// materializes the EXPIRED status for listings and stats.
pub const API_KEY_EXPIRY_SWEEP_INTERVAL_SECS: u64 = 300;
/// Default issuer shown by authenticator apps for staff TOTP enrollments;
/// admins can override it from the dashboard Settings (`app_settings`).
pub const TOTP_ISSUER: &str = "Crypto Payment Gateway";

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
pub const API_TRAFFIC_RETENTION_DEFAULT_DAYS: i64 = 90;
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

/// Lightning payout limits (`app_settings`, changed from the dashboard with
/// a TOTP code). `0` disables payouts.
pub const PAYOUT_MAX_SATS_DEFAULT: u64 = 1_000_000;
pub const PAYOUT_DAILY_MAX_SATS_DEFAULT: u64 = 10_000_000;
/// A payout request stuck PENDING longer than this (crash, timeout while
/// the SDK was sending) may be retried with the same Idempotency-Key — the
/// SDK answers a replayed key from its own storage, never sending twice.
pub const PAYOUT_STALE_PENDING_SECS: i64 = 5 * 60;
/// Upper bound on the whole payout call (address resolution + send).
pub const PAYOUT_TIMEOUT_SECS: u64 = 60;

// Webhook delivery worker.
pub const WEBHOOK_HTTP_TIMEOUT_SECS: u64 = 10;
/// Deliveries attempted concurrently per worker tick.
pub const WEBHOOK_DELIVERY_CONCURRENCY: usize = 16;
/// A claimed queue member is hidden from other workers for this long; if
/// the worker dies mid-delivery the member reappears (at-least-once).
pub const WEBHOOK_CLAIM_LEASE_SECS: i64 = 60;
const _: () = assert!(WEBHOOK_CLAIM_LEASE_SECS as u64 > WEBHOOK_HTTP_TIMEOUT_SECS * 3);

/// Staff passwords (argon2id): the upper bound caps the hashing cost.
pub const PASSWORD_MIN_LEN: usize = 12;
pub const PASSWORD_MAX_LEN: usize = 128;

// Bounds of the env-tunable security parameters (refused at boot).
pub const ETH_CONFIRMATIONS_MIN: i32 = 12;
pub const TRON_CONFIRMATIONS_MIN: i32 = 19;
pub const BTC_CONFIRMATIONS_MIN: i32 = 1;
/// Below this a warning is logged (RBF double-spends get easy).
pub const BTC_CONFIRMATIONS_RECOMMENDED: i32 = 3;
pub const AMOUNT_TOLERANCE_PERCENT_MAX: f64 = 5.0;
pub const ACCESS_TOKEN_TTL_MAX_MINUTES: i64 = 60;
pub const REFRESH_TOKEN_TTL_MAX_DAYS: i64 = 90;
pub const JWT_SECRET_MIN_LEN: usize = 32;
pub const SETUP_TOKEN_MIN_LEN: usize = 16;

/// The `APP_ENV` value under which insecure local conveniences (public test
/// mnemonic, plain-text Redis/Postgres, `http://` providers and webhooks)
/// are tolerated.
pub const DEVELOPMENT_ENV: &str = "development";

/// Mnemonics published as BIP39 test vectors (and in this repo's
/// `.env.example`): anyone can derive their keys, so they are refused
/// outside development and never seed the mainnet Lightning wallet.
const PUBLIC_TEST_MNEMONICS: [&str; 6] = [
    "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about",
    "legal winner thank year wave sausage worth useful legal winner thank yellow",
    "letter advice cage absurd amount doctor acoustic avoid letter advice cage above",
    "zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo wrong",
    "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon art",
    "zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo vote",
];

/// Whether `mnemonic` is one of the publicly known test vectors
/// (whitespace- and case-insensitive).
pub fn is_public_test_mnemonic(mnemonic: &str) -> bool {
    let normalized = mnemonic
        .split_whitespace()
        .map(str::to_lowercase)
        .collect::<Vec<_>>()
        .join(" ");
    PUBLIC_TEST_MNEMONICS.contains(&normalized.as_str())
}

pub const USDT_ETH: &str = "0xdAC17F958D2ee523a2206206994597C13D831ec7";
pub const USDT_TRON: &str = "TR7NHqjeKQxGTCi8q8ZY4pL8otSzgjLj6t";
pub const USDT_SOLANA: &str = "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB";

/// Runtime configuration. Secret fields (`wallet_mnemonic`, `database_url`,
/// `redis_url`, provider keys, `jwt_secret`, `setup_token`) are redacted
/// from `Debug` and zeroized when the config is dropped — never log them.
#[derive(Clone)]
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
    /// Validated origins (`scheme://host[:port]`), never `*`.
    pub cors_allowed_origins: Vec<String>,
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
    /// First-admin bootstrap token for `POST /setup`; generated at boot
    /// (and logged) when unset while no staff exists.
    pub setup_token: Option<String>,
    /// Escape hatch for Redis/Postgres without TLS outside development
    /// (`ALLOW_INSECURE_TRANSPORT=true`, warned loudly at boot).
    pub allow_insecure_transport: bool,
}

impl std::fmt::Debug for AppConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        const REDACTED: &str = "<redacted>";
        let redact = |value: &Option<String>| value.as_ref().map(|_| REDACTED);
        f.debug_struct("AppConfig")
            .field("port", &self.port)
            .field("app_env", &self.app_env)
            .field("log_level", &self.log_level)
            .field("wallet_mnemonic", &REDACTED)
            .field("database_url", &REDACTED)
            .field("redis_url", &REDACTED)
            .field("redis_prefix", &self.redis_prefix)
            .field("polling_interval_seconds", &self.polling_interval_seconds)
            .field("eth_confirmations", &self.eth_confirmations)
            .field("tron_confirmations", &self.tron_confirmations)
            .field("amount_tolerance_percent", &self.amount_tolerance_percent)
            .field("trongrid_api_key", &redact(&self.trongrid_api_key))
            .field("alchemy_api_key", &redact(&self.alchemy_api_key))
            .field("helius_api_key", &redact(&self.helius_api_key))
            .field("trongrid_base_url", &self.trongrid_base_url)
            .field("eth_rpc_base_url", &self.eth_rpc_base_url)
            .field("helius_rpc_base_url", &self.helius_rpc_base_url)
            .field("helius_api_base_url", &self.helius_api_base_url)
            .field("jwt_secret", &redact(&self.jwt_secret))
            .field("access_token_ttl_minutes", &self.access_token_ttl_minutes)
            .field("refresh_token_ttl_days", &self.refresh_token_ttl_days)
            .field("cors_allowed_origins", &self.cors_allowed_origins)
            .field("api_traffic_enabled", &self.api_traffic_enabled)
            .field("bitcoin_esplora_base_url", &self.bitcoin_esplora_base_url)
            .field("btc_confirmations", &self.btc_confirmations)
            .field("breez_api_key", &redact(&self.breez_api_key))
            .field("breez_storage_dir", &self.breez_storage_dir)
            .field("trusted_proxies", &self.trusted_proxies)
            .field("setup_token", &redact(&self.setup_token))
            .field("allow_insecure_transport", &self.allow_insecure_transport)
            .finish()
    }
}

impl Drop for AppConfig {
    fn drop(&mut self) {
        self.wallet_mnemonic.zeroize();
        self.database_url.zeroize();
        self.redis_url.zeroize();
        for secret in [
            &mut self.trongrid_api_key,
            &mut self.alchemy_api_key,
            &mut self.helius_api_key,
            &mut self.jwt_secret,
            &mut self.breez_api_key,
            &mut self.setup_token,
        ] {
            secret.zeroize();
        }
    }
}

impl AppConfig {
    pub fn from_env() -> Result<Self> {
        load_dotenv();
        Self::from_lookup(|key| env::var(key).ok())
    }

    /// Build the config from any variable source (`from_env` passes the
    /// process environment; tests pass a map). Unsafe values are refused
    /// here rather than silently replaced by a default.
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let vars = Vars(&lookup);
        let app_env = vars.raw_or("APP_ENV", DEVELOPMENT_ENV);
        let development = app_env == DEVELOPMENT_ENV;
        let wallet_mnemonic = vars.required("WALLET_MNEMONIC")?;
        let database_url = vars.required("DATABASE_URL")?;
        let redis_url = vars.required("REDIS_URL")?;

        let config = Self {
            port: vars.parse_or("PORT", 3000)?,
            log_level: vars.raw_or("LOG_LEVEL", "info"),
            redis_prefix: format!("{REDIS_NAMESPACE}-{app_env}"),
            app_env,
            wallet_mnemonic,
            database_url,
            redis_url,
            polling_interval_seconds: vars.parse_or("POLLING_INTERVAL_SECONDS", 10)?,
            eth_confirmations: vars.parse_or("ETH_CONFIRMATIONS", 12)?,
            tron_confirmations: vars.parse_or("TRON_CONFIRMATIONS", 19)?,
            amount_tolerance_percent: vars.parse_or("AMOUNT_TOLERANCE_PERCENT", 1.0)?,
            trongrid_api_key: vars.optional("TRONGRID_API_KEY"),
            alchemy_api_key: vars.optional("ALCHEMY_API_KEY"),
            helius_api_key: vars.optional("HELIUS_API_KEY"),
            trongrid_base_url: vars.string_or("TRONGRID_BASE_URL", "https://api.trongrid.io"),
            eth_rpc_base_url: vars
                .string_or("ETH_RPC_BASE_URL", "https://eth-mainnet.g.alchemy.com/v2"),
            helius_rpc_base_url: vars
                .string_or("HELIUS_RPC_BASE_URL", "https://mainnet.helius-rpc.com"),
            helius_api_base_url: vars.string_or("HELIUS_API_BASE_URL", "https://api.helius.xyz"),
            jwt_secret: vars.optional("JWT_SECRET"),
            access_token_ttl_minutes: vars.parse_or("ACCESS_TOKEN_TTL_MINUTES", 15)?,
            refresh_token_ttl_days: vars.parse_or("REFRESH_TOKEN_TTL_DAYS", 30)?,
            cors_allowed_origins: parse_origins(&vars.optional("CORS_ALLOWED_ORIGINS").unwrap_or_default())?,
            api_traffic_enabled: vars.parse_or("API_TRAFFIC_ENABLED", true)?,
            bitcoin_esplora_base_url: vars.optional("BITCOIN_ESPLORA_BASE_URL"),
            btc_confirmations: vars.parse_or("BTC_CONFIRMATIONS", 3)?,
            breez_api_key: vars.optional("BREEZ_API_KEY"),
            breez_storage_dir: absolute_path(&vars.string_or("BREEZ_STORAGE_DIR", "./.breez-data"))?,
            trusted_proxies: parse_cidr_list(&vars.optional("TRUSTED_PROXIES").unwrap_or_default())?,
            setup_token: vars.optional("SETUP_TOKEN"),
            allow_insecure_transport: vars.parse_or("ALLOW_INSECURE_TRANSPORT", false)?,
        };
        config.validate(development)?;
        Ok(config)
    }

    /// Insecure local conveniences are only tolerated here.
    pub fn is_development(&self) -> bool {
        self.app_env == DEVELOPMENT_ENV
    }

    fn validate(&self, development: bool) -> Result<()> {
        if !development && is_public_test_mnemonic(&self.wallet_mnemonic) {
            bail!(
                "WALLET_MNEMONIC is a public BIP39 test vector — refused outside APP_ENV=development (anyone can derive its keys)"
            );
        }
        if let Some(secret) = &self.jwt_secret {
            if secret.len() < JWT_SECRET_MIN_LEN {
                bail!("JWT_SECRET must be at least {JWT_SECRET_MIN_LEN} characters");
            }
        }
        if let Some(token) = &self.setup_token {
            if token.len() < SETUP_TOKEN_MIN_LEN {
                bail!("SETUP_TOKEN must be at least {SETUP_TOKEN_MIN_LEN} characters");
            }
        }
        if !(1..=ACCESS_TOKEN_TTL_MAX_MINUTES).contains(&self.access_token_ttl_minutes) {
            bail!("ACCESS_TOKEN_TTL_MINUTES must be between 1 and {ACCESS_TOKEN_TTL_MAX_MINUTES}");
        }
        if !(1..=REFRESH_TOKEN_TTL_MAX_DAYS).contains(&self.refresh_token_ttl_days) {
            bail!("REFRESH_TOKEN_TTL_DAYS must be between 1 and {REFRESH_TOKEN_TTL_MAX_DAYS}");
        }
        if self.eth_confirmations < ETH_CONFIRMATIONS_MIN {
            bail!("ETH_CONFIRMATIONS must be at least {ETH_CONFIRMATIONS_MIN}");
        }
        if self.tron_confirmations < TRON_CONFIRMATIONS_MIN {
            bail!("TRON_CONFIRMATIONS must be at least {TRON_CONFIRMATIONS_MIN} (solidified blocks)");
        }
        if self.btc_confirmations < BTC_CONFIRMATIONS_MIN {
            bail!("BTC_CONFIRMATIONS must be at least {BTC_CONFIRMATIONS_MIN} (0 credits mempool transactions)");
        }
        if !(0.0..=AMOUNT_TOLERANCE_PERCENT_MAX).contains(&self.amount_tolerance_percent) {
            bail!("AMOUNT_TOLERANCE_PERCENT must be between 0 and {AMOUNT_TOLERANCE_PERCENT_MAX}");
        }
        if self.polling_interval_seconds == 0 {
            bail!("POLLING_INTERVAL_SECONDS must be at least 1");
        }

        // Provider responses decide what counts as a confirmed payment: a
        // plain-text hop lets anyone on the path fabricate one.
        if !development {
            let providers = [
                ("TRONGRID_BASE_URL", Some(&self.trongrid_base_url)),
                ("ETH_RPC_BASE_URL", Some(&self.eth_rpc_base_url)),
                ("HELIUS_RPC_BASE_URL", Some(&self.helius_rpc_base_url)),
                ("HELIUS_API_BASE_URL", Some(&self.helius_api_base_url)),
                ("BITCOIN_ESPLORA_BASE_URL", self.bitcoin_esplora_base_url.as_ref()),
            ];
            for (name, url) in providers {
                if let Some(url) = url {
                    if !url.starts_with("https://") {
                        bail!("{name} must use https:// outside APP_ENV=development");
                    }
                }
            }
        }

        if !development && !self.allow_insecure_transport {
            if !self.redis_url.starts_with("rediss://") {
                bail!(
                    "REDIS_URL must use TLS (rediss://) outside APP_ENV=development — set ALLOW_INSECURE_TRANSPORT=true only if Redis is reachable over a private network"
                );
            }
            if postgres_sslmode(&self.database_url).as_deref() != Some("verify-full") {
                bail!(
                    "DATABASE_URL must set sslmode=verify-full outside APP_ENV=development — set ALLOW_INSECURE_TRANSPORT=true only if Postgres is reachable over a private network"
                );
            }
        }
        Ok(())
    }

    /// Whether Redis or Postgres is reached without a verified TLS session
    /// (logged as a warning at boot).
    pub fn insecure_transport(&self) -> bool {
        !self.redis_url.starts_with("rediss://")
            || postgres_sslmode(&self.database_url).as_deref() != Some("verify-full")
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

    // packages/rust/config → workspace root/.env (works even if cwd is elsewhere)
    let workspace_env = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../.env");
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

    /// Unset or blank = `default`; anything else must parse — a typo is a
    /// startup error, never a silent fallback.
    fn parse_or<T: std::str::FromStr>(&self, key: &str, default: T) -> Result<T> {
        match self.optional(key) {
            None => Ok(default),
            Some(value) => value
                .parse()
                .ok()
                .with_context(|| format!("{key} has an invalid value: {value}")),
        }
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

/// Comma-separated CORS origins (`scheme://host[:port]`). `*` and anything
/// with a path are refused at startup instead of being silently dropped.
fn parse_origins(raw: &str) -> Result<Vec<String>> {
    raw.split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(|origin| {
            let host = origin
                .strip_prefix("https://")
                .or_else(|| origin.strip_prefix("http://"));
            let valid = matches!(host, Some(host) if !host.is_empty()
                && !host.contains(['/', '?', '#', '*', ' ', '@']));
            if !valid {
                bail!("CORS_ALLOWED_ORIGINS contains an invalid origin: {origin} (expected scheme://host[:port], never *)");
            }
            Ok(origin.to_string())
        })
        .collect()
}

/// The `sslmode` query parameter of a Postgres URL, if any.
fn postgres_sslmode(database_url: &str) -> Option<String> {
    let (_, query) = database_url.split_once('?')?;
    query.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        (key == "sslmode").then(|| value.to_string())
    })
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
    fn redis_namespace_survives_the_project_rename() {
        // Frozen on purpose: the webhook queue, staff sessions, the payout
        // watermark and the balance cache all live under this prefix.
        let config = config_with(&[("APP_ENV", "development")]).unwrap();
        assert_eq!(
            config.redis_key("zset:webhook-delivery"),
            "stablecoin-payment-service-development:zset:webhook-delivery"
        );
        assert_eq!(REDIS_NAMESPACE, "stablecoin-payment-service");
    }

    #[test]
    fn totp_issuer_carries_the_product_name() {
        assert_eq!(TOTP_ISSUER, "Crypto Payment Gateway");
    }

    #[test]
    fn missing_required_var_is_an_error() {
        let err = AppConfig::from_lookup(|_| None).unwrap_err();
        assert!(format!("{err:#}").contains("WALLET_MNEMONIC"));
    }

    const PROD_MNEMONIC: &str =
        "pistol lazy spoil bundle swear fruit tomorrow scorpion inside rebuild depart crane";

    fn production_with(extra: &[(&str, &str)]) -> Result<AppConfig> {
        let base = [
            ("APP_ENV", "production"),
            ("WALLET_MNEMONIC", PROD_MNEMONIC),
            ("DATABASE_URL", "postgresql://db.internal/app?sslmode=verify-full"),
            ("REDIS_URL", "rediss://redis.internal:6380"),
        ];
        let vars: HashMap<String, String> = base
            .iter()
            .chain(extra)
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        AppConfig::from_lookup(|key| vars.get(key).cloned())
    }

    #[test]
    fn public_test_mnemonics_are_refused_outside_development() {
        assert!(config_with(&[]).is_ok(), "allowed in development");
        for env in ["production", "staging", "test", ""] {
            let err = config_with(&[("APP_ENV", env)]).unwrap_err();
            assert!(format!("{err:#}").contains("public BIP39 test vector"), "{env}");
        }
        assert!(is_public_test_mnemonic("  ABANDON abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about "));
        assert!(!is_public_test_mnemonic(PROD_MNEMONIC));
        for vector in PUBLIC_TEST_MNEMONICS {
            assert!([12, 24].contains(&vector.split(' ').count()), "{vector}");
        }
    }

    #[test]
    fn production_requires_verified_tls_unless_explicitly_waived() {
        assert!(production_with(&[]).is_ok());

        let err = production_with(&[("REDIS_URL", "redis://redis.internal:6379")]).unwrap_err();
        assert!(format!("{err:#}").contains("rediss://"));
        for url in [
            "postgresql://db/app",
            "postgresql://db/app?sslmode=prefer",
            "postgresql://db/app?sslmode=require",
        ] {
            let err = production_with(&[("DATABASE_URL", url)]).unwrap_err();
            assert!(format!("{err:#}").contains("verify-full"), "{url}");
        }

        let waived = production_with(&[
            ("REDIS_URL", "redis://redis.internal:6379"),
            ("DATABASE_URL", "postgresql://db/app"),
            ("ALLOW_INSECURE_TRANSPORT", "true"),
        ])
        .unwrap();
        assert!(waived.allow_insecure_transport);
        assert!(waived.insecure_transport());
        assert!(!production_with(&[]).unwrap().insecure_transport());
    }

    #[test]
    fn production_providers_must_use_https() {
        let err = production_with(&[("BITCOIN_ESPLORA_BASE_URL", "http://esplora.local/api")])
            .unwrap_err();
        assert!(format!("{err:#}").contains("BITCOIN_ESPLORA_BASE_URL"));
        let err = production_with(&[("ETH_RPC_BASE_URL", "http://node:8545")]).unwrap_err();
        assert!(format!("{err:#}").contains("ETH_RPC_BASE_URL"));
        // Development keeps local nodes usable.
        assert!(config_with(&[("BITCOIN_ESPLORA_BASE_URL", "http://localhost:3002")]).is_ok());
    }

    #[test]
    fn short_jwt_secret_is_an_error_not_a_silent_disable() {
        let err = config_with(&[("JWT_SECRET", "too-short")]).unwrap_err();
        assert!(format!("{err:#}").contains("JWT_SECRET"));
        let config = config_with(&[("JWT_SECRET", &"x".repeat(32))]).unwrap();
        assert!(config.jwt_secret.is_some());
        // Blank = unset (the admin API stays disabled).
        assert!(config_with(&[("JWT_SECRET", " ")]).unwrap().jwt_secret.is_none());
    }

    #[test]
    fn invalid_numbers_fail_startup_instead_of_falling_back() {
        for (key, value) in [
            ("BTC_CONFIRMATIONS", "three"),
            ("PORT", "http"),
            ("API_TRAFFIC_ENABLED", "yes"),
            ("AMOUNT_TOLERANCE_PERCENT", "1%"),
        ] {
            let err = config_with(&[(key, value)]).unwrap_err();
            assert!(format!("{err:#}").contains(key), "{key}");
        }
        // Blank values keep the default.
        assert_eq!(config_with(&[("BTC_CONFIRMATIONS", "")]).unwrap().btc_confirmations, 3);
    }

    #[test]
    fn security_parameters_have_floors_and_bounds() {
        for (key, value) in [
            ("BTC_CONFIRMATIONS", "0"),
            ("ETH_CONFIRMATIONS", "11"),
            ("TRON_CONFIRMATIONS", "1"),
            ("AMOUNT_TOLERANCE_PERCENT", "100"),
            ("AMOUNT_TOLERANCE_PERCENT", "-1"),
            ("ACCESS_TOKEN_TTL_MINUTES", "0"),
            ("ACCESS_TOKEN_TTL_MINUTES", "1440"),
            ("REFRESH_TOKEN_TTL_DAYS", "365"),
            ("POLLING_INTERVAL_SECONDS", "0"),
            ("SETUP_TOKEN", "short"),
        ] {
            let err = config_with(&[(key, value)]).unwrap_err();
            assert!(format!("{err:#}").contains(key), "{key}={value}");
        }
        let config = config_with(&[("BTC_CONFIRMATIONS", "1"), ("AMOUNT_TOLERANCE_PERCENT", "5")]).unwrap();
        assert_eq!(config.btc_confirmations, 1);
    }

    #[test]
    fn cors_origins_are_validated() {
        let config = config_with(&[(
            "CORS_ALLOWED_ORIGINS",
            "https://dash.example.com, http://localhost:5173",
        )])
        .unwrap();
        assert_eq!(
            config.cors_allowed_origins,
            vec!["https://dash.example.com", "http://localhost:5173"]
        );
        for bad in ["*", "https://*.example.com", "dash.example.com", "https://a.com/path"] {
            let err = config_with(&[("CORS_ALLOWED_ORIGINS", bad)]).unwrap_err();
            assert!(format!("{err:#}").contains("CORS_ALLOWED_ORIGINS"), "{bad}");
        }
    }

    #[test]
    fn debug_output_never_contains_secrets() {
        let config = production_with(&[
            ("JWT_SECRET", "jwt-secret-value-that-is-long-enough!!"),
            ("ALCHEMY_API_KEY", "alchemy-key-value"),
            ("BREEZ_API_KEY", "breez-key-value"),
            ("SETUP_TOKEN", "setup-token-value-123"),
        ])
        .unwrap();
        let debug = format!("{config:?}");
        for secret in [
            PROD_MNEMONIC,
            "db.internal",
            "redis.internal",
            "jwt-secret-value",
            "alchemy-key-value",
            "breez-key-value",
            "setup-token-value",
        ] {
            assert!(!debug.contains(secret), "{secret} leaked: {debug}");
        }
        assert!(debug.contains("<redacted>"));
    }

    #[test]
    fn postgres_sslmode_is_read_from_the_query() {
        assert_eq!(postgres_sslmode("postgres://h/db?a=1&sslmode=verify-full").as_deref(), Some("verify-full"));
        assert_eq!(postgres_sslmode("postgres://h/db"), None);
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
