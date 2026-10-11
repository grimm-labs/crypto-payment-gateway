//! Tron transactions. The node builds them (`wallet/triggersmartcontract`,
//! `wallet/createtransaction`); the gateway never signs what it has not
//! decoded and checked: `raw_data_hex` is parsed with hand-written protobuf
//! messages, must re-encode to the exact same bytes (no unknown or extra
//! field slips through), and its owner, contract, selector, recipient,
//! amount and fee limit must be the requested ones. The txid is recomputed
//! (`sha256(raw_data)`), and the signed transaction is the original
//! `raw_data` bytes plus the 65-byte signature, broadcast as protobuf hex.

use anyhow::{Context, Result};
use async_trait::async_trait;
use prost::Message;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use wallet::{KeyRef, Signer};

use crate::destination::tron_decode;
use crate::{request, ChainClients};

/// `transfer(address,uint256)`.
const TRANSFER_SELECTOR: [u8; 4] = [0xa9, 0x05, 0x9c, 0xbb];
const TRIGGER_SMART_CONTRACT: i32 = 31;
const TRANSFER_CONTRACT: i32 = 1;
const TRIGGER_TYPE_URL: &str = "type.googleapis.com/protocol.TriggerSmartContract";
const TRANSFER_TYPE_URL: &str = "type.googleapis.com/protocol.TransferContract";
/// Upper bound of a node-chosen expiration (TAPOS is ~60 s by default).
const MAX_EXPIRATION_MS: i64 = 24 * 60 * 60 * 1000;
/// Bytes of a signed TRC-20 transfer, for the bandwidth estimate.
pub const TRC20_TRANSFER_BANDWIDTH: u64 = 345;
/// Bytes of a signed TRX transfer.
pub const TRX_TRANSFER_BANDWIDTH: u64 = 270;
/// Energy fee limit = estimate × this percent (spec: × 1.5).
pub const FEE_LIMIT_PERCENT: u64 = 150;

/// `protocol.Transaction.raw` (only the fields a payment uses; anything
/// else makes the re-encoding differ and the transaction refused).
#[derive(Clone, PartialEq, Message)]
pub struct TransactionRaw {
    #[prost(bytes = "vec", tag = "1")]
    pub ref_block_bytes: Vec<u8>,
    #[prost(int64, tag = "3")]
    pub ref_block_num: i64,
    #[prost(bytes = "vec", tag = "4")]
    pub ref_block_hash: Vec<u8>,
    #[prost(int64, tag = "8")]
    pub expiration: i64,
    /// Memo — never set by the gateway.
    #[prost(bytes = "vec", tag = "10")]
    pub data: Vec<u8>,
    #[prost(message, repeated, tag = "11")]
    pub contract: Vec<Contract>,
    #[prost(bytes = "vec", tag = "12")]
    pub scripts: Vec<u8>,
    #[prost(int64, tag = "14")]
    pub timestamp: i64,
    #[prost(int64, tag = "18")]
    pub fee_limit: i64,
}

#[derive(Clone, PartialEq, Message)]
pub struct Contract {
    #[prost(int32, tag = "1")]
    pub r#type: i32,
    #[prost(message, optional, tag = "2")]
    pub parameter: Option<Any>,
    #[prost(bytes = "vec", tag = "3")]
    pub provider: Vec<u8>,
    #[prost(bytes = "vec", tag = "4")]
    pub contract_name: Vec<u8>,
    #[prost(int32, tag = "5")]
    pub permission_id: i32,
}

/// `google.protobuf.Any`.
#[derive(Clone, PartialEq, Message)]
pub struct Any {
    #[prost(string, tag = "1")]
    pub type_url: String,
    #[prost(bytes = "vec", tag = "2")]
    pub value: Vec<u8>,
}

#[derive(Clone, PartialEq, Message)]
pub struct TriggerSmartContract {
    #[prost(bytes = "vec", tag = "1")]
    pub owner_address: Vec<u8>,
    #[prost(bytes = "vec", tag = "2")]
    pub contract_address: Vec<u8>,
    #[prost(int64, tag = "3")]
    pub call_value: i64,
    #[prost(bytes = "vec", tag = "4")]
    pub data: Vec<u8>,
    #[prost(int64, tag = "5")]
    pub call_token_value: i64,
    #[prost(int64, tag = "6")]
    pub token_id: i64,
}

#[derive(Clone, PartialEq, Message)]
pub struct TransferContract {
    #[prost(bytes = "vec", tag = "1")]
    pub owner_address: Vec<u8>,
    #[prost(bytes = "vec", tag = "2")]
    pub to_address: Vec<u8>,
    #[prost(int64, tag = "3")]
    pub amount: i64,
}

/// What the gateway asked the node to build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TronTransfer {
    /// TRC-20 `transfer` (USDT).
    Trc20 {
        owner: String,
        contract: String,
        recipient: String,
        amount: u128,
        fee_limit: u64,
    },
    /// Native TRX.
    Trx { owner: String, recipient: String, amount: u64 },
}

