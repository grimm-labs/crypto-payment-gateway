//! Treasury balances and availability (spec §7.5, §7.10): what each
//! treasury holds at the confirmation depth, what withdrawals reserve, the
//! gas reserve and hot-wallet cap alerts, and the unswept / unattributed
//! figures of the dashboard.

use std::collections::HashMap;
use std::time::Duration;

use anyhow::{Context, Result};
use chain::btc_tx::{EsploraRpc, SpendableUtxo};
use chain::evm::{BlockTag, EvmRpc};
use chain::solana_tx::SolanaRpc;
use chain::tron_tx::TronRpc;
use config::{TREASURY_BALANCE_CACHE_SECS, TREASURY_MONITOR_INTERVAL_SECS};
use domain::{AlertType, Network, Token};
use redis::AsyncCommands;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tracing::{error, warn};
use wallet::KeyRef;

use crate::withdrawals::rules::Balance;
use crate::{alerts, AppState};

/// Networks with an on-chain treasury the gateway signs from.
pub const TREASURY_NETWORKS: [Network; 4] =
    [Network::Ethereum, Network::Tron, Network::Solana, Network::Bitcoin];

/// The treasury address of `network` (computed from the seed, never read
/// from storage — invariant 2). `None` for Lightning (the Spark wallet).
pub fn treasury_address(state: &AppState, network: Network) -> Result<Option<String>> {
    if network == Network::Lightning {
        return Ok(None);
    }
    Ok(Some(state.wallet.treasury_address(network)?))
}

/// Where USDT sits for the treasury: the address itself, except on
/// Solana where it is the owner's associated token account.
pub fn treasury_token_holder(state: &AppState, network: Network, token: Token) -> Result<String> {
    if network == Network::Solana && token == Token::Usdt {
        return Ok(state
            .wallet
            .solana_usdt_token_account(KeyRef::Treasury(Network::Solana))?);
    }
    treasury_address(state, network)?.context("no treasury address")
}

/// One deposit address's balance, as last scanned by the sweeper (the
/// unswept figure, and the Bitcoin UTXO candidates).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ScannedDepositBalance {
    pub address: String,
    pub derivation_index: i32,
    pub token: String,
    pub amount_raw: String,
}

pub(crate) fn deposit_balances_key(state: &AppState, network: Network) -> String {
    state.config.redis_key(&format!("deposit-balances:{}", network.as_db()))
}

/// Record (or clear, at 0) a scanned deposit balance.
pub async fn record_deposit_balance(
    state: &AppState,
    network: Network,
    deposit_address_id: &str,
    scanned: &ScannedDepositBalance,
) -> Result<()> {
    let key = deposit_balances_key(state, network);
    let field = format!("{deposit_address_id}:{}", scanned.token);
    let mut redis = state.redis.clone();
    if scanned.amount_raw == "0" {
        let _: () = redis.hdel(&key, field).await?;
    } else {
        let _: () = redis.hset(&key, field, serde_json::to_string(scanned)?).await?;
    }
    Ok(())
}

/// Every non-zero deposit balance last scanned on `network`.
pub async fn scanned_deposit_balances(state: &AppState, network: Network) -> Result<Vec<ScannedDepositBalance>> {
    let mut redis = state.redis.clone();
    let entries: HashMap<String, String> = redis.hgetall(deposit_balances_key(state, network)).await?;
    Ok(entries
        .into_values()
        .filter_map(|value| serde_json::from_str(&value).ok())
        .collect())
}

