//! `projects`: the merchant's applications (docs/projects-spec.md §6.1,
//! §12, §13). CRUD, the per-project counts of the Projects page, and the
//! first-project transaction that attaches (or releases) the rows created
//! before projects existed.

use anyhow::Result;
use chrono::{DateTime, Utc};
use domain::{split_legacy_endpoint, LegacyEndpointSplit, OwnerKind, ProjectColor, WebhookEventType};
use sqlx::{FromRow, PgConnection};
use uuid::Uuid;

use crate::Db;

const PROJECT_COLUMNS: &str = r#"id, name, color, "keyTag", "createdById", "createdAt" AT TIME ZONE 'UTC' AS "createdAt", "updatedAt" AT TIME ZONE 'UTC' AS "updatedAt""#;

/// Serializes the creation of the first project with the attach step
/// (`pg_advisory_xact_lock(hashtext('projects-first'))`).
const FIRST_PROJECT_LOCK: &str = "projects-first";

#[derive(Debug, Clone, FromRow)]
pub struct ProjectRow {
    pub id: String,
    pub name: String,
    /// Palette value (`domain::ProjectColor`).
    pub color: String,
    /// Tag of the keys generated for the project; `None` = the global tag.
    #[sqlx(rename = "keyTag")]
    pub key_tag: Option<String>,
    #[sqlx(rename = "createdById")]
    pub created_by_id: String,
    #[sqlx(rename = "createdAt")]
    pub created_at: DateTime<Utc>,
    #[sqlx(rename = "updatedAt")]
    pub updated_at: DateTime<Utc>,
}

/// How many owned objects reference a project.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, FromRow)]
pub struct ProjectCounts {
    pub api_keys: i64,
    pub deposit_addresses: i64,
    pub withdrawals: i64,
    pub webhook_endpoints: i64,
    pub address_book_entries: i64,
}

/// A project with its counts (the Projects page).
#[derive(Debug, Clone, FromRow)]
pub struct ProjectListRow {
    #[sqlx(flatten)]
    pub project: ProjectRow,
    #[sqlx(flatten)]
    pub counts: ProjectCounts,
}

/// Rows from before the first project (`"projectId" IS NULL`), as shown by
/// the "attach existing data" checkbox: usable keys, payments, key
/// withdrawals, endpoints and live address-book entries.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub struct UnassignedCounts {
    pub api_keys: i64,
    pub deposit_addresses: i64,
    pub withdrawals: i64,
    pub webhook_endpoints: i64,
    pub address_book_entries: i64,
}

impl UnassignedCounts {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

#[derive(Debug, Clone)]
pub struct NewProject<'a> {
    pub name: &'a str,
    pub color: ProjectColor,
    pub key_tag: Option<&'a str>,
    pub created_by_id: &'a str,
}

/// What to do with the legacy rows when the first project is created.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LegacyData {
    /// Keys, payments, key withdrawals and address-book entries become the
    /// project's; endpoints are split by their subscriptions.
    Attach,
    /// Keys are revoked, endpoints keep their merchant events only, the
    /// rest stays the merchant's.
    Release,
}

/// What the first-project step did with the legacy rows (audit details).
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct LegacyOutcome {
    pub attach: bool,
    pub api_keys: u64,
    pub deposit_addresses: u64,
    pub withdrawals: u64,
    pub address_book_entries: u64,
    /// Endpoints attached to the project with every one of their events.
    pub endpoints_attached: u64,
    /// Endpoints attached to the project, with a new merchant endpoint
    /// carrying their merchant events.
    pub endpoints_split: u64,
    /// Endpoints left to the merchant.
    pub endpoints_merchant: u64,
    /// `Release` only.
    pub api_keys_revoked: Vec<String>,
    pub endpoints_deactivated: u64,
}

/// A created project, with what the first-project step did with the
/// legacy rows (`None` when it was not the first project).
#[derive(Debug)]
pub struct CreatedProject {
    pub project: ProjectRow,
    pub legacy: Option<LegacyOutcome>,
}

#[derive(Debug)]
pub enum ProjectCreate {
    Created(Box<CreatedProject>),
    NameTaken,
    KeyTagTaken,
    /// A legacy-data choice was given but a project already exists.
    NotFirst,
}

#[derive(Debug)]
pub enum ProjectUpdate {
    Updated(ProjectRow),
    NotFound,
    NameTaken,
    KeyTagTaken,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectDelete {
    Deleted,
    NotFound,
    /// A key, payment, withdrawal, transfer, endpoint or address-book entry
    /// (deleted ones included) references it.
    InUse,
    LastProject,
}

/// The unique index a `23505` names, if any.
fn violated_index(err: &sqlx::Error) -> Option<String> {
    match err {
        sqlx::Error::Database(db) if db.code().as_deref() == Some("23505") => {
            db.constraint().map(str::to_string)
        }
        _ => None,
    }
}

impl Db {
    pub async fn count_projects(&self) -> Result<i64> {
        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM projects")
            .fetch_one(&self.pool)
            .await?;
        Ok(count)
    }

