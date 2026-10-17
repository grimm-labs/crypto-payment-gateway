//! The project ledger (docs/projects-spec.md §7.2): the components of every
//! project balance, per (project, network, token), in one round trip —
//! `UNION ALL` of grouped aggregates over deposits, sweeps, the Bitcoin
//! scan, withdrawals and internal transfers. Takes a connection, so the
//! withdrawal admission runs it inside its locked transaction.

use anyhow::Result;
use domain::{BalanceComponents, Network, Token};
use sqlx::PgConnection;

use crate::{Db, RawAmount};

/// Which rows the components cover.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LedgerFilter {
    /// Every project, every pair.
    All,
    /// One project, every pair.
    Project(String),
    /// One project, one pair (the admission check).
    ProjectPair {
        project_id: String,
        network: Network,
        token: Token,
    },
    /// Every project, one pair (the reconciliation).
    Pair { network: Network, token: Token },
}

impl LedgerFilter {
    fn binds(&self) -> (Option<&str>, Option<&'static str>, Option<&'static str>) {
        match self {
            Self::All => (None, None, None),
            Self::Project(id) => (Some(id), None, None),
            Self::ProjectPair { project_id, network, token } => {
                (Some(project_id), Some(network.as_db()), Some(token.as_db()))
            }
            Self::Pair { network, token } => (None, Some(network.as_db()), Some(token.as_db())),
        }
    }
}

/// The components of one (project, network, token).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectComponentsRow {
    pub project_id: String,
    pub network: Network,
    pub token: Token,
    pub components: BalanceComponents,
}

/// One row per (project, network, token) with activity. Each component is
/// a separate grouped aggregate, summed in the outer query; every address
/// and withdrawal carries the project of the row it belongs to.
const COMPONENTS_SQL: &str = r#"
WITH parts AS (
    -- Confirmed deposits, and the detected ones (informational).
    SELECT a."projectId" AS project_id, a.network, a.token,
           COALESCE(SUM(d."amountRaw"::numeric) FILTER (WHERE d.status = 'CONFIRMED'::"DepositStatus"), 0) AS deposits,
           0::numeric AS unattributed, 0::numeric AS withdrawn, 0::numeric AS fees,
           0::numeric AS transfers_in, 0::numeric AS transfers_out, 0::numeric AS reserved,
           COALESCE(SUM(d."amountRaw"::numeric) FILTER (WHERE d.status = 'DETECTED'::"DepositStatus"), 0) AS pending
    FROM deposits d
    JOIN deposit_addresses a ON a.id = d."depositAddressId"
    WHERE a."projectId" IS NOT NULL
    GROUP BY 1, 2, 3
  UNION ALL
    -- Unattributed funds swept from the project's addresses.
    SELECT a."projectId", s.network, s.token,
           0, COALESCE(SUM(s."unattributedRaw"), 0), 0, 0, 0, 0, 0, 0
    FROM sweeps s
    JOIN deposit_addresses a ON a.id = s."depositAddressId"
    WHERE a."projectId" IS NOT NULL AND s.status = 'CONFIRMED'::"SweepStatus"
    GROUP BY 1, 2, 3
  UNION ALL
    -- Bitcoin: the scan surplus of each address, floored at 0.
    SELECT a."projectId", a.network, a.token,
           0, COALESCE(SUM(GREATEST(a."btcReceivedRaw" - COALESCE((
                 SELECT SUM(d."amountRaw"::numeric) FROM deposits d
                 WHERE d."depositAddressId" = a.id AND d.status = 'CONFIRMED'::"DepositStatus"
               ), 0), 0)), 0), 0, 0, 0, 0, 0, 0
    FROM deposit_addresses a
    WHERE a."projectId" IS NOT NULL AND a.network = 'BITCOIN'::"Network" AND a."btcReceivedRaw" IS NOT NULL
    GROUP BY 1, 2, 3
  UNION ALL
    -- Withdrawals: what left, the fees spent in the same asset, and what
    -- the open ones reserve (amount + fee cap, or estimate, in the same asset).
    SELECT w."projectId", w.network, w.token,
           0, 0,
           COALESCE(SUM(w."amountRaw") FILTER (WHERE w.status = 'CONFIRMED'::"WithdrawalStatus"), 0),
           COALESCE(SUM(COALESCE(w."feeRaw", 0)) FILTER (
               WHERE w.status IN ('CONFIRMED'::"WithdrawalStatus", 'FAILED'::"WithdrawalStatus") AND w.token = w."feeToken"), 0),
           0, 0,
           COALESCE(SUM(w."amountRaw" + CASE WHEN w.token = w."feeToken" THEN COALESCE(w."feeCapRaw", w."feeEstimateRaw", 0) ELSE 0 END)
               FILTER (WHERE w.status IN ('PENDING_APPROVAL'::"WithdrawalStatus", 'QUEUED'::"WithdrawalStatus", 'SIGNED'::"WithdrawalStatus", 'BROADCAST'::"WithdrawalStatus", 'STUCK'::"WithdrawalStatus")), 0),
           0
    FROM withdrawals w
    WHERE w."projectId" IS NOT NULL
    GROUP BY 1, 2, 3
  UNION ALL
    SELECT t."toProjectId", t.network, t.token, 0, 0, 0, 0, COALESCE(SUM(t."amountRaw"), 0), 0, 0, 0
    FROM internal_transfers t WHERE t."toProjectId" IS NOT NULL GROUP BY 1, 2, 3
  UNION ALL
    SELECT t."fromProjectId", t.network, t.token, 0, 0, 0, 0, 0, COALESCE(SUM(t."amountRaw"), 0), 0, 0
    FROM internal_transfers t WHERE t."fromProjectId" IS NOT NULL GROUP BY 1, 2, 3
)
SELECT project_id, network::text, token::text,
       SUM(deposits)::text, SUM(unattributed)::text, SUM(withdrawn)::text, SUM(fees)::text,
       SUM(transfers_in)::text, SUM(transfers_out)::text, SUM(reserved)::text, SUM(pending)::text
