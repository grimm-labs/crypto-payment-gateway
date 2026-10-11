use anyhow::Result;
use chrono::{DateTime, Utc};
use domain::StaffRole;
use sqlx::FromRow;
use uuid::Uuid;

use crate::Db;

/// Advisory lock serializing the "first admin" insert and every change that
/// can remove an active admin (setup race, last-admin TOCTOU).
const STAFF_ADMIN_LOCK: i64 = 0x5354_4146_4641_444d; // "STAFFADM"

const STAFF_USER_COLUMNS: &str = r#"id, email, "firstName", "lastName", "passwordHash", role::text AS role, "isActive", "totpSecret", "totpEnabledAt" AT TIME ZONE 'UTC' AS "totpEnabledAt", "createdAt" AT TIME ZONE 'UTC' AS "createdAt", "updatedAt" AT TIME ZONE 'UTC' AS "updatedAt""#;

#[derive(Clone, FromRow)]
pub struct StaffUserRow {
    pub id: String,
    pub email: String,
    #[sqlx(rename = "firstName")]
    pub first_name: String,
    #[sqlx(rename = "lastName")]
    pub last_name: String,
    #[sqlx(rename = "passwordHash")]
    pub password_hash: String,
    pub role: String,
    #[sqlx(rename = "isActive")]
    pub is_active: bool,
    #[sqlx(rename = "totpSecret")]
    pub totp_secret: Option<String>,
    #[sqlx(rename = "totpEnabledAt")]
    pub totp_enabled_at: Option<DateTime<Utc>>,
    #[sqlx(rename = "createdAt")]
    pub created_at: DateTime<Utc>,
    #[sqlx(rename = "updatedAt")]
    pub updated_at: DateTime<Utc>,
}

/// Manual: the password hash and the TOTP secret never reach a log line.
impl std::fmt::Debug for StaffUserRow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StaffUserRow")
            .field("id", &self.id)
            .field("email", &self.email)
            .field("role", &self.role)
            .field("is_active", &self.is_active)
            .field("totp_enabled", &self.totp_enabled())
            .finish_non_exhaustive()
    }
}

/// Outcome of [`Db::update_staff`].
#[derive(Debug, Clone)]
pub enum StaffUpdate {
    Updated(Box<StaffUserRow>),
    NotFound,
    /// The change would leave no active admin.
    LastActiveAdmin,
}

impl StaffUserRow {
    pub fn totp_enabled(&self) -> bool {
        self.totp_secret.is_some() && self.totp_enabled_at.is_some()
    }
}

impl Db {
    pub async fn count_staff(&self) -> Result<i64> {
        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM staff_users")
            .fetch_one(&self.pool)
            .await?;
        Ok(count)
    }

