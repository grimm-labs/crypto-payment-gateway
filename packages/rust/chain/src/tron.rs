//! TronGrid payloads and the pure functions that turn them into detected
//! transfers.
//!
//! `GET /v1/accounts/{address}/transactions/trc20` carries no block number
//! (only `block_timestamp`), so each transfer's block is resolved through
//! `POST /walletsolidity/gettransactioninfobyid` — answered only once the
//! transaction is solidified — and cached, since a solidified block never
//! changes.

use std::collections::{HashMap, VecDeque};
use std::sync::{Mutex, PoisonError};

use serde::Deserialize;

/// Txids whose solidified block is remembered; past this, the oldest are
/// forgotten (they are re-resolved if they show up again).
pub(crate) const TX_BLOCK_CACHE_CAPACITY: usize = 4096;

/// `GET /v1/accounts/{address}/transactions/trc20`.
#[derive(Debug, Deserialize)]
pub(crate) struct Trc20Page {
    pub(crate) data: Option<Vec<Trc20Tx>>,
    pub(crate) success: Option<bool>,
    pub(crate) error: Option<String>,
}

/// One TRC-20 event. Fields are optional so that one odd entry is skipped
/// instead of failing the whole page.
#[derive(Debug, Deserialize)]
pub(crate) struct Trc20Tx {
    transaction_id: Option<String>,
    token_info: Option<Trc20TokenInfo>,
    to: Option<String>,
    /// Event name: `Transfer`, `Approval`, …
    #[serde(rename = "type")]
    kind: Option<String>,
    /// Integer string in token base units.
    value: Option<String>,
    /// Milliseconds since the epoch of the transaction's block.
    block_timestamp: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct Trc20TokenInfo {
    address: Option<String>,
}

/// A USDT transfer to the polled address, its block still to be resolved.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Trc20Candidate {
    pub(crate) tx_id: String,
    pub(crate) amount_raw: String,
    /// Unix seconds of its block (`block_timestamp`).
    pub(crate) block_time: Option<i64>,
}

/// USDT `Transfer` events paying `address`. Anything else — another token,
/// an `Approval` naming the address as spender, a transfer elsewhere, a
/// malformed txid or amount — is dropped.
pub(crate) fn usdt_transfers_to(txs: Vec<Trc20Tx>, usdt_contract: &str, address: &str) -> Vec<Trc20Candidate> {
    txs.into_iter()
        .filter_map(|tx| {
            if tx.kind.as_deref() != Some("Transfer")
                || tx.to.as_deref() != Some(address)
                || tx.token_info.and_then(|t| t.address).as_deref() != Some(usdt_contract)
            {
                return None;
            }
            let tx_id = tx.transaction_id.filter(|id| is_tx_id(id))?;
            let amount: u128 = tx.value?.parse().ok()?;
            Some(Trc20Candidate {
                tx_id,
                amount_raw: amount.to_string(),
                block_time: tx.block_timestamp.map(|ms| ms.div_euclid(1000)),
            })
        })
        .collect()
}

fn is_tx_id(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit())
}

/// `POST /walletsolidity/gettransactioninfobyid`: `{}` while the solidity
/// node does not know the transaction (not solidified yet), `{"Error": …}`
/// on a malformed id.
#[derive(Debug, Deserialize)]
pub(crate) struct TxInfo {
    id: Option<String>,
    #[serde(rename = "blockNumber")]
    block_number: Option<u64>,
    /// Absent on success (proto default `SUCESS`), `FAILED` otherwise.
    result: Option<String>,
    receipt: Option<TxReceipt>,
}

#[derive(Debug, Deserialize)]
struct TxReceipt {
    /// Contract execution result: `SUCCESS`, `REVERT`, `OUT_OF_ENERGY`, …
    result: Option<String>,
}

/// The solidified block holding `tx_id`, or `None` when the node does not
/// know it (yet), answers about another tx, or reports a failed execution.
/// Never a guessed height.
pub(crate) fn solidified_block(info: &TxInfo, tx_id: &str) -> Option<u64> {
    if !info.id.as_deref().is_some_and(|id| id.eq_ignore_ascii_case(tx_id)) {
        return None;
    }
    if !matches!(info.result.as_deref(), None | Some("SUCESS") | Some("SUCCESS")) {
        return None;
    }
    if info.receipt.as_ref().and_then(|r| r.result.as_deref()) != Some("SUCCESS") {
        return None;
    }
    // Block 0 is the genesis block: no TRC-20 transfer lives there.
    info.block_number.filter(|block| *block > 0)
}

