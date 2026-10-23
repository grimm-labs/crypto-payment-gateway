//! `outbound_transactions` (every transaction the gateway signs, stored
//! before broadcast), Ethereum nonce allocation (`signer_nonces`) and the
//! Bitcoin UTXOs held by unconfirmed gateway transactions
//! (`btc_utxo_reservations`).

use anyhow::Result;
use chrono::{DateTime, Utc};
use domain::{Network, OutboundPurpose, OutboundTxStatus, Token};
use serde_json::Value;
use sqlx::{FromRow, PgConnection};
use uuid::Uuid;

use crate::{Db, RawAmount};

const OUTBOUND_COLUMNS: &str = r#"id, purpose::text AS purpose, "withdrawalId", "sweepId", network::text AS network, token::text AS token, "fromAddress", "toAddress", "amountRaw"::text AS "amountRaw", nonce, "txHash", "signedTx", "feeParams", "feeRaw"::text AS "feeRaw", status::text AS status, "replacesId", "expiresAt" AT TIME ZONE 'UTC' AS "expiresAt", "blockNumber", confirmations, "lastError", "createdAt" AT TIME ZONE 'UTC' AS "createdAt", "updatedAt" AT TIME ZONE 'UTC' AS "updatedAt", "broadcastAt" AT TIME ZONE 'UTC' AS "broadcastAt", "confirmedAt" AT TIME ZONE 'UTC' AS "confirmedAt""#;

#[derive(Debug, Clone, FromRow)]
pub struct OutboundTxRow {
    pub id: String,
    pub purpose: String,
    #[sqlx(rename = "withdrawalId")]
    pub withdrawal_id: Option<String>,
    #[sqlx(rename = "sweepId")]
    pub sweep_id: Option<String>,
    pub network: String,
    pub token: String,
    #[sqlx(rename = "fromAddress")]
    pub from_address: String,
    #[sqlx(rename = "toAddress")]
    pub to_address: String,
    #[sqlx(rename = "amountRaw")]
    pub amount_raw: RawAmount,
    pub nonce: Option<i64>,
    #[sqlx(rename = "txHash")]
    pub tx_hash: String,
    /// What gets (re)broadcast: hex, or base64 on Solana. Public data once
    /// broadcast, but never logged nor put in audit details.
    #[sqlx(rename = "signedTx")]
    pub signed_tx: String,
    #[sqlx(rename = "feeParams")]
    pub fee_params: Value,
    #[sqlx(rename = "feeRaw")]
    pub fee_raw: Option<RawAmount>,
    pub status: String,
    #[sqlx(rename = "replacesId")]
    pub replaces_id: Option<String>,
    #[sqlx(rename = "expiresAt")]
    pub expires_at: Option<DateTime<Utc>>,
    #[sqlx(rename = "blockNumber")]
    pub block_number: Option<i64>,
    pub confirmations: i32,
    #[sqlx(rename = "lastError")]
    pub last_error: Option<String>,
    #[sqlx(rename = "createdAt")]
    pub created_at: DateTime<Utc>,
    #[sqlx(rename = "updatedAt")]
    pub updated_at: DateTime<Utc>,
    #[sqlx(rename = "broadcastAt")]
    pub broadcast_at: Option<DateTime<Utc>>,
    #[sqlx(rename = "confirmedAt")]
    pub confirmed_at: Option<DateTime<Utc>>,
}

impl OutboundTxRow {
    pub fn network(&self) -> Result<Network> {
        Ok(Network::from_db(&self.network)?)
    }

    pub fn token(&self) -> Result<Token> {
        Ok(Token::from_db(&self.token)?)
    }

    pub fn status(&self) -> Result<OutboundTxStatus> {
        Ok(OutboundTxStatus::from_db(&self.status)?)
    }

    pub fn purpose(&self) -> Result<OutboundPurpose> {
        Ok(OutboundPurpose::from_db(&self.purpose)?)
    }
}

