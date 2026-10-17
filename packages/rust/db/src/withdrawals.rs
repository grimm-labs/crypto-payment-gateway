//! `withdrawals`: admission under per-asset advisory locks, worker claims,
//! and status transitions (always conditional on the expected current
//! status, so two actors can never both move the same withdrawal).

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use domain::{CancelReason, DestinationKind, Network, Owner, Token, WithdrawalStatus};
use serde_json::Value;
use sqlx::{FromRow, PgConnection, Postgres, Transaction};
use uuid::Uuid;

use crate::{owner_condition, Db, OwnerFilter, RawAmount};

pub(crate) const WITHDRAWAL_COLUMNS: &str = r#"id, "idempotencyKey", "apiKeyId", "staffId", "projectId", "paramsHash", network::text AS network, token::text AS token, "destinationKind"::text AS "destinationKind", destination, "addressBookEntryId", "solanaTokenAccount", "amountRaw"::text AS "amountRaw", "feeEstimateRaw"::text AS "feeEstimateRaw", "feeCapRaw"::text AS "feeCapRaw", "feeRaw"::text AS "feeRaw", "feeToken"::text AS "feeToken", status::text AS status, "approvalRequired", "approvedById", "approvedAt" AT TIME ZONE 'UTC' AS "approvedAt", "selfApprovalNotBefore" AT TIME ZONE 'UTC' AS "selfApprovalNotBefore", "approvalExpiresAt" AT TIME ZONE 'UTC' AS "approvalExpiresAt", "cancelledById", "cancelReason", "txHash", confirmations, "failureReason", reference, comment, metadata, "createdAt" AT TIME ZONE 'UTC' AS "createdAt", "updatedAt" AT TIME ZONE 'UTC' AS "updatedAt", "broadcastAt" AT TIME ZONE 'UTC' AS "broadcastAt", "confirmedAt" AT TIME ZONE 'UTC' AS "confirmedAt""#;

#[derive(Debug, Clone, FromRow)]
pub struct WithdrawalRow {
    pub id: String,
    #[sqlx(rename = "idempotencyKey")]
    pub idempotency_key: String,
    #[sqlx(rename = "apiKeyId")]
    pub api_key_id: Option<String>,
    #[sqlx(rename = "staffId")]
    pub staff_id: Option<String>,
    /// Owning project (a key's project, or the admin's choice); NULL = the
    /// merchant funds.
    #[sqlx(rename = "projectId")]
    pub project_id: Option<String>,
    #[sqlx(rename = "paramsHash")]
    pub params_hash: String,
    pub network: String,
    pub token: String,
    #[sqlx(rename = "destinationKind")]
    pub destination_kind: String,
    pub destination: String,
    #[sqlx(rename = "addressBookEntryId")]
    pub address_book_entry_id: Option<String>,
    #[sqlx(rename = "amountRaw")]
    pub amount_raw: RawAmount,
    #[sqlx(rename = "feeEstimateRaw")]
    pub fee_estimate_raw: Option<RawAmount>,
    #[sqlx(rename = "feeCapRaw")]
    pub fee_cap_raw: Option<RawAmount>,
    #[sqlx(rename = "feeRaw")]
    pub fee_raw: Option<RawAmount>,
    #[sqlx(rename = "feeToken")]
    pub fee_token: String,
    pub status: String,
    #[sqlx(rename = "approvalRequired")]
    pub approval_required: bool,
    #[sqlx(rename = "approvedById")]
    pub approved_by_id: Option<String>,
    #[sqlx(rename = "approvedAt")]
    pub approved_at: Option<DateTime<Utc>>,
    #[sqlx(rename = "selfApprovalNotBefore")]
    pub self_approval_not_before: Option<DateTime<Utc>>,
    #[sqlx(rename = "approvalExpiresAt")]
    pub approval_expires_at: Option<DateTime<Utc>>,
    #[sqlx(rename = "cancelledById")]
    pub cancelled_by_id: Option<String>,
    #[sqlx(rename = "cancelReason")]
    pub cancel_reason: Option<String>,
    #[sqlx(rename = "txHash")]
    pub tx_hash: Option<String>,
    pub confirmations: i32,
    #[sqlx(rename = "failureReason")]
    pub failure_reason: Option<String>,
    pub reference: Option<String>,
    pub comment: Option<String>,
    pub metadata: Value,
    /// Solana USDT: the token account resolved at admission.
    #[sqlx(rename = "solanaTokenAccount")]
    pub solana_token_account: Option<String>,
    #[sqlx(rename = "createdAt")]
    pub created_at: DateTime<Utc>,
    #[sqlx(rename = "updatedAt")]
    pub updated_at: DateTime<Utc>,
    #[sqlx(rename = "broadcastAt")]
    pub broadcast_at: Option<DateTime<Utc>>,
    #[sqlx(rename = "confirmedAt")]
    pub confirmed_at: Option<DateTime<Utc>>,
}

