//! Project balances (docs/projects-spec.md §7): derived from source rows on
//! every read (invariant 4), never stored. The admission check reads them on
//! its locked transaction and never through the cache; dashboard reads may
//! share a 30 s Redis cache (`ledger:projects`), invalidated on every
//! withdrawal status change and every internal transfer.

use anyhow::Result;
use config::LEDGER_CACHE_SECS;
use db::LedgerFilter;
use domain::{BalanceComponents, Network, ProjectBalance, Token};
use redis::AsyncCommands;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::PgConnection;
use tracing::warn;

use crate::{treasury, AppState};

const LEDGER_CACHE_KEY: &str = "ledger:projects";

/// What the gateway really holds of one payment pair (spec §10.4): the
/// confirmed treasury balance plus the unswept balance of the deposit
/// addresses (Bitcoin: the spendable UTXOs of both, Lightning: the SDK
/// balance). `None` = unknown: network or Lightning disabled, provider
/// error, or sweeping disabled for the pair (the unswept balance is not
/// scanned then).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Holdings {
    pub amount: Option<u128>,
}

/// The reconciliation of one pair: holdings against the project balances.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MerchantFunds {
    pub holdings: Option<u128>,
    /// Σ project balances (signed).
    pub project_total: i128,
    /// holdings − project_total; `None` when the holdings are unknown.
    pub merchant_funds: Option<i128>,
}

impl MerchantFunds {
    /// `ok`, `shortfall` or `unknown` (spec §10.4).
    pub fn status(&self) -> &'static str {
        match self.merchant_funds {
            Some(funds) if funds >= 0 => "ok",
            Some(_) => "shortfall",
            None => "unknown",
        }
    }
}

/// The holdings of `(network, token)`, from the treasury's 30 s balance
/// cache plus the sweeper's last deposit scan.
pub async fn holdings(state: &AppState, network: Network, token: Token) -> Holdings {
    if !state.is_network_enabled(network) {
        return Holdings { amount: None };
    }
    let confirmed = match treasury::confirmed_balance(state, network, token, false).await {
        Ok(value) => value,
        Err(err) => {
            warn!("Holdings of {} {} unreadable: {err:#}", network.as_api(), token.as_api());
            return Holdings { amount: None };
        }
    };
    // Bitcoin's confirmed balance already spans the deposit UTXOs; Lightning
    // has no deposit addresses.
    if matches!(network, Network::Bitcoin | Network::Lightning) {
        return Holdings { amount: Some(confirmed) };
    }
    let sweeping = match state.db.sweep_setting(network, token).await {
        Ok(Some(setting)) => setting.enabled,
        Ok(None) => false,
        Err(err) => {
            warn!("Sweep setting of {} {} unreadable: {err:#}", network.as_api(), token.as_api());
            return Holdings { amount: None };
        }
    };
    if !sweeping {
        return Holdings { amount: None };
    }
    let unswept: u128 = match treasury::scanned_deposit_balances(state, network).await {
        Ok(scanned) => scanned
            .iter()
            .filter(|s| s.token == token.as_db())
            .filter_map(|s| s.amount_raw.parse::<u128>().ok())
            .fold(0, u128::saturating_add),
        Err(err) => {
            warn!("Deposit scan of {} unreadable: {err:#}", network.as_api());
            return Holdings { amount: None };
        }
    };
    Holdings { amount: Some(confirmed.saturating_add(unswept)) }
}

/// Merchant funds of one pair on `conn` (the transfer admission reads the
/// project balances inside its lock).
pub async fn merchant_funds(
    state: &AppState,
    conn: &mut PgConnection,
    network: Network,
    token: Token,
) -> Result<MerchantFunds> {
    let rows = db::ledger::project_components(conn, &LedgerFilter::Pair { network, token }).await?;
    let project_total = rows
        .iter()
        .map(|row| ProjectBalance::from_components(row.components).balance)
        .fold(0i128, i128::saturating_add);
    let held = holdings(state, network, token).await;
    Ok(reconcile(held, project_total))
}

