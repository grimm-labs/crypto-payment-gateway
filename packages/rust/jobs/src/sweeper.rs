//! Sweeps (spec §7.9): confirmed funds of final deposit addresses move into
//! the network's treasury — computed from the seed, never read from storage
//! (invariant 2). A sweep never touches a deposit or deposit-address status
//! (invariant 9). Bitcoin has none (withdrawals spend deposit UTXOs), but
//! its funded deposit addresses are scanned the same way to feed the UTXO
//! pool.
//!
//! - planning (every 5 min, never while frozen): candidate addresses (a
//!   confirmed deposit since their last sweep, plus a daily rescan of every
//!   final address), balances read at the confirmation depth, a PLANNED
//!   sweep above the per-(network, token) minimum;
//! - advancing (each engine tick): gas top-up when needed (Ethereum USDT,
//!   Tron), then the sweep transaction signed by the deposit key.

use std::time::Duration;

use anyhow::{Context, Result};
use chain::evm::{erc20_transfer_data, BlockTag, EvmFees, EvmRpc, ETH_TRANSFER_GAS};
use chain::solana_tx::{SolanaAccount, SolanaFees, SolanaRpc, TransferSpec, CU_LIMIT_SWEEP, TOKEN_ACCOUNT_RENT_LAMPORTS};
use chain::tron_tx::{fee_limit_sun, TronRpc, TronTransfer, TRC20_TRANSFER_BANDWIDTH};
use config::{ADDRESS_REQUEST_DELAY_MS, SWEEP_FULL_RESCAN_INTERVAL_SECS, SWEEP_INTERVAL_SECS, TRON_FEE_LIMIT_MAX_SUN};
use db::{OutboundTxRow, SweepCandidate, SweepRow};
use domain::{Network, OutboundPurpose, SweepStatus, Token};
use redis::AsyncCommands;
use serde_json::json;
use tracing::{error, info, warn};
use wallet::KeyRef;

use crate::audit::{self, Actor};
use crate::fees::{padded_gas, EthPlan, USDT_ERC20_GAS_FALLBACK, TRC20_ENERGY_FALLBACK};
use crate::outbound::{broadcast_and_record, eth, solana, tron};
use crate::treasury::{record_deposit_balance, ScannedDepositBalance};
use crate::{freeze, AppState};

/// Gas margin of a top-up (× 1.2, spec §7.9).
const TOPUP_MARGIN_PERCENT: u128 = 120;
/// Sweeps advanced per network per tick, and claimed at most (waiting ones
/// included, which never use up the budget).
const MAX_SWEEPS_PER_TICK: usize = 5;
const MAX_SWEEP_CLAIMS_PER_TICK: usize = 20;

/// Networks the sweeper scans.
pub const SWEPT_NETWORKS: [Network; 4] = [Network::Ethereum, Network::Tron, Network::Solana, Network::Bitcoin];

/// The planning loop of `network`.
pub async fn sweep_loop(state: AppState, network: Network) {
    tokio::time::sleep(Duration::from_secs(30)).await;
    let mut interval = tokio::time::interval(Duration::from_secs(SWEEP_INTERVAL_SECS));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        interval.tick().await;
        if !state.is_network_enabled(network) || !planning_allowed(network, freeze::outflows_frozen(&state).await) {
            continue;
        }
        if let Err(err) = plan(&state, network).await {
            error!("{} sweep planning error: {err:#}", network.as_api());
        }
    }
}

/// Whether the planning pass of `network` runs: never while frozen (spec
/// §7.11), except Bitcoin's, which only scans received amounts for the
/// project ledger (spec §7.3) and never signs anything — BTC is not swept.
pub fn planning_allowed(network: Network, frozen: bool) -> bool {
    !frozen || network == Network::Bitcoin
}