/// `POST /wallet/getnowblock` and `/wallet/getblockbynum`.
#[derive(Debug, Deserialize)]
pub(crate) struct Block {
    #[serde(rename = "blockID")]
    pub(crate) block_id: Option<String>,
    block_header: Option<BlockHeader>,
}

#[derive(Debug, Deserialize)]
struct BlockHeader {
    raw_data: Option<BlockRawData>,
}

#[derive(Debug, Deserialize)]
struct BlockRawData {
    number: Option<u64>,
}

impl Block {
    pub(crate) fn number(&self) -> Option<u64> {
        self.block_header.as_ref()?.raw_data.as_ref()?.number
    }
}

/// Bounded FIFO cache txid → solidified block.
#[derive(Debug)]
pub(crate) struct TxBlockCache {
    capacity: usize,
    inner: Mutex<CacheInner>,
}

#[derive(Debug, Default)]
struct CacheInner {
    blocks: HashMap<String, u64>,
    order: VecDeque<String>,
}

impl TxBlockCache {
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            inner: Mutex::new(CacheInner::default()),
        }
    }

    pub(crate) fn get(&self, tx_id: &str) -> Option<u64> {
        let inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        inner.blocks.get(&tx_id.to_ascii_lowercase()).copied()
    }

    pub(crate) fn insert(&self, tx_id: &str, block: u64) {
        let key = tx_id.to_ascii_lowercase();
        let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        if inner.blocks.insert(key.clone(), block).is_some() {
            return;
        }
        inner.order.push_back(key);
        while inner.order.len() > self.capacity {
            if let Some(oldest) = inner.order.pop_front() {
                inner.blocks.remove(&oldest);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use config::USDT_TRON;

    const ADDRESS: &str = "TJDENsfBJs4RFETt1X1W8wMDc8M5XnJhCe";
    const TX_A: &str = "4e5169e0889ec563b270240c27c3a1b4a517a86ec6a2ffc0cc33da4b4d107048";
    const TX_B: &str = "ad001f194b8a33cdcef756a4ed1c87612e45d8116b02034451f50434b85511c0";

    /// Real TronGrid answer (`only_confirmed=true&only_to=true&
    /// contract_address=USDT&limit=2`, 2026-10) — note: no block number.
    const TRC20_PAGE: &str = r#"{
        "data": [
            {
                "transaction_id": "4e5169e0889ec563b270240c27c3a1b4a517a86ec6a2ffc0cc33da4b4d107048",
                "token_info": {
                    "symbol": "USDT",
                    "address": "TR7NHqjeKQxGTCi8q8ZY4pL8otSzgjLj6t",
                    "decimals": 6,
                    "name": "Tether USD"
                },
                "block_timestamp": 1791668424000,
                "from": "TDqSquXBgUCLYvYC4XZgrprLK589dkhSCf",
                "to": "TJDENsfBJs4RFETt1X1W8wMDc8M5XnJhCe",
                "type": "Transfer",
                "value": "27033535803560"
            },
            {
                "transaction_id": "ad001f194b8a33cdcef756a4ed1c87612e45d8116b02034451f50434b85511c0",
                "token_info": {
                    "symbol": "USDT",
                    "address": "TR7NHqjeKQxGTCi8q8ZY4pL8otSzgjLj6t",
                    "decimals": 6,
                    "name": "Tether USD"
                },
                "block_timestamp": 1791630024000,
                "from": "TDqSquXBgUCLYvYC4XZgrprLK589dkhSCf",
                "to": "TJDENsfBJs4RFETt1X1W8wMDc8M5XnJhCe",
                "type": "Transfer",
                "value": "26710221406060"
            }
        ],
        "success": true,
        "meta": {
            "at": 1791681042189,
            "fingerprint": "Cndjyb3vXAcmbZ5Qkzas1XWWxWmyEp6mfFF5uhUvq7pGtedGVQCdzZzjAxFf5bd4SDZPYE3iUvz2Uxx5aamD2R1gL7XYiN9pFGhfGXMSjExS8jPfGDbZxatN3P6w4hvVXQzzxkEM24jJ4XDjjt22d7Yu9vVT825stNKi5cAKtN8n9aDF8RrvpSZs6KamiyoSGvQB7M7Gj7kmuZU6CsMztJTEZwCDqgx7CHc6L6yYVVFGYBhLXRCDbxM2RfqT6MtWuJZnwbuere2qx",
            "links": {
                "next": "https://api.trongrid.io/v1/accounts/TJDENsfBJs4RFETt1X1W8wMDc8M5XnJhCe/transactions/trc20?contract_address=TR7NHqjeKQxGTCi8q8ZY4pL8otSzgjLj6t&limit=2&only_confirmed=true&only_to=true&fingerprint=Cndjyb3vXAcmbZ5Qkzas1XWWxWmyEp6mfFF5uhUvq7pGtedGVQCdzZzjAxFf5bd4SDZPYE3iUvz2Uxx5aamD2R1gL7XYiN9pFGhfGXMSjExS8jPfGDbZxatN3P6w4hvVXQzzxkEM24jJ4XDjjt22d7Yu9vVT825stNKi5cAKtN8n9aDF8RrvpSZs6KamiyoSGvQB7M7Gj7kmuZU6CsMztJTEZwCDqgx7CHc6L6yYVVFGYBhLXRCDbxM2RfqT6MtWuJZnwbuere2qx"
            },
            "page_size": 2
        }
    }"#;

    /// Real `/walletsolidity/gettransactioninfobyid` answer for `TX_A`.
    const TX_INFO: &str = r#"{
        "id": "4e5169e0889ec563b270240c27c3a1b4a517a86ec6a2ffc0cc33da4b4d107048",
        "blockNumber": 86998534,
        "blockTimeStamp": 1791668424000,
        "contractResult": [
            "0000000000000000000000000000000000000000000000000000000000000000"
        ],
        "contract_address": "41a614f803b6fd780986a42c78ec9c7f77e6ded13c",
        "receipt": {
            "energy_usage": 64285,
            "energy_usage_total": 64285,
            "net_usage": 345,
            "result": "SUCCESS",
            "energy_penalty_total": 49635
        },
        "log": [
            {
                "address": "a614f803b6fd780986a42c78ec9c7f77e6ded13c",
                "topics": [
                    "ddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef",
                    "0000000000000000000000002a68baf67f1c497d9a4a609276a90dcd6ea77444",
                    "0000000000000000000000005a67fa7cc56bd6d043a98e17d329c1dc9e14753f"
                ],
                "data": "000000000000000000000000000000000000000000000000000018963c4c70a8"
            }
        ]
    }"#;

    fn page(json: &str) -> Vec<Trc20Tx> {
        serde_json::from_str::<Trc20Page>(json).unwrap().data.unwrap()
    }

    fn info(json: &str) -> TxInfo {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn real_trc20_page_yields_its_usdt_transfers() {
        let candidates = usdt_transfers_to(page(TRC20_PAGE), USDT_TRON, ADDRESS);
        assert_eq!(
            candidates,
            vec![
                Trc20Candidate { tx_id: TX_A.into(), amount_raw: "27033535803560".into(), block_time: Some(1_791_668_424) },
                Trc20Candidate { tx_id: TX_B.into(), amount_raw: "26710221406060".into(), block_time: Some(1_791_630_024) },
            ]
        );
    }

    #[test]
    fn other_tokens_approvals_and_other_recipients_are_dropped() {
        let entry = |id: &str, token: &str, to: &str, kind: &str, value: &str| {
            format!(
                r#"{{ "transaction_id": "{id}", "token_info": {{ "address": "{token}", "decimals": 6 }},
                     "block_timestamp": 1, "from": "TDqSquXBgUCLYvYC4XZgrprLK589dkhSCf",
                     "to": "{to}", "type": "{kind}", "value": "{value}" }}"#
            )
        };
        let usdc = "TEkxiTehnzSmSe2XqrBj4w32RUN966rdz8";
        let other = "TDqSquXBgUCLYvYC4XZgrprLK589dkhSCf";
        let json = format!(
            r#"{{ "data": [{}, {}, {}, {}, {}, {}, {{ "transaction_id": "{TX_A}" }}], "success": true }}"#,
            entry(TX_A, usdc, ADDRESS, "Transfer", "1"),
            entry(TX_A, USDT_TRON, ADDRESS, "Approval", "25000000"),
            entry(TX_A, USDT_TRON, other, "Transfer", "25000000"),
            entry("not-a-txid", USDT_TRON, ADDRESS, "Transfer", "25000000"),
            entry(TX_A, USDT_TRON, ADDRESS, "Transfer", "25.5"),
            entry(TX_B, USDT_TRON, ADDRESS, "Transfer", "25000000"),
        );
        assert_eq!(
            usdt_transfers_to(page(&json), USDT_TRON, ADDRESS),
            vec![Trc20Candidate { tx_id: TX_B.into(), amount_raw: "25000000".into(), block_time: Some(0) }]
        );
    }

    #[test]
    fn a_trc20_transfer_mined_before_the_payment_is_not_its_deposit() {
        // `block_timestamp` is in milliseconds; TronGrid lists solidified
        // transfers only, so every one has a block.
        let candidates = usdt_transfers_to(page(TRC20_PAGE), USDT_TRON, ADDRESS);
        let (tx_a, tx_b) = (candidates[0].block_time, candidates[1].block_time);
        // A payment created right after TX_A: TX_B (10 h 40 before) is old.
        let created = 1_791_668_424 + 60;
        assert!(!domain::predates_payment(tx_a, created));
        assert!(domain::predates_payment(tx_b, created));
        // Within the clock-skew margin: counted.
        assert!(!domain::predates_payment(tx_a, 1_791_668_424 + domain::TRANSFER_BEFORE_PAYMENT_MARGIN_SECS));
        assert!(domain::predates_payment(tx_a, 1_791_668_424 + domain::TRANSFER_BEFORE_PAYMENT_MARGIN_SECS + 1));
        // No timestamp (an odd entry): never judged older.
        assert!(!domain::predates_payment(None, created));
    }

    #[test]
    fn empty_or_error_pages_have_no_data() {
        let empty: Trc20Page = serde_json::from_str(r#"{ "data": [], "success": true, "meta": {} }"#).unwrap();
        assert!(usdt_transfers_to(empty.data.unwrap(), USDT_TRON, ADDRESS).is_empty());
        let failed: Trc20Page =
            serde_json::from_str(r#"{ "success": false, "error": "bad address", "statusCode": 400 }"#)
                .unwrap();
        assert_eq!(failed.success, Some(false));
        assert!(failed.data.is_none());
    }

    #[test]
    fn real_tx_info_resolves_its_solidified_block() {
        assert_eq!(solidified_block(&info(TX_INFO), TX_A), Some(86998534));
        assert_eq!(solidified_block(&info(TX_INFO), &TX_A.to_uppercase()), Some(86998534));
    }

    #[test]
    fn unknown_or_malformed_tx_info_resolves_nothing() {
        // Not solidified yet.
        assert_eq!(solidified_block(&info("{}"), TX_A), None);
        assert_eq!(
            solidified_block(
                &info(r#"{"Error":"class org.tron.core.services.http.JsonFormat$ParseException : 1:10: INVALID hex String"}"#),
                TX_A
            ),
            None
        );
        // Answer about another transaction.
        assert_eq!(solidified_block(&info(TX_INFO), TX_B), None);
        // No block number.
        let no_block = format!(r#"{{ "id": "{TX_A}", "receipt": {{ "result": "SUCCESS" }} }}"#);
        assert_eq!(solidified_block(&info(&no_block), TX_A), None);
    }

    #[test]
    fn failed_executions_resolve_nothing() {
        let reverted = format!(
            r#"{{ "id": "{TX_A}", "blockNumber": 5, "result": "FAILED", "resMessage": "5452414e534645525f4641494c4544",
                 "receipt": {{ "result": "REVERT" }} }}"#
        );
        assert_eq!(solidified_block(&info(&reverted), TX_A), None);
        let no_receipt_result = format!(r#"{{ "id": "{TX_A}", "blockNumber": 5, "receipt": {{ "net_fee": 345 }} }}"#);
        assert_eq!(solidified_block(&info(&no_receipt_result), TX_A), None);
    }

    #[test]
    fn real_head_block_parses_its_number() {
        let block: Block = serde_json::from_str(
            r#"{"blockID":"00000000052f8e6fa5e9ed4a82ca3f7a1ffdb55a78c09550566902caa54cb164",
                "block_header":{"raw_data":{"number":87002735,
                "txTrieRoot":"836d84e3d849fd1d6a9a690e9f1f8439fb84b9f1fd77c6997de2f53fdf103883",
                "witness_address":"418b0359acac03bac62cbf89c4b787cb10b3c3f513",
                "parentHash":"00000000052f8e6efee0000000000000000000000000000000000000000000000",
                "version":32,"timestamp":1791681042000},
                "witness_signature":"00"}}"#,
        )
        .unwrap();
        assert_eq!(block.number(), Some(87002735));
        // 4202 confirmations for TX_A, solidified at 86998534.
        assert_eq!(crate::confirmations_between(87002735, 86998534), 4202);
        let empty: Block = serde_json::from_str("{}").unwrap();
        assert_eq!(empty.number(), None);
    }

    #[test]
    fn cache_is_case_insensitive_and_bounded() {
        let cache = TxBlockCache::new(2);
        cache.insert(TX_A, 10);
        assert_eq!(cache.get(&TX_A.to_uppercase()), Some(10));
        cache.insert(TX_B, 11);
        cache.insert(TX_B, 11);
        cache.insert("cc", 12);
        // TX_A was the oldest: evicted.
        assert_eq!(cache.get(TX_A), None);
        assert_eq!(cache.get(TX_B), Some(11));
        assert_eq!(cache.get("CC"), Some(12));
    }
}
