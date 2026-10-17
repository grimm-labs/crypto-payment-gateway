//! `address_book_entries`: admin-curated withdrawal destinations, usable
//! once their cooling-off delay (`activeAt`) has passed. No update: an
//! entry is deleted and re-created (which restarts the delay).

use anyhow::Result;
use chrono::{DateTime, Utc};
use domain::{DestinationKind, Network, Owner};
use sqlx::FromRow;
use uuid::Uuid;

use crate::{owner_condition, Db, OwnerFilter};

const ENTRY_COLUMNS: &str = r#"e.id, e.network::text AS network, e."destinationKind"::text AS "destinationKind", e.destination, e.label, e."isColdStorage", e."createdById", s."firstName" || ' ' || s."lastName" AS "createdByName", e."projectId", e."createdAt" AT TIME ZONE 'UTC' AS "createdAt", e."activeAt" AT TIME ZONE 'UTC' AS "activeAt", e."deletedAt" AT TIME ZONE 'UTC' AS "deletedAt", e."deletedById""#;

#[derive(Debug, Clone, FromRow)]
pub struct AddressBookEntryRow {
    pub id: String,
    pub network: String,
    #[sqlx(rename = "destinationKind")]
    pub destination_kind: String,
    pub destination: String,
    pub label: String,
    #[sqlx(rename = "isColdStorage")]
    pub is_cold_storage: bool,
    #[sqlx(rename = "createdById")]
    pub created_by_id: String,
    #[sqlx(rename = "createdByName")]
    pub created_by_name: Option<String>,
    /// Owning project; NULL = a merchant entry.
    #[sqlx(rename = "projectId")]
    pub project_id: Option<String>,
    #[sqlx(rename = "createdAt")]
    pub created_at: DateTime<Utc>,
    #[sqlx(rename = "activeAt")]
    pub active_at: DateTime<Utc>,
    #[sqlx(rename = "deletedAt")]
    pub deleted_at: Option<DateTime<Utc>>,
    #[sqlx(rename = "deletedById")]
    pub deleted_by_id: Option<String>,
}

impl AddressBookEntryRow {
    pub fn network(&self) -> Result<Network> {
        Ok(Network::from_db(&self.network)?)
    }

    pub fn destination_kind(&self) -> Result<DestinationKind> {
        Ok(DestinationKind::from_db(&self.destination_kind)?)
    }

    /// Usable: past its cooling-off delay and not deleted.
    pub fn is_active(&self, now: DateTime<Utc>) -> bool {
        self.deleted_at.is_none() && self.active_at <= now
    }

    pub fn owner(&self) -> Owner {
        Owner::from_project_id(self.project_id.as_deref())
    }
}

#[derive(Debug, Clone)]
pub struct NewAddressBookEntry<'a> {
    pub network: Network,
    pub destination_kind: DestinationKind,
    pub destination: &'a str,
    pub label: &'a str,
    pub is_cold_storage: bool,
    pub created_by_id: &'a str,
    pub active_at: DateTime<Utc>,
    /// The owner (a project, or the merchant).
    pub owner: Owner,
}

impl Db {
    /// `None` when the owner already has a live entry for the destination
    /// on that network.
    pub async fn create_address_book_entry(
        &self,
        new: &NewAddressBookEntry<'_>,
    ) -> Result<Option<AddressBookEntryRow>> {
        let id = Uuid::new_v4().to_string();
        let inserted: Option<(String,)> = sqlx::query_as(
            r#"
            INSERT INTO address_book_entries
              (id, network, "destinationKind", destination, label, "isColdStorage", "createdById", "createdAt", "activeAt", "projectId")
            VALUES ($1, $2::"Network", $3::"DestinationKind", $4, $5, $6, $7, NOW(), $8, $9)
            ON CONFLICT DO NOTHING
            RETURNING id
            "#,
        )
        .bind(&id)
        .bind(new.network.as_db())
        .bind(new.destination_kind.as_db())
        .bind(new.destination)
        .bind(new.label)
        .bind(new.is_cold_storage)
        .bind(new.created_by_id)
        .bind(new.active_at)
        .bind(new.owner.project_id())
        .fetch_optional(&self.pool)
        .await?;
        match inserted {
            Some(_) => self.find_address_book_entry(&id).await,
            None => Ok(None),
        }
    }

    pub async fn find_address_book_entry(&self, id: &str) -> Result<Option<AddressBookEntryRow>> {
        let row = sqlx::query_as::<_, AddressBookEntryRow>(&format!(
            r#"SELECT {ENTRY_COLUMNS} FROM address_book_entries e
               LEFT JOIN staff_users s ON s.id = e."createdById" WHERE e.id = $1"#
        ))
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    /// Live entries (not deleted) of `owner`, newest first; `active_only`
    /// also hides the ones still in their cooling-off delay.
    pub async fn list_address_book_entries(
        &self,
        network: Option<Network>,
        active_only: bool,
        owner: &OwnerFilter,
    ) -> Result<Vec<AddressBookEntryRow>> {
        let (project_id, merchant_only) = owner.binds();
        let rows = sqlx::query_as::<_, AddressBookEntryRow>(&format!(
            r#"SELECT {ENTRY_COLUMNS} FROM address_book_entries e
               LEFT JOIN staff_users s ON s.id = e."createdById"
               WHERE e."deletedAt" IS NULL
                 AND ($1::text IS NULL OR e.network = $1::"Network")
                 AND (NOT $2 OR e."activeAt" <= (NOW() AT TIME ZONE 'UTC'))
                 AND {}
               ORDER BY e."createdAt" DESC"#,
            owner_condition(r#"e."projectId""#, "$3", "$4")
        ))
        .bind(network.map(|n| n.as_db()))
        .bind(active_only)
        .bind(project_id)
        .bind(merchant_only)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// The owner's live entry for a normalized destination, if any.
    pub async fn find_address_book_entry_by_destination(
        &self,
        owner: &Owner,
        network: Network,
        destination: &str,
    ) -> Result<Option<AddressBookEntryRow>> {
        let row = sqlx::query_as::<_, AddressBookEntryRow>(&format!(
            r#"SELECT {ENTRY_COLUMNS} FROM address_book_entries e
               LEFT JOIN staff_users s ON s.id = e."createdById"
               WHERE e."deletedAt" IS NULL AND e.network = $1::"Network" AND e.destination = $2
                 AND e."projectId" IS NOT DISTINCT FROM $3"#
        ))
        .bind(network.as_db())
        .bind(destination)
        .bind(owner.project_id())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    /// Immediate soft delete; `None` when absent or already deleted.
    pub async fn delete_address_book_entry(
        &self,
        id: &str,
        deleted_by_id: &str,
    ) -> Result<Option<AddressBookEntryRow>> {
        let updated: Option<(String,)> = sqlx::query_as(
            r#"UPDATE address_book_entries SET "deletedAt" = NOW(), "deletedById" = $2
               WHERE id = $1 AND "deletedAt" IS NULL RETURNING id"#,
        )
        .bind(id)
        .bind(deleted_by_id)
        .fetch_optional(&self.pool)
        .await?;
        match updated {
            Some(_) => self.find_address_book_entry(id).await,
            None => Ok(None),
        }
    }
}
