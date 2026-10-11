//! Solana transactions: USDT `transfer_checked` (with the destination's
//! token account created idempotently when missing), native SOL transfers,
//! and deposit sweeps (`transfer_checked` + `close_account`). Messages are
//! built with solana-sdk / spl-token; signatures come from
//! `Signer::sign_ed25519` and are inserted by hand (never
//! `solana_sdk::Keypair`, ADR 0003), and the wire format — short-vec of
//! signatures, then the message — is assembled here.

use std::str::FromStr;

use anyhow::{Context, Result};
use async_trait::async_trait;
use base64::Engine;
use serde::Deserialize;
use solana_sdk::compute_budget::ComputeBudgetInstruction;
use solana_sdk::hash::Hash;
use solana_sdk::instruction::{AccountMeta, Instruction};
use solana_sdk::message::Message;
use solana_sdk::pubkey::Pubkey;
use spl_associated_token_account::get_associated_token_address;
use spl_associated_token_account::instruction::create_associated_token_account_idempotent;
use wallet::{KeyRef, Signer};

use crate::{json_rpc, ChainClients};

/// Base fee per signature, lamports.
pub const LAMPORTS_PER_SIGNATURE: u64 = 5_000;
/// Rent-exempt balance of a token account (165 bytes), lamports — what
/// creating a destination's USDT account costs the fee payer.
pub const TOKEN_ACCOUNT_RENT_LAMPORTS: u64 = 2_039_280;
/// Compute-unit limits per transaction shape (generous: the price is per
/// requested unit, the limit only bounds it).
pub const CU_LIMIT_USDT_TRANSFER: u32 = 40_000;
pub const CU_LIMIT_USDT_TRANSFER_WITH_ATA: u32 = 80_000;
pub const CU_LIMIT_SOL_TRANSFER: u32 = 5_000;
/// A sweep also (idempotently) creates the treasury's USDT token account.
pub const CU_LIMIT_SWEEP: u32 = 90_000;

/// Compute budget of a transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SolanaFees {
    pub cu_limit: u32,
    /// Micro-lamports per compute unit.
    pub cu_price: u64,
}

impl SolanaFees {
    /// Priority fee paid on top of the signature fees, lamports (rounded up).
    pub fn priority_lamports(&self) -> u64 {
        u64::try_from((u128::from(self.cu_limit) * u128::from(self.cu_price)).div_ceil(1_000_000))
            .unwrap_or(u64::MAX)
    }

    /// Everything the fee payer can be charged (signatures + priority).
    pub fn max_fee_lamports(&self, signatures: u64) -> u64 {
        LAMPORTS_PER_SIGNATURE
            .saturating_mul(signatures)
            .saturating_add(self.priority_lamports())
    }
}

/// What to send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SolanaTransfer {
    /// USDT from the treasury's token account to `destination_account`
    /// (created first, paid by the treasury, when `create_for_owner` is the
    /// destination owner whose associated account does not exist yet).
    Usdt {
        treasury_owner: Pubkey,
        mint: Pubkey,
        destination_account: Pubkey,
        create_for_owner: Option<Pubkey>,
        amount: u64,
    },
    /// Native SOL from the treasury owner.
    Sol { treasury_owner: Pubkey, to: Pubkey, lamports: u64 },
    /// A deposit's USDT into the treasury's token account, fees paid by the
    /// treasury; `close` then closes the deposit account (its rent goes back
    /// to the treasury owner).
    Sweep {
        treasury_owner: Pubkey,
        deposit_owner: Pubkey,
        mint: Pubkey,
        amount: u64,
        close: bool,
    },
}

impl SolanaTransfer {
    pub fn default_cu_limit(&self) -> u32 {
        match self {
            Self::Usdt { create_for_owner: Some(_), .. } => CU_LIMIT_USDT_TRANSFER_WITH_ATA,
            Self::Usdt { .. } => CU_LIMIT_USDT_TRANSFER,
            Self::Sol { .. } => CU_LIMIT_SOL_TRANSFER,
            Self::Sweep { .. } => CU_LIMIT_SWEEP,
        }
    }
}

