//! Ethereum: native ETH detection (`alchemy_getAssetTransfers`), EIP-1559
//! transactions (ETH and USDT `transfer`) built and encoded here, signed
//! through `wallet::Signer`, and the RPC calls the outbound engine needs.

use std::collections::{BTreeMap, BTreeSet};

use alloy_consensus::{SignableTransaction, TxEip1559};
use alloy_primitives::{Address, Bytes, Signature, TxKind, U256};
use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Deserialize;
use tracing::warn;
use wallet::{KeyRef, Signer};

use crate::{confirmations_between, json_rpc, parse_hex_u128, parse_hex_u64, ChainClients, DetectedOnChainTransfer, ETH_LOG_SCAN_BLOCKS};

/// Gas of a plain ETH transfer to an externally owned account.
pub const ETH_TRANSFER_GAS: u64 = 21_000;
/// `eth_estimateGas` of a token transfer is padded by this much (×1.3).
pub const ERC20_GAS_ESTIMATE_PERCENT: u64 = 130;
/// The replacement rule nodes enforce: +10 % on both fee fields.
pub const REPLACEMENT_MIN_PERCENT: u128 = 110;
/// `alchemy_getAssetTransfers` pages followed per poll (1000 transfers
/// each): a safety stop against a runaway `pageKey`, far beyond what one
/// deposit address receives in the scan window. Past it, the transfers
/// already read are processed (detection is idempotent) and the rest wait
/// for the next poll.
const ASSET_TRANSFER_MAX_PAGES: usize = 200;

/// EIP-1559 fee fields, in wei per gas.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EvmFees {
    pub max_fee_per_gas: u128,
    pub max_priority_fee_per_gas: u128,
}

impl EvmFees {
    /// `maxFeePerGas = 2 × baseFee + priority` (spec §7.7): survives several
    /// consecutive full blocks.
    pub fn from_base_fee(base_fee: u128, priority: u128) -> Self {
        Self {
            max_fee_per_gas: base_fee.saturating_mul(2).saturating_add(priority),
            max_priority_fee_per_gas: priority,
        }
    }

    /// Most a transaction with `gas_limit` can cost.
    pub fn max_cost(&self, gas_limit: u64) -> u128 {
        self.max_fee_per_gas.saturating_mul(u128::from(gas_limit))
    }

    /// Both fields raised by `percent` (rounded up), and never by less than
    /// the 10 % replacement minimum.
    pub fn bumped(&self, percent: u128) -> Self {
        let percent = percent.max(REPLACEMENT_MIN_PERCENT - 100);
        let up = |value: u128| value.saturating_mul(100 + percent).div_ceil(100).max(value + 1);
        Self {
            max_fee_per_gas: up(self.max_fee_per_gas),
            max_priority_fee_per_gas: up(self.max_priority_fee_per_gas),
        }
    }

    /// Whether `self` may replace `old` in the mempool.
    pub fn replaces(&self, old: &Self) -> bool {
        let enough = |new: u128, old: u128| new.saturating_mul(100) >= old.saturating_mul(REPLACEMENT_MIN_PERCENT);
        enough(self.max_fee_per_gas, old.max_fee_per_gas)
            && enough(self.max_priority_fee_per_gas, old.max_priority_fee_per_gas)
    }
}

/// What to sign: a type-2 transaction from the key's address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvmTxRequest {
    pub chain_id: u64,
    pub nonce: u64,
    /// Recipient (ETH) or token contract (USDT).
    pub to: String,
    pub value: u128,
    pub data: Vec<u8>,
    pub gas_limit: u64,
    pub fees: EvmFees,
}

/// A signed transaction: the EIP-2718 bytes to broadcast and its hash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignedEvmTx {
    pub raw: Vec<u8>,
    /// `0x…`, lowercase.
    pub hash: String,
}

/// `transfer(address,uint256)` calldata (selector `a9059cbb`). USDT returns
/// no bool: success is the receipt status only.
pub fn erc20_transfer_data(to: &str, amount: u128) -> Result<Vec<u8>> {
    let to: Address = to.parse().context("invalid ERC-20 recipient")?;
    let mut data = Vec::with_capacity(68);
    data.extend_from_slice(&[0xa9, 0x05, 0x9c, 0xbb]);
    data.extend_from_slice(&[0u8; 12]);
    data.extend_from_slice(to.as_slice());
    data.extend_from_slice(&U256::from(amount).to_be_bytes::<32>());
    Ok(data)
}

fn unsigned(request: &EvmTxRequest) -> Result<TxEip1559> {
    let to: Address = request.to.parse().context("invalid transaction recipient")?;
    Ok(TxEip1559 {
        chain_id: request.chain_id,
        nonce: request.nonce,
        gas_limit: request.gas_limit,
        max_fee_per_gas: request.max_fee_per_gas(),
        max_priority_fee_per_gas: request.fees.max_priority_fee_per_gas,
        to: TxKind::Call(to),
        value: U256::from(request.value),
        access_list: Default::default(),
        input: Bytes::from(request.data.clone()),
    })
}

