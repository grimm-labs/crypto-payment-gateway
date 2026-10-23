use std::collections::HashMap;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use domain::{
    admit_transfer, decimal_to_raw, deposit_event, is_amount_paid, DepositAddressStatus,
    DepositStatus, Network, Token, TransferAdmission, WebhookDeliveryStatus, WebhookEventType,
    WebhookSubject,
};
use rust_decimal::Decimal;
use serde_json::Value;
use sqlx::{postgres::PgPoolOptions, FromRow, PgConnection, PgPool};
use uuid::Uuid;

mod address_book;
mod admin_queries;
mod alerts;
mod amount;
mod api_keys;
mod api_traffic;
mod asset_settings;
mod audit;
pub mod ledger;
pub mod outbound;
mod payouts;
mod projects;
mod settings;
mod staff;
mod sweeps;
pub mod transfers;
mod webhook_endpoints;
mod withdrawals;

pub use address_book::{AddressBookEntryRow, NewAddressBookEntry};
pub use admin_queries::{AdminStats, DailyDepositStat, FailedWebhookTarget, WebhookDeliveryDetailRow};
pub use alerts::{AlertRow, NewAlert};
pub use amount::RawAmount;
pub use api_keys::{ApiKeyListRow, ApiKeyRow, WithdrawalGrant};
pub use asset_settings::{
    ApiKeyWithdrawalLimitRow, AssetSettingsRow, AssetSettingsUpdate, SweepSettingsRow,
};
pub use outbound::{NewOutboundTx, OutboundTxRow, OutboundUpdate};
pub use api_traffic::{
    ApiRequestFilter, ApiRequestRow, ApiTrafficEvent, TrafficApiKeyStat, TrafficBucket,
    TrafficRouteStat, TrafficSummary,
};
pub use audit::{AuditActor, AuditLogRow, NewAuditEntry};
pub use ledger::{LedgerFilter, ProjectComponentsRow};
pub use payouts::PayoutRequestRow;
pub use projects::{
    CreatedProject, LegacyData, LegacyOutcome, NewProject, ProjectCounts, ProjectCreate,
    ProjectDelete, ProjectListRow, ProjectRow, ProjectUpdate, UnassignedCounts,
};
pub use settings::{
    API_KEY_PREFIX_SETTING, API_TRAFFIC_RETENTION_FOREVER, API_TRAFFIC_RETENTION_SETTING,
    DEPOSIT_EXPIRY_SETTING, OUTFLOWS_FROZEN_SETTING, TOTP_ISSUER_SETTING,
};
pub use staff::{StaffUpdate, StaffUserRow};
pub use sweeps::{NewSweep, SweepCandidate, SweepClaim, SweepRow};
pub use transfers::{InternalTransferRow, NewInternalTransfer, TransferFilter};
pub use webhook_endpoints::{WebhookEndpointListRow, WebhookEndpointRow};
pub use withdrawals::{
    NewWithdrawal, Reservations, WithdrawalAdmission, WithdrawalClaim, WithdrawalFilter,
    WithdrawalRow, WithdrawalUpdate,
};

#[derive(Clone)]
pub struct Db {
    pool: PgPool,
}

// sqlx refuses to decode a Postgres enum column into a Rust String, and a
// TIMESTAMP(3) column into DateTime<Utc>, so every query that materializes a
// row must cast enums to text and timestamps to timestamptz (stored values
// are UTC instants) instead of using SELECT * / RETURNING *.
const DEPOSIT_ADDRESS_COLUMNS: &str = r#"id, network::text AS network, token::text AS token, address, "derivationIndex", "expectedAmount", status::text AS status, "expiresAt" AT TIME ZONE 'UTC' AS "expiresAt", metadata, "apiKeyId", "projectId", "btcReceivedRaw"::text AS "btcReceivedRaw", "watchUntil" AT TIME ZONE 'UTC' AS "watchUntil", "createdAt" AT TIME ZONE 'UTC' AS "createdAt", "updatedAt" AT TIME ZONE 'UTC' AS "updatedAt""#;
const DEPOSIT_COLUMNS: &str = r#"id, "depositAddressId", "txHash", amount, "amountRaw", confirmations, status::text AS status, late, "detectedAt" AT TIME ZONE 'UTC' AS "detectedAt", "confirmedAt" AT TIME ZONE 'UTC' AS "confirmedAt""#;
/// The confirmed total of the deposits selected, in base units (text).
const CONFIRMED_TOTAL: &str = r#"COALESCE(SUM("amountRaw"::numeric), 0)::text"#;
const WEBHOOK_DELIVERY_COLUMNS: &str = r#"id, "depositId", "depositAddressId", "lightningPaymentId", "withdrawalId", "alertId", "transferId", "eventType", "webhookEndpointId", status::text AS status, attempts, "lastResponse", "lastError", "createdAt" AT TIME ZONE 'UTC' AS "createdAt", "updatedAt" AT TIME ZONE 'UTC' AS "updatedAt""#;

#[derive(Debug, Clone, FromRow)]
pub struct DepositAddressRow {
    pub id: String,
    pub network: String,
    pub token: String,
    pub address: String,
    #[sqlx(rename = "derivationIndex")]
    pub derivation_index: i32,
    #[sqlx(rename = "expectedAmount")]
    pub expected_amount: Decimal,
    pub status: String,
    #[sqlx(rename = "expiresAt")]
    pub expires_at: DateTime<Utc>,
    pub metadata: Value,
    /// Merchant key that opened the payment; NULL when created from the
    /// dashboard (or before this column existed).
    #[sqlx(rename = "apiKeyId")]
    pub api_key_id: Option<String>,
    /// Owning project; NULL only on payments from before the first project.
    #[sqlx(rename = "projectId")]
    pub project_id: Option<String>,
    /// Bitcoin: the total confirmed amount ever received, as last scanned
    /// (only ever raised). NULL elsewhere.
    #[sqlx(rename = "btcReceivedRaw")]
    pub btc_received_raw: Option<RawAmount>,
    /// End of the late watch (ADR 0005): a paid or expired address is
    /// polled until then.
    #[sqlx(rename = "watchUntil")]
    pub watch_until: DateTime<Utc>,
    #[sqlx(rename = "createdAt")]
    pub created_at: DateTime<Utc>,
    #[sqlx(rename = "updatedAt")]
    pub updated_at: DateTime<Utc>,
}

#[derive(FromRow)]
struct KeyedDepositAddressRow {
    #[sqlx(flatten)]
    row: DepositAddressRow,
    #[sqlx(rename = "paramsHash")]
    params_hash: Option<String>,
}

/// Which owned rows a listing shows: every row, one project's, or the
/// merchant's (`"projectId" IS NULL`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum OwnerFilter {
    #[default]
    Any,
    Project(String),
    Merchant,
}

impl OwnerFilter {
    /// The two binds of [`OWNER_CONDITION`]: the project id to match, and
    /// whether only merchant rows are wanted.
    pub fn binds(&self) -> (Option<&str>, bool) {
        match self {
            Self::Any => (None, false),
            Self::Project(id) => (Some(id.as_str()), false),
            Self::Merchant => (None, true),
        }
    }
}