/// The instructions and the fee payer (always the treasury owner).
pub fn build_message(transfer: &SolanaTransfer, fees: SolanaFees, blockhash: &Hash) -> Result<Message> {
    let token_program = spl_token::id();
    let mut instructions: Vec<Instruction> = vec![
        ComputeBudgetInstruction::set_compute_unit_limit(fees.cu_limit),
        ComputeBudgetInstruction::set_compute_unit_price(fees.cu_price),
    ];
    let payer = match transfer {
        SolanaTransfer::Usdt {
            treasury_owner,
            mint,
            destination_account,
            create_for_owner,
            amount,
        } => {
            if let Some(owner) = create_for_owner {
                instructions.push(create_associated_token_account_idempotent(
                    treasury_owner,
                    owner,
                    mint,
                    &token_program,
                ));
            }
            let source = get_associated_token_address(treasury_owner, mint);
            instructions.push(spl_token::instruction::transfer_checked(
                &token_program,
                &source,
                mint,
                destination_account,
                treasury_owner,
                &[],
                *amount,
                domain::USDT_DECIMALS as u8,
            )?);
            *treasury_owner
        }
        SolanaTransfer::Sol {
            treasury_owner,
            to,
            lamports,
        } => {
            instructions.push(system_transfer(treasury_owner, to, *lamports));
            *treasury_owner
        }
        SolanaTransfer::Sweep {
            treasury_owner,
            deposit_owner,
            mint,
            amount,
            close,
        } => {
            let source = get_associated_token_address(deposit_owner, mint);
            let destination = get_associated_token_address(treasury_owner, mint);
            // Nothing else ever creates the treasury's token account (a no-op
            // once it exists; the first sweep pays its rent).
            instructions.push(create_associated_token_account_idempotent(
                treasury_owner,
                treasury_owner,
                mint,
                &token_program,
            ));
            instructions.push(spl_token::instruction::transfer_checked(
                &token_program,
                &source,
                mint,
                &destination,
                deposit_owner,
                &[],
                *amount,
                domain::USDT_DECIMALS as u8,
            )?);
            if *close {
                instructions.push(spl_token::instruction::close_account(
                    &token_program,
                    &source,
                    treasury_owner,
                    deposit_owner,
                    &[],
                )?);
            }
            *treasury_owner
        }
    };
    Ok(Message::new_with_blockhash(&instructions, Some(&payer), blockhash))
}

/// System program `Transfer` (instruction 2: u32 LE tag, u64 LE lamports).
fn system_transfer(from: &Pubkey, to: &Pubkey, lamports: u64) -> Instruction {
    let mut data = Vec::with_capacity(12);
    data.extend_from_slice(&2u32.to_le_bytes());
    data.extend_from_slice(&lamports.to_le_bytes());
    Instruction::new_with_bytes(
        solana_sdk::system_program::id(),
        &data,
        vec![AccountMeta::new(*from, true), AccountMeta::new(*to, false)],
    )
}

/// A signed transaction: wire bytes and its id (the fee payer's signature,
/// base58).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignedSolanaTx {
    pub wire: Vec<u8>,
    pub signature: String,
}

impl SignedSolanaTx {
    pub fn base64(&self) -> String {
        base64::engine::general_purpose::STANDARD.encode(&self.wire)
    }
}

/// Sign `message` with every required signer, in message order. `keys`
/// maps each signer's public key to the wallet key that signs for it.
pub fn sign_message(message: &Message, signer: &dyn Signer, keys: &[(Pubkey, KeyRef)]) -> Result<SignedSolanaTx> {
    let data = message.serialize();
    let required = usize::from(message.header.num_required_signatures);
    let mut signatures = Vec::with_capacity(required);
    for account in message.account_keys.iter().take(required) {
        let key = keys
            .iter()
            .find(|(pubkey, _)| pubkey == account)
            .map(|(_, key)| *key)
            .with_context(|| format!("no key to sign for {account}"))?;
        signatures.push(signer.sign_ed25519(key, &data).context("Solana signing failed")?);
    }
    let mut wire = Vec::with_capacity(3 + 64 * required + data.len());
    encode_compact_u16(required as u16, &mut wire);
    for signature in &signatures {
        wire.extend_from_slice(signature);
    }
    wire.extend_from_slice(&data);
    Ok(SignedSolanaTx {
        wire,
        signature: bs58::encode(signatures.first().context("message has no signer")?).into_string(),
    })
}

