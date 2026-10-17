//! `internal_transfers` (docs/projects-spec.md §6.3, §9): an admin's ledger
//! move of one asset on one network between two owners (`NULL` = the
//! merchant funds). Append-only (DB trigger): a mistake is undone with the
//! reverse transfer.

use anyhow::Result;
use chrono::{DateTime, Utc};
use domain::{Network, Owner, Token};
use sqlx::{FromRow, PgConnection};
use uuid::Uuid;

use crate::{Db, OwnerFilter, RawAmount};

pub(crate) const TRANSFER_COLUMNS: &str = r#"id, "idempotencyKey", "paramsHash", network::text AS network, token::text AS token, "amountRaw"::text AS "amountRaw", "fromProjectId", "toProjectId", note, "staffId", "createdAt" AT TIME ZONE 'UTC' AS "createdAt""#;

#[derive(Debug, Clone, FromRow)]
pub struct InternalTransferRow {
    pub id: String,
    #[sqlx(rename = "idempotencyKey")]
    pub idempotency_key: String,
    #[sqlx(rename = "paramsHash")]
    pub params_hash: String,
    pub network: String,
    pub token: String,
    #[sqlx(rename = "amountRaw")]
    pub amount_raw: RawAmount,
    /// NULL = the merchant funds.
    #[sqlx(rename = "fromProjectId")]
    pub from_project_id: Option<String>,
    #[sqlx(rename = "toProjectId")]
    pub to_project_id: Option<String>,
    pub note: String,
    #[sqlx(rename = "staffId")]
    pub staff_id: String,
    #[sqlx(rename = "createdAt")]
    pub created_at: DateTime<Utc>,
}

impl InternalTransferRow {
    pub fn network(&self) -> Result<Network> {
        Ok(Network::from_db(&self.network)?)
    }

    pub fn token(&self) -> Result<Token> {
        Ok(Token::from_db(&self.token)?)
    }

    pub fn from_owner(&self) -> Owner {
        Owner::from_project_id(self.from_project_id.as_deref())
    }

    pub fn to_owner(&self) -> Owner {
        Owner::from_project_id(self.to_project_id.as_deref())
    }
}

/// A transfer being recorded (spec §9), inside the per-asset lock.
#[derive(Debug, Clone)]
pub struct NewInternalTransfer<'a> {
    pub idempotency_key: &'a str,
    pub params_hash: &'a str,
    pub network: Network,
    pub token: Token,
    pub amount_raw: u128,
    pub from: Owner,
    pub to: Owner,
    pub note: &'a str,
    pub staff_id: &'a str,
}

#[derive(Debug, Clone, Default)]
pub struct TransferFilter {
    /// Matched on either side (`Merchant` = a side with no project).
    pub owner: OwnerFilter,
    pub network: Option<Network>,
    pub token: Option<Token>,
}

/// Insert on `conn` (the locked admission transaction, spec §9).
pub async fn insert_internal_transfer(conn: &mut PgConnection, new: &NewInternalTransfer<'_>) -> Result<InternalTransferRow> {
    let row = sqlx::query_as::<_, InternalTransferRow>(&format!(
        r#"
        INSERT INTO internal_transfers
          (id, "idempotencyKey", "paramsHash", network, token, "amountRaw", "fromProjectId", "toProjectId", note, "staffId", "createdAt")
        VALUES ($1, $2, $3, $4::"Network", $5::"Token", $6::numeric, $7, $8, $9, $10, NOW())
        RETURNING {TRANSFER_COLUMNS}
        "#
    ))
    .bind(Uuid::new_v4().to_string())
    .bind(new.idempotency_key)
    .bind(new.params_hash)
    .bind(new.network.as_db())
    .bind(new.token.as_db())
    .bind(RawAmount(new.amount_raw))
    .bind(new.from.project_id())
    .bind(new.to.project_id())
    .bind(new.note)
    .bind(new.staff_id)
    .fetch_one(&mut *conn)
    .await?;
    Ok(row)
}

/// The transfer under an Idempotency-Key, if any (on `conn`, locked).
pub async fn find_internal_transfer_by_key(conn: &mut PgConnection, key: &str) -> Result<Option<InternalTransferRow>> {
    let row = sqlx::query_as::<_, InternalTransferRow>(&format!(
        r#"SELECT {TRANSFER_COLUMNS} FROM internal_transfers WHERE "idempotencyKey" = $1"#
    ))
    .bind(key)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(row)
}

/// SQL of a side filter: the project on either side, or a merchant side.
fn side_condition(project: &str, merchant: &str) -> String {
    format!(
        r#"(({project}::text IS NULL OR "fromProjectId" = {project} OR "toProjectId" = {project})
            AND (NOT {merchant} OR "fromProjectId" IS NULL OR "toProjectId" IS NULL))"#
    )
}

impl Db {
    pub async fn find_internal_transfer(&self, id: &str) -> Result<Option<InternalTransferRow>> {
        let row = sqlx::query_as::<_, InternalTransferRow>(&format!(
            "SELECT {TRANSFER_COLUMNS} FROM internal_transfers WHERE id = $1"
        ))
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    /// The merchant surface's lookup (invariant 2): the transfer only when
    /// the project is one of its sides, in the query itself.
    pub async fn find_internal_transfer_for_project(&self, id: &str, project_id: &str) -> Result<Option<InternalTransferRow>> {
        let row = sqlx::query_as::<_, InternalTransferRow>(&format!(
            r#"SELECT {TRANSFER_COLUMNS} FROM internal_transfers
               WHERE id = $1 AND ("fromProjectId" = $2 OR "toProjectId" = $2)"#
        ))
        .bind(id)
        .bind(project_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    pub async fn list_internal_transfers(
        &self,
        filter: &TransferFilter,
        limit: i64,
        offset: i64,
    ) -> Result<(Vec<InternalTransferRow>, i64)> {
        let (project_id, merchant_only) = filter.owner.binds();
        let network = filter.network.map(|n| n.as_db());
        let token = filter.token.map(|t| t.as_db());
        let condition = format!(
            r#"{} AND ($3::text IS NULL OR network = $3::"Network") AND ($4::text IS NULL OR token = $4::"Token")"#,
            side_condition("$1", "$2")
        );
        let rows = sqlx::query_as::<_, InternalTransferRow>(&format!(
            r#"SELECT {TRANSFER_COLUMNS} FROM internal_transfers WHERE {condition}
               ORDER BY "createdAt" DESC, id DESC LIMIT $5 OFFSET $6"#
        ))
        .bind(project_id)
        .bind(merchant_only)
        .bind(network)
        .bind(token)
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.pool)
        .await?;
        let (total,): (i64,) = sqlx::query_as(&format!("SELECT COUNT(*) FROM internal_transfers WHERE {condition}"))
            .bind(project_id)
            .bind(merchant_only)
            .bind(network)
            .bind(token)
            .fetch_one(&self.pool)
            .await?;
        Ok((rows, total))
    }
}