impl EvmTxRequest {
    fn max_fee_per_gas(&self) -> u128 {
        self.fees.max_fee_per_gas
    }
}

/// Encode and sign `request` with `key` (keccak-256 of the EIP-1559 signing
/// payload, through the `Signer`).
pub fn sign_eip1559(request: &EvmTxRequest, signer: &dyn Signer, key: KeyRef) -> Result<SignedEvmTx> {
    let tx = unsigned(request)?;
    let digest = tx.signature_hash();
    let signature = signer
        .sign_secp256k1_recoverable(key, digest.0)
        .context("EIP-1559 signing failed")?;
    let signature = Signature::new(
        U256::from_be_bytes(signature.r),
        U256::from_be_bytes(signature.s),
        signature.y_parity(),
    );
    let signed = tx.into_signed(signature);
    let mut raw = Vec::with_capacity(signed.eip2718_encoded_length());
    signed.eip2718_encode(&mut raw);
    Ok(SignedEvmTx {
        raw,
        hash: format!("{:#x}", signed.hash()),
    })
}

/// A mined transaction's outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EvmReceipt {
    /// `status = 1`. USDT returns no bool: this is the only success signal.
    pub success: bool,
    pub block_number: u64,
    /// `gasUsed × effectiveGasPrice`, in wei.
    pub fee_wei: u128,
}

#[derive(Debug, Deserialize)]
pub(crate) struct RawReceipt {
    status: Option<String>,
    #[serde(rename = "blockNumber")]
    block_number: Option<String>,
    #[serde(rename = "gasUsed")]
    gas_used: Option<String>,
    #[serde(rename = "effectiveGasPrice")]
    effective_gas_price: Option<String>,
}

pub(crate) fn parse_receipt(raw: RawReceipt) -> Result<EvmReceipt> {
    let block_number = raw
        .block_number
        .as_deref()
        .context("receipt without blockNumber")
        .and_then(parse_hex_u64)?;
    let gas_used = parse_hex_u128(raw.gas_used.as_deref().unwrap_or("0x0"))?;
    let price = parse_hex_u128(raw.effective_gas_price.as_deref().unwrap_or("0x0"))?;
    Ok(EvmReceipt {
        success: raw.status.as_deref().is_some_and(|s| parse_hex_u64(s).ok() == Some(1)),
        block_number,
        fee_wei: gas_used.saturating_mul(price),
    })
}

/// One entry of `alchemy_getAssetTransfers`. `value` (a float) is never
/// read: amounts come from `rawContract.value` (hex wei).
#[derive(Debug, Deserialize)]
pub(crate) struct AssetTransfer {
    #[serde(rename = "blockNum")]
    block_num: Option<String>,
    hash: Option<String>,
    to: Option<String>,
    asset: Option<String>,
    category: Option<String>,
    #[serde(rename = "rawContract")]
    raw_contract: Option<RawContract>,
    /// `withMetadata: true`: the block's timestamp.
    metadata: Option<AssetTransferMetadata>,
}