/// Plan sweeps now (also the admin "run sweeps" action).
pub async fn plan(state: &AppState, network: Network) -> Result<usize> {
    let marker = state.config.redis_key(&format!("sweep:full-scan:{}", network.as_db()));
    let mut redis = state.redis.clone();
    let rescanned_recently: Option<String> = redis.get(&marker).await?;
    let full = rescanned_recently.is_none();
    let candidates = if full {
        state.db.final_deposit_addresses(network).await?
    } else {
        state.db.sweep_candidates(network).await?
    };
    let mut planned = 0;
    for (i, candidate) in candidates.iter().enumerate() {
        if i > 0 {
            tokio::time::sleep(Duration::from_millis(ADDRESS_REQUEST_DELAY_MS)).await;
        }
        match plan_address(state, network, candidate).await {
            Ok(count) => planned += count,
            Err(err) => warn!("{} sweep scan of {} failed: {err:#}", network.as_api(), candidate.address),
        }
    }
    if full {
        let _: () = redis.set_ex(&marker, "1", SWEEP_FULL_RESCAN_INTERVAL_SECS as u64).await?;
    }
    if planned > 0 {
        info!("{} sweeps planned on {}", planned, network.as_api());
    }
    Ok(planned)
}

async fn plan_address(state: &AppState, network: Network, candidate: &SweepCandidate) -> Result<usize> {
    let mut planned = 0;
    if network == Network::Bitcoin {
        // The project ledger's received watermark (spec §7.3): everything the
        // address ever received at the confirmation depth, spent outputs
        // included; only ever raised.
        match state
            .chain
            .bitcoin_received_at_depth(&candidate.address, state.config.btc_confirmations)
            .await
        {
            Ok(Some(received)) => {
                state
                    .db
                    .raise_btc_received(&candidate.deposit_address_id, received)
                    .await?;
            }
            Ok(None) => {}
            Err(err) => warn!("Bitcoin received scan of {} failed: {err:#}", candidate.address),
        }
    }
    for token in network.payment_tokens() {
        let token = *token;
        let balance = confirmed_deposit_balance(state, network, token, &candidate.address).await?;
        record_deposit_balance(
            state,
            network,
            &candidate.deposit_address_id,
            &ScannedDepositBalance {
                address: candidate.address.clone(),
                derivation_index: candidate.derivation_index,
                token: token.as_db().into(),
                amount_raw: balance.to_string(),
            },
        )
        .await?;
        let Some(setting) = state.db.sweep_setting(network, token).await? else {
            continue;
        };
        if !setting.enabled || balance < setting.minimum_raw.0.max(1) {
            continue;
        }
        let credited = state
            .db
            .credited_since_last_sweep(&candidate.deposit_address_id, token)
            .await?;
        if let Some(sweep) = state
            .db
            .create_sweep(&db::NewSweep {
                network,
                token,
                deposit_address_id: &candidate.deposit_address_id,
                from_address: &candidate.address,
                amount_raw: balance,
                credited_raw: credited,
                status: SweepStatus::Planned,
            })
            .await?
        {
            if sweep.unattributed_raw.0 > 0 {
                warn!(
                    "Sweep {} of {} carries {} unattributed {} base units",
                    sweep.id, candidate.address, sweep.unattributed_raw.0, token.as_api()
                );
            }
            planned += 1;
        }
    }
    Ok(planned)
}

/// A deposit address's balance at the confirmation depth.
async fn confirmed_deposit_balance(state: &AppState, network: Network, token: Token, address: &str) -> Result<u128> {
    match (network, token) {
        (Network::Ethereum, _) => {
            let rpc: &dyn EvmRpc = &state.chain;
            let tip = rpc.block_number().await?;
            let depth = BlockTag::Number(tip.saturating_sub(u64::try_from(state.config.eth_confirmations - 1).unwrap_or(11)));
            match token {
                Token::Eth => rpc.eth_balance(address, depth).await,
                _ => rpc.erc20_balance(&state.config.chain.usdt_eth, address, depth).await,
            }
        }
        (Network::Tron, _) => TronRpc::solidity_trc20_balance(&state.chain, &state.config.chain.usdt_tron, address).await,
        // The deposit address is the USDT token account itself.
        (Network::Solana, _) => SolanaRpc::token_balance(&state.chain, address).await,
        (Network::Bitcoin, _) => {
            let tip = chain::btc_tx::EsploraRpc::tip_height(&state.chain).await?;
            let required = state.config.btc_confirmations;
            Ok(chain::btc_tx::EsploraRpc::utxos(&state.chain, address)
                .await?
                .into_iter()
                .filter(|u| {
                    u.status.confirmed
                        && u.status
                            .block_height
                            .is_some_and(|h| chain::confirmations_between(tip, h) >= required)
                })
                .map(|u| u128::from(u.value))
                .sum())
        }
        (Network::Lightning, _) => Ok(0),
    }
}

