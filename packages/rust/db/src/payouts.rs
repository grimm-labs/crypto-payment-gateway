use anyhow::Result;
use chrono::{DateTime, Utc};
use sqlx::FromRow;
use uuid::Uuid;

use crate::Db;

/// Serializes payout reservations (limit check + insert) across every API
/// instance — payouts are rare, contention is not a concern.
const PAYOUT_RESERVATION_LOCK: i64 = 0x5041_594f_5554; // "PAYOUT"

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

/// A payout about to be sent. Exactly one of `api_key_id` / `staff_id`.
#[derive(Debug, Clone)]
pub struct NewPayoutRequest<'a> {
    pub idempotency_key: &'a str,
    pub api_key_id: Option<&'a str>,
    pub staff_id: Option<&'a str>,
    pub params_hash: &'a str,
    pub ln_address: &'a str,
    pub amount_sats: u64,
    pub comment: Option<&'a str>,
}

/// Outcome of [`Db::reserve_payout`].
#[derive(Debug, Clone)]
pub enum PayoutReservation {
    /// Counted against the limits and marked PENDING: go send it.
    Reserved(PayoutRequestRow),
    /// Same caller, same parameters, already sent: answer from the stored
    /// payment, never send again.
    AlreadySent(PayoutRequestRow),
    /// Same key, still being sent by another request.
    InProgress,
    /// The key was used by another caller.
    CallerMismatch,
    /// The key was used with other parameters.
    ParamsMismatch,
    /// A limit is 0.
    Disabled,
    PerPayoutLimit { max_sats: u64 },
    DailyLimit { used_sats: u64, max_sats: u64 },
}

impl Db {
    /// Reserve a payout under its Idempotency-Key, checking the per-payout
    /// and rolling-24 h limits atomically (one advisory lock serializes every
    /// reservation, so parallel requests cannot all pass the daily check).
    /// The 24 h window counts PENDING and SENT requests by `updatedAt`.
    /// A FAILED request, or one stuck PENDING for more than `stale_secs`,
    /// may be re-reserved by its original caller with the same parameters.
    pub async fn reserve_payout(
        &self,
        request: &NewPayoutRequest<'_>,
        max_sats: u64,
        daily_max_sats: u64,
        stale_secs: i64,
    ) -> Result<PayoutReservation> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("SELECT pg_advisory_xact_lock($1)")
            .bind(PAYOUT_RESERVATION_LOCK)
            .execute(&mut *tx)
            .await?;

        let existing = sqlx::query_as::<_, PayoutRequestRow>(&format!(
            r#"SELECT {PAYOUT_REQUEST_COLUMNS} FROM payout_requests WHERE "idempotencyKey" = $1 FOR UPDATE"#
        ))
        .bind(request.idempotency_key)
        .fetch_optional(&mut *tx)
        .await?;

        if let Some(existing) = &existing {
            if existing.api_key_id.as_deref() != request.api_key_id
                || existing.staff_id.as_deref() != request.staff_id
            {
                return Ok(PayoutReservation::CallerMismatch);
            }
            if existing.params_hash != request.params_hash {
                return Ok(PayoutReservation::ParamsMismatch);
            }
            match existing.status.as_str() {
                "SENT" => return Ok(PayoutReservation::AlreadySent(existing.clone())),
                "PENDING"
                    if existing.updated_at > Utc::now() - chrono::Duration::seconds(stale_secs) =>
                {
                    return Ok(PayoutReservation::InProgress)
                }
                _ => {}
            }
        }

        if max_sats == 0 || daily_max_sats == 0 {
            return Ok(PayoutReservation::Disabled);
        }
        if request.amount_sats > max_sats {
            return Ok(PayoutReservation::PerPayoutLimit { max_sats });
        }
        // A stale PENDING retry is already inside the window: do not count
        // it twice.
        let (used,): (i64,) = sqlx::query_as(
            r#"
            SELECT COALESCE(SUM("amountSats"), 0)::BIGINT FROM payout_requests
            WHERE status IN ('PENDING'::"PayoutRequestStatus", 'SENT'::"PayoutRequestStatus")
              AND "updatedAt" > (NOW() AT TIME ZONE 'UTC') - INTERVAL '24 hours'
              AND "idempotencyKey" <> $1
            "#,
        )
        .bind(request.idempotency_key)
        .fetch_one(&mut *tx)
        .await?;
        let used_sats = u64::try_from(used).unwrap_or(0);
        if used_sats.saturating_add(request.amount_sats) > daily_max_sats {
            return Ok(PayoutReservation::DailyLimit {
                used_sats,
                max_sats: daily_max_sats,
            });
        }

        let now = Utc::now();
        let row = match existing {
            Some(existing) => {
                sqlx::query_as::<_, PayoutRequestRow>(&format!(
                    r#"
                    UPDATE payout_requests
                    SET status = 'PENDING'::"PayoutRequestStatus", "lastError" = NULL, "updatedAt" = $2
                    WHERE id = $1
                    RETURNING {PAYOUT_REQUEST_COLUMNS}
                    "#
                ))
                .bind(&existing.id)
                .bind(now)
                .fetch_one(&mut *tx)
                .await?
            }
            None => {
                sqlx::query_as::<_, PayoutRequestRow>(&format!(
                    r#"
                    INSERT INTO payout_requests
                      (id, "idempotencyKey", "apiKeyId", "staffId", "paramsHash", "lnAddress", "amountSats", comment, status, "createdAt", "updatedAt")
                    VALUES
                      ($1, $2, $3, $4, $5, $6, $7, $8, 'PENDING'::"PayoutRequestStatus", $9, $9)
                    RETURNING {PAYOUT_REQUEST_COLUMNS}
                    "#
                ))
                .bind(Uuid::new_v4().to_string())
                .bind(request.idempotency_key)
                .bind(request.api_key_id)
                .bind(request.staff_id)
                .bind(request.params_hash)
                .bind(request.ln_address)
                .bind(i64::try_from(request.amount_sats)?)
                .bind(request.comment)
                .bind(now)
                .fetch_one(&mut *tx)
                .await?
            }
        };
        tx.commit().await?;
        Ok(PayoutReservation::Reserved(row))
    }

    pub async fn mark_payout_sent(&self, id: &str, payment_id: &str) -> Result<()> {
        sqlx::query(
            r#"
            UPDATE payout_requests
            SET status = 'SENT'::"PayoutRequestStatus", "paymentId" = $2, "lastError" = NULL, "updatedAt" = NOW()
            WHERE id = $1
            "#,
        )
        .bind(id)
        .bind(payment_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// `error` is a generic, client-safe reason (the SDK detail stays in the
    /// logs).
    pub async fn mark_payout_failed(&self, id: &str, error: &str) -> Result<()> {
        sqlx::query(
            r#"
            UPDATE payout_requests
            SET status = 'FAILED'::"PayoutRequestStatus", "lastError" = $2, "updatedAt" = NOW()
            WHERE id = $1
            "#,
        )
        .bind(id)
        .bind(error)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}
