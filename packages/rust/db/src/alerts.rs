//! `alerts`: operational conditions and events, deduplicated on a key for
//! a window (one per day while a condition holds).

use anyhow::Result;
use chrono::{DateTime, Utc};
use domain::{AlertType, Network, Token};
use serde_json::Value;
use sqlx::FromRow;
use uuid::Uuid;

use crate::Db;

const ALERT_COLUMNS: &str = r#"id, type::text AS type, network::text AS network, token::text AS token, "dedupKey", details, "createdAt" AT TIME ZONE 'UTC' AS "createdAt""#;

#[derive(Debug, Clone, FromRow)]
pub struct AlertRow {
    pub id: String,
    #[sqlx(rename = "type")]
    pub alert_type: String,
    pub network: Option<String>,
    pub token: Option<String>,
    #[sqlx(rename = "dedupKey")]
    pub dedup_key: String,
    pub details: Value,
    #[sqlx(rename = "createdAt")]
    pub created_at: DateTime<Utc>,
}

impl AlertRow {
    pub fn alert_type(&self) -> Result<AlertType> {
        Ok(AlertType::from_db(&self.alert_type)?)
    }
}

#[derive(Debug, Clone)]
pub struct NewAlert<'a> {
    pub alert_type: AlertType,
    pub network: Option<Network>,
    pub token: Option<Token>,
    pub dedup_key: &'a str,
    pub details: Value,
}

impl Db {
    /// Record an alert unless one with the same key was recorded within
    /// `dedup_secs` (`None` then). Serialized per key, so two workers never
    /// both record it.
    pub async fn record_alert(&self, new: &NewAlert<'_>, dedup_secs: i64) -> Result<Option<AlertRow>> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1))")
            .bind(format!("alert:{}", new.dedup_key))
            .execute(&mut *tx)
            .await?;
        let (recent,): (bool,) = sqlx::query_as(
            r#"SELECT EXISTS (SELECT 1 FROM alerts WHERE "dedupKey" = $1
                 AND "createdAt" > (NOW() AT TIME ZONE 'UTC') - make_interval(secs => $2))"#,
        )
        .bind(new.dedup_key)
        .bind(dedup_secs as f64)
        .fetch_one(&mut *tx)
        .await?;
        if recent {
            return Ok(None);
        }
        let row = sqlx::query_as::<_, AlertRow>(&format!(
            r#"
            INSERT INTO alerts (id, type, network, token, "dedupKey", details, "createdAt")
            VALUES ($1, $2::"AlertType", $3::"Network", $4::"Token", $5, $6, NOW())
            RETURNING {ALERT_COLUMNS}
            "#
        ))
        .bind(Uuid::new_v4().to_string())
        .bind(new.alert_type.as_db())
        .bind(new.network.map(|n| n.as_db()))
        .bind(new.token.map(|t| t.as_db()))
        .bind(new.dedup_key)
        .bind(&new.details)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(Some(row))
    }

    pub async fn find_alert(&self, id: &str) -> Result<Option<AlertRow>> {
        let row = sqlx::query_as::<_, AlertRow>(&format!("SELECT {ALERT_COLUMNS} FROM alerts WHERE id = $1"))
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row)
    }

    pub async fn list_alerts(&self, limit: i64, offset: i64) -> Result<(Vec<AlertRow>, i64)> {
        let rows = sqlx::query_as::<_, AlertRow>(&format!(
            r#"SELECT {ALERT_COLUMNS} FROM alerts ORDER BY "createdAt" DESC, id DESC LIMIT $1 OFFSET $2"#
        ))
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.pool)
        .await?;
        let (total,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM alerts")
            .fetch_one(&self.pool)
            .await?;
        Ok((rows, total))
    }
}
