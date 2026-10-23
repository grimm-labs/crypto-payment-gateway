//! Esplora REST payloads (mempool.space / blockstream.info / self-hosted)
//! and the pure functions that turn them into detected transfers.

use serde::Deserialize;

use crate::{confirmations_between, DetectedOnChainTransfer};

/// One entry of `GET /address/{address}/txs` (mempool + recent confirmed).
#[derive(Debug, Deserialize)]
pub(crate) struct EsploraTx {
    txid: String,
    #[serde(default)]
    vout: Vec<EsploraVout>,
    status: EsploraTxStatus,
}

#[derive(Debug, Deserialize)]
struct EsploraVout {
    /// Absent on outputs without a standard address (OP_RETURN, …).
    scriptpubkey_address: Option<String>,
    /// Satoshis.
    value: u64,
}

#[derive(Debug, Deserialize)]
struct EsploraTxStatus {
    confirmed: bool,
    block_height: Option<u64>,
    /// Unix seconds of the block (confirmed transactions only).
    block_time: Option<i64>,
}

/// `GET /address/{address}`.
#[derive(Debug, Deserialize)]
pub(crate) struct EsploraAddress {
    chain_stats: EsploraChainStats,
}

#[derive(Debug, Deserialize)]
struct EsploraChainStats {
    funded_txo_sum: u64,
    spent_txo_sum: u64,
}

/// One transfer per tx paying `address`: the amount is the sum of every
/// output to that address (a tx may pay it several times); txs that pay it
/// nothing (spends, zero-value outputs) are skipped. Mempool txs have 0
/// confirmations.
pub(crate) fn esplora_transfers(
    txs: Vec<EsploraTx>,
    address: &str,
    tip: u64,
) -> Vec<DetectedOnChainTransfer> {
    txs.into_iter()
        .filter_map(|tx| {
            let amount: u128 = tx
                .vout
                .iter()
                .filter(|out| out.scriptpubkey_address.as_deref() == Some(address))
                .map(|out| u128::from(out.value))
                .sum();
            if amount == 0 {
                return None;
            }
            let confirmations = match (tx.status.confirmed, tx.status.block_height) {
                (true, Some(height)) => confirmations_between(tip, height),
                _ => 0,
            };
            Some(DetectedOnChainTransfer {
                tx_hash: tx.txid,
                amount_raw: amount.to_string(),
                confirmations,
                block_time: tx.status.confirmed.then_some(tx.status.block_time).flatten(),
            })
        })
        .collect()
}

/// Confirmed balance in sats (mempool excluded).
pub(crate) fn confirmed_balance_sats(account: &EsploraAddress) -> u128 {
    u128::from(
        account
            .chain_stats
            .funded_txo_sum
            .saturating_sub(account.chain_stats.spent_txo_sum),
    )
}