/// Solana's short-vec length prefix.
fn encode_compact_u16(mut value: u16, out: &mut Vec<u8>) {
    loop {
        let mut byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return;
        }
        byte |= 0x80;
        out.push(byte);
    }
}

/// A transfer described with plain strings (callers stay free of Solana
/// types).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransferSpec {
    /// USDT from the treasury to `destination_account`, created for
    /// `create_for_owner` when given.
    Usdt {
        destination_account: String,
        create_for_owner: Option<String>,
        amount: u64,
    },
    /// SOL from the treasury.
    Sol { to: String, lamports: u64 },
    /// A deposit's USDT into the treasury (signed by `deposit` too).
    Sweep { deposit: KeyRef, amount: u64, close: bool },
}

/// Build and sign `spec` with the treasury as fee payer, on `blockhash`.
pub fn sign_transfer(
    spec: &TransferSpec,
    mint: &str,
    fees: SolanaFees,
    blockhash: &str,
    signer: &dyn Signer,
) -> Result<SignedSolanaTx> {
    let treasury_key = KeyRef::Treasury(domain::Network::Solana);
    let treasury_owner = owner_pubkey(signer, treasury_key)?;
    let mint = parse_pubkey(mint)?;
    let blockhash = Hash::from_str(blockhash).map_err(|e| anyhow::anyhow!("invalid blockhash: {e}"))?;
    let mut keys = vec![(treasury_owner, treasury_key)];
    let transfer = match spec {
        TransferSpec::Usdt {
            destination_account,
            create_for_owner,
            amount,
        } => SolanaTransfer::Usdt {
            treasury_owner,
            mint,
            destination_account: parse_pubkey(destination_account)?,
            create_for_owner: create_for_owner.as_deref().map(parse_pubkey).transpose()?,
            amount: *amount,
        },
        TransferSpec::Sol { to, lamports } => SolanaTransfer::Sol {
            treasury_owner,
            to: parse_pubkey(to)?,
            lamports: *lamports,
        },
        TransferSpec::Sweep { deposit, amount, close } => {
            let deposit_owner = owner_pubkey(signer, *deposit)?;
            keys.push((deposit_owner, *deposit));
            SolanaTransfer::Sweep {
                treasury_owner,
                deposit_owner,
                mint,
                amount: *amount,
                close: *close,
            }
        }
    };
    sign_message(&build_message(&transfer, fees, &blockhash)?, signer, &keys)
}

/// The owner public key a `KeyRef` signs for (from the wallet).
pub fn owner_pubkey(signer: &dyn Signer, key: KeyRef) -> Result<Pubkey> {
    let bytes: [u8; 32] = signer
        .public_key(key)?
        .try_into()
        .map_err(|_| anyhow::anyhow!("not an ed25519 key"))?;
    Ok(Pubkey::new_from_array(bytes))
}

/// The associated token account of `owner` for `mint` (both base58).
pub fn associated_token_account(owner: &str, mint: &str) -> Result<String> {
    Ok(get_associated_token_address(&parse_pubkey(owner)?, &parse_pubkey(mint)?).to_string())
}

/// The System program: owner of every plain wallet holding SOL.
pub const SYSTEM_PROGRAM_ID: &str = "11111111111111111111111111111111";

impl SolanaAccount {
    /// A plain wallet: nothing there yet, or an account of the System
    /// program — never a mint, a program, or a program-owned account.
    pub fn is_wallet(&self) -> bool {
        match self {
            Self::Missing => true,
            Self::Other { program } => program == SYSTEM_PROGRAM_ID,
            Self::Token { .. } => false,
        }
    }
}

/// Whether `address` is an ed25519 point — a key someone can sign for. A
/// program-derived address (token accounts created as ATAs, vaults…) is
/// not: a token account created for it would be lost.
pub fn is_on_curve(address: &str) -> Result<bool> {
    Ok(parse_pubkey(address)?.is_on_curve())
}

