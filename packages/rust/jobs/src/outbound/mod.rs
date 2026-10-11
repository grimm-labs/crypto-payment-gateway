//! The outbound transaction engine (spec §7.8): one loop per network that
//!
//! 1. tracks every live transaction (confirmations, reverts, replacements,
//!    provable expiry) and bumps the stuck ones up to their fee cap;
//! 2. re-broadcasts, byte for byte, transactions stored SIGNED by a worker
//!    that died before broadcasting them (invariant 1);
//! 3. signs again withdrawals whose every transaction is provably dead;
//! 4. claims QUEUED withdrawals (`FOR UPDATE SKIP LOCKED`), re-checks them
//!    (invariant 6), signs and stores them (persist before broadcast) and
//!    broadcasts;
//! 5. advances sweeps and gas top-ups (`crate::sweeper`).
//!
//! The freeze (invariant 3) is checked right before signing and right
//! before broadcasting; while frozen only already-public transactions are
//! re-broadcast.

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::Result;
use chain::btc_tx::EsploraRpc;
use chain::evm::EvmRpc;
use chrono::Utc;
use db::outbound::has_open_or_landed_tx;
use db::{OutboundTxRow, OutboundUpdate, WithdrawalClaim, WithdrawalRow, WithdrawalUpdate};
use domain::{
    next_withdrawal_status, required_confirmations, AlertType, Network, OutboundPurpose, OutboundTxStatus,
    WebhookEventType, WithdrawalEvent, WithdrawalStatus,
};
use serde_json::json;
use tracing::{error, info, warn};

use crate::audit::{self, Actor};
use crate::withdrawals::{notify_withdrawal, recheck_before_signing};
use crate::{alerts, freeze, sweeper, AppState};

pub mod btc;
pub mod eth;
pub mod lightning;
pub mod solana;
pub mod tron;

/// QUEUED withdrawals signed per network per tick…
const MAX_SIGNED_PER_TICK: usize = 5;
/// …and claimed at most (waiting ones included, which never use up the
/// signing budget).
const MAX_CLAIMS_PER_TICK: usize = 20;

/// Result of signing a withdrawal's transaction.
#[derive(Debug)]
pub enum SignResult {
    /// Stored (SIGNED) on the claim's connection.
    Signed(Box<OutboundTxRow>),
    /// Not now (fee above the cap at first signing, in-flight limit…): the
    /// claim is dropped, the withdrawal stays as it was.
    Wait(String),
    /// A replacement would exceed the fee cap: the withdrawal is STUCK.
    Stall,
    /// Cannot be executed: FAILED with this client-safe reason.
    Fail(&'static str),
}

/// Result of broadcasting a stored transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BroadcastResult {
    /// Accepted (or already known / mined).
    Accepted,
    /// The node says it cannot land (nonce used, TAPOS/blockhash expired,
    /// inputs spent). Never proof on its own — an earlier send whose answer
    /// was lost may have been accepted: only tracking (`Observation::Dead`)
    /// closes a transaction.
    Dead(&'static str),
    /// Refused, maybe transiently: retried later (detail for the logs).
    Rejected(String),
}

/// What tracking found about a live transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Observation {
    /// Not landed. `stuck`: waited past the network's window without
    /// confirming; `resend`: the network does not know it (re-broadcast).
    Pending { stuck: bool, resend: bool },
    /// Landed (mined / solidified / finalized), successfully or not.
    Landed {
        success: bool,
        block: u64,
        confirmations: i32,
        fee: u128,
    },
    /// Provably unable to land any more.
    Dead(&'static str),
}

/// What to do with one subject's (withdrawal's or sweep's) live
/// transactions, given what tracking observed of each (oldest first).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrackingDecision {
    /// `landed` landed: the others (if still live) are REPLACED.
    Landed { landed: usize, success: bool, confirmations: i32, final_: bool },
    /// Every transaction is dead: replace them (sign again).
    AllDead,
    /// Still pending; `dead` are expired siblings to close; `bump` when the
    /// latest one is stuck; `resend` the latest one.
    Pending { dead: Vec<usize>, bump: bool, resend: bool },
}

/// The tracking decision of a subject (pure): the first landed
/// transaction wins (a nonce or an input set can only be spent once).
pub fn decide(observations: &[Observation], required_confirmations: i32) -> TrackingDecision {
    if let Some((index, Observation::Landed { success, confirmations, .. })) = observations
        .iter()
        .enumerate()
        .find(|(_, o)| matches!(o, Observation::Landed { .. }))
    {
        return TrackingDecision::Landed {
            landed: index,
            success: *success,
            confirmations: *confirmations,
            final_: !*success || *confirmations >= required_confirmations,
        };
    }
    let dead: Vec<usize> = observations
        .iter()
        .enumerate()
        .filter(|(_, o)| matches!(o, Observation::Dead(_)))
        .map(|(i, _)| i)
        .collect();
    if !observations.is_empty() && dead.len() == observations.len() {
        return TrackingDecision::AllDead;
    }
    let (bump, resend) = match observations.last() {
        Some(Observation::Pending { stuck, resend }) => (*stuck, *resend),
        _ => (false, false),
    };
    TrackingDecision::Pending { dead, bump, resend }
}

