//! Ethereum: ETH and USDT withdrawals from the treasury, and the
//! transactions of sweeps (signed by deposit keys) and gas top-ups.

use anyhow::{Context, Result};
use chain::evm::{sign_eip1559, BlockTag, EvmFees, EvmRpc, EvmSendOutcome, EvmTxRequest};
use config::{ETH_MAX_IN_FLIGHT_PER_ADDRESS, ETH_NONCE_USED_DEPTH, ETH_STUCK_BLOCKS, FEE_BUMP_PERCENT};
use db::outbound::{allocate_nonce, in_flight_count, insert_outbound_tx, lock_sender};
use db::{NewOutboundTx, OutboundTxRow, WithdrawalClaim};
use domain::{Network, OutboundPurpose, Token, WithdrawalStatus};
use serde_json::json;
use sqlx::PgConnection;
use wallet::KeyRef;

use super::{BroadcastResult, Observation, SignResult};
use crate::fees::{plan_eth, EthPlan};
use crate::AppState;

/// One Ethereum transaction to number, sign and store.
pub(crate) struct EthSend<'a> {
    pub key: KeyRef,
    pub from: &'a str,
    pub purpose: OutboundPurpose,
    pub withdrawal_id: Option<&'a str>,
    pub sweep_id: Option<&'a str>,
    pub token: Token,
    /// Who receives the value (not the token contract).
    pub recipient: &'a str,
    pub amount: u128,
    pub plan: EthPlan,
    /// A fee bump keeps the nonce of the transaction it replaces.
    pub replaces: Option<&'a OutboundTxRow>,
}

/// Number (a fresh nonce under the sender lock, or the replaced one), sign
/// and store as SIGNED. `Err(reason)` when it must wait (too many in
/// flight).
pub(crate) async fn sign_and_store(
    state: &AppState,
    rpc: &dyn EvmRpc,
    conn: &mut PgConnection,
    send: EthSend<'_>,
) -> Result<Result<OutboundTxRow, String>> {
    lock_sender(conn, Network::Ethereum, send.from).await?;
    let nonce = match send.replaces {
        Some(previous) => u64::try_from(previous.nonce.context("replaced transaction has no nonce")?)?,
        None => {
            let in_flight = in_flight_count(conn, Network::Ethereum, send.from).await?;
            if in_flight >= ETH_MAX_IN_FLIGHT_PER_ADDRESS {
                return Ok(Err(format!("{in_flight} transactions already in flight from {}", send.from)));
            }
            // Nothing in flight: the chain is the source of truth again.
            let resync = if in_flight == 0 { Some(rpc.pending_nonce(send.from).await?) } else { None };
            allocate_nonce(conn, Network::Ethereum, send.from, resync).await?
        }
    };
    let tip = rpc.block_number().await?;
    let request = EvmTxRequest {
        chain_id: state.config.chain.eth_chain_id,
        nonce,
        to: send.plan.to.clone(),
        value: send.plan.value,
        data: send.plan.data.clone(),
        gas_limit: send.plan.gas_limit,
        fees: send.plan.fees,
    };
    let signed = sign_eip1559(&request, &state.wallet, send.key)?;
    let row = insert_outbound_tx(
        conn,
        &NewOutboundTx {
            purpose: send.purpose,
            withdrawal_id: send.withdrawal_id,
            sweep_id: send.sweep_id,
            network: Network::Ethereum,
            token: send.token,
            from_address: send.from,
            to_address: send.recipient,
            amount_raw: send.amount,
            nonce: Some(nonce),
            tx_hash: &signed.hash,
            signed_tx: &hex::encode(&signed.raw),
            fee_params: json!({
                "maxFeePerGas": send.plan.fees.max_fee_per_gas.to_string(),
                "maxPriorityFeePerGas": send.plan.fees.max_priority_fee_per_gas.to_string(),
                "gasLimit": send.plan.gas_limit,
                "sentAtBlock": tip,
            }),
            fee_raw: Some(send.plan.max_fee()),
            replaces_id: send.replaces.map(|r| r.id.as_str()),
            expires_at: None,
        },
    )
    .await?;
    Ok(Ok(row))
}

