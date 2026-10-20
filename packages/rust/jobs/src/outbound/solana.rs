//! Solana: USDT and SOL withdrawals from the treasury (its owner pays every
//! fee), and deposit sweeps.

use anyhow::{Context, Result};
use base64::Engine;
use chain::solana_tx::{sign_transfer, SignedSolanaTx, SolanaAccount, SolanaFees, SolanaRpc, SolanaSendOutcome, TransferSpec};
use chrono::Utc;
use db::outbound::{insert_outbound_tx, lock_sender};
use db::{NewOutboundTx, OutboundTxRow, WithdrawalClaim};
use domain::{Network, OutboundPurpose, Token, WithdrawalStatus};
use serde_json::json;
use sqlx::PgConnection;
use wallet::KeyRef;

use super::{BroadcastResult, Observation, SignResult};
use crate::fees::plan_solana;
use crate::withdrawals::SolanaRoute;
use crate::AppState;

/// A recent blockhash stays valid ~150 slots (~60–90 s).
const BLOCKHASH_VALIDITY_SECS: i64 = 90;
/// Blocks past `lastValidBlockHeight` before an unknown signature counts as
/// expired: room for an RPC node lagging behind the one that answered the
/// height.
const EXPIRY_MARGIN_BLOCKS: u64 = 64;

/// One Solana transaction to sign (treasury as fee payer) and store.
pub(crate) struct SolanaSend<'a> {
    pub spec: TransferSpec,
    pub fees: SolanaFees,
    pub purpose: OutboundPurpose,
    pub withdrawal_id: Option<&'a str>,
    pub sweep_id: Option<&'a str>,
    pub token: Token,
    pub from: &'a str,
    pub recipient: &'a str,
    pub amount: u128,
    /// Rent the transaction may pay (a created token account), lamports.
    pub rent: u64,
}

pub(crate) async fn sign_and_store(
    state: &AppState,
    rpc: &dyn SolanaRpc,
    conn: &mut PgConnection,
    send: SolanaSend<'_>,
) -> Result<OutboundTxRow> {
    let treasury = state.wallet.treasury_address(Network::Solana)?;
    lock_sender(conn, Network::Solana, &treasury).await?;
    let blockhash = rpc.latest_blockhash().await?;
    let signed = sign_transfer(
        &send.spec,
        &state.config.chain.usdt_solana_mint,
        send.fees,
        &blockhash.blockhash,
        &state.wallet,
    )?;
    let signatures = if matches!(send.spec, TransferSpec::Sweep { .. }) { 2 } else { 1 };
    let fee = send.fees.max_fee_lamports(signatures);
    let row = insert_outbound_tx(
        conn,
        &NewOutboundTx {
            purpose: send.purpose,
            withdrawal_id: send.withdrawal_id,
            sweep_id: send.sweep_id,
            network: Network::Solana,
            token: send.token,
            from_address: send.from,
            to_address: send.recipient,
            amount_raw: send.amount,
            nonce: None,
            tx_hash: &signed.signature,
            signed_tx: &signed.base64(),
            fee_params: json!({
                "cuLimit": send.fees.cu_limit,
                "cuPrice": send.fees.cu_price,
                "fee": fee,
                "rent": send.rent,
                "lastValidBlockHeight": blockhash.last_valid_block_height,
            }),
            fee_raw: Some(u128::from(fee) + u128::from(send.rent)),
            replaces_id: None,
            expires_at: Some(Utc::now() + chrono::Duration::seconds(BLOCKHASH_VALIDITY_SECS)),
        },
    )
    .await?;
    Ok(row)
}