#[derive(Debug, Deserialize)]
struct AssetTransferMetadata {
    /// RFC 3339 (`2026-10-23T12:00:00.000Z`).
    #[serde(rename = "blockTimestamp")]
    block_timestamp: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawContract {
    value: Option<String>,
    /// `null` for ETH (no token contract).
    address: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct AssetTransfersPage {
    #[serde(default)]
    transfers: Vec<AssetTransfer>,
    #[serde(rename = "pageKey")]
    page_key: Option<String>,
}

/// Native ETH paid to `address` (external + internal transfers), one
/// detected transfer per transaction: every transfer of a tx to the address
/// is summed (a contract can forward ETH to it several times), like
/// Esplora outputs for BTC. Filters are re-checked rather than trusted; a
/// transfer without `rawContract.value` is skipped, never guessed.
/// `now` (unix seconds) bounds the block time of a transfer without
/// `metadata.blockTimestamp` (`eth_block_time_upper_bound`).
pub(crate) fn native_eth_transfers(
    transfers: Vec<AssetTransfer>,
    address: &str,
    tip: u64,
    now: i64,
) -> Vec<DetectedOnChainTransfer> {
    // BTreeMap: deterministic order; (amount, block, block time).
    let mut by_hash: BTreeMap<String, (u128, u64, Option<i64>)> = BTreeMap::new();
    // Transactions with a transfer whose value is unreadable: a partial
    // sum would be stored and never corrected (the deposit upsert keeps
    // its amount), so the whole transaction waits for a complete read.
    let mut unreadable: BTreeSet<String> = BTreeSet::new();
    for transfer in transfers {
        let is_eth_to_address = transfer.to.as_deref().is_some_and(|to| to.eq_ignore_ascii_case(address))
            && matches!(transfer.category.as_deref(), Some("external") | Some("internal"))
            && transfer.asset.as_deref() == Some("ETH");
        let Some(raw_contract) = transfer.raw_contract else {
            continue;
        };
        if !is_eth_to_address || raw_contract.address.is_some() {
            continue;
        }
        let (Some(hash), Some(block)) = (transfer.hash, transfer.block_num) else {
            continue;
        };
        let value = raw_contract.value.as_deref().and_then(|v| parse_hex_u128(v).ok());
        let (Some(value), Ok(block)) = (value, parse_hex_u64(&block)) else {
            warn!("ETH transfer in {hash} with an unreadable value or block — transaction skipped");
            unreadable.insert(hash.to_ascii_lowercase());
            continue;
        };
        let block_time = transfer
            .metadata
            .and_then(|metadata| metadata.block_timestamp)
            .and_then(|ts| chrono::DateTime::parse_from_rfc3339(&ts).ok())
            .map(|ts| ts.timestamp());
        let entry = by_hash.entry(hash.to_ascii_lowercase()).or_insert((0, block, block_time));
        entry.0 = entry.0.saturating_add(value);
        entry.1 = entry.1.max(block);
        entry.2 = entry.2.max(block_time);
    }
    by_hash
        .into_iter()
        .filter(|(hash, (amount, _, _))| *amount > 0 && !unreadable.contains(hash))
        .map(|(hash, (amount, block, block_time))| DetectedOnChainTransfer {
            tx_hash: hash,
            amount_raw: amount.to_string(),
            confirmations: confirmations_between(tip, block),
            block_time: Some(
                block_time.unwrap_or_else(|| crate::eth_block_time_upper_bound(now, tip.saturating_sub(block))),
            ),
        })
        .collect()
}

/// A block to read state at: `latest`, or a fixed height (balances at the
/// confirmation depth).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockTag {
    Latest,
    Number(u64),
}

impl BlockTag {
    fn param(&self) -> String {
        match self {
            Self::Latest => "latest".into(),
            Self::Number(n) => format!("0x{n:x}"),
        }
    }
}

/// Outcome of `eth_sendRawTransaction`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EvmSendOutcome {
    /// Accepted, or already in the mempool / mined (a re-broadcast).
    Accepted,
    /// The nonce is already used by a mined transaction.
    NonceTooLow,
    /// Refused for another reason (fee too low, insufficient funds…); the
    /// detail is for the logs.
    Rejected(String),
}

/// What the outbound engine needs from an Ethereum node.
#[async_trait]
pub trait EvmRpc: Send + Sync {
    async fn block_number(&self) -> Result<u64>;
    /// Next base fee and the suggested priority fee.
    async fn fee_quote(&self) -> Result<EvmFees>;
    /// `eth_getTransactionCount(address, pending)`.
    async fn pending_nonce(&self, address: &str) -> Result<u64>;
    /// `eth_getTransactionCount(address, block)`: nonces below it were mined
    /// by that block.
    async fn mined_nonce(&self, address: &str, block: BlockTag) -> Result<u64>;
    async fn estimate_gas(&self, from: &str, to: &str, value: u128, data: &[u8]) -> Result<u64>;
    async fn send_raw_transaction(&self, raw: &[u8]) -> Result<EvmSendOutcome>;
    async fn receipt(&self, tx_hash: &str) -> Result<Option<EvmReceipt>>;
    /// `eth_getTransactionByHash` is not null: in the node's mempool or mined.
    async fn is_known(&self, tx_hash: &str) -> Result<bool>;
    async fn eth_balance(&self, address: &str, block: BlockTag) -> Result<u128>;
    async fn erc20_balance(&self, contract: &str, address: &str, block: BlockTag) -> Result<u128>;
}

impl ChainClients {
    fn eth_rpc(&self) -> Result<String> {
        let key = self
            .api_key(domain::Network::Ethereum)
            .context("ALCHEMY_API_KEY not configured")?;
        Ok(self.config.eth_rpc_url(key))
    }