/// Advance the open sweeps of `network` (engine tick, not frozen).
pub async fn advance(state: &AppState, network: Network) -> Result<()> {
    if !matches!(network, Network::Ethereum | Network::Tron | Network::Solana) {
        return Ok(());
    }
    // Sweeps found waiting are skipped for this tick and go to the back of
    // the queue: they never hold back the others (an ETH sweep refilling
    // the treasury…), nor use up the budget.
    let mut waiting: Vec<String> = Vec::new();
    let mut progressed = 0;
    for _ in 0..MAX_SWEEP_CLAIMS_PER_TICK {
        if progressed >= MAX_SWEEPS_PER_TICK {
            break;
        }
        let Some(mut claim) = state.db.claim_sweep(network, &[SweepStatus::Planned, SweepStatus::Ready], &waiting).await? else {
            return Ok(());
        };
        let sweep = claim.row().clone();
        let Some(address) = state.db.find_deposit_address(&sweep.deposit_address_id).await? else {
            claim.set_status(SweepStatus::Skipped, Some("deposit address not found"), None).await?;
            claim.commit().await?;
            continue;
        };
        let index = u32::try_from(address.derivation_index).context("negative derivation index")?;
        if freeze::outflows_frozen(state).await {
            return Ok(());
        }
        let step = match network {
            Network::Ethereum => advance_ethereum(state, &mut claim, index).await,
            Network::Tron => advance_tron(state, &mut claim, index).await,
            _ => advance_solana(state, &mut claim, index).await,
        };
        match step {
            Ok(Step::Signed(tx)) => {
                claim.commit().await?;
                broadcast_and_record(state, &tx).await?;
                progressed += 1;
            }
            Ok(Step::Moved) => {
                claim.commit().await?;
                progressed += 1;
            }
            Ok(Step::Wait(reason)) => {
                warn!("Sweep {} waits: {reason}", sweep.id);
                // Nothing written: the claim rolls back first.
                drop(claim);
                state.db.touch_sweep(&sweep.id).await?;
                waiting.push(sweep.id);
            }
            Err(err) => {
                drop(claim);
                error!("Sweep {} failed: {err:#}", sweep.id);
                state.db.set_sweep_status(&sweep.id, SweepStatus::Failed, Some("sweep_failed"), None).await?;
                progressed += 1;
            }
        }
    }
    Ok(())
}

enum Step {
    /// A transaction was signed and stored: broadcast it after commit.
    Signed(Box<OutboundTxRow>),
    /// Status moved without a transaction.
    Moved,
    Wait(String),
}

async fn advance_ethereum(state: &AppState, claim: &mut db::SweepClaim, index: u32) -> Result<Step> {
    let sweep = claim.row().clone();
    let rpc: &dyn EvmRpc = &state.chain;
    let token = sweep.token()?;
    let deposit_key = KeyRef::Deposit { network: Network::Ethereum, index };
    let treasury = state.wallet.treasury_address(Network::Ethereum)?;
    let fees = rpc.fee_quote().await?;
    let deposit_eth = rpc.eth_balance(&sweep.from_address, BlockTag::Latest).await?;
    match token {
        // ETH: the deposit sends its balance minus the gas (dust stays).
        Token::Eth => {
            let gas_cost = fees.max_cost(ETH_TRANSFER_GAS);
            let Some(value) = deposit_eth.checked_sub(gas_cost).filter(|v| *v > 0) else {
                claim.set_status(SweepStatus::Skipped, Some("balance does not cover the gas"), None).await?;
                return Ok(Step::Moved);
            };
            let plan = EthPlan { to: treasury.clone(), value, data: vec![], gas_limit: ETH_TRANSFER_GAS, fees };
            sign_eth_sweep(state, claim, deposit_key, &treasury, plan, value).await
        }
        Token::Usdt => {
            let usdt = state.config.chain.usdt_eth.clone();
            let data = erc20_transfer_data(&treasury, sweep.amount_raw.0)?;
            let estimate = rpc.estimate_gas(&sweep.from_address, &usdt, 0, &data).await.ok();
            let plan = EthPlan {
                to: usdt,
                value: 0,
                data,
                gas_limit: padded_gas(estimate, USDT_ERC20_GAS_FALLBACK).max(ETH_TRANSFER_GAS),
                fees,
            };
            let needed = plan.max_fee().saturating_mul(TOPUP_MARGIN_PERCENT) / 100;
            if deposit_eth >= plan.max_fee() {
                return sign_eth_sweep(state, claim, deposit_key, &treasury, plan, sweep.amount_raw.0).await;
            }
            if sweep.status()? == SweepStatus::Ready {
                // The top-up landed but gas rose since: top up again.
                claim.set_status(SweepStatus::Planned, None, None).await?;
                return Ok(Step::Moved);
            }
            topup_eth(state, claim, &treasury, needed - deposit_eth.min(needed), fees).await
        }
        other => anyhow::bail!("{} is not swept on ethereum", other.as_api()),
    }
}

