//! `internal_transfers` (docs/projects-spec.md §6.3, §9): an admin's ledger
//! move of one asset on one network between two owners (`NULL` = the
//! merchant funds). Append-only (DB trigger): a mistake is undone with the
//! reverse transfer.

use anyhow::Result;
use chrono::{DateTime, Utc};
use domain::{Network, Owner, Token};
use sqlx::FromRow;

use crate::{Db, RawAmount};

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
}