/// The engine loop of `network`.
pub async fn outbound_loop(state: AppState, network: Network) {
    let secs = state.config.polling_interval_seconds.max(config::OUTBOUND_WORKER_INTERVAL_SECS);
    let mut interval = tokio::time::interval(Duration::from_secs(secs));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        interval.tick().await;
        if !state.is_network_enabled(network) {
            continue;
        }
        if let Err(err) = run_once(&state, network).await {
            error!("{} outbound engine error: {err:#}", network.as_api());
        }
    }
}

async fn run_once(state: &AppState, network: Network) -> Result<()> {
    let frozen = freeze::outflows_frozen(state).await;
    if network == Network::Lightning {
        lightning::track(state).await?;
        if !frozen {
            for _ in 0..MAX_SIGNED_PER_TICK {
                if !lightning::execute_next(state).await? {
                    break;
                }
            }
        }
        return Ok(());
    }
    track(state, network, frozen).await?;
    settle_landed(state, network).await?;
    if frozen {
        return Ok(());
    }
    rebroadcast_stale(state, network).await?;
    resign_dead(state, network).await?;
    // Withdrawals found waiting (fee above their cap, UTXOs not confirmed
    // yet…) are skipped for this tick and go to the back of the queue: they
    // never hold back the others, nor use up the signing budget.
    let mut waiting: Vec<String> = Vec::new();
    let mut progressed = 0;
    for _ in 0..MAX_CLAIMS_PER_TICK {
        if progressed >= MAX_SIGNED_PER_TICK {
            break;
        }
        match execute_next(state, network, &waiting).await? {
            Next::Idle => break,
            Next::Progress => progressed += 1,
            Next::Waiting(id) => {
                state.db.touch_queued_withdrawal(&id).await?;
                waiting.push(id);
            }
        }
    }
    sweeper::advance(state, network).await
}

/// What one `execute_next` did.
enum Next {
    /// Nothing to claim (or frozen): stop for this tick.
    Idle,
    Progress,
    /// This withdrawal cannot be signed yet.
    Waiting(String),
}

/// Sign one transaction for `claim` (first signing, re-sign after a proven
/// expiry, or a fee bump of `replaces`).
async fn sign(state: &AppState, network: Network, claim: &mut WithdrawalClaim, replaces: Option<&OutboundTxRow>) -> Result<SignResult> {
    match network {
        Network::Ethereum => eth::sign_withdrawal(state, &state.chain, claim, replaces).await,
        Network::Tron => tron::sign_withdrawal(state, &state.chain, claim).await,
        Network::Solana => solana::sign_withdrawal(state, &state.chain, claim).await,
        Network::Bitcoin => btc::sign_withdrawal(state, &state.chain, claim, replaces).await,
        Network::Lightning => anyhow::bail!("Lightning withdrawals are not signed locally"),
    }
}

/// Broadcast the stored bytes of `tx` (never anything else).
pub async fn broadcast_stored(state: &AppState, tx: &OutboundTxRow) -> Result<BroadcastResult> {
    match tx.network()? {
        Network::Ethereum => eth::broadcast(&state.chain, tx).await,
        Network::Tron => tron::broadcast(&state.chain, tx).await,
        Network::Solana => solana::broadcast(&state.chain, tx).await,
        Network::Bitcoin => btc::broadcast(&state.chain, tx).await,
        Network::Lightning => anyhow::bail!("no Lightning transaction to broadcast"),
    }
}

async fn observe(state: &AppState, tx: &OutboundTxRow) -> Result<Observation> {
    match tx.network()? {
        Network::Ethereum => eth::observe(&state.chain, tx).await,
        Network::Tron => tron::observe(&state.chain, tx).await,
        Network::Solana => solana::observe(&state.chain, tx).await,
        Network::Bitcoin => btc::observe(&state.chain, tx).await,
        Network::Lightning => anyhow::bail!("no Lightning transaction to observe"),
    }
}