/// Sign a withdrawal (first signing, or again once its previous
/// transaction's blockhash provably expired — never bumped).
pub async fn sign_withdrawal(state: &AppState, rpc: &dyn SolanaRpc, claim: &mut WithdrawalClaim) -> Result<SignResult> {
    let row = claim.row().clone();
    let token = row.token()?;
    let amount = u64::try_from(row.amount_raw.0).context("amount out of range")?;
    let treasury = state.wallet.treasury_address(Network::Solana)?;
    let route = match stored_route(rpc, &state.config.chain.usdt_solana_mint, &row).await {
        Ok(Some(route)) => route,
        Ok(None) => return Ok(SignResult::Fail("destination_invalid")),
        Err(err) => return Ok(SignResult::Wait(format!("{err:#}"))),
    };
    let plan = plan_solana(rpc, &treasury, &route).await?;
    let cap = row.fee_cap_raw.map(|c| c.0).unwrap_or(u128::MAX);
    if plan.max_fee() > cap {
        return Ok(if row.status()? == WithdrawalStatus::Queued {
            SignResult::Wait(format!("network fee {} lamports above the fee cap {cap}", plan.max_fee()))
        } else {
            SignResult::Stall
        });
    }
    let lamports = rpc.lamports(&treasury).await?;
    let spec = match (token, route) {
        (Token::Sol, SolanaRoute::Wallet) => {
            if lamports < u128::from(amount) + plan.max_fee() {
                return Ok(SignResult::Fail("insufficient_funds"));
            }
            TransferSpec::Sol { to: row.destination.clone(), lamports: amount }
        }
        (Token::Usdt, SolanaRoute::TokenAccount { account, create_for_owner }) => {
            let holder = state.wallet.solana_usdt_token_account(KeyRef::Treasury(Network::Solana))?;
            if rpc.token_balance(&holder).await? < u128::from(amount) || lamports < plan.max_fee() {
                return Ok(SignResult::Fail("insufficient_funds"));
            }
            TransferSpec::Usdt { destination_account: account, create_for_owner, amount }
        }
        _ => return Ok(SignResult::Fail("destination_invalid")),
    };
    let tx = sign_and_store(
        state,
        rpc,
        claim.conn(),
        SolanaSend {
            spec,
            fees: plan.fees,
            purpose: OutboundPurpose::Withdrawal,
            withdrawal_id: Some(&row.id),
            sweep_id: None,
            token,
            from: &treasury,
            recipient: &row.destination,
            amount: u128::from(amount),
            rent: plan.rent,
        },
    )
    .await?;
    Ok(SignResult::Signed(Box::new(tx)))
}

/// The route fixed at admission (never guessed again): a token account
/// must still be a USDT one; a wallet's associated account is created when
/// missing. `None`: the destination is no longer valid.
async fn stored_route(rpc: &dyn SolanaRpc, usdt_mint: &str, row: &db::WithdrawalRow) -> Result<Option<SolanaRoute>> {
    match (row.token()?, row.solana_token_account.as_deref()) {
        (Token::Sol, _) => Ok(Some(SolanaRoute::Wallet)),
        (Token::Usdt, Some(account)) if account == row.destination => match rpc.account(account).await? {
            SolanaAccount::Token { mint, .. } if mint == usdt_mint => {
                Ok(Some(SolanaRoute::TokenAccount { account: account.to_string(), create_for_owner: None }))
            }
            // Closed (or reused) since admission: never re-read as a wallet.
            _ => Ok(None),
        },
        (Token::Usdt, Some(account)) => {
            let missing = matches!(rpc.account(account).await?, SolanaAccount::Missing);
            Ok(Some(SolanaRoute::TokenAccount {
                account: account.to_string(),
                create_for_owner: missing.then(|| row.destination.clone()),
            }))
        }
        _ => Ok(None),
    }
}

fn stored_tx(tx: &OutboundTxRow) -> Result<SignedSolanaTx> {
    Ok(SignedSolanaTx {
        wire: base64::engine::general_purpose::STANDARD
            .decode(&tx.signed_tx)
            .context("stored transaction is not base64")?,
        signature: tx.tx_hash.clone(),
    })
}

pub async fn broadcast(rpc: &dyn SolanaRpc, tx: &OutboundTxRow) -> Result<BroadcastResult> {
    Ok(match rpc.send(&stored_tx(tx)?).await? {
        SolanaSendOutcome::Accepted => BroadcastResult::Accepted,
        SolanaSendOutcome::BlockhashExpired => BroadcastResult::Dead("expired"),
        SolanaSendOutcome::Rejected(detail) => BroadcastResult::Rejected(detail),
    })
}

