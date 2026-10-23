// Raw reqwest calls leak the URL — and the provider key in it — into
// errors: `clippy.toml` bans them outside `request.rs`, and this makes the
// ban an error even though CI runs clippy without `-D warnings`.
#![deny(clippy::disallowed_methods)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use config::{AppConfig, DEPOSIT_EXPIRY_MAX_MINUTES};
use domain::{decimal_str_to_raw, Network, Token};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use solana_sdk::pubkey::Pubkey;
use tracing::{debug, warn};

pub mod btc_tx;
pub mod destination;
pub mod evm;
pub mod solana_tx;
pub mod tron_tx;
mod esplora;
mod mainnet;
mod request;
mod tron;

use esplora::{confirmed_balance_sats, esplora_transfers, received_at_depth, EsploraAddress, EsploraTx};
pub use mainnet::WrongChainError;

/// keccak256("Transfer(address,address,uint256)")
const TRANSFER_TOPIC: &str =
    "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef";

/// Ethereum mainnet slot time.
const ETH_BLOCK_TIME_SECS: u64 = 12;
/// Detection grace past the longest payment lifetime.
const ETH_LOG_SCAN_GRACE_MINUTES: u64 = 120;
/// Blocks scanned back by `eth_getLogs`: the longest payment lifetime
/// (`DEPOSIT_EXPIRY_MAX_MINUTES`) plus the grace, one block per slot
/// (7800 blocks for 1440 + 120 min).
const ETH_LOG_SCAN_BLOCKS: u64 = ((DEPOSIT_EXPIRY_MAX_MINUTES as u64
    + ETH_LOG_SCAN_GRACE_MINUTES)
    * 60)
    .div_ceil(ETH_BLOCK_TIME_SECS);
const _: () = assert!(DEPOSIT_EXPIRY_MAX_MINUTES > 0);
const _: () = assert!(
    ETH_LOG_SCAN_BLOCKS * ETH_BLOCK_TIME_SECS
        >= (DEPOSIT_EXPIRY_MAX_MINUTES as u64 + ETH_LOG_SCAN_GRACE_MINUTES) * 60
);

