//! Tron: USDT and TRX withdrawals from the treasury, and the transactions
//! of sweeps and TRX top-ups. Every node-built transaction is decoded and
//! checked against the request before it is signed
//! (`chain::tron_tx::verify_tron_transaction`).

use anyhow::{Context, Result};
use chain::tron_tx::{sign_tron, verify_tron_transaction, TronBroadcastOutcome, TronRpc, TronTransfer};
use chrono::{DateTime, Utc};
use db::outbound::{insert_outbound_tx, lock_sender};
use db::{NewOutboundTx, OutboundTxRow, WithdrawalClaim};
use domain::{Network, OutboundPurpose, Token, WithdrawalStatus};
use serde_json::json;
use sqlx::PgConnection;
use tracing::error;
use wallet::KeyRef;

use super::{BroadcastResult, Observation, SignResult};
use crate::fees::plan_tron;
use crate::AppState;

/// After its TAPOS expiration, a transaction the node still knows but that
/// never solidified cannot be proven dead: STUCK after this long.
const UNPROVABLE_AFTER_SECS: i64 = 10 * 60;
/// Margin past the expiration before a transaction is called expired.
const EXPIRY_MARGIN_SECS: i64 = 60;

/// One Tron transaction to build (by the node), verify, sign and store.
pub(crate) struct TronSend<'a> {
    pub key: KeyRef,
    pub transfer: TronTransfer,
    pub purpose: OutboundPurpose,
    pub withdrawal_id: Option<&'a str>,
    pub sweep_id: Option<&'a str>,
    pub token: Token,
    /// Most it can cost (fee limit + bandwidth + activation), sun.
    pub max_fee: u128,
}

/// `Err(reason)` when the node built something else than asked — never
/// signed (a misbehaving or compromised provider), logged as an error.
pub(crate) async fn sign_and_store(
    state: &AppState,
    rpc: &dyn TronRpc,
    conn: &mut PgConnection,
    send: TronSend<'_>,
) -> Result<Result<OutboundTxRow, String>> {
    let (owner, recipient, amount) = match &send.transfer {
        TronTransfer::Trc20 { owner, recipient, amount, .. } => (owner.clone(), recipient.clone(), *amount),
        TronTransfer::Trx { owner, recipient, amount } => (owner.clone(), recipient.clone(), u128::from(*amount)),
    };
    lock_sender(conn, Network::Tron, &owner).await?;
    let unsigned = match &send.transfer {
        TronTransfer::Trc20 { owner, contract, recipient, amount, fee_limit } => {
            rpc.build_trc20_transfer(owner, contract, recipient, *amount, *fee_limit).await?
        }
        TronTransfer::Trx { owner, recipient, amount } => rpc.build_trx_transfer(owner, recipient, *amount).await?,
    };
    let verified = match verify_tron_transaction(&unsigned.raw_data_hex, unsigned.tx_id.as_deref(), &send.transfer, Utc::now().timestamp_millis()) {
        Ok(verified) => verified,
        Err(err) => {
            error!("TronGrid built a transaction that differs from the request — NOT signed: {err}");
            return Ok(Err(format!("node-built transaction refused: {err}")));
        }
    };
    let signed = sign_tron(&verified, &state.wallet, send.key)?;
    let row = insert_outbound_tx(
        conn,
        &NewOutboundTx {
            purpose: send.purpose,
            withdrawal_id: send.withdrawal_id,
            sweep_id: send.sweep_id,
            network: Network::Tron,
            token: send.token,
            from_address: &owner,
            to_address: &recipient,
            amount_raw: amount,
            nonce: None,
            tx_hash: &verified.tx_id,
            signed_tx: &hex::encode(&signed),
            fee_params: json!({ "feeLimit": verified.fee_limit, "expirationMs": verified.expiration_ms }),
            fee_raw: Some(send.max_fee),
            replaces_id: None,
            expires_at: DateTime::from_timestamp_millis(verified.expiration_ms),
        },
    )
    .await?;
    Ok(Ok(row))
}

