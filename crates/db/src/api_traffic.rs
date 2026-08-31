use anyhow::Result;
use chrono::{DateTime, Utc};
use domain::{ApiSurface, StatusClass};
use sqlx::FromRow;

use crate::Db;

/// One recorded HTTP request. Produced by the API middleware, buffered in
/// memory and batch-inserted by the traffic writer worker.
#[derive(Debug, Clone)]
pub struct ApiTrafficEvent {
    pub surface: ApiSurface,
    pub method: String,
    /// Matched route template, never the raw path.
    pub route: String,
    pub status: i16,
    pub duration_ms: f64,
    pub api_key_id: Option<String>,
    pub staff_id: Option<String>,
    pub ip: Option<String>,
    /// Request start.
    pub created_at: DateTime<Utc>,
}

/// Traffic-log row joined with the key / staff display names.
#[derive(Debug, Clone, FromRow)]
pub struct ApiRequestRow {
    pub id: i64,
    pub surface: String,
    pub method: String,
    pub route: String,
    pub status: i16,
    #[sqlx(rename = "durationMs")]
    pub duration_ms: f64,
    #[sqlx(rename = "apiKeyId")]
    pub api_key_id: Option<String>,
    #[sqlx(rename = "apiKeyName")]
    pub api_key_name: Option<String>,
    #[sqlx(rename = "apiKeyPrefix")]
    pub api_key_prefix: Option<String>,
    #[sqlx(rename = "staffId")]
    pub staff_id: Option<String>,
    #[sqlx(rename = "staffName")]
    pub staff_name: Option<String>,
    pub ip: Option<String>,
    #[sqlx(rename = "createdAt")]
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Default)]
pub struct TrafficSummary {
    pub total: i64,
    pub last_minute: i64,
    pub client_errors: i64,
    pub server_errors: i64,
    /// NULL when the window holds no request.
    pub p50_ms: Option<f64>,
    pub p95_ms: Option<f64>,
    /// Distinct authenticated API keys seen in the window.
    pub active_keys: i64,
}

/// One chart bucket; buckets with no traffic are present with zero counts.
#[derive(Debug, Clone)]
pub struct TrafficBucket {
    pub bucket: DateTime<Utc>,
    pub ok: i64,
    pub client_error: i64,
    pub server_error: i64,
    pub p95_ms: Option<f64>,
}

#[derive(Debug, Clone)]
pub struct TrafficRouteStat {
    pub method: String,
    pub route: String,
    pub count: i64,
    pub client_errors: i64,
    pub server_errors: i64,
    pub p95_ms: Option<f64>,
}

/// Per-key merchant traffic. `api_key_id` is `None` for the group of
/// requests that carried no valid key (missing / invalid / expired).
#[derive(Debug, Clone)]
pub struct TrafficApiKeyStat {
    pub api_key_id: Option<String>,
    pub name: Option<String>,
    pub prefix: Option<String>,
    pub status: Option<String>,
    pub count: i64,
    pub client_errors: i64,
    pub server_errors: i64,
    pub p95_ms: Option<f64>,
    pub last_seen_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Default)]
pub struct ApiRequestFilter {
    pub surface: Option<ApiSurface>,
    pub status_class: Option<StatusClass>,
    pub route: Option<String>,
    pub method: Option<String>,
    pub api_key_id: Option<String>,
}

/// (bucket, ok, 4xx, 5xx, p95)
type BucketRow = (DateTime<Utc>, i64, i64, i64, Option<f64>);

/// (apiKeyId, name, prefix, status, count, 4xx, 5xx, p95, lastSeenAt)
type TopApiKeyRow = (
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    i64,
    i64,
    i64,
    Option<f64>,
    DateTime<Utc>,
);

const REQUEST_LIST_COLUMNS: &str = r#"
    r.id, r.surface::text AS surface, r.method, r.route, r.status, r."durationMs",
    r."apiKeyId", k.name AS "apiKeyName", k.prefix AS "apiKeyPrefix",
    r."staffId", (s."firstName" || ' ' || s."lastName") AS "staffName",
    r.ip, r."createdAt" AT TIME ZONE 'UTC' AS "createdAt"