async fn sign_eth_sweep(
    state: &AppState,
    claim: &mut db::SweepClaim,
    key: KeyRef,
    treasury: &str,
    plan: EthPlan,
    amount: u128,
) -> Result<Step> {
    let sweep = claim.row().clone();
    let send = eth::EthSend {
        key,
        from: &sweep.from_address,
        purpose: OutboundPurpose::Sweep,
        withdrawal_id: None,
        sweep_id: Some(&sweep.id),
        token: sweep.token()?,
        recipient: treasury,
        amount,
        plan,
        replaces: None,
    };
    match eth::sign_and_store(state, &state.chain, claim.conn(), send).await? {
        Ok(tx) => {
            claim.set_status(SweepStatus::Broadcast, None, None).await?;
            Ok(Step::Signed(Box::new(tx)))
        }
        Err(reason) => Ok(Step::Wait(reason)),
    }
}

/// Exact gas shortfall from the treasury to the deposit address — never
/// below the treasury's gas reserve floor.
async fn topup_eth(state: &AppState, claim: &mut db::SweepClaim, treasury: &str, shortfall: u128, fees: EvmFees) -> Result<Step> {
    let sweep = claim.row().clone();
    let rpc: &dyn EvmRpc = &state.chain;
    let floor = crate::treasury::gas_floor(state, Network::Ethereum, Token::Eth).await?;
    let treasury_eth = rpc.eth_balance(treasury, BlockTag::Latest).await?;
    let plan = EthPlan { to: sweep.from_address.clone(), value: shortfall, data: vec![], gas_limit: ETH_TRANSFER_GAS, fees };
    if treasury_eth < shortfall.saturating_add(plan.max_fee()).saturating_add(floor) {
        return Ok(Step::Wait("treasury ETH at its gas reserve floor".into()));
    }
    let send = eth::EthSend {
        key: KeyRef::Treasury(Network::Ethereum),
        from: treasury,
        purpose: OutboundPurpose::GasTopup,
        withdrawal_id: None,
        sweep_id: Some(&sweep.id),
        token: Token::Eth,
        recipient: &sweep.from_address,
        amount: shortfall,
        plan,
        replaces: None,
    };
    match eth::sign_and_store(state, rpc, claim.conn(), send).await? {
        Ok(tx) => {
            claim.set_status(SweepStatus::TopupSent, None, None).await?;
            Ok(Step::Signed(Box::new(tx)))
        }
        Err(reason) => Ok(Step::Wait(reason)),
    }
}