/// Confirmed UTXOs the gateway may spend: the treasury's and those of final
/// deposit addresses (as last scanned), at `BTC_CONFIRMATIONS`, minus the
/// ones an unconfirmed gateway transaction already spends.
pub async fn spendable_btc_utxos(state: &AppState, esplora: &dyn EsploraRpc) -> Result<Vec<SpendableUtxo>> {
    let tip = esplora.tip_height().await?;
    let required = u64::try_from(state.config.btc_confirmations).unwrap_or(3);
    let reserved = state.db.reserved_utxos().await?;
    let mut owners: Vec<(String, KeyRef)> = vec![(
        state.wallet.treasury_address(Network::Bitcoin)?,
        KeyRef::Treasury(Network::Bitcoin),
    )];
    for scanned in scanned_deposit_balances(state, Network::Bitcoin).await? {
        let index = u32::try_from(scanned.derivation_index).context("negative derivation index")?;
        owners.push((scanned.address, KeyRef::Deposit { network: Network::Bitcoin, index }));
    }
    let mut spendable = Vec::new();
    for (address, key) in owners {
        for utxo in esplora.utxos(&address).await? {
            let Some(height) = utxo.status.block_height.filter(|_| utxo.status.confirmed) else {
                continue;
            };
            if chain::confirmations_between(tip, height) < required as i32 {
                continue;
            }
            if reserved.iter().any(|(txid, vout)| *txid == utxo.txid && *vout == utxo.vout) {
                continue;
            }
            spendable.push(SpendableUtxo {
                txid: utxo.txid,
                vout: utxo.vout,
                value: utxo.value,
                address: address.clone(),
                key,
                height,
            });
        }
    }
    Ok(spendable)
}

/// The treasury's confirmed balance of `token` on `network`, read at the
/// confirmation depth: Ethereum at `tip − (confirmations − 1)`, Tron on the
/// solidity node, Solana `finalized`, Bitcoin the spendable UTXOs,
/// Lightning the SDK balance.
pub async fn read_confirmed_balance(state: &AppState, network: Network, token: Token) -> Result<u128> {
    match (network, token) {
        (Network::Ethereum, _) => {
            let rpc: &dyn EvmRpc = &state.chain;
            let tip = rpc.block_number().await?;
            let depth = BlockTag::Number(tip.saturating_sub(u64::try_from(state.config.eth_confirmations - 1).unwrap_or(11)));
            let treasury = treasury_token_holder(state, network, token)?;
            match token {
                Token::Eth => rpc.eth_balance(&treasury, depth).await,
                Token::Usdt => rpc.erc20_balance(&state.config.chain.usdt_eth, &treasury, depth).await,
                _ => anyhow::bail!("{} is not held on ethereum", token.as_api()),
            }
        }
        (Network::Tron, _) => {
            let rpc: &dyn TronRpc = &state.chain;
            let treasury = treasury_token_holder(state, network, token)?;
            match token {
                Token::Trx => rpc.solidity_trx_balance(&treasury).await,
                Token::Usdt => rpc.solidity_trc20_balance(&state.config.chain.usdt_tron, &treasury).await,
                _ => anyhow::bail!("{} is not held on tron", token.as_api()),
            }
        }
        (Network::Solana, _) => {
            let rpc: &dyn SolanaRpc = &state.chain;
            let holder = treasury_token_holder(state, network, token)?;
            match token {
                Token::Sol => rpc.lamports(&holder).await,
                Token::Usdt => rpc.token_balance(&holder).await,
                _ => anyhow::bail!("{} is not held on solana", token.as_api()),
            }
        }
        (Network::Bitcoin, Token::Btc) => Ok(spendable_btc_utxos(state, &state.chain)
            .await?
            .iter()
            .map(|u| u128::from(u.value))
            .sum()),
        (Network::Lightning, Token::Btc) => {
            let lightning = state.lightning.as_ref().context("Lightning is not configured")?;
            Ok(u128::from(lightning.balance_sats(false).await?))
        }
        _ => anyhow::bail!("{} is not held on {}", token.as_api(), network.as_api()),
    }
}

fn balance_cache_key(state: &AppState, network: Network, token: Token) -> String {
    state
        .config
        .redis_key(&format!("treasury-balance:{}:{}", network.as_db(), token.as_db()))
}