    /// Native ETH paid to `address` within the scan window (same as USDT).
    pub async fn poll_ethereum_native(&self, address: &str) -> Result<Vec<DetectedOnChainTransfer>> {
        let Ok(rpc) = self.eth_rpc() else {
            return Ok(vec![]);
        };
        let tip = crate::eth_block_number(&self.http, &rpc).await?;
        let mut transfers = Vec::new();
        let mut page_key: Option<String> = None;
        for _ in 0..ASSET_TRANSFER_MAX_PAGES {
            let mut params = serde_json::json!({
                "fromBlock": format!("0x{:x}", tip.saturating_sub(ETH_LOG_SCAN_BLOCKS)),
                "toBlock": format!("0x{tip:x}"),
                "toAddress": address,
                "category": ["external", "internal"],
                "excludeZeroValue": true,
                // The block timestamp: a transfer older than its payment is
                // not a deposit of it.
                "withMetadata": true,
                "order": "asc",
                "maxCount": "0x3e8",
            });
            if let Some(key) = &page_key {
                params["pageKey"] = serde_json::json!(key);
            }
            let page: AssetTransfersPage =
                json_rpc(&self.http, &rpc, "alchemy_getAssetTransfers", serde_json::json!([params])).await?;
            transfers.extend(page.transfers);
            page_key = page.page_key.filter(|key| !key.is_empty());
            if page_key.is_none() {
                break;
            }
        }
        if page_key.is_some() {
            warn!("alchemy_getAssetTransfers: more than {ASSET_TRANSFER_MAX_PAGES} pages for {address} — the rest waits for the next poll");
        }
        Ok(native_eth_transfers(transfers, address, tip, chrono::Utc::now().timestamp()))
    }
}

#[async_trait]
impl EvmRpc for ChainClients {
    async fn block_number(&self) -> Result<u64> {
        crate::eth_block_number(&self.http, &self.eth_rpc()?).await
    }

    async fn fee_quote(&self) -> Result<EvmFees> {
        let rpc = self.eth_rpc()?;
        #[derive(Deserialize)]
        struct FeeHistory {
            #[serde(rename = "baseFeePerGas")]
            base_fee_per_gas: Vec<String>,
        }
        let history: FeeHistory =
            json_rpc(&self.http, &rpc, "eth_feeHistory", serde_json::json!(["0x1", "latest", []])).await?;
        // The last entry is the next block's base fee.
        let base_fee = history
            .base_fee_per_gas
            .last()
            .context("eth_feeHistory returned no base fee")
            .and_then(|v| parse_hex_u128(v))?;
        let priority: String =
            json_rpc(&self.http, &rpc, "eth_maxPriorityFeePerGas", serde_json::json!([])).await?;
        Ok(EvmFees::from_base_fee(base_fee, parse_hex_u128(&priority)?))
    }

    async fn pending_nonce(&self, address: &str) -> Result<u64> {
        let count: String = json_rpc(
            &self.http,
            &self.eth_rpc()?,
            "eth_getTransactionCount",
            serde_json::json!([address, "pending"]),
        )
        .await?;
        parse_hex_u64(&count)
    }

    async fn mined_nonce(&self, address: &str, block: BlockTag) -> Result<u64> {
        let count: String = json_rpc(
            &self.http,
            &self.eth_rpc()?,
            "eth_getTransactionCount",
            serde_json::json!([address, block.param()]),
        )
        .await?;
        parse_hex_u64(&count)
    }

    async fn estimate_gas(&self, from: &str, to: &str, value: u128, data: &[u8]) -> Result<u64> {
        let gas: String = json_rpc(
            &self.http,
            &self.eth_rpc()?,
            "eth_estimateGas",
            serde_json::json!([{
                "from": from,
                "to": to,
                "value": format!("0x{value:x}"),
                "data": format!("0x{}", hex::encode(data)),
            }]),
        )
        .await?;
        parse_hex_u64(&gas)
    }

    async fn send_raw_transaction(&self, raw: &[u8]) -> Result<EvmSendOutcome> {
        let result: Result<String> = json_rpc(
            &self.http,
            &self.eth_rpc()?,
            "eth_sendRawTransaction",
            serde_json::json!([format!("0x{}", hex::encode(raw))]),
        )
        .await;
        match result {
            Ok(_) => Ok(EvmSendOutcome::Accepted),
            Err(err) => Ok(classify_send_error(&format!("{err:#}"))),
        }
    }