async fn advance_tron(state: &AppState, claim: &mut db::SweepClaim, index: u32) -> Result<Step> {
    let sweep = claim.row().clone();
    let rpc: &dyn TronRpc = &state.chain;
    let treasury = state.wallet.treasury_address(Network::Tron)?;
    let usdt = state.config.chain.usdt_tron.clone();
    let prices = rpc.chain_fees().await?;
    let energy = rpc
        .estimate_trc20_energy(&sweep.from_address, &usdt, &treasury, sweep.amount_raw.0)
        .await
        .unwrap_or(TRC20_ENERGY_FALLBACK);
    let burn = u128::from(energy) * u128::from(prices.energy_sun);
    let bandwidth = u128::from(TRC20_TRANSFER_BANDWIDTH * prices.bandwidth_sun);
    let needed = burn * TOPUP_MARGIN_PERCENT / 100 + bandwidth;
    let balance = rpc.account_balance(&sweep.from_address).await?;
    if let Some(balance) = balance.filter(|b| *b >= needed) {
        // The deposit signs its own transfer; its TRX dust stays.
        let fee_limit = u64::try_from(balance.saturating_sub(bandwidth))
            .unwrap_or(u64::MAX)
            .min(fee_limit_sun(energy, prices.energy_sun, TRON_FEE_LIMIT_MAX_SUN));
        let send = tron::TronSend {
            key: KeyRef::Deposit { network: Network::Tron, index },
            transfer: TronTransfer::Trc20 {
                owner: sweep.from_address.clone(),
                contract: usdt,
                recipient: treasury,
                amount: sweep.amount_raw.0,
                fee_limit,
            },
            purpose: OutboundPurpose::Sweep,
            withdrawal_id: None,
            sweep_id: Some(&sweep.id),
            token: Token::Usdt,
            max_fee: u128::from(fee_limit) + bandwidth,
        };
        return match tron::sign_and_store(state, rpc, claim.conn(), send).await? {
            Ok(tx) => {
                claim.set_status(SweepStatus::Broadcast, None, None).await?;
                Ok(Step::Signed(Box::new(tx)))
            }
            Err(reason) => Ok(Step::Wait(reason)),
        };
    }
    if sweep.status()? == SweepStatus::Ready {
        claim.set_status(SweepStatus::Planned, None, None).await?;
        return Ok(Step::Moved);
    }
    // Top-up: the shortfall (sending TRX also activates a new account, the
    // treasury burning the activation fee).
    let shortfall = needed - balance.unwrap_or(0).min(needed);
    let activation = if balance.is_none() { u128::from(prices.create_account_sun) } else { 0 };
    let floor = crate::treasury::gas_floor(state, Network::Tron, Token::Trx).await?;
    let treasury_trx = rpc.account_balance(&treasury).await?.unwrap_or(0);
    let topup_fee = u128::from(chain::tron_tx::TRX_TRANSFER_BANDWIDTH * prices.bandwidth_sun) + activation;
    if treasury_trx < shortfall + topup_fee + floor {
        return Ok(Step::Wait("treasury TRX at its gas reserve floor".into()));
    }
    let send = tron::TronSend {
        key: KeyRef::Treasury(Network::Tron),
        transfer: TronTransfer::Trx {
            owner: treasury,
            recipient: sweep.from_address.clone(),
            amount: u64::try_from(shortfall).context("top-up out of range")?,
        },
        purpose: OutboundPurpose::GasTopup,
        withdrawal_id: None,
        sweep_id: Some(&sweep.id),
        token: Token::Trx,
        max_fee: topup_fee,
    };
    match tron::sign_and_store(state, rpc, claim.conn(), send).await? {
        Ok(tx) => {
            claim.set_status(SweepStatus::TopupSent, None, None).await?;
            Ok(Step::Signed(Box::new(tx)))
        }
        Err(reason) => Ok(Step::Wait(reason)),
    }
}