impl WithdrawalRow {
    pub fn network(&self) -> Result<Network> {
        Ok(Network::from_db(&self.network)?)
    }

    pub fn token(&self) -> Result<Token> {
        Ok(Token::from_db(&self.token)?)
    }

    pub fn fee_token(&self) -> Result<Token> {
        Ok(Token::from_db(&self.fee_token)?)
    }

    pub fn status(&self) -> Result<WithdrawalStatus> {
        Ok(WithdrawalStatus::from_db(&self.status)?)
    }

    pub fn destination_kind(&self) -> Result<DestinationKind> {
        Ok(DestinationKind::from_db(&self.destination_kind)?)
    }

    pub fn owner(&self) -> Owner {
        Owner::from_project_id(self.project_id.as_deref())
    }
}

/// A withdrawal being admitted. Exactly one of `api_key_id` / `staff_id`.
#[derive(Debug, Clone)]
pub struct NewWithdrawal<'a> {
    /// A fresh UUID when `None`.
    pub id: Option<&'a str>,
    pub idempotency_key: &'a str,
    pub api_key_id: Option<&'a str>,
    pub staff_id: Option<&'a str>,
    /// The owner: a project, or the merchant funds.
    pub owner: Owner,
    pub params_hash: &'a str,
    pub network: Network,
    pub token: Token,
    pub destination_kind: DestinationKind,
    pub destination: &'a str,
    pub address_book_entry_id: Option<&'a str>,
    /// Solana USDT: the token account the transfer goes to.
    pub solana_token_account: Option<&'a str>,
    pub amount_raw: u128,
    pub fee_estimate_raw: u128,
    pub fee_cap_raw: u128,
    pub fee_token: Token,
    /// `PendingApproval` or `Queued`.
    pub status: WithdrawalStatus,
    pub approval_required: bool,
    pub self_approval_not_before: Option<DateTime<Utc>>,
    pub approval_expires_at: Option<DateTime<Utc>>,
    pub reference: Option<&'a str>,
    pub comment: Option<&'a str>,
    pub metadata: &'a Value,
}

/// What the reserving withdrawals of one (network, token) hold back.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Reservations {
    /// Σ amounts of withdrawals of that token on that network.
    pub amount: u128,
    /// Σ estimated fees of every reserving withdrawal on that network whose
    /// fee is paid in that token (the gas token).
    pub fees: u128,
}

#[derive(Debug, Clone, Default)]
pub struct WithdrawalFilter {
    pub status: Option<WithdrawalStatus>,
    pub network: Option<Network>,
    pub token: Option<Token>,
    /// Staff filter on the requesting key.
    pub api_key_id: Option<String>,
    /// A project's withdrawals, the merchant's, or every one.
    pub owner: OwnerFilter,
}

const RESERVING: &str = r#"('PENDING_APPROVAL', 'QUEUED', 'SIGNED', 'BROADCAST', 'STUCK')"#;
const COUNTED_IN_LIMITS: &str =
    r#"('PENDING_APPROVAL', 'QUEUED', 'SIGNED', 'BROADCAST', 'STUCK', 'CONFIRMED')"#;

/// Admission of withdrawals of some assets, serialized across every API
/// instance by per-asset advisory locks held until `commit` (invariant 5):
/// the limit and balance checks and the insert happen atomically.
pub struct WithdrawalAdmission {
    tx: Transaction<'static, Postgres>,
}

