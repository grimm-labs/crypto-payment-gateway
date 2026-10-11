//! Per-asset guard rails (`asset_settings`), sweep minimums
//! (`sweep_settings`) and per-key withdrawal limits
//! (`api_key_withdrawal_limits`). All amounts in base units.

use anyhow::Result;
use chrono::{DateTime, Utc};
use domain::{Network, Token};
use sqlx::FromRow;

use crate::{Db, RawAmount};

const ASSET_COLUMNS: &str = r#"token::text AS token, "maxPerWithdrawalRaw"::text AS "maxPerWithdrawalRaw", "dailyMaxRaw"::text AS "dailyMaxRaw", "approvalThresholdRaw"::text AS "approvalThresholdRaw", "hotWalletCapRaw"::text AS "hotWalletCapRaw", "gasReserveFloorRaw"::text AS "gasReserveFloorRaw", "gasAlertBelowRaw"::text AS "gasAlertBelowRaw", "updatedAt" AT TIME ZONE 'UTC' AS "updatedAt""#;

#[derive(Debug, Clone, FromRow)]
pub struct AssetSettingsRow {
    pub token: String,
    /// `0` disables withdrawals of the asset (as payouts used to).
    #[sqlx(rename = "maxPerWithdrawalRaw")]
    pub max_per_withdrawal_raw: RawAmount,
    #[sqlx(rename = "dailyMaxRaw")]
    pub daily_max_raw: RawAmount,
    #[sqlx(rename = "approvalThresholdRaw")]
    pub approval_threshold_raw: RawAmount,
    #[sqlx(rename = "hotWalletCapRaw")]
    pub hot_wallet_cap_raw: Option<RawAmount>,
    #[sqlx(rename = "gasReserveFloorRaw")]
    pub gas_reserve_floor_raw: Option<RawAmount>,
    #[sqlx(rename = "gasAlertBelowRaw")]
    pub gas_alert_below_raw: Option<RawAmount>,
    #[sqlx(rename = "updatedAt")]
    pub updated_at: DateTime<Utc>,
}

impl AssetSettingsRow {
    pub fn token(&self) -> Result<Token> {
        Ok(Token::from_db(&self.token)?)
    }

    /// Settings of an asset whose row is missing: every withdrawal refused.
    pub fn disabled(token: Token) -> Self {
        Self {
            token: token.as_db().into(),
            max_per_withdrawal_raw: RawAmount(0),
            daily_max_raw: RawAmount(0),
            approval_threshold_raw: RawAmount(0),
            hot_wallet_cap_raw: None,
            gas_reserve_floor_raw: None,
            gas_alert_below_raw: None,
            updated_at: Utc::now(),
        }
    }
}

/// Partial update of one asset row (absent = untouched; the nullable
/// columns take `Some(None)` to clear).
#[derive(Debug, Clone, Default)]
pub struct AssetSettingsUpdate {
    pub max_per_withdrawal_raw: Option<u128>,
    pub daily_max_raw: Option<u128>,
    pub approval_threshold_raw: Option<u128>,
    pub hot_wallet_cap_raw: Option<Option<u128>>,
    pub gas_reserve_floor_raw: Option<Option<u128>>,
    pub gas_alert_below_raw: Option<Option<u128>>,
}

#[derive(Debug, Clone, FromRow)]
pub struct SweepSettingsRow {
    pub network: String,
    pub token: String,
    #[sqlx(rename = "minimumRaw")]
    pub minimum_raw: RawAmount,
    pub enabled: bool,
    #[sqlx(rename = "updatedAt")]
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, FromRow)]
pub struct ApiKeyWithdrawalLimitRow {
    #[sqlx(rename = "apiKeyId")]
    pub api_key_id: String,
    pub token: String,
    #[sqlx(rename = "maxPerWithdrawalRaw")]
    pub max_per_withdrawal_raw: RawAmount,
    #[sqlx(rename = "dailyMaxRaw")]
    pub daily_max_raw: RawAmount,
}

impl Db {
    pub async fn list_asset_settings(&self) -> Result<Vec<AssetSettingsRow>> {
        let rows = sqlx::query_as::<_, AssetSettingsRow>(&format!(
            "SELECT {ASSET_COLUMNS} FROM asset_settings ORDER BY token"
        ))
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// The asset's row, or a disabled placeholder when it is missing.
    pub async fn asset_settings(&self, token: Token) -> Result<AssetSettingsRow> {
        let row = sqlx::query_as::<_, AssetSettingsRow>(&format!(
            r#"SELECT {ASSET_COLUMNS} FROM asset_settings WHERE token = $1::"Token""#
        ))
        .bind(token.as_db())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.unwrap_or_else(|| AssetSettingsRow::disabled(token)))
    }