/// The pure reconciliation: holdings (unknown or not) against the project
/// total.
pub fn reconcile(holdings: Holdings, project_total: i128) -> MerchantFunds {
    MerchantFunds {
        holdings: holdings.amount,
        project_total,
        merchant_funds: holdings
            .amount
            .map(|held| i128::try_from(held).unwrap_or(i128::MAX).saturating_sub(project_total)),
    }
}

/// One project's balance of one asset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectAssetBalance {
    pub project_id: String,
    pub network: Network,
    pub token: Token,
    pub balance: ProjectBalance,
}

/// The payment pairs a project can hold (spec Q3: never TRX/SOL).
pub fn payment_pairs() -> Vec<(Network, Token)> {
    Network::ALL
        .iter()
        .flat_map(|network| network.payment_tokens().iter().map(move |token| (*network, *token)))
        .collect()
}

/// Every project balance matching `filter`, with a zero entry for each
/// payment pair without activity when the filter names a project.
/// `cached`: dashboard reads (30 s); the admission check never passes it.
pub async fn project_balances(
    state: &AppState,
    filter: &LedgerFilter,
    cached: bool,
) -> Result<Vec<ProjectAssetBalance>> {
    let rows = if cached {
        cached_components(state).await?
            .into_iter()
            .filter(|row| matches_filter(filter, &row.project_id, row.network, row.token))
            .collect()
    } else {
        state.db.project_components(filter).await?
    };
    let mut balances: Vec<ProjectAssetBalance> = rows
        .into_iter()
        .map(|row| ProjectAssetBalance {
            project_id: row.project_id,
            network: row.network,
            token: row.token,
            balance: ProjectBalance::from_components(row.components),
        })
        .collect();
    if let LedgerFilter::Project(project_id) = filter {
        for (network, token) in payment_pairs() {
            if !balances.iter().any(|b| b.network == network && b.token == token) {
                balances.push(ProjectAssetBalance {
                    project_id: project_id.clone(),
                    network,
                    token,
                    balance: ProjectBalance::from_components(BalanceComponents::default()),
                });
            }
        }
        balances.sort_by_key(|b| pair_rank(b.network, b.token));
    }
    Ok(balances)
}

fn pair_rank(network: Network, token: Token) -> usize {
    payment_pairs()
        .iter()
        .position(|(n, t)| *n == network && *t == token)
        .unwrap_or(usize::MAX)
}

fn matches_filter(filter: &LedgerFilter, project_id: &str, network: Network, token: Token) -> bool {
    match filter {
        LedgerFilter::All => true,
        LedgerFilter::Project(id) => id == project_id,
        LedgerFilter::ProjectPair { project_id: id, network: n, token: t } => {
            id == project_id && *n == network && *t == token
        }
        LedgerFilter::Pair { network: n, token: t } => *n == network && *t == token,
    }
}

/// The balance of one (project, network, token) on `conn` — the admission
/// and approval checks (spec §8.3), inside the per-asset lock, never cached.
pub async fn project_balance_on(
    conn: &mut PgConnection,
    project_id: &str,
    network: Network,
    token: Token,
) -> Result<ProjectBalance> {
    let filter = LedgerFilter::ProjectPair {
        project_id: project_id.to_string(),
        network,
        token,
    };
    let components = db::ledger::project_components(conn, &filter)
        .await?
        .into_iter()
        .next()
        .map(|row| row.components)
        .unwrap_or_default();
    Ok(ProjectBalance::from_components(components))
}

/// The balance of one (project, network, token) on a pool connection,
/// never cached (the quote).
pub async fn project_balance(
    state: &AppState,
    project_id: &str,
    network: Network,
    token: Token,
) -> Result<ProjectBalance> {
    let filter = LedgerFilter::ProjectPair {
        project_id: project_id.to_string(),
        network,
        token,
    };
    let components = state
        .db
        .project_components(&filter)
        .await?
        .into_iter()
        .next()
        .map(|row| row.components)
        .unwrap_or_default();
    Ok(ProjectBalance::from_components(components))
}