/// A node-built transaction that passed every check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedTronTx {
    /// The exact `raw_data` bytes to sign and broadcast.
    pub raw: Vec<u8>,
    /// `sha256(raw)`, hex — the txid.
    pub tx_id: String,
    /// TAPOS expiration, unix milliseconds.
    pub expiration_ms: i64,
    pub fee_limit: u64,
}

/// Why a node-built transaction is refused (never signed).
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum TronVerifyError {
    #[error("raw_data is not valid protobuf")]
    Undecodable,
    #[error("raw_data carries fields the gateway did not ask for")]
    NonCanonical,
    #[error("unexpected transaction shape: {0}")]
    Shape(&'static str),
    #[error("{0} does not match the request")]
    Mismatch(&'static str),
    #[error("txID does not match sha256(raw_data)")]
    TxIdMismatch,
}

fn decode_canonical<M: Message + Default>(bytes: &[u8]) -> Result<M, TronVerifyError> {
    let message = M::decode(bytes).map_err(|_| TronVerifyError::Undecodable)?;
    if message.encode_to_vec() != bytes {
        return Err(TronVerifyError::NonCanonical);
    }
    Ok(message)
}

fn address_bytes(address: &str) -> Result<Vec<u8>, TronVerifyError> {
    tron_decode(address)
        .map(|bytes| bytes.to_vec())
        .map_err(|_| TronVerifyError::Shape("invalid address in the request"))
}

/// `transfer(recipient, amount)` calldata (recipient as 20 bytes, without
/// the `0x41` prefix).
pub fn trc20_transfer_data(recipient: &str, amount: u128) -> Result<Vec<u8>> {
    let recipient = tron_decode(recipient).map_err(|e| anyhow::anyhow!("{e}"))?;
    let mut data = Vec::with_capacity(68);
    data.extend_from_slice(&TRANSFER_SELECTOR);
    data.extend_from_slice(&[0u8; 12]);
    data.extend_from_slice(&recipient[1..]);
    data.extend_from_slice(&[0u8; 16]);
    data.extend_from_slice(&amount.to_be_bytes());
    Ok(data)
}

/// Decode `raw_data_hex` and check it is exactly `expected`; `node_tx_id`
/// (the node's `txID`) must equal the recomputed one. `now_ms` bounds the
/// expiration.
pub fn verify_tron_transaction(
    raw_data_hex: &str,
    node_tx_id: Option<&str>,
    expected: &TronTransfer,
    now_ms: i64,
) -> Result<VerifiedTronTx, TronVerifyError> {
    let raw = hex::decode(raw_data_hex).map_err(|_| TronVerifyError::Undecodable)?;
    let decoded: TransactionRaw = decode_canonical(&raw)?;
    if !decoded.data.is_empty() || !decoded.scripts.is_empty() {
        return Err(TronVerifyError::Shape("memo or scripts present"));
    }
    let [contract] = decoded.contract.as_slice() else {
        return Err(TronVerifyError::Shape("exactly one contract expected"));
    };
    if !contract.provider.is_empty() || !contract.contract_name.is_empty() || contract.permission_id != 0 {
        return Err(TronVerifyError::Shape("unexpected contract options"));
    }
    let parameter = contract
        .parameter
        .as_ref()
        .ok_or(TronVerifyError::Shape("contract without parameter"))?;
    if decoded.expiration <= now_ms || decoded.expiration > now_ms + MAX_EXPIRATION_MS {
        return Err(TronVerifyError::Shape("expiration out of bounds"));
    }

    let fee_limit = match expected {
        TronTransfer::Trc20 {
            owner,
            contract: token,
            recipient,
            amount,
            fee_limit,
        } => {
            if contract.r#type != TRIGGER_SMART_CONTRACT || parameter.type_url != TRIGGER_TYPE_URL {
                return Err(TronVerifyError::Shape("not a TriggerSmartContract"));
            }
            let call: TriggerSmartContract = decode_canonical(&parameter.value)?;
            if call.owner_address != address_bytes(owner)? {
                return Err(TronVerifyError::Mismatch("owner"));
            }
            if call.contract_address != address_bytes(token)? {
                return Err(TronVerifyError::Mismatch("contract"));
            }
            if call.call_value != 0 || call.call_token_value != 0 || call.token_id != 0 {
                return Err(TronVerifyError::Shape("value attached to the call"));
            }
            let expected_data = trc20_transfer_data(recipient, *amount)
                .map_err(|_| TronVerifyError::Shape("invalid recipient in the request"))?;
            if call.data.get(..4) != Some(&TRANSFER_SELECTOR[..]) {
                return Err(TronVerifyError::Mismatch("selector"));
            }
            if call.data.get(4..36) != Some(&expected_data[4..36]) {
                return Err(TronVerifyError::Mismatch("recipient"));
            }
            if call.data != expected_data {
                return Err(TronVerifyError::Mismatch("amount"));
            }
            if u64::try_from(decoded.fee_limit).ok() != Some(*fee_limit) {
                return Err(TronVerifyError::Mismatch("fee_limit"));
            }
            *fee_limit
        }
        TronTransfer::Trx {
            owner,
            recipient,
            amount,
        } => {
            if contract.r#type != TRANSFER_CONTRACT || parameter.type_url != TRANSFER_TYPE_URL {
                return Err(TronVerifyError::Shape("not a TransferContract"));
            }
            let transfer: TransferContract = decode_canonical(&parameter.value)?;
            if transfer.owner_address != address_bytes(owner)? {
                return Err(TronVerifyError::Mismatch("owner"));
            }
            if transfer.to_address != address_bytes(recipient)? {
                return Err(TronVerifyError::Mismatch("recipient"));
            }
            if u64::try_from(transfer.amount).ok() != Some(*amount) {
                return Err(TronVerifyError::Mismatch("amount"));
            }
            if decoded.fee_limit != 0 {
                return Err(TronVerifyError::Shape("fee_limit on a TRX transfer"));
            }
            0
        }
    };

    let tx_id = hex::encode(Sha256::digest(&raw));
    if node_tx_id.is_some_and(|id| !id.eq_ignore_ascii_case(&tx_id)) {
        return Err(TronVerifyError::TxIdMismatch);
    }
    Ok(VerifiedTronTx {
        raw,
        tx_id,
        expiration_ms: decoded.expiration,
        fee_limit,
    })
}