"#;

const REQUEST_LIST_WHERE: &str = r#"
    ($1::text IS NULL OR r.surface = $1::"ApiSurface")
    AND ($2::smallint IS NULL OR r.status BETWEEN $2::smallint AND $3::smallint)
    AND ($4::text IS NULL OR r.route = $4)
    AND ($5::text IS NULL OR r.method = $5)
    AND ($6::text IS NULL OR r."apiKeyId" = $6)
"#;

impl Db {
    /// Columnar multi-row insert: one prepared statement regardless of the
    /// batch size.
    pub async fn insert_api_requests(&self, events: &[ApiTrafficEvent]) -> Result<u64> {
        if events.is_empty() {
            return Ok(0);
        }
        let surfaces: Vec<String> = events.iter().map(|e| e.surface.as_db().to_string()).collect();
        let methods: Vec<String> = events.iter().map(|e| e.method.clone()).collect();
        let routes: Vec<String> = events.iter().map(|e| e.route.clone()).collect();
        let statuses: Vec<i16> = events.iter().map(|e| e.status).collect();
        let durations: Vec<f64> = events.iter().map(|e| e.duration_ms).collect();
        let api_key_ids: Vec<Option<String>> = events.iter().map(|e| e.api_key_id.clone()).collect();
        let staff_ids: Vec<Option<String>> = events.iter().map(|e| e.staff_id.clone()).collect();
        let ips: Vec<Option<String>> = events.iter().map(|e| e.ip.clone()).collect();
        let created: Vec<DateTime<Utc>> = events.iter().map(|e| e.created_at).collect();

        let result = sqlx::query(
            r#"
            INSERT INTO api_requests
                (surface, method, route, status, "durationMs", "apiKeyId", "staffId", ip, "createdAt")
            SELECT t.surface::"ApiSurface", t.method, t.route, t.status, t.duration,
                   t.api_key_id, t.staff_id, t.ip, (t.ts AT TIME ZONE 'UTC')
            FROM UNNEST(
                $1::text[], $2::text[], $3::text[], $4::smallint[], $5::float8[],
                $6::text[], $7::text[], $8::text[], $9::timestamptz[]
            ) AS t(surface, method, route, status, duration, api_key_id, staff_id, ip, ts)
            "#,
        )
        .bind(&surfaces)
        .bind(&methods)
        .bind(&routes)
        .bind(&statuses)
        .bind(&durations)
        .bind(&api_key_ids)
        .bind(&staff_ids)
        .bind(&ips)
        .bind(&created)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    pub async fn traffic_summary(
        &self,
        surface: Option<ApiSurface>,
        window_secs: i64,
    ) -> Result<TrafficSummary> {
        let surface_db = surface.map(|s| s.as_db().to_string());
        let (total, last_minute, client_errors, server_errors, p50_ms, p95_ms, active_keys): (
            i64,
            i64,
            i64,
            i64,
            Option<f64>,
            Option<f64>,
            i64,
        ) = sqlx::query_as(
            r#"
            SELECT COUNT(*),
                   COUNT(*) FILTER (WHERE "createdAt" >= (NOW() AT TIME ZONE 'UTC') - interval '60 seconds'),
                   COUNT(*) FILTER (WHERE status BETWEEN 400 AND 499),
                   COUNT(*) FILTER (WHERE status >= 500),
                   percentile_cont(0.5)  WITHIN GROUP (ORDER BY "durationMs"),
                   percentile_cont(0.95) WITHIN GROUP (ORDER BY "durationMs"),
                   COUNT(DISTINCT "apiKeyId")
            FROM api_requests
            WHERE "createdAt" >= (NOW() AT TIME ZONE 'UTC') - make_interval(secs => $2::double precision)
              AND ($1::text IS NULL OR surface = $1::"ApiSurface")
            "#,
        )
        .bind(surface_db.as_deref())
        .bind(window_secs)
        .fetch_one(&self.pool)
        .await?;
        Ok(TrafficSummary {
            total,
            last_minute,
            client_errors,
            server_errors,
            p50_ms,
            p95_ms,
            active_keys,
        })
    }

    /// Requests per `bucket_secs` bucket over the trailing `window_secs`,
    /// gap-filled in SQL so the chart always has `window / bucket` points
    /// (the last one being the current, partial bucket).
    pub async fn traffic_timeseries(
        &self,
        surface: Option<ApiSurface>,
        window_secs: i64,
        bucket_secs: i64,
    ) -> Result<Vec<TrafficBucket>> {
        let surface_db = surface.map(|s| s.as_db().to_string());
        let rows: Vec<BucketRow> = sqlx::query_as(
            r#"
            WITH bounds AS (
                SELECT to_timestamp(floor(extract(epoch FROM NOW()) / $3) * $3) AS end_bucket
            ), agg AS (
                SELECT to_timestamp(floor(extract(epoch FROM ("createdAt" AT TIME ZONE 'UTC')) / $3) * $3) AS bucket,
                       COUNT(*) FILTER (WHERE status < 400)               AS ok,
                       COUNT(*) FILTER (WHERE status BETWEEN 400 AND 499) AS client_error,
                       COUNT(*) FILTER (WHERE status >= 500)               AS server_error,
                       percentile_cont(0.95) WITHIN GROUP (ORDER BY "durationMs") AS p95
                FROM api_requests
                WHERE "createdAt" >= (NOW() AT TIME ZONE 'UTC') - make_interval(secs => $2::double precision)
                  AND ($1::text IS NULL OR surface = $1::"ApiSurface")
                GROUP BY 1
            ), series AS (
                SELECT generate_series(
                    b.end_bucket - make_interval(secs => $2::double precision) + make_interval(secs => $3::double precision),
                    b.end_bucket,
                    make_interval(secs => $3::double precision)
                ) AS bucket
                FROM bounds b
            )
            SELECT s.bucket,
                   COALESCE(a.ok, 0), COALESCE(a.client_error, 0), COALESCE(a.server_error, 0),
                   a.p95
            FROM series s
            LEFT JOIN agg a ON a.bucket = s.bucket
            ORDER BY s.bucket
            "#,
        )
        .bind(surface_db.as_deref())
        .bind(window_secs)
        .bind(bucket_secs)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(bucket, ok, client_error, server_error, p95_ms)| TrafficBucket {
                bucket,
                ok,
                client_error,
                server_error,
                p95_ms,
            })
            .collect())
    }

    pub async fn traffic_top_routes(
        &self,
        surface: Option<ApiSurface>,
        window_secs: i64,
        limit: i64,
    ) -> Result<Vec<TrafficRouteStat>> {
        let surface_db = surface.map(|s| s.as_db().to_string());
        let rows: Vec<(String, String, i64, i64, i64, Option<f64>)> = sqlx::query_as(
            r#"
            SELECT method, route, COUNT(*),
                   COUNT(*) FILTER (WHERE status BETWEEN 400 AND 499),
                   COUNT(*) FILTER (WHERE status >= 500),
                   percentile_cont(0.95) WITHIN GROUP (ORDER BY "durationMs")
            FROM api_requests
            WHERE "createdAt" >= (NOW() AT TIME ZONE 'UTC') - make_interval(secs => $2::double precision)
              AND ($1::text IS NULL OR surface = $1::"ApiSurface")
            GROUP BY method, route
            ORDER BY 3 DESC, route ASC
            LIMIT $3
            "#,
        )
        .bind(surface_db.as_deref())
        .bind(window_secs)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(
                |(method, route, count, client_errors, server_errors, p95_ms)| TrafficRouteStat {
                    method,
                    route,
                    count,
                    client_errors,
                    server_errors,
                    p95_ms,
                },
            )
            .collect())
    }

    /// Merchant-surface traffic grouped by API key. Rows with no key are
    /// kept as a single `api_key_id = NULL` group (rejected attempts).
    pub async fn traffic_top_api_keys(
        &self,
        window_secs: i64,
        limit: i64,
    ) -> Result<Vec<TrafficApiKeyStat>> {
        let rows: Vec<TopApiKeyRow> = sqlx::query_as(
            r#"
            SELECT r."apiKeyId", k.name, k.prefix, k.status::text,
                   COUNT(*),
                   COUNT(*) FILTER (WHERE r.status BETWEEN 400 AND 499),
                   COUNT(*) FILTER (WHERE r.status >= 500),
                   percentile_cont(0.95) WITHIN GROUP (ORDER BY r."durationMs"),
                   MAX(r."createdAt") AT TIME ZONE 'UTC'
            FROM api_requests r
            LEFT JOIN api_keys k ON k.id = r."apiKeyId"
            WHERE r.surface = 'MERCHANT'::"ApiSurface"
              AND r."createdAt" >= (NOW() AT TIME ZONE 'UTC') - make_interval(secs => $1::double precision)
            GROUP BY 1, 2, 3, 4
            ORDER BY 5 DESC
            LIMIT $2
            "#,
        )
        .bind(window_secs)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|row| {
                let (
                    api_key_id,
                    name,
                    prefix,
                    status,
                    count,
                    client_errors,
                    server_errors,
                    p95_ms,
                    last_seen_at,
                ) = row;
                TrafficApiKeyStat {
                    api_key_id,
                    name,
                    prefix,
                    status,
                    count,
                    client_errors,
                    server_errors,
                    p95_ms,
                    last_seen_at,
                }
            })
            .collect())
    }

    pub async fn list_api_requests(
        &self,
        filter: &ApiRequestFilter,
        limit: i64,
        offset: i64,
    ) -> Result<(Vec<ApiRequestRow>, i64)> {
        let surface_db = filter.surface.map(|s| s.as_db().to_string());
        let (status_lo, status_hi) = match filter.status_class.map(|c| c.range()) {
            Some((lo, hi)) => (Some(lo), Some(hi)),
            None => (None, None),
        };

        let rows = sqlx::query_as::<_, ApiRequestRow>(&format!(
            r#"
            SELECT {REQUEST_LIST_COLUMNS}
            FROM api_requests r
            LEFT JOIN api_keys k ON k.id = r."apiKeyId"
            LEFT JOIN staff_users s ON s.id = r."staffId"
            WHERE {REQUEST_LIST_WHERE}
            ORDER BY r."createdAt" DESC, r.id DESC
            LIMIT $7 OFFSET $8
            "#
        ))
        .bind(surface_db.as_deref())
        .bind(status_lo)
        .bind(status_hi)
        .bind(filter.route.as_deref())
        .bind(filter.method.as_deref())
        .bind(filter.api_key_id.as_deref())
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.pool)
        .await?;

        let (total,): (i64,) = sqlx::query_as(&format!(
            r#"
            SELECT COUNT(*) FROM api_requests r
            WHERE {REQUEST_LIST_WHERE}
            "#
        ))
        .bind(surface_db.as_deref())
        .bind(status_lo)
        .bind(status_hi)
        .bind(filter.route.as_deref())
        .bind(filter.method.as_deref())
        .bind(filter.api_key_id.as_deref())
        .fetch_one(&self.pool)
        .await?;

        Ok((rows, total))
    }

    /// Delete up to `batch` rows older than `days`. Callers loop until this
    /// returns 0 so no single DELETE holds locks for long.
    pub async fn purge_api_requests(&self, days: i64, batch: i64) -> Result<u64> {
        let result = sqlx::query(
            r#"
            DELETE FROM api_requests
            WHERE id IN (
                SELECT id FROM api_requests
                WHERE "createdAt" < (NOW() AT TIME ZONE 'UTC') - make_interval(days => $1::int)
                ORDER BY "createdAt"
                LIMIT $2
            )
            "#,
        )
        .bind(days)
        .bind(batch)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }
}
