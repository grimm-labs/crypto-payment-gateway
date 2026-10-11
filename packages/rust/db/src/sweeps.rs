//! `sweeps`: moving the confirmed funds of a deposit address into its
//! treasury. A sweep never changes the deposit or deposit-address status
//! (invariant 9).

use anyhow::Result;
use chrono::{DateTime, Utc};
use domain::{Network, SweepStatus, Token};
use sqlx::{FromRow, PgConnection, Postgres, Transaction};
use uuid::Uuid;

use crate::{Db, RawAmount};

const SWEEP_COLUMNS: &str = r#"id, network::text AS network, token::text AS token, "depositAddressId", "fromAddress", "amountRaw"::text AS "amountRaw", "creditedRaw"::text AS "creditedRaw", "unattributedRaw"::text AS "unattributedRaw", "feeRaw"::text AS "feeRaw", status::text AS status, "lastError", "createdAt" AT TIME ZONE 'UTC' AS "createdAt", "updatedAt" AT TIME ZONE 'UTC' AS "updatedAt""#;

#[derive(Debug, Clone, FromRow)]
pub struct SweepRow {
    pub id: String,
    pub network: String,
    pub token: String,
    #[sqlx(rename = "depositAddressId")]
    pub deposit_address_id: String,
    #[sqlx(rename = "fromAddress")]
    pub from_address: String,
    #[sqlx(rename = "amountRaw")]
    pub amount_raw: RawAmount,
    #[sqlx(rename = "creditedRaw")]
    pub credited_raw: RawAmount,
    #[sqlx(rename = "unattributedRaw")]
    pub unattributed_raw: RawAmount,
    #[sqlx(rename = "feeRaw")]
    pub fee_raw: Option<RawAmount>,
    pub status: String,
    #[sqlx(rename = "lastError")]
    pub last_error: Option<String>,
    #[sqlx(rename = "createdAt")]
    pub created_at: DateTime<Utc>,
    #[sqlx(rename = "updatedAt")]
    pub updated_at: DateTime<Utc>,
}

impl SweepRow {
    pub fn network(&self) -> Result<Network> {
        Ok(Network::from_db(&self.network)?)
    }

    pub fn token(&self) -> Result<Token> {
        Ok(Token::from_db(&self.token)?)
    }

    pub fn status(&self) -> Result<SweepStatus> {
        Ok(SweepStatus::from_db(&self.status)?)
    }
}

/// A deposit address the sweeper looks at.
#[derive(Debug, Clone, FromRow)]
pub struct SweepCandidate {
    #[sqlx(rename = "depositAddressId")]
    pub deposit_address_id: String,
    pub address: String,
    #[sqlx(rename = "derivationIndex")]
    pub derivation_index: i32,
    /// The token the address was issued for (its deposits are in it).
    pub token: String,
}

#[derive(Debug, Clone)]
pub struct NewSweep<'a> {
    pub network: Network,
    pub token: Token,
    pub deposit_address_id: &'a str,
    pub from_address: &'a str,
    pub amount_raw: u128,
    pub credited_raw: u128,
    pub status: SweepStatus,
}

/// A sweep locked by a worker until `commit`.
pub struct SweepClaim {
    tx: Transaction<'static, Postgres>,
    row: SweepRow,
}

impl SweepClaim {
    pub fn row(&self) -> &SweepRow {
        &self.row
    }

    pub fn conn(&mut self) -> &mut PgConnection {
        &mut self.tx
    }

    pub async fn set_status(
        &mut self,
        to: SweepStatus,
        last_error: Option<&str>,
        add_fee_raw: Option<u128>,
    ) -> Result<SweepRow> {
        let row = set_status(&mut self.tx, &self.row.id, to, last_error, add_fee_raw).await?;
        self.row = row.clone();
        Ok(row)
    }

    pub async fn commit(self) -> Result<SweepRow> {
        self.tx.commit().await?;
        Ok(self.row)
    }
}

async fn set_status(
    conn: &mut PgConnection,
    id: &str,
    to: SweepStatus,
    last_error: Option<&str>,
    add_fee_raw: Option<u128>,
) -> Result<SweepRow> {
    let row = sqlx::query_as::<_, SweepRow>(&format!(
        r#"
        UPDATE sweeps SET
            status = $2::"SweepStatus",
            "lastError" = COALESCE($3, "lastError"),
            "feeRaw" = CASE WHEN $4::numeric IS NULL THEN "feeRaw" ELSE COALESCE("feeRaw", 0) + $4::numeric END,
            "updatedAt" = NOW()
        WHERE id = $1
        RETURNING {SWEEP_COLUMNS}
        "#
    ))
    .bind(id)
    .bind(to.as_db())
    .bind(last_error)
    .bind(add_fee_raw.map(RawAmount))
    .fetch_one(&mut *conn)
    .await?;
    Ok(row)
}