/// The treasury pays the fee (no top-up), then the emptied deposit token
/// account is closed: its rent returns to the treasury owner.
async fn advance_solana(state: &AppState, claim: &mut db::SweepClaim, index: u32) -> Result<Step> {
    let sweep = claim.row().clone();
    let rpc: &dyn SolanaRpc = &state.chain;
    let treasury = state.wallet.treasury_address(Network::Solana)?;
    let amount = u64::try_from(sweep.amount_raw.0).context("amount out of range")?;
    // Close only if nothing else arrived since the scan.
    let close = rpc.token_balance(&sweep.from_address).await? == u128::from(amount);
    let fees = SolanaFees {
        cu_limit: CU_LIMIT_SWEEP,
        cu_price: rpc.priority_fee(&[treasury.clone(), sweep.from_address.clone()]).await.unwrap_or(0),
    };
    let treasury_account = state
        .wallet
        .solana_usdt_token_account(KeyRef::Treasury(Network::Solana))?;
    // The first sweep creates the treasury's token account (rent).
    let rent = match rpc.account(&treasury_account).await? {
        SolanaAccount::Missing => TOKEN_ACCOUNT_RENT_LAMPORTS,
        _ => 0,
    };
    let floor = crate::treasury::gas_floor(state, Network::Solana, Token::Sol).await?;
    if rpc.lamports(&treasury).await? < u128::from(fees.max_fee_lamports(2)) + u128::from(rent) + floor {
        return Ok(Step::Wait("treasury SOL at its gas reserve floor".into()));
    }
    let tx = solana::sign_and_store(
        state,
        rpc,
        claim.conn(),
        solana::SolanaSend {
            spec: TransferSpec::Sweep {
                deposit: KeyRef::Deposit { network: Network::Solana, index },
                amount,
                close,
            },
            fees,
            purpose: OutboundPurpose::Sweep,
            withdrawal_id: None,
            sweep_id: Some(&sweep.id),
            token: Token::Usdt,
            from: &sweep.from_address,
            recipient: &treasury_account,
            amount: u128::from(amount),
            rent,
        },
    )
    .await?;
    claim.set_status(SweepStatus::Broadcast, None, None).await?;
    Ok(Step::Signed(Box::new(tx)))
}

/// A sweep or top-up transaction was accepted by the network.
pub(crate) async fn on_broadcast(state: &AppState, tx: &OutboundTxRow, purpose: OutboundPurpose) -> Result<()> {
    let action = match purpose {
        OutboundPurpose::GasTopup => "gas_topup.broadcast",
        _ => "sweep.broadcast",
    };
    audit::record(
        state,
        &Actor::system(),
        action,
        Some(("sweep", tx.sweep_id.as_deref().unwrap_or_default())),
        json!({ "tx_hash": tx.tx_hash, "network": tx.network, "amount_raw": tx.amount_raw.0.to_string() }),
    )
    .await;
    Ok(())
}

/// A top-up with a first confirmation makes its sweep READY.
pub(crate) async fn on_progress(state: &AppState, tx: &OutboundTxRow, confirmations: i32) -> Result<()> {
    if tx.purpose()? == OutboundPurpose::GasTopup && confirmations >= 1 {
        mark_ready(state, tx).await?;
    }
    Ok(())
}

async fn mark_ready(state: &AppState, tx: &OutboundTxRow) -> Result<()> {
    let Some(sweep) = find_sweep(state, tx).await? else {
        return Ok(());
    };
    if sweep.status()? == SweepStatus::TopupSent {
        state.db.set_sweep_status(&sweep.id, SweepStatus::Ready, None, tx.fee_raw.map(|f| f.0)).await?;
    }
    Ok(())
}

async fn find_sweep(state: &AppState, tx: &OutboundTxRow) -> Result<Option<SweepRow>> {
    match tx.sweep_id.as_deref() {
        Some(id) => state.db.find_sweep(id).await,
        None => Ok(None),
    }
}

/// A sweep or top-up transaction reached its final status.
pub(crate) async fn on_final(state: &AppState, tx: &OutboundTxRow, success: bool, fee: u128) -> Result<()> {
    let Some(sweep) = find_sweep(state, tx).await? else {
        return Ok(());
    };
    match (tx.purpose()?, success) {
        (OutboundPurpose::GasTopup, true) => {
            if sweep.status()? == SweepStatus::TopupSent {
                state.db.set_sweep_status(&sweep.id, SweepStatus::Ready, None, Some(fee)).await?;
            }
        }
        (OutboundPurpose::Sweep, true) => {
            state.db.set_sweep_status(&sweep.id, SweepStatus::Confirmed, None, Some(fee)).await?;
            info!("Sweep {} confirmed: {}", sweep.id, tx.tx_hash);
            audit::record(state, &Actor::system(), "sweep.confirmed", Some(("sweep", &sweep.id)), json!({ "tx_hash": tx.tx_hash, "amount_raw": tx.amount_raw.0.to_string(), "fee_raw": fee.to_string() })).await;
        }
        (_, false) => {
            state.db.set_sweep_status(&sweep.id, SweepStatus::Failed, Some("reverted"), Some(fee)).await?;
        }
        _ => {}
    }
    Ok(())
}

