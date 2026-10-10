use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use config::{AppConfig, USDT_ETH, USDT_SOLANA, USDT_TRON};
use domain::{Network, Token};
use serde::Deserialize;
use solana_sdk::pubkey::Pubkey;
use tracing::{debug, warn};

mod esplora;

use esplora::{confirmed_balance_sats, esplora_transfers, EsploraAddress, EsploraTx};

/// keccak256("Transfer(address,address,uint256)")
const TRANSFER_TOPIC: &str =
    "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef";

#[derive(Debug, Clone)]
pub struct DetectedOnChainTransfer {
    pub tx_hash: String,
    pub amount_raw: String,
    pub confirmations: i32,
}

#[derive(Clone)]
pub struct ChainClients {
    config: Arc<AppConfig>,
    http: reqwest::Client,
    tron_rate_limited_until: Arc<tokio::sync::Mutex<Option<Instant>>>,
}

impl ChainClients {
    pub fn new(config: Arc<AppConfig>) -> Self {
        Self {
            config,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(15))
                .build()
                .expect("http client"),
            tron_rate_limited_until: Arc::new(tokio::sync::Mutex::new(None)),
        }
    }

    /// Lightning is never available here: it is served by the Breez SDK
    /// client held in `AppState`, not by a chain provider.
    pub fn is_network_available(&self, network: Network) -> bool {
        match network {
            Network::Tron => self.config.trongrid_api_key.is_some(),
            Network::Ethereum => self.config.alchemy_api_key.is_some(),
            Network::Solana => self.config.helius_api_key.is_some(),
            Network::Bitcoin => self.config.bitcoin_esplora_base_url.is_some(),
            Network::Lightning => false,
        }
    }

    pub fn has_any_network_available(&self) -> bool {
        [Network::Tron, Network::Ethereum, Network::Solana, Network::Bitcoin]
            .into_iter()
            .any(|network| self.is_network_available(network))
    }

    pub fn missing_key_message(&self, network: Network) -> &'static str {
        match network {
            Network::Tron => "TRONGRID_API_KEY not configured",
            Network::Ethereum => "ALCHEMY_API_KEY not configured",
            Network::Solana => "HELIUS_API_KEY not configured",
            Network::Bitcoin => "BITCOIN_ESPLORA_BASE_URL not configured",
            Network::Lightning => "BREEZ_API_KEY not configured",
        }
    }

    pub async fn fetch_usdt_balance(&self, network: Network, address: &str) -> Result<u128> {
        match network {
            Network::Tron => self.fetch_tron_balance(address).await,
            Network::Ethereum => self.fetch_eth_balance(address).await,
            Network::Solana => self.fetch_sol_balance(address).await,
            Network::Bitcoin | Network::Lightning => {
                anyhow::bail!("{} is not a USDT network", network.as_api())
            }
        }
    }

    /// Confirmed on-chain balance of a Bitcoin address, in sats.
    pub async fn fetch_btc_balance(&self, address: &str) -> Result<u128> {
        let Some(url) = self.config.esplora_url(&format!("address/{address}")) else {
            return Ok(0);
        };
        let account: EsploraAddress = self
            .http
            .get(url)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(confirmed_balance_sats(&account))
    }

    /// Transfers to `address` among its most recent page of transactions
    /// (mempool + last 25 confirmed — no pagination, see the spec).
    pub async fn poll_bitcoin_address(
        &self,
        address: &str,
    ) -> Result<Vec<DetectedOnChainTransfer>> {
        let (Some(tip_url), Some(txs_url)) = (
            self.config.esplora_url("blocks/tip/height"),
            self.config.esplora_url(&format!("address/{address}/txs")),
        ) else {
            debug!("Skipping Bitcoin monitor — BITCOIN_ESPLORA_BASE_URL not set");
            return Ok(vec![]);
        };

        let tip: u64 = self
            .http
            .get(tip_url)
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?
            .trim()
            .parse()
            .context("Esplora returned an invalid tip height")?;
        let txs: Vec<EsploraTx> = self
            .http
            .get(txs_url)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(esplora_transfers(txs, address, tip))
    }

    pub async fn poll_tron_address(&self, address: &str) -> Result<Vec<DetectedOnChainTransfer>> {
        let Some(api_key) = &self.config.trongrid_api_key else {
            debug!("Skipping Tron monitor — TRONGRID_API_KEY not set");
            return Ok(vec![]);
        };

        {
            let until = self.tron_rate_limited_until.lock().await;
            if let Some(until) = *until {
                if Instant::now() < until {
                    debug!("Tron monitor skipped — backing off after TronGrid rate limit");
                    return Ok(vec![]);
                }
            }
        }

        let current_block = match self.get_tron_block(api_key).await {
            Ok(b) => b,
            Err(e) => {
                if self.handle_tron_rate_limit(&e).await {
                    return Ok(vec![]);
                }
                return Err(e);
            }
        };

        let txs = match self.fetch_trc20(address, api_key).await {
            Ok(t) => t,
            Err(e) => {
                if self.handle_tron_rate_limit(&e).await {
                    return Ok(vec![]);
                }
                return Err(e);
            }
        };

        let mut out = Vec::new();
        for tx in txs {
            if tx.to.as_deref() != Some(address) {
                continue;
            }
            let tx_block = tx.block_number.unwrap_or(current_block);
            let confirmations = confirmations_between(current_block, tx_block);
            out.push(DetectedOnChainTransfer {
                tx_hash: tx.transaction_id,
                amount_raw: tx.value,
                confirmations,
            });
        }
        Ok(out)
    }

    pub async fn poll_ethereum_address(
        &self,
        address: &str,
    ) -> Result<Vec<DetectedOnChainTransfer>> {
        let Some(api_key) = &self.config.alchemy_api_key else {
            debug!("Skipping Ethereum monitor — ALCHEMY_API_KEY not set");
            return Ok(vec![]);
        };

        let rpc = self.config.eth_rpc_url(api_key);
        let current_block = eth_block_number(&self.http, &rpc).await?;
        let padded = pad_address_topic(address)?;

        let logs: Vec<EthLog> = eth_rpc(
            &self.http,
            &rpc,
            "eth_getLogs",
            serde_json::json!([{
                "address": USDT_ETH,
                "topics": [TRANSFER_TOPIC, serde_json::Value::Null, padded],
                "fromBlock": format!("0x{:x}", current_block.saturating_sub(5000)),
                "toBlock": format!("0x{current_block:x}")
            }]),
        )
        .await?;

        let mut out = Vec::new();
        for log in logs {
            let Some(tx_hash) = log.transaction_hash else {
                continue;
            };
            let Some(block_hex) = log.block_number else {
                continue;
            };
            let block_number = parse_hex_u64(&block_hex)?;
            let value = parse_hex_u128(&log.data)?;
            let confirmations = confirmations_between(current_block, block_number);
            out.push(DetectedOnChainTransfer {
                tx_hash,
                amount_raw: value.to_string(),
                confirmations,
            });
        }
        Ok(out)
    }

    pub async fn poll_solana_address(
        &self,
        address: &str,
    ) -> Result<Vec<DetectedOnChainTransfer>> {
        let Some(api_key) = &self.config.helius_api_key else {
            debug!("Skipping Solana monitor — HELIUS_API_KEY not set");
            return Ok(vec![]);
        };

        let rpc = self.config.helius_rpc_url(api_key);
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "getSignaturesForAddress",
            "params": [address, { "limit": 20 }]
        });
        let resp: serde_json::Value = self
            .http
            .post(&rpc)
            .json(&body)
            .send()
            .await?
            .json()
            .await?;

        let signatures = resp["result"].as_array().cloned().unwrap_or_default();

        let mut out = Vec::new();
        for sig_info in signatures {
            let Some(signature) = sig_info["signature"].as_str() else {
                continue;
            };
            let Some(parsed) = self.fetch_helius_parsed(api_key, signature).await? else {
                continue;
            };
            for transfer in parsed.token_transfers.unwrap_or_default() {
                if transfer.mint.as_deref() != Some(USDT_SOLANA) {
                    continue;
                }
                let destination = transfer
                    .to_token_account
                    .as_deref()
                    .or(transfer.to_user_account.as_deref());
                if destination != Some(address) {
                    continue;
                }
                let amount_raw = transfer
                    .raw_token_amount
                    .and_then(|r| r.token_amount)
                    .unwrap_or_else(|| {
                        let unit = 10f64.powi(Token::Usdt.decimals() as i32);
                        ((transfer.token_amount.unwrap_or(0.0) * unit).round() as i64).to_string()
                    });
                out.push(DetectedOnChainTransfer {
                    tx_hash: parsed.signature.clone(),
                    amount_raw,
                    confirmations: 1,
                });
            }
        }
        Ok(out)
    }

    async fn fetch_tron_balance(&self, address: &str) -> Result<u128> {
        let Some(api_key) = &self.config.trongrid_api_key else {
            return Ok(0);
        };
        let url = self.config.trongrid_url(&format!("v1/accounts/{address}"));
        let resp: TronAccountResponse = self
            .http
            .get(url)
            .header("TRON-PRO-API-KEY", api_key)
            .send()
            .await?
            .json()
            .await?;
        if let Some(entries) = resp.data.and_then(|d| d.into_iter().next()) {
            for map in entries.trc20.unwrap_or_default() {
                if let Some(balance) = map.get(USDT_TRON) {
                    return Ok(balance.parse().unwrap_or(0));
                }
            }
        }
        Ok(0)
    }

    async fn fetch_eth_balance(&self, address: &str) -> Result<u128> {
        let Some(api_key) = &self.config.alchemy_api_key else {
            return Ok(0);
        };
        let rpc = self.config.eth_rpc_url(api_key);
        // balanceOf(address) selector = 0x70a08231
        let data = format!("0x70a08231{}", pad_address_no_prefix(address)?);
        let result: String = eth_rpc(
            &self.http,
            &rpc,
            "eth_call",
            serde_json::json!([{ "to": USDT_ETH, "data": data }, "latest"]),
        )
        .await?;
        parse_hex_u128(&result)
    }

    async fn fetch_sol_balance(&self, address: &str) -> Result<u128> {
        let Some(api_key) = &self.config.helius_api_key else {
            return Ok(0);
        };
        let _pk: Pubkey = address.parse().context("invalid solana address")?;
        let rpc = self.config.helius_rpc_url(api_key);
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "getTokenAccountBalance",
            "params": [address]
        });
        let resp: serde_json::Value = self.http.post(rpc).json(&body).send().await?.json().await?;
        if let Some(amount) = resp["result"]["value"]["amount"].as_str() {
            return Ok(amount.parse().unwrap_or(0));
        }
        Ok(0)
    }

    async fn get_tron_block(&self, api_key: &str) -> Result<u64> {
        let resp: TronBlockResponse = self
            .http
            .post(self.config.trongrid_url("wallet/getnowblock"))
            .header("TRON-PRO-API-KEY", api_key)
            .json(&serde_json::json!({}))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        resp.block_header
            .and_then(|h| h.raw_data)
            .and_then(|r| r.number)
            .context("Unable to fetch current Tron block number")
    }

    async fn fetch_trc20(&self, address: &str, api_key: &str) -> Result<Vec<TronTrc20Tx>> {
        let url = self
            .config
            .trongrid_url(&format!("v1/accounts/{address}/transactions/trc20"));
        let resp: TronTrc20Response = self
            .http
            .get(url)
            .header("TRON-PRO-API-KEY", api_key)
            .query(&[
                ("only_to", "true"),
                ("contract_address", USDT_TRON),
                ("limit", "50"),
            ])
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(resp.data.unwrap_or_default())
    }

    async fn fetch_helius_parsed(
        &self,
        api_key: &str,
        signature: &str,
    ) -> Result<Option<HeliusTransaction>> {
        let url = self.config.helius_transactions_url(api_key);
        let resp = self
            .http
            .post(url)
            .json(&serde_json::json!({ "transactions": [signature] }))
            .send()
            .await?;
        if !resp.status().is_success() {
            return Ok(None);
        }
        let data: Vec<HeliusTransaction> = resp.json().await?;
        Ok(data.into_iter().next())
    }

    async fn handle_tron_rate_limit(&self, err: &anyhow::Error) -> bool {
        let msg = format!("{err:#}");
        if msg.contains("429") {
            let secs = 60u64;
            warn!(
                "TronGrid rate limited (429) — backing off for {secs}s. Configure TRONGRID_API_KEY for higher limits."
            );
            *self.tron_rate_limited_until.lock().await =
                Some(Instant::now() + Duration::from_secs(secs));
            return true;
        }
        false
    }
}