const TRON_RATE_LIMIT_BACKOFF_SECS: u64 = 60;

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
    /// Solidified block of the TRC-20 txs already resolved.
    tron_tx_blocks: Arc<tron::TxBlockCache>,
    /// Configured networks turned off for this process (`disable_network`).
    disabled: Vec<Network>,
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
            tron_tx_blocks: Arc::new(tron::TxBlockCache::new(tron::TX_BLOCK_CACHE_CAPACITY)),
            disabled: Vec::new(),
        }
    }

    /// Lightning is never available here: it is served by the Breez SDK
    /// client held in `AppState`, not by a chain provider.
    pub fn is_network_available(&self, network: Network) -> bool {
        self.is_configured(network) && !self.disabled.contains(&network)
    }

    pub fn has_any_network_available(&self) -> bool {
        Network::ALL
            .into_iter()
            .any(|network| self.is_network_available(network))
    }

    pub fn missing_key_message(&self, network: Network) -> &'static str {
        if self.is_configured(network) && self.disabled.contains(&network) {
            return "Provider failed the mainnet check at boot";
        }
        match network {
            Network::Tron => "TRONGRID_API_KEY not configured",
            Network::Ethereum => "ALCHEMY_API_KEY not configured",
            Network::Solana => "HELIUS_API_KEY not configured",
            Network::Bitcoin => "BITCOIN_ESPLORA_BASE_URL not configured",
            Network::Lightning => "BREEZ_API_KEY not configured",
        }
    }

    /// Turn a configured network off for this process — typically after a
    /// failed `verify_networks` check. `is_network_available` then reports
    /// it unavailable (with `missing_key_message` saying why) and its polls
    /// and balance fetches return nothing. Call it before cloning the
    /// clients into `AppState`: clones taken earlier keep the old set.
    pub fn disable_network(&mut self, network: Network) {
        if !self.disabled.contains(&network) {
            self.disabled.push(network);
        }
    }

    /// Boot-time check that every available chain provider serves
    /// **mainnet**: Ethereum `eth_chainId == 0x1`, and the genesis block of
    /// Bitcoin (Esplora `block-height/0`), Solana (`getGenesisHash`) and Tron
    /// (`wallet/getblockbynum {"num":0}`). The checks run concurrently.
    ///
    /// One entry per network that is configured and not disabled —
    /// Lightning and unconfigured networks are absent. An `Err` is either a
    /// [`WrongChainError`] (the provider answered for another chain) or a
    /// request failure (unreachable, rate-limited, bad key): in both cases
    /// the network cannot be trusted and the caller should
    /// `disable_network` it.
    pub async fn verify_networks(&self) -> Vec<(Network, Result<()>)> {
        let (tron, ethereum, solana, bitcoin) = tokio::join!(
            self.verify_network(Network::Tron),
            self.verify_network(Network::Ethereum),
            self.verify_network(Network::Solana),
            self.verify_network(Network::Bitcoin),
        );
        [
            (Network::Tron, tron),
            (Network::Ethereum, ethereum),
            (Network::Solana, solana),
            (Network::Bitcoin, bitcoin),
        ]
        .into_iter()
        .filter_map(|(network, result)| result.map(|result| (network, result)))
        .collect()
    }

    /// The mainnet check of one network (see `verify_networks`); `None`
    /// when it is not available (unconfigured, disabled, Lightning).
    pub async fn verify_network(&self, network: Network) -> Option<Result<()>> {
        if !self.is_network_available(network) {
            return None;
        }
        Some(match network {
            Network::Tron => self.verify_tron().await,
            Network::Ethereum => self.verify_ethereum().await,
            Network::Solana => self.verify_solana().await,
            Network::Bitcoin => self.verify_bitcoin().await,
            Network::Lightning => return None,
        })
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

    /// Balance of `token` held by `address` (latest state; on Solana the
    /// address is the token account for USDT, the owner for SOL). Zero when
    /// the network is unconfigured.
    pub async fn fetch_balance(&self, network: Network, token: Token, address: &str) -> Result<u128> {
        if !self.is_network_available(network) {
            return Ok(0);
        }
        match (network, token) {
            (_, Token::Usdt) => self.fetch_usdt_balance(network, address).await,
            (Network::Bitcoin, Token::Btc) => self.fetch_btc_balance(address).await,
            (Network::Ethereum, Token::Eth) => {
                evm::EvmRpc::eth_balance(self, address, evm::BlockTag::Latest).await
            }
            (Network::Tron, Token::Trx) => {
                Ok(tron_tx::TronRpc::account_balance(self, address).await?.unwrap_or(0))
            }
            (Network::Solana, Token::Sol) => solana_tx::SolanaRpc::lamports(self, address).await,
            _ => anyhow::bail!("{} is not held on {}", token.as_api(), network.as_api()),
        }
    }

    /// Confirmed on-chain balance of a Bitcoin address, in sats.
    pub async fn fetch_btc_balance(&self, address: &str) -> Result<u128> {
        let Some(url) = self.esplora_url(&format!("address/{address}")) else {
            return Ok(0);
        };
        let account: EsploraAddress = request::send_json(self.http.get(url))
            .await
            .context("Esplora address request failed")?;
        Ok(confirmed_balance_sats(&account))
    }

    /// Everything `address` ever received with at least `depth`
    /// confirmations, spent outputs included (sats) — the project ledger's
    /// Bitcoin watermark (spec §7.3). `None` when Bitcoin is not configured.
    pub async fn bitcoin_received_at_depth(&self, address: &str, depth: i32) -> Result<Option<u128>> {
        let (Some(tip_url), Some(account_url), Some(txs_url)) = (
            self.esplora_url("blocks/tip/height"),
            self.esplora_url(&format!("address/{address}")),
            self.esplora_url(&format!("address/{address}/txs")),
        ) else {
            return Ok(None);
        };
        let tip: u64 = request::send_text(self.http.get(tip_url))
            .await
            .context("Esplora tip request failed")?
            .trim()
            .parse()
            .context("Esplora returned an invalid tip height")?;
        let account: EsploraAddress = request::send_json(self.http.get(account_url))
            .await
            .context("Esplora address request failed")?;
        let txs: Vec<EsploraTx> = request::send_json(self.http.get(txs_url))
            .await
            .context("Esplora address txs request failed")?;
        Ok(Some(received_at_depth(&account, &txs, address, tip, depth)))
    }

    /// Transfers to `address` among its most recent page of transactions
    /// (mempool + last 25 confirmed — no pagination, see the spec).
    pub async fn poll_bitcoin_address(
        &self,
        address: &str,
    ) -> Result<Vec<DetectedOnChainTransfer>> {
        let (Some(tip_url), Some(txs_url)) = (
            self.esplora_url("blocks/tip/height"),
            self.esplora_url(&format!("address/{address}/txs")),
        ) else {
            debug!("Skipping Bitcoin monitor — BITCOIN_ESPLORA_BASE_URL not set");
            return Ok(vec![]);
        };

        let tip: u64 = request::send_text(self.http.get(tip_url))
            .await
            .context("Esplora tip request failed")?
            .trim()
            .parse()
            .context("Esplora returned an invalid tip height")?;
        let txs: Vec<EsploraTx> = request::send_json(self.http.get(txs_url))
            .await
            .context("Esplora address txs request failed")?;
        Ok(esplora_transfers(txs, address, tip))
    }

    /// USDT transfers to `address` from its solidified TRC-20 history, each
    /// with its real confirmation count (head block − solidified block of
    /// the tx). A transfer whose block cannot be resolved yet is left out.
    pub async fn poll_tron_address(&self, address: &str) -> Result<Vec<DetectedOnChainTransfer>> {
        let Some(api_key) = self.api_key(Network::Tron) else {
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
            Err(e) => return self.tron_failure(e, vec![]).await,
        };

        let candidates = match self.fetch_trc20(address, api_key).await {
            Ok(txs) => tron::usdt_transfers_to(txs, &self.config.chain.usdt_tron, address),
            Err(e) => return self.tron_failure(e, vec![]).await,
        };

        let mut out = Vec::new();
        for candidate in candidates {
            let tx_block = match self.tron_tx_block(api_key, &candidate.tx_id).await {
                Ok(Some(block)) => block,
                Ok(None) => {
                    debug!(
                        "Tron tx {} has no solidified block yet — skipped this round",
                        candidate.tx_id
                    );
                    continue;
                }
                // Keep what is already resolved: those counts are real.
                Err(e) => return self.tron_failure(e, out).await,
            };
            out.push(DetectedOnChainTransfer {
                tx_hash: candidate.tx_id,
                amount_raw: candidate.amount_raw,
                confirmations: confirmations_between(current_block, tx_block),
            });
        }
        Ok(out)
    }

    pub async fn poll_ethereum_address(
        &self,
        address: &str,
    ) -> Result<Vec<DetectedOnChainTransfer>> {
        let Some(api_key) = self.api_key(Network::Ethereum) else {
            debug!("Skipping Ethereum monitor — ALCHEMY_API_KEY not set");
            return Ok(vec![]);
        };

        let rpc = self.config.eth_rpc_url(api_key);
        let current_block = eth_block_number(&self.http, &rpc).await?;
        let recipient_topic = pad_address_topic(address)?;

        let logs: Vec<EthLog> = json_rpc(
            &self.http,
            &rpc,
            "eth_getLogs",
            serde_json::json!([{
                "address": self.config.chain.usdt_eth,
                "topics": [TRANSFER_TOPIC, serde_json::Value::Null, recipient_topic],
                "fromBlock": format!("0x{:x}", current_block.saturating_sub(ETH_LOG_SCAN_BLOCKS)),
                "toBlock": format!("0x{current_block:x}")
            }]),
        )
        .await?;

        eth_usdt_transfers(logs, &self.config.chain.usdt_eth, &recipient_topic, current_block)
    }

    /// USDT transfers to the token account `address` (a deposit's
    /// `tokenAccount`, or the address of a Solana row from before spec §4.4,
    /// which is its token account): its finalized signatures, matched on
    /// `toTokenAccount` — a wallet creating the account as it pays is
    /// listed too.
    pub async fn poll_solana_address(
        &self,
        address: &str,
    ) -> Result<Vec<DetectedOnChainTransfer>> {
        let Some(api_key) = self.api_key(Network::Solana) else {
            debug!("Skipping Solana monitor — HELIUS_API_KEY not set");
            return Ok(vec![]);
        };

        let rpc = self.config.helius_rpc_url(api_key);
        let signatures: Vec<SolanaSignatureInfo> = json_rpc(
            &self.http,
            &rpc,
            "getSignaturesForAddress",
            serde_json::json!([address, { "limit": 20, "commitment": "finalized" }]),
        )
        .await?;

        let mut out = Vec::new();
        for signature in finalized_successful_signatures(signatures) {
            let Some(parsed) = self.fetch_helius_parsed(api_key, &signature).await? else {
                continue;
            };
            if parsed.signature != signature {
                warn!("Helius answered for another transaction than {signature} — skipped");
                continue;
            }
            out.extend(helius_usdt_transfers(parsed, &self.config.chain.usdt_solana_mint, address));
        }
        Ok(out)
    }

    /// A configured network's provider key, `None` when it is unset or the
    /// network is disabled.
    fn api_key(&self, network: Network) -> Option<&str> {
        if self.disabled.contains(&network) {
            return None;
        }
        match network {
            Network::Tron => self.config.trongrid_api_key.as_deref(),
            Network::Ethereum => self.config.alchemy_api_key.as_deref(),
            Network::Solana => self.config.helius_api_key.as_deref(),
            Network::Bitcoin | Network::Lightning => None,
        }
    }

    /// `None` when Bitcoin is unconfigured or disabled.
    fn esplora_url(&self, path: &str) -> Option<String> {
        if self.disabled.contains(&Network::Bitcoin) {
            return None;
        }
        self.config.esplora_url(path)
    }

    fn is_configured(&self, network: Network) -> bool {
        match network {
            Network::Tron => self.config.trongrid_api_key.is_some(),
            Network::Ethereum => self.config.alchemy_api_key.is_some(),
            Network::Solana => self.config.helius_api_key.is_some(),
            Network::Bitcoin => self.config.bitcoin_esplora_base_url.is_some(),
            Network::Lightning => false,
        }
    }

    fn expected_genesis(&self, network: Network) -> Result<&'static str> {
        mainnet::expected_genesis(network, self.config.chain.mode)
            .with_context(|| format!("no genesis identity for {}", network.as_api()))
    }

    async fn verify_tron(&self) -> Result<()> {
        let api_key = self
            .api_key(Network::Tron)
            .context("TRONGRID_API_KEY not configured")?;
        let genesis: tron::Block = request::send_json(
            self.http
                .post(self.config.trongrid_url("wallet/getblockbynum"))
                .header("TRON-PRO-API-KEY", api_key)
                .json(&serde_json::json!({ "num": 0 })),
        )
        .await
        .context("Tron mainnet check: wallet/getblockbynum failed")?;
        mainnet::expect_identity(
            Network::Tron,
            self.expected_genesis(Network::Tron)?,
            genesis.block_id.as_deref().unwrap_or_default(),
        )?;
        Ok(())
    }

    async fn verify_ethereum(&self) -> Result<()> {
        let api_key = self
            .api_key(Network::Ethereum)
            .context("ALCHEMY_API_KEY not configured")?;
        let rpc = self.config.eth_rpc_url(api_key);
        let chain_id: String = json_rpc(&self.http, &rpc, "eth_chainId", serde_json::json!([]))
            .await
            .context("Ethereum mainnet check failed")?;
        let actual = parse_hex_u64(&chain_id)
            .map(|id| format!("0x{id:x}"))
            .unwrap_or(chain_id);
        mainnet::expect_identity(
            Network::Ethereum,
            &format!("0x{:x}", self.config.chain.eth_chain_id),
            &actual,
        )?;
        Ok(())
    }

    async fn verify_solana(&self) -> Result<()> {
        let api_key = self
            .api_key(Network::Solana)
            .context("HELIUS_API_KEY not configured")?;
        let rpc = self.config.helius_rpc_url(api_key);
        let genesis: String = json_rpc(&self.http, &rpc, "getGenesisHash", serde_json::json!([]))
            .await
            .context("Solana mainnet check failed")?;
        mainnet::expect_identity(Network::Solana, self.expected_genesis(Network::Solana)?, &genesis)?;
        Ok(())
    }

    async fn verify_bitcoin(&self) -> Result<()> {
        let url = self
            .esplora_url("block-height/0")
            .context("BITCOIN_ESPLORA_BASE_URL not configured")?;
        let genesis = request::send_text(self.http.get(url))
            .await
            .context("Bitcoin mainnet check: Esplora block-height/0 failed")?;
        mainnet::expect_identity(Network::Bitcoin, self.expected_genesis(Network::Bitcoin)?, &genesis)?;
        Ok(())
    }

    async fn fetch_tron_balance(&self, address: &str) -> Result<u128> {
        let Some(api_key) = self.api_key(Network::Tron) else {
            return Ok(0);
        };
        let url = self.config.trongrid_url(&format!("v1/accounts/{address}"));
        let resp: TronAccountResponse =
            request::send_json(self.http.get(url).header("TRON-PRO-API-KEY", api_key))
                .await
                .context("TronGrid account request failed")?;
        if let Some(entries) = resp.data.and_then(|d| d.into_iter().next()) {
            for map in entries.trc20.unwrap_or_default() {
                if let Some(balance) = map.get(&self.config.chain.usdt_tron) {
                    return Ok(balance.parse().unwrap_or(0));
                }
            }
        }
        Ok(0)
    }

    async fn fetch_eth_balance(&self, address: &str) -> Result<u128> {
        let Some(api_key) = self.api_key(Network::Ethereum) else {
            return Ok(0);
        };
        let rpc = self.config.eth_rpc_url(api_key);
        // balanceOf(address) selector = 0x70a08231
        let data = format!("0x70a08231{}", pad_address_no_prefix(address)?);
        let result: String = json_rpc(
            &self.http,
            &rpc,
            "eth_call",
            serde_json::json!([{ "to": self.config.chain.usdt_eth, "data": data }, "latest"]),
        )
        .await?;
        parse_hex_u128(&result)
    }

    async fn fetch_sol_balance(&self, address: &str) -> Result<u128> {
        let Some(api_key) = self.api_key(Network::Solana) else {
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
        // An unfunded token account answers an RPC error: balance 0.
        let resp: serde_json::Value = request::send_json(self.http.post(rpc).json(&body))
            .await
            .context("RPC getTokenAccountBalance request failed")?;
        if let Some(amount) = resp["result"]["value"]["amount"].as_str() {
            return Ok(amount.parse().unwrap_or(0));
        }
        Ok(0)
    }

    async fn get_tron_block(&self, api_key: &str) -> Result<u64> {
        let block: tron::Block = request::send_json(
            self.http
                .post(self.config.trongrid_url("wallet/getnowblock"))
                .header("TRON-PRO-API-KEY", api_key)
                .json(&serde_json::json!({})),
        )
        .await
        .context("TronGrid getnowblock request failed")?;
        block
            .number()
            .context("Unable to fetch current Tron block number")
    }

    /// Solidified TRC-20 USDT history of `address` (`only_confirmed`: no
    /// transfer that could still be reverted).
    async fn fetch_trc20(&self, address: &str, api_key: &str) -> Result<Vec<tron::Trc20Tx>> {
        let url = self
            .config
            .trongrid_url(&format!("v1/accounts/{address}/transactions/trc20"));
        let page: tron::Trc20Page = request::send_json(
            self.http
                .get(url)
                .header("TRON-PRO-API-KEY", api_key)
                .query(&[
                    ("only_confirmed", "true"),
                    ("only_to", "true"),
                    ("contract_address", self.config.chain.usdt_tron.as_str()),
                    ("limit", "50"),
                ]),
        )
        .await
        .context("TronGrid trc20 history request failed")?;
        if page.success == Some(false) {
            let error: String = page.error.unwrap_or_default().chars().take(200).collect();
            anyhow::bail!("TronGrid trc20 history failed: {error}");
        }
        Ok(page.data.unwrap_or_default())
    }

    /// Solidified block of `tx_id` (cached once known), `None` while the
    /// solidity node does not know the tx or reports it failed.
    async fn tron_tx_block(&self, api_key: &str, tx_id: &str) -> Result<Option<u64>> {
        if let Some(block) = self.tron_tx_blocks.get(tx_id) {
            return Ok(Some(block));
        }
        let info: tron::TxInfo = request::send_json(
            self.http
                .post(self.config.trongrid_url("walletsolidity/gettransactioninfobyid"))
                .header("TRON-PRO-API-KEY", api_key)
                .json(&serde_json::json!({ "value": tx_id })),
        )
        .await
        .context("TronGrid gettransactioninfobyid request failed")?;
        let block = tron::solidified_block(&info, tx_id);
        if let Some(block) = block {
            self.tron_tx_blocks.insert(tx_id, block);
        }
        Ok(block)
    }

    /// `Ok(None)` when Helius cannot parse this signature (non-2xx other
    /// than 429): one odd transaction in the history must not block the
    /// detection of the others.
    async fn fetch_helius_parsed(
        &self,
        api_key: &str,
        signature: &str,
    ) -> Result<Option<HeliusTransaction>> {
        let url = self.config.helius_transactions_url(api_key);
        let parsed = request::send_json::<Vec<HeliusTransaction>>(self.http.post(url).json(
            &serde_json::json!({ "transactions": [signature], "commitment": "finalized" }),
        ))
        .await;
        match parsed {
            Ok(data) => Ok(data.into_iter().next()),
            Err(err) if request::status(&err).is_some() && !request::is_rate_limited(&err) => {
                debug!("Helius could not parse Solana tx {signature}: {err:#}");
                Ok(None)
            }
            Err(err) => Err(err.context("Helius transaction request failed")),
        }
    }

    /// On a TronGrid 429, back off and return what was gathered so far; any
    /// other error is propagated.
    async fn tron_failure(
        &self,
        err: anyhow::Error,
        partial: Vec<DetectedOnChainTransfer>,
    ) -> Result<Vec<DetectedOnChainTransfer>> {
        if !request::is_rate_limited(&err) {
            return Err(err);
        }
        let secs = TRON_RATE_LIMIT_BACKOFF_SECS;
        warn!("TronGrid rate limited (429) — backing off for {secs}s");
        *self.tron_rate_limited_until.lock().await =
            Some(Instant::now() + Duration::from_secs(secs));
        Ok(partial)
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
    let hex: String = json_rpc(http, rpc, "eth_blockNumber", serde_json::json!([])).await?;
    parse_hex_u64(&hex).context("eth_blockNumber returned an invalid block number")
}

/// JSON-RPC 2.0 call (Ethereum and Solana). The URL may carry the API key:
/// it never appears in the returned errors.
async fn json_rpc<T: DeserializeOwned>(
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
    let resp: RpcResponse<T> = request::send_json(http.post(rpc).json(&body))
        .await
        .with_context(|| format!("RPC {method} request failed"))?;
    if let Some(error) = resp.error {
        anyhow::bail!("RPC {method} failed: {}", error.describe());
    }
    resp.result
        .with_context(|| format!("RPC {method} returned no result"))
}

/// USDT `Transfer` logs paying `recipient_topic`. Everything `eth_getLogs`
/// was asked to filter on is checked again — contract, event, recipient —
/// rather than trusted from the provider.
fn eth_usdt_transfers(
    logs: Vec<EthLog>,
    usdt_contract: &str,
    recipient_topic: &str,
    current_block: u64,
) -> Result<Vec<DetectedOnChainTransfer>> {
    let mut out = Vec::new();
    for log in logs {
        let topics = log.topics.unwrap_or_default();
        let is_usdt_transfer_to_recipient = log
            .address
            .as_deref()
            .is_some_and(|contract| contract.eq_ignore_ascii_case(usdt_contract))
            // ERC-20 Transfer: event signature, from, to (data = value).
            && matches!(
                topics.as_slice(),
                [event, _from, to]
                    if event.eq_ignore_ascii_case(TRANSFER_TOPIC)
                        && to.eq_ignore_ascii_case(recipient_topic)
            )
            && log.removed != Some(true);
        if !is_usdt_transfer_to_recipient {
            continue;
        }
        let (Some(tx_hash), Some(block_hex), Some(data)) =
            (log.transaction_hash, log.block_number, log.data)
        else {
            continue;
        };
        let block_number = parse_hex_u64(&block_hex)
            .context("eth_getLogs returned an invalid blockNumber")?;
        let value = parse_hex_u128(&data).context("eth_getLogs returned an invalid value")?;
        out.push(DetectedOnChainTransfer {
            tx_hash,
            amount_raw: value.to_string(),
            confirmations: confirmations_between(current_block, block_number),
        });
    }
    Ok(out)
}

/// Signatures of the finalized, successful transactions (`err` null) —
/// a failed transaction moves no token even if it carries a transfer.
fn finalized_successful_signatures(infos: Vec<SolanaSignatureInfo>) -> Vec<String> {
    infos
        .into_iter()
        .filter(|info| {
            info.err.is_none()
                && info
                    .confirmation_status
                    .as_deref()
                    .is_none_or(|status| status == "finalized")
        })
        .filter_map(|info| info.signature)
        .collect()
}

/// USDT transfers to the token account `address` in a Helius-parsed
/// transaction; none when the transaction failed. Solana needs one finalized confirmation
/// (`domain::required_confirmations`), which `commitment: finalized` gives.
fn helius_usdt_transfers(
    parsed: HeliusTransaction,
    usdt_mint: &str,
    address: &str,
) -> Vec<DetectedOnChainTransfer> {
    if parsed.transaction_error.is_some() {
        return vec![];
    }
    let mut out = Vec::new();
    for transfer in parsed.token_transfers.unwrap_or_default() {
        if transfer.mint.as_deref() != Some(usdt_mint) {
            continue;
        }
        let destination = transfer
            .to_token_account
            .as_deref()
            .or(transfer.to_user_account.as_deref());
        if destination != Some(address) {
            continue;
        }
        let Some(amount_raw) = helius_amount_raw(&transfer) else {
            warn!(
                "Solana tx {}: USDT transfer without a usable amount — skipped",
                parsed.signature
            );
            continue;
        };
        out.push(DetectedOnChainTransfer {
            tx_hash: parsed.signature.clone(),
            amount_raw,
            confirmations: 1,
        });
    }
    out
}

/// Base-unit amount of a Helius token transfer: `rawTokenAmount` when
/// present, else the UI `tokenAmount` converted from its decimal text —
/// never through float arithmetic. `None` rather than a made-up amount.
fn helius_amount_raw(transfer: &HeliusTokenTransfer) -> Option<String> {
    let raw = match transfer
        .raw_token_amount
        .as_ref()
        .and_then(|r| r.token_amount.as_deref())
    {
        Some(raw) => raw.to_string(),
        None => match transfer.token_amount.as_ref()? {
            serde_json::Value::Number(ui) => {
                let ui = ui.as_f64()?;
                // Bounded first: `decimal_to_raw` multiplies `Decimal`s,
                // which panics on overflow (≈ 7.9e28).
                if !(ui.is_finite() && (0.0..=MAX_HELIUS_UI_AMOUNT).contains(&ui)) {
                    return None;
                }
                decimal_str_to_raw(&ui.to_string(), Token::Usdt).ok()?
            }
            _ => return None,
        },
    };
    raw.parse::<u128>().ok().map(|amount| amount.to_string())
}

/// Far above USDT's total supply, far below what `Decimal` can scale.
const MAX_HELIUS_UI_AMOUNT: f64 = 1e15;

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
struct RpcResponse<T> {
    result: Option<T>,
    error: Option<RpcError>,
}

#[derive(Debug, Deserialize)]
struct RpcError {
    code: Option<i64>,
    message: Option<String>,
}

impl RpcError {
    fn describe(&self) -> String {
        let message: String = self
            .message
            .as_deref()
            .unwrap_or_default()
            .chars()
            .take(200)
            .collect();
        match self.code {
            Some(code) => format!("{code} {message}"),
            None => message,
        }
    }
}

#[derive(Debug, Deserialize)]
struct EthLog {
    /// Emitting contract.
    address: Option<String>,
    topics: Option<Vec<String>>,
    data: Option<String>,
    #[serde(rename = "transactionHash")]
    transaction_hash: Option<String>,
    #[serde(rename = "blockNumber")]
    block_number: Option<String>,
    /// `true` when the log was dropped by a reorg.
    removed: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct TronAccountResponse {
    data: Option<Vec<TronAccount>>,
}

#[derive(Debug, Deserialize)]
struct TronAccount {
    trc20: Option<Vec<std::collections::HashMap<String, String>>>,
}

/// One entry of `getSignaturesForAddress`.
#[derive(Debug, Deserialize)]
struct SolanaSignatureInfo {
    signature: Option<String>,
    /// `null` on success, the `TransactionError` otherwise.
    err: Option<serde_json::Value>,
    #[serde(rename = "confirmationStatus")]
    confirmation_status: Option<String>,
}

#[derive(Debug, Deserialize)]
struct HeliusTransaction {
    signature: String,
    /// `null` on success.
    #[serde(rename = "transactionError")]
    transaction_error: Option<serde_json::Value>,
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
    /// UI amount (a JSON number).
    #[serde(rename = "tokenAmount")]
    token_amount: Option<serde_json::Value>,
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
    use config::{USDT_ETH, USDT_SOLANA};

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

    #[test]
    fn eth_scan_window_covers_the_longest_payment_plus_grace() {
        // (1440 + 120) min × 60 s / 12 s per block.
        assert_eq!(ETH_LOG_SCAN_BLOCKS, 7800);
    }

    const RECIPIENT: &str = "0x9858effd232b4033e47d90003d41ec34ecaeda94";

    fn recipient_topic() -> String {
        pad_address_topic(RECIPIENT).unwrap()
    }

    fn eth_log(contract: &str, topics: &[&str], removed: bool) -> String {
        let topics: Vec<String> = topics.iter().map(|t| format!("\"{t}\"")).collect();
        format!(
            r#"{{
                "address": "{contract}",
                "topics": [{}],
                "data": "0x00000000000000000000000000000000000000000000000000000000017d7840",
                "blockNumber": "0x14a3c2f",
                "transactionHash": "0x5e6b3d1c1f6c6f1b7b7f5f2b6ce4b1b7f5b8a7b1c3d6e9f2a4b5c6d7e8f90a1b",
                "transactionIndex": "0x5",
                "blockHash": "0x8b2a1d3c4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f9",
                "logIndex": "0x1b",
                "removed": {removed}
            }}"#,
            topics.join(", ")
        )
    }

    #[test]
    fn eth_logs_are_rechecked_against_the_filter() {
        let from = "0x000000000000000000000000a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4";
        let to = recipient_topic();
        let other = "0x0000000000000000000000001111111111111111111111111111111111111111";
        // Same Transfer from a fake token contract, an ERC-721-style
        // 4-topic Transfer, another recipient, a reorged log, and the
        // genuine USDT log (contract in checksum case).
        let logs = format!(
            "[{}, {}, {}, {}, {}]",
            eth_log("0x1234567890abcdef1234567890abcdef12345678", &[TRANSFER_TOPIC, from, &to], false),
            eth_log(USDT_ETH, &[TRANSFER_TOPIC, from, &to, other], false),
            eth_log(USDT_ETH, &[TRANSFER_TOPIC, from, other], false),
            eth_log(USDT_ETH, &[TRANSFER_TOPIC, from, &to], true),
            eth_log(&USDT_ETH.to_lowercase(), &[TRANSFER_TOPIC, from, &to.to_uppercase().replace("0X", "0x")], false),
        );
        let logs: Vec<EthLog> = serde_json::from_str(&logs).unwrap();
        let transfers = eth_usdt_transfers(logs, USDT_ETH, &to, 0x14a3c2f + 11).unwrap();
        assert_eq!(transfers.len(), 1);
        assert_eq!(transfers[0].amount_raw, "25000000");
        assert_eq!(transfers[0].confirmations, 12);
        assert_eq!(
            transfers[0].tx_hash,
            "0x5e6b3d1c1f6c6f1b7b7f5f2b6ce4b1b7f5b8a7b1c3d6e9f2a4b5c6d7e8f90a1b"
        );
    }

    #[test]
    fn eth_logs_without_contract_or_topics_are_skipped() {
        let logs: Vec<EthLog> = serde_json::from_str(
            r#"[{ "data": "0x01", "blockNumber": "0x1", "transactionHash": "0xab", "topics": null }]"#,
        )
        .unwrap();
        assert!(eth_usdt_transfers(logs, USDT_ETH, &recipient_topic(), 10).unwrap().is_empty());
    }

    #[test]
    fn rpc_errors_are_reported_not_mistaken_for_results() {
        let resp: RpcResponse<String> = serde_json::from_str(
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32602,"message":"Invalid param: could not find account"}}"#,
        )
        .unwrap();
        assert!(resp.result.is_none());
        assert_eq!(
            resp.error.unwrap().describe(),
            "-32602 Invalid param: could not find account"
        );
    }

    /// `getSignaturesForAddress` answer: one success, one failed tx, one
    /// not yet finalized.
    const SIGNATURES: &str = r#"{
        "jsonrpc": "2.0",
        "result": [
            {
                "blockTime": 1702345325,
                "confirmationStatus": "finalized",
                "err": null,
                "memo": null,
                "signature": "5h6xBEauJ3PK6SWCZ1PGjBvj8vDdWG3KpwATGy1ARAXFSDwt8GFXM7W5Ncn16wmqokgpiKRLuS83KUxyZyv2sUYv",
                "slot": 114
            },
            {
                "blockTime": 1702345326,
                "confirmationStatus": "finalized",
                "err": { "InstructionError": [0, { "Custom": 1 }] },
                "memo": null,
                "signature": "3AsdoALgZFuq2oUVWrDYhg2pNeaLJKPLf8hU2mQ6U8qJxeJ6hsrPVpMn9ma39DtfYCrDQSvngWRP8NnTpEhezJpE",
                "slot": 115
            },
            {
                "blockTime": 1702345327,
                "confirmationStatus": "confirmed",
                "err": null,
                "memo": null,
                "signature": "4YQdBQnkzKY6TnHRqgDXq4Ef7AeZWNYwRhDF3kmxLk3fHGoKY3BrzgFZoZQKS1dZGXB4LCGnAUpDZsHxfhHvdcWm",
                "slot": 116
            }
        ],
        "id": 1
    }"#;

    #[test]
    fn failed_and_unfinalized_signatures_are_skipped() {
        let resp: RpcResponse<Vec<SolanaSignatureInfo>> = serde_json::from_str(SIGNATURES).unwrap();
        assert_eq!(
            finalized_successful_signatures(resp.result.unwrap()),
            vec!["5h6xBEauJ3PK6SWCZ1PGjBvj8vDdWG3KpwATGy1ARAXFSDwt8GFXM7W5Ncn16wmqokgpiKRLuS83KUxyZyv2sUYv"]
        );
    }

    const ATA: &str = "7UX2i7SucgLMQcfZ75s3VXmZZY4YRUyJN9X1RgfMoDUi";

    fn helius(transaction_error: &str, transfers: &str) -> HeliusTransaction {
        serde_json::from_str(&format!(
            r#"{{
                "description": "",
                "type": "TRANSFER",
                "source": "SOLANA_PROGRAM_LIBRARY",
                "fee": 5000,
                "feePayer": "8psNvWTrdNTiVRNzAgsou9kETXNJm2SXZyaKuJraVRtf",
                "signature": "5h6xBEauJ3PK6SWCZ1PGjBvj8vDdWG3KpwATGy1ARAXFSDwt8GFXM7W5Ncn16wmqokgpiKRLuS83KUxyZyv2sUYv",
                "slot": 114,
                "timestamp": 1702345325,
                "tokenTransfers": {transfers},
                "nativeTransfers": [],
                "accountData": [],
                "transactionError": {transaction_error},
                "instructions": [],
                "events": {{}}
            }}"#
        ))
        .unwrap()
    }

    fn token_transfer(mint: &str, to_token_account: &str, amount: &str) -> String {
        format!(
            r#"{{
                "fromTokenAccount": "3Gf3hU7pTkTGo8vCgx6ruqBJ3DqDWSWCDWJ9eG5Uf9yB",
                "toTokenAccount": "{to_token_account}",
                "fromUserAccount": "8psNvWTrdNTiVRNzAgsou9kETXNJm2SXZyaKuJraVRtf",
                "toUserAccount": "9WzDXwBbmkg8ZTbNMqUxvQRAyrZzDsGYdLVL9zYtAWWM",
                "tokenAmount": {amount},
                "mint": "{mint}",
                "tokenStandard": "Fungible"
            }}"#
        )
    }

    #[test]
    fn helius_usdt_transfers_to_the_address_are_detected() {
        let usdc = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
        let transfers = format!(
            "[{}, {}, {}]",
            token_transfer(USDT_SOLANA, ATA, "25.5"),
            token_transfer(usdc, ATA, "100"),
            token_transfer(USDT_SOLANA, "3Gf3hU7pTkTGo8vCgx6ruqBJ3DqDWSWCDWJ9eG5Uf9yB", "7"),
        );
        let detected = helius_usdt_transfers(helius("null", &transfers), USDT_SOLANA, ATA);
        assert_eq!(detected.len(), 1);
        assert_eq!(detected[0].amount_raw, "25500000");
        assert_eq!(detected[0].confirmations, 1);
    }

    #[test]
    fn failed_helius_transactions_credit_nothing() {
        let transfers = format!("[{}]", token_transfer(USDT_SOLANA, ATA, "25"));
        let failed = helius(r#"{ "InstructionError": [0, { "Custom": 1 }] }"#, &transfers);
        assert!(helius_usdt_transfers(failed, USDT_SOLANA, ATA).is_empty());
    }

    #[test]
    fn helius_amounts_are_never_made_up() {
        // Raw amount wins; a string or missing UI amount is not guessed.
        let raw = r#"{ "toTokenAccount": "ATA", "mint": "M", "tokenAmount": 1.5,
                       "rawTokenAmount": { "tokenAmount": "1500001", "decimals": 6 } }"#;
        let raw: HeliusTokenTransfer = serde_json::from_str(raw).unwrap();
        assert_eq!(helius_amount_raw(&raw).as_deref(), Some("1500001"));
        let text: HeliusTokenTransfer =
            serde_json::from_str(r#"{ "tokenAmount": "25000000" }"#).unwrap();
        assert_eq!(helius_amount_raw(&text), None);
        let missing: HeliusTokenTransfer = serde_json::from_str("{}").unwrap();
        assert_eq!(helius_amount_raw(&missing), None);
        let negative: HeliusTokenTransfer =
            serde_json::from_str(r#"{ "tokenAmount": -3 }"#).unwrap();
        assert_eq!(helius_amount_raw(&negative), None);
        let tiny: HeliusTokenTransfer =
            serde_json::from_str(r#"{ "tokenAmount": 0.000001 }"#).unwrap();
        assert_eq!(helius_amount_raw(&tiny).as_deref(), Some("1"));
        // Would overflow `Decimal` in `decimal_to_raw` (panic) if not bounded.
        for huge in ["1e25", "7.9e28", "1e300"] {
            let transfer: HeliusTokenTransfer =
                serde_json::from_str(&format!(r#"{{ "tokenAmount": {huge} }}"#)).unwrap();
            assert_eq!(helius_amount_raw(&transfer), None, "{huge}");
        }
    }

    fn clients(vars: &[(&str, &str)]) -> ChainClients {
        let mut all = vec![
            ("WALLET_MNEMONIC", "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about"),
            ("DATABASE_URL", "postgres://localhost/test"),
            ("REDIS_URL", "redis://localhost"),
        ];
        all.extend_from_slice(vars);
        let config = AppConfig::from_lookup(|key| {
            all.iter().find(|(k, _)| *k == key).map(|(_, v)| v.to_string())
        })
        .unwrap();
        ChainClients::new(Arc::new(config))
    }

    #[tokio::test]
    async fn disabled_networks_are_unavailable_and_not_polled() {
        let mut chain = clients(&[("TRONGRID_API_KEY", "k"), ("ALCHEMY_API_KEY", "k")]);
        assert!(chain.is_network_available(Network::Tron));
        assert!(!chain.is_network_available(Network::Solana));

        chain.disable_network(Network::Tron);
        chain.disable_network(Network::Tron);
        assert!(!chain.is_network_available(Network::Tron));
        assert!(chain.is_network_available(Network::Ethereum));
        assert_eq!(
            chain.missing_key_message(Network::Tron),
            "Provider failed the mainnet check at boot"
        );
        assert_eq!(chain.missing_key_message(Network::Solana), "HELIUS_API_KEY not configured");
        // No request is made for a disabled or unconfigured network.
        assert!(chain.poll_tron_address("TJDENsfBJs4RFETt1X1W8wMDc8M5XnJhCe").await.unwrap().is_empty());
        assert!(chain.verify_network(Network::Tron).await.is_none());
        assert!(chain.verify_network(Network::Solana).await.is_none());
        assert!(chain.verify_network(Network::Lightning).await.is_none());
    }
}
