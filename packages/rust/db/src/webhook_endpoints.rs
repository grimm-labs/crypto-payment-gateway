use anyhow::Result;
use chrono::{DateTime, Utc};
use domain::{Owner, WebhookEventType};
use sqlx::FromRow;
use uuid::Uuid;

use crate::{owner_condition, Db, OwnerFilter};

/// The endpoints an event fans out to (spec §10.1), over `webhook_endpoints
/// e`: active, subscribed to the event at `$event`, and — when `$filtered`
/// (once projects exist) — owned by the project at `$project` (NULL: the
/// merchant). `jobs::webhooks::endpoints_for` and the deliveries written
/// with a status change both use it.
pub(crate) fn subscribed_endpoints(event: &str, filtered: &str, project: &str) -> String {
    format!(r#"e."isActive" = true AND {event} = ANY(e.events) AND (NOT {filtered} OR e."projectId" IS NOT DISTINCT FROM {project})"#)
}

const WEBHOOK_ENDPOINT_COLUMNS: &str = r#"id, url, "isActive", events, secret, "createdById", "projectId", "createdAt" AT TIME ZONE 'UTC' AS "createdAt", "updatedAt" AT TIME ZONE 'UTC' AS "updatedAt""#;

#[derive(Clone, FromRow)]
pub struct WebhookEndpointRow {
    pub id: String,
    pub url: String,
    #[sqlx(rename = "isActive")]
    pub is_active: bool,
    /// DB values of the subscribed `WebhookEventType`s.
    pub events: Vec<String>,
    /// Signing secret for X-Webhook-Signature; NULL = unsigned deliveries.
    /// Never expose through the API — only `has_secret`.
    pub secret: Option<String>,
    #[sqlx(rename = "createdById")]
    pub created_by_id: String,
    /// Owning project; NULL = a merchant endpoint (instance events), fixed
    /// at creation.
    #[sqlx(rename = "projectId")]
    pub project_id: Option<String>,
    #[sqlx(rename = "createdAt")]
    pub created_at: DateTime<Utc>,
    #[sqlx(rename = "updatedAt")]
    pub updated_at: DateTime<Utc>,
}

/// Manual: the signing secret must never reach a log line.
impl std::fmt::Debug for WebhookEndpointRow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebhookEndpointRow")
            .field("id", &self.id)
            .field("url", &self.url)
            .field("is_active", &self.is_active)
            .field("events", &self.events)
            .field("secret", &self.secret.as_ref().map(|_| "<redacted>"))
            .field("created_by_id", &self.created_by_id)
            .field("project_id", &self.project_id)
            .field("created_at", &self.created_at)
            .field("updated_at", &self.updated_at)
            .finish()
    }
}

impl WebhookEndpointRow {
    pub fn has_secret(&self) -> bool {
        self.secret.as_deref().is_some_and(|s| !s.is_empty())
    }
}

impl WebhookEndpointRow {
    pub fn accepts_event(&self, event: WebhookEventType) -> bool {
        self.events.iter().any(|e| e == event.as_db())
    }

    pub fn owner(&self) -> Owner {
        Owner::from_project_id(self.project_id.as_deref())
    }
}

/// List row: `WebhookEndpointRow` plus the creator's display name.
#[derive(Debug, Clone, FromRow)]
pub struct WebhookEndpointListRow {
    pub id: String,
    pub url: String,
    #[sqlx(rename = "isActive")]
    pub is_active: bool,
    pub events: Vec<String>,
    /// The secret itself never leaves the delivery path.
    #[sqlx(rename = "hasSecret")]
    pub has_secret: bool,
    #[sqlx(rename = "createdById")]
    pub created_by_id: String,
    #[sqlx(rename = "createdByName")]
    pub created_by_name: Option<String>,
    #[sqlx(rename = "projectId")]
    pub project_id: Option<String>,
    #[sqlx(rename = "createdAt")]
    pub created_at: DateTime<Utc>,
    #[sqlx(rename = "updatedAt")]
    pub updated_at: DateTime<Utc>,
}