/// Everything ever received by `address` with at least `depth`
/// confirmations, spent outputs included (the Bitcoin "received"
/// watermark of the project ledger). `chain_stats.funded_txo_sum` counts
/// every confirmed funding output (one confirmation or more); the funding
/// of the recent transactions still short of `depth` — all of them are in
/// the most recent page Esplora returns — is subtracted from it. Mempool
/// transactions were never counted.
pub(crate) fn received_at_depth(
    account: &EsploraAddress,
    txs: &[EsploraTx],
    address: &str,
    tip: u64,
    depth: i32,
) -> u128 {
    let shallow: u128 = txs
        .iter()
        .filter(|tx| match (tx.status.confirmed, tx.status.block_height) {
            (true, Some(height)) => confirmations_between(tip, height) < depth,
            _ => false,
        })
        .flat_map(|tx| tx.vout.iter())
        .filter(|out| out.scriptpubkey_address.as_deref() == Some(address))
        .map(|out| u128::from(out.value))
        .sum();
    u128::from(account.chain_stats.funded_txo_sum).saturating_sub(shallow)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ADDRESS: &str = "bc1qcr8te4kr609gcawutmrza0j4xv80jy8z306fyu";
    const OTHER: &str = "bc1qnjg0jd8228aq7egyzacy8cys3knf9xvrerkf9g";

    fn txs(json: &str) -> Vec<EsploraTx> {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn sums_every_output_paying_the_address() {
        let txs = txs(&format!(
            r#"[{{
                "txid": "aa",
                "vout": [
                    {{ "scriptpubkey_address": "{ADDRESS}", "value": 100000 }},
                    {{ "scriptpubkey_address": "{OTHER}", "value": 777 }},
                    {{ "scriptpubkey_address": "{ADDRESS}", "value": 50000 }},
                    {{ "scriptpubkey_type": "op_return", "value": 0 }}
                ],
                "status": {{ "confirmed": true, "block_height": 850000 }}
            }}]"#
        ));
        let transfers = esplora_transfers(txs, ADDRESS, 850002);
        assert_eq!(transfers.len(), 1);
        assert_eq!(transfers[0].tx_hash, "aa");
        assert_eq!(transfers[0].amount_raw, "150000");
        assert_eq!(transfers[0].confirmations, 3);
    }

    #[test]
    fn skips_transactions_that_do_not_pay_the_address() {
        // e.g. the address spending its own funds, or a zero-value output.
        let txs = txs(&format!(
            r#"[
                {{ "txid": "spend", "vout": [{{ "scriptpubkey_address": "{OTHER}", "value": 1000 }}],
                   "status": {{ "confirmed": true, "block_height": 10 }} }},
                {{ "txid": "zero", "vout": [{{ "scriptpubkey_address": "{ADDRESS}", "value": 0 }}],
                   "status": {{ "confirmed": false }} }}
            ]"#
        ));
        assert!(esplora_transfers(txs, ADDRESS, 12).is_empty());
    }

    #[test]
    fn bitcoin_transfers_mined_before_the_payment_are_not_its_deposits() {
        let created = 1_800_000_000;
        let txs = txs(&format!(
            r#"[
                {{ "txid": "old", "vout": [{{ "scriptpubkey_address": "{ADDRESS}", "value": 3517100 }}],
                   "status": {{ "confirmed": true, "block_height": 100, "block_time": {} }} }},
                {{ "txid": "margin", "vout": [{{ "scriptpubkey_address": "{ADDRESS}", "value": 1 }}],
                   "status": {{ "confirmed": true, "block_height": 101, "block_time": {} }} }},
                {{ "txid": "new", "vout": [{{ "scriptpubkey_address": "{ADDRESS}", "value": 10450 }}],
                   "status": {{ "confirmed": true, "block_height": 102, "block_time": {} }} }},
                {{ "txid": "mempool", "vout": [{{ "scriptpubkey_address": "{ADDRESS}", "value": 10450 }}],
                   "status": {{ "confirmed": false }} }}
            ]"#,
            created - 86_400,
            created - domain::TRANSFER_BEFORE_PAYMENT_MARGIN_SECS,
            created + 300,
        ));
        let transfers = esplora_transfers(txs, ADDRESS, 103);
        let predates: Vec<(&str, bool)> = transfers
            .iter()
            .map(|t| (t.tx_hash.as_str(), domain::predates_payment(t.block_time, created)))
            .collect();
        // Older than creation: ignored; within the margin, after, mempool:
        // counted.
        assert_eq!(predates, vec![("old", true), ("margin", false), ("new", false), ("mempool", false)]);
        assert_eq!(transfers[3].block_time, None);
    }

    #[test]
    fn mempool_transactions_have_zero_confirmations() {
        let txs = txs(&format!(
            r#"[{{ "txid": "mempool", "vout": [{{ "scriptpubkey_address": "{ADDRESS}", "value": 42 }}],
                  "status": {{ "confirmed": false }} }}]"#
        ));
        let transfers = esplora_transfers(txs, ADDRESS, 850000);
        assert_eq!(transfers[0].confirmations, 0);
        assert_eq!(transfers[0].amount_raw, "42");
    }

    #[test]
    fn tip_behind_block_height_saturates_to_one_confirmation() {
        // The tip is fetched before the tx list: a block can land in between.
        let txs = txs(&format!(
            r#"[{{ "txid": "fresh", "vout": [{{ "scriptpubkey_address": "{ADDRESS}", "value": 42 }}],
                  "status": {{ "confirmed": true, "block_height": 850001 }} }}]"#
        ));
        assert_eq!(esplora_transfers(txs, ADDRESS, 850000)[0].confirmations, 1);
    }

    #[test]
    fn received_at_depth_subtracts_the_fundings_still_short_of_the_depth() {
        // funded 150 000 (two confirmed fundings: 100 000 deep, 50 000 at
        // one confirmation), 30 000 spent; a mempool funding never counts.
        let account: EsploraAddress = serde_json::from_str(
            r#"{ "chain_stats": { "funded_txo_sum": 150000, "spent_txo_sum": 30000 } }"#,
        )
        .unwrap();
        let txs = txs(&format!(
            r#"[
                {{ "txid": "deep", "vout": [{{ "scriptpubkey_address": "{ADDRESS}", "value": 100000 }}],
                   "status": {{ "confirmed": true, "block_height": 849990 }} }},
                {{ "txid": "fresh", "vout": [{{ "scriptpubkey_address": "{ADDRESS}", "value": 50000 }},
                                             {{ "scriptpubkey_address": "{OTHER}", "value": 5 }}],
                   "status": {{ "confirmed": true, "block_height": 850000 }} }},
                {{ "txid": "mempool", "vout": [{{ "scriptpubkey_address": "{ADDRESS}", "value": 7 }}],
                   "status": {{ "confirmed": false }} }}
            ]"#
        ));
        // Tip 850000: "fresh" has 1 confirmation — not at depth 3.
        assert_eq!(received_at_depth(&account, &txs, ADDRESS, 850000, 3), 100_000);
        // Two blocks later it reaches the depth; the spend never lowers it.
        assert_eq!(received_at_depth(&account, &txs, ADDRESS, 850002, 3), 150_000);
        // Depth 1: every confirmed funding counts, as funded_txo_sum does.
        assert_eq!(received_at_depth(&account, &txs, ADDRESS, 850000, 1), 150_000);
    }

    #[test]
    fn confirmed_without_height_is_not_counted_as_confirmed() {
        let txs = txs(&format!(
            r#"[{{ "txid": "odd", "vout": [{{ "scriptpubkey_address": "{ADDRESS}", "value": 42 }}],
                  "status": {{ "confirmed": true }} }}]"#
        ));
        assert_eq!(esplora_transfers(txs, ADDRESS, 850000)[0].confirmations, 0);
    }

    #[test]
    fn balance_is_funded_minus_spent_saturating() {
        let account: EsploraAddress = serde_json::from_str(
            r#"{ "address": "x", "chain_stats": { "funded_txo_sum": 250000, "spent_txo_sum": 100000, "tx_count": 3 },
                 "mempool_stats": { "funded_txo_sum": 0, "spent_txo_sum": 0, "tx_count": 0 } }"#,
        )
        .unwrap();
        assert_eq!(confirmed_balance_sats(&account), 150000);

        let inconsistent: EsploraAddress = serde_json::from_str(
            r#"{ "chain_stats": { "funded_txo_sum": 1, "spent_txo_sum": 2 } }"#,
        )
        .unwrap();
        assert_eq!(confirmed_balance_sats(&inconsistent), 0);
    }
}