/// Sign a withdrawal from the treasury (first signing, or again once its
/// previous transaction provably expired — Tron is never bumped).
pub async fn sign_withdrawal(state: &AppState, rpc: &dyn TronRpc, claim: &mut WithdrawalClaim) -> Result<SignResult> {
    let row = claim.row().clone();
    let token = row.token()?;
    let amount = row.amount_raw.0;
    let treasury = state.wallet.treasury_address(Network::Tron)?;
    let usdt = state.config.chain.usdt_tron.clone();
    let plan = plan_tron(rpc, &treasury, token, &usdt, &row.destination, amount).await?;
    let cap = row.fee_cap_raw.map(|c| c.0).unwrap_or(u128::MAX);
    if plan.max_fee() > cap {
        return Ok(if row.status()? == WithdrawalStatus::Queued {
            SignResult::Wait(format!("network fee {} sun above the fee cap {cap}", plan.max_fee()))
        } else {
            SignResult::Stall
        });
    }
    let Some(trx) = rpc.account_balance(&treasury).await? else {
        return Ok(SignResult::Fail("insufficient_funds"));
    };
    let transfer = match token {
        Token::Usdt => {
            if rpc.solidity_trc20_balance(&usdt, &treasury).await? < amount || trx < plan.max_fee() {
                return Ok(SignResult::Fail("insufficient_funds"));
            }
            TronTransfer::Trc20 {
                owner: treasury.clone(),
                contract: usdt,
                recipient: row.destination.clone(),
                amount,
                fee_limit: plan.fee_limit,
            }
        }
        Token::Trx => {
            if trx < amount.saturating_add(plan.max_fee()) {
                return Ok(SignResult::Fail("insufficient_funds"));
            }
            TronTransfer::Trx {
                owner: treasury.clone(),
                recipient: row.destination.clone(),
                amount: u64::try_from(amount).context("TRX amount out of range")?,
            }
        }
        other => anyhow::bail!("{} is not sent on tron", other.as_api()),
    };
    let send = TronSend {
        key: KeyRef::Treasury(Network::Tron),
        transfer,
        purpose: OutboundPurpose::Withdrawal,
        withdrawal_id: Some(&row.id),
        sweep_id: None,
        token,
        max_fee: plan.max_fee(),
    };
    Ok(match sign_and_store(state, rpc, claim.conn(), send).await? {
        Ok(tx) => SignResult::Signed(Box::new(tx)),
        Err(reason) => SignResult::Wait(reason),
    })
}

pub async fn broadcast(rpc: &dyn TronRpc, tx: &OutboundTxRow) -> Result<BroadcastResult> {
    let signed = hex::decode(&tx.signed_tx).context("stored transaction is not hex")?;
    Ok(match rpc.broadcast(&signed).await? {
        TronBroadcastOutcome::Accepted => BroadcastResult::Accepted,
        TronBroadcastOutcome::Expired => BroadcastResult::Dead("expired"),
        TronBroadcastOutcome::Rejected(detail) => BroadcastResult::Rejected(detail),
    })
}