/// Claim, re-check and sign the oldest QUEUED withdrawal of `network`
/// (except `skip`), then broadcast it.
async fn execute_next(state: &AppState, network: Network, skip: &[String]) -> Result<Next> {
    let Some(mut claim) = state.db.claim_queued_withdrawal(network, skip).await? else {
        return Ok(Next::Idle);
    };
    let row = claim.row().clone();
    if let Err(reason) = recheck_before_signing(state, &row).await? {
        fail_claim(state, claim, reason).await?;
        return Ok(Next::Progress);
    }
    // Invariant 3: nothing is signed while frozen.
    if freeze::outflows_frozen(state).await {
        return Ok(Next::Idle);
    }
    match sign(state, network, &mut claim, None).await? {
        SignResult::Signed(tx) => {
            transition_claim(&mut claim, WithdrawalEvent::Sign, &WithdrawalUpdate {
                tx_hash: Some(tx.tx_hash.clone()),
                ..Default::default()
            })
            .await?;
            let row = claim.commit().await?;
            audit::record(state, &Actor::system(), "withdrawal.signed", Some(("withdrawal", &row.id)), json!({ "tx_hash": tx.tx_hash })).await;
            broadcast_and_record(state, &tx).await?;
            Ok(Next::Progress)
        }
        SignResult::Wait(reason) => {
            warn!("Withdrawal {} not signed yet: {reason}", row.id);
            Ok(Next::Waiting(row.id))
        }
        SignResult::Fail(reason) => {
            fail_claim(state, claim, reason).await?;
            Ok(Next::Progress)
        }
        SignResult::Stall => {
            warn!("Withdrawal {} not signed: fee above its cap", row.id);
            Ok(Next::Waiting(row.id))
        }
    }
}

async fn transition_claim(claim: &mut WithdrawalClaim, event: WithdrawalEvent, update: &WithdrawalUpdate) -> Result<()> {
    let (id, from, lightning) = {
        let row = claim.row();
        (row.id.clone(), row.status()?, row.network()? == Network::Lightning)
    };
    let to = next_withdrawal_status(from, event, lightning)?;
    claim
        .transition(to, update)
        .await?
        .ok_or_else(|| anyhow::anyhow!("withdrawal {id} moved under its claim"))?;
    Ok(())
}

/// FAILED before anything was broadcast: the reservation is released.
pub(crate) async fn fail_claim(state: &AppState, mut claim: WithdrawalClaim, reason: &'static str) -> Result<()> {
    transition_claim(&mut claim, WithdrawalEvent::Fail, &WithdrawalUpdate {
        failure_reason: Some(reason.to_string()),
        ..Default::default()
    })
    .await?;
    let row = claim.commit().await?;
    warn!("Withdrawal {} failed: {reason}", row.id);
    audit::record(state, &Actor::system(), "withdrawal.failed", Some(("withdrawal", &row.id)), json!({ "reason": reason })).await;
    notify_withdrawal(state, &row, WebhookEventType::WithdrawalFailed).await;
    Ok(())
}

/// Move a withdrawal with the status machine (outside a claim). `None`
/// when it already moved elsewhere.
pub(crate) async fn transition(state: &AppState, row: &WithdrawalRow, event: WithdrawalEvent, update: &WithdrawalUpdate) -> Result<Option<WithdrawalRow>> {
    let from = row.status()?;
    let Ok(to) = next_withdrawal_status(from, event, row.network()? == Network::Lightning) else {
        return Ok(None);
    };
    state.db.transition_withdrawal(&row.id, &[from], to, update).await
}

/// Broadcast a stored transaction (if not frozen) and record the outcome.
pub(crate) async fn broadcast_and_record(state: &AppState, tx: &OutboundTxRow) -> Result<()> {
    // Invariant 3: nothing new leaves while frozen (it stays SIGNED and is
    // re-broadcast once unfrozen).
    if freeze::outflows_frozen(state).await {
        return Ok(());
    }
    match broadcast_stored(state, tx).await {
        Ok(BroadcastResult::Accepted) => mark_broadcast(state, tx).await,
        // Not proof (see `BroadcastResult::Dead`): kept, tracking decides.
        Ok(BroadcastResult::Dead(reason)) => {
            warn!("{} transaction {} refused ({reason}) — kept until tracking proves it", tx.network, tx.tx_hash);
            state.db.record_outbound_error(&tx.id, reason).await
        }
        Ok(BroadcastResult::Rejected(detail)) => {
            warn!("{} transaction {} refused (kept, retried): {detail}", tx.network, tx.tx_hash);
            state.db.record_outbound_error(&tx.id, "rejected_by_network").await
        }
        Err(err) => {
            warn!("{} broadcast of {} failed (retried): {err:#}", tx.network, tx.tx_hash);
            state.db.record_outbound_error(&tx.id, "broadcast_failed").await
        }
    }
}

/// SIGNED → BROADCAST, then what follows for its subject (once: a
/// transaction already moved is left alone).
async fn mark_broadcast(state: &AppState, tx: &OutboundTxRow) -> Result<()> {
    let Some(tx) = state
        .db
        .transition_outbound_tx(&tx.id, &[OutboundTxStatus::Signed], OutboundTxStatus::Broadcast, &OutboundUpdate::default())
        .await?
    else {
        return Ok(());
    };
    on_broadcast(state, &tx).await
}