/// The fee fields a stored transaction was signed with.
pub(crate) fn signed_fees(tx: &OutboundTxRow) -> Option<EvmFees> {
    let field = |name: &str| tx.fee_params[name].as_str().and_then(|v| v.parse::<u128>().ok());
    Some(EvmFees {
        max_fee_per_gas: field("maxFeePerGas")?,
        max_priority_fee_per_gas: field("maxPriorityFeePerGas")?,
    })
}

/// The fees of a replacement: +25 % on what `previous` paid, and never
/// below what the network asks now.
pub fn replacement_fees(previous: EvmFees, current: EvmFees) -> EvmFees {
    let bumped = previous.bumped(FEE_BUMP_PERCENT);
    EvmFees {
        max_fee_per_gas: bumped.max_fee_per_gas.max(current.max_fee_per_gas),
        max_priority_fee_per_gas: bumped.max_priority_fee_per_gas.max(current.max_priority_fee_per_gas),
    }
}

/// Sign a withdrawal from the treasury (or a fee bump of `replaces`).
pub async fn sign_withdrawal(
    state: &AppState,
    rpc: &dyn EvmRpc,
    claim: &mut WithdrawalClaim,
    replaces: Option<&OutboundTxRow>,
) -> Result<SignResult> {
    let row = claim.row().clone();
    let token = row.token()?;
    let amount = row.amount_raw.0;
    let treasury = state.wallet.treasury_address(Network::Ethereum)?;
    let usdt = state.config.chain.usdt_eth.clone();
    let mut plan = plan_eth(rpc, &treasury, token, &usdt, &row.destination, amount).await?;
    if let Some(previous) = replaces {
        let old = signed_fees(previous).context("replaced transaction has no fee fields")?;
        plan.fees = replacement_fees(old, plan.fees);
        if let Some(gas) = previous.fee_params["gasLimit"].as_u64() {
            plan.gas_limit = plan.gas_limit.max(gas);
        }
    }
    let cap = row.fee_cap_raw.map(|c| c.0).unwrap_or(u128::MAX);
    if plan.max_fee() > cap {
        return Ok(if replaces.is_some() || row.status()? != WithdrawalStatus::Queued {
            SignResult::Stall
        } else {
            SignResult::Wait(format!("network fee {} wei above the fee cap {cap}", plan.max_fee()))
        });
    }
    if replaces.is_none() {
        // The treasury must hold the amount and the gas at the tip.
        let eth = rpc.eth_balance(&treasury, BlockTag::Latest).await?;
        if eth < plan.max_fee().saturating_add(plan.value) {
            return Ok(SignResult::Fail("insufficient_funds"));
        }
        if token == Token::Usdt && rpc.erc20_balance(&usdt, &treasury, BlockTag::Latest).await? < amount {
            return Ok(SignResult::Fail("insufficient_funds"));
        }
    }
    let send = EthSend {
        key: KeyRef::Treasury(Network::Ethereum),
        from: &treasury,
        purpose: OutboundPurpose::Withdrawal,
        withdrawal_id: Some(&row.id),
        sweep_id: None,
        token,
        recipient: &row.destination,
        amount,
        plan,
        replaces,
    };
    Ok(match sign_and_store(state, rpc, claim.conn(), send).await? {
        Ok(tx) => SignResult::Signed(Box::new(tx)),
        Err(reason) => SignResult::Wait(reason),
    })
}

/// Send the stored bytes. "nonce too low" never proves a transaction dead
/// here — it may be this very transaction, mined a moment ago: tracking
/// decides (`observe`), the transaction stays as it is meanwhile.
pub async fn broadcast(rpc: &dyn EvmRpc, tx: &OutboundTxRow) -> Result<BroadcastResult> {
    let raw = hex::decode(&tx.signed_tx).context("stored transaction is not hex")?;
    Ok(match rpc.send_raw_transaction(&raw).await? {
        EvmSendOutcome::Accepted => BroadcastResult::Accepted,
        EvmSendOutcome::NonceTooLow => {
            if rpc.receipt(&tx.tx_hash).await?.is_some() {
                BroadcastResult::Accepted
            } else {
                BroadcastResult::Rejected("nonce too low".into())
            }
        }
        EvmSendOutcome::Rejected(detail) => BroadcastResult::Rejected(detail),
    })
}