/// The whole-instance component rows from the 30 s cache (filled on a miss).
#[derive(Debug, Serialize, Deserialize)]
struct CachedRow {
    project_id: String,
    network: String,
    token: String,
    // Base units as strings: 128-bit integers do not fit JSON numbers.
    deposits: String,
    unattributed: String,
    withdrawn: String,
    fees: String,
    transfers_in: String,
    transfers_out: String,
    reserved: String,
    pending: String,
}

async fn cached_components(state: &AppState) -> Result<Vec<db::ProjectComponentsRow>> {
    let key = state.config.redis_key(LEDGER_CACHE_KEY);
    let mut redis = state.redis.clone();
    let cached: Option<String> = redis.get(&key).await.unwrap_or_else(|err| {
        warn!("ledger cache read failed: {err}");
        None
    });
    if let Some(cached) = cached {
        if let Ok(rows) = serde_json::from_str::<Vec<CachedRow>>(&cached) {
            let parsed: Option<Vec<db::ProjectComponentsRow>> = rows.into_iter().map(from_cached).collect();
            if let Some(parsed) = parsed {
                return Ok(parsed);
            }
        }
    }
    let rows = state.db.project_components(&LedgerFilter::All).await?;
    let serialized = serde_json::to_string(&rows.iter().map(to_cached).collect::<Vec<_>>())?;
    if let Err(err) = redis.set_ex::<_, _, ()>(&key, serialized, LEDGER_CACHE_SECS).await {
        warn!("ledger cache write failed: {err}");
    }
    Ok(rows)
}

fn to_cached(row: &db::ProjectComponentsRow) -> CachedRow {
    let c = row.components;
    CachedRow {
        project_id: row.project_id.clone(),
        network: row.network.as_db().into(),
        token: row.token.as_db().into(),
        deposits: c.deposits.to_string(),
        unattributed: c.unattributed.to_string(),
        withdrawn: c.withdrawn.to_string(),
        fees: c.fees.to_string(),
        transfers_in: c.transfers_in.to_string(),
        transfers_out: c.transfers_out.to_string(),
        reserved: c.reserved.to_string(),
        pending: c.pending.to_string(),
    }
}

fn from_cached(row: CachedRow) -> Option<db::ProjectComponentsRow> {
    let parse = |value: &str| value.parse::<u128>().ok();
    Some(db::ProjectComponentsRow {
        project_id: row.project_id,
        network: Network::from_db(&row.network).ok()?,
        token: Token::from_db(&row.token).ok()?,
        components: BalanceComponents {
            deposits: parse(&row.deposits)?,
            unattributed: parse(&row.unattributed)?,
            withdrawn: parse(&row.withdrawn)?,
            fees: parse(&row.fees)?,
            transfers_in: parse(&row.transfers_in)?,
            transfers_out: parse(&row.transfers_out)?,
            reserved: parse(&row.reserved)?,
            pending: parse(&row.pending)?,
        },
    })
}

/// Drop the dashboard cache: called on every withdrawal status change and
/// every internal transfer. Never fails the caller.
pub async fn invalidate_cache(state: &AppState) {
    let key = state.config.redis_key(LEDGER_CACHE_KEY);
    let mut redis = state.redis.clone();
    if let Err(err) = redis.del::<_, ()>(&key).await {
        warn!("ledger cache invalidation failed: {err}");
    }
}