/// SQL of an [`OwnerFilter`] over a `"projectId"` column, with the binds of
/// `OwnerFilter::binds` at `$project` / `$merchant` (callers format them).
pub(crate) fn owner_condition(column: &str, project: &str, merchant: &str) -> String {
    format!(r#"({project}::text IS NULL OR {column} = {project}) AND (NOT {merchant} OR {column} IS NULL)"#)
}

#[derive(Debug, Clone, FromRow)]
pub struct DepositRow {
    pub id: String,
    #[sqlx(rename = "depositAddressId")]
    pub deposit_address_id: String,
    #[sqlx(rename = "txHash")]
    pub tx_hash: String,
    pub amount: Decimal,
    #[sqlx(rename = "amountRaw")]
    pub amount_raw: String,
    pub confirmations: i32,
    pub status: String,
    /// First seen once its address had left PENDING (ADR 0005).
    pub late: bool,
    #[sqlx(rename = "detectedAt")]
    pub detected_at: DateTime<Utc>,
    #[sqlx(rename = "confirmedAt")]
    pub confirmed_at: Option<DateTime<Utc>>,
}

/// A deposit address to create.
#[derive(Debug, Clone)]
pub struct NewDepositAddress<'a> {
    pub network: Network,
    pub token: Token,
    pub address: &'a str,
    pub derivation_index: i32,
    pub expected_amount: Decimal,
    pub expires_at: DateTime<Utc>,
    /// End of the late watch (ADR 0005).
    pub watch_until: DateTime<Utc>,
    pub metadata: Value,
    pub api_key_id: Option<&'a str>,
    pub project_id: Option<&'a str>,
    /// The `Idempotency-Key` of the request and the hash of its parameters
    /// (merchant keys only).
    pub idempotency: Option<(&'a str, &'a str)>,
}

/// Whether `err` is the unique violation of an idempotency key (a
/// concurrent request with the same key won the race).
pub fn is_idempotency_conflict(err: &anyhow::Error) -> bool {
    err.downcast_ref::<sqlx::Error>()
        .and_then(|e| e.as_database_error())
        .is_some_and(|e| e.is_unique_violation() && e.constraint().is_some_and(|c| c.contains("idempotencyKey")))
}

/// What recording a transfer did (ADR 0005).
#[derive(Debug, Clone)]
pub struct RecordedDeposit {
    pub deposit: DepositRow,
    /// This call confirmed the deposit (it was detected, or is new).
    pub newly_confirmed: bool,
    /// This call set the address PAID: its confirmed total reached the
    /// expected amount within the tolerance.
    pub paid: bool,
    /// The event this call fired (`domain::deposit_event`)…
    pub event: Option<WebhookEventType>,
    /// …and the endpoints whose PENDING delivery row it wrote, in the same
    /// transaction: the caller queues them after the commit.
    pub deliveries: Vec<String>,
}

/// An address the expiry sweep expired, with the endpoints whose EXPIRED
/// delivery row was written in the same transaction.
#[derive(Debug, Clone)]
pub struct ExpiredAddress {
    pub id: String,
    pub deliveries: Vec<String>,
}

/// A PENDING delivery as the reconciliation reads it: enough to rebuild its
/// queue member and to tell when its next attempt is due.
#[derive(Debug, Clone, FromRow)]
pub struct PendingDeliveryRow {
    pub id: String,
    #[sqlx(rename = "eventType")]
    pub event_type: String,
    #[sqlx(rename = "depositId")]
    pub deposit_id: Option<String>,
    #[sqlx(rename = "depositAddressId")]
    pub deposit_address_id: Option<String>,
    #[sqlx(rename = "lightningPaymentId")]
    pub lightning_payment_id: Option<String>,
    #[sqlx(rename = "withdrawalId")]
    pub withdrawal_id: Option<String>,
    #[sqlx(rename = "alertId")]
    pub alert_id: Option<String>,
    #[sqlx(rename = "transferId")]
    pub transfer_id: Option<String>,
    #[sqlx(rename = "webhookEndpointId")]
    pub webhook_endpoint_id: String,
    pub attempts: i32,
    #[sqlx(rename = "updatedAt")]
    pub updated_at: DateTime<Utc>,
}

impl PendingDeliveryRow {
    /// The subject id the queue member of `event` carries.
    pub fn subject_id(&self, event: WebhookEventType) -> Option<&str> {
        match event.subject() {
            WebhookSubject::Deposit => self.deposit_id.as_deref(),
            WebhookSubject::DepositAddress => self.deposit_address_id.as_deref(),
            WebhookSubject::LightningPayment => self.lightning_payment_id.as_deref(),
            WebhookSubject::Withdrawal => self.withdrawal_id.as_deref(),
            WebhookSubject::Alert => self.alert_id.as_deref(),
            WebhookSubject::Transfer => self.transfer_id.as_deref(),
        }
    }
}

/// The subject of the deliveries written with a status change.
#[derive(Debug, Clone, Copy)]
enum DeliverySubject<'a> {
    /// PAID, LATE.
    Deposit { deposit_id: &'a str, address_id: &'a str },
    /// PENDING, EXPIRED.
    Address(&'a str),
}

/// Write the PENDING delivery rows of `event` about `subject`, one per
/// endpoint it fans out to (`webhook_endpoints::subscribed_endpoints`:
/// active, subscribed and, once projects exist — `owner_filtered` — owned
/// by `project_id`), inside the caller's transaction: a crash or a lost
/// queue member after the commit loses nothing, the reconciliation queues
/// the row again. Returns the endpoints of the rows written (a key already
/// present is left alone).
async fn insert_event_deliveries(
    conn: &mut PgConnection,
    event: WebhookEventType,
    subject: DeliverySubject<'_>,
    project_id: Option<&str>,
    owner_filtered: bool,
) -> Result<Vec<String>> {
    let condition = webhook_endpoints::subscribed_endpoints("$1", "$2", "$3");
    let (deposit_id, address_id) = match subject {
        DeliverySubject::Deposit { deposit_id, address_id } => (Some(deposit_id), address_id),
        DeliverySubject::Address(address_id) => (None, address_id),
    };
    let rows: Vec<(String,)> = sqlx::query_as(&format!(
        r#"
        INSERT INTO webhook_deliveries
          (id, "depositId", "depositAddressId", "eventType", "webhookEndpointId", status, attempts, "createdAt", "updatedAt")
        SELECT gen_random_uuid()::text, $4, $5, $1, e.id, 'PENDING'::"WebhookDeliveryStatus", 0, NOW(), NOW()
        FROM webhook_endpoints e
        WHERE {condition}
        ON CONFLICT DO NOTHING
        RETURNING "webhookEndpointId"
        "#
    ))
    .bind(event.as_db())
    .bind(owner_filtered)
    .bind(project_id)
    .bind(deposit_id)
    .bind(address_id)
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows.into_iter().map(|(endpoint,)| endpoint).collect())
}