/// Mined (receipt), pending, or dead: its nonce was used
/// `ETH_NONCE_USED_DEPTH` blocks deep by something else. The nonce is read
/// *before* the receipt, so a transaction mined between the two calls is
/// seen landed, never dead.
pub async fn observe(rpc: &dyn EvmRpc, tx: &OutboundTxRow) -> Result<Observation> {
    let tip = rpc.block_number().await?;
    let used_deep = rpc
        .mined_nonce(&tx.from_address, BlockTag::Number(tip.saturating_sub(ETH_NONCE_USED_DEPTH)))
        .await?;
    if let Some(receipt) = rpc.receipt(&tx.tx_hash).await? {
        return Ok(Observation::Landed {
            success: receipt.success,
            block: receipt.block_number,
            confirmations: chain::confirmations_between(tip, receipt.block_number),
            fee: receipt.fee_wei,
        });
    }
    let nonce = u64::try_from(tx.nonce.unwrap_or(i64::MAX)).unwrap_or(u64::MAX);
    if nonce < used_deep {
        // Its nonce was used deep by another transaction: this one can
        // never land (a sibling that landed wins in `decide`).
        return Ok(Observation::Dead("nonce_used"));
    }
    let sent_at = tx.fee_params["sentAtBlock"].as_u64().unwrap_or(tip);
    // Accepted once, then dropped from the mempool (pool full, node
    // restart): the same bytes go out again.
    let broadcast = tx.status()? == domain::OutboundTxStatus::Broadcast;
    let resend = broadcast && !rpc.is_known(&tx.tx_hash).await?;
    Ok(Observation::Pending {
        stuck: tx.status()?.is_live() && tip.saturating_sub(sent_at) >= ETH_STUCK_BLOCKS,
        resend,
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use async_trait::async_trait;
    use chain::evm::EvmReceipt;
    use std::sync::Mutex;

    /// An Ethereum node that records what it is sent.
    #[derive(Default)]
    pub(crate) struct FakeEvm {
        pub sent: Mutex<Vec<Vec<u8>>>,
        pub send_outcome: Option<EvmSendOutcome>,
        pub receipt: Option<EvmReceipt>,
        pub tip: u64,
        pub mined_nonce: u64,
        pub nonce_blocks: Mutex<Vec<BlockTag>>,
        pub known: bool,
    }

    #[async_trait]
    impl EvmRpc for FakeEvm {
        async fn block_number(&self) -> Result<u64> {
            Ok(self.tip)
        }
        async fn fee_quote(&self) -> Result<EvmFees> {
            Ok(EvmFees::from_base_fee(10, 1))
        }
        async fn pending_nonce(&self, _: &str) -> Result<u64> {
            Ok(self.mined_nonce)
        }
        async fn mined_nonce(&self, _: &str, block: BlockTag) -> Result<u64> {
            self.nonce_blocks.lock().unwrap().push(block);
            Ok(self.mined_nonce)
        }
        async fn estimate_gas(&self, _: &str, _: &str, _: u128, _: &[u8]) -> Result<u64> {
            Ok(21_000)
        }
        async fn send_raw_transaction(&self, raw: &[u8]) -> Result<EvmSendOutcome> {
            self.sent.lock().unwrap().push(raw.to_vec());
            Ok(self.send_outcome.clone().unwrap_or(EvmSendOutcome::Accepted))
        }
        async fn receipt(&self, _: &str) -> Result<Option<EvmReceipt>> {
            Ok(self.receipt)
        }
        async fn is_known(&self, _: &str) -> Result<bool> {
            Ok(self.known)
        }
        async fn eth_balance(&self, _: &str, _: BlockTag) -> Result<u128> {
            Ok(0)
        }
        async fn erc20_balance(&self, _: &str, _: &str, _: BlockTag) -> Result<u128> {
            Ok(0)
        }
    }

    pub(crate) fn stored(signed_tx: &str, nonce: i64, sent_at_block: u64) -> OutboundTxRow {
        let now = chrono::Utc::now();
        OutboundTxRow {
            id: "tx-1".into(),
            purpose: "WITHDRAWAL".into(),
            withdrawal_id: Some("w-1".into()),
            sweep_id: None,
            network: "ETHEREUM".into(),
            token: "ETH".into(),
            from_address: "0x78839F6054d7ed13918bAe0473BA31b1Ca9D7265".into(),
            to_address: "0x5aAeb6053F3E94C9b9A09f33669435E7Ef1BeAed".into(),
            amount_raw: db::RawAmount(1),
            nonce: Some(nonce),
            tx_hash: "0xabc".into(),
            signed_tx: signed_tx.into(),
            fee_params: json!({ "maxFeePerGas": "21", "maxPriorityFeePerGas": "1", "gasLimit": 21000, "sentAtBlock": sent_at_block }),
            fee_raw: None,
            status: "BROADCAST".into(),
            replaces_id: None,
            expires_at: None,
            block_number: None,
            confirmations: 0,
            last_error: None,
            created_at: now,
            updated_at: now,
            broadcast_at: None,
            confirmed_at: None,
        }
    }

    #[tokio::test]
    async fn a_stored_transaction_is_rebroadcast_byte_for_byte() {
        // Crash between signing and broadcasting: the same bytes go out.
        let rpc = FakeEvm::default();
        let tx = stored("02f86b0103", 7, 100);
        assert_eq!(broadcast(&rpc, &tx).await.unwrap(), BroadcastResult::Accepted);
        assert_eq!(rpc.sent.lock().unwrap().as_slice(), &[hex::decode("02f86b0103").unwrap()]);
    }

    #[tokio::test]
    async fn nonce_too_low_never_proves_a_transaction_dead() {
        let tx = stored("02", 7, 100);
        // Maybe this very transaction, mined a moment ago: tracking decides.
        let no_receipt_yet = FakeEvm { send_outcome: Some(EvmSendOutcome::NonceTooLow), ..Default::default() };
        assert_eq!(broadcast(&no_receipt_yet, &tx).await.unwrap(), BroadcastResult::Rejected("nonce too low".into()));
        let mined = FakeEvm {
            send_outcome: Some(EvmSendOutcome::NonceTooLow),
            receipt: Some(EvmReceipt { success: true, block_number: 90, fee_wei: 1 }),
            ..Default::default()
        };
        assert_eq!(broadcast(&mined, &tx).await.unwrap(), BroadcastResult::Accepted);
    }

    #[tokio::test]
    async fn observation_of_mined_pending_stuck_and_replaced_transactions() {
        let tx = stored("02", 7, 100);
        let mined = FakeEvm { tip: 111, receipt: Some(EvmReceipt { success: true, block_number: 100, fee_wei: 42 }), ..Default::default() };
        assert_eq!(
            observe(&mined, &tx).await.unwrap(),
            Observation::Landed { success: true, block: 100, confirmations: 12, fee: 42 }
        );
        let pending = FakeEvm { tip: 104, mined_nonce: 7, known: true, ..Default::default() };
        assert_eq!(observe(&pending, &tx).await.unwrap(), Observation::Pending { stuck: false, resend: false });
        // Dropped from the mempool after its broadcast: sent again.
        let dropped = FakeEvm { tip: 104, mined_nonce: 7, ..Default::default() };
        assert_eq!(observe(&dropped, &tx).await.unwrap(), Observation::Pending { stuck: false, resend: true });
        let stuck = FakeEvm { tip: 105, mined_nonce: 7, known: true, ..Default::default() };
        assert_eq!(observe(&stuck, &tx).await.unwrap(), Observation::Pending { stuck: true, resend: false });
        // Nonce 7 used deep and no receipt for this hash: it can never land.
        let replaced = FakeEvm { tip: 125, mined_nonce: 8, ..Default::default() };
        assert_eq!(observe(&replaced, &tx).await.unwrap(), Observation::Dead("nonce_used"));
        // The nonce is read 12 blocks deep, before the receipt.
        assert_eq!(replaced.nonce_blocks.lock().unwrap().as_slice(), &[BlockTag::Number(113)]);
    }

    #[test]
    fn replacements_raise_fees_by_at_least_the_bump_and_follow_the_market() {
        let old = EvmFees { max_fee_per_gas: 100, max_priority_fee_per_gas: 10 };
        let calm = EvmFees { max_fee_per_gas: 50, max_priority_fee_per_gas: 1 };
        assert_eq!(replacement_fees(old, calm), EvmFees { max_fee_per_gas: 125, max_priority_fee_per_gas: 13 });
        let busy = EvmFees { max_fee_per_gas: 300, max_priority_fee_per_gas: 20 };
        let next = replacement_fees(old, busy);
        assert_eq!(next, busy);
        assert!(next.replaces(&old));
    }
}