    async fn receipt(&self, tx_hash: &str) -> Result<Option<EvmReceipt>> {
        let rpc = self.eth_rpc()?;
        let body = serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "eth_getTransactionReceipt", "params": [tx_hash]
        });
        // `result: null` while pending: not an error.
        let resp: crate::RpcResponse<RawReceipt> = crate::request::send_json(self.http.post(rpc).json(&body))
            .await
            .context("RPC eth_getTransactionReceipt request failed")?;
        if let Some(error) = resp.error {
            anyhow::bail!("RPC eth_getTransactionReceipt failed: {}", error.describe());
        }
        resp.result.map(parse_receipt).transpose()
    }

    async fn is_known(&self, tx_hash: &str) -> Result<bool> {
        let rpc = self.eth_rpc()?;
        let body = serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "eth_getTransactionByHash", "params": [tx_hash]
        });
        // `result: null` when the node does not know it: not an error.
        let resp: crate::RpcResponse<serde_json::Value> = crate::request::send_json(self.http.post(rpc).json(&body))
            .await
            .context("RPC eth_getTransactionByHash request failed")?;
        if let Some(error) = resp.error {
            anyhow::bail!("RPC eth_getTransactionByHash failed: {}", error.describe());
        }
        Ok(resp.result.is_some_and(|tx| !tx.is_null()))
    }

    async fn eth_balance(&self, address: &str, block: BlockTag) -> Result<u128> {
        let balance: String = json_rpc(
            &self.http,
            &self.eth_rpc()?,
            "eth_getBalance",
            serde_json::json!([address, block.param()]),
        )
        .await?;
        parse_hex_u128(&balance)
    }

    async fn erc20_balance(&self, contract: &str, address: &str, block: BlockTag) -> Result<u128> {
        // balanceOf(address) selector = 0x70a08231
        let data = format!("0x70a08231{}", crate::pad_address_no_prefix(address)?);
        let result: String = json_rpc(
            &self.http,
            &self.eth_rpc()?,
            "eth_call",
            serde_json::json!([{ "to": contract, "data": data }, block.param()]),
        )
        .await?;
        parse_hex_u128(&result)
    }
}