/// A sweep or top-up transaction can never land: the sweep fails and is
/// planned again by a later scan.
pub(crate) async fn on_dead(state: &AppState, tx: &OutboundTxRow) -> Result<()> {
    if let Some(sweep) = find_sweep(state, tx).await? {
        if !sweep.status()?.is_final() {
            state.db.set_sweep_status(&sweep.id, SweepStatus::Failed, Some("expired"), None).await?;
        }
    }
    Ok(())
}

/// Most an Ethereum top-up's fee per gas may grow to through bumps, as a
/// multiple of its first signing.
const TOPUP_MAX_FEE_MULTIPLIER: u128 = 3;

/// An Ethereum gas top-up shares the treasury's nonce sequence: stuck, it
/// would block every later treasury transaction — it is replaced with
/// higher fees (same nonce, recipient, amount), up to 3 × its first fee
/// per gas. Sweeps themselves (deposit nonces, blocking nothing else) and
/// Tron/Solana transactions are left to land or expire, and reported.
pub(crate) async fn bump(state: &AppState, tx: &OutboundTxRow) -> Result<()> {
    if tx.network()? != Network::Ethereum || tx.purpose()? != OutboundPurpose::GasTopup {
        warn!("{} {} transaction {} is slow to confirm", tx.network, tx.purpose, tx.tx_hash);
        return Ok(());
    }
    let Some(sweep_id) = tx.sweep_id.as_deref() else {
        return Ok(());
    };
    let Some(mut claim) = state.db.claim_sweep_by_id(sweep_id).await? else {
        return Ok(());
    };
    if claim.row().status()? != SweepStatus::TopupSent {
        return Ok(());
    }
    let rpc: &dyn EvmRpc = &state.chain;
    let first = state
        .db
        .outbound_txs_for_sweep(sweep_id)
        .await?
        .into_iter()
        .find(|t| t.purpose == OutboundPurpose::GasTopup.as_db() && t.nonce == tx.nonce)
        .unwrap_or_else(|| tx.clone());
    let (Some(previous), Some(original)) = (eth::signed_fees(tx), eth::signed_fees(&first)) else {
        return Ok(());
    };
    let fees = eth::replacement_fees(previous, rpc.fee_quote().await?);
    if fees.max_fee_per_gas > original.max_fee_per_gas.saturating_mul(TOPUP_MAX_FEE_MULTIPLIER) {
        error!("Gas top-up {} of sweep {sweep_id} is stuck beyond 3 × its fee: the treasury nonce is blocked", tx.tx_hash);
        return Ok(());
    }
    if freeze::outflows_frozen(state).await {
        return Ok(());
    }
    let treasury = state.wallet.treasury_address(Network::Ethereum)?;
    let plan = EthPlan { to: tx.to_address.clone(), value: tx.amount_raw.0, data: vec![], gas_limit: ETH_TRANSFER_GAS, fees };
    let send = eth::EthSend {
        key: KeyRef::Treasury(Network::Ethereum),
        from: &treasury,
        purpose: OutboundPurpose::GasTopup,
        withdrawal_id: None,
        sweep_id: Some(sweep_id),
        token: Token::Eth,
        recipient: &tx.to_address,
        amount: tx.amount_raw.0,
        plan,
        replaces: Some(tx),
    };
    match eth::sign_and_store(state, rpc, claim.conn(), send).await? {
        Ok(replacement) => {
            claim.commit().await?;
            info!("Gas top-up {} replaced by {} (fee bump)", tx.tx_hash, replacement.tx_hash);
            broadcast_and_record(state, &replacement).await
        }
        Err(reason) => {
            warn!("Gas top-up {} not bumped: {reason}", tx.tx_hash);
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The freeze stops sweeps and top-ups (spec §7.11); the Bitcoin pass
    /// only scans received amounts for the project ledger (spec §7.3), so
    /// it keeps running — a long freeze must not under-credit projects.
    #[test]
    fn the_freeze_stops_every_planning_but_the_bitcoin_scan() {
        for network in SWEPT_NETWORKS {
            assert!(planning_allowed(network, false), "{network:?}");
            assert_eq!(planning_allowed(network, true), network == Network::Bitcoin, "{network:?}");
        }
    }
}