/// Confirmations of a tx included at `tx_block` when the chain tip is
/// `current_block`, counting the inclusion block itself. Never underflows
/// (the tip may be read before a fresher tx list) and clamps to `i32::MAX`.
pub fn confirmations_between(current_block: u64, tx_block: u64) -> i32 {
    let confirmations = current_block.saturating_sub(tx_block).saturating_add(1);
    i32::try_from(confirmations).unwrap_or(i32::MAX)
}

async fn eth_block_number(http: &reqwest::Client, rpc: &str) -> Result<u64> {
    let hex: String = eth_rpc(http, rpc, "eth_blockNumber", serde_json::json!([])).await?;
    parse_hex_u64(&hex)
}

async fn eth_rpc<T: for<'de> Deserialize<'de>>(
    http: &reqwest::Client,
    rpc: &str,
    method: &str,
    params: serde_json::Value,
) -> Result<T> {
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": method,
        "params": params
    });
    let resp: EthRpcResponse<T> = http.post(rpc).json(&body).send().await?.json().await?;
    resp.result.context(format!("RPC {method} returned no result"))
}

fn pad_address_topic(address: &str) -> Result<String> {
    Ok(format!("0x{}", pad_address_no_prefix(address)?))
}

fn pad_address_no_prefix(address: &str) -> Result<String> {
    let hex = address.trim_start_matches("0x").to_lowercase();
    if hex.len() != 40 {
        anyhow::bail!("invalid eth address length");
    }
    Ok(format!("{:0>64}", hex))
}