/// The confirmed balance from the 30 s cache, read from the chain when
/// absent or when `fresh`.
pub async fn confirmed_balance(state: &AppState, network: Network, token: Token, fresh: bool) -> Result<u128> {
    let key = balance_cache_key(state, network, token);
    let mut redis = state.redis.clone();
    if !fresh {
        let cached: Option<String> = redis.get(&key).await?;
        if let Some(value) = cached.and_then(|v| v.parse::<u128>().ok()) {
            return Ok(value);
        }
    }
    let value = read_confirmed_balance(state, network, token).await?;
    let _: () = redis
        .set_ex(&key, value.to_string(), TREASURY_BALANCE_CACHE_SECS)
        .await?;
    Ok(value)
}

/// Gas floor of `token` when it is `network`'s gas asset (0 otherwise).
pub async fn gas_floor(state: &AppState, network: Network, token: Token) -> Result<u128> {
    if network.gas_token() != Some(token) || network == Network::Bitcoin {
        return Ok(0);
    }
    Ok(state
        .db
        .asset_settings(token)
        .await?
        .gas_reserve_floor_raw
        .map(|r| r.0)
        .unwrap_or(0))
}

/// One row of the treasury view.
#[derive(Debug, Clone, Serialize)]
pub struct TreasuryRow {
    pub network: &'static str,
    pub token: &'static str,
    pub treasury_address: Option<String>,
    /// Solana USDT: the token account the treasury's USDT sits on.
    pub token_account: Option<String>,
    pub confirmed_raw: Option<String>,
    pub reserved_raw: String,
    pub reserved_fees_raw: String,
    pub available_raw: Option<String>,
    pub unswept_raw: String,
    pub unattributed_raw: String,
    pub gas_floor_raw: Option<String>,
    pub gas_alert_below_raw: Option<String>,
    pub gas_low: bool,
    pub hot_wallet_cap_raw: Option<String>,
    pub cap_exceeded: bool,
    pub available: bool,
    /// Why the balance could not be read (client-safe).
    pub message: Option<String>,
}

/// Every (network, token) the gateway can hold, with its treasury figures.
pub async fn treasury_view(state: &AppState) -> Result<Vec<TreasuryRow>> {
    let unattributed: HashMap<(String, String), u128> = state
        .db
        .unattributed_totals()
        .await?
        .into_iter()
        .map(|(network, token, raw)| ((network, token), raw.0))
        .collect();
    let mut rows = Vec::new();
    for network in Network::ALL {
        let enabled = state.is_network_enabled(network);
        let unswept = if enabled && network != Network::Lightning {
            scanned_deposit_balances(state, network).await.unwrap_or_default()
        } else {
            Vec::new()
        };
        for token in network.withdrawable_tokens() {
            let token = *token;
            let settings = state.db.asset_settings(token).await?;
            let reservations = state.db.withdrawal_reservations(network, token).await?;
            let is_gas = network.gas_token() == Some(token) && network != Network::Bitcoin;
            let floor = if is_gas { settings.gas_reserve_floor_raw.map(|r| r.0) } else { None };
            let alert_below = if is_gas { settings.gas_alert_below_raw.map(|r| r.0) } else { None };
            let (confirmed, message) = if enabled {
                match confirmed_balance(state, network, token, false).await {
                    Ok(value) => (Some(value), None),
                    Err(err) => {
                        warn!("Treasury balance of {} {} unreadable: {err:#}", network.as_api(), token.as_api());
                        (None, Some("Balance unavailable from the provider".to_string()))
                    }
                }
            } else {
                (None, Some(state.network_disabled_message(network).to_string()))
            };
            let balance = Balance {
                confirmed: confirmed.unwrap_or(0),
                reserved: reservations.amount,
                reserved_fees: reservations.fees,
                floor: floor.unwrap_or(0),
            };
            let unswept_raw: u128 = unswept
                .iter()
                .filter(|s| s.token == token.as_db())
                .filter_map(|s| s.amount_raw.parse::<u128>().ok())
                .fold(0, u128::saturating_add);
            let cap = settings.hot_wallet_cap_raw.map(|r| r.0);
            rows.push(TreasuryRow {
                network: network.as_api(),
                token: token.as_api(),
                treasury_address: treasury_address(state, network).ok().flatten(),
                token_account: (network == Network::Solana && token == Token::Usdt)
                    .then(|| treasury_token_holder(state, network, token).ok())
                    .flatten(),
                confirmed_raw: confirmed.map(|c| c.to_string()),
                reserved_raw: reservations.amount.to_string(),
                reserved_fees_raw: reservations.fees.to_string(),
                available_raw: confirmed.map(|_| balance.available().to_string()),
                unswept_raw: unswept_raw.to_string(),
                unattributed_raw: unattributed
                    .get(&(network.as_db().to_string(), token.as_db().to_string()))
                    .copied()
                    .unwrap_or(0)
                    .to_string(),
                gas_floor_raw: floor.map(|f| f.to_string()),
                gas_alert_below_raw: alert_below.map(|a| a.to_string()),
                gas_low: matches!((confirmed, alert_below), (Some(c), Some(a)) if c < a),
                hot_wallet_cap_raw: cap.map(|c| c.to_string()),
                cap_exceeded: matches!((confirmed, cap), (Some(c), Some(cap)) if c > cap),
                available: enabled && confirmed.is_some(),
                message,
            });
        }
    }
    Ok(rows)
}

