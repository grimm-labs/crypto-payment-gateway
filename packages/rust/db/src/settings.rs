use anyhow::Result;
use config::{
    API_TRAFFIC_RETENTION_DEFAULT_DAYS, DEPOSIT_EXPIRY_DEFAULT_MINUTES, PAYOUT_DAILY_MAX_SATS_DEFAULT,
    PAYOUT_MAX_SATS_DEFAULT, TOTP_ISSUER,
};

use crate::Db;

pub const DEPOSIT_EXPIRY_SETTING: &str = "depositExpiryMinutes";
pub const API_KEY_PREFIX_SETTING: &str = "apiKeyPrefix";
pub const TOTP_ISSUER_SETTING: &str = "totpIssuer";
pub const API_TRAFFIC_RETENTION_SETTING: &str = "apiTrafficRetentionDays";
pub const PAYOUT_MAX_SETTING: &str = "payoutMaxSats";
pub const PAYOUT_DAILY_MAX_SETTING: &str = "payoutDailyMaxSats";
/// Stored value meaning "never purge the traffic log".
pub const API_TRAFFIC_RETENTION_FOREVER: &str = "0";

impl Db {
    pub async fn get_setting(&self, key: &str) -> Result<Option<String>> {
        let row: Option<(String,)> =
            sqlx::query_as(r#"SELECT "value" FROM app_settings WHERE "key" = $1"#)
                .bind(key)
                .fetch_optional(&self.pool)
                .await?;
        Ok(row.map(|(value,)| value))
    }

    pub async fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        sqlx::query(
            r#"
            INSERT INTO app_settings ("key", "value", "updatedAt")
            VALUES ($1, $2, NOW())
            ON CONFLICT ("key") DO UPDATE SET "value" = $2, "updatedAt" = NOW()
            "#,
        )
        .bind(key)
        .bind(value)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Optional tag prepended to newly generated API keys (`{tag}_{8}_{40}`).
    /// `None` when unset or blank — keys are then generated as `{8}_{40}`.
    pub async fn api_key_prefix(&self) -> Result<Option<String>> {
        let value = self.get_setting(API_KEY_PREFIX_SETTING).await?;
        Ok(value
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty()))
    }

    /// Issuer shown by authenticator apps for TOTP enrollments,
    /// admin-configurable from the dashboard. Falls back to the code default
    /// when unset or blank.
    pub async fn totp_issuer(&self) -> Result<String> {
        let value = self.get_setting(TOTP_ISSUER_SETTING).await?;
        Ok(value
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| TOTP_ISSUER.to_string()))
    }

    /// Default payment-address expiry, admin-configurable from the dashboard.
    /// Falls back to the code default when the row is missing or unparsable.
    pub async fn deposit_expiry_minutes(&self) -> Result<i64> {
        let value = self.get_setting(DEPOSIT_EXPIRY_SETTING).await?;
        Ok(value
            .and_then(|v| v.parse::<i64>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(DEPOSIT_EXPIRY_DEFAULT_MINUTES))
    }

    /// Retention of the API traffic log, admin-configurable from the
    /// dashboard. `Ok(None)` means indefinite (stored as "0" — rows are never
    /// purged). A missing or unparsable row falls back to the code default.
    pub async fn api_traffic_retention_days(&self) -> Result<Option<i64>> {
        let value = self.get_setting(API_TRAFFIC_RETENTION_SETTING).await?;
        Ok(match value.and_then(|v| v.trim().parse::<i64>().ok()) {
            Some(0) => None,
            Some(days) if days > 0 => Some(days),
            _ => Some(API_TRAFFIC_RETENTION_DEFAULT_DAYS),
        })
    }

    /// Largest single Lightning payout, in sats (`0` = payouts disabled).
    pub async fn payout_max_sats(&self) -> Result<u64> {
        self.sats_setting(PAYOUT_MAX_SETTING, PAYOUT_MAX_SATS_DEFAULT).await
    }

    /// Lightning payouts allowed over a rolling 24 h, in sats (`0` =
    /// payouts disabled).
    pub async fn payout_daily_max_sats(&self) -> Result<u64> {
        self.sats_setting(PAYOUT_DAILY_MAX_SETTING, PAYOUT_DAILY_MAX_SATS_DEFAULT).await
    }

    /// A missing row means the code default; an unreadable one is an error
    /// rather than a silently raised limit.
    async fn sats_setting(&self, key: &str, default: u64) -> Result<u64> {
        match self.get_setting(key).await? {
            None => Ok(default),
            Some(value) => value
                .trim()
                .parse::<u64>()
                .map_err(|_| anyhow::anyhow!("app_settings.{key} is not a valid sats amount")),
        }
    }
}