fn parse_hex_u64(value: &str) -> Result<u64> {
    let hex = value.trim_start_matches("0x");
    Ok(u64::from_str_radix(hex, 16)?)
}

fn parse_hex_u128(value: &str) -> Result<u128> {
    let hex = value.trim_start_matches("0x");
    if hex.is_empty() {
        return Ok(0);
    }
    Ok(u128::from_str_radix(hex, 16)?)
}

#[derive(Debug, Deserialize)]
struct EthRpcResponse<T> {
    result: Option<T>,
}

#[derive(Debug, Deserialize)]
struct EthLog {
    data: String,
    #[serde(rename = "transactionHash")]
    transaction_hash: Option<String>,
    #[serde(rename = "blockNumber")]
    block_number: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TronTrc20Response {
    data: Option<Vec<TronTrc20Tx>>,
}

#[derive(Debug, Deserialize)]
struct TronTrc20Tx {
    transaction_id: String,
    to: Option<String>,
    value: String,
    block_number: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct TronBlockResponse {
    block_header: Option<TronBlockHeader>,
}

#[derive(Debug, Deserialize)]
struct TronBlockHeader {
    raw_data: Option<TronRawData>,
}

#[derive(Debug, Deserialize)]
struct TronRawData {
    number: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct TronAccountResponse {
    data: Option<Vec<TronAccount>>,
}

#[derive(Debug, Deserialize)]
struct TronAccount {
    trc20: Option<Vec<std::collections::HashMap<String, String>>>,
}

#[derive(Debug, Deserialize)]
struct HeliusTransaction {
    signature: String,
    #[serde(rename = "tokenTransfers")]
    token_transfers: Option<Vec<HeliusTokenTransfer>>,
}

#[derive(Debug, Deserialize)]
struct HeliusTokenTransfer {
    mint: Option<String>,
    #[serde(rename = "toUserAccount")]
    to_user_account: Option<String>,
    #[serde(rename = "toTokenAccount")]
    to_token_account: Option<String>,
    #[serde(rename = "tokenAmount")]
    token_amount: Option<f64>,
    #[serde(rename = "rawTokenAmount")]
    raw_token_amount: Option<HeliusRawAmount>,
}

#[derive(Debug, Deserialize)]
struct HeliusRawAmount {
    #[serde(rename = "tokenAmount")]
    token_amount: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn confirmations_count_the_inclusion_block() {
        assert_eq!(confirmations_between(100, 100), 1);
        assert_eq!(confirmations_between(100, 98), 3);
    }

    #[test]
    fn confirmations_never_underflow_when_tip_lags_the_tx_block() {
        assert_eq!(confirmations_between(98, 100), 1);
        assert_eq!(confirmations_between(0, u64::MAX), 1);
    }

    #[test]
    fn confirmations_clamp_to_i32_max() {
        assert_eq!(confirmations_between(u64::MAX, 0), i32::MAX);
        assert_eq!(confirmations_between(i32::MAX as u64 + 10, 0), i32::MAX);
    }
}