/// The API shape of one balance (spec §11): every figure in pairs
/// (`balance` / `balance_raw`, …), signed where it can be negative.
pub fn balance_json(entry: &ProjectAssetBalance) -> Value {
    let token = entry.token;
    let b = entry.balance;
    let c = b.components;
    let unsigned = |raw: u128| json!(domain::raw_u128_to_string(raw, token));
    let signed = |raw: i128| json!(domain::signed_raw_to_string(raw, token));
    json!({
        "network": entry.network.as_api(),
        "token": token.as_api(),
        "deposits": unsigned(c.deposits), "deposits_raw": c.deposits.to_string(),
        "unattributed": unsigned(c.unattributed), "unattributed_raw": c.unattributed.to_string(),
        "withdrawn": unsigned(c.withdrawn), "withdrawn_raw": c.withdrawn.to_string(),
        "fees": unsigned(c.fees), "fees_raw": c.fees.to_string(),
        "transfers_in": unsigned(c.transfers_in), "transfers_in_raw": c.transfers_in.to_string(),
        "transfers_out": unsigned(c.transfers_out), "transfers_out_raw": c.transfers_out.to_string(),
        "balance": signed(b.balance), "balance_raw": b.balance.to_string(),
        "reserved": unsigned(b.reserved), "reserved_raw": b.reserved.to_string(),
        "available": signed(b.available), "available_raw": b.available.to_string(),
        "pending": unsigned(c.pending), "pending_raw": c.pending.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_payment_pair_once_and_no_gas_asset() {
        let pairs = payment_pairs();
        assert_eq!(pairs.len(), 6);
        assert!(pairs.contains(&(Network::Ethereum, Token::Eth)));
        assert!(pairs.contains(&(Network::Lightning, Token::Btc)));
        assert!(!pairs.iter().any(|(_, t)| t.is_gas_only()));
    }

    #[test]
    fn cached_rows_roundtrip_through_strings() {
        let row = db::ProjectComponentsRow {
            project_id: "p1".into(),
            network: Network::Ethereum,
            token: Token::Eth,
            components: BalanceComponents {
                deposits: u128::MAX,
                unattributed: 1,
                withdrawn: 2,
                fees: 3,
                transfers_in: 4,
                transfers_out: 5,
                reserved: 6,
                pending: 7,
            },
        };
        let json = serde_json::to_string(&[to_cached(&row)]).unwrap();
        let back: Vec<CachedRow> = serde_json::from_str(&json).unwrap();
        assert_eq!(from_cached(back.into_iter().next().unwrap()).unwrap(), row);
    }

    #[test]
    fn reconciliation_status_follows_the_sign_of_the_merchant_funds() {
        let ok = reconcile(Holdings { amount: Some(1_000) }, 400);
        assert_eq!(ok.merchant_funds, Some(600));
        assert_eq!(ok.status(), "ok");
        let exact = reconcile(Holdings { amount: Some(1_000) }, 1_000);
        assert_eq!(exact.status(), "ok");
        let short = reconcile(Holdings { amount: Some(1_000) }, 1_001);
        assert_eq!(short.merchant_funds, Some(-1));
        assert_eq!(short.status(), "shortfall");
        // Negative project balances raise the merchant funds.
        assert_eq!(reconcile(Holdings { amount: Some(100) }, -50).merchant_funds, Some(150));
        let unknown = reconcile(Holdings { amount: None }, 5);
        assert_eq!(unknown.merchant_funds, None);
        assert_eq!(unknown.status(), "unknown");
    }

    #[test]
    fn balance_json_renders_signed_amounts_at_token_scale() {
        let entry = ProjectAssetBalance {
            project_id: "p1".into(),
            network: Network::Tron,
            token: Token::Usdt,
            balance: ProjectBalance::from_components(BalanceComponents {
                deposits: 1_000_000,
                withdrawn: 2_500_000,
                reserved: 100_000,
                ..Default::default()
            }),
        };
        let json = balance_json(&entry);
        assert_eq!(json["balance"], "-1.5");
        assert_eq!(json["balance_raw"], "-1500000");
        assert_eq!(json["available"], "-1.6");
        assert_eq!(json["reserved"], "0.1");
        assert_eq!(json["deposits_raw"], "1000000");
        assert_eq!(json["network"], "tron");
    }
}