/// Finalized (landed), pending, or provably expired: the cluster is past
/// its blockhash's last valid height (plus a margin) *and* has no trace of
/// it (history searched). The height is read *before* the status, so a
/// transaction landing in the last valid blocks is seen, never expired.
pub async fn observe(rpc: &dyn SolanaRpc, tx: &OutboundTxRow) -> Result<Observation> {
    let fee = tx.fee_params["fee"].as_u64().map(u128::from).unwrap_or(0);
    let rent = tx.fee_params["rent"].as_u64().map(u128::from).unwrap_or(0);
    let height = rpc.block_height().await?;
    match rpc.signature_status(&tx.tx_hash).await? {
        // A failed transaction still pays its fee (the rent was not spent).
        // Failure is final at `finalized` only, like success: a block seen
        // at `confirmed` can still be dropped.
        Some(status) if status.finalized && status.failed => {
            Ok(Observation::Landed { success: false, block: status.slot, confirmations: 1, fee })
        }
        Some(status) if status.finalized => Ok(Observation::Landed {
            success: true,
            block: status.slot,
            confirmations: 1,
            fee: fee + rent,
        }),
        Some(_) => Ok(Observation::Pending { stuck: false, resend: false }),
        None => {
            let last_valid = tx.fee_params["lastValidBlockHeight"].as_u64().unwrap_or(u64::MAX);
            if height > last_valid.saturating_add(EXPIRY_MARGIN_BLOCKS) {
                Ok(Observation::Dead("expired"))
            } else {
                Ok(Observation::Pending { stuck: false, resend: tx.status()? == domain::OutboundTxStatus::Broadcast })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use chain::solana_tx::{RecentBlockhash, SolanaAccount, SolanaSignatureStatus};

    #[derive(Default)]
    struct FakeSolana {
        status: Option<SolanaSignatureStatus>,
        height: u64,
        account: Option<SolanaAccount>,
        sent: std::sync::Mutex<Vec<SignedSolanaTx>>,
    }

    #[async_trait]
    impl SolanaRpc for FakeSolana {
        async fn latest_blockhash(&self) -> Result<RecentBlockhash> {
            unimplemented!()
        }
        async fn block_height(&self) -> Result<u64> {
            Ok(self.height)
        }
        async fn account(&self, _: &str) -> Result<SolanaAccount> {
            Ok(self.account.clone().unwrap_or(SolanaAccount::Missing))
        }
        async fn priority_fee(&self, _: &[String]) -> Result<u64> {
            unimplemented!()
        }
        async fn send(&self, signed: &SignedSolanaTx) -> Result<SolanaSendOutcome> {
            self.sent.lock().unwrap().push(signed.clone());
            Ok(SolanaSendOutcome::Accepted)
        }
        async fn signature_status(&self, _: &str) -> Result<Option<SolanaSignatureStatus>> {
            Ok(self.status.clone())
        }
        async fn lamports(&self, _: &str) -> Result<u128> {
            unimplemented!()
        }
        async fn token_balance(&self, _: &str) -> Result<u128> {
            unimplemented!()
        }
    }

    fn tx() -> OutboundTxRow {
        let mut tx = super::super::eth::tests::stored("AQ==", 0, 0);
        tx.network = "SOLANA".into();
        tx.fee_params = json!({ "fee": 6_000, "rent": 2_039_280, "lastValidBlockHeight": 500 });
        tx
    }

    /// Invariant 1: a crash between signing and broadcasting re-sends the
    /// stored bytes under the stored signature, never a different message.
    #[tokio::test]
    async fn a_stored_transaction_is_rebroadcast_byte_for_byte() {
        let rpc = FakeSolana::default();
        let tx = tx();
        assert_eq!(broadcast(&rpc, &tx).await.unwrap(), BroadcastResult::Accepted);
        let sent = rpc.sent.lock().unwrap();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].wire, vec![1u8]);
        assert_eq!(sent[0].signature, tx.tx_hash);
    }

    #[tokio::test]
    async fn blockhash_expiry_is_proven_by_height_and_absence() {
        let unknown_valid = FakeSolana { status: None, height: 500, ..Default::default() };
        assert_eq!(observe(&unknown_valid, &tx()).await.unwrap(), Observation::Pending { stuck: false, resend: true });
        // Just past the last valid height: a lagging node may not show it yet.
        let unknown_just_past = FakeSolana { status: None, height: 501, ..Default::default() };
        assert_eq!(observe(&unknown_just_past, &tx()).await.unwrap(), Observation::Pending { stuck: false, resend: true });
        let unknown_expired = FakeSolana { status: None, height: 565, ..Default::default() };
        assert_eq!(observe(&unknown_expired, &tx()).await.unwrap(), Observation::Dead("expired"));
    }

    #[tokio::test]
    async fn finalized_and_failed_transactions_land_with_their_fee() {
        let finalized = FakeSolana {
            status: Some(SolanaSignatureStatus { slot: 9, finalized: true, failed: false }),
            height: 0,
            ..Default::default()
        };
        assert_eq!(
            observe(&finalized, &tx()).await.unwrap(),
            Observation::Landed { success: true, block: 9, confirmations: 1, fee: 6_000 + 2_039_280 }
        );
        let failed = FakeSolana {
            status: Some(SolanaSignatureStatus { slot: 9, finalized: true, failed: true }),
            height: 0,
            ..Default::default()
        };
        assert_eq!(observe(&failed, &tx()).await.unwrap(), Observation::Landed { success: false, block: 9, confirmations: 1, fee: 6_000 });
        // A failure seen at `confirmed` is not final yet: the block may still
        // be dropped. Success and failure both wait for `finalized`.
        let failed_unfinalized = FakeSolana {
            status: Some(SolanaSignatureStatus { slot: 9, finalized: false, failed: true }),
            height: 0,
            ..Default::default()
        };
        assert_eq!(observe(&failed_unfinalized, &tx()).await.unwrap(), Observation::Pending { stuck: false, resend: false });
        let confirmed = FakeSolana {
            status: Some(SolanaSignatureStatus { slot: 9, finalized: false, failed: false }),
            height: 0,
            ..Default::default()
        };
        assert_eq!(observe(&confirmed, &tx()).await.unwrap(), Observation::Pending { stuck: false, resend: false });
    }

    fn usdt_withdrawal(destination: &str, token_account: &str) -> db::WithdrawalRow {
        db::WithdrawalRow {
            id: "w-1".into(),
            idempotency_key: "k".into(),
            api_key_id: Some("key".into()),
            staff_id: None,
            project_id: Some("p1".into()),
            params_hash: "h".into(),
            network: "SOLANA".into(),
            token: "USDT".into(),
            destination_kind: "ADDRESS".into(),
            destination: destination.into(),
            address_book_entry_id: None,
            solana_token_account: Some(token_account.into()),
            amount_raw: db::RawAmount(1),
            fee_estimate_raw: None,
            fee_cap_raw: None,
            fee_raw: None,
            fee_token: "SOL".into(),
            status: "QUEUED".into(),
            approval_required: false,
            approved_by_id: None,
            approved_at: None,
            self_approval_not_before: None,
            approval_expires_at: None,
            cancelled_by_id: None,
            cancel_reason: None,
            tx_hash: None,
            confirmations: 0,
            failure_reason: None,
            reference: None,
            comment: None,
            metadata: json!({}),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            broadcast_at: None,
            confirmed_at: None,
        }
    }

    #[tokio::test]
    async fn the_route_fixed_at_admission_is_never_guessed_again() {
        const MINT: &str = "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB";
        // A token account at admission, closed since: refused, never read
        // as a wallet (a token account created for it would be lost).
        let token_account = usdt_withdrawal("TokenAcct", "TokenAcct");
        assert_eq!(stored_route(&FakeSolana::default(), MINT, &token_account).await.unwrap(), None);
        let still_there = FakeSolana {
            account: Some(SolanaAccount::Token { mint: MINT.into(), owner: "Owner".into() }),
            ..Default::default()
        };
        assert_eq!(
            stored_route(&still_there, MINT, &token_account).await.unwrap(),
            Some(SolanaRoute::TokenAccount { account: "TokenAcct".into(), create_for_owner: None })
        );
        // A wallet at admission: its associated account, created if missing.
        let wallet = usdt_withdrawal("Wallet", "WalletAta");
        assert_eq!(
            stored_route(&FakeSolana::default(), MINT, &wallet).await.unwrap(),
            Some(SolanaRoute::TokenAccount { account: "WalletAta".into(), create_for_owner: Some("Wallet".into()) })
        );
    }
}