/// Statuses a deposit address must have to be swept: the payment can no
/// longer complete.
const FINAL_ADDRESS: &str = r#"a.status IN ('PAID'::"DepositAddressStatus", 'EXPIRED'::"DepositAddressStatus")
  AND NOT EXISTS (SELECT 1 FROM deposits d WHERE d."depositAddressId" = a.id AND d.status = 'DETECTED'::"DepositStatus")"#;

impl Db {
    /// Create a sweep; `None` when one is already in progress for the
    /// address, whatever its token (unique open-sweep index).
    pub async fn create_sweep(&self, new: &NewSweep<'_>) -> Result<Option<SweepRow>> {
        let unattributed = new.amount_raw.saturating_sub(new.credited_raw);
        let credited = new.amount_raw - unattributed;
        let row = sqlx::query_as::<_, SweepRow>(&format!(
            r#"
            INSERT INTO sweeps
              (id, network, token, "depositAddressId", "fromAddress", "amountRaw", "creditedRaw",
               "unattributedRaw", status, "createdAt", "updatedAt")
            VALUES ($1, $2::"Network", $3::"Token", $4, $5, $6::numeric, $7::numeric, $8::numeric,
                    $9::"SweepStatus", NOW(), NOW())
            ON CONFLICT DO NOTHING
            RETURNING {SWEEP_COLUMNS}
            "#
        ))
        .bind(Uuid::new_v4().to_string())
        .bind(new.network.as_db())
        .bind(new.token.as_db())
        .bind(new.deposit_address_id)
        .bind(new.from_address)
        .bind(RawAmount(new.amount_raw))
        .bind(RawAmount(credited))
        .bind(RawAmount(unattributed))
        .bind(new.status.as_db())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    pub async fn find_sweep(&self, id: &str) -> Result<Option<SweepRow>> {
        let row = sqlx::query_as::<_, SweepRow>(&format!("SELECT {SWEEP_COLUMNS} FROM sweeps WHERE id = $1"))
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row)
    }

    /// Lock the oldest sweep of `network` in one of `statuses` for a worker
    /// (`SKIP LOCKED`).
    /// Lock the least recently touched sweep in one of `statuses`, except
    /// those in `skip` (already found waiting this tick).
    pub async fn claim_sweep(&self, network: Network, statuses: &[SweepStatus], skip: &[String]) -> Result<Option<SweepClaim>> {
        let statuses: Vec<&str> = statuses.iter().map(|s| s.as_db()).collect();
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query_as::<_, SweepRow>(&format!(
            r#"SELECT {SWEEP_COLUMNS} FROM sweeps
               WHERE network = $1::"Network" AND status::text = ANY($2) AND NOT (id = ANY($3))
               ORDER BY "updatedAt" ASC LIMIT 1
               FOR UPDATE SKIP LOCKED"#
        ))
        .bind(network.as_db())
        .bind(&statuses)
        .bind(skip)
        .fetch_optional(&mut *tx)
        .await?;
        Ok(row.map(|row| SweepClaim { tx, row }))
    }

