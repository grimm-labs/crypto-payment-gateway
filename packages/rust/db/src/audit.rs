use anyhow::Result;
use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::FromRow;
use uuid::Uuid;

use crate::Db;

/// Who performed an audited action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuditActor {
    Staff(String),
    ApiKey(String),
    System,
}

impl AuditActor {
    fn parts(&self) -> (&'static str, Option<&str>) {
        match self {
            Self::Staff(id) => ("STAFF", Some(id)),
            Self::ApiKey(id) => ("API_KEY", Some(id)),
            Self::System => ("SYSTEM", None),
        }
    }
}

/// One entry of the append-only audit trail. `details` must never carry a
/// secret (passwords, keys, TOTP codes or secrets, webhook secrets).
#[derive(Debug, Clone)]
pub struct NewAuditEntry<'a> {
    pub actor: AuditActor,
    pub action: &'a str,
    pub target_type: Option<&'a str>,
    pub target_id: Option<&'a str>,
    pub details: Value,
    pub ip: Option<&'a str>,
}

#[derive(Debug, Clone, FromRow)]
pub struct AuditLogRow {
    pub id: String,
    /// DB value: `STAFF` / `API_KEY` / `SYSTEM`.
    #[sqlx(rename = "actorType")]
    pub actor_type: String,
    #[sqlx(rename = "actorId")]
    pub actor_id: Option<String>,
    /// Staff full name or API key name, when the actor still exists.
    #[sqlx(rename = "actorName")]
    pub actor_name: Option<String>,
    pub action: String,
    #[sqlx(rename = "targetType")]
    pub target_type: Option<String>,
    #[sqlx(rename = "targetId")]
    pub target_id: Option<String>,
    pub details: Value,
    pub ip: Option<String>,
    #[sqlx(rename = "createdAt")]
    pub created_at: DateTime<Utc>,
}

impl Db {
    pub async fn insert_audit(&self, entry: &NewAuditEntry<'_>) -> Result<()> {
        let (actor_type, actor_id) = entry.actor.parts();
        sqlx::query(
            r#"
            INSERT INTO audit_log
              (id, "actorType", "actorId", action, "targetType", "targetId", details, ip, "createdAt")
            VALUES
              ($1, $2::"AuditActorType", $3, $4, $5, $6, $7, $8, $9)
            "#,
        )
        .bind(Uuid::new_v4().to_string())
        .bind(actor_type)
        .bind(actor_id)
        .bind(entry.action)
        .bind(entry.target_type)
        .bind(entry.target_id)
        .bind(&entry.details)
        .bind(entry.ip)
        .bind(Utc::now())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Newest first; `action` filters on a prefix (`payout.` matches every
    /// payout action).
    pub async fn list_audit_log(
        &self,
        action: Option<&str>,
        limit: i64,
        offset: i64,
    ) -> Result<(Vec<AuditLogRow>, i64)> {
        let pattern = action.map(|a| format!("{}%", a.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")));
        let rows = sqlx::query_as::<_, AuditLogRow>(
            r#"
            SELECT l.id, l."actorType"::text AS "actorType", l."actorId",
                   CASE l."actorType"
                     WHEN 'STAFF' THEN s."firstName" || ' ' || s."lastName"
                     WHEN 'API_KEY' THEN k.name
                   END AS "actorName",
                   l.action, l."targetType", l."targetId", l.details, l.ip,
                   l."createdAt" AT TIME ZONE 'UTC' AS "createdAt"
            FROM audit_log l
            LEFT JOIN staff_users s ON l."actorType" = 'STAFF' AND s.id = l."actorId"
            LEFT JOIN api_keys k ON l."actorType" = 'API_KEY' AND k.id = l."actorId"
            WHERE ($1::text IS NULL OR l.action LIKE $1)
            ORDER BY l."createdAt" DESC, l.id DESC
            LIMIT $2 OFFSET $3
            "#,
        )
        .bind(pattern.as_deref())
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.pool)
        .await?;

        let (total,): (i64,) = sqlx::query_as(
            r#"SELECT COUNT(*) FROM audit_log WHERE ($1::text IS NULL OR action LIKE $1)"#,
        )
        .bind(pattern.as_deref())
        .fetch_one(&self.pool)
        .await?;
        Ok((rows, total))
    }
}