pub fn parse_pubkey(value: &str) -> Result<Pubkey> {
    Pubkey::from_str(value).map_err(|e| anyhow::anyhow!("invalid Solana address {value}: {e}"))
}

/// What an address is on chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SolanaAccount {
    /// Nothing there (a fresh wallet).
    Missing,
    /// An SPL token account.
    Token { mint: String, owner: String },
    /// Anything else: a wallet, a program account…
    Other { program: String },
}

#[derive(Debug, Deserialize)]
pub(crate) struct AccountInfoValue {
    owner: Option<String>,
    data: Option<serde_json::Value>,
}

pub(crate) fn classify_account(value: Option<AccountInfoValue>) -> SolanaAccount {
    let Some(value) = value else {
        return SolanaAccount::Missing;
    };
    let program = value.owner.unwrap_or_default();
    if program == spl_token::id().to_string() {
        let info = value.data.as_ref().map(|d| &d["parsed"]["info"]);
        if let Some(info) = info {
            if let (Some(mint), Some(owner)) = (info["mint"].as_str(), info["owner"].as_str()) {
                return SolanaAccount::Token {
                    mint: mint.into(),
                    owner: owner.into(),
                };
            }
        }
    }
    SolanaAccount::Other { program }
}

/// A recent blockhash and the last block height it is valid at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecentBlockhash {
    pub blockhash: String,
    pub last_valid_block_height: u64,
}

/// `getSignatureStatuses` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SolanaSignatureStatus {
    pub slot: u64,
    pub finalized: bool,
    /// The transaction landed but failed.
    pub failed: bool,
}

/// Outcome of `sendTransaction`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SolanaSendOutcome {
    Accepted,
    /// The blockhash is gone: this transaction can never land.
    BlockhashExpired,
    Rejected(String),
}

pub(crate) fn classify_send_error(message: &str) -> SolanaSendOutcome {
    let lower = message.to_ascii_lowercase();
    if lower.contains("already been processed") {
        SolanaSendOutcome::Accepted
    } else if lower.contains("blockhash not found") {
        SolanaSendOutcome::BlockhashExpired
    } else {
        SolanaSendOutcome::Rejected(message.chars().take(300).collect())
    }
}

/// What the outbound engine needs from a Solana node.
#[async_trait]
pub trait SolanaRpc: Send + Sync {
    async fn latest_blockhash(&self) -> Result<RecentBlockhash>;
    async fn block_height(&self) -> Result<u64>;
    async fn account(&self, address: &str) -> Result<SolanaAccount>;
    /// Helius `getPriorityFeeEstimate` (Medium), micro-lamports per CU.
    async fn priority_fee(&self, account_keys: &[String]) -> Result<u64>;
    async fn send(&self, tx: &SignedSolanaTx) -> Result<SolanaSendOutcome>;
    /// `None` when the cluster does not know the signature (history
    /// searched).
    async fn signature_status(&self, signature: &str) -> Result<Option<SolanaSignatureStatus>>;
    /// Finalized lamports.
    async fn lamports(&self, address: &str) -> Result<u128>;
    /// Finalized token amount of a token account (0 when it does not exist).
    async fn token_balance(&self, token_account: &str) -> Result<u128>;
}

impl ChainClients {
    fn solana_rpc(&self) -> Result<String> {
        let key = self
            .api_key(domain::Network::Solana)
            .context("HELIUS_API_KEY not configured")?;
        Ok(self.config.helius_rpc_url(key))
    }
}

#[async_trait]
impl SolanaRpc for ChainClients {
    async fn latest_blockhash(&self) -> Result<RecentBlockhash> {
        #[derive(Deserialize)]
        struct Response {
            value: Value,
        }
        #[derive(Deserialize)]
        struct Value {
            blockhash: String,
            #[serde(rename = "lastValidBlockHeight")]
            last_valid_block_height: u64,
        }
        let response: Response = json_rpc(
            &self.http,
            &self.solana_rpc()?,
            "getLatestBlockhash",
            serde_json::json!([{ "commitment": "confirmed" }]),
        )
        .await?;
        Ok(RecentBlockhash {
            blockhash: response.value.blockhash,
            last_valid_block_height: response.value.last_valid_block_height,
        })
    }