/// Solidified (landed), pending, or provably expired: TAPOS expiration
/// passed *and* the full node does not know it.
pub async fn observe(rpc: &dyn TronRpc, tx: &OutboundTxRow) -> Result<Observation> {
    if let Some(outcome) = rpc.solidified_outcome(&tx.tx_hash).await? {
        let head = rpc.head_block().await?;
        return Ok(Observation::Landed {
            success: outcome.success,
            block: outcome.block,
            confirmations: chain::confirmations_between(head, outcome.block),
            fee: outcome.fee_sun,
        });
    }
    let now = Utc::now();
    let Some(expires_at) = tx.expires_at else {
        return Ok(Observation::Pending { stuck: false, resend: false });
    };
    if now <= expires_at + chrono::Duration::seconds(EXPIRY_MARGIN_SECS) {
        return Ok(Observation::Pending { stuck: false, resend: false });
    }
    if !rpc.is_known(&tx.tx_hash).await? {
        return Ok(Observation::Dead("expired"));
    }
    Ok(Observation::Pending {
        stuck: now > expires_at + chrono::Duration::seconds(UNPROVABLE_AFTER_SECS),
        resend: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use chain::tron_tx::{TronChainFees, TronTxOutcome, TronUnsignedTx};

    #[derive(Default)]
    struct FakeTron {
        outcome: Option<TronTxOutcome>,
        known: bool,
        sent: std::sync::Mutex<Vec<Vec<u8>>>,
    }

    #[async_trait]
    impl TronRpc for FakeTron {
        async fn head_block(&self) -> Result<u64> {
            Ok(1_018)
        }
        async fn chain_fees(&self) -> Result<TronChainFees> {
            unimplemented!()
        }
        async fn estimate_trc20_energy(&self, _: &str, _: &str, _: &str, _: u128) -> Result<u64> {
            unimplemented!()
        }
        async fn build_trc20_transfer(&self, _: &str, _: &str, _: &str, _: u128, _: u64) -> Result<TronUnsignedTx> {
            unimplemented!()
        }
        async fn build_trx_transfer(&self, _: &str, _: &str, _: u64) -> Result<TronUnsignedTx> {
            unimplemented!()
        }
        async fn account_balance(&self, _: &str) -> Result<Option<u128>> {
            unimplemented!()
        }
        async fn solidity_trx_balance(&self, _: &str) -> Result<u128> {
            unimplemented!()
        }
        async fn solidity_trc20_balance(&self, _: &str, _: &str) -> Result<u128> {
            unimplemented!()
        }
        async fn broadcast(&self, signed: &[u8]) -> Result<TronBroadcastOutcome> {
            self.sent.lock().unwrap().push(signed.to_vec());
            Ok(TronBroadcastOutcome::Accepted)
        }
        async fn solidified_outcome(&self, _: &str) -> Result<Option<TronTxOutcome>> {
            Ok(self.outcome)
        }
        async fn is_known(&self, _: &str) -> Result<bool> {
            Ok(self.known)
        }
    }

    /// Invariant 1: a crash between signing and broadcasting re-sends the
    /// stored bytes, never a different transaction.
    #[tokio::test]
    async fn a_stored_transaction_is_rebroadcast_byte_for_byte() {
        let rpc = FakeTron::default();
        let mut tx = super::super::eth::tests::stored("0a02cafe", 0, 0);
        tx.network = "TRON".into();
        assert_eq!(broadcast(&rpc, &tx).await.unwrap(), BroadcastResult::Accepted);
        assert_eq!(rpc.sent.lock().unwrap().as_slice(), &[hex::decode("0a02cafe").unwrap()]);
    }

    fn tx(expired_secs_ago: i64) -> OutboundTxRow {
        let mut tx = super::super::eth::tests::stored("0a02", 0, 0);
        tx.network = "TRON".into();
        tx.nonce = None;
        tx.expires_at = Some(Utc::now() - chrono::Duration::seconds(expired_secs_ago));
        tx
    }

    #[tokio::test]
    async fn expiry_is_only_proven_when_the_node_does_not_know_the_transaction() {
        // Not expired yet.
        assert_eq!(observe(&FakeTron::default(), &tx(-30)).await.unwrap(), Observation::Pending { stuck: false, resend: false });
        // Expired and unknown: provably dead, it may be signed again.
        assert_eq!(observe(&FakeTron::default(), &tx(120)).await.unwrap(), Observation::Dead("expired"));
        // Expired but still known: not provable yet…
        let known = FakeTron { known: true, ..Default::default() };
        assert_eq!(observe(&known, &tx(120)).await.unwrap(), Observation::Pending { stuck: false, resend: false });
        // …and STUCK once that lasts.
        assert_eq!(observe(&known, &tx(11 * 60)).await.unwrap(), Observation::Pending { stuck: true, resend: false });
    }

    #[tokio::test]
    async fn solidified_transactions_report_their_outcome() {
        let solid = FakeTron { outcome: Some(TronTxOutcome { block: 1_000, success: true, fee_sun: 6_773_500 }), ..Default::default() };
        assert_eq!(
            observe(&solid, &tx(120)).await.unwrap(),
            Observation::Landed { success: true, block: 1_000, confirmations: 19, fee: 6_773_500 }
        );
    }
}