/// Sign a verified transaction: secp256k1 over the txid, 65-byte `r‖s‖v`,
/// then `protocol.Transaction { raw_data = <original bytes>, signature }`.
pub fn sign_tron(verified: &VerifiedTronTx, signer: &dyn Signer, key: KeyRef) -> Result<Vec<u8>> {
    let digest: [u8; 32] = Sha256::digest(&verified.raw).into();
    let signature = signer
        .sign_secp256k1_recoverable(key, digest)
        .context("Tron signing failed")?
        .to_tron_bytes();
    let mut out = Vec::with_capacity(verified.raw.len() + 80);
    out.push(0x0a); // field 1, length-delimited
    prost::encoding::encode_varint(verified.raw.len() as u64, &mut out);
    out.extend_from_slice(&verified.raw);
    out.push(0x12); // field 2, length-delimited
    prost::encoding::encode_varint(signature.len() as u64, &mut out);
    out.extend_from_slice(&signature);
    Ok(out)
}

/// `fee_limit` for a call estimated at `energy`: estimate × price × 1.5,
/// capped (spec §7.7).
pub fn fee_limit_sun(energy: u64, energy_price_sun: u64, cap_sun: u64) -> u64 {
    (energy.saturating_mul(energy_price_sun).saturating_mul(FEE_LIMIT_PERCENT) / 100).min(cap_sun)
}

/// Network prices read from `wallet/getchainparameters` (never hard-coded).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TronChainFees {
    /// `getEnergyFee`: sun burnt per energy unit.
    pub energy_sun: u64,
    /// `getTransactionFee`: sun burnt per bandwidth byte.
    pub bandwidth_sun: u64,
    /// `getCreateAccountFee`: what activating an account by a TRX transfer
    /// burns on top.
    pub create_account_sun: u64,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ChainParameters {
    #[serde(rename = "chainParameter", default)]
    chain_parameter: Vec<ChainParameter>,
}

#[derive(Debug, Deserialize)]
struct ChainParameter {
    key: String,
    value: Option<i64>,
}

pub(crate) fn parse_chain_fees(params: &ChainParameters) -> Result<TronChainFees> {
    let get = |key: &str| {
        params
            .chain_parameter
            .iter()
            .find(|p| p.key == key)
            .and_then(|p| p.value)
            .and_then(|v| u64::try_from(v).ok())
            .with_context(|| format!("getchainparameters has no {key}"))
    };
    Ok(TronChainFees {
        energy_sun: get("getEnergyFee")?,
        bandwidth_sun: get("getTransactionFee")?,
        create_account_sun: get("getCreateAccountFee")?,
    })
}

/// An unsigned transaction as the node answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TronUnsignedTx {
    pub raw_data_hex: String,
    pub tx_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct NodeTx {
    #[serde(rename = "raw_data_hex")]
    raw_data_hex: Option<String>,
    #[serde(rename = "txID")]
    tx_id: Option<String>,
    #[serde(rename = "Error")]
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TriggerResponse {
    transaction: Option<NodeTx>,
    result: Option<TriggerResult>,
    energy_used: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct TriggerResult {
    result: Option<bool>,
    message: Option<String>,
}

/// Outcome of `wallet/broadcasthex`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TronBroadcastOutcome {
    /// Accepted, or already known (`DUP_TRANSACTION_ERROR`: a re-broadcast).
    Accepted,
    /// TAPOS expired: this transaction can never land.
    Expired,
    Rejected(String),
}

#[derive(Debug, Deserialize)]
pub(crate) struct BroadcastResponse {
    result: Option<bool>,
    code: Option<String>,
    message: Option<String>,
}

pub(crate) fn classify_broadcast(response: &BroadcastResponse) -> TronBroadcastOutcome {
    if response.result == Some(true) {
        return TronBroadcastOutcome::Accepted;
    }
    match response.code.as_deref() {
        Some("DUP_TRANSACTION_ERROR") => TronBroadcastOutcome::Accepted,
        Some("TRANSACTION_EXPIRATION_ERROR") => TronBroadcastOutcome::Expired,
        code => {
            // `message` is hex-encoded text.
            let message = response
                .message
                .as_deref()
                .and_then(|m| hex::decode(m).ok())
                .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                .unwrap_or_default();
            TronBroadcastOutcome::Rejected(
                format!("{} {message}", code.unwrap_or("UNKNOWN")).chars().take(300).collect(),
            )
        }
    }
}

/// A solidified transaction's outcome (`walletsolidity/gettransactioninfobyid`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TronTxOutcome {
    pub block: u64,
    /// Contract result `SUCCESS` (TRC-20), or a plain transfer that landed.
    pub success: bool,
    /// Sun burnt (energy + bandwidth).
    pub fee_sun: u128,
}