    async fn block_height(&self) -> Result<u64> {
        json_rpc(&self.http, &self.solana_rpc()?, "getBlockHeight", serde_json::json!([{ "commitment": "confirmed" }])).await
    }

    async fn account(&self, address: &str) -> Result<SolanaAccount> {
        #[derive(Deserialize)]
        struct Response {
            value: Option<AccountInfoValue>,
        }
        let response: Response = json_rpc(
            &self.http,
            &self.solana_rpc()?,
            "getAccountInfo",
            serde_json::json!([address, { "encoding": "jsonParsed", "commitment": "confirmed" }]),
        )
        .await?;
        Ok(classify_account(response.value))
    }

    async fn priority_fee(&self, account_keys: &[String]) -> Result<u64> {
        #[derive(Deserialize)]
        struct Response {
            #[serde(rename = "priorityFeeEstimate")]
            priority_fee_estimate: Option<f64>,
        }
        let response: Response = json_rpc(
            &self.http,
            &self.solana_rpc()?,
            "getPriorityFeeEstimate",
            serde_json::json!([{ "accountKeys": account_keys, "options": { "priorityLevel": "Medium" } }]),
        )
        .await?;
        // A fee rate, never an amount of funds: rounding up is safe.
        let estimate = response.priority_fee_estimate.unwrap_or(0.0);
        Ok(if estimate.is_finite() && estimate > 0.0 { estimate.ceil().min(1e12) as u64 } else { 0 })
    }

    async fn send(&self, tx: &SignedSolanaTx) -> Result<SolanaSendOutcome> {
        let result: Result<String> = json_rpc(
            &self.http,
            &self.solana_rpc()?,
            "sendTransaction",
            serde_json::json!([tx.base64(), { "encoding": "base64", "preflightCommitment": "confirmed" }]),
        )
        .await;
        match result {
            Ok(_) => Ok(SolanaSendOutcome::Accepted),
            Err(err) => Ok(classify_send_error(&format!("{err:#}"))),
        }
    }

    async fn signature_status(&self, signature: &str) -> Result<Option<SolanaSignatureStatus>> {
        #[derive(Deserialize)]
        struct Response {
            value: Vec<Option<Status>>,
        }
        #[derive(Deserialize)]
        struct Status {
            slot: u64,
            err: Option<serde_json::Value>,
            #[serde(rename = "confirmationStatus")]
            confirmation_status: Option<String>,
        }
        let response: Response = json_rpc(
            &self.http,
            &self.solana_rpc()?,
            "getSignatureStatuses",
            serde_json::json!([[signature], { "searchTransactionHistory": true }]),
        )
        .await?;
        Ok(response.value.into_iter().next().flatten().map(|status| SolanaSignatureStatus {
            slot: status.slot,
            finalized: status.confirmation_status.as_deref() == Some("finalized"),
            failed: status.err.is_some_and(|err| !err.is_null()),
        }))
    }

    async fn lamports(&self, address: &str) -> Result<u128> {
        #[derive(Deserialize)]
        struct Response {
            value: u64,
        }
        let response: Response = json_rpc(
            &self.http,
            &self.solana_rpc()?,
            "getBalance",
            serde_json::json!([address, { "commitment": "finalized" }]),
        )
        .await?;
        Ok(u128::from(response.value))
    }