    pub async fn update_asset_settings(
        &self,
        token: Token,
        update: &AssetSettingsUpdate,
    ) -> Result<AssetSettingsRow> {
        let current = self.asset_settings(token).await?;
        let pick = |new: Option<u128>, old: RawAmount| RawAmount(new.unwrap_or(old.0));
        let pick_opt = |new: Option<Option<u128>>, old: Option<RawAmount>| match new {
            Some(value) => value.map(RawAmount),
            None => old,
        };
        let row = sqlx::query_as::<_, AssetSettingsRow>(&format!(
            r#"
            INSERT INTO asset_settings
              (token, "maxPerWithdrawalRaw", "dailyMaxRaw", "approvalThresholdRaw", "hotWalletCapRaw",
               "gasReserveFloorRaw", "gasAlertBelowRaw", "updatedAt")
            VALUES ($1::"Token", $2::numeric, $3::numeric, $4::numeric, $5::numeric, $6::numeric, $7::numeric, NOW())
            ON CONFLICT (token) DO UPDATE SET
              "maxPerWithdrawalRaw" = $2::numeric, "dailyMaxRaw" = $3::numeric,
              "approvalThresholdRaw" = $4::numeric, "hotWalletCapRaw" = $5::numeric,
              "gasReserveFloorRaw" = $6::numeric, "gasAlertBelowRaw" = $7::numeric, "updatedAt" = NOW()
            RETURNING {ASSET_COLUMNS}
            "#
        ))
        .bind(token.as_db())
        .bind(pick(update.max_per_withdrawal_raw, current.max_per_withdrawal_raw))
        .bind(pick(update.daily_max_raw, current.daily_max_raw))
        .bind(pick(update.approval_threshold_raw, current.approval_threshold_raw))
        .bind(pick_opt(update.hot_wallet_cap_raw, current.hot_wallet_cap_raw))
        .bind(pick_opt(update.gas_reserve_floor_raw, current.gas_reserve_floor_raw))
        .bind(pick_opt(update.gas_alert_below_raw, current.gas_alert_below_raw))
        .fetch_one(&self.pool)
        .await?;
        Ok(row)
    }

    pub async fn list_sweep_settings(&self) -> Result<Vec<SweepSettingsRow>> {
        let rows = sqlx::query_as::<_, SweepSettingsRow>(
            r#"SELECT network::text AS network, token::text AS token, "minimumRaw"::text AS "minimumRaw", enabled,
                      "updatedAt" AT TIME ZONE 'UTC' AS "updatedAt"
               FROM sweep_settings ORDER BY network, token"#,
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// `None` when (network, token) is never swept.
    pub async fn sweep_setting(&self, network: Network, token: Token) -> Result<Option<SweepSettingsRow>> {
        let row = sqlx::query_as::<_, SweepSettingsRow>(
            r#"SELECT network::text AS network, token::text AS token, "minimumRaw"::text AS "minimumRaw", enabled,
                      "updatedAt" AT TIME ZONE 'UTC' AS "updatedAt"
               FROM sweep_settings WHERE network = $1::"Network" AND token = $2::"Token""#,
        )
        .bind(network.as_db())
        .bind(token.as_db())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    pub async fn update_sweep_setting(
        &self,
        network: Network,
        token: Token,
        minimum_raw: Option<u128>,
        enabled: Option<bool>,
    ) -> Result<Option<SweepSettingsRow>> {
        let row = sqlx::query_as::<_, SweepSettingsRow>(
            r#"UPDATE sweep_settings SET
                 "minimumRaw" = COALESCE($3::numeric, "minimumRaw"),
                 enabled = COALESCE($4, enabled),
                 "updatedAt" = NOW()
               WHERE network = $1::"Network" AND token = $2::"Token"
               RETURNING network::text AS network, token::text AS token, "minimumRaw"::text AS "minimumRaw", enabled,
                         "updatedAt" AT TIME ZONE 'UTC' AS "updatedAt""#,
        )
        .bind(network.as_db())
        .bind(token.as_db())
        .bind(minimum_raw.map(RawAmount))
        .bind(enabled)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    pub async fn api_key_withdrawal_limits(&self, api_key_id: &str) -> Result<Vec<ApiKeyWithdrawalLimitRow>> {
        let rows = sqlx::query_as::<_, ApiKeyWithdrawalLimitRow>(
            r#"SELECT "apiKeyId", token::text AS token, "maxPerWithdrawalRaw"::text AS "maxPerWithdrawalRaw",
                      "dailyMaxRaw"::text AS "dailyMaxRaw"
               FROM api_key_withdrawal_limits WHERE "apiKeyId" = $1 ORDER BY token"#,
        )
        .bind(api_key_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }
}