FROM parts
WHERE ($1::text IS NULL OR project_id = $1)
  AND ($2::text IS NULL OR network = $2::"Network")
  AND ($3::text IS NULL OR token = $3::"Token")
GROUP BY project_id, network, token
ORDER BY project_id, network, token
"#;

type ComponentsTuple = (
    String,
    String,
    String,
    RawAmount,
    RawAmount,
    RawAmount,
    RawAmount,
    RawAmount,
    RawAmount,
    RawAmount,
    RawAmount,
);

/// The components of every (project, network, token) matching `filter`,
/// on `conn` (the admission transaction, or any connection).
pub async fn project_components(conn: &mut PgConnection, filter: &LedgerFilter) -> Result<Vec<ProjectComponentsRow>> {
    let (project_id, network, token) = filter.binds();
    let rows: Vec<ComponentsTuple> = sqlx::query_as(COMPONENTS_SQL)
        .bind(project_id)
        .bind(network)
        .bind(token)
        .fetch_all(&mut *conn)
        .await?;
    rows.into_iter()
        .map(|(project_id, network, token, deposits, unattributed, withdrawn, fees, transfers_in, transfers_out, reserved, pending)| {
            Ok(ProjectComponentsRow {
                project_id,
                network: Network::from_db(&network)?,
                token: Token::from_db(&token)?,
                components: BalanceComponents {
                    deposits: deposits.0,
                    unattributed: unattributed.0,
                    withdrawn: withdrawn.0,
                    fees: fees.0,
                    transfers_in: transfers_in.0,
                    transfers_out: transfers_out.0,
                    reserved: reserved.0,
                    pending: pending.0,
                },
            })
        })
        .collect()
}

impl Db {
    /// [`project_components`] on a pool connection (dashboard reads).
    pub async fn project_components(&self, filter: &LedgerFilter) -> Result<Vec<ProjectComponentsRow>> {
        let mut conn = self.pool.acquire().await?;
        project_components(&mut conn, filter).await
    }

    /// Raise a Bitcoin address's received watermark (never lowered: a
    /// smaller reading from a lagging provider is ignored).
    pub async fn raise_btc_received(&self, deposit_address_id: &str, received: u128) -> Result<()> {
        sqlx::query(
            r#"UPDATE deposit_addresses
               SET "btcReceivedRaw" = GREATEST(COALESCE("btcReceivedRaw", 0), $2::numeric)
               WHERE id = $1 AND network = 'BITCOIN'::"Network""#,
        )
        .bind(deposit_address_id)
        .bind(RawAmount(received))
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}