    pub async fn find_project(&self, id: &str) -> Result<Option<ProjectRow>> {
        let row = sqlx::query_as::<_, ProjectRow>(&format!(
            "SELECT {PROJECT_COLUMNS} FROM projects WHERE id = $1"
        ))
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    /// Every project, oldest first, with its counts.
    pub async fn list_projects(&self) -> Result<Vec<ProjectListRow>> {
        let rows = sqlx::query_as::<_, ProjectListRow>(&format!(
            r#"
            SELECT {PROJECT_COLUMNS},
                   (SELECT COUNT(*) FROM api_keys k WHERE k."projectId" = p.id) AS api_keys,
                   (SELECT COUNT(*) FROM deposit_addresses a WHERE a."projectId" = p.id) AS deposit_addresses,
                   (SELECT COUNT(*) FROM withdrawals w WHERE w."projectId" = p.id) AS withdrawals,
                   (SELECT COUNT(*) FROM webhook_endpoints e WHERE e."projectId" = p.id) AS webhook_endpoints,
                   (SELECT COUNT(*) FROM address_book_entries b WHERE b."projectId" = p.id AND b."deletedAt" IS NULL) AS address_book_entries
            FROM projects p
            ORDER BY p."createdAt" ASC, p.id ASC
            "#
        ))
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// Colours in use (palette auto-assignment).
    pub async fn project_colors(&self) -> Result<Vec<ProjectColor>> {
        let rows: Vec<(String,)> = sqlx::query_as("SELECT color FROM projects")
            .fetch_all(&self.pool)
            .await?;
        Ok(rows
            .into_iter()
            .filter_map(|(color,)| ProjectColor::parse(&color).ok())
            .collect())
    }

    pub async fn unassigned_counts(&self) -> Result<UnassignedCounts> {
        let (api_keys, deposit_addresses, withdrawals, webhook_endpoints, address_book_entries): (i64, i64, i64, i64, i64) =
            sqlx::query_as(
                r#"
                SELECT
                    (SELECT COUNT(*) FROM api_keys WHERE "projectId" IS NULL AND status <> 'REVOKED'::"ApiKeyStatus"),
                    (SELECT COUNT(*) FROM deposit_addresses WHERE "projectId" IS NULL),
                    (SELECT COUNT(*) FROM withdrawals WHERE "projectId" IS NULL AND "apiKeyId" IS NOT NULL),
                    (SELECT COUNT(*) FROM webhook_endpoints WHERE "projectId" IS NULL),
                    (SELECT COUNT(*) FROM address_book_entries WHERE "projectId" IS NULL AND "deletedAt" IS NULL)
                "#,
            )
            .fetch_one(&self.pool)
            .await?;
        Ok(UnassignedCounts {
            api_keys,
            deposit_addresses,
            withdrawals,
            webhook_endpoints,
            address_book_entries,
        })
    }

    /// Create a project. The first one also settles the legacy rows in the
    /// same transaction, under an advisory lock that re-checks it is the
    /// first (`legacy` = `None` means the caller said nothing: the first
    /// project attaches, a later one changes nothing).
    pub async fn create_project(&self, new: &NewProject<'_>, legacy: Option<LegacyData>) -> Result<ProjectCreate> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1))")
            .bind(FIRST_PROJECT_LOCK)
            .execute(&mut *tx)
            .await?;
        let (existing,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM projects")
            .fetch_one(&mut *tx)
            .await?;
        let first = existing == 0;
        if !first && legacy.is_some() {
            return Ok(ProjectCreate::NotFirst);
        }
        let inserted = sqlx::query_as::<_, ProjectRow>(&format!(
            r#"
            INSERT INTO projects (id, name, color, "keyTag", "createdById", "createdAt", "updatedAt")
            VALUES ($1, $2, $3, $4, $5, NOW(), NOW())
            RETURNING {PROJECT_COLUMNS}
            "#
        ))
        .bind(Uuid::new_v4().to_string())
        .bind(new.name)
        .bind(new.color.as_str())
        .bind(new.key_tag)
        .bind(new.created_by_id)
        .fetch_one(&mut *tx)
        .await;
        let project = match inserted {
            Ok(project) => project,
            Err(err) => {
                return Ok(match violated_index(&err).as_deref() {
                    Some("projects_name_key") => ProjectCreate::NameTaken,
                    Some("projects_keyTag_key") => ProjectCreate::KeyTagTaken,
                    _ => return Err(err.into()),
                })
            }
        };
        let outcome = if first {
            Some(match legacy.unwrap_or(LegacyData::Attach) {
                LegacyData::Attach => attach_legacy(&mut tx, &project.id).await?,
                LegacyData::Release => release_legacy(&mut tx).await?,
            })
        } else {
            None
        };
        tx.commit().await?;
        Ok(ProjectCreate::Created(Box::new(CreatedProject {
            project,
            legacy: outcome,
        })))
    }

    /// Rename / recolour / retag. `key_tag`: absent = untouched, `Some(None)`
    /// clears it.
    pub async fn update_project(
        &self,
        id: &str,
        name: Option<&str>,
        color: Option<ProjectColor>,
        key_tag: Option<Option<&str>>,
    ) -> Result<ProjectUpdate> {
        let (clear_tag, tag) = match key_tag {
            None => (false, None),
            Some(None) => (true, None),
            Some(Some(tag)) => (false, Some(tag)),
        };
        let updated = sqlx::query_as::<_, ProjectRow>(&format!(
            r#"
            UPDATE projects SET
                name = COALESCE($2, name),
                color = COALESCE($3, color),
                "keyTag" = CASE WHEN $4 THEN NULL ELSE COALESCE($5, "keyTag") END,
                "updatedAt" = NOW()
            WHERE id = $1
            RETURNING {PROJECT_COLUMNS}
            "#
        ))
        .bind(id)
        .bind(name)
        .bind(color.map(|c| c.as_str()))
        .bind(clear_tag)
        .bind(tag)
        .fetch_optional(&self.pool)
        .await;
        Ok(match updated {
            Ok(Some(row)) => ProjectUpdate::Updated(row),
            Ok(None) => ProjectUpdate::NotFound,
            Err(err) => match violated_index(&err).as_deref() {
                Some("projects_name_key") => ProjectUpdate::NameTaken,
                Some("projects_keyTag_key") => ProjectUpdate::KeyTagTaken,
                _ => return Err(err.into()),
            },
        })
    }

    /// Delete an empty project — never the last one. Checked and deleted
    /// under the row lock, so a key created meanwhile is seen.
    pub async fn delete_project(&self, id: &str) -> Result<ProjectDelete> {
        let mut tx = self.pool.begin().await?;
        let locked: Option<(String,)> = sqlx::query_as("SELECT id FROM projects WHERE id = $1 FOR UPDATE")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
        if locked.is_none() {
            return Ok(ProjectDelete::NotFound);
        }
        let (total,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM projects")
            .fetch_one(&mut *tx)
            .await?;
        if total <= 1 {
            return Ok(ProjectDelete::LastProject);
        }
        let (references,): (i64,) = sqlx::query_as(
            r#"
            SELECT
                (SELECT COUNT(*) FROM api_keys WHERE "projectId" = $1)
              + (SELECT COUNT(*) FROM deposit_addresses WHERE "projectId" = $1)
              + (SELECT COUNT(*) FROM withdrawals WHERE "projectId" = $1)
              + (SELECT COUNT(*) FROM webhook_endpoints WHERE "projectId" = $1)
              + (SELECT COUNT(*) FROM address_book_entries WHERE "projectId" = $1)
              + (SELECT COUNT(*) FROM internal_transfers WHERE "fromProjectId" = $1 OR "toProjectId" = $1)
            "#,
        )
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
        if references > 0 {
            return Ok(ProjectDelete::InUse);
        }
        sqlx::query("DELETE FROM projects WHERE id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(ProjectDelete::Deleted)
    }
}

/// Legacy endpoints (no owner yet) as the attach step reads them.
#[derive(Debug, Clone, FromRow)]
struct LegacyEndpoint {
    id: String,
    url: String,
    #[sqlx(rename = "isActive")]
    is_active: bool,
    events: Vec<String>,
    secret: Option<String>,
    #[sqlx(rename = "createdById")]
    created_by_id: String,
}

fn parse_events(events: &[String]) -> Vec<WebhookEventType> {
    events.iter().filter_map(|e| WebhookEventType::from_db(e).ok()).collect()
}

fn db_events(events: &[WebhookEventType]) -> Vec<String> {
    events.iter().map(|e| e.as_db().to_string()).collect()
}

async fn legacy_endpoints(conn: &mut PgConnection) -> Result<Vec<LegacyEndpoint>> {
    Ok(sqlx::query_as::<_, LegacyEndpoint>(
        r#"SELECT id, url, "isActive", events, secret, "createdById"
           FROM webhook_endpoints WHERE "projectId" IS NULL ORDER BY "createdAt" ASC"#,
    )
    .fetch_all(&mut *conn)
    .await?)
}

async fn set_endpoint(conn: &mut PgConnection, id: &str, project_id: Option<&str>, events: &[WebhookEventType], is_active: Option<bool>) -> Result<()> {
    sqlx::query(
        r#"UPDATE webhook_endpoints
           SET "projectId" = $2, events = $3, "isActive" = COALESCE($4, "isActive"), "updatedAt" = NOW()
           WHERE id = $1"#,
    )
    .bind(id)
    .bind(project_id)
    .bind(db_events(events))
    .bind(is_active)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Spec §13, `attach_existing = true`.
async fn attach_legacy(conn: &mut PgConnection, project_id: &str) -> Result<LegacyOutcome> {
    let mut outcome = LegacyOutcome {
        attach: true,
        ..Default::default()
    };
    outcome.api_keys = sqlx::query(r#"UPDATE api_keys SET "projectId" = $1 WHERE "projectId" IS NULL"#)
        .bind(project_id)
        .execute(&mut *conn)
        .await?
        .rows_affected();
    outcome.deposit_addresses =
        sqlx::query(r#"UPDATE deposit_addresses SET "projectId" = $1, "updatedAt" = NOW() WHERE "projectId" IS NULL"#)
            .bind(project_id)
            .execute(&mut *conn)
            .await?
            .rows_affected();
    // Staff withdrawals stay NULL: merchant funds.
    outcome.withdrawals = sqlx::query(
        r#"UPDATE withdrawals SET "projectId" = $1, "updatedAt" = NOW() WHERE "projectId" IS NULL AND "apiKeyId" IS NOT NULL"#,
    )
    .bind(project_id)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    outcome.address_book_entries =
        sqlx::query(r#"UPDATE address_book_entries SET "projectId" = $1 WHERE "projectId" IS NULL"#)
            .bind(project_id)
            .execute(&mut *conn)
            .await?
            .rows_affected();
    for endpoint in legacy_endpoints(conn).await? {
        match split_legacy_endpoint(&parse_events(&endpoint.events)) {
            LegacyEndpointSplit::Project(events) => {
                set_endpoint(conn, &endpoint.id, Some(project_id), &events, None).await?;
                outcome.endpoints_attached += 1;
            }
            LegacyEndpointSplit::Merchant(_) => outcome.endpoints_merchant += 1,
            LegacyEndpointSplit::Both { project, merchant } => {
                set_endpoint(conn, &endpoint.id, Some(project_id), &project, None).await?;
                sqlx::query(
                    r#"INSERT INTO webhook_endpoints
                         (id, url, "isActive", events, secret, "createdById", "projectId", "createdAt", "updatedAt")
                       VALUES ($1, $2, $3, $4, $5, $6, NULL, NOW(), NOW())"#,
                )
                .bind(Uuid::new_v4().to_string())
                .bind(&endpoint.url)
                .bind(endpoint.is_active)
                .bind(db_events(&merchant))
                .bind(&endpoint.secret)
                .bind(&endpoint.created_by_id)
                .execute(&mut *conn)
                .await?;
                outcome.endpoints_split += 1;
            }
        }
    }
    Ok(outcome)
}

/// Spec §13, `attach_existing = false`: the legacy rows become the
/// merchant's. Active keys are revoked (invariant 1: once a project exists,
/// no active key lacks one).
async fn release_legacy(conn: &mut PgConnection) -> Result<LegacyOutcome> {
    let mut outcome = LegacyOutcome {
        attach: false,
        ..Default::default()
    };
    let revoked: Vec<(String,)> = sqlx::query_as(
        r#"UPDATE api_keys SET status = 'REVOKED'::"ApiKeyStatus", "revokedAt" = NOW()
           WHERE "projectId" IS NULL AND "revokedAt" IS NULL RETURNING id"#,
    )
    .fetch_all(&mut *conn)
    .await?;
    outcome.api_keys_revoked = revoked.into_iter().map(|(id,)| id).collect();
    for endpoint in legacy_endpoints(conn).await? {
        let events: Vec<WebhookEventType> = parse_events(&endpoint.events)
            .into_iter()
            .filter(|e| domain::is_event_allowed(OwnerKind::Merchant, *e))
            .collect();
        let deactivate = events.is_empty() && endpoint.is_active;
        set_endpoint(conn, &endpoint.id, None, &events, deactivate.then_some(false)).await?;
        outcome.endpoints_merchant += 1;
        if deactivate {
            outcome.endpoints_deactivated += 1;
        }
    }
    Ok(outcome)
}