    async fn token_balance(&self, token_account: &str) -> Result<u128> {
        let body = serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "getTokenAccountBalance",
            "params": [token_account, { "commitment": "finalized" }]
        });
        // A missing token account answers an RPC error: balance 0.
        let response: serde_json::Value = crate::request::send_json(self.http.post(self.solana_rpc()?).json(&body))
            .await
            .context("RPC getTokenAccountBalance request failed")?;
        Ok(response["result"]["value"]["amount"]
            .as_str()
            .and_then(|amount| amount.parse().ok())
            .unwrap_or(0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::Verifier;
    use wallet::WalletService;

    const MNEMONIC: &str =
        "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";

    fn wallet() -> WalletService {
        WalletService::from_mnemonic(MNEMONIC).unwrap()
    }

    fn mint() -> Pubkey {
        parse_pubkey(config::USDT_SOLANA).unwrap()
    }

    fn fees() -> SolanaFees {
        SolanaFees { cu_limit: 80_000, cu_price: 12_500 }
    }

    fn blockhash() -> Hash {
        Hash::new_from_array([7; 32])
    }

    /// Parse our wire bytes: (signatures, message bytes).
    fn split_wire(wire: &[u8]) -> (Vec<[u8; 64]>, &[u8]) {
        let count = usize::from(wire[0]);
        let signatures = (0..count)
            .map(|i| wire[1 + 64 * i..1 + 64 * (i + 1)].try_into().unwrap())
            .collect();
        (signatures, &wire[1 + 64 * count..])
    }

    fn program_ids(message: &Message) -> Vec<Pubkey> {
        message
            .instructions
            .iter()
            .map(|ix| message.account_keys[usize::from(ix.program_id_index)])
            .collect()
    }

    #[test]
    fn usdt_transfer_creates_the_destination_account_and_is_paid_by_the_treasury() {
        let wallet = wallet();
        let treasury_key = KeyRef::Treasury(domain::Network::Solana);
        let treasury = owner_pubkey(&wallet, treasury_key).unwrap();
        let recipient = parse_pubkey("HAgk14JpMQLgt6rVgv7cBQFJWFto5Dqxi472uT3DKpqk").unwrap();
        let destination = get_associated_token_address(&recipient, &mint());
        let transfer = SolanaTransfer::Usdt {
            treasury_owner: treasury,
            mint: mint(),
            destination_account: destination,
            create_for_owner: Some(recipient),
            amount: 25_000_000,
        };
        let message = build_message(&transfer, fees(), &blockhash()).unwrap();
        // Fee payer first and the only signer.
        assert_eq!(message.account_keys[0], treasury);
        assert_eq!(message.header.num_required_signatures, 1);
        assert_eq!(
            program_ids(&message),
            vec![
                solana_sdk::compute_budget::id(),
                solana_sdk::compute_budget::id(),
                spl_associated_token_account::id(),
                spl_token::id(),
            ]
        );
        // transfer_checked: instruction 12, amount LE, 6 decimals.
        let transfer_ix = message.instructions.last().unwrap();
        assert_eq!(transfer_ix.data[0], 12);
        assert_eq!(u64::from_le_bytes(transfer_ix.data[1..9].try_into().unwrap()), 25_000_000);
        assert_eq!(transfer_ix.data[9], 6);
        // Create-idempotent is instruction 1 of the ATA program.
        assert_eq!(message.instructions[2].data, vec![1]);
        assert_eq!(transfer.default_cu_limit(), CU_LIMIT_USDT_TRANSFER_WITH_ATA);

        let signed = sign_message(&message, &wallet, &[(treasury, treasury_key)]).unwrap();
        let (signatures, data) = split_wire(&signed.wire);
        assert_eq!(signatures.len(), 1);
        assert_eq!(data, message.serialize().as_slice());
        ed25519_dalek::VerifyingKey::from_bytes(&treasury.to_bytes())
            .unwrap()
            .verify(data, &ed25519_dalek::Signature::from_bytes(&signatures[0]))
            .unwrap();
        assert_eq!(signed.signature, bs58::encode(signatures[0]).into_string());
        // solana-sdk agrees the transaction is fully and correctly signed.
        let tx = solana_sdk::transaction::Transaction {
            signatures: signatures.iter().map(|s| solana_sdk::signature::Signature::from(*s)).collect(),
            message,
        };
        assert!(tx.verify().is_ok());
    }

    #[test]
    fn an_existing_token_account_is_paid_directly() {
        let wallet = wallet();
        let treasury = owner_pubkey(&wallet, KeyRef::Treasury(domain::Network::Solana)).unwrap();
        let destination = parse_pubkey("6pXYguUixhfdydHDMgsvSy3sAWjStL2eEYh4KeYJAUDk").unwrap();
        let message = build_message(
            &SolanaTransfer::Usdt {
                treasury_owner: treasury,
                mint: mint(),
                destination_account: destination,
                create_for_owner: None,
                amount: 1,
            },
            fees(),
            &blockhash(),
        )
        .unwrap();
        assert_eq!(program_ids(&message).len(), 3);
        assert!(message.account_keys.contains(&destination));
    }

    #[test]
    fn sweeps_are_signed_by_the_deposit_and_the_treasury_then_close_the_account() {
        let wallet = wallet();
        let treasury_key = KeyRef::Treasury(domain::Network::Solana);
        let deposit_key = KeyRef::Deposit { network: domain::Network::Solana, index: 0 };
        let treasury = owner_pubkey(&wallet, treasury_key).unwrap();
        let deposit = owner_pubkey(&wallet, deposit_key).unwrap();
        let message = build_message(
            &SolanaTransfer::Sweep {
                treasury_owner: treasury,
                deposit_owner: deposit,
                mint: mint(),
                amount: 30_000_000,
                close: true,
            },
            SolanaFees { cu_limit: CU_LIMIT_SWEEP, cu_price: 1_000 },
            &blockhash(),
        )
        .unwrap();
        assert_eq!(message.account_keys[0], treasury, "the treasury pays the fees");
        assert_eq!(message.header.num_required_signatures, 2);
        let close = message.instructions.last().unwrap();
        assert_eq!(close.data, vec![9], "CloseAccount");
        // Rent goes back to the treasury owner.
        assert_eq!(message.account_keys[usize::from(close.accounts[1])], treasury);
        // The treasury's own token account is created first if missing…
        assert_eq!(program_ids(&message)[2], spl_associated_token_account::id());
        assert_eq!(message.account_keys[usize::from(message.instructions[2].accounts[2])], treasury);
        // …and the deposit account swept is the deposit address itself.
        assert_eq!(
            message.account_keys[usize::from(message.instructions[3].accounts[0])].to_string(),
            wallet.derive_address(domain::Network::Solana, 0).unwrap()
        );

        let signed = sign_message(&message, &wallet, &[(treasury, treasury_key), (deposit, deposit_key)]).unwrap();
        let (signatures, data) = split_wire(&signed.wire);
        assert_eq!(signatures.len(), 2);
        let tx = solana_sdk::transaction::Transaction {
            signatures: signatures.iter().map(|s| solana_sdk::signature::Signature::from(*s)).collect(),
            message: message.clone(),
        };
        assert!(tx.verify().is_ok());
        assert_eq!(data, message.serialize().as_slice());
        // A missing key is an error, never a partial signature.
        assert!(sign_message(&message, &wallet, &[(treasury, treasury_key)]).is_err());
    }

    #[test]
    fn sol_transfers_and_fees() {
        let wallet = wallet();
        let treasury = owner_pubkey(&wallet, KeyRef::Treasury(domain::Network::Solana)).unwrap();
        let to = parse_pubkey("HAgk14JpMQLgt6rVgv7cBQFJWFto5Dqxi472uT3DKpqk").unwrap();
        let message = build_message(
            &SolanaTransfer::Sol { treasury_owner: treasury, to, lamports: 100_000_000 },
            SolanaFees { cu_limit: CU_LIMIT_SOL_TRANSFER, cu_price: 0 },
            &blockhash(),
        )
        .unwrap();
        assert_eq!(program_ids(&message)[2], solana_sdk::system_program::id());
        #[allow(deprecated)]
        let reference = solana_sdk::system_instruction::transfer(&treasury, &to, 100_000_000);
        assert_eq!(system_transfer(&treasury, &to, 100_000_000), reference);
        assert_eq!(SolanaFees { cu_limit: 80_000, cu_price: 12_500 }.priority_lamports(), 1_000);
        assert_eq!(SolanaFees { cu_limit: 1, cu_price: 1 }.priority_lamports(), 1);
        assert_eq!(SolanaFees { cu_limit: 80_000, cu_price: 12_500 }.max_fee_lamports(2), 11_000);
    }

    #[test]
    fn string_specs_sign_like_the_typed_builder() {
        let wallet = wallet();
        let blockhash = "4uQeVj5tqViQh7yWWGStvkEG1Zmhx6uasJtWCJziofM";
        let deposit = KeyRef::Deposit { network: domain::Network::Solana, index: 3 };
        let signed = sign_transfer(
            &TransferSpec::Sweep { deposit, amount: 7, close: false },
            config::USDT_SOLANA,
            SolanaFees { cu_limit: CU_LIMIT_SWEEP, cu_price: 0 },
            blockhash,
            &wallet,
        )
        .unwrap();
        let (signatures, data) = split_wire(&signed.wire);
        assert_eq!(signatures.len(), 2);
        let message: Message = {
            let treasury = owner_pubkey(&wallet, KeyRef::Treasury(domain::Network::Solana)).unwrap();
            let deposit_owner = owner_pubkey(&wallet, deposit).unwrap();
            build_message(
                &SolanaTransfer::Sweep { treasury_owner: treasury, deposit_owner, mint: mint(), amount: 7, close: false },
                SolanaFees { cu_limit: CU_LIMIT_SWEEP, cu_price: 0 },
                &Hash::from_str(blockhash).unwrap(),
            )
            .unwrap()
        };
        assert_eq!(data, message.serialize().as_slice());
        assert!(sign_transfer(&TransferSpec::Sol { to: "not-base58!".into(), lamports: 1 }, config::USDT_SOLANA, fees(), blockhash, &wallet).is_err());
    }

    #[test]
    fn compact_u16_lengths() {
        let encode = |value| {
            let mut out = Vec::new();
            encode_compact_u16(value, &mut out);
            out
        };
        assert_eq!(encode(1), vec![1]);
        assert_eq!(encode(127), vec![0x7f]);
        assert_eq!(encode(128), vec![0x80, 0x01]);
        assert_eq!(encode(16_384), vec![0x80, 0x80, 0x01]);
    }

    #[test]
    fn accounts_are_classified() {
        let parse = |json: &str| classify_account(serde_json::from_str::<Option<AccountInfoValue>>(json).unwrap());
        assert_eq!(parse("null"), SolanaAccount::Missing);
        assert_eq!(
            parse(r#"{"owner":"TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA","lamports":2039280,
                     "data":{"program":"spl-token","parsed":{"type":"account","info":{
                       "mint":"Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB",
                       "owner":"HAgk14JpMQLgt6rVgv7cBQFJWFto5Dqxi472uT3DKpqk",
                       "tokenAmount":{"amount":"25000000","decimals":6}}}}}"#),
            SolanaAccount::Token {
                mint: "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB".into(),
                owner: "HAgk14JpMQLgt6rVgv7cBQFJWFto5Dqxi472uT3DKpqk".into(),
            }
        );
        let wallet = parse(r#"{"owner":"11111111111111111111111111111111","lamports":5,"data":["","base64"]}"#);
        assert_eq!(wallet, SolanaAccount::Other { program: SYSTEM_PROGRAM_ID.into() });
        // Wallets only: empty or System-owned — never the mint itself.
        assert!(wallet.is_wallet());
        assert!(SolanaAccount::Missing.is_wallet());
        let mint = parse(r#"{"owner":"TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA","lamports":1,"data":{"program":"spl-token","parsed":{"type":"mint","info":{"decimals":6}}}}"#);
        assert_eq!(mint, SolanaAccount::Other { program: "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA".into() });
        assert!(!mint.is_wallet());
    }

    #[test]
    fn send_errors_are_classified() {
        assert_eq!(
            classify_send_error("RPC sendTransaction failed: -32002 Transaction simulation failed: Blockhash not found"),
            SolanaSendOutcome::BlockhashExpired
        );
        assert_eq!(
            classify_send_error("-32002 This transaction has already been processed"),
            SolanaSendOutcome::Accepted
        );
        assert!(matches!(classify_send_error("-32002 insufficient funds"), SolanaSendOutcome::Rejected(_)));
    }
}