#[derive(Debug, Clone, FromRow)]
pub struct WebhookDeliveryRow {
    pub id: String,
    /// NULL on PENDING/EXPIRED event deliveries (no deposit involved).
    #[sqlx(rename = "depositId")]
    pub deposit_id: Option<String>,
    /// NULL on payout deliveries (keyed on `lightning_payment_id`).
    #[sqlx(rename = "depositAddressId")]
    pub deposit_address_id: Option<String>,
    /// SDK payment id — set on payout deliveries only.
    #[sqlx(rename = "lightningPaymentId")]
    pub lightning_payment_id: Option<String>,
    /// Set on withdrawal deliveries only.
    #[sqlx(rename = "withdrawalId")]
    pub withdrawal_id: Option<String>,
    /// Set on alert deliveries only.
    #[sqlx(rename = "alertId")]
    pub alert_id: Option<String>,
    /// Set on internal-transfer deliveries only.
    #[sqlx(rename = "transferId")]
    pub transfer_id: Option<String>,
    /// DB value of the `WebhookEventType` this delivery notifies.
    #[sqlx(rename = "eventType")]
    pub event_type: String,
    /// NULL on rows written before per-endpoint deliveries existed.
    #[sqlx(rename = "webhookEndpointId")]
    pub webhook_endpoint_id: Option<String>,
    pub status: String,
    pub attempts: i32,
    #[sqlx(rename = "lastResponse")]
    pub last_response: Option<i32>,
    #[sqlx(rename = "lastError")]
    pub last_error: Option<String>,
    #[sqlx(rename = "createdAt")]
    pub created_at: DateTime<Utc>,
    #[sqlx(rename = "updatedAt")]
    pub updated_at: DateTime<Utc>,
}

impl Db {
    pub async fn connect(database_url: &str) -> Result<Self> {
        let pool = PgPoolOptions::new()
            .max_connections(10)
            .connect(database_url)
            .await
            .context("Failed to connect to PostgreSQL")?;
        Ok(Self { pool })
    }

    /// A pool that connects on first use — for tests that build an
    /// `AppState` without a database (nothing is ever queried).
    pub fn connect_lazy(database_url: &str) -> Result<Self> {
        let pool = PgPoolOptions::new()
            .max_connections(1)
            .connect_lazy(database_url)
            .context("Invalid DATABASE_URL")?;
        Ok(Self { pool })
    }