/// A freshly signed transaction. Exactly one of `withdrawal_id` / `sweep_id`
/// (gas top-ups belong to their sweep).
#[derive(Debug, Clone)]
pub struct NewOutboundTx<'a> {
    pub purpose: OutboundPurpose,
    pub withdrawal_id: Option<&'a str>,
    pub sweep_id: Option<&'a str>,
    pub network: Network,
    pub token: Token,
    pub from_address: &'a str,
    pub to_address: &'a str,
    pub amount_raw: u128,
    pub nonce: Option<u64>,
    pub tx_hash: &'a str,
    pub signed_tx: &'a str,
    pub fee_params: Value,
    /// Most the transaction can cost (its fee is recorded on confirmation).
    pub fee_raw: Option<u128>,
    pub replaces_id: Option<&'a str>,
    pub expires_at: Option<DateTime<Utc>>,
}

/// Store a signed transaction (status SIGNED) — on the claim's connection,
/// so it commits together with the withdrawal/sweep move to SIGNED.
pub async fn insert_outbound_tx(conn: &mut PgConnection, new: &NewOutboundTx<'_>) -> Result<OutboundTxRow> {
    let row = sqlx::query_as::<_, OutboundTxRow>(&format!(
        r#"
        INSERT INTO outbound_transactions
          (id, purpose, "withdrawalId", "sweepId", network, token, "fromAddress", "toAddress",
           "amountRaw", nonce, "txHash", "signedTx", "feeParams", "feeRaw", status, "replacesId",
           "expiresAt", "createdAt", "updatedAt")
        VALUES
          ($1, $2::"OutboundPurpose", $3, $4, $5::"Network", $6::"Token", $7, $8,
           $9::numeric, $10, $11, $12, $13, $14::numeric, 'SIGNED'::"OutboundTxStatus", $15,
           $16, NOW(), NOW())
        RETURNING {OUTBOUND_COLUMNS}
        "#
    ))
    .bind(Uuid::new_v4().to_string())
    .bind(new.purpose.as_db())
    .bind(new.withdrawal_id)
    .bind(new.sweep_id)
    .bind(new.network.as_db())
    .bind(new.token.as_db())
    .bind(new.from_address)
    .bind(new.to_address)
    .bind(RawAmount(new.amount_raw))
    .bind(new.nonce.map(|n| n as i64))
    .bind(new.tx_hash)
    .bind(new.signed_tx)
    .bind(&new.fee_params)
    .bind(new.fee_raw.map(RawAmount))
    .bind(new.replaces_id)
    .bind(new.expires_at)
    .fetch_one(&mut *conn)
    .await?;
    Ok(row)
}

/// Serialize everything that allocates from one sending address (nonces,
/// UTXOs, gas) across workers and containers, until the transaction ends.
pub async fn lock_sender(conn: &mut PgConnection, network: Network, address: &str) -> Result<()> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1))")
        .bind(format!("sender:{}:{}", network.as_db(), address.to_ascii_lowercase()))
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// Whether a withdrawal has a transaction that may still land or that
/// landed — then it must never be signed again. Run on the claim's
/// connection, under the withdrawal's row lock: every signer holds it, so
/// a transaction committed by another worker is seen.
pub async fn has_open_or_landed_tx(conn: &mut PgConnection, withdrawal_id: &str) -> Result<bool> {
    let (found,): (bool,) = sqlx::query_as(
        r#"
        SELECT EXISTS (
          SELECT 1 FROM outbound_transactions WHERE "withdrawalId" = $1
            AND status IN ('SIGNED'::"OutboundTxStatus", 'BROADCAST'::"OutboundTxStatus",
                           'CONFIRMED'::"OutboundTxStatus", 'FAILED'::"OutboundTxStatus")
        )
        "#,
    )
    .bind(withdrawal_id)
    .fetch_one(&mut *conn)
    .await?;
    Ok(found)
}

