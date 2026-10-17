use anyhow::Result;
use chrono::{DateTime, Utc};
use domain::{Network, Token, WithdrawalPolicy};
use sqlx::FromRow;
use uuid::Uuid;

use crate::{owner_condition, Db, OwnerFilter, RawAmount};

const API_KEY_COLUMNS: &str = r#"id, name, prefix, "keyHash", status::text AS status, "createdById", "projectId", "lastUsedAt" AT TIME ZONE 'UTC' AS "lastUsedAt", "revokedAt" AT TIME ZONE 'UTC' AS "revokedAt", "expiresAt" AT TIME ZONE 'UTC' AS "expiresAt", "withdrawalPolicy"::text AS "withdrawalPolicy", "withdrawalTokens", "withdrawalNetworks", "createdAt" AT TIME ZONE 'UTC' AS "createdAt""#;

#[derive(Debug, Clone, FromRow)]
pub struct ApiKeyRow {
    pub id: String,
    pub name: String,
    pub prefix: String,
    #[sqlx(rename = "keyHash")]
    pub key_hash: String,
    pub status: String,
    #[sqlx(rename = "createdById")]
    pub created_by_id: String,
    /// Owning project; NULL only on a legacy key from before the first
    /// project (which keeps working unscoped until one exists).
    #[sqlx(rename = "projectId")]
    pub project_id: Option<String>,
    #[sqlx(rename = "lastUsedAt")]
    pub last_used_at: Option<DateTime<Utc>>,
    #[sqlx(rename = "revokedAt")]
    pub revoked_at: Option<DateTime<Utc>>,
    /// NULL = never expires (revocable only).
    #[sqlx(rename = "expiresAt")]
    pub expires_at: Option<DateTime<Utc>>,
    /// What the key may withdraw to (`NONE` by default; granting needs an
    /// expiring key and a TOTP step-up).
    #[sqlx(rename = "withdrawalPolicy")]
    pub withdrawal_policy: String,
    /// DB token values the key may withdraw.
    #[sqlx(rename = "withdrawalTokens")]
    pub withdrawal_tokens: Vec<String>,
    /// DB network values; `None` = every network of the allowed tokens.
    #[sqlx(rename = "withdrawalNetworks")]
    pub withdrawal_networks: Option<Vec<String>>,
    #[sqlx(rename = "createdAt")]
    pub created_at: DateTime<Utc>,
}

impl ApiKeyRow {
    pub fn withdrawal_policy(&self) -> WithdrawalPolicy {
        WithdrawalPolicy::from_db(&self.withdrawal_policy).unwrap_or(WithdrawalPolicy::None)
    }
}

/// A key's withdrawal permission, with its per-token limits.
#[derive(Debug, Clone)]
pub struct WithdrawalGrant {
    pub policy: WithdrawalPolicy,
    pub tokens: Vec<Token>,
    pub networks: Option<Vec<Network>>,
    /// (token, max per withdrawal, rolling 24 h) in base units. Mandatory
    /// for every token of an `OPEN` key, optional for `BOOK`.
    pub limits: Vec<(Token, u128, u128)>,
}

/// List row: `ApiKeyRow` plus the creator's display name.
#[derive(Debug, Clone, FromRow)]
pub struct ApiKeyListRow {
    pub id: String,
    pub name: String,
    pub prefix: String,
    pub status: String,
    #[sqlx(rename = "createdById")]
    pub created_by_id: String,
    #[sqlx(rename = "createdByName")]
    pub created_by_name: Option<String>,
    #[sqlx(rename = "projectId")]
    pub project_id: Option<String>,
    #[sqlx(rename = "lastUsedAt")]
    pub last_used_at: Option<DateTime<Utc>>,
    #[sqlx(rename = "revokedAt")]
    pub revoked_at: Option<DateTime<Utc>>,
    #[sqlx(rename = "expiresAt")]
    pub expires_at: Option<DateTime<Utc>>,
    #[sqlx(rename = "withdrawalPolicy")]
    pub withdrawal_policy: String,
    #[sqlx(rename = "withdrawalTokens")]
    pub withdrawal_tokens: Vec<String>,
    #[sqlx(rename = "withdrawalNetworks")]
    pub withdrawal_networks: Option<Vec<String>>,
    /// `[{token, max_per_withdrawal_raw, daily_max_raw}]` (amounts as
    /// strings).
    #[sqlx(rename = "withdrawalLimits")]
    pub withdrawal_limits: serde_json::Value,
    #[sqlx(rename = "createdAt")]
    pub created_at: DateTime<Utc>,
}