    pub async fn migrate(&self) -> Result<()> {
        sqlx::migrate!("./migrations")
            .run(&self.pool)
            .await
            .context("Failed to run database migrations")?;
        Ok(())
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    pub async fn ping(&self) -> Result<()> {
        sqlx::query("SELECT 1")
            .execute(&self.pool)
            .await
            .context("PostgreSQL ping failed")?;
        Ok(())
    }

    /// Allocate the next derivation index of `network` atomically: two
    /// concurrent creations never get the same index. An index whose
    /// creation then fails is simply skipped (gaps are harmless).
    pub async fn next_derivation_index(&self, network: Network) -> Result<i32> {
        let (index,): (i32,) = sqlx::query_as(
            r#"
            INSERT INTO derivation_counters (network, "nextIndex")
            VALUES (
                $1::"Network",
                COALESCE((SELECT MAX("derivationIndex") + 1 FROM deposit_addresses WHERE network = $1::"Network"), 0) + 1
            )
            ON CONFLICT (network) DO UPDATE SET "nextIndex" = derivation_counters."nextIndex" + 1
            RETURNING "nextIndex" - 1
            "#,
        )
        .bind(network.as_db())
        .fetch_one(&self.pool)
        .await?;
        Ok(index)
    }

    pub async fn max_derivation_index(&self, network: Network) -> Result<i32> {
        let row: Option<(i32,)> = sqlx::query_as(
            r#"SELECT "derivationIndex" FROM deposit_addresses WHERE network = $1::"Network" ORDER BY "derivationIndex" DESC LIMIT 1"#,
        )
        .bind(network.as_db())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|(i,)| i).unwrap_or(0))
    }

    /// Create a PENDING deposit address, with its `pending` deliveries (see
    /// `insert_event_deliveries` for `owner_filtered`). Returns the
    /// endpoints of the deliveries written. A reused idempotency key fails
    /// with a unique violation (`is_idempotency_conflict`).
    pub async fn create_deposit_address(
        &self,
        new: NewDepositAddress<'_>,
        owner_filtered: bool,
    ) -> Result<(DepositAddressRow, Vec<String>)> {
        let id = Uuid::new_v4().to_string();
        let now = Utc::now();
        let (idempotency_key, params_hash) = new.idempotency.unzip();
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query_as::<_, DepositAddressRow>(&format!(
            r#"
            INSERT INTO deposit_addresses
              (id, network, token, address, "derivationIndex", "expectedAmount", status, "expiresAt", metadata, "createdAt", "updatedAt", "apiKeyId", "projectId", "watchUntil", "idempotencyKey", "paramsHash")
            VALUES
              ($1, $2::"Network", $3::"Token", $4, $5, $6, 'PENDING'::"DepositAddressStatus", $7, $8, $9, $9, $10, $11, $12, $13, $14)
            RETURNING {DEPOSIT_ADDRESS_COLUMNS}
            "#
        ))
        .bind(&id)
        .bind(new.network.as_db())
        .bind(new.token.as_db())
        .bind(new.address)
        .bind(new.derivation_index)
        .bind(new.expected_amount)
        .bind(new.expires_at)
        .bind(new.metadata)
        .bind(now)
        .bind(new.api_key_id)
        .bind(new.project_id)
        .bind(new.watch_until)
        .bind(idempotency_key)
        .bind(params_hash)
        .fetch_one(&mut *tx)
        .await?;
        // `pending` is written with the address (spec §4.5).
        let deliveries = insert_event_deliveries(
            &mut tx,
            WebhookEventType::Pending,
            DeliverySubject::Address(&row.id),
            row.project_id.as_deref(),
            owner_filtered,
        )
        .await?;
        tx.commit().await?;
        Ok((row, deliveries))
    }

    /// The payment a merchant key created with `idempotency_key`, and the
    /// parameters hash it was created with.
    pub async fn find_deposit_address_by_idempotency_key(
        &self,
        api_key_id: &str,
        idempotency_key: &str,
    ) -> Result<Option<(DepositAddressRow, String)>> {
        let row = sqlx::query_as::<_, KeyedDepositAddressRow>(&format!(
            r#"SELECT {DEPOSIT_ADDRESS_COLUMNS}, "paramsHash" FROM deposit_addresses
               WHERE "apiKeyId" = $1 AND "idempotencyKey" = $2"#
        ))
        .bind(api_key_id)
        .bind(idempotency_key)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|keyed| (keyed.row, keyed.params_hash.unwrap_or_default())))
    }

    /// Open (PENDING) payments created with this merchant key.
    pub async fn count_pending_addresses_for_api_key(&self, api_key_id: &str) -> Result<i64> {
        let (count,): (i64,) = sqlx::query_as(
            r#"SELECT COUNT(*) FROM deposit_addresses WHERE "apiKeyId" = $1 AND status = 'PENDING'::"DepositAddressStatus""#,
        )
        .bind(api_key_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(count)
    }

    /// `owner`: the project's payments, the merchant's (legacy rows), or all.
    pub async fn list_deposit_addresses(
        &self,
        status: Option<DepositAddressStatus>,
        network: Option<Network>,
        owner: &OwnerFilter,
        limit: i64,
        offset: i64,
    ) -> Result<(Vec<DepositAddressRow>, i64)> {
        let status_db = status.map(|s| s.as_db().to_string());
        let network_db = network.map(|n| n.as_db().to_string());
        let (project_id, merchant_only) = owner.binds();
        let condition = format!(
            r#"($1::text IS NULL OR status = $1::"DepositAddressStatus")
               AND ($2::text IS NULL OR network = $2::"Network")
               AND {}"#,
            owner_condition(r#""projectId""#, "$3", "$4")
        );

        let rows = sqlx::query_as::<_, DepositAddressRow>(&format!(
            r#"
            SELECT {DEPOSIT_ADDRESS_COLUMNS} FROM deposit_addresses
            WHERE {condition}
            ORDER BY "createdAt" DESC
            LIMIT $5 OFFSET $6
            "#
        ))
        .bind(status_db.as_deref())
        .bind(network_db.as_deref())
        .bind(project_id)
        .bind(merchant_only)
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.pool)
        .await?;

        let (total,): (i64,) = sqlx::query_as(&format!(
            "SELECT COUNT(*) FROM deposit_addresses WHERE {condition}"
        ))
        .bind(status_db.as_deref())
        .bind(network_db.as_deref())
        .bind(project_id)
        .bind(merchant_only)
        .fetch_one(&self.pool)
        .await?;

        Ok((rows, total))
    }

    pub async fn find_deposit_address(&self, id: &str) -> Result<Option<DepositAddressRow>> {
        let row = sqlx::query_as::<_, DepositAddressRow>(&format!(
            "SELECT {DEPOSIT_ADDRESS_COLUMNS} FROM deposit_addresses WHERE id = $1"
        ))
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    /// The merchant surface's lookup (invariant 2): the payment only when
    /// `owner` matches, in the query itself — another owner's payment is
    /// as absent as a missing one.
    pub async fn find_deposit_address_owned(
        &self,
        id: &str,
        owner: &OwnerFilter,
    ) -> Result<Option<DepositAddressRow>> {
        let (project_id, merchant_only) = owner.binds();
        let row = sqlx::query_as::<_, DepositAddressRow>(&format!(
            "SELECT {DEPOSIT_ADDRESS_COLUMNS} FROM deposit_addresses WHERE id = $1 AND {}",
            owner_condition(r#""projectId""#, "$2", "$3")
        ))
        .bind(id)
        .bind(project_id)
        .bind(merchant_only)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    /// The merchant surface's deposit lookup (invariant 2): the deposit
    /// only when its address has `owner`, in the query itself.
    pub async fn find_deposit_owned(&self, id: &str, owner: &OwnerFilter) -> Result<Option<DepositRow>> {
        let (project_id, merchant_only) = owner.binds();
        let row = sqlx::query_as::<_, DepositRow>(&format!(
            r#"SELECT {DEPOSIT_COLUMNS} FROM deposits d
               WHERE d.id = $1 AND EXISTS (
                 SELECT 1 FROM deposit_addresses a WHERE a.id = d."depositAddressId" AND {}
               )"#,
            owner_condition(r#"a."projectId""#, "$2", "$3")
        ))
        .bind(id)
        .bind(project_id)
        .bind(merchant_only)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    pub async fn list_deposits_for_address(&self, address_id: &str) -> Result<Vec<DepositRow>> {
        let rows = sqlx::query_as::<_, DepositRow>(&format!(
            r#"SELECT {DEPOSIT_COLUMNS} FROM deposits WHERE "depositAddressId" = $1 ORDER BY "detectedAt" DESC"#
        ))
        .bind(address_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// The confirmed total of an address in base units, late deposits
    /// included (ADR 0005); 0 when nothing is confirmed.
    pub async fn confirmed_total_raw(&self, address_id: &str) -> Result<u128> {
        Ok(self
            .confirmed_totals_raw(&[address_id.to_string()])
            .await?
            .remove(address_id)
            .unwrap_or(0))
    }

    /// `confirmed_total_raw` of several addresses (absent = 0).
    pub async fn confirmed_totals_raw(&self, address_ids: &[String]) -> Result<HashMap<String, u128>> {
        if address_ids.is_empty() {
            return Ok(HashMap::new());
        }
        let rows: Vec<(String, RawAmount)> = sqlx::query_as(&format!(
            r#"SELECT "depositAddressId", {CONFIRMED_TOTAL} FROM deposits
               WHERE "depositAddressId" = ANY($1) AND status = 'CONFIRMED'::"DepositStatus"
               GROUP BY "depositAddressId""#
        ))
        .bind(address_ids)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|(id, total)| (id, total.0)).collect())
    }

    /// Transition `network`'s PENDING addresses to EXPIRED once their
    /// deadline is more than `grace_secs` old — unless a DETECTED deposit is
    /// still waiting for its confirmations (then they are held until
    /// `hold_secs` past the deadline). Returns the addresses expired by THIS
    /// call (the UPDATE is atomic, so a concurrent caller gets an empty set),
    /// with the EXPIRED delivery rows written in the same transaction —
    /// callers queue them once.
    ///
    /// `polled` lists the addresses the caller just polled successfully
    /// (`None` = all of them): an address whose last poll failed is not
    /// expired on the grace rule — only once `hold_secs` have passed, the
    /// hard stop that also bounds a provider outage. `owner_filtered`: see
    /// `insert_event_deliveries`.
    pub async fn expire_stale_addresses(
        &self,
        network: Network,
        grace_secs: i64,
        hold_secs: i64,
        polled: Option<&[String]>,
        owner_filtered: bool,
    ) -> Result<Vec<ExpiredAddress>> {
        let mut tx = self.pool.begin().await?;
        let rows: Vec<(String, Option<String>)> = sqlx::query_as(
            r#"
            UPDATE deposit_addresses a
            SET status = 'EXPIRED'::"DepositAddressStatus", "updatedAt" = NOW()
            WHERE a.network = $1::"Network"
              AND a.status = 'PENDING'::"DepositAddressStatus"
              AND a."expiresAt" < (NOW() AT TIME ZONE 'UTC') - make_interval(secs => $2)
              AND (
                a."expiresAt" < (NOW() AT TIME ZONE 'UTC') - make_interval(secs => $3)
                OR (
                  ($4::text[] IS NULL OR a.id = ANY($4))
                  AND NOT EXISTS (
                    SELECT 1 FROM deposits d
                    WHERE d."depositAddressId" = a.id AND d.status = 'DETECTED'::"DepositStatus"
                  )
                )
              )
            RETURNING a.id, a."projectId"
            "#,
        )
        .bind(network.as_db())
        .bind(grace_secs as f64)
        .bind(hold_secs as f64)
        .bind(polled)
        .fetch_all(&mut *tx)
        .await?;
        let mut expired = Vec::with_capacity(rows.len());
        for (id, project_id) in rows {
            let deliveries = insert_event_deliveries(
                &mut tx,
                WebhookEventType::Expired,
                DeliverySubject::Address(&id),
                project_id.as_deref(),
                owner_filtered,
            )
            .await?;
            expired.push(ExpiredAddress { id, deliveries });
        }
        tx.commit().await?;
        Ok(expired)
    }

    /// The addresses the pollers watch on `network` (ADR 0005, the SQL form
    /// of `domain::is_watched`): every PENDING one — deadline passed or
    /// not, an address is polled until the expiry sweep (which runs right
    /// after the poll, see `expire_stale_addresses`) has expired it — then
    /// the PAID and EXPIRED ones whose late watch is still open or that
    /// still hold a DETECTED deposit. Never a Lightning invoice once it left
    /// PENDING (an expired invoice cannot be paid). Open payments first,
    /// oldest first.
    pub async fn watched_addresses(&self, network: Network) -> Result<Vec<DepositAddressRow>> {
        let rows = sqlx::query_as::<_, DepositAddressRow>(&format!(
            r#"
            SELECT {DEPOSIT_ADDRESS_COLUMNS} FROM deposit_addresses a
            WHERE a.network = $1::"Network"
              AND (
                a.status = 'PENDING'::"DepositAddressStatus"
                OR (
                  a.network <> 'LIGHTNING'::"Network"
                  AND (
                    a."watchUntil" > (NOW() AT TIME ZONE 'UTC')
                    OR EXISTS (
                      SELECT 1 FROM deposits d
                      WHERE d."depositAddressId" = a.id AND d.status = 'DETECTED'::"DepositStatus"
                    )
                  )
                )
              )
            ORDER BY (a.status <> 'PENDING'::"DepositAddressStatus"), a."createdAt" ASC
            "#
        ))
        .bind(network.as_db())
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// Every address issued on `network` (whatever token it was issued
    /// for — an address can hold any token of its network).
    pub async fn addresses_for_network(&self, network: Network) -> Result<Vec<String>> {
        let rows: Vec<(String,)> = sqlx::query_as(
            r#"SELECT DISTINCT address FROM deposit_addresses WHERE network = $1::"Network""#,
        )
        .bind(network.as_db())
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|(a,)| a).collect())
    }

    /// Whether `address` is a deposit address the gateway issued on
    /// `network` (case-insensitive: Ethereum addresses come in two
    /// casings). Withdrawals never go there (invariant 7).
    pub async fn is_deposit_address(&self, network: Network, address: &str) -> Result<bool> {
        let (found,): (bool,) = sqlx::query_as(
            r#"SELECT EXISTS (SELECT 1 FROM deposit_addresses WHERE network = $1::"Network" AND lower(address) = lower($2))"#,
        )
        .bind(network.as_db())
        .bind(address)
        .fetch_one(&self.pool)
        .await?;
        Ok(found)
    }

    /// Map an address string back to its row — for Lightning, `address` is
    /// the BOLT11 payment request (unique index on `address`).
    pub async fn find_deposit_address_by_address(
        &self,
        address: &str,
    ) -> Result<Option<DepositAddressRow>> {
        let row = sqlx::query_as::<_, DepositAddressRow>(&format!(
            "SELECT {DEPOSIT_ADDRESS_COLUMNS} FROM deposit_addresses WHERE address = $1"
        ))
        .bind(address)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    /// `(invoice, deposit_addresses.id)` for the given BOLT11 strings that
    /// are gateway Lightning invoices.
    pub async fn lightning_invoice_ids(&self, invoices: &[String]) -> Result<Vec<(String, String)>> {
        if invoices.is_empty() {
            return Ok(Vec::new());
        }
        let rows: Vec<(String, String)> = sqlx::query_as(
            r#"SELECT address, id FROM deposit_addresses WHERE network = 'LIGHTNING'::"Network" AND address = ANY($1)"#,
        )
        .bind(invoices)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// A deposit is identified by (address, tx): one Bitcoin transaction can
    /// pay several gateway addresses.
    pub async fn find_deposit_by_address_and_tx(
        &self,
        deposit_address_id: &str,
        tx_hash: &str,
    ) -> Result<Option<DepositRow>> {
        let row = sqlx::query_as::<_, DepositRow>(&format!(
            r#"SELECT {DEPOSIT_COLUMNS} FROM deposits WHERE "depositAddressId" = $1 AND "txHash" = $2"#
        ))
        .bind(deposit_address_id)
        .bind(tx_hash)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    pub async fn find_deposit(&self, id: &str) -> Result<Option<DepositRow>> {
        let row = sqlx::query_as::<_, DepositRow>(&format!(
            "SELECT {DEPOSIT_COLUMNS} FROM deposits WHERE id = $1"
        ))
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    /// Record a transfer to an address (ADR 0005), under the address row
    /// lock: a new deposit — `late` when the address had already left
    /// PENDING — or the progress of a known one. When the call confirms a
    /// deposit of a PENDING address whose confirmed total now pays it
    /// (`domain::is_amount_paid`), the address turns PAID in the same
    /// transaction — the update re-checks PENDING, so exactly one call ever
    /// pays it. `None` when the transfer is not recorded: no such address, a
    /// Lightning invoice no longer pending, a new transfer after the watch
    /// (`domain::admit_transfer`).
    #[allow(clippy::too_many_arguments)]
    pub async fn upsert_deposit_and_maybe_pay(
        &self,
        deposit_address_id: &str,
        tx_hash: &str,
        amount: Decimal,
        amount_raw: &str,
        confirmations: i32,
        status: DepositStatus,
        tolerance_percent: f64,
        owner_filtered: bool,
    ) -> Result<Option<RecordedDeposit>> {
        let mut tx = self.pool.begin().await?;

        // The address first, then its deposit: every writer of both takes
        // them in this order.
        let address: Option<(String, String, String, Decimal, bool, Option<String>)> = sqlx::query_as(
            r#"
            SELECT status::text, network::text, token::text, "expectedAmount",
                   "watchUntil" > (NOW() AT TIME ZONE 'UTC'), "projectId"
            FROM deposit_addresses WHERE id = $1
            FOR UPDATE
            "#,
        )
        .bind(deposit_address_id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((address_status, network, token, expected_amount, watch_open, project_id)) = address else {
            return Ok(None);
        };
        let address_status = DepositAddressStatus::from_db(&address_status)?;
        let network = Network::from_db(&network)?;
        let token = Token::from_db(&token)?;

        let existing = sqlx::query_as::<_, DepositRow>(&format!(
            r#"SELECT {DEPOSIT_COLUMNS} FROM deposits WHERE "depositAddressId" = $1 AND "txHash" = $2 FOR UPDATE"#
        ))
        .bind(deposit_address_id)
        .bind(tx_hash)
        .fetch_optional(&mut *tx)
        .await?;

        let confirmed_at = (status == DepositStatus::Confirmed).then(Utc::now);
        let deposit = match existing {
            Some(existing) if existing.status == DepositStatus::Confirmed.as_db() => {
                return Ok(Some(RecordedDeposit {
                    deposit: existing,
                    newly_confirmed: false,
                    paid: false,
                    event: None,
                    deliveries: Vec::new(),
                }));
            }
            Some(existing) => {
                sqlx::query_as::<_, DepositRow>(&format!(
                    r#"
                    UPDATE deposits
                    SET confirmations = $2,
                        status = $3::"DepositStatus",
                        "confirmedAt" = $4
                    WHERE id = $1
                    RETURNING {DEPOSIT_COLUMNS}
                    "#
                ))
                .bind(&existing.id)
                .bind(confirmations)
                .bind(status.as_db())
                .bind(confirmed_at.or(existing.confirmed_at))
                .fetch_one(&mut *tx)
                .await?
            }
            None => {
                let TransferAdmission::Record { late } = admit_transfer(network, address_status, watch_open) else {
                    return Ok(None);
                };
                sqlx::query_as::<_, DepositRow>(&format!(
                    r#"
                    INSERT INTO deposits
                      (id, "depositAddressId", "txHash", amount, "amountRaw", confirmations, status, "detectedAt", "confirmedAt", late)
                    VALUES
                      ($1, $2, $3, $4, $5, $6, $7::"DepositStatus", NOW(), $8, $9)
                    RETURNING {DEPOSIT_COLUMNS}
                    "#
                ))
                .bind(Uuid::new_v4().to_string())
                .bind(deposit_address_id)
                .bind(tx_hash)
                .bind(amount)
                .bind(amount_raw)
                .bind(confirmations)
                .bind(status.as_db())
                .bind(confirmed_at)
                .bind(late)
                .fetch_one(&mut *tx)
                .await?
            }
        };

        let newly_confirmed = status == DepositStatus::Confirmed;
        let mut paid = false;
        if newly_confirmed && address_status == DepositAddressStatus::Pending {
            let (total,): (RawAmount,) = sqlx::query_as(&format!(
                r#"SELECT {CONFIRMED_TOTAL} FROM deposits
                   WHERE "depositAddressId" = $1 AND status = 'CONFIRMED'::"DepositStatus""#
            ))
            .bind(deposit_address_id)
            .fetch_one(&mut *tx)
            .await?;
            let expected_raw: u128 = decimal_to_raw(expected_amount, token)
                .parse()
                .context("expected amount out of range")?;
            if is_amount_paid(total.0, expected_raw, tolerance_percent) {
                paid = sqlx::query(
                    r#"
                    UPDATE deposit_addresses
                    SET status = 'PAID'::"DepositAddressStatus", "updatedAt" = NOW()
                    WHERE id = $1 AND status = 'PENDING'::"DepositAddressStatus"
                    "#,
                )
                .bind(deposit_address_id)
                .execute(&mut *tx)
                .await?
                .rows_affected()
                    == 1;
            }
        }

        // `paid` / `late` are written with the status change (spec §4.5).
        let event = deposit_event(paid, newly_confirmed, deposit.late);
        let deliveries = match event {
            Some(event) => {
                insert_event_deliveries(
                    &mut tx,
                    event,
                    DeliverySubject::Deposit { deposit_id: &deposit.id, address_id: deposit_address_id },
                    project_id.as_deref(),
                    owner_filtered,
                )
                .await?
            }
            None => Vec::new(),
        };

        tx.commit().await?;
        Ok(Some(RecordedDeposit { deposit, newly_confirmed, paid, event, deliveries }))
    }

    /// Get-or-create for a delivery about a deposit (PAID, LATE), keyed on
    /// (deposit, event, endpoint).
    pub async fn get_or_create_deposit_webhook_delivery(
        &self,
        event: WebhookEventType,
        deposit_id: &str,
        endpoint_id: &str,
    ) -> Result<WebhookDeliveryRow> {
        if let Some(row) = self.find_deposit_webhook_delivery(event, deposit_id, endpoint_id).await? {
            return Ok(row);
        }
        self.insert_deposit_webhook_delivery(event, deposit_id, endpoint_id).await
    }

    /// New deposit delivery row; `depositAddressId` is resolved from the
    /// deposit. A concurrent insert of the same key (unique index) yields
    /// the row that won instead of a duplicate.
    async fn insert_deposit_webhook_delivery(
        &self,
        event: WebhookEventType,
        deposit_id: &str,
        endpoint_id: &str,
    ) -> Result<WebhookDeliveryRow> {
        deposit_subject(event)?;
        let id = Uuid::new_v4().to_string();
        let now = Utc::now();
        let inserted = sqlx::query_as::<_, WebhookDeliveryRow>(&format!(
            r#"
            INSERT INTO webhook_deliveries
              (id, "depositId", "depositAddressId", "eventType", "webhookEndpointId", status, attempts, "createdAt", "updatedAt")
            SELECT $1, d.id, d."depositAddressId", $5, $3, 'PENDING'::"WebhookDeliveryStatus", 0, $4, $4
            FROM deposits d WHERE d.id = $2
            ON CONFLICT DO NOTHING
            RETURNING {WEBHOOK_DELIVERY_COLUMNS}
            "#
        ))
        .bind(&id)
        .bind(deposit_id)
        .bind(endpoint_id)
        .bind(now)
        .bind(event.as_db())
        .fetch_optional(&self.pool)
        .await?;
        match inserted {
            Some(row) => Ok(row),
            None => self
                .find_deposit_webhook_delivery(event, deposit_id, endpoint_id)
                .await?
                .with_context(|| format!("No delivery row for deposit {deposit_id}")),
        }
    }

    /// Get-or-create for an address-scoped event delivery (PENDING/EXPIRED —
    /// no deposit involved), keyed on (address, event, endpoint).
    pub async fn get_or_create_address_webhook_delivery(
        &self,
        address_id: &str,
        event: WebhookEventType,
        endpoint_id: &str,
    ) -> Result<WebhookDeliveryRow> {
        if let Some(row) = self
            .find_address_webhook_delivery(address_id, event, endpoint_id)
            .await?
        {
            return Ok(row);
        }

        let id = Uuid::new_v4().to_string();
        let now = Utc::now();
        let inserted = sqlx::query_as::<_, WebhookDeliveryRow>(&format!(
            r#"
            INSERT INTO webhook_deliveries
              (id, "depositAddressId", "eventType", "webhookEndpointId", status, attempts, "createdAt", "updatedAt")
            VALUES
              ($1, $2, $3, $4, 'PENDING'::"WebhookDeliveryStatus", 0, $5, $5)
            ON CONFLICT DO NOTHING
            RETURNING {WEBHOOK_DELIVERY_COLUMNS}
            "#
        ))
        .bind(&id)
        .bind(address_id)
        .bind(event.as_db())
        .bind(endpoint_id)
        .bind(now)
        .fetch_optional(&self.pool)
        .await?;
        match inserted {
            Some(row) => Ok(row),
            None => self
                .find_address_webhook_delivery(address_id, event, endpoint_id)
                .await?
                .with_context(|| format!("No delivery row for address {address_id}")),
        }
    }

    /// Most recent delivery row for an (address, event, endpoint) triple.
    pub async fn find_address_webhook_delivery(
        &self,
        address_id: &str,
        event: WebhookEventType,
        endpoint_id: &str,
    ) -> Result<Option<WebhookDeliveryRow>> {
        let row = sqlx::query_as::<_, WebhookDeliveryRow>(&format!(
            r#"
            SELECT {WEBHOOK_DELIVERY_COLUMNS} FROM webhook_deliveries
            WHERE "depositAddressId" = $1 AND "eventType" = $2 AND "webhookEndpointId" = $3
            ORDER BY "createdAt" DESC LIMIT 1
            "#
        ))
        .bind(address_id)
        .bind(event.as_db())
        .bind(endpoint_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    /// Get-or-create for a payout event delivery, keyed on (Lightning
    /// payment id, event, endpoint).
    pub async fn get_or_create_payout_webhook_delivery(
        &self,
        payment_id: &str,
        event: WebhookEventType,
        endpoint_id: &str,
    ) -> Result<WebhookDeliveryRow> {
        if let Some(row) = self
            .find_payout_webhook_delivery(payment_id, event, endpoint_id)
            .await?
        {
            return Ok(row);
        }

        let id = Uuid::new_v4().to_string();
        let now = Utc::now();
        let inserted = sqlx::query_as::<_, WebhookDeliveryRow>(&format!(
            r#"
            INSERT INTO webhook_deliveries
              (id, "lightningPaymentId", "eventType", "webhookEndpointId", status, attempts, "createdAt", "updatedAt")
            VALUES
              ($1, $2, $3, $4, 'PENDING'::"WebhookDeliveryStatus", 0, $5, $5)
            ON CONFLICT DO NOTHING
            RETURNING {WEBHOOK_DELIVERY_COLUMNS}
            "#
        ))
        .bind(&id)
        .bind(payment_id)
        .bind(event.as_db())
        .bind(endpoint_id)
        .bind(now)
        .fetch_optional(&self.pool)
        .await?;
        match inserted {
            Some(row) => Ok(row),
            None => self
                .find_payout_webhook_delivery(payment_id, event, endpoint_id)
                .await?
                .with_context(|| format!("No delivery row for payment {payment_id}")),
        }
    }

    /// Most recent delivery row for a (Lightning payment, event, endpoint).
    pub async fn find_payout_webhook_delivery(
        &self,
        payment_id: &str,
        event: WebhookEventType,
        endpoint_id: &str,
    ) -> Result<Option<WebhookDeliveryRow>> {
        let row = sqlx::query_as::<_, WebhookDeliveryRow>(&format!(
            r#"
            SELECT {WEBHOOK_DELIVERY_COLUMNS} FROM webhook_deliveries
            WHERE "lightningPaymentId" = $1 AND "eventType" = $2 AND "webhookEndpointId" = $3
            ORDER BY "createdAt" DESC LIMIT 1
            "#
        ))
        .bind(payment_id)
        .bind(event.as_db())
        .bind(endpoint_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    /// Get-or-create for a delivery keyed on (subject, event, endpoint),
    /// the subject column following `event.subject()` — every event but
    /// PAID and LATE (keyed on their deposit, see
    /// `get_or_create_deposit_webhook_delivery`).
    pub async fn get_or_create_subject_webhook_delivery(
        &self,
        event: WebhookEventType,
        subject_id: &str,
        endpoint_id: &str,
    ) -> Result<WebhookDeliveryRow> {
        if let Some(row) = self
            .find_subject_webhook_delivery(event, subject_id, endpoint_id)
            .await?
        {
            return Ok(row);
        }
        let column = subject_column(event)?;
        let inserted = sqlx::query_as::<_, WebhookDeliveryRow>(&format!(
            r#"
            INSERT INTO webhook_deliveries
              (id, "{column}", "eventType", "webhookEndpointId", status, attempts, "createdAt", "updatedAt")
            VALUES
              ($1, $2, $3, $4, 'PENDING'::"WebhookDeliveryStatus", 0, NOW(), NOW())
            ON CONFLICT DO NOTHING
            RETURNING {WEBHOOK_DELIVERY_COLUMNS}
            "#
        ))
        .bind(Uuid::new_v4().to_string())
        .bind(subject_id)
        .bind(event.as_db())
        .bind(endpoint_id)
        .fetch_optional(&self.pool)
        .await?;
        match inserted {
            Some(row) => Ok(row),
            None => self
                .find_subject_webhook_delivery(event, subject_id, endpoint_id)
                .await?
                .with_context(|| format!("No delivery row for {} {subject_id}", event.as_api())),
        }
    }

    /// Most recent delivery row for a (subject, event, endpoint).
    pub async fn find_subject_webhook_delivery(
        &self,
        event: WebhookEventType,
        subject_id: &str,
        endpoint_id: &str,
    ) -> Result<Option<WebhookDeliveryRow>> {
        let column = subject_column(event)?;
        let row = sqlx::query_as::<_, WebhookDeliveryRow>(&format!(
            r#"
            SELECT {WEBHOOK_DELIVERY_COLUMNS} FROM webhook_deliveries
            WHERE "{column}" = $1 AND "eventType" = $2 AND "webhookEndpointId" = $3
            ORDER BY "createdAt" DESC LIMIT 1
            "#
        ))
        .bind(subject_id)
        .bind(event.as_db())
        .bind(endpoint_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    pub async fn find_webhook_delivery_by_id(
        &self,
        id: &str,
    ) -> Result<Option<WebhookDeliveryRow>> {
        let row = sqlx::query_as::<_, WebhookDeliveryRow>(&format!(
            "SELECT {WEBHOOK_DELIVERY_COLUMNS} FROM webhook_deliveries WHERE id = $1"
        ))
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    /// Reset one delivery row to PENDING / 0 attempts for a manual replay.
    pub async fn reset_webhook_delivery_by_id(&self, id: &str) -> Result<Option<WebhookDeliveryRow>> {
        let row = sqlx::query_as::<_, WebhookDeliveryRow>(&format!(
            r#"
            UPDATE webhook_deliveries
            SET status = 'PENDING'::"WebhookDeliveryStatus",
                attempts = 0,
                "lastResponse" = NULL,
                "lastError" = NULL,
                "updatedAt" = NOW()
            WHERE id = $1
            RETURNING {WEBHOOK_DELIVERY_COLUMNS}
            "#
        ))
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    /// Most recent delivery row of a deposit's `event` (PAID, LATE) towards
    /// an endpoint. The newest row is the authoritative one; legacy rows
    /// (NULL endpoint) are excluded.
    pub async fn find_deposit_webhook_delivery(
        &self,
        event: WebhookEventType,
        deposit_id: &str,
        endpoint_id: &str,
    ) -> Result<Option<WebhookDeliveryRow>> {
        let row = sqlx::query_as::<_, WebhookDeliveryRow>(&format!(
            r#"SELECT {WEBHOOK_DELIVERY_COLUMNS} FROM webhook_deliveries
               WHERE "depositId" = $1 AND "eventType" = $2 AND "webhookEndpointId" = $3
               ORDER BY "createdAt" DESC LIMIT 1"#
        ))
        .bind(deposit_id)
        .bind(event.as_db())
        .bind(endpoint_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    /// All delivery rows for a set of deposits, newest first. Callers keep the
    /// first row seen per deposit to get the authoritative one.
    pub async fn find_webhook_deliveries_for_deposits(
        &self,
        deposit_ids: &[String],
    ) -> Result<Vec<WebhookDeliveryRow>> {
        if deposit_ids.is_empty() {
            return Ok(Vec::new());
        }
        let rows = sqlx::query_as::<_, WebhookDeliveryRow>(&format!(
            r#"SELECT {WEBHOOK_DELIVERY_COLUMNS} FROM webhook_deliveries WHERE "depositId" = ANY($1) ORDER BY "createdAt" DESC"#
        ))
        .bind(deposit_ids)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// Reset the most recent `event` delivery row of a (deposit, endpoint)
    /// pair to PENDING / 0 attempts (creating it if none exists) so a manual
    /// retrigger restarts a full retry cycle. Older rows are left untouched.
    pub async fn reset_deposit_webhook_delivery(
        &self,
        event: WebhookEventType,
        deposit_id: &str,
        endpoint_id: &str,
    ) -> Result<WebhookDeliveryRow> {
        if let Some(existing) = self.find_deposit_webhook_delivery(event, deposit_id, endpoint_id).await? {
            let row = sqlx::query_as::<_, WebhookDeliveryRow>(&format!(
                r#"
                UPDATE webhook_deliveries
                SET status = 'PENDING'::"WebhookDeliveryStatus",
                    attempts = 0,
                    "lastResponse" = NULL,
                    "lastError" = NULL,
                    "updatedAt" = NOW()
                WHERE id = $1
                RETURNING {WEBHOOK_DELIVERY_COLUMNS}
                "#
            ))
            .bind(&existing.id)
            .fetch_one(&self.pool)
            .await?;
            return Ok(row);
        }

        self.insert_deposit_webhook_delivery(event, deposit_id, endpoint_id).await
    }

    /// Authoritative deliveries of `deposit_id` (PAID or LATE, newest per
    /// endpoint) that ended FAILED — what a merchant may re-queue.
    pub async fn failed_deposit_deliveries(&self, deposit_id: &str) -> Result<Vec<WebhookDeliveryRow>> {
        let rows = sqlx::query_as::<_, WebhookDeliveryRow>(&format!(
            r#"
            SELECT * FROM (
                SELECT DISTINCT ON ("depositAddressId", "lightningPaymentId", "withdrawalId", "alertId", "transferId", "eventType", "depositId", "webhookEndpointId") {WEBHOOK_DELIVERY_COLUMNS}
                FROM webhook_deliveries
                WHERE "depositId" = $1 AND "webhookEndpointId" IS NOT NULL
                ORDER BY "depositAddressId", "lightningPaymentId", "withdrawalId", "alertId", "transferId", "eventType", "depositId", "webhookEndpointId", "createdAt" DESC
            ) t
            WHERE t.status = 'FAILED'
            "#
        ))
        .bind(deposit_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// Close a PENDING delivery that will never be attempted (endpoint
    /// deactivated, unsubscribed or deleted, subject gone) so it does not
    /// stay PENDING forever; a manual retry can still reopen it.
    pub async fn fail_pending_webhook_delivery(&self, id: &str, error: &str) -> Result<()> {
        sqlx::query(
            r#"
            UPDATE webhook_deliveries
            SET status = 'FAILED'::"WebhookDeliveryStatus", "lastError" = $2, "updatedAt" = NOW()
            WHERE id = $1 AND status = 'PENDING'::"WebhookDeliveryStatus"
            "#,
        )
        .bind(id)
        .bind(error)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn update_webhook_delivery(
        &self,
        id: &str,
        status: WebhookDeliveryStatus,
        attempts: i32,
        last_response: Option<i32>,
        last_error: Option<String>,
        last_payload: Option<serde_json::Value>,
    ) -> Result<()> {
        sqlx::query(
            r#"
            UPDATE webhook_deliveries
            SET status = $2::"WebhookDeliveryStatus",
                attempts = $3,
                "lastResponse" = $4,
                "lastError" = $5,
                "lastPayload" = COALESCE($6, "lastPayload"),
                "updatedAt" = NOW()
            WHERE id = $1
            "#,
        )
        .bind(id)
        .bind(status.as_db())
        .bind(attempts)
        .bind(last_response)
        .bind(last_error)
        .bind(last_payload)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

impl Db {
    /// One page of the PENDING deliveries towards an endpoint (legacy rows
    /// without one are left alone) touched in the last `max_age_secs`, by
    /// id after `after_id` — what the reconciliation walks to queue again
    /// the rows whose queue member is missing.
    pub async fn pending_deliveries_page(
        &self,
        after_id: Option<&str>,
        max_age_secs: i64,
        limit: i64,
    ) -> Result<Vec<PendingDeliveryRow>> {
        let rows = sqlx::query_as::<_, PendingDeliveryRow>(
            r#"
            SELECT id, "eventType", "depositId", "depositAddressId", "lightningPaymentId", "withdrawalId",
                   "alertId", "transferId", "webhookEndpointId", attempts,
                   "updatedAt" AT TIME ZONE 'UTC' AS "updatedAt"
            FROM webhook_deliveries
            WHERE status = 'PENDING'::"WebhookDeliveryStatus" AND "webhookEndpointId" IS NOT NULL
              AND "updatedAt" > (NOW() AT TIME ZONE 'UTC') - make_interval(secs => $2)
              AND ($1::text IS NULL OR id > $1)
            ORDER BY id
            LIMIT $3
            "#,
        )
        .bind(after_id)
        .bind(max_age_secs as f64)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }
}

/// The `webhook_deliveries` column holding the subject id of `event`'s
/// deliveries (static names — never user input in the SQL).
fn subject_column(event: WebhookEventType) -> Result<&'static str> {
    Ok(match event.subject() {
        WebhookSubject::DepositAddress => "depositAddressId",
        WebhookSubject::LightningPayment => "lightningPaymentId",
        WebhookSubject::Withdrawal => "withdrawalId",
        WebhookSubject::Alert => "alertId",
        WebhookSubject::Transfer => "transferId",
        WebhookSubject::Deposit => anyhow::bail!("PAID and LATE deliveries are keyed on their deposit"),
    })
}

/// Deposit deliveries are PAID and LATE only.
fn deposit_subject(event: WebhookEventType) -> Result<()> {
    if event.subject() != WebhookSubject::Deposit {
        anyhow::bail!("{} deliveries are not keyed on a deposit", event.as_api());
    }
    Ok(())
}
