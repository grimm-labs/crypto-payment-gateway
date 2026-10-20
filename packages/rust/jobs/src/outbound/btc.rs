//! Bitcoin: withdrawals spend the treasury's change and the confirmed
//! UTXOs of final deposit addresses directly (no sweeps), RBF-enabled; a
//! stuck one is replaced with the same inputs (BIP125).

use anyhow::{Context, Result};
use chain::btc_tx::{
    build_and_sign, select_replacement, BtcBroadcastOutcome, CoinSelectionError, EsploraRpc, SpendableUtxo,
};
use chain::destination::parse_bitcoin_address;
use config::{
    BTC_FEE_TARGET_BLOCKS, BTC_MAX_FEE_RATE_SAT_VB, BTC_MAX_INPUTS, BTC_MIN_FEE_RATE_SAT_VB, BTC_STUCK_BLOCKS,
    FEE_BUMP_PERCENT,
};
use db::outbound::{insert_outbound_tx, lock_sender, reserve_utxos};
use db::{NewOutboundTx, OutboundTxRow, WithdrawalClaim};
use domain::{Network, OutboundPurpose, Token, WithdrawalStatus};
use serde::{Deserialize, Serialize};
use serde_json::json;
use wallet::KeyRef;

use super::{BroadcastResult, Observation, SignResult};
use crate::AppState;

/// Every Bitcoin spend allocates from one UTXO pool: one lock for all.
const UTXO_POOL_LOCK: &str = "utxo-pool";

/// An input as stored with its transaction (for replacements and expiry
/// proofs).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredInput {
    pub txid: String,
    pub vout: u32,
    pub value: u64,
    pub address: String,
    /// `None` = the treasury.
    pub deposit_index: Option<u32>,
    pub height: u64,
}

impl StoredInput {
    fn from_utxo(utxo: &SpendableUtxo) -> Self {
        Self {
            txid: utxo.txid.clone(),
            vout: utxo.vout,
            value: utxo.value,
            address: utxo.address.clone(),
            deposit_index: match utxo.key {
                KeyRef::Deposit { index, .. } => Some(index),
                KeyRef::Treasury(_) => None,
            },
            height: utxo.height,
        }
    }

    fn to_utxo(&self) -> SpendableUtxo {
        SpendableUtxo {
            txid: self.txid.clone(),
            vout: self.vout,
            value: self.value,
            address: self.address.clone(),
            key: match self.deposit_index {
                Some(index) => KeyRef::Deposit { network: Network::Bitcoin, index },
                None => KeyRef::Treasury(Network::Bitcoin),
            },
            height: self.height,
        }
    }
}

fn stored_inputs(tx: &OutboundTxRow) -> Vec<StoredInput> {
    serde_json::from_value(tx.fee_params["inputs"].clone()).unwrap_or_default()
}