/// Map a node's refusal of a raw transaction: re-broadcasting a known or
/// mined transaction is not an error.
pub fn classify_send_error(message: &str) -> EvmSendOutcome {
    let lower = message.to_ascii_lowercase();
    if lower.contains("already known") || lower.contains("known transaction") || lower.contains("alreadyknown") {
        EvmSendOutcome::Accepted
    } else if lower.contains("nonce too low") {
        EvmSendOutcome::NonceTooLow
    } else {
        EvmSendOutcome::Rejected(message.chars().take(300).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use k256::ecdsa::{RecoveryId, Signature as K256Signature, VerifyingKey};
    use sha3::{Digest, Keccak256};
    use wallet::{RecoverableSignature, WalletError, WalletService};

    /// Answers with a fixed signature, after checking the digest it is asked
    /// to sign.
    struct FixedSigner {
        expected_digest: [u8; 32],
        signature: RecoverableSignature,
    }

    impl Signer for FixedSigner {
        fn address(&self, _: KeyRef) -> Result<String, WalletError> {
            unimplemented!()
        }
        fn public_key(&self, _: KeyRef) -> Result<Vec<u8>, WalletError> {
            unimplemented!()
        }
        fn sign_secp256k1_recoverable(&self, _: KeyRef, digest: [u8; 32]) -> Result<RecoverableSignature, WalletError> {
            assert_eq!(digest, self.expected_digest, "signing payload hash");
            Ok(self.signature)
        }
        fn sign_p2wpkh(&self, _: KeyRef, _: [u8; 32]) -> Result<wallet::P2wpkhWitness, WalletError> {
            unimplemented!()
        }
        fn sign_ed25519(&self, _: KeyRef, _: &[u8]) -> Result<[u8; 64], WalletError> {
            unimplemented!()
        }
    }

    fn bytes32(hex_str: &str) -> [u8; 32] {
        hex::decode(hex_str).unwrap().try_into().unwrap()
    }

    fn recover(digest: &[u8; 32], signature: &RecoverableSignature) -> String {
        let mut rs = [0u8; 64];
        rs[..32].copy_from_slice(&signature.r);
        rs[32..].copy_from_slice(&signature.s);
        let key = VerifyingKey::recover_from_prehash(
            digest,
            &K256Signature::from_slice(&rs).unwrap(),
            RecoveryId::from_byte(signature.recovery_id).unwrap(),
        )
        .unwrap();
        let point = key.to_encoded_point(false);
        format!("0x{}", hex::encode(&Keccak256::digest(&point.as_bytes()[1..])[12..]))
    }

    /// A mainnet EIP-1559 transaction (alloy-consensus' `recover_signer_eip1559`
    /// vector): our encoding yields its signing hash, its transaction hash,
    /// and its signature recovers its sender.
    #[test]
    fn eip1559_matches_a_published_mainnet_transaction() {
        let request = EvmTxRequest {
            chain_id: 1,
            nonce: 0x42,
            to: "0x6069a6c32cf691f5982febae4faf8a6f3ab2f0f6".into(),
            value: 0,
            data: hex::decode("a22cb4650000000000000000000000005eee75727d804a2b13038928d36f8b188945a57a0000000000000000000000000000000000000000000000000000000000000000").unwrap(),
            gas_limit: 44386,
            fees: EvmFees {
                max_fee_per_gas: 0x4a817c800,
                max_priority_fee_per_gas: 0x3b9aca00,
            },
        };
        let signature = RecoverableSignature {
            r: bytes32("840cfc572845f5786e702984c2a582528cad4b49b2a10b9db1be7fca90058565"),
            s: bytes32("25e7109ceb98168d95b09b18bbf6b685130e0562f233877d492b94eee0c5b6d1"),
            recovery_id: 0,
        };
        let digest = bytes32("0d5688ac3897124635b6cf1bc0e29d6dfebceebdc10a54d74f2ef8b56535b682");
        let signer = FixedSigner { expected_digest: digest, signature };
        let signed = sign_eip1559(&request, &signer, KeyRef::Treasury(domain::Network::Ethereum)).unwrap();
        assert_eq!(signed.hash, "0x0ec0b6a2df4d87424e5f6ad2a654e27aaeb7dac20ae9e8385cc09087ad532ee0");
        assert_eq!(signed.raw[0], 0x02, "type-2 envelope");
        assert_eq!(
            format!("0x{}", hex::encode(Keccak256::digest(&signed.raw))),
            signed.hash
        );
        assert_eq!(recover(&digest, &signature), "0xdd6b8b3dc6b7ad97db52f08a275ff4483e024cea");
    }

    #[test]
    fn the_wallet_signs_from_the_treasury_address() {
        let wallet = WalletService::from_mnemonic(
            "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about",
        )
        .unwrap();
        let key = KeyRef::Treasury(domain::Network::Ethereum);
        let request = EvmTxRequest {
            chain_id: 11_155_111,
            nonce: 3,
            to: "0x5aAeb6053F3E94C9b9A09f33669435E7Ef1BeAed".into(),
            value: 50_000_000_000_000_000,
            data: vec![],
            gas_limit: ETH_TRANSFER_GAS,
            fees: EvmFees::from_base_fee(20_000_000_000, 1_000_000_000),
        };
        let signed = sign_eip1559(&request, &wallet, key).unwrap();
        // Re-sign through the wallet to recover the sender of that payload.
        let digest = unsigned(&request).unwrap().signature_hash().0;
        let signature = wallet.sign_secp256k1_recoverable(key, digest).unwrap();
        assert_eq!(
            recover(&digest, &signature),
            wallet.address(key).unwrap().to_ascii_lowercase()
        );
        // Deterministic (RFC 6979): signing twice gives the same bytes.
        assert_eq!(sign_eip1559(&request, &wallet, key).unwrap(), signed);
    }

    #[test]
    fn erc20_transfer_calldata() {
        let data = erc20_transfer_data("0x5aAeb6053F3E94C9b9A09f33669435E7Ef1BeAed", 25_000_000).unwrap();
        assert_eq!(
            hex::encode(data),
            "a9059cbb0000000000000000000000005aaeb6053f3e94c9b9a09f33669435e7ef1beaed00000000000000000000000000000000000000000000000000000000017d7840"
        );
        assert!(erc20_transfer_data("0x123", 1).is_err());
    }

    #[test]
    fn fee_math() {
        let fees = EvmFees::from_base_fee(30_000_000_000, 2_000_000_000);
        assert_eq!(fees.max_fee_per_gas, 62_000_000_000);
        assert_eq!(fees.max_cost(21_000), 62_000_000_000 * 21_000);
        let bumped = fees.bumped(25);
        assert_eq!(bumped.max_fee_per_gas, 77_500_000_000);
        assert_eq!(bumped.max_priority_fee_per_gas, 2_500_000_000);
        assert!(bumped.replaces(&fees));
        // Below the replacement minimum, the bump is raised to 10 %.
        let small = fees.bumped(5);
        assert_eq!(small.max_fee_per_gas, 68_200_000_000);
        assert!(small.replaces(&fees));
        assert!(!fees.replaces(&fees));
        // Tiny values still strictly grow.
        let one = EvmFees { max_fee_per_gas: 1, max_priority_fee_per_gas: 0 }.bumped(25);
        assert_eq!(one, EvmFees { max_fee_per_gas: 2, max_priority_fee_per_gas: 1 });
    }

    #[test]
    fn receipts_report_status_block_and_fee() {
        let receipt: RawReceipt = serde_json::from_str(
            r#"{"status":"0x1","blockNumber":"0x14a3c2f","gasUsed":"0xbaf2","effectiveGasPrice":"0x4a817c800","logs":[]}"#,
        )
        .unwrap();
        let receipt = parse_receipt(receipt).unwrap();
        assert!(receipt.success);
        assert_eq!(receipt.block_number, 0x14a3c2f);
        assert_eq!(receipt.fee_wei, 0xbaf2 * 0x4a817c800);
        let reverted: RawReceipt =
            serde_json::from_str(r#"{"status":"0x0","blockNumber":"0x1","gasUsed":"0x5208","effectiveGasPrice":"0x1"}"#).unwrap();
        assert!(!parse_receipt(reverted).unwrap().success);
    }

    #[test]
    fn send_errors_are_classified() {
        assert_eq!(classify_send_error("RPC eth_sendRawTransaction failed: -32000 already known"), EvmSendOutcome::Accepted);
        assert_eq!(classify_send_error("-32000 nonce too low: next nonce 8, tx nonce 7"), EvmSendOutcome::NonceTooLow);
        assert!(matches!(classify_send_error("-32000 replacement transaction underpriced"), EvmSendOutcome::Rejected(_)));
    }

    const ADDRESS: &str = "0x9858EfFD232B4033E47d90003D41EC34EcaEda94";

    /// `alchemy_getAssetTransfers` answer, shaped like Alchemy's documented
    /// output: one external transfer, a contract forwarding ETH twice in one
    /// tx (two internal transfers), a token transfer, a transfer elsewhere,
    /// one without `rawContract.value`, and the next page key.
    const ASSET_TRANSFERS: &str = r#"{
        "transfers": [
            {
                "blockNum": "0x14a3c00",
                "uniqueId": "0xaaa1:external",
                "hash": "0xaaa1",
                "from": "0x1111111111111111111111111111111111111111",
                "to": "0x9858effd232b4033e47d90003d41ec34ecaeda94",
                "value": 0.05,
                "erc721TokenId": null,
                "erc1155Metadata": null,
                "tokenId": null,
                "asset": "ETH",
                "category": "external",
                "rawContract": { "value": "0xb1a2bc2ec50000", "address": null, "decimal": "0x12" }
            },
            {
                "blockNum": "0x14a3c05",
                "uniqueId": "0xbbb2:internal:0",
                "hash": "0xBBB2",
                "from": "0x2222222222222222222222222222222222222222",
                "to": "0x9858effd232b4033e47d90003d41ec34ecaeda94",
                "value": 0.01,
                "asset": "ETH",
                "category": "internal",
                "rawContract": { "value": "0x2386f26fc10000", "address": null, "decimal": "0x12" }
            },
            {
                "blockNum": "0x14a3c05",
                "uniqueId": "0xbbb2:internal:1",
                "hash": "0xbbb2",
                "from": "0x2222222222222222222222222222222222222222",
                "to": "0x9858effd232b4033e47d90003d41ec34ecaeda94",
                "value": 0.03,
                "asset": "ETH",
                "category": "internal",
                "rawContract": { "value": "0x6a94d74f430000", "address": null, "decimal": "0x12" }
            },
            {
                "blockNum": "0x14a3c06",
                "uniqueId": "0xccc3:log:7",
                "hash": "0xccc3",
                "to": "0x9858effd232b4033e47d90003d41ec34ecaeda94",
                "value": 25,
                "asset": "USDT",
                "category": "erc20",
                "rawContract": { "value": "0x17d7840", "address": "0xdac17f958d2ee523a2206206994597c13d831ec7", "decimal": "0x6" }
            },
            {
                "blockNum": "0x14a3c07",
                "uniqueId": "0xddd4:external",
                "hash": "0xddd4",
                "to": "0x3333333333333333333333333333333333333333",
                "value": 1,
                "asset": "ETH",
                "category": "external",
                "rawContract": { "value": "0xde0b6b3a7640000", "address": null, "decimal": "0x12" }
            },
            {
                "blockNum": "0x14a3c08",
                "uniqueId": "0xeee5:external",
                "hash": "0xeee5",
                "to": "0x9858effd232b4033e47d90003d41ec34ecaeda94",
                "value": 2,
                "asset": "ETH",
                "category": "external",
                "rawContract": { "address": null, "decimal": "0x12" }
            }
        ],
        "pageKey": "6f9e3c6a-3c5b-4ad8-9d5b-6dfb1e09d0a4"
    }"#;

    #[test]
    fn native_transfers_are_summed_per_transaction() {
        let page: AssetTransfersPage = serde_json::from_str(ASSET_TRANSFERS).unwrap();
        assert_eq!(page.page_key.as_deref(), Some("6f9e3c6a-3c5b-4ad8-9d5b-6dfb1e09d0a4"));
        let detected = native_eth_transfers(page.transfers, ADDRESS, 0x14a3c00 + 11, 1_800_000_000);
        assert_eq!(detected.len(), 2);
        assert_eq!(detected[0].tx_hash, "0xaaa1");
        assert_eq!(detected[0].amount_raw, "50000000000000000");
        assert_eq!(detected[0].confirmations, 12);
        // Two internal transfers of one tx: 0.01 + 0.03 ETH, hash
        // case-normalized.
        assert_eq!(detected[1].tx_hash, "0xbbb2");
        assert_eq!(detected[1].amount_raw, "40000000000000000");
        assert_eq!(detected[1].confirmations, 7);
    }

    #[test]
    fn native_eth_mined_before_the_payment_is_not_a_deposit() {
        let transfer = |metadata: &str, block: &str| {
            format!(
                r#"{{"blockNum": "{block}", "hash": "0x{block}", "to": "0x9858effd232b4033e47d90003d41ec34ecaeda94",
                    "asset": "ETH", "category": "external", "rawContract": {{"value": "0x1", "address": null}}{metadata}}}"#
            )
        };
        let now = 1_800_000_000;
        let created = now - 3_600;
        let page: AssetTransfersPage = serde_json::from_str(&format!(
            r#"{{"transfers": [{}, {}, {}]}}"#,
            // `withMetadata`: the block timestamp, a day before the payment…
            transfer(r#", "metadata": {"blockTimestamp": "2027-01-14T08:00:00.000Z"}"#, "0x10"),
            // …or 5 minutes after it.
            transfer(r#", "metadata": {"blockTimestamp": "2027-01-15T07:05:00.000Z"}"#, "0x11"),
            // No metadata: the 12 s-per-block bound (0x20 − 0x12 = 14 blocks).
            transfer("", "0x12"),
        ))
        .unwrap();
        let detected = native_eth_transfers(page.transfers, ADDRESS, 0x20, now);
        let times: Vec<Option<i64>> = detected.iter().map(|t| t.block_time).collect();
        assert_eq!(times, vec![Some(1_799_913_600), Some(1_799_996_700), Some(now - 14 * 12)]);
        let predates: Vec<bool> = times.iter().map(|t| domain::predates_payment(*t, created)).collect();
        assert_eq!(predates, vec![true, false, false]);
    }

    #[test]
    fn last_page_and_hex_edge_cases() {
        let page: AssetTransfersPage = serde_json::from_str(r#"{"transfers": []}"#).unwrap();
        assert!(page.page_key.is_none());
        let page: AssetTransfersPage = serde_json::from_str(
            r#"{"transfers": [{"blockNum": "0x10", "hash": "0xff", "to": "0x9858effd232b4033e47d90003d41ec34ecaeda94",
                "asset": "ETH", "category": "external", "rawContract": {"value": "0x0", "address": null}}]}"#,
        )
        .unwrap();
        // A zero transfer settles nothing.
        assert!(native_eth_transfers(page.transfers, ADDRESS, 0x10, 1_800_000_000).is_empty());
        // One readable and one unreadable transfer in the same transaction:
        // a partial sum would be stored and never corrected (the deposit
        // upsert keeps its amount), so the whole transaction is skipped
        // until a later poll reads it entirely.
        let page: AssetTransfersPage = serde_json::from_str(
            r#"{"transfers": [
                {"blockNum": "0x10", "hash": "0xab", "to": "0x9858effd232b4033e47d90003d41ec34ecaeda94",
                 "asset": "ETH", "category": "internal", "rawContract": {"value": "0x2386f26fc10000", "address": null}},
                {"blockNum": "0x10", "hash": "0xab", "to": "0x9858effd232b4033e47d90003d41ec34ecaeda94",
                 "asset": "ETH", "category": "internal", "rawContract": {"address": null}},
                {"blockNum": "0x11", "hash": "0xcd", "to": "0x9858effd232b4033e47d90003d41ec34ecaeda94",
                 "asset": "ETH", "category": "external", "rawContract": {"value": "0x1", "address": null}}
            ]}"#,
        )
        .unwrap();
        let detected = native_eth_transfers(page.transfers, ADDRESS, 0x11, 1_800_000_000);
        assert_eq!(detected.len(), 1);
        assert_eq!(detected[0].tx_hash, "0xcd");
        // Same rule for a transfer whose block is unreadable.
        let page: AssetTransfersPage = serde_json::from_str(
            r#"{"transfers": [
                {"blockNum": "0x10", "hash": "0xef", "to": "0x9858effd232b4033e47d90003d41ec34ecaeda94",
                 "asset": "ETH", "category": "internal", "rawContract": {"value": "0x1", "address": null}},
                {"blockNum": "nope", "hash": "0xef", "to": "0x9858effd232b4033e47d90003d41ec34ecaeda94",
                 "asset": "ETH", "category": "internal", "rawContract": {"value": "0x1", "address": null}}
            ]}"#,
        )
        .unwrap();
        assert!(native_eth_transfers(page.transfers, ADDRESS, 0x11, 1_800_000_000).is_empty());
        // Wei beyond u64.
        assert_eq!(parse_hex_u128("0x3635c9adc5dea00000").unwrap(), 1_000 * 10u128.pow(18));
        assert_eq!(parse_hex_u128("0x").unwrap(), 0);
    }
}