    /// A sweep found waiting goes to the back of the queue (claims take the
    /// least recently touched first).
    pub async fn touch_sweep(&self, id: &str) -> Result<()> {
        sqlx::query(r#"UPDATE sweeps SET "updatedAt" = NOW() WHERE id = $1"#)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Lock one sweep (skipped if a worker holds it).
    pub async fn claim_sweep_by_id(&self, id: &str) -> Result<Option<SweepClaim>> {
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query_as::<_, SweepRow>(&format!(
            "SELECT {SWEEP_COLUMNS} FROM sweeps WHERE id = $1 FOR UPDATE SKIP LOCKED"
        ))
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
        Ok(row.map(|row| SweepClaim { tx, row }))
    }

    pub async fn set_sweep_status(
        &self,
        id: &str,
        to: SweepStatus,
        last_error: Option<&str>,
        add_fee_raw: Option<u128>,
    ) -> Result<SweepRow> {
        let mut conn = self.pool.acquire().await?;
        set_status(&mut conn, id, to, last_error, add_fee_raw).await
    }

    /// Sweeps that are not final yet, oldest first.
    pub async fn open_sweeps(&self, network: Option<Network>) -> Result<Vec<SweepRow>> {
        let rows = sqlx::query_as::<_, SweepRow>(&format!(
            r#"SELECT {SWEEP_COLUMNS} FROM sweeps
               WHERE status NOT IN ('CONFIRMED'::"SweepStatus", 'FAILED'::"SweepStatus", 'SKIPPED'::"SweepStatus")
                 AND ($1::text IS NULL OR network = $1::"Network")
               ORDER BY "createdAt" ASC"#
        ))
        .bind(network.map(|n| n.as_db()))
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    pub async fn list_sweeps(
        &self,
        network: Option<Network>,
        status: Option<SweepStatus>,
        limit: i64,
        offset: i64,
    ) -> Result<(Vec<SweepRow>, i64)> {
        let network = network.map(|n| n.as_db());
        let status = status.map(|s| s.as_db());
        let condition = r#"($1::text IS NULL OR network = $1::"Network") AND ($2::text IS NULL OR status = $2::"SweepStatus")"#;
        let rows = sqlx::query_as::<_, SweepRow>(&format!(
            r#"SELECT {SWEEP_COLUMNS} FROM sweeps WHERE {condition} ORDER BY "createdAt" DESC, id DESC LIMIT $3 OFFSET $4"#
        ))
        .bind(network)
        .bind(status)
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.pool)
        .await?;
        let (total,): (i64,) = sqlx::query_as(&format!("SELECT COUNT(*) FROM sweeps WHERE {condition}"))
            .bind(network)
            .bind(status)
            .fetch_one(&self.pool)
            .await?;
        Ok((rows, total))
    }

    /// Final deposit addresses of `network` with a confirmed deposit newer
    /// than their last confirmed sweep (of their token), and no sweep in
    /// progress.
    pub async fn sweep_candidates(&self, network: Network) -> Result<Vec<SweepCandidate>> {
        let rows = sqlx::query_as::<_, SweepCandidate>(&format!(
            r#"
            SELECT a.id AS "depositAddressId", a.address, a."derivationIndex", a.token::text AS token
            FROM deposit_addresses a
            WHERE a.network = $1::"Network"
              AND {FINAL_ADDRESS}
              AND EXISTS (
                SELECT 1 FROM deposits d
                WHERE d."depositAddressId" = a.id AND d.status = 'CONFIRMED'::"DepositStatus"
                  AND d."confirmedAt" > COALESCE((
                    SELECT MAX(s."createdAt") FROM sweeps s
                    WHERE s."depositAddressId" = a.id AND s.token = a.token AND s.status = 'CONFIRMED'::"SweepStatus"
                  ), '-infinity'::timestamp)
              )
              AND NOT EXISTS (
                SELECT 1 FROM sweeps s WHERE s."depositAddressId" = a.id
                  AND s.status NOT IN ('CONFIRMED'::"SweepStatus", 'FAILED'::"SweepStatus", 'SKIPPED'::"SweepStatus")
              )
            ORDER BY a."createdAt" ASC
            "#
        ))
        .bind(network.as_db())
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// Every final deposit address of `network` — the daily full rescan
    /// (late payments, unattributed funds).
    pub async fn final_deposit_addresses(&self, network: Network) -> Result<Vec<SweepCandidate>> {
        let rows = sqlx::query_as::<_, SweepCandidate>(&format!(
            r#"
            SELECT a.id AS "depositAddressId", a.address, a."derivationIndex", a.token::text AS token
            FROM deposit_addresses a
            WHERE a.network = $1::"Network" AND {FINAL_ADDRESS}
              AND NOT EXISTS (
                SELECT 1 FROM sweeps s WHERE s."depositAddressId" = a.id
                  AND s.status NOT IN ('CONFIRMED'::"SweepStatus", 'FAILED'::"SweepStatus", 'SKIPPED'::"SweepStatus")
              )
            ORDER BY a."createdAt" ASC
            "#
        ))
        .bind(network.as_db())
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// Confirmed deposits of `token` on the address since its last
    /// confirmed sweep of that token — what a sweep of it is credited with.
    pub async fn credited_since_last_sweep(&self, deposit_address_id: &str, token: Token) -> Result<u128> {
        let (credited,): (RawAmount,) = sqlx::query_as(
            r#"
            SELECT COALESCE(SUM(d."amountRaw"::numeric), 0)::text
            FROM deposits d
            JOIN deposit_addresses a ON a.id = d."depositAddressId"
            WHERE d."depositAddressId" = $1 AND a.token = $2::"Token"
              AND d.status = 'CONFIRMED'::"DepositStatus"
              AND d."confirmedAt" > COALESCE((
                SELECT MAX(s."createdAt") FROM sweeps s
                WHERE s."depositAddressId" = $1 AND s.token = $2::"Token" AND s.status = 'CONFIRMED'::"SweepStatus"
              ), '-infinity'::timestamp)
            "#,
        )
        .bind(deposit_address_id)
        .bind(token.as_db())
        .fetch_one(&self.pool)
        .await?;
        Ok(credited.0)
    }

    /// Σ unattributed amounts swept per (network, token) — the dashboard
    /// figure of funds that settled no payment.
    pub async fn unattributed_totals(&self) -> Result<Vec<(String, String, RawAmount)>> {
        let rows: Vec<(String, String, RawAmount)> = sqlx::query_as(
            r#"
            SELECT network::text, token::text, COALESCE(SUM("unattributedRaw"), 0)::text
            FROM sweeps WHERE status = 'CONFIRMED'::"SweepStatus"
            GROUP BY network, token
            "#,
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }
}