/// A transaction just became public: its withdrawal leaves SIGNED (webhook),
/// or its sweep records it.
async fn on_broadcast(state: &AppState, tx: &OutboundTxRow) -> Result<()> {
    match tx.purpose()? {
        OutboundPurpose::Withdrawal => {
            let Some(row) = state.db.find_withdrawal(tx.withdrawal_id.as_deref().unwrap_or_default()).await? else {
                return Ok(());
            };
            if row.status()? == WithdrawalStatus::Signed {
                if let Some(row) = transition(state, &row, WithdrawalEvent::Broadcast, &WithdrawalUpdate {
                    tx_hash: Some(tx.tx_hash.clone()),
                    broadcast_now: true,
                    ..Default::default()
                })
                .await?
                {
                    info!("Withdrawal {} broadcast: {}", row.id, tx.tx_hash);
                    audit::record(state, &Actor::system(), "withdrawal.broadcast", Some(("withdrawal", &row.id)), json!({ "tx_hash": tx.tx_hash })).await;
                    notify_withdrawal(state, &row, WebhookEventType::WithdrawalBroadcast).await;
                }
            }
            Ok(())
        }
        purpose => sweeper::on_broadcast(state, tx, purpose).await,
    }
}

/// A transaction tracking proved can never land: closed, its inputs freed
/// (handed back to a live sibling that spends them too); a withdrawal is
/// signed again next tick, a sweep is re-planned.
async fn close_dead(state: &AppState, tx: &OutboundTxRow, reason: &'static str) -> Result<()> {
    info!("{} transaction {} cannot land ({reason})", tx.network, tx.tx_hash);
    state
        .db
        .transition_outbound_tx(
            &tx.id,
            &[OutboundTxStatus::Signed, OutboundTxStatus::Broadcast],
            OutboundTxStatus::Expired,
            &OutboundUpdate { last_error: Some(reason.into()), ..Default::default() },
        )
        .await?;
    if tx.purpose()? == OutboundPurpose::Withdrawal && tx.network()? == Network::Bitcoin {
        // A refused RBF replacement had taken over its original's inputs:
        // the original, still live, takes them back first (no window where
        // they look free).
        btc::restore_sibling_reservations(state, tx).await?;
    }
    state.db.release_utxo_reservations(&tx.id).await?;
    if tx.purpose()? != OutboundPurpose::Withdrawal {
        sweeper::on_dead(state, tx).await?;
    }
    Ok(())
}

/// Transactions stored SIGNED longer than 30 s: a worker died between
/// storing and broadcasting. The same bytes go out (invariant 1).
async fn rebroadcast_stale(state: &AppState, network: Network) -> Result<()> {
    for tx in state.db.stale_signed_outbound_txs(config::OUTBOUND_REBROADCAST_AFTER_SECS).await? {
        if tx.network != network.as_db() {
            continue;
        }
        info!("Re-broadcasting stored {} transaction {}", tx.network, tx.tx_hash);
        broadcast_and_record(state, &tx).await?;
    }
    Ok(())
}

/// Withdrawals whose every transaction is provably dead are signed again
/// (re-checked first: they never left).
async fn resign_dead(state: &AppState, network: Network) -> Result<()> {
    for row in state.db.withdrawals_without_live_tx(network).await? {
        let Some(mut claim) = state.db.claim_withdrawal(&row.id).await? else {
            continue;
        };
        let status = claim.row().status()?;
        if !matches!(status, WithdrawalStatus::Signed | WithdrawalStatus::Broadcast) {
            continue;
        }
        // The candidate list was read without the lock: another worker may
        // have signed (or settled) it since. Never twice.
        let id = claim.row().id.clone();
        if has_open_or_landed_tx(claim.conn(), &id).await? {
            continue;
        }
        // A closed transaction of it may have landed after all: tracked
        // again instead of signing anything new.
        if revive_family(state, &id).await? {
            continue;
        }
        if let Err(reason) = recheck_before_signing(state, claim.row()).await? {
            fail_claim(state, claim, reason).await?;
            continue;
        }
        if freeze::outflows_frozen(state).await {
            return Ok(());
        }
        match sign(state, network, &mut claim, None).await? {
            SignResult::Signed(tx) => {
                transition_claim(&mut claim, WithdrawalEvent::Replace, &WithdrawalUpdate {
                    tx_hash: Some(tx.tx_hash.clone()),
                    ..Default::default()
                })
                .await?;
                let row = claim.commit().await?;
                audit::record(state, &Actor::system(), "withdrawal.signed", Some(("withdrawal", &row.id)), json!({ "tx_hash": tx.tx_hash, "replaces_expired": true })).await;
                broadcast_and_record(state, &tx).await?;
            }
            SignResult::Fail(reason) => fail_claim(state, claim, reason).await?,
            SignResult::Stall => stall(state, claim).await?,
            SignResult::Wait(reason) => warn!("Withdrawal {} not signed again yet: {reason}", row.id),
        }
    }
    Ok(())
}