#[derive(Debug, Deserialize)]
pub(crate) struct TxInfoFull {
    id: Option<String>,
    #[serde(rename = "blockNumber")]
    block_number: Option<u64>,
    fee: Option<u64>,
    result: Option<String>,
    receipt: Option<ReceiptFull>,
}

#[derive(Debug, Deserialize)]
struct ReceiptFull {
    result: Option<String>,
}

pub(crate) fn parse_tx_outcome(info: &TxInfoFull, tx_id: &str) -> Option<TronTxOutcome> {
    if !info.id.as_deref().is_some_and(|id| id.eq_ignore_ascii_case(tx_id)) {
        return None;
    }
    let block = info.block_number.filter(|b| *b > 0)?;
    let failed = matches!(info.result.as_deref(), Some("FAILED"));
    let contract_ok = match info.receipt.as_ref().and_then(|r| r.result.as_deref()) {
        // Plain TRX transfers carry no contract result.
        None => true,
        Some(result) => result == "SUCCESS",
    };
    Some(TronTxOutcome {
        block,
        success: !failed && contract_ok,
        fee_sun: u128::from(info.fee.unwrap_or(0)),
    })
}

/// What the outbound engine needs from a Tron node.
#[async_trait]
pub trait TronRpc: Send + Sync {
    async fn head_block(&self) -> Result<u64>;
    async fn chain_fees(&self) -> Result<TronChainFees>;
    /// Energy a `transfer` would use now (`triggerconstantcontract`, the
    /// dynamic-energy penalty included).
    async fn estimate_trc20_energy(&self, owner: &str, contract: &str, recipient: &str, amount: u128) -> Result<u64>;
    async fn build_trc20_transfer(
        &self,
        owner: &str,
        contract: &str,
        recipient: &str,
        amount: u128,
        fee_limit: u64,
    ) -> Result<TronUnsignedTx>;
    async fn build_trx_transfer(&self, owner: &str, recipient: &str, amount: u64) -> Result<TronUnsignedTx>;
    /// `None` when the account does not exist (never activated).
    async fn account_balance(&self, address: &str) -> Result<Option<u128>>;
    /// TRX balance on the solidity node (confirmed state).
    async fn solidity_trx_balance(&self, address: &str) -> Result<u128>;
    async fn solidity_trc20_balance(&self, contract: &str, address: &str) -> Result<u128>;
    async fn broadcast(&self, signed: &[u8]) -> Result<TronBroadcastOutcome>;
    /// The solidified outcome, `None` while not solidified.
    async fn solidified_outcome(&self, tx_id: &str) -> Result<Option<TronTxOutcome>>;
    /// Whether the full node knows the transaction (mempool or block).
    async fn is_known(&self, tx_id: &str) -> Result<bool>;
}

impl ChainClients {
    fn tron_key(&self) -> Result<&str> {
        self.api_key(domain::Network::Tron)
            .context("TRONGRID_API_KEY not configured")
    }

    async fn tron_post<T: serde::de::DeserializeOwned>(&self, path: &str, body: serde_json::Value) -> Result<T> {
        let key = self.tron_key()?;
        request::send_json(
            self.http
                .post(self.config.trongrid_url(path))
                .header("TRON-PRO-API-KEY", key)
                .json(&body),
        )
        .await
        .with_context(|| format!("TronGrid {path} request failed"))
    }

    fn node_tx(tx: Option<NodeTx>, what: &str) -> Result<TronUnsignedTx> {
        let tx = tx.with_context(|| format!("{what}: no transaction returned"))?;
        if let Some(error) = tx.error {
            anyhow::bail!("{what} failed: {}", error.chars().take(200).collect::<String>());
        }
        Ok(TronUnsignedTx {
            raw_data_hex: tx.raw_data_hex.with_context(|| format!("{what}: no raw_data_hex"))?,
            tx_id: tx.tx_id,
        })
    }