/// Sign a withdrawal (or an RBF replacement of `replaces`).
pub async fn sign_withdrawal(
    state: &AppState,
    rpc: &dyn EsploraRpc,
    claim: &mut WithdrawalClaim,
    replaces: Option<&OutboundTxRow>,
) -> Result<SignResult> {
    let row = claim.row().clone();
    let amount = u64::try_from(row.amount_raw.0).context("amount out of range")?;
    let testnet = state.config.chain.is_testnet();
    let destination = match parse_bitcoin_address(&row.destination, testnet) {
        Ok(address) => address,
        Err(_) => return Ok(SignResult::Fail("destination_invalid")),
    };
    let change = parse_bitcoin_address(&state.wallet.treasury_address(Network::Bitcoin)?, testnet)
        .map_err(|e| anyhow::anyhow!("treasury address: {e}"))?;
    lock_sender(claim.conn(), Network::Bitcoin, UTXO_POOL_LOCK).await?;
    let tip = rpc.tip_height().await?;
    let market = rpc
        .fee_rate(BTC_FEE_TARGET_BLOCKS, BTC_MIN_FEE_RATE_SAT_VB, BTC_MAX_FEE_RATE_SAT_VB)
        .await?;
    let spendable = crate::treasury::spendable_btc_utxos(state, rpc).await?;
    let script_len = destination.script_pubkey().len();
    let (selection, fee_rate) = match replaces {
        None => match chain::btc_tx::select_coins(spendable, amount, market, script_len, BTC_MAX_INPUTS) {
            Ok(selection) => (selection, market),
            // Change of an unconfirmed spend not usable yet: wait for it.
            Err(CoinSelectionError::Insufficient { .. }) => {
                return Ok(SignResult::Wait("not enough confirmed spendable UTXOs yet".into()))
            }
            Err(err @ CoinSelectionError::TooManyInputs(_)) => return Ok(SignResult::Wait(err.to_string())),
        },
        Some(previous) => {
            let previous_rate = previous.fee_params["feeRate"].as_u64().unwrap_or(market);
            let rate = market.max(previous_rate.saturating_mul(100 + FEE_BUMP_PERCENT as u64).div_ceil(100));
            let inputs: Vec<SpendableUtxo> = stored_inputs(previous).iter().map(StoredInput::to_utxo).collect();
            let previous_fee = previous.fee_raw.map(|f| f.0).unwrap_or(0);
            match select_replacement(
                inputs,
                spendable,
                amount,
                u64::try_from(previous_fee).unwrap_or(u64::MAX),
                rate,
                script_len,
                BTC_MAX_INPUTS,
            ) {
                Ok(selection) => (selection, rate),
                Err(err) => return Ok(SignResult::Wait(format!("no replacement possible yet: {err}"))),
            }
        }
    };
    let cap = row.fee_cap_raw.map(|c| c.0).unwrap_or(u128::MAX);
    if u128::from(selection.fee) > cap {
        return Ok(if replaces.is_some() || row.status()? != WithdrawalStatus::Queued {
            SignResult::Stall
        } else {
            SignResult::Wait(format!("network fee {} sats above the fee cap {cap}", selection.fee))
        });
    }
    let signed = build_and_sign(&selection, &destination, amount, &change, &state.wallet)?;
    let inputs: Vec<StoredInput> = selection.inputs.iter().map(StoredInput::from_utxo).collect();
    let tx = insert_outbound_tx(
        claim.conn(),
        &NewOutboundTx {
            purpose: OutboundPurpose::Withdrawal,
            withdrawal_id: Some(&row.id),
            sweep_id: None,
            network: Network::Bitcoin,
            token: Token::Btc,
            from_address: &state.wallet.treasury_address(Network::Bitcoin)?,
            to_address: &row.destination,
            amount_raw: u128::from(amount),
            nonce: None,
            tx_hash: &signed.txid,
            signed_tx: &signed.hex,
            fee_params: json!({
                "feeRate": fee_rate,
                "vsize": signed.vsize,
                "change": selection.change,
                "sentAtHeight": tip,
                "inputs": inputs,
            }),
            fee_raw: Some(u128::from(signed.fee)),
            replaces_id: replaces.map(|r| r.id.as_str()),
            expires_at: None,
        },
    )
    .await?;
    // A replacement takes over its inputs' reservations.
    let outpoints: Vec<(String, u32)> = inputs.iter().map(|i| (i.txid.clone(), i.vout)).collect();
    reserve_utxos(claim.conn(), &tx.id, &outpoints).await?;
    Ok(SignResult::Signed(Box::new(tx)))
}

/// Before `closed`'s reservations are released, a live sibling (same
/// withdrawal) reserves the inputs it spends again.
pub(crate) async fn restore_sibling_reservations(state: &AppState, closed: &OutboundTxRow) -> Result<()> {
    let Some(withdrawal_id) = closed.withdrawal_id.as_deref() else {
        return Ok(());
    };
    for sibling in state.db.outbound_txs_for_withdrawal(withdrawal_id).await? {
        if sibling.id == closed.id || !sibling.status()?.is_live() {
            continue;
        }
        reserve_inputs(state, &sibling).await?;
    }
    Ok(())
}