/// Whether a closed transaction is known to the network after all: mined
/// (a reorg, a replaced Bitcoin original mined anyway…) or, on Bitcoin,
/// back in the mempool. Tron/Solana closures are provable expiries; only a
/// landing counts there.
async fn seen_on_chain(state: &AppState, tx: &OutboundTxRow) -> Result<bool> {
    Ok(match tx.network()? {
        Network::Ethereum => EvmRpc::receipt(&state.chain, &tx.tx_hash).await?.is_some(),
        Network::Bitcoin => EsploraRpc::tx_status(&state.chain, &tx.tx_hash).await?.is_some(),
        _ => matches!(observe(state, tx).await?, Observation::Landed { .. }),
    })
}

/// Before a withdrawal is signed again or abandoned, every transaction
/// ever signed for it (closed ones included) is looked up on chain. One
/// that is known after all is tracked again (and, on Bitcoin, its inputs
/// reserved again): nothing new is signed. `true` when one was revived.
async fn revive_family(state: &AppState, withdrawal_id: &str) -> Result<bool> {
    let mut revived = false;
    for tx in state.db.outbound_txs_for_withdrawal(withdrawal_id).await? {
        if !matches!(tx.status()?, OutboundTxStatus::Replaced | OutboundTxStatus::Expired) {
            continue;
        }
        if !seen_on_chain(state, &tx).await? {
            continue;
        }
        warn!("{} transaction {} of withdrawal {withdrawal_id} is on chain after all — tracked again", tx.network, tx.tx_hash);
        if state
            .db
            .transition_outbound_tx(
                &tx.id,
                &[OutboundTxStatus::Replaced, OutboundTxStatus::Expired],
                OutboundTxStatus::Broadcast,
                &OutboundUpdate::default(),
            )
            .await?
            .is_some()
        {
            if tx.network()? == Network::Bitcoin {
                btc::reserve_inputs(state, &tx).await?;
            }
            revived = true;
        }
    }
    Ok(revived)
}

/// STUCK: the fee cap is reached. Alert, audit, webhook.
async fn stall(state: &AppState, mut claim: WithdrawalClaim) -> Result<()> {
    transition_claim(&mut claim, WithdrawalEvent::Stall, &WithdrawalUpdate::default()).await?;
    let row = claim.commit().await?;
    warn!("Withdrawal {} is STUCK (fee cap reached)", row.id);
    audit::record(state, &Actor::system(), "withdrawal.stuck", Some(("withdrawal", &row.id)), json!({ "fee_cap_raw": row.fee_cap_raw.map(|c| c.0.to_string()) })).await;
    alerts::raise(
        state,
        AlertType::WithdrawalStuck,
        Some(row.network()?),
        Some(row.token()?),
        Some(&row.id),
        json!({ "withdrawal_id": row.id, "tx_hash": row.tx_hash, "fee_cap_raw": row.fee_cap_raw.map(|c| c.0.to_string()) }),
    )
    .await?;
    Ok(())
}

/// Track every live transaction of `network`, grouped by subject.
async fn track(state: &AppState, network: Network, frozen: bool) -> Result<()> {
    let required = required_confirmations(
        network,
        state.config.tron_confirmations,
        state.config.eth_confirmations,
        state.config.btc_confirmations,
    );
    let mut subjects: BTreeMap<String, Vec<OutboundTxRow>> = BTreeMap::new();
    for tx in state.db.live_outbound_txs(network).await? {
        let key = tx
            .withdrawal_id
            .clone()
            .map(|id| format!("w:{id}"))
            .or_else(|| tx.sweep_id.clone().map(|id| format!("s:{id}:{}", tx.purpose)))
            .unwrap_or_default();
        subjects.entry(key).or_default().push(tx);
    }
    for txs in subjects.into_values() {
        if let Err(err) = track_subject(state, txs, required, frozen).await {
            error!("{} tracking error: {err:#}", network.as_api());
        }
    }
    Ok(())
}