    async fn trigger(&self, path: &str, owner: &str, contract: &str, parameter: &str, fee_limit: Option<u64>) -> Result<TriggerResponse> {
        let mut body = serde_json::json!({
            "owner_address": owner,
            "contract_address": contract,
            "function_selector": "transfer(address,uint256)",
            "parameter": parameter,
            "call_value": 0,
            "visible": true,
        });
        if let Some(fee_limit) = fee_limit {
            body["fee_limit"] = serde_json::json!(fee_limit);
        }
        let response: TriggerResponse = self.tron_post(path, body).await?;
        if let Some(result) = &response.result {
            if result.result != Some(true) {
                let message = result
                    .message
                    .as_deref()
                    .and_then(|m| hex::decode(m).ok())
                    .map(|b| String::from_utf8_lossy(&b).into_owned())
                    .unwrap_or_default();
                anyhow::bail!("{path} refused: {}", message.chars().take(200).collect::<String>());
            }
        }
        Ok(response)
    }
}

fn transfer_parameter(recipient: &str, amount: u128) -> Result<String> {
    Ok(hex::encode(&trc20_transfer_data(recipient, amount)?[4..]))
}

#[async_trait]
impl TronRpc for ChainClients {
    async fn head_block(&self) -> Result<u64> {
        let block: crate::tron::Block = self.tron_post("wallet/getnowblock", serde_json::json!({})).await?;
        block.number().context("Unable to fetch current Tron block number")
    }

    async fn chain_fees(&self) -> Result<TronChainFees> {
        let key = self.tron_key()?;
        let params: ChainParameters = request::send_json(
            self.http
                .get(self.config.trongrid_url("wallet/getchainparameters"))
                .header("TRON-PRO-API-KEY", key),
        )
        .await
        .context("TronGrid getchainparameters request failed")?;
        parse_chain_fees(&params)
    }

    async fn estimate_trc20_energy(&self, owner: &str, contract: &str, recipient: &str, amount: u128) -> Result<u64> {
        let response = self
            .trigger(
                "wallet/triggerconstantcontract",
                owner,
                contract,
                &transfer_parameter(recipient, amount)?,
                None,
            )
            .await?;
        response.energy_used.context("triggerconstantcontract returned no energy_used")
    }

    async fn build_trc20_transfer(
        &self,
        owner: &str,
        contract: &str,
        recipient: &str,
        amount: u128,
        fee_limit: u64,
    ) -> Result<TronUnsignedTx> {
        let response = self
            .trigger(
                "wallet/triggersmartcontract",
                owner,
                contract,
                &transfer_parameter(recipient, amount)?,
                Some(fee_limit),
            )
            .await?;
        Self::node_tx(response.transaction, "triggersmartcontract")
    }

    async fn build_trx_transfer(&self, owner: &str, recipient: &str, amount: u64) -> Result<TronUnsignedTx> {
        let tx: NodeTx = self
            .tron_post(
                "wallet/createtransaction",
                serde_json::json!({ "owner_address": owner, "to_address": recipient, "amount": amount, "visible": true }),
            )
            .await?;
        Self::node_tx(Some(tx), "createtransaction")
    }

    async fn account_balance(&self, address: &str) -> Result<Option<u128>> {
        let account: serde_json::Value = self
            .tron_post("wallet/getaccount", serde_json::json!({ "address": address, "visible": true }))
            .await?;
        // `{}` for an account that was never activated.
        if account.as_object().is_none_or(|o| o.is_empty()) {
            return Ok(None);
        }
        Ok(Some(u128::from(account["balance"].as_u64().unwrap_or(0))))
    }

    async fn solidity_trx_balance(&self, address: &str) -> Result<u128> {
        let account: serde_json::Value = self
            .tron_post("walletsolidity/getaccount", serde_json::json!({ "address": address, "visible": true }))
            .await?;
        Ok(u128::from(account["balance"].as_u64().unwrap_or(0)))
    }

    async fn solidity_trc20_balance(&self, contract: &str, address: &str) -> Result<u128> {
        let owner = tron_decode(address).map_err(|e| anyhow::anyhow!("{e}"))?;
        let response: serde_json::Value = self
            .tron_post(
                "walletsolidity/triggerconstantcontract",
                serde_json::json!({
                    "owner_address": address,
                    "contract_address": contract,
                    "function_selector": "balanceOf(address)",
                    "parameter": format!("{:0>64}", hex::encode(&owner[1..])),
                    "visible": true,
                }),
            )
            .await?;
        let word = response["constant_result"][0]
            .as_str()
            .context("balanceOf returned no constant_result")?;
        crate::parse_hex_u128(word)
    }

    async fn broadcast(&self, signed: &[u8]) -> Result<TronBroadcastOutcome> {
        let response: BroadcastResponse = self
            .tron_post("wallet/broadcasthex", serde_json::json!({ "transaction": hex::encode(signed) }))
            .await?;
        Ok(classify_broadcast(&response))
    }

    async fn solidified_outcome(&self, tx_id: &str) -> Result<Option<TronTxOutcome>> {
        let info: TxInfoFull = self
            .tron_post("walletsolidity/gettransactioninfobyid", serde_json::json!({ "value": tx_id }))
            .await?;
        Ok(parse_tx_outcome(&info, tx_id))
    }

