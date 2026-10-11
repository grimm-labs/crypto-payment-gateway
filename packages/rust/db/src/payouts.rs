//! Legacy Lightning payout requests (`payout_requests`), kept read-only for
//! history: new sends are Lightning withdrawals and never write here.

use anyhow::Result;
use chrono::{DateTime, Utc};
use sqlx::FromRow;

use crate::Db;

const PAYOUT_REQUEST_COLUMNS: &str = r#"id, "idempotencyKey", "apiKeyId", "staffId", "paramsHash", "lnAddress", "amountSats", comment, status::text AS status, "paymentId", "lastError", "createdAt" AT TIME ZONE 'UTC' AS "createdAt", "updatedAt" AT TIME ZONE 'UTC' AS "updatedAt""#;

#[derive(Debug, Clone, FromRow)]
pub struct PayoutRequestRow {
    pub id: String,
    #[sqlx(rename = "idempotencyKey")]
    pub idempotency_key: String,
    #[sqlx(rename = "apiKeyId")]
    pub api_key_id: Option<String>,
    #[sqlx(rename = "staffId")]
    pub staff_id: Option<String>,
    #[sqlx(rename = "paramsHash")]
    pub params_hash: String,
    #[sqlx(rename = "lnAddress")]
    pub ln_address: String,
    #[sqlx(rename = "amountSats")]
    pub amount_sats: i64,
    pub comment: Option<String>,
    /// `PENDING` / `SENT` / `FAILED`.
    pub status: String,
    /// SDK `Payment.id` once the send was accepted.
    #[sqlx(rename = "paymentId")]
    pub payment_id: Option<String>,
    #[sqlx(rename = "lastError")]
    pub last_error: Option<String>,
    #[sqlx(rename = "createdAt")]
    pub created_at: DateTime<Utc>,
    #[sqlx(rename = "updatedAt")]
    pub updated_at: DateTime<Utc>,
}

impl Db {
    /// The pre-upgrade payout request made with this `Idempotency-Key`
    /// (its SDK payment id was the key itself).
    pub async fn find_payout_request_by_key(&self, idempotency_key: &str) -> Result<Option<PayoutRequestRow>> {
        let row = sqlx::query_as::<_, PayoutRequestRow>(&format!(
            r#"SELECT {PAYOUT_REQUEST_COLUMNS} FROM payout_requests WHERE "idempotencyKey" = $1 ORDER BY "createdAt" ASC LIMIT 1"#
        ))
        .bind(idempotency_key)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    /// Newest first.
    pub async fn list_payout_requests(&self, limit: i64, offset: i64) -> Result<(Vec<PayoutRequestRow>, i64)> {
        let rows = sqlx::query_as::<_, PayoutRequestRow>(&format!(
            r#"SELECT {PAYOUT_REQUEST_COLUMNS} FROM payout_requests ORDER BY "createdAt" DESC, id DESC LIMIT $1 OFFSET $2"#
        ))
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.pool)
        .await?;
        let (total,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM payout_requests")
            .fetch_one(&self.pool)
            .await?;
        Ok((rows, total))
    }
}