/// Reserve `tx`'s stored inputs for it (an upsert: taken over from
/// whichever transaction held them).
pub(crate) async fn reserve_inputs(state: &AppState, tx: &OutboundTxRow) -> Result<()> {
    let outpoints: Vec<(String, u32)> = stored_inputs(tx).iter().map(|i| (i.txid.clone(), i.vout)).collect();
    state.db.reserve_utxos_for(&tx.id, &outpoints).await
}

pub async fn broadcast(rpc: &dyn EsploraRpc, tx: &OutboundTxRow) -> Result<BroadcastResult> {
    Ok(match rpc.broadcast(&tx.signed_tx).await? {
        BtcBroadcastOutcome::Accepted => BroadcastResult::Accepted,
        // Our own transaction already in the chain spends them too.
        BtcBroadcastOutcome::InputsSpent if rpc.tx_status(&tx.tx_hash).await?.is_some() => BroadcastResult::Accepted,
        BtcBroadcastOutcome::InputsSpent => BroadcastResult::Dead("inputs_spent"),
        BtcBroadcastOutcome::Rejected(detail) => BroadcastResult::Rejected(detail),
    })
}

/// Confirmed (landed), pending (stuck after 3 blocks), unknown (re-sent),
/// or provably dead: one of its inputs is spent by another transaction.
pub async fn observe(rpc: &dyn EsploraRpc, tx: &OutboundTxRow) -> Result<Observation> {
    let tip = rpc.tip_height().await?;
    let fee = tx.fee_raw.map(|f| f.0).unwrap_or(0);
    match rpc.tx_status(&tx.tx_hash).await? {
        Some(status) if status.confirmed => {
            let height = status.block_height.unwrap_or(tip);
            Ok(Observation::Landed {
                success: true,
                block: height,
                confirmations: chain::confirmations_between(tip, height),
                fee,
            })
        }
        Some(_) => Ok(Observation::Pending { stuck: stuck_since_sent(tx, tip), resend: false }),
        None => {
            for input in stored_inputs(tx) {
                if let Some(spender) = rpc.spent_by(&input.txid, input.vout).await? {
                    if spender != tx.tx_hash {
                        return Ok(Observation::Dead("inputs_spent"));
                    }
                }
            }
            // Dropped from the mempool (fee floor in a congestion) or never
            // accepted: the same bytes go out again, and the wait counts
            // from the first send, so the fee bump runs instead of the
            // same bytes forever.
            Ok(Observation::Pending {
                stuck: stuck_since_sent(tx, tip),
                resend: tx.status()? == domain::OutboundTxStatus::Broadcast,
            })
        }
    }
}