impl Db {
    pub async fn create_api_key(
        &self,
        name: &str,
        prefix: &str,
        key_hash: &str,
        created_by_id: &str,
        expires_at: Option<DateTime<Utc>>,
        project_id: &str,
    ) -> Result<ApiKeyRow> {
        let id = Uuid::new_v4().to_string();
        let now = Utc::now();
        let row = sqlx::query_as::<_, ApiKeyRow>(&format!(
            r#"
            INSERT INTO api_keys
              (id, name, prefix, "keyHash", status, "createdById", "expiresAt", "createdAt", "projectId")
            VALUES
              ($1, $2, $3, $4, 'ACTIVE'::"ApiKeyStatus", $5, $6, $7, $8)
            RETURNING {API_KEY_COLUMNS}
            "#
        ))
        .bind(&id)
        .bind(name)
        .bind(prefix)
        .bind(key_hash)
        .bind(created_by_id)
        .bind(expires_at)
        .bind(now)
        .bind(project_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(row)
    }

    pub async fn list_api_keys(&self, owner: &OwnerFilter) -> Result<Vec<ApiKeyListRow>> {
        let (project_id, merchant_only) = owner.binds();
        let rows = sqlx::query_as::<_, ApiKeyListRow>(&format!(
            r#"
            SELECT k.id, k.name, k.prefix, k.status::text AS status, k."createdById",
                   s."firstName" || ' ' || s."lastName" AS "createdByName",
                   k."projectId",
                   k."lastUsedAt" AT TIME ZONE 'UTC' AS "lastUsedAt",
                   k."revokedAt" AT TIME ZONE 'UTC' AS "revokedAt",
                   k."expiresAt" AT TIME ZONE 'UTC' AS "expiresAt",
                   k."withdrawalPolicy"::text AS "withdrawalPolicy", k."withdrawalTokens", k."withdrawalNetworks",
                   COALESCE((
                     SELECT json_agg(json_build_object(
                       'token', l.token::text,
                       'max_per_withdrawal_raw', l."maxPerWithdrawalRaw"::text,
                       'daily_max_raw', l."dailyMaxRaw"::text) ORDER BY l.token)
                     FROM api_key_withdrawal_limits l WHERE l."apiKeyId" = k.id
                   ), '[]'::json) AS "withdrawalLimits",
                   k."createdAt" AT TIME ZONE 'UTC' AS "createdAt"
            FROM api_keys k
            LEFT JOIN staff_users s ON s.id = k."createdById"
            WHERE {}
            ORDER BY k."createdAt" DESC
            "#,
            owner_condition(r#"k."projectId""#, "$1", "$2")
        ))
        .bind(project_id)
        .bind(merchant_only)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// Usable key for a clear-text prefix. The expiry check here is what
    /// rejects a key the second its deadline passes — the periodic sweep
    /// only materializes the EXPIRED status afterwards.
    pub async fn find_api_key_by_prefix(&self, prefix: &str) -> Result<Option<ApiKeyRow>> {
        let row = sqlx::query_as::<_, ApiKeyRow>(&format!(
            r#"
            SELECT {API_KEY_COLUMNS} FROM api_keys
            WHERE prefix = $1
              AND status = 'ACTIVE'::"ApiKeyStatus"
              AND ("expiresAt" IS NULL OR "expiresAt" > NOW())
            "#
        ))
        .bind(prefix)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    pub async fn find_api_key(&self, id: &str) -> Result<Option<ApiKeyRow>> {
        let row = sqlx::query_as::<_, ApiKeyRow>(&format!(
            "SELECT {API_KEY_COLUMNS} FROM api_keys WHERE id = $1"
        ))
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    /// Replace a key's withdrawal permission and its per-token limits,
    /// atomically. `false` when the key does not exist. The DB refuses a
    /// policy other than `NONE` on a key without expiry.
    pub async fn set_api_key_withdrawal_grant(&self, id: &str, grant: &WithdrawalGrant) -> Result<bool> {
        let mut tx = self.pool.begin().await?;
        let tokens: Vec<&str> = grant.tokens.iter().map(|t| t.as_db()).collect();
        let networks: Option<Vec<&str>> = grant
            .networks
            .as_ref()
            .map(|networks| networks.iter().map(|n| n.as_db()).collect());
        let result = sqlx::query(
            r#"UPDATE api_keys SET "withdrawalPolicy" = $2::"WithdrawalPolicy", "withdrawalTokens" = $3,
                   "withdrawalNetworks" = $4 WHERE id = $1"#,
        )
        .bind(id)
        .bind(grant.policy.as_db())
        .bind(&tokens)
        .bind(&networks)
        .execute(&mut *tx)
        .await?;
        if result.rows_affected() == 0 {
            return Ok(false);
        }
        sqlx::query(r#"DELETE FROM api_key_withdrawal_limits WHERE "apiKeyId" = $1"#)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        for (token, max, daily) in &grant.limits {
            sqlx::query(
                r#"INSERT INTO api_key_withdrawal_limits ("apiKeyId", token, "maxPerWithdrawalRaw", "dailyMaxRaw", "updatedAt")
                   VALUES ($1, $2::"Token", $3::numeric, $4::numeric, NOW())"#,
            )
            .bind(id)
            .bind(token.as_db())
            .bind(RawAmount(*max))
            .bind(RawAmount(*daily))
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(true)
    }

    /// Returns `false` when the key does not exist or is already revoked.
    pub async fn revoke_api_key(&self, id: &str) -> Result<bool> {
        let result = sqlx::query(
            r#"
            UPDATE api_keys
            SET status = 'REVOKED'::"ApiKeyStatus", "revokedAt" = NOW()
            WHERE id = $1 AND "revokedAt" IS NULL
            "#,
        )
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Revoke every key created by `staff_id` that is not already revoked —
    /// used when the account is deactivated. Returns how many were revoked.
    pub async fn revoke_api_keys_created_by(&self, staff_id: &str) -> Result<u64> {
        let result = sqlx::query(
            r#"
            UPDATE api_keys
            SET status = 'REVOKED'::"ApiKeyStatus", "revokedAt" = NOW()
            WHERE "createdById" = $1 AND "revokedAt" IS NULL
            "#,
        )
        .bind(staff_id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    /// Materialize the EXPIRED status for keys whose deadline has passed.
    /// Lookup already rejects them; this keeps listings and stats honest.
    pub async fn expire_stale_api_keys(&self) -> Result<u64> {
        let result = sqlx::query(
            r#"
            UPDATE api_keys
            SET status = 'EXPIRED'::"ApiKeyStatus"
            WHERE status = 'ACTIVE'::"ApiKeyStatus" AND "expiresAt" <= NOW()
            "#,
        )
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    pub async fn touch_api_key_last_used(&self, id: &str) -> Result<()> {
        sqlx::query(r#"UPDATE api_keys SET "lastUsedAt" = NOW() WHERE id = $1"#)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}