async fn track_subject(state: &AppState, txs: Vec<OutboundTxRow>, required: i32, frozen: bool) -> Result<()> {
    let mut observations = Vec::with_capacity(txs.len());
    for tx in &txs {
        observations.push(observe(state, tx).await?);
    }
    match decide(&observations, required) {
        TrackingDecision::Landed { landed, success, confirmations, final_ } => {
            let tx = &txs[landed];
            let Observation::Landed { block, fee, .. } = observations[landed] else {
                unreachable!("decide picked a landed observation")
            };
            on_landed(state, tx, &txs, success, block, confirmations, fee, final_).await
        }
        TrackingDecision::AllDead => {
            for (tx, observation) in txs.iter().zip(&observations) {
                let Observation::Dead(reason) = observation else { continue };
                close_dead(state, tx, reason).await?;
            }
            Ok(())
        }
        TrackingDecision::Pending { dead, bump, resend } => {
            for index in dead {
                if let Observation::Dead(reason) = observations[index] {
                    close_dead(state, &txs[index], reason).await?;
                }
            }
            let latest = txs.last().expect("subjects have transactions");
            if resend && latest.status()? == OutboundTxStatus::Broadcast {
                // Already public: allowed even while frozen.
                if let Ok(BroadcastResult::Rejected(detail)) = broadcast_stored(state, latest).await {
                    warn!("Re-broadcast of {} refused: {detail}", latest.tx_hash);
                }
            }
            if bump && !frozen {
                bump_subject(state, latest).await?;
            }
            Ok(())
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn on_landed(
    state: &AppState,
    tx: &OutboundTxRow,
    siblings: &[OutboundTxRow],
    success: bool,
    block: u64,
    confirmations: i32,
    fee: u128,
    final_: bool,
) -> Result<()> {
    let block_number = i64::try_from(block).ok();
    // Broadcast by a run that died before recording it (the transaction, or
    // only its withdrawal, still SIGNED): recorded now. `on_broadcast` is
    // idempotent for a withdrawal; a sweep's is recorded once.
    let moved = tx.status()? == OutboundTxStatus::Signed
        && state
            .db
            .transition_outbound_tx(&tx.id, &[OutboundTxStatus::Signed], OutboundTxStatus::Broadcast, &OutboundUpdate::default())
            .await?
            .is_some();
    if moved || tx.purpose()? == OutboundPurpose::Withdrawal {
        on_broadcast(state, tx).await?;
    }
    if !final_ {
        state.db.update_outbound_progress(&tx.id, block_number, confirmations).await?;
        if tx.purpose()? == OutboundPurpose::Withdrawal {
            update_withdrawal_progress(state, tx.withdrawal_id.as_deref().unwrap_or_default(), &tx.tx_hash, confirmations).await?;
        } else {
            sweeper::on_progress(state, tx, confirmations).await?;
        }
        return Ok(());
    }

    // Final: any sibling still live is replaced by the one that landed. Not
    // before — a reorg may still mine a sibling instead, so they stay
    // tracked until then (`decide` picks the landed one meanwhile).
    for sibling in siblings.iter().filter(|s| s.id != tx.id) {
        state
            .db
            .transition_outbound_tx(
                &sibling.id,
                &[OutboundTxStatus::Signed, OutboundTxStatus::Broadcast],
                OutboundTxStatus::Replaced,
                &OutboundUpdate::default(),
            )
            .await?;
        state.db.release_utxo_reservations(&sibling.id).await?;
    }
    let (to, error) = if success {
        (OutboundTxStatus::Confirmed, None)
    } else {
        (OutboundTxStatus::Failed, Some("reverted".to_string()))
    };
    state
        .db
        .transition_outbound_tx(
            &tx.id,
            &[OutboundTxStatus::Signed, OutboundTxStatus::Broadcast],
            to,
            &OutboundUpdate {
                block_number,
                confirmations: Some(confirmations),
                fee_raw: Some(fee),
                last_error: error,
            },
        )
        .await?;
    // Spent (or failed on chain): its inputs no longer exist / are free.
    state.db.release_utxo_reservations(&tx.id).await?;

    if tx.purpose()? != OutboundPurpose::Withdrawal {
        return sweeper::on_final(state, tx, success, fee).await;
    }
    settle_withdrawal(state, tx, success, fee, confirmations).await
}

/// Move a withdrawal to CONFIRMED / FAILED from its landed transaction
/// (idempotent: a withdrawal already settled is left alone).
async fn settle_withdrawal(state: &AppState, tx: &OutboundTxRow, success: bool, fee: u128, confirmations: i32) -> Result<()> {
    let Some(row) = state.db.find_withdrawal(tx.withdrawal_id.as_deref().unwrap_or_default()).await? else {
        return Ok(());
    };
    let (event, update, action, webhook) = if success {
        (
            WithdrawalEvent::Confirm,
            WithdrawalUpdate {
                tx_hash: Some(tx.tx_hash.clone()),
                fee_raw: Some(fee),
                confirmations: Some(confirmations),
                confirmed_now: true,
                broadcast_now: true,
                ..Default::default()
            },
            "withdrawal.confirmed",
            WebhookEventType::WithdrawalConfirmed,
        )
    } else {
        (
            WithdrawalEvent::Fail,
            WithdrawalUpdate {
                tx_hash: Some(tx.tx_hash.clone()),
                fee_raw: Some(fee),
                failure_reason: Some("reverted".into()),
                broadcast_now: true,
                ..Default::default()
            },
            "withdrawal.failed",
            WebhookEventType::WithdrawalFailed,
        )
    };
    if let Some(row) = transition(state, &row, event, &update).await? {
        info!("Withdrawal {} {}: {}", row.id, row.status, tx.tx_hash);
        audit::record(state, &Actor::system(), action, Some(("withdrawal", &row.id)), json!({ "tx_hash": tx.tx_hash, "fee_raw": fee.to_string() })).await;
        notify_withdrawal(state, &row, webhook).await;
    }
    Ok(())
}

/// Withdrawals left open although one of their transactions is stored as
/// landed (a run died between the two updates): settled from the stored
/// outcome — never signed again (`withdrawals_without_live_tx` skips them).
async fn settle_landed(state: &AppState, network: Network) -> Result<()> {
    for tx in state.db.landed_txs_of_open_withdrawals(network).await? {
        warn!("Withdrawal {} settled late from {}", tx.withdrawal_id.as_deref().unwrap_or_default(), tx.tx_hash);
        let success = tx.status()? == OutboundTxStatus::Confirmed;
        let fee = tx.fee_raw.map(|f| f.0).unwrap_or(0);
        settle_withdrawal(state, &tx, success, fee, tx.confirmations).await?;
    }
    Ok(())
}

/// Record a withdrawal's confirmation count without moving it.
async fn update_withdrawal_progress(state: &AppState, id: &str, tx_hash: &str, confirmations: i32) -> Result<()> {
    let Some(row) = state.db.find_withdrawal(id).await? else {
        return Ok(());
    };
    let status = row.status()?;
    if matches!(status, WithdrawalStatus::Broadcast | WithdrawalStatus::Stuck) {
        state
            .db
            .transition_withdrawal(id, &[status], status, &WithdrawalUpdate {
                confirmations: Some(confirmations),
                tx_hash: Some(tx_hash.to_string()),
                ..Default::default()
            })
            .await?;
    }
    Ok(())
}

/// The latest transaction of a subject waited past its window: replace it
/// with higher fees (withdrawals within their fee cap — beyond, STUCK).
/// Tron is never replaced before a proven expiry: a transaction the node
/// keeps knowing past it is STUCK at once, for an admin to see (a Solana
/// blockhash expires provably within minutes, never reported stuck).
async fn bump_subject(state: &AppState, latest: &OutboundTxRow) -> Result<()> {
    let network = latest.network()?;
    if latest.purpose()? != OutboundPurpose::Withdrawal {
        return sweeper::bump(state, latest).await;
    }
    let Some(mut claim) = state.db.claim_withdrawal(latest.withdrawal_id.as_deref().unwrap_or_default()).await? else {
        return Ok(());
    };
    // SIGNED too: a transaction the node keeps refusing is stuck as well.
    // STUCK is the admin's call (bump or abandon).
    if !matches!(claim.row().status()?, WithdrawalStatus::Signed | WithdrawalStatus::Broadcast) {
        return Ok(());
    }
    if !matches!(network, Network::Ethereum | Network::Bitcoin) {
        return stall(state, claim).await;
    }
    // A later replacement may have been refused and closed (an RBF
    // replacement that did not outbid the descendants it would evict): the
    // next one escalates from its fees, after the same wait — never a new
    // attempt every tick at the same fee. Only its fees: the inputs (and
    // nonce) stay the live transaction's, whose reservations are held — the
    // closed attempt's extra inputs were released and may be spent since.
    let attempts = state.db.outbound_txs_for_withdrawal(&claim.row().id).await?;
    let basis = match attempts.into_iter().filter(|t| t.created_at > latest.created_at).max_by_key(|t| t.created_at) {
        Some(newer) => {
            let wait = match network {
                Network::Bitcoin => config::BTC_STUCK_BLOCKS as i64 * 600,
                _ => config::ETH_STUCK_BLOCKS as i64 * 12,
            };
            if Utc::now() - newer.created_at < chrono::Duration::seconds(wait) {
                return Ok(());
            }
            escalation_basis(latest, &newer)
        }
        None => latest.clone(),
    };
    if freeze::outflows_frozen(state).await {
        return Ok(());
    }
    match sign(state, network, &mut claim, Some(&basis)).await? {
        SignResult::Signed(tx) => {
            transition_claim(&mut claim, WithdrawalEvent::Replace, &WithdrawalUpdate {
                tx_hash: Some(tx.tx_hash.clone()),
                ..Default::default()
            })
            .await?;
            let row = claim.commit().await?;
            info!("Withdrawal {} bumped: {} replaces {}", row.id, tx.tx_hash, latest.tx_hash);
            audit::record(state, &Actor::system(), "withdrawal.bumped", Some(("withdrawal", &row.id)), json!({ "tx_hash": tx.tx_hash, "replaces": latest.tx_hash })).await;
            broadcast_and_record(state, &tx).await
        }
        SignResult::Stall => stall(state, claim).await,
        SignResult::Fail(reason) => {
            warn!("Bump of withdrawal {} impossible: {reason}", claim.row().id);
            Ok(())
        }
        SignResult::Wait(reason) => {
            warn!("Bump of withdrawal {} deferred: {reason}", claim.row().id);
            Ok(())
        }
    }
}

/// `latest` (inputs, nonce, id) with the fee level of a newer, closed
/// attempt to escalate from.
fn escalation_basis(latest: &OutboundTxRow, newer: &OutboundTxRow) -> OutboundTxRow {
    let mut basis = latest.clone();
    for field in ["feeRate", "maxFeePerGas", "maxPriorityFeePerGas"] {
        if let Some(value) = newer.fee_params.get(field) {
            basis.fee_params[field] = value.clone();
        }
    }
    basis.fee_raw = newer.fee_raw.max(latest.fee_raw);
    basis
}

/// Admin abandon of a STUCK withdrawal, only once the network proves none
/// of its transactions can land (spec §7.3). `Ok(false)` when it cannot be
/// proven yet.
pub async fn abandon(state: &AppState, actor: &Actor, row: &WithdrawalRow) -> Result<bool> {
    let all = state.db.outbound_txs_for_withdrawal(&row.id).await?;
    // One of them landed: it settles the withdrawal (`settle_landed`),
    // never an abandon.
    if all
        .iter()
        .any(|tx| matches!(tx.status(), Ok(OutboundTxStatus::Confirmed | OutboundTxStatus::Failed)))
    {
        return Ok(false);
    }
    // Nor while a closed one turns out to be on chain.
    if revive_family(state, &row.id).await? {
        return Ok(false);
    }
    let txs: Vec<OutboundTxRow> = all
        .into_iter()
        .filter(|tx| tx.status().is_ok_and(|s| s.is_live()))
        .collect();
    let mut observations = Vec::with_capacity(txs.len());
    for tx in &txs {
        observations.push(observe(state, tx).await?);
    }
    if !observations.iter().all(|o| matches!(o, Observation::Dead(_))) {
        return Ok(false);
    }
    for (tx, observation) in txs.iter().zip(&observations) {
        if let Observation::Dead(reason) = observation {
            close_dead(state, tx, reason).await?;
        }
    }
    let Some(row) = transition(state, row, WithdrawalEvent::Abandon, &WithdrawalUpdate {
        failure_reason: Some("abandoned".into()),
        ..Default::default()
    })
    .await?
    else {
        return Ok(false);
    };
    audit::record(state, actor, "withdrawal.abandoned", Some(("withdrawal", &row.id)), json!({})).await;
    notify_withdrawal(state, &row, WebhookEventType::WithdrawalFailed).await;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pending(stuck: bool) -> Observation {
        Observation::Pending { stuck, resend: false }
    }

    fn landed(success: bool, confirmations: i32) -> Observation {
        Observation::Landed { success, block: 100, confirmations, fee: 21 }
    }

    #[test]
    fn a_landed_transaction_settles_its_subject() {
        // Bump chain: the original landed, the replacement did not.
        assert_eq!(
            decide(&[landed(true, 12), pending(false)], 12),
            TrackingDecision::Landed { landed: 0, success: true, confirmations: 12, final_: true }
        );
        // Not deep enough yet.
        assert_eq!(
            decide(&[pending(false), landed(true, 3)], 12),
            TrackingDecision::Landed { landed: 1, success: true, confirmations: 3, final_: false }
        );
        // A revert is final at once.
        assert_eq!(
            decide(&[landed(false, 1)], 12),
            TrackingDecision::Landed { landed: 0, success: false, confirmations: 1, final_: true }
        );
    }

    #[test]
    fn dead_transactions_are_replaced_only_when_all_are_dead() {
        assert_eq!(decide(&[Observation::Dead("expired")], 19), TrackingDecision::AllDead);
        assert_eq!(
            decide(&[Observation::Dead("expired"), pending(false)], 19),
            TrackingDecision::Pending { dead: vec![0], bump: false, resend: false }
        );
    }

    #[test]
    fn escalation_keeps_the_live_inputs_and_takes_the_newer_fees() {
        let mut latest = eth::tests::stored("0200", 7, 100);
        latest.fee_params = json!({ "feeRate": 10, "inputs": [{ "txid": "aa", "vout": 0 }] });
        latest.fee_raw = Some(db::RawAmount(1_000));
        let mut refused = eth::tests::stored("0201", 7, 100);
        refused.id = "tx-2".into();
        refused.fee_params = json!({ "feeRate": 13, "inputs": [{ "txid": "aa", "vout": 0 }, { "txid": "bb", "vout": 1 }] });
        refused.fee_raw = Some(db::RawAmount(1_700));
        let basis = escalation_basis(&latest, &refused);
        assert_eq!(basis.id, latest.id);
        assert_eq!(basis.fee_params["inputs"], latest.fee_params["inputs"], "the released extra input is not reused");
        assert_eq!(basis.fee_params["feeRate"], 13);
        assert_eq!(basis.fee_raw, Some(db::RawAmount(1_700)));
    }

    #[test]
    fn only_the_latest_transaction_is_bumped_or_resent() {
        assert_eq!(
            decide(&[pending(true), pending(false)], 12),
            TrackingDecision::Pending { dead: vec![], bump: false, resend: false }
        );
        assert_eq!(
            decide(&[pending(false), Observation::Pending { stuck: true, resend: true }], 12),
            TrackingDecision::Pending { dead: vec![], bump: true, resend: true }
        );
    }
}