impl Db {
    /// `None` when the owner already has an endpoint with this URL.
    pub async fn create_webhook_endpoint(
        &self,
        url: &str,
        events: &[WebhookEventType],
        secret: Option<&str>,
        created_by_id: &str,
        owner: &Owner,
    ) -> Result<Option<WebhookEndpointRow>> {
        let id = Uuid::new_v4().to_string();
        let now = Utc::now();
        let events_db: Vec<String> = events.iter().map(|e| e.as_db().to_string()).collect();
        let row = sqlx::query_as::<_, WebhookEndpointRow>(&format!(
            r#"
            INSERT INTO webhook_endpoints
              (id, url, "isActive", events, secret, "createdById", "projectId", "createdAt", "updatedAt")
            VALUES
              ($1, $2, true, $3, $4, $5, $6, $7, $7)
            ON CONFLICT DO NOTHING
            RETURNING {WEBHOOK_ENDPOINT_COLUMNS}
            "#
        ))
        .bind(&id)
        .bind(url)
        .bind(&events_db)
        .bind(secret)
        .bind(created_by_id)
        .bind(owner.project_id())
        .bind(now)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    pub async fn list_webhook_endpoints(&self, owner: &OwnerFilter) -> Result<Vec<WebhookEndpointListRow>> {
        let (project_id, merchant_only) = owner.binds();
        let rows = sqlx::query_as::<_, WebhookEndpointListRow>(&format!(
            r#"
            SELECT e.id, e.url, e."isActive", e.events,
                   (e.secret IS NOT NULL AND e.secret <> '') AS "hasSecret",
                   e."createdById",
                   s."firstName" || ' ' || s."lastName" AS "createdByName",
                   e."projectId",
                   e."createdAt" AT TIME ZONE 'UTC' AS "createdAt",
                   e."updatedAt" AT TIME ZONE 'UTC' AS "updatedAt"
            FROM webhook_endpoints e
            LEFT JOIN staff_users s ON s.id = e."createdById"
            WHERE {}
            ORDER BY e."createdAt" ASC
            "#,
            owner_condition(r#"e."projectId""#, "$1", "$2")
        ))
        .bind(project_id)
        .bind(merchant_only)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    pub async fn find_webhook_endpoint(&self, id: &str) -> Result<Option<WebhookEndpointRow>> {
        let row = sqlx::query_as::<_, WebhookEndpointRow>(&format!(
            "SELECT {WEBHOOK_ENDPOINT_COLUMNS} FROM webhook_endpoints WHERE id = $1"
        ))
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    /// The owner's endpoint at `url`, if any (URLs are unique per owner).
    pub async fn find_webhook_endpoint_by_url(
        &self,
        owner: &Owner,
        url: &str,
    ) -> Result<Option<WebhookEndpointRow>> {
        let row = sqlx::query_as::<_, WebhookEndpointRow>(&format!(
            r#"SELECT {WEBHOOK_ENDPOINT_COLUMNS} FROM webhook_endpoints
               WHERE url = $1 AND "projectId" IS NOT DISTINCT FROM $2"#
        ))
        .bind(url)
        .bind(owner.project_id())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    pub async fn active_webhook_endpoints(&self) -> Result<Vec<WebhookEndpointRow>> {
        let rows = sqlx::query_as::<_, WebhookEndpointRow>(&format!(
            r#"SELECT {WEBHOOK_ENDPOINT_COLUMNS} FROM webhook_endpoints WHERE "isActive" = true ORDER BY "createdAt" ASC"#
        ))
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// Active endpoints subscribed to `event` that belong to `owner` — the
    /// fan-out set when that event fires (spec §10.1). `None` = every owner
    /// (only before the first project exists).
    pub async fn active_webhook_endpoints_for_event(
        &self,
        event: WebhookEventType,
        owner: Option<&Owner>,
    ) -> Result<Vec<WebhookEndpointRow>> {
        let rows = sqlx::query_as::<_, WebhookEndpointRow>(&format!(
            r#"
            SELECT {WEBHOOK_ENDPOINT_COLUMNS} FROM webhook_endpoints e
            WHERE {}
            ORDER BY "createdAt" ASC
            "#,
            subscribed_endpoints("$1", "$2", "$3")
        ))
        .bind(event.as_db())
        .bind(owner.is_some())
        .bind(owner.and_then(|o| o.project_id()))
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    pub async fn set_webhook_endpoint_active(
        &self,
        id: &str,
        is_active: bool,
    ) -> Result<Option<WebhookEndpointRow>> {
        let row = sqlx::query_as::<_, WebhookEndpointRow>(&format!(
            r#"
            UPDATE webhook_endpoints
            SET "isActive" = $2, "updatedAt" = NOW()
            WHERE id = $1
            RETURNING {WEBHOOK_ENDPOINT_COLUMNS}
            "#
        ))
        .bind(id)
        .bind(is_active)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    pub async fn set_webhook_endpoint_events(
        &self,
        id: &str,
        events: &[WebhookEventType],
    ) -> Result<Option<WebhookEndpointRow>> {
        let events_db: Vec<String> = events.iter().map(|e| e.as_db().to_string()).collect();
        let row = sqlx::query_as::<_, WebhookEndpointRow>(&format!(
            r#"
            UPDATE webhook_endpoints
            SET events = $2, "updatedAt" = NOW()
            WHERE id = $1
            RETURNING {WEBHOOK_ENDPOINT_COLUMNS}
            "#
        ))
        .bind(id)
        .bind(&events_db)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    /// Set (rotate) or clear the signing secret. `None` disables signing.
    pub async fn set_webhook_endpoint_secret(
        &self,
        id: &str,
        secret: Option<&str>,
    ) -> Result<Option<WebhookEndpointRow>> {
        let row = sqlx::query_as::<_, WebhookEndpointRow>(&format!(
            r#"
            UPDATE webhook_endpoints
            SET secret = $2, "updatedAt" = NOW()
            WHERE id = $1
            RETURNING {WEBHOOK_ENDPOINT_COLUMNS}
            "#
        ))
        .bind(id)
        .bind(secret)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    /// Hard delete, allowed only when no delivery references the endpoint.
    /// Returns Ok(false) when deliveries exist (caller maps to 409).
    pub async fn delete_webhook_endpoint_if_unused(&self, id: &str) -> Result<Option<bool>> {
        if self.find_webhook_endpoint(id).await?.is_none() {
            return Ok(None);
        }
        let (used,): (i64,) = sqlx::query_as(
            r#"SELECT COUNT(*) FROM webhook_deliveries WHERE "webhookEndpointId" = $1"#,
        )
        .bind(id)
        .fetch_one(&self.pool)
        .await?;
        if used > 0 {
            return Ok(Some(false));
        }
        sqlx::query("DELETE FROM webhook_endpoints WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(Some(true))
    }
}