    pub async fn count_active_admins(&self) -> Result<i64> {
        let (count,): (i64,) = sqlx::query_as(
            r#"SELECT COUNT(*) FROM staff_users WHERE role = 'ADMIN'::"StaffRole" AND "isActive" = true"#,
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(count)
    }

    pub async fn create_staff_user(
        &self,
        email: &str,
        first_name: &str,
        last_name: &str,
        password_hash: &str,
        role: StaffRole,
    ) -> Result<StaffUserRow> {
        let id = Uuid::new_v4().to_string();
        let now = Utc::now();
        let row = sqlx::query_as::<_, StaffUserRow>(&format!(
            r#"
            INSERT INTO staff_users
              (id, email, "firstName", "lastName", "passwordHash", role, "isActive", "createdAt", "updatedAt")
            VALUES
              ($1, $2, $3, $4, $5, $6::"StaffRole", true, $7, $7)
            RETURNING {STAFF_USER_COLUMNS}
            "#
        ))
        .bind(&id)
        .bind(email)
        .bind(first_name)
        .bind(last_name)
        .bind(password_hash)
        .bind(role.as_db())
        .bind(now)
        .fetch_one(&self.pool)
        .await?;
        Ok(row)
    }

    /// Insert the first admin only while the table is still empty. The
    /// guarded INSERT alone takes no lock (READ COMMITTED), so an advisory
    /// lock serializes concurrent setup calls: exactly one can succeed.
    /// Returns `None` when a staff user already exists.
    pub async fn create_first_admin(
        &self,
        email: &str,
        first_name: &str,
        last_name: &str,
        password_hash: &str,
    ) -> Result<Option<StaffUserRow>> {
        let id = Uuid::new_v4().to_string();
        let now = Utc::now();
        let mut tx = self.pool.begin().await?;
        sqlx::query("SELECT pg_advisory_xact_lock($1)")
            .bind(STAFF_ADMIN_LOCK)
            .execute(&mut *tx)
            .await?;
        let row = sqlx::query_as::<_, StaffUserRow>(&format!(
            r#"
            INSERT INTO staff_users
              (id, email, "firstName", "lastName", "passwordHash", role, "isActive", "createdAt", "updatedAt")
            SELECT $1, $2, $3, $4, $5, 'ADMIN'::"StaffRole", true, $6, $6
            WHERE NOT EXISTS (SELECT 1 FROM staff_users)
            RETURNING {STAFF_USER_COLUMNS}
            "#
        ))
        .bind(&id)
        .bind(email)
        .bind(first_name)
        .bind(last_name)
        .bind(password_hash)
        .bind(now)
        .fetch_optional(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(row)
    }

    pub async fn find_staff_by_email(&self, email: &str) -> Result<Option<StaffUserRow>> {
        let row = sqlx::query_as::<_, StaffUserRow>(&format!(
            "SELECT {STAFF_USER_COLUMNS} FROM staff_users WHERE lower(email) = lower($1)"
        ))
        .bind(email)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    pub async fn find_staff_by_id(&self, id: &str) -> Result<Option<StaffUserRow>> {
        let row = sqlx::query_as::<_, StaffUserRow>(&format!(
            "SELECT {STAFF_USER_COLUMNS} FROM staff_users WHERE id = $1"
        ))
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    pub async fn list_staff(&self, limit: i64, offset: i64) -> Result<(Vec<StaffUserRow>, i64)> {
        let rows = sqlx::query_as::<_, StaffUserRow>(&format!(
            r#"SELECT {STAFF_USER_COLUMNS} FROM staff_users ORDER BY "createdAt" ASC LIMIT $1 OFFSET $2"#
        ))
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.pool)
        .await?;

        let total = self.count_staff().await?;
        Ok((rows, total))
    }

    /// Partial update. Demoting or deactivating the last active admin is
    /// refused; the check and the write run under one advisory lock, so two
    /// admins demoting each other concurrently cannot leave zero admins.
    pub async fn update_staff(
        &self,
        id: &str,
        first_name: Option<&str>,
        last_name: Option<&str>,
        role: Option<StaffRole>,
        is_active: Option<bool>,
        password_hash: Option<&str>,
    ) -> Result<StaffUpdate> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("SELECT pg_advisory_xact_lock($1)")
            .bind(STAFF_ADMIN_LOCK)
            .execute(&mut *tx)
            .await?;

        let target: Option<(String, bool)> = sqlx::query_as(
            r#"SELECT role::text, "isActive" FROM staff_users WHERE id = $1 FOR UPDATE"#,
        )
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((current_role, current_active)) = target else {
            return Ok(StaffUpdate::NotFound);
        };
        let is_active_admin = current_role == StaffRole::Admin.as_db() && current_active;
        let loses_admin = role == Some(StaffRole::Operator) || is_active == Some(false);
        if is_active_admin && loses_admin {
            let (others,): (i64,) = sqlx::query_as(
                r#"SELECT COUNT(*) FROM staff_users WHERE role = 'ADMIN'::"StaffRole" AND "isActive" = true AND id <> $1"#,
            )
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
            if others == 0 {
                return Ok(StaffUpdate::LastActiveAdmin);
            }
        }

        let row = sqlx::query_as::<_, StaffUserRow>(&format!(
            r#"
            UPDATE staff_users
            SET "firstName" = COALESCE($2, "firstName"),
                "lastName" = COALESCE($3, "lastName"),
                role = COALESCE($4::"StaffRole", role),
                "isActive" = COALESCE($5, "isActive"),
                "passwordHash" = COALESCE($6, "passwordHash"),
                "updatedAt" = NOW()
            WHERE id = $1
            RETURNING {STAFF_USER_COLUMNS}
            "#
        ))
        .bind(id)
        .bind(first_name)
        .bind(last_name)
        .bind(role.map(|r| r.as_db()))
        .bind(is_active)
        .bind(password_hash)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(StaffUpdate::Updated(Box::new(row)))
    }

    /// Self-service password change (the caller already verified the
    /// current password).
    pub async fn set_staff_password(&self, id: &str, password_hash: &str) -> Result<bool> {
        let result = sqlx::query(
            r#"UPDATE staff_users SET "passwordHash" = $2, "updatedAt" = NOW() WHERE id = $1"#,
        )
        .bind(id)
        .bind(password_hash)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    pub async fn enable_staff_totp(&self, id: &str, secret: &str) -> Result<Option<StaffUserRow>> {
        let row = sqlx::query_as::<_, StaffUserRow>(&format!(
            r#"
            UPDATE staff_users
            SET "totpSecret" = $2,
                "totpEnabledAt" = NOW(),
                "updatedAt" = NOW()
            WHERE id = $1
            RETURNING {STAFF_USER_COLUMNS}
            "#
        ))
        .bind(id)
        .bind(secret)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    pub async fn clear_staff_totp(&self, id: &str) -> Result<Option<StaffUserRow>> {
        let row = sqlx::query_as::<_, StaffUserRow>(&format!(
            r#"
            UPDATE staff_users
            SET "totpSecret" = NULL,
                "totpEnabledAt" = NULL,
                "updatedAt" = NOW()
            WHERE id = $1
            RETURNING {STAFF_USER_COLUMNS}
            "#
        ))
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }
}