/// Waiting `BTC_STUCK_BLOCKS` blocks since the first send, whether the
/// transaction sits in the mempool or fell out of it.
fn stuck_since_sent(tx: &OutboundTxRow, tip: u64) -> bool {
    let sent_at = tx.fee_params["sentAtHeight"].as_u64().unwrap_or(tip);
    tip.saturating_sub(sent_at) >= BTC_STUCK_BLOCKS
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use chain::btc_tx::{BtcTxStatus, EsploraUtxo};

    struct FakeEsplora {
        status: Option<BtcTxStatus>,
        spender: Option<String>,
        sent: std::sync::Mutex<Vec<String>>,
    }

    impl FakeEsplora {
        fn with(status: Option<BtcTxStatus>, spender: Option<String>) -> Self {
            Self { status, spender, sent: Default::default() }
        }
    }

    #[async_trait]
    impl EsploraRpc for FakeEsplora {
        async fn tip_height(&self) -> Result<u64> {
            Ok(1_003)
        }
        async fn utxos(&self, _: &str) -> Result<Vec<EsploraUtxo>> {
            unimplemented!()
        }
        async fn fee_rate(&self, _: u32, _: u64, _: u64) -> Result<u64> {
            unimplemented!()
        }
        async fn broadcast(&self, hex: &str) -> Result<BtcBroadcastOutcome> {
            self.sent.lock().unwrap().push(hex.to_string());
            Ok(BtcBroadcastOutcome::Accepted)
        }
        async fn tx_status(&self, _: &str) -> Result<Option<BtcTxStatus>> {
            Ok(self.status)
        }
        async fn spent_by(&self, _: &str, _: u32) -> Result<Option<String>> {
            Ok(self.spender.clone())
        }
    }

    fn tx() -> OutboundTxRow {
        tx_sent_at(1_000)
    }

    fn tx_sent_at(height: u64) -> OutboundTxRow {
        let mut tx = super::super::eth::tests::stored("0200", 0, 0);
        tx.network = "BITCOIN".into();
        tx.tx_hash = "ours".into();
        tx.fee_raw = Some(db::RawAmount(700));
        tx.fee_params = json!({
            "sentAtHeight": height,
            "inputs": [{ "txid": "aa", "vout": 0, "value": 5000, "address": "bc1q", "deposit_index": 3, "height": 900 }],
        });
        tx
    }

    /// Invariant 1: a crash between signing and broadcasting re-sends the
    /// stored bytes, never a different transaction.
    #[tokio::test]
    async fn a_stored_transaction_is_rebroadcast_byte_for_byte() {
        let rpc = FakeEsplora::with(None, None);
        assert_eq!(broadcast(&rpc, &tx()).await.unwrap(), BroadcastResult::Accepted);
        assert_eq!(rpc.sent.lock().unwrap().as_slice(), &["0200".to_string()]);
    }

    #[tokio::test]
    async fn a_transaction_whose_input_is_spent_elsewhere_is_dead() {
        let double_spent = FakeEsplora::with(None, Some("theirs".into()));
        assert_eq!(observe(&double_spent, &tx()).await.unwrap(), Observation::Dead("inputs_spent"));
        let dropped = FakeEsplora::with(None, None);
        assert_eq!(observe(&dropped, &tx_sent_at(1_002)).await.unwrap(), Observation::Pending { stuck: false, resend: true });
    }

    /// A transaction the mempool dropped (fee floor in a congestion) is
    /// re-sent, and escalates like one still waiting: after three blocks
    /// it is stuck, so the fee bump runs instead of the same bytes forever.
    #[tokio::test]
    async fn dropped_transactions_are_stuck_after_three_blocks_too() {
        let dropped = FakeEsplora::with(None, None);
        assert_eq!(observe(&dropped, &tx_sent_at(1_000)).await.unwrap(), Observation::Pending { stuck: true, resend: true });
    }

    #[tokio::test]
    async fn mempool_transactions_are_stuck_after_three_blocks() {
        let mempool = FakeEsplora::with(Some(BtcTxStatus { confirmed: false, block_height: None }), None);
        assert_eq!(observe(&mempool, &tx()).await.unwrap(), Observation::Pending { stuck: true, resend: false });
        let confirmed = FakeEsplora::with(Some(BtcTxStatus { confirmed: true, block_height: Some(1_001) }), None);
        assert_eq!(
            observe(&confirmed, &tx()).await.unwrap(),
            Observation::Landed { success: true, block: 1_001, confirmations: 3, fee: 700 }
        );
    }

    #[test]
    fn stored_inputs_keep_their_key() {
        let inputs = stored_inputs(&tx());
        assert_eq!(inputs[0].to_utxo().key, KeyRef::Deposit { network: Network::Bitcoin, index: 3 });
        let treasury = StoredInput { deposit_index: None, ..inputs[0].clone() };
        assert_eq!(treasury.to_utxo().key, KeyRef::Treasury(Network::Bitcoin));
        assert_eq!(StoredInput::from_utxo(&inputs[0].to_utxo()), inputs[0]);
    }
}