/// Unconfirmed transactions (SIGNED or BROADCAST) sent from `address`,
/// counted once per nonce (a fee bump shares its nonce).
pub async fn in_flight_count(conn: &mut PgConnection, network: Network, address: &str) -> Result<i64> {
    let (count,): (i64,) = sqlx::query_as(
        r#"
        SELECT COUNT(DISTINCT COALESCE(nonce::text, id)) FROM outbound_transactions
        WHERE network = $1::"Network" AND lower("fromAddress") = lower($2)
          AND status IN ('SIGNED'::"OutboundTxStatus", 'BROADCAST'::"OutboundTxStatus")
        "#,
    )
    .bind(network.as_db())
    .bind(address)
    .fetch_one(&mut *conn)
    .await?;
    Ok(count)
}

/// Next Ethereum nonce of `address` (call under `lock_sender`). `resync_to`
/// — the chain's `pending` count — is used when the address has no
/// transaction in flight; otherwise the stored counter is authoritative.
pub async fn allocate_nonce(
    conn: &mut PgConnection,
    network: Network,
    address: &str,
    resync_to: Option<u64>,
) -> Result<u64> {
    let key = address.to_ascii_lowercase();
    let stored: Option<(i64,)> = sqlx::query_as(
        r#"SELECT "nextNonce" FROM signer_nonces WHERE network = $1::"Network" AND address = $2 FOR UPDATE"#,
    )
    .bind(network.as_db())
    .bind(&key)
    .fetch_optional(&mut *conn)
    .await?;
    let nonce = match (resync_to, stored) {
        (Some(chain), _) => chain,
        (None, Some((stored,))) => u64::try_from(stored)?,
        (None, None) => anyhow::bail!("no stored nonce for {address} and no chain nonce to start from"),
    };
    sqlx::query(
        r#"
        INSERT INTO signer_nonces (network, address, "nextNonce", "updatedAt")
        VALUES ($1::"Network", $2, $3, NOW())
        ON CONFLICT (network, address) DO UPDATE SET "nextNonce" = $3, "updatedAt" = NOW()
        "#,
    )
    .bind(network.as_db())
    .bind(&key)
    .bind(i64::try_from(nonce + 1)?)
    .execute(&mut *conn)
    .await?;
    Ok(nonce)
}