fn asset_lock_key(token: Token) -> String {
    format!("withdrawal-asset:{}", token.as_db())
}

impl Db {
    /// Lock the assets a withdrawal touches — its token and the network's
    /// fee token — in a fixed order (no deadlock between two admissions).
    pub async fn begin_withdrawal_admission(&self, tokens: &[Token]) -> Result<WithdrawalAdmission> {
        let mut keys: Vec<String> = tokens.iter().map(|t| asset_lock_key(*t)).collect();
        keys.sort();
        keys.dedup();
        let mut tx = self.pool.begin().await?;
        for key in keys {
            sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1))")
                .bind(key)
                .execute(&mut *tx)
                .await?;
        }
        Ok(WithdrawalAdmission { tx })
    }

    pub async fn find_withdrawal(&self, id: &str) -> Result<Option<WithdrawalRow>> {
        let row = sqlx::query_as::<_, WithdrawalRow>(&format!(
            "SELECT {WITHDRAWAL_COLUMNS} FROM withdrawals WHERE id = $1"
        ))
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    /// The merchant surface's lookup (invariant 2): the withdrawal only when
    /// it matches `filter`'s owner (and key, for a legacy key), in the query
    /// itself.
    pub async fn find_withdrawal_visible(&self, id: &str, filter: &WithdrawalFilter) -> Result<Option<WithdrawalRow>> {
        let (project_id, merchant_only) = filter.owner.binds();
        let row = sqlx::query_as::<_, WithdrawalRow>(&format!(
            r#"SELECT {WITHDRAWAL_COLUMNS} FROM withdrawals
               WHERE id = $1 AND ($2::text IS NULL OR "apiKeyId" = $2) AND {}"#,
            owner_condition(r#""projectId""#, "$3", "$4")
        ))
        .bind(id)
        .bind(filter.api_key_id.as_deref())
        .bind(project_id)
        .bind(merchant_only)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    pub async fn find_withdrawal_by_idempotency_key(&self, key: &str) -> Result<Option<WithdrawalRow>> {
        let row = sqlx::query_as::<_, WithdrawalRow>(&format!(
            r#"SELECT {WITHDRAWAL_COLUMNS} FROM withdrawals WHERE "idempotencyKey" = $1"#
        ))
        .bind(key)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    pub async fn list_withdrawals(
        &self,
        filter: &WithdrawalFilter,
        limit: i64,
        offset: i64,
    ) -> Result<(Vec<WithdrawalRow>, i64)> {
        let status = filter.status.map(|s| s.as_db());
        let network = filter.network.map(|n| n.as_db());
        let token = filter.token.map(|t| t.as_db());
        let (project_id, merchant_only) = filter.owner.binds();
        let condition = format!(
            r#"
            ($1::text IS NULL OR status = $1::"WithdrawalStatus")
            AND ($2::text IS NULL OR network = $2::"Network")
            AND ($3::text IS NULL OR token = $3::"Token")
            AND ($4::text IS NULL OR "apiKeyId" = $4)
            AND {}
        "#,
            owner_condition(r#""projectId""#, "$5", "$6")
        );
        let rows = sqlx::query_as::<_, WithdrawalRow>(&format!(
            r#"SELECT {WITHDRAWAL_COLUMNS} FROM withdrawals WHERE {condition}
               ORDER BY "createdAt" DESC, id DESC LIMIT $7 OFFSET $8"#
        ))
        .bind(status)
        .bind(network)
        .bind(token)
        .bind(filter.api_key_id.as_deref())
        .bind(project_id)
        .bind(merchant_only)
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.pool)
        .await?;
        let (total,): (i64,) =
            sqlx::query_as(&format!("SELECT COUNT(*) FROM withdrawals WHERE {condition}"))
                .bind(status)
                .bind(network)
                .bind(token)
                .bind(filter.api_key_id.as_deref())
                .bind(project_id)
                .bind(merchant_only)
                .fetch_one(&self.pool)
                .await?;
        Ok((rows, total))
    }

    pub async fn count_withdrawals_with_status(&self, status: WithdrawalStatus) -> Result<i64> {
        let (count,): (i64,) = sqlx::query_as(
            r#"SELECT COUNT(*) FROM withdrawals WHERE status = $1::"WithdrawalStatus""#,
        )
        .bind(status.as_db())
        .fetch_one(&self.pool)
        .await?;
        Ok(count)
    }

    /// Reservations of one (network, token), outside any admission (the
    /// treasury view).
    pub async fn withdrawal_reservations(&self, network: Network, token: Token) -> Result<Reservations> {
        let mut conn = self.pool.acquire().await?;
        reservations(&mut conn, network, token, None).await
    }

    /// Withdrawals in a status, oldest first (worker scans: approval
    /// expiry, confirmation tracking, Lightning reconciliation).
    pub async fn withdrawals_with_status(
        &self,
        status: WithdrawalStatus,
        network: Option<Network>,
    ) -> Result<Vec<WithdrawalRow>> {
        let rows = sqlx::query_as::<_, WithdrawalRow>(&format!(
            r#"SELECT {WITHDRAWAL_COLUMNS} FROM withdrawals
               WHERE status = $1::"WithdrawalStatus" AND ($2::text IS NULL OR network = $2::"Network")
               ORDER BY "createdAt" ASC"#
        ))
        .bind(status.as_db())
        .bind(network.map(|n| n.as_db()))
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// SIGNED / BROADCAST withdrawals of `network` with no transaction that
    /// can still land (all expired or dropped) and none that landed: they
    /// are signed again (re-checked under the row lock,
    /// `outbound::has_open_or_landed_tx`).
    pub async fn withdrawals_without_live_tx(&self, network: Network) -> Result<Vec<WithdrawalRow>> {
        let rows = sqlx::query_as::<_, WithdrawalRow>(&format!(
            r#"SELECT {WITHDRAWAL_COLUMNS} FROM withdrawals w
               WHERE w.network = $1::"Network"
                 AND w.status IN ('SIGNED'::"WithdrawalStatus", 'BROADCAST'::"WithdrawalStatus")
                 AND NOT EXISTS (
                   SELECT 1 FROM outbound_transactions o
                   WHERE o."withdrawalId" = w.id
                     AND o.status IN ('SIGNED'::"OutboundTxStatus", 'BROADCAST'::"OutboundTxStatus",
                                      'CONFIRMED'::"OutboundTxStatus", 'FAILED'::"OutboundTxStatus")
                 )
               ORDER BY w."createdAt" ASC"#
        ))
        .bind(network.as_db())
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// Move a withdrawal from one of `from` to `to`, setting the given
    /// optional fields (absent = untouched). `None` when it was not in an
    /// expected status any more (someone else moved it first).
    pub async fn transition_withdrawal(
        &self,
        id: &str,
        from: &[WithdrawalStatus],
        to: WithdrawalStatus,
        update: &WithdrawalUpdate,
    ) -> Result<Option<WithdrawalRow>> {
        let mut conn = self.pool.acquire().await?;
        transition(&mut conn, id, from, to, update).await
    }

    /// Cancel every approval request past its deadline (`expired`),
    /// releasing their reservations. Returns the cancelled rows.
    pub async fn expire_pending_approvals(&self) -> Result<Vec<WithdrawalRow>> {
        let rows = sqlx::query_as::<_, WithdrawalRow>(&format!(
            r#"
            UPDATE withdrawals
            SET status = 'CANCELLED'::"WithdrawalStatus", "cancelReason" = $1, "updatedAt" = NOW()
            WHERE status = 'PENDING_APPROVAL'::"WithdrawalStatus"
              AND "approvalExpiresAt" < (NOW() AT TIME ZONE 'UTC')
            RETURNING {WITHDRAWAL_COLUMNS}
            "#
        ))
        .bind(CancelReason::Expired.as_str())
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// A QUEUED withdrawal found waiting goes to the back of the queue
    /// (claims take the least recently touched first).
    pub async fn touch_queued_withdrawal(&self, id: &str) -> Result<()> {
        sqlx::query(r#"UPDATE withdrawals SET "updatedAt" = NOW() WHERE id = $1 AND status = 'QUEUED'::"WithdrawalStatus""#)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Claim the least recently touched QUEUED withdrawal of `network` for a
    /// worker, except those in `skip` (already found waiting this tick). The row stays
    /// locked (`FOR UPDATE SKIP LOCKED`: another container skips it) until
    /// the claim commits or drops.
    pub async fn claim_queued_withdrawal(&self, network: Network, skip: &[String]) -> Result<Option<WithdrawalClaim>> {
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query_as::<_, WithdrawalRow>(&format!(
            r#"SELECT {WITHDRAWAL_COLUMNS} FROM withdrawals
               WHERE status = 'QUEUED'::"WithdrawalStatus" AND network = $1::"Network" AND NOT (id = ANY($2))
               ORDER BY "updatedAt" ASC, "createdAt" ASC LIMIT 1
               FOR UPDATE SKIP LOCKED"#
        ))
        .bind(network.as_db())
        .bind(skip)
        .fetch_optional(&mut *tx)
        .await?;
        Ok(row.map(|row| WithdrawalClaim { tx, row }))
    }

    /// Lock one withdrawal (any status) for an admin action or a
    /// replacement, skipping it if a worker holds it.
    pub async fn claim_withdrawal(&self, id: &str) -> Result<Option<WithdrawalClaim>> {
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query_as::<_, WithdrawalRow>(&format!(
            "SELECT {WITHDRAWAL_COLUMNS} FROM withdrawals WHERE id = $1 FOR UPDATE SKIP LOCKED"
        ))
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
        Ok(row.map(|row| WithdrawalClaim { tx, row }))
    }

    /// Whether `payment_id` is a gateway withdrawal (or a legacy payout
    /// request): its wallet send is reported by `withdrawal_*` webhooks (or
    /// was by the legacy flow), not by `payout_*` ones.
    pub async fn is_gateway_send(&self, payment_id: &str) -> Result<bool> {
        let (found,): (bool,) = sqlx::query_as(
            r#"
            SELECT EXISTS (SELECT 1 FROM withdrawals WHERE id = $1 OR ("network" = 'LIGHTNING'::"Network" AND "txHash" = $1))
                OR EXISTS (SELECT 1 FROM payout_requests WHERE "paymentId" = $1 OR "idempotencyKey" = $1)
            "#,
        )
        .bind(payment_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(found)
    }
}

/// Optional fields a transition may set.
#[derive(Debug, Clone, Default)]
pub struct WithdrawalUpdate {
    pub tx_hash: Option<String>,
    pub fee_raw: Option<u128>,
    pub fee_cap_raw: Option<u128>,
    pub confirmations: Option<i32>,
    pub failure_reason: Option<String>,
    pub approved_by_id: Option<String>,
    pub cancelled_by_id: Option<String>,
    pub cancel_reason: Option<CancelReason>,
    /// Set `broadcastAt` to now (first broadcast only).
    pub broadcast_now: bool,
    /// Set `confirmedAt` to now.
    pub confirmed_now: bool,
}

async fn transition(
    conn: &mut PgConnection,
    id: &str,
    from: &[WithdrawalStatus],
    to: WithdrawalStatus,
    update: &WithdrawalUpdate,
) -> Result<Option<WithdrawalRow>> {
    let from: Vec<&str> = from.iter().map(|s| s.as_db()).collect();
    let row = sqlx::query_as::<_, WithdrawalRow>(&format!(
        r#"
        UPDATE withdrawals SET
            status = $3::"WithdrawalStatus",
            "txHash" = COALESCE($4, "txHash"),
            "feeRaw" = COALESCE($5::numeric, "feeRaw"),
            "feeCapRaw" = COALESCE($6::numeric, "feeCapRaw"),
            confirmations = COALESCE($7, confirmations),
            "failureReason" = COALESCE($8, "failureReason"),
            "approvedById" = COALESCE($9, "approvedById"),
            "approvedAt" = CASE WHEN $9::text IS NULL THEN "approvedAt" ELSE NOW() END,
            "cancelledById" = COALESCE($10, "cancelledById"),
            "cancelReason" = COALESCE($11, "cancelReason"),
            "broadcastAt" = CASE WHEN $12 AND "broadcastAt" IS NULL THEN NOW() ELSE "broadcastAt" END,
            "confirmedAt" = CASE WHEN $13 THEN NOW() ELSE "confirmedAt" END,
            "updatedAt" = NOW()
        WHERE id = $1 AND status::text = ANY($2)
        RETURNING {WITHDRAWAL_COLUMNS}
        "#
    ))
    .bind(id)
    .bind(&from)
    .bind(to.as_db())
    .bind(update.tx_hash.as_deref())
    .bind(update.fee_raw.map(RawAmount))
    .bind(update.fee_cap_raw.map(RawAmount))
    .bind(update.confirmations)
    .bind(update.failure_reason.as_deref())
    .bind(update.approved_by_id.as_deref())
    .bind(update.cancelled_by_id.as_deref())
    .bind(update.cancel_reason.map(|r| r.as_str()))
    .bind(update.broadcast_now)
    .bind(update.confirmed_now)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(row)
}

async fn reservations(
    conn: &mut PgConnection,
    network: Network,
    token: Token,
    exclude_id: Option<&str>,
) -> Result<Reservations> {
    let (amount, fees): (RawAmount, RawAmount) = sqlx::query_as(&format!(
        r#"
        SELECT
            COALESCE(SUM("amountRaw") FILTER (WHERE token = $2::"Token"), 0)::text,
            COALESCE(SUM(COALESCE("feeEstimateRaw", 0)) FILTER (WHERE "feeToken" = $2::"Token"), 0)::text
        FROM withdrawals
        WHERE network = $1::"Network"
          AND status::text IN {RESERVING}
          AND ($3::text IS NULL OR id <> $3)
        "#
    ))
    .bind(network.as_db())
    .bind(token.as_db())
    .bind(exclude_id)
    .fetch_one(&mut *conn)
    .await?;
    Ok(Reservations {
        amount: amount.0,
        fees: fees.0,
    })
}

impl WithdrawalAdmission {
    /// An earlier withdrawal under this Idempotency-Key, locked.
    pub async fn find_by_idempotency_key(&mut self, key: &str) -> Result<Option<WithdrawalRow>> {
        let row = sqlx::query_as::<_, WithdrawalRow>(&format!(
            r#"SELECT {WITHDRAWAL_COLUMNS} FROM withdrawals WHERE "idempotencyKey" = $1 FOR UPDATE"#
        ))
        .bind(key)
        .fetch_optional(&mut *self.tx)
        .await?;
        Ok(row)
    }

    /// Lock one withdrawal inside the admission (approval re-validation).
    pub async fn lock_withdrawal(&mut self, id: &str) -> Result<Option<WithdrawalRow>> {
        let row = sqlx::query_as::<_, WithdrawalRow>(&format!(
            "SELECT {WITHDRAWAL_COLUMNS} FROM withdrawals WHERE id = $1 FOR UPDATE"
        ))
        .bind(id)
        .fetch_optional(&mut *self.tx)
        .await?;
        Ok(row)
    }

    /// Withdrawn (or reserved) amount of `token` over the rolling 24 h,
    /// every network together — for one merchant key when given. Legacy
    /// Lightning payout requests count for BTC while they are in the window.
    /// `exclude_id`: a withdrawal being re-validated (already counted).
    pub async fn used_last_24h(
        &mut self,
        token: Token,
        api_key_id: Option<&str>,
        exclude_id: Option<&str>,
    ) -> Result<u128> {
        let (used,): (RawAmount,) = sqlx::query_as(&format!(
            r#"
            SELECT (
                COALESCE((
                    SELECT SUM("amountRaw") FROM withdrawals
                    WHERE token = $1::"Token"
                      AND status::text IN {COUNTED_IN_LIMITS}
                      AND "createdAt" > (NOW() AT TIME ZONE 'UTC') - INTERVAL '24 hours'
                      AND ($2::text IS NULL OR "apiKeyId" = $2)
                      AND ($3::text IS NULL OR id <> $3)
                ), 0)
                + CASE WHEN $1 = 'BTC' THEN COALESCE((
                    SELECT SUM("amountSats") FROM payout_requests
                    WHERE status IN ('PENDING'::"PayoutRequestStatus", 'SENT'::"PayoutRequestStatus")
                      AND "updatedAt" > (NOW() AT TIME ZONE 'UTC') - INTERVAL '24 hours'
                      AND ($2::text IS NULL OR "apiKeyId" = $2)
                ), 0) ELSE 0 END
            )::text
            "#
        ))
        .bind(token.as_db())
        .bind(api_key_id)
        .bind(exclude_id)
        .fetch_one(&mut *self.tx)
        .await?;
        Ok(used.0)
    }

    pub async fn reservations(
        &mut self,
        network: Network,
        token: Token,
        exclude_id: Option<&str>,
    ) -> Result<Reservations> {
        reservations(&mut self.tx, network, token, exclude_id).await
    }

    pub async fn insert(&mut self, new: &NewWithdrawal<'_>) -> Result<WithdrawalRow> {
        let row = sqlx::query_as::<_, WithdrawalRow>(&format!(
            r#"
            INSERT INTO withdrawals
              (id, "idempotencyKey", "apiKeyId", "staffId", "paramsHash", network, token,
               "destinationKind", destination, "addressBookEntryId", "amountRaw", "feeEstimateRaw",
               "feeCapRaw", "feeToken", status, "approvalRequired", "selfApprovalNotBefore",
               "approvalExpiresAt", reference, comment, metadata, "solanaTokenAccount", "projectId", "createdAt", "updatedAt")
            VALUES
              ($1, $2, $3, $4, $5, $6::"Network", $7::"Token",
               $8::"DestinationKind", $9, $10, $11::numeric, $12::numeric,
               $13::numeric, $14::"Token", $15::"WithdrawalStatus", $16, $17,
               $18, $19, $20, $21, $22, $23, NOW(), NOW())
            RETURNING {WITHDRAWAL_COLUMNS}
            "#
        ))
        .bind(new.id.map(str::to_string).unwrap_or_else(|| Uuid::new_v4().to_string()))
        .bind(new.idempotency_key)
        .bind(new.api_key_id)
        .bind(new.staff_id)
        .bind(new.params_hash)
        .bind(new.network.as_db())
        .bind(new.token.as_db())
        .bind(new.destination_kind.as_db())
        .bind(new.destination)
        .bind(new.address_book_entry_id)
        .bind(RawAmount(new.amount_raw))
        .bind(RawAmount(new.fee_estimate_raw))
        .bind(RawAmount(new.fee_cap_raw))
        .bind(new.fee_token.as_db())
        .bind(new.status.as_db())
        .bind(new.approval_required)
        .bind(new.self_approval_not_before)
        .bind(new.approval_expires_at)
        .bind(new.reference)
        .bind(new.comment)
        .bind(new.metadata)
        .bind(new.solana_token_account)
        .bind(new.owner.project_id())
        .fetch_one(&mut *self.tx)
        .await
        .context("insert withdrawal")?;
        Ok(row)
    }

    pub async fn transition(
        &mut self,
        id: &str,
        from: &[WithdrawalStatus],
        to: WithdrawalStatus,
        update: &WithdrawalUpdate,
    ) -> Result<Option<WithdrawalRow>> {
        transition(&mut self.tx, id, from, to, update).await
    }

    pub async fn commit(self) -> Result<()> {
        self.tx.commit().await?;
        Ok(())
    }
}

/// A withdrawal row locked by a worker (or an admin action) until `commit`.
/// Everything signed under the claim is stored in the same transaction
/// (persist before broadcast, invariant 1).
pub struct WithdrawalClaim {
    tx: Transaction<'static, Postgres>,
    row: WithdrawalRow,
}

impl WithdrawalClaim {
    pub fn row(&self) -> &WithdrawalRow {
        &self.row
    }

    /// The claim's connection, for the outbound helpers (`crate::outbound`).
    pub fn conn(&mut self) -> &mut PgConnection {
        &mut self.tx
    }

    pub async fn transition(
        &mut self,
        to: WithdrawalStatus,
        update: &WithdrawalUpdate,
    ) -> Result<Option<WithdrawalRow>> {
        let from = self.row.status()?;
        let row = transition(&mut self.tx, &self.row.id, &[from], to, update).await?;
        if let Some(row) = &row {
            self.row = row.clone();
        }
        Ok(row)
    }

    pub async fn commit(self) -> Result<WithdrawalRow> {
        self.tx.commit().await?;
        Ok(self.row)
    }
}