    async fn is_known(&self, tx_id: &str) -> Result<bool> {
        let tx: serde_json::Value = self
            .tron_post("wallet/gettransactionbyid", serde_json::json!({ "value": tx_id }))
            .await?;
        Ok(tx["txID"].as_str().is_some_and(|id| id.eq_ignore_ascii_case(tx_id)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wallet::WalletService;

    /// Real `wallet/triggersmartcontract` answer (TronGrid, 2026-10-11):
    /// 20 USDT from the test mnemonic's treasury to TJDENsf…, fee_limit
    /// 30 TRX.
    const TRC20_RAW: &str = "0a02e69b220883da2fdc40e5cc8e40e8f4b8e492345aae01081f12a9010a31747970652e676f6f676c65617069732e636f6d2f70726f746f636f6c2e54726967676572536d617274436f6e747261637412740a154178839f6054d7ed13918bae0473ba31b1ca9d7265121541a614f803b6fd780986a42c78ec9c7f77e6ded13c2244a9059cbb0000000000000000000000005a67fa7cc56bd6d043a98e17d329c1dc9e14753f0000000000000000000000000000000000000000000000000000000001312d0070e2a7b5e4923490018087a70e";
    const TRC20_TXID: &str = "98f8aa28d7da55f3dc6535619137640249f42c105efaf7ebecda7fee44ce1f79";
    /// Its TAPOS expiration and creation time (unix ms).
    const TRC20_EXPIRATION: i64 = 1_791_748_881_000;
    const TRC20_CREATED: i64 = 1_791_748_821_986;
    const TREASURY: &str = "TLxRmNgk8aegM7DSs1JELYxG2REizYZnnX";
    const USDT: &str = "TR7NHqjeKQxGTCi8q8ZY4pL8otSzgjLj6t";
    const RECIPIENT: &str = "TJDENsfBJs4RFETt1X1W8wMDc8M5XnJhCe";

    /// Real `wallet/createtransaction` answer: 1.5 TRX TDqSquX… → TJDENsf….
    const TRX_RAW: &str = "0a02e6a12208f88f1a8b6ea2d2be40b881bae492345a67080112630a2d747970652e676f6f676c65617069732e636f6d2f70726f746f636f6c2e5472616e73666572436f6e747261637412320a15412a68baf67f1c497d9a4a609276a90dcd6ea774441215415a67fa7cc56bd6d043a98e17d329c1dc9e14753f18e0c65b70bfb6b6e49234";
    const TRX_TXID: &str = "8a2dd9cfc0c6ccc965aa1822f4d8943b4dc0f219a1a6fbf0ebc19389c9bedd89";
    const TRX_OWNER: &str = "TDqSquXBgUCLYvYC4XZgrprLK589dkhSCf";

    fn trc20(recipient: &str, amount: u128, fee_limit: u64) -> TronTransfer {
        TronTransfer::Trc20 {
            owner: TREASURY.into(),
            contract: USDT.into(),
            recipient: recipient.into(),
            amount,
            fee_limit,
        }
    }

    #[test]
    fn a_real_trc20_transfer_is_verified() {
        let verified = verify_tron_transaction(
            TRC20_RAW,
            Some(TRC20_TXID),
            &trc20(RECIPIENT, 20_000_000, 30_000_000),
            TRC20_CREATED,
        )
        .unwrap();
        assert_eq!(verified.tx_id, TRC20_TXID);
        assert_eq!(verified.expiration_ms, TRC20_EXPIRATION);
        assert_eq!(verified.fee_limit, 30_000_000);
        assert_eq!(hex::encode(&verified.raw), TRC20_RAW);
    }

    #[test]
    fn a_trc20_transfer_that_differs_from_the_request_is_refused() {
        let check = |expected: &TronTransfer| {
            verify_tron_transaction(TRC20_RAW, Some(TRC20_TXID), expected, TRC20_CREATED).unwrap_err()
        };
        assert_eq!(check(&trc20(RECIPIENT, 20_000_001, 30_000_000)), TronVerifyError::Mismatch("amount"));
        assert_eq!(check(&trc20(TREASURY, 20_000_000, 30_000_000)), TronVerifyError::Mismatch("recipient"));
        assert_eq!(check(&trc20(RECIPIENT, 20_000_000, 20_000_000)), TronVerifyError::Mismatch("fee_limit"));
        let other_owner = TronTransfer::Trc20 {
            owner: RECIPIENT.into(),
            contract: USDT.into(),
            recipient: RECIPIENT.into(),
            amount: 20_000_000,
            fee_limit: 30_000_000,
        };
        assert_eq!(check(&other_owner), TronVerifyError::Mismatch("owner"));
        let other_token = TronTransfer::Trc20 {
            owner: TREASURY.into(),
            contract: "TEkxiTehnzSmSe2XqrBj4w32RUN966rdz8".into(),
            recipient: RECIPIENT.into(),
            amount: 20_000_000,
            fee_limit: 30_000_000,
        };
        assert_eq!(check(&other_token), TronVerifyError::Mismatch("contract"));
        let as_trx = TronTransfer::Trx { owner: TREASURY.into(), recipient: RECIPIENT.into(), amount: 20_000_000 };
        assert_eq!(check(&as_trx), TronVerifyError::Shape("not a TransferContract"));
        // The node's txID must be sha256(raw_data).
        assert_eq!(
            verify_tron_transaction(TRC20_RAW, Some(TRX_TXID), &trc20(RECIPIENT, 20_000_000, 30_000_000), TRC20_CREATED)
                .unwrap_err(),
            TronVerifyError::TxIdMismatch
        );
        // Expired by the time it is checked.
        assert_eq!(
            verify_tron_transaction(TRC20_RAW, None, &trc20(RECIPIENT, 20_000_000, 30_000_000), TRC20_EXPIRATION)
                .unwrap_err(),
            TronVerifyError::Shape("expiration out of bounds")
        );
    }

    /// Re-encode a tampered copy of the real transaction.
    fn tampered(edit: impl FnOnce(&mut TransactionRaw)) -> String {
        let mut raw = TransactionRaw::decode(hex::decode(TRC20_RAW).unwrap().as_slice()).unwrap();
        edit(&mut raw);
        hex::encode(raw.encode_to_vec())
    }

    #[test]
    fn tampered_trc20_transactions_are_refused() {
        let expected = trc20(RECIPIENT, 20_000_000, 30_000_000);
        let verify = |raw: &str| verify_tron_transaction(raw, None, &expected, TRC20_CREATED).unwrap_err();
        assert_eq!(verify(&tampered(|raw| raw.data = b"memo".to_vec())), TronVerifyError::Shape("memo or scripts present"));
        assert_eq!(
            verify(&tampered(|raw| raw.contract.push(raw.contract[0].clone()))),
            TronVerifyError::Shape("exactly one contract expected")
        );
        assert_eq!(
            verify(&tampered(|raw| raw.contract[0].permission_id = 2)),
            TronVerifyError::Shape("unexpected contract options")
        );
        assert_eq!(
            verify(&tampered(|raw| {
                let parameter = raw.contract[0].parameter.as_mut().unwrap();
                let mut call = TriggerSmartContract::decode(parameter.value.as_slice()).unwrap();
                call.call_value = 1;
                parameter.value = call.encode_to_vec();
            })),
            TronVerifyError::Shape("value attached to the call")
        );
        assert_eq!(
            verify(&tampered(|raw| {
                let parameter = raw.contract[0].parameter.as_mut().unwrap();
                let mut call = TriggerSmartContract::decode(parameter.value.as_slice()).unwrap();
                call.data[0..4].copy_from_slice(&[0x09, 0x5e, 0xa7, 0xb3]); // approve(address,uint256)
                parameter.value = call.encode_to_vec();
            })),
            TronVerifyError::Mismatch("selector")
        );
        // An unknown field (tag 9, `auths`) appended to raw_data.
        let mut with_unknown = hex::decode(TRC20_RAW).unwrap();
        with_unknown.extend_from_slice(&[0x4a, 0x00]);
        assert_eq!(verify(&hex::encode(with_unknown)), TronVerifyError::NonCanonical);
        assert_eq!(verify("zz"), TronVerifyError::Undecodable);
    }

    #[test]
    fn a_real_trx_transfer_is_verified() {
        let expected = TronTransfer::Trx { owner: TRX_OWNER.into(), recipient: RECIPIENT.into(), amount: 1_500_000 };
        let verified = verify_tron_transaction(TRX_RAW, Some(TRX_TXID), &expected, 1_791_748_840_255).unwrap();
        assert_eq!(verified.tx_id, TRX_TXID);
        assert_eq!(verified.fee_limit, 0);
        let more = TronTransfer::Trx { owner: TRX_OWNER.into(), recipient: RECIPIENT.into(), amount: 1_500_001 };
        assert_eq!(
            verify_tron_transaction(TRX_RAW, None, &more, 1_791_748_840_255).unwrap_err(),
            TronVerifyError::Mismatch("amount")
        );
    }

    #[test]
    fn signed_transactions_recover_the_owner() {
        use k256::ecdsa::{RecoveryId, Signature, VerifyingKey};
        use sha3::Keccak256;
        let wallet = WalletService::from_mnemonic(
            "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about",
        )
        .unwrap();
        let key = KeyRef::Treasury(domain::Network::Tron);
        let verified = verify_tron_transaction(
            TRC20_RAW,
            Some(TRC20_TXID),
            &trc20(RECIPIENT, 20_000_000, 30_000_000),
            TRC20_CREATED,
        )
        .unwrap();
        let signed = sign_tron(&verified, &wallet, key).unwrap();
        // Transaction { raw_data: <exact bytes>, signature: [65 bytes] };
        // raw_data is 211 bytes (varint d3 01).
        assert_eq!(verified.raw.len(), 211);
        assert_eq!(&signed[..3], &[0x0a, 0xd3, 0x01]);
        assert_eq!(&signed[3..3 + verified.raw.len()], verified.raw.as_slice());
        let tail = &signed[3 + verified.raw.len()..];
        assert_eq!(&tail[..2], &[0x12, 65]);
        let signature = &tail[2..];
        assert!(signature[64] == 27 || signature[64] == 28);

        let digest: [u8; 32] = Sha256::digest(&verified.raw).into();
        let recovered = VerifyingKey::recover_from_prehash(
            &digest,
            &Signature::from_slice(&signature[..64]).unwrap(),
            RecoveryId::from_byte(signature[64] - 27).unwrap(),
        )
        .unwrap();
        let point = recovered.to_encoded_point(false);
        let mut address = [0u8; 21];
        address[0] = 0x41;
        address[1..].copy_from_slice(&Keccak256::digest(&point.as_bytes()[1..])[12..]);
        assert_eq!(crate::destination::tron_encode(&address), TREASURY);
    }

    #[test]
    fn transfer_calldata_and_fee_limit() {
        assert_eq!(
            hex::encode(trc20_transfer_data(RECIPIENT, 20_000_000).unwrap()),
            "a9059cbb0000000000000000000000005a67fa7cc56bd6d043a98e17d329c1dc9e14753f0000000000000000000000000000000000000000000000000000000001312d00"
        );
        // 64 285 energy (real estimate) × 100 sun × 1.5.
        assert_eq!(fee_limit_sun(64_285, 100, 100_000_000), 9_642_750);
        assert_eq!(fee_limit_sun(10_000_000, 100, 100_000_000), 100_000_000);
    }

    #[test]
    fn real_chain_parameters_give_the_fee_schedule() {
        let params: ChainParameters = serde_json::from_str(
            r#"{"chainParameter":[{"key":"getMaintenanceTimeInterval","value":21600000},
                {"key":"getCreateAccountFee","value":100000},{"key":"getTransactionFee","value":1000},
                {"key":"getEnergyFee","value":100},{"key":"getAllowTvmCompatibleEvm"}]}"#,
        )
        .unwrap();
        assert_eq!(
            parse_chain_fees(&params).unwrap(),
            TronChainFees { energy_sun: 100, bandwidth_sun: 1000, create_account_sun: 100_000 }
        );
        let missing: ChainParameters = serde_json::from_str(r#"{"chainParameter":[]}"#).unwrap();
        assert!(parse_chain_fees(&missing).is_err());
    }

    #[test]
    fn broadcast_answers_are_classified() {
        let parse = |json: &str| classify_broadcast(&serde_json::from_str(json).unwrap());
        assert_eq!(parse(r#"{"result":true,"txid":"aa"}"#), TronBroadcastOutcome::Accepted);
        assert_eq!(parse(r#"{"code":"DUP_TRANSACTION_ERROR","message":"6475702074"}"#), TronBroadcastOutcome::Accepted);
        assert_eq!(parse(r#"{"code":"TRANSACTION_EXPIRATION_ERROR"}"#), TronBroadcastOutcome::Expired);
        assert_eq!(
            parse(r#"{"code":"CONTRACT_VALIDATE_ERROR","message":"62616c616e6365206973206e6f742073756666696369656e74"}"#),
            TronBroadcastOutcome::Rejected("CONTRACT_VALIDATE_ERROR balance is not sufficient".into())
        );
    }

    #[test]
    fn solidified_outcomes() {
        let ok: TxInfoFull = serde_json::from_str(
            r#"{"id":"98f8aa28d7da55f3dc6535619137640249f42c105efaf7ebecda7fee44ce1f79","fee":6773500,"blockNumber":86998534,
                "receipt":{"energy_fee":6428500,"energy_usage_total":64285,"net_fee":345000,"result":"SUCCESS"}}"#,
        )
        .unwrap();
        assert_eq!(
            parse_tx_outcome(&ok, TRC20_TXID),
            Some(TronTxOutcome { block: 86_998_534, success: true, fee_sun: 6_773_500 })
        );
        let reverted: TxInfoFull = serde_json::from_str(
            r#"{"id":"98f8aa28d7da55f3dc6535619137640249f42c105efaf7ebecda7fee44ce1f79","fee":1000,"blockNumber":5,"result":"FAILED","receipt":{"result":"REVERT"}}"#,
        )
        .unwrap();
        assert!(!parse_tx_outcome(&reverted, TRC20_TXID).unwrap().success);
        // A TRX transfer has no contract result.
        let trx: TxInfoFull = serde_json::from_str(
            r#"{"id":"8a2dd9cfc0c6ccc965aa1822f4d8943b4dc0f219a1a6fbf0ebc19389c9bedd89","blockNumber":7,"receipt":{"net_usage":268}}"#,
        )
        .unwrap();
        assert!(parse_tx_outcome(&trx, TRX_TXID).unwrap().success);
        let unknown: TxInfoFull = serde_json::from_str("{}").unwrap();
        assert_eq!(parse_tx_outcome(&unknown, TRC20_TXID), None);
    }
}