/// Mark UTXOs as spent by `outbound_id` (same transaction as its insert).
pub async fn reserve_utxos(conn: &mut PgConnection, outbound_id: &str, utxos: &[(String, u32)]) -> Result<()> {
    for (txid, vout) in utxos {
        sqlx::query(
            r#"
            INSERT INTO btc_utxo_reservations (txid, vout, "outboundTxId", "createdAt")
            VALUES ($1, $2, $3, NOW())
            ON CONFLICT (txid, vout) DO UPDATE SET "outboundTxId" = $3
            "#,
        )
        .bind(txid)
        .bind(i32::try_from(*vout)?)
        .bind(outbound_id)
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

/// A transaction-scoped advisory lock taken without waiting (`None`: another
/// session holds it), released when the returned transaction drops. It
/// never conflicts with row locks, so the holder may update rows through
/// other connections meanwhile.
pub struct AdvisoryGuard {
    _tx: sqlx::Transaction<'static, sqlx::Postgres>,
}

impl Db {
    pub async fn try_advisory_lock(&self, key: &str) -> Result<Option<AdvisoryGuard>> {
        let mut tx = self.pool.begin().await?;
        let (locked,): (bool,) = sqlx::query_as("SELECT pg_try_advisory_xact_lock(hashtext($1))")
            .bind(key)
            .fetch_one(&mut *tx)
            .await?;
        Ok(locked.then_some(AdvisoryGuard { _tx: tx }))
    }

    /// Whether `tx_hash` is a transaction the gateway signed on `network`
    /// (a gas top-up to a deposit address is never a deposit of it).
    /// Ethereum hashes come in two casings.
    pub async fn is_gateway_outbound_tx(&self, network: Network, tx_hash: &str) -> Result<bool> {
        let (found,): (bool,) = sqlx::query_as(
            r#"SELECT EXISTS (SELECT 1 FROM outbound_transactions WHERE network = $1::"Network" AND lower("txHash") = lower($2))"#,
        )
        .bind(network.as_db())
        .bind(tx_hash)
        .fetch_one(&self.pool)
        .await?;
        Ok(found)
    }

    pub async fn find_outbound_tx(&self, id: &str) -> Result<Option<OutboundTxRow>> {
        let row = sqlx::query_as::<_, OutboundTxRow>(&format!(
            "SELECT {OUTBOUND_COLUMNS} FROM outbound_transactions WHERE id = $1"
        ))
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    /// The transactions of a withdrawal (oldest first: the fee-bump chain).
    pub async fn outbound_txs_for_withdrawal(&self, withdrawal_id: &str) -> Result<Vec<OutboundTxRow>> {
        let rows = sqlx::query_as::<_, OutboundTxRow>(&format!(
            r#"SELECT {OUTBOUND_COLUMNS} FROM outbound_transactions WHERE "withdrawalId" = $1 ORDER BY "createdAt" ASC"#
        ))
        .bind(withdrawal_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// Landed (CONFIRMED / reverted FAILED) transactions of `network` whose
    /// withdrawal is still open: a run died between the two updates; the
    /// withdrawal is settled from the stored outcome.
    pub async fn landed_txs_of_open_withdrawals(&self, network: Network) -> Result<Vec<OutboundTxRow>> {
        let rows = sqlx::query_as::<_, OutboundTxRow>(&format!(
            r#"SELECT {OUTBOUND_COLUMNS} FROM outbound_transactions
               WHERE network = $1::"Network"
                 AND status IN ('CONFIRMED'::"OutboundTxStatus", 'FAILED'::"OutboundTxStatus")
                 AND "withdrawalId" IN (
                   SELECT id FROM withdrawals
                   WHERE status IN ('SIGNED'::"WithdrawalStatus", 'BROADCAST'::"WithdrawalStatus", 'STUCK'::"WithdrawalStatus")
                 )
               ORDER BY "createdAt" ASC"#
        ))
        .bind(network.as_db())
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    pub async fn outbound_txs_for_sweep(&self, sweep_id: &str) -> Result<Vec<OutboundTxRow>> {
        let rows = sqlx::query_as::<_, OutboundTxRow>(&format!(
            r#"SELECT {OUTBOUND_COLUMNS} FROM outbound_transactions WHERE "sweepId" = $1 ORDER BY "createdAt" ASC"#
        ))
        .bind(sweep_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// Transactions of `network` that can still land (SIGNED / BROADCAST).
    pub async fn live_outbound_txs(&self, network: Network) -> Result<Vec<OutboundTxRow>> {
        let rows = sqlx::query_as::<_, OutboundTxRow>(&format!(
            r#"SELECT {OUTBOUND_COLUMNS} FROM outbound_transactions
               WHERE network = $1::"Network" AND status IN ('SIGNED'::"OutboundTxStatus", 'BROADCAST'::"OutboundTxStatus")
               ORDER BY "createdAt" ASC"#
        ))
        .bind(network.as_db())
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// SIGNED transactions older than `older_than_secs` — a crash between
    /// signing and broadcasting; they are re-broadcast as stored.
    pub async fn stale_signed_outbound_txs(&self, older_than_secs: i64) -> Result<Vec<OutboundTxRow>> {
        let rows = sqlx::query_as::<_, OutboundTxRow>(&format!(
            r#"SELECT {OUTBOUND_COLUMNS} FROM outbound_transactions
               WHERE status = 'SIGNED'::"OutboundTxStatus"
                 AND "createdAt" < (NOW() AT TIME ZONE 'UTC') - make_interval(secs => $1)
               ORDER BY "createdAt" ASC"#
        ))
        .bind(older_than_secs as f64)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// Move a transaction from one of `from` to `to` with the given fields.
    /// `None` when it was not in an expected status.
    pub async fn transition_outbound_tx(
        &self,
        id: &str,
        from: &[OutboundTxStatus],
        to: OutboundTxStatus,
        update: &OutboundUpdate,
    ) -> Result<Option<OutboundTxRow>> {
        let from: Vec<&str> = from.iter().map(|s| s.as_db()).collect();
        let row = sqlx::query_as::<_, OutboundTxRow>(&format!(
            r#"
            UPDATE outbound_transactions SET
                status = $3::"OutboundTxStatus",
                "blockNumber" = COALESCE($4, "blockNumber"),
                confirmations = COALESCE($5, confirmations),
                "feeRaw" = COALESCE($6::numeric, "feeRaw"),
                "lastError" = COALESCE($7, "lastError"),
                "broadcastAt" = CASE WHEN $3 = 'BROADCAST' AND "broadcastAt" IS NULL THEN NOW() ELSE "broadcastAt" END,
                "confirmedAt" = CASE WHEN $3 = 'CONFIRMED' THEN NOW() ELSE "confirmedAt" END,
                "updatedAt" = NOW()
            WHERE id = $1 AND status::text = ANY($2)
            RETURNING {OUTBOUND_COLUMNS}
            "#
        ))
        .bind(id)
        .bind(&from)
        .bind(to.as_db())
        .bind(update.block_number)
        .bind(update.confirmations)
        .bind(update.fee_raw.map(RawAmount))
        .bind(update.last_error.as_deref())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    /// Record progress of a broadcast transaction without changing status.
    pub async fn update_outbound_progress(
        &self,
        id: &str,
        block_number: Option<i64>,
        confirmations: i32,
    ) -> Result<()> {
        sqlx::query(
            r#"UPDATE outbound_transactions SET "blockNumber" = COALESCE($2, "blockNumber"), confirmations = $3, "updatedAt" = NOW() WHERE id = $1"#,
        )
        .bind(id)
        .bind(block_number)
        .bind(confirmations)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn record_outbound_error(&self, id: &str, error: &str) -> Result<()> {
        sqlx::query(r#"UPDATE outbound_transactions SET "lastError" = $2, "updatedAt" = NOW() WHERE id = $1"#)
            .bind(id)
            .bind(error)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Every UTXO held by an unconfirmed gateway transaction.
    pub async fn reserved_utxos(&self) -> Result<Vec<(String, u32)>> {
        let rows: Vec<(String, i32)> = sqlx::query_as("SELECT txid, vout FROM btc_utxo_reservations")
            .fetch_all(&self.pool)
            .await?;
        Ok(rows
            .into_iter()
            .filter_map(|(txid, vout)| u32::try_from(vout).ok().map(|vout| (txid, vout)))
            .collect())
    }

    /// The transaction failed or was dropped: its inputs are free again.
    /// `reserve_utxos` outside a claim (an upsert: moves the outpoints to
    /// `outbound_id`).
    pub async fn reserve_utxos_for(&self, outbound_id: &str, utxos: &[(String, u32)]) -> Result<()> {
        let mut conn = self.pool.acquire().await?;
        reserve_utxos(&mut conn, outbound_id, utxos).await
    }

    pub async fn release_utxo_reservations(&self, outbound_id: &str) -> Result<()> {
        sqlx::query(r#"DELETE FROM btc_utxo_reservations WHERE "outboundTxId" = $1"#)
            .bind(outbound_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

/// Optional fields of an outbound transition.
#[derive(Debug, Clone, Default)]
pub struct OutboundUpdate {
    pub block_number: Option<i64>,
    pub confirmations: Option<i32>,
    pub fee_raw: Option<u128>,
    pub last_error: Option<String>,
}