/// Every 5 min: refresh the treasury balances and raise gas-reserve and
/// hot-wallet-cap alerts.
pub async fn treasury_monitor_loop(state: AppState) {
    tokio::time::sleep(Duration::from_secs(20)).await;
    let mut interval = tokio::time::interval(Duration::from_secs(TREASURY_MONITOR_INTERVAL_SECS));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        interval.tick().await;
        if let Err(err) = monitor_once(&state).await {
            error!("treasury monitor error: {err:#}");
        }
    }
}

async fn monitor_once(state: &AppState) -> Result<()> {
    // The project ledger against the holdings (spec §10.4): three
    // consecutive shortfalls raise an alert.
    if let Err(err) = crate::ledger::check_shortfalls(state).await {
        error!("ledger shortfall check error: {err:#}");
    }
    // Per asset: the treasuries of every network together (for BTC every
    // spendable UTXO plus the Lightning balance) against the cap.
    let mut holdings: HashMap<Token, u128> = HashMap::new();
    for network in Network::ALL {
        if !state.is_network_enabled(network) {
            continue;
        }
        for token in network.withdrawable_tokens() {
            let token = *token;
            let balance = match confirmed_balance(state, network, token, true).await {
                Ok(balance) => balance,
                Err(err) => {
                    warn!("treasury monitor: {} {} unreadable: {err:#}", network.as_api(), token.as_api());
                    continue;
                }
            };
            let held = holdings.entry(token).or_default();
            *held = held.saturating_add(balance);
            if network.gas_token() == Some(token) && network != Network::Bitcoin {
                let settings = state.db.asset_settings(token).await?;
                if let Some(alert_below) = settings.gas_alert_below_raw.map(|r| r.0) {
                    if balance < alert_below {
                        alerts::raise(
                            state,
                            AlertType::GasReserveLow,
                            Some(network),
                            Some(token),
                            None,
                            json!({
                                "balance_raw": balance.to_string(),
                                "alert_below_raw": alert_below.to_string(),
                                "floor_raw": settings.gas_reserve_floor_raw.map(|r| r.0.to_string()),
                                "treasury_address": treasury_address(state, network)?,
                            }),
                        )
                        .await?;
                    }
                }
            }
        }
    }
    for (token, total) in holdings {
        let Some(cap) = state.db.asset_settings(token).await?.hot_wallet_cap_raw.map(|r| r.0) else {
            continue;
        };
        if total > cap {
            alerts::raise(
                state,
                AlertType::TreasuryCapExceeded,
                None,
                Some(token),
                None,
                json!({
                    "holdings_raw": total.to_string(),
                    "cap_raw": cap.to_string(),
                    "excess_raw": (total - cap).to_string(),
                }),
            )
            .await?;
        }
    }
    Ok(())
}
