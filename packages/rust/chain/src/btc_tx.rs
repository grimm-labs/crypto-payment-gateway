//! Bitcoin on-chain withdrawals. There are no sweeps: a withdrawal spends
//! the treasury's change and the confirmed UTXOs of final deposit addresses
//! directly, in one multi-input transaction (ADR 0001). Built with
//! rust-bitcoin, every input signed with its own key over its BIP143
//! sighash through `wallet::Signer`, RBF-enabled.

use std::str::FromStr;

use anyhow::{Context, Result};
use async_trait::async_trait;
use bitcoin::absolute::LockTime;
use bitcoin::consensus::encode::serialize_hex;
use bitcoin::hashes::Hash;
use bitcoin::sighash::{EcdsaSighashType, SighashCache};
use bitcoin::transaction::{predict_weight, InputWeightPrediction, Version};
use bitcoin::{Address, Amount, CompressedPublicKey, OutPoint, ScriptBuf, Sequence, Transaction, TxIn, TxOut, Txid, Witness};
use serde::Deserialize;
use wallet::{KeyRef, Signer};

use crate::{request, ChainClients};

/// Below this change would be dust (P2WPKH): it goes to fees instead.
pub const CHANGE_DUST_SATS: u64 = 294;
/// A UTXO is only spent when worth at least this many times what adding
/// it costs at the current fee rate.
pub const MIN_VALUE_INPUT_COST_MULTIPLE: u64 = 3;
/// BIP125: a replacement pays at least this much more per vbyte of its own
/// size than the original (Bitcoin Core's default incremental relay fee is
/// lower since v29; 1 sat/vB is accepted by every version).
pub const INCREMENTAL_RELAY_SAT_VB: u64 = 1;
/// Script length of the P2WPKH change output.
const P2WPKH_SCRIPT_LEN: usize = 22;

/// A confirmed UTXO the gateway can spend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpendableUtxo {
    pub txid: String,
    pub vout: u32,
    pub value: u64,
    pub address: String,
    /// The key of `address` (treasury, or the deposit's index).
    pub key: KeyRef,
    /// Confirmation height — older UTXOs are spent first.
    pub height: u64,
}

impl SpendableUtxo {
    fn is_treasury(&self) -> bool {
        matches!(self.key, KeyRef::Treasury(_))
    }
}

/// Inputs to spend, the fee, and the change back to the treasury (0 =
/// none).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoinSelection {
    pub inputs: Vec<SpendableUtxo>,
    pub fee: u64,
    pub change: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CoinSelectionError {
    #[error("insufficient spendable funds ({available} sats usable)")]
    Insufficient { available: u64 },
    #[error("more than {0} inputs would be needed")]
    TooManyInputs(usize),
}

/// Virtual size of a transaction with `inputs` P2WPKH inputs and outputs of
/// the given script lengths.
pub fn estimate_vsize(inputs: usize, output_script_lens: &[usize]) -> u64 {
    predict_weight(
        std::iter::repeat_n(InputWeightPrediction::P2WPKH_MAX, inputs),
        output_script_lens.iter().copied(),
    )
    .to_vbytes_ceil()
}

/// What one more P2WPKH input costs at `fee_rate` (sat/vB).
pub fn input_cost(fee_rate: u64) -> u64 {
    (estimate_vsize(2, &[]) - estimate_vsize(1, &[])).saturating_mul(fee_rate)
}

/// Pick inputs for `amount` to a `destination_script_len` output at
/// `fee_rate` (sat/vB): treasury change first, then the oldest deposit
/// UTXOs; UTXOs worth less than 3× their own input cost are skipped; at
/// most `max_inputs`. Change below dust is left to fees.
pub fn select_coins(
    mut utxos: Vec<SpendableUtxo>,
    amount: u64,
    fee_rate: u64,
    destination_script_len: usize,
    max_inputs: usize,
) -> Result<CoinSelection, CoinSelectionError> {
    let min_value = input_cost(fee_rate).saturating_mul(MIN_VALUE_INPUT_COST_MULTIPLE);
    utxos.retain(|utxo| utxo.value >= min_value);
    utxos.sort_by(|a, b| {
        b.is_treasury()
            .cmp(&a.is_treasury())
            .then(a.height.cmp(&b.height))
            .then(a.txid.cmp(&b.txid))
            .then(a.vout.cmp(&b.vout))
    });
    let available: u64 = utxos.iter().map(|u| u.value).fold(0, u64::saturating_add);
    let mut inputs = Vec::new();
    let mut total = 0u64;
    for utxo in utxos {
        if inputs.len() == max_inputs {
            return Err(CoinSelectionError::TooManyInputs(max_inputs));
        }
        total = total.saturating_add(utxo.value);
        inputs.push(utxo);
        if let Some(selection) = settle(&inputs, total, amount, fee_rate, destination_script_len, 0) {
            return Ok(selection);
        }
    }
    Err(CoinSelectionError::Insufficient { available })
}

/// The fee and change of spending `inputs` (worth `total`) on `amount`, if
/// they are enough: with a change output when it is above dust, else
/// without (the remainder is fee). `min_fee` raises the fee (RBF).
fn settle(
    inputs: &[SpendableUtxo],
    total: u64,
    amount: u64,
    fee_rate: u64,
    destination_script_len: usize,
    min_fee: u64,
) -> Option<CoinSelection> {
    let with_change = estimate_vsize(inputs.len(), &[destination_script_len, P2WPKH_SCRIPT_LEN])
        .saturating_mul(fee_rate)
        .max(min_fee);
    if let Some(change) = total.checked_sub(amount.saturating_add(with_change)) {
        if change >= CHANGE_DUST_SATS {
            return Some(CoinSelection { inputs: inputs.to_vec(), fee: with_change, change });
        }
    }
    let without_change = estimate_vsize(inputs.len(), &[destination_script_len])
        .saturating_mul(fee_rate)
        .max(min_fee);
    if total >= amount.saturating_add(without_change) {
        return Some(CoinSelection { inputs: inputs.to_vec(), fee: total - amount, change: 0 });
    }
    None
}

/// A BIP125 replacement of a transaction that spent `previous` for
/// `amount` and paid `previous_fee`: every original input is kept (the
/// replacement conflicts with the whole original), `extra` inputs are added
/// only if needed; the fee is at least `fee_rate` × size and at least the
/// original fee plus the incremental relay fee on the new size.
pub fn select_replacement(
    previous: Vec<SpendableUtxo>,
    mut extra: Vec<SpendableUtxo>,
    amount: u64,
    previous_fee: u64,
    fee_rate: u64,
    destination_script_len: usize,
    max_inputs: usize,
) -> Result<CoinSelection, CoinSelectionError> {
    extra.retain(|u| !previous.iter().any(|p| p.txid == u.txid && p.vout == u.vout));
    extra.sort_by(|a, b| b.is_treasury().cmp(&a.is_treasury()).then(a.height.cmp(&b.height)));
    let mut inputs = previous;
    let mut total: u64 = inputs.iter().map(|u| u.value).fold(0, u64::saturating_add);
    let mut extra = extra.into_iter();
    loop {
        let size = estimate_vsize(inputs.len(), &[destination_script_len, P2WPKH_SCRIPT_LEN]);
        let min_fee = previous_fee.saturating_add(INCREMENTAL_RELAY_SAT_VB.saturating_mul(size));
        if let Some(selection) = settle(&inputs, total, amount, fee_rate, destination_script_len, min_fee) {
            return Ok(selection);
        }
        let Some(next) = extra.next() else {
            return Err(CoinSelectionError::Insufficient { available: total });
        };
        if inputs.len() == max_inputs {
            return Err(CoinSelectionError::TooManyInputs(max_inputs));
        }
        total = total.saturating_add(next.value);
        inputs.push(next);
    }
}

/// A signed transaction ready to broadcast.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignedBtcTx {
    pub hex: String,
    pub txid: String,
    pub vsize: u64,
    pub fee: u64,
}

/// Build and sign: `amount` to `destination`, change (if any) to
/// `change_address` (the treasury). Each input is signed with its own key
/// after checking that key really owns the input's address.
pub fn build_and_sign(
    selection: &CoinSelection,
    destination: &Address,
    amount: u64,
    change_address: &Address,
    signer: &dyn Signer,
) -> Result<SignedBtcTx> {
    let mut tx = Transaction {
        version: Version::TWO,
        lock_time: LockTime::ZERO,
        input: Vec::with_capacity(selection.inputs.len()),
        output: vec![TxOut {
            value: Amount::from_sat(amount),
            script_pubkey: destination.script_pubkey(),
        }],
    };
    if selection.change > 0 {
        tx.output.push(TxOut {
            value: Amount::from_sat(selection.change),
            script_pubkey: change_address.script_pubkey(),
        });
    }
    let mut keys = Vec::with_capacity(selection.inputs.len());
    for utxo in &selection.inputs {
        if signer.address(utxo.key)? != utxo.address {
            anyhow::bail!("key {:?} does not own input address {}", utxo.key, utxo.address);
        }
        let public_key = CompressedPublicKey::from_slice(&signer.public_key(utxo.key)?)
            .context("invalid secp256k1 public key")?;
        tx.input.push(TxIn {
            previous_output: OutPoint {
                txid: Txid::from_str(&utxo.txid).context("invalid input txid")?,
                vout: utxo.vout,
            },
            script_sig: ScriptBuf::new(),
            sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
            witness: Witness::new(),
        });
        keys.push(public_key);
    }

    // Segwit v0 sighashes do not cover witnesses: compute them all on the
    // unsigned transaction, then fill the witnesses.
    let mut sighashes = Vec::with_capacity(tx.input.len());
    {
        let mut cache = SighashCache::new(&tx);
        for (index, (utxo, public_key)) in selection.inputs.iter().zip(&keys).enumerate() {
            let script = ScriptBuf::new_p2wpkh(&public_key.wpubkey_hash());
            let sighash = cache
                .p2wpkh_signature_hash(index, &script, Amount::from_sat(utxo.value), EcdsaSighashType::All)
                .context("sighash")?;
            sighashes.push(sighash.to_byte_array());
        }
    }
    for (index, utxo) in selection.inputs.iter().enumerate() {
        let witness = signer
            .sign_p2wpkh(utxo.key, sighashes[index])
            .context("P2WPKH signing failed")?;
        let mut signature = witness.signature_der;
        signature.push(EcdsaSighashType::All as u8);
        tx.input[index].witness = Witness::from_slice(&[signature, witness.public_key.to_vec()]);
    }

    let inputs_total: u64 = selection.inputs.iter().map(|u| u.value).sum();
    let outputs_total: u64 = tx.output.iter().map(|o| o.value.to_sat()).sum();
    if inputs_total.checked_sub(outputs_total) != Some(selection.fee) {
        anyhow::bail!("fee does not balance inputs and outputs");
    }
    Ok(SignedBtcTx {
        hex: serialize_hex(&tx),
        txid: tx.compute_txid().to_string(),
        vsize: tx.vsize() as u64,
        fee: selection.fee,
    })
}

/// The Esplora fee estimate for `target` blocks (the nearest target at or
/// below it), clamped and rounded up to whole sat/vB.
pub fn fee_rate_for_target(estimates: &std::collections::HashMap<String, f64>, target: u32, min: u64, max: u64) -> u64 {
    let rate = (1..=target)
        .rev()
        .find_map(|t| estimates.get(&t.to_string()).copied())
        .or_else(|| estimates.values().copied().reduce(f64::max))
        .unwrap_or(min as f64);
    let rate = if rate.is_finite() { rate.ceil() as u64 } else { max };
    rate.clamp(min, max)
}

/// One entry of `GET /address/{address}/utxo`.
#[derive(Debug, Clone, Deserialize)]
pub struct EsploraUtxo {
    pub txid: String,
    pub vout: u32,
    pub value: u64,
    pub status: EsploraUtxoStatus,
}

#[derive(Debug, Clone, Deserialize)]
pub struct EsploraUtxoStatus {
    pub confirmed: bool,
    pub block_height: Option<u64>,
}

/// Outcome of `POST /tx`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BtcBroadcastOutcome {
    /// Accepted, or already known (a re-broadcast).
    Accepted,
    /// An input is already spent (by a replacement or another transaction).
    InputsSpent,
    Rejected(String),
}

pub(crate) fn classify_broadcast(status: reqwest::StatusCode, body: &str) -> BtcBroadcastOutcome {
    if status.is_success() {
        return BtcBroadcastOutcome::Accepted;
    }
    let lower = body.to_ascii_lowercase();
    if lower.contains("already in block chain") || lower.contains("txn-already-known") || lower.contains("txn-already-in-mempool") {
        BtcBroadcastOutcome::Accepted
    } else if lower.contains("missingorspent") || lower.contains("txn-mempool-conflict") {
        // A refused RBF replacement ("insufficient fee, rejecting
        // replacement") is not: its original still spends the inputs.
        BtcBroadcastOutcome::InputsSpent
    } else {
        BtcBroadcastOutcome::Rejected(format!("HTTP {} {}", status.as_u16(), body.chars().take(300).collect::<String>()))
    }
}

/// `GET /tx/{txid}/status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BtcTxStatus {
    pub confirmed: bool,
    pub block_height: Option<u64>,
}

/// What the outbound engine needs from Esplora.
#[async_trait]
pub trait EsploraRpc: Send + Sync {
    async fn tip_height(&self) -> Result<u64>;
    async fn utxos(&self, address: &str) -> Result<Vec<EsploraUtxo>>;
    /// sat/vB for `target` blocks, clamped.
    async fn fee_rate(&self, target: u32, min: u64, max: u64) -> Result<u64>;
    async fn broadcast(&self, hex: &str) -> Result<BtcBroadcastOutcome>;
    /// `None` when Esplora does not know the transaction (dropped).
    async fn tx_status(&self, txid: &str) -> Result<Option<BtcTxStatus>>;
    /// The transaction spending `txid:vout`, if any.
    async fn spent_by(&self, txid: &str, vout: u32) -> Result<Option<String>>;
}

impl ChainClients {
    fn esplora(&self, path: &str) -> Result<String> {
        self.esplora_url(path).context("BITCOIN_ESPLORA_BASE_URL not configured")
    }
}

#[async_trait]
impl EsploraRpc for ChainClients {
    async fn tip_height(&self) -> Result<u64> {
        request::send_text(self.http.get(self.esplora("blocks/tip/height")?))
            .await
            .context("Esplora tip request failed")?
            .trim()
            .parse()
            .context("Esplora returned an invalid tip height")
    }

    async fn utxos(&self, address: &str) -> Result<Vec<EsploraUtxo>> {
        request::send_json(self.http.get(self.esplora(&format!("address/{address}/utxo"))?))
            .await
            .context("Esplora utxo request failed")
    }

    async fn fee_rate(&self, target: u32, min: u64, max: u64) -> Result<u64> {
        let estimates: std::collections::HashMap<String, f64> =
            request::send_json(self.http.get(self.esplora("fee-estimates")?))
                .await
                .context("Esplora fee-estimates request failed")?;
        Ok(fee_rate_for_target(&estimates, target, min, max))
    }

    async fn broadcast(&self, hex: &str) -> Result<BtcBroadcastOutcome> {
        let (status, body) = request::send_any(self.http.post(self.esplora("tx")?).body(hex.to_string()))
            .await
            .context("Esplora broadcast request failed")?;
        Ok(classify_broadcast(status, &body))
    }

    async fn tx_status(&self, txid: &str) -> Result<Option<BtcTxStatus>> {
        let (status, body) = request::send_any(self.http.get(self.esplora(&format!("tx/{txid}/status"))?))
            .await
            .context("Esplora tx status request failed")?;
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !status.is_success() {
            anyhow::bail!("Esplora tx status: HTTP {}", status.as_u16());
        }
        #[derive(Deserialize)]
        struct Status {
            confirmed: bool,
            block_height: Option<u64>,
        }
        let parsed: Status = serde_json::from_str(&body).context("Esplora tx status: invalid JSON")?;
        Ok(Some(BtcTxStatus { confirmed: parsed.confirmed, block_height: parsed.block_height }))
    }

    async fn spent_by(&self, txid: &str, vout: u32) -> Result<Option<String>> {
        #[derive(Deserialize)]
        struct Outspend {
            spent: bool,
            txid: Option<String>,
        }
        let outspend: Outspend = request::send_json(self.http.get(self.esplora(&format!("tx/{txid}/outspend/{vout}"))?))
            .await
            .context("Esplora outspend request failed")?;
        Ok(if outspend.spent { outspend.txid } else { None })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::Network;
    use k256::ecdsa::signature::hazmat::PrehashVerifier;
    use wallet::WalletService;

    const MNEMONIC: &str =
        "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";

    fn utxo(key: KeyRef, value: u64, height: u64, n: u8) -> SpendableUtxo {
        let wallet = WalletService::from_mnemonic(MNEMONIC).unwrap();
        SpendableUtxo {
            txid: hex::encode([n; 32]),
            vout: u32::from(n),
            value,
            address: wallet.address(key).unwrap(),
            key,
            height,
        }
    }

    fn deposit(index: u32) -> KeyRef {
        KeyRef::Deposit { network: Network::Bitcoin, index }
    }

    const TREASURY: KeyRef = KeyRef::Treasury(Network::Bitcoin);
    const P2WPKH: usize = 22;

    #[test]
    fn treasury_change_is_spent_first_then_the_oldest_deposits() {
        let utxos = vec![
            utxo(deposit(2), 300_000, 900, 1),
            utxo(deposit(1), 300_000, 800, 2),
            utxo(TREASURY, 100_000, 950, 3),
        ];
        let selection = select_coins(utxos.clone(), 350_000, 10, P2WPKH, 200).unwrap();
        let order: Vec<KeyRef> = selection.inputs.iter().map(|u| u.key).collect();
        // Treasury + the oldest deposit cover it: the newer one is left.
        assert_eq!(order, vec![TREASURY, deposit(1)]);
        let selection = select_coins(utxos, 450_000, 10, P2WPKH, 200).unwrap();
        let order: Vec<KeyRef> = selection.inputs.iter().map(|u| u.key).collect();
        assert_eq!(order, vec![TREASURY, deposit(1), deposit(2)]);
        // 3 inputs, 2 outputs at 10 sat/vB.
        assert_eq!(selection.fee, estimate_vsize(3, &[P2WPKH, P2WPKH]) * 10);
        assert_eq!(selection.change, 700_000 - 450_000 - selection.fee);
    }

    #[test]
    fn uneconomic_utxos_are_skipped() {
        // At 20 sat/vB an input costs ~68 vB × 20 = 1 360 sats: anything
        // below ~4 080 is not worth spending.
        assert_eq!(input_cost(20), 68 * 20);
        let utxos = vec![utxo(deposit(1), 4_000, 1, 1), utxo(deposit(2), 50_000, 2, 2)];
        let selection = select_coins(utxos, 10_000, 20, P2WPKH, 200).unwrap();
        assert_eq!(selection.inputs.len(), 1);
        assert_eq!(selection.inputs[0].value, 50_000);
    }

    #[test]
    fn dust_change_goes_to_fees() {
        let amount = 100_000;
        let fee_no_change = estimate_vsize(1, &[P2WPKH]) * 2;
        // Exactly enough without change, 100 sats more than that.
        let selection = select_coins(vec![utxo(TREASURY, amount + fee_no_change + 100, 1, 1)], amount, 2, P2WPKH, 200).unwrap();
        assert_eq!(selection.change, 0);
        assert_eq!(selection.fee, fee_no_change + 100);
    }

    #[test]
    fn insufficient_funds_and_input_cap() {
        let err = select_coins(vec![utxo(TREASURY, 10_000, 1, 1)], 50_000, 1, P2WPKH, 200).unwrap_err();
        assert_eq!(err, CoinSelectionError::Insufficient { available: 10_000 });
        let many: Vec<SpendableUtxo> = (0..5).map(|i| utxo(deposit(u32::from(i)), 10_000, u64::from(i), i)).collect();
        assert_eq!(
            select_coins(many, 45_000, 1, P2WPKH, 3).unwrap_err(),
            CoinSelectionError::TooManyInputs(3)
        );
    }

    #[test]
    fn replacements_keep_every_input_and_pay_more() {
        let previous = select_coins(vec![utxo(TREASURY, 200_000, 1, 1)], 100_000, 5, P2WPKH, 200).unwrap();
        let replacement = select_replacement(previous.inputs.clone(), vec![], 100_000, previous.fee, 7, P2WPKH, 200).unwrap();
        assert_eq!(replacement.inputs, previous.inputs);
        let size = estimate_vsize(1, &[P2WPKH, P2WPKH]);
        assert!(replacement.fee >= previous.fee + INCREMENTAL_RELAY_SAT_VB * size);
        assert!(replacement.fee >= 7 * size);
        assert_eq!(replacement.change, 200_000 - 100_000 - replacement.fee);

        // Not enough in the original inputs: another one is added.
        let tight = select_coins(vec![utxo(TREASURY, 101_000, 1, 1)], 100_000, 1, P2WPKH, 200).unwrap();
        let bumped = select_replacement(
            tight.inputs.clone(),
            vec![utxo(deposit(4), 30_000, 5, 9), tight.inputs[0].clone()],
            100_000,
            tight.fee,
            30,
            P2WPKH,
            200,
        )
        .unwrap();
        assert_eq!(bumped.inputs.len(), 2);
        assert_eq!(bumped.inputs[0], tight.inputs[0]);
    }

    #[test]
    fn transactions_are_signed_per_input_with_rbf() {
        let wallet = WalletService::from_mnemonic(MNEMONIC).unwrap();
        let selection = select_coins(
            vec![utxo(TREASURY, 120_000, 5, 1), utxo(deposit(0), 80_000, 3, 2)],
            150_000,
            4,
            P2WPKH,
            200,
        )
        .unwrap();
        let destination = crate::destination::parse_bitcoin_address("bc1qnjg0jd8228aq7egyzacy8cys3knf9xvrerkf9g", false).unwrap();
        let change = crate::destination::parse_bitcoin_address(&wallet.treasury_address(Network::Bitcoin).unwrap(), false).unwrap();
        let signed = build_and_sign(&selection, &destination, 150_000, &change, &wallet).unwrap();

        let tx: Transaction = bitcoin::consensus::encode::deserialize_hex(&signed.hex).unwrap();
        assert_eq!(tx.compute_txid().to_string(), signed.txid);
        assert_eq!(tx.input.len(), 2);
        assert!(tx.input.iter().all(|i| i.sequence == Sequence::ENABLE_RBF_NO_LOCKTIME));
        assert_eq!(tx.output[0].value.to_sat(), 150_000);
        assert_eq!(tx.output[1].script_pubkey, change.script_pubkey());
        assert_eq!(signed.fee, 200_000 - 150_000 - tx.output[1].value.to_sat());
        // The estimate never undercounts the real size.
        assert!(signed.vsize <= estimate_vsize(2, &[P2WPKH, P2WPKH]));

        // Each witness is [DER ‖ SIGHASH_ALL, pubkey] verifying the input's
        // BIP143 sighash under that input's own key.
        let mut cache = SighashCache::new(&tx);
        for (index, input) in tx.input.iter().enumerate() {
            let elements: Vec<&[u8]> = input.witness.iter().collect();
            assert_eq!(elements.len(), 2);
            let (signature, sighash_type) = elements[0].split_at(elements[0].len() - 1);
            assert_eq!(sighash_type, &[0x01]);
            let public_key = CompressedPublicKey::from_slice(elements[1]).unwrap();
            assert_eq!(elements[1], wallet.public_key(selection.inputs[index].key).unwrap().as_slice());
            let sighash = cache
                .p2wpkh_signature_hash(
                    index,
                    &ScriptBuf::new_p2wpkh(&public_key.wpubkey_hash()),
                    Amount::from_sat(selection.inputs[index].value),
                    EcdsaSighashType::All,
                )
                .unwrap();
            k256::ecdsa::VerifyingKey::from_sec1_bytes(elements[1])
                .unwrap()
                .verify_prehash(&sighash.to_byte_array(), &k256::ecdsa::Signature::from_der(signature).unwrap())
                .unwrap();
        }
    }

    #[test]
    fn a_key_that_does_not_own_the_input_is_refused() {
        let wallet = WalletService::from_mnemonic(MNEMONIC).unwrap();
        let mut forged = utxo(deposit(0), 200_000, 1, 1);
        forged.key = deposit(1);
        let selection = CoinSelection { inputs: vec![forged], fee: 1_000, change: 0 };
        let destination = crate::destination::parse_bitcoin_address("bc1qnjg0jd8228aq7egyzacy8cys3knf9xvrerkf9g", false).unwrap();
        assert!(build_and_sign(&selection, &destination, 199_000, &destination, &wallet).is_err());
    }

    #[test]
    fn fee_rates_follow_the_target_and_bounds() {
        let estimates: std::collections::HashMap<String, f64> =
            serde_json::from_str(r#"{"1": 87.882, "2": 87.882, "3": 41.2, "6": 20.1, "144": 1.027, "1008": 0.5}"#).unwrap();
        assert_eq!(fee_rate_for_target(&estimates, 3, 1, 500), 42);
        assert_eq!(fee_rate_for_target(&estimates, 4, 1, 500), 42);
        assert_eq!(fee_rate_for_target(&estimates, 3, 1, 30), 30);
        assert_eq!(fee_rate_for_target(&std::collections::HashMap::new(), 3, 1, 500), 1);
    }

    #[test]
    fn broadcast_answers_are_classified() {
        use reqwest::StatusCode;
        assert_eq!(classify_broadcast(StatusCode::OK, "abc"), BtcBroadcastOutcome::Accepted);
        assert_eq!(
            classify_broadcast(StatusCode::BAD_REQUEST, "sendrawtransaction RPC error: {\"code\":-27,\"message\":\"Transaction already in block chain\"}"),
            BtcBroadcastOutcome::Accepted
        );
        assert_eq!(
            classify_broadcast(StatusCode::BAD_REQUEST, "{\"code\":-25,\"message\":\"bad-txns-inputs-missingorspent\"}"),
            BtcBroadcastOutcome::InputsSpent
        );
        assert!(matches!(
            classify_broadcast(StatusCode::BAD_REQUEST, "{\"code\":-26,\"message\":\"min relay fee not met\"}"),
            BtcBroadcastOutcome::Rejected(_)
        ));
        // A refused RBF replacement: its original still holds the inputs.
        assert!(matches!(
            classify_broadcast(
                StatusCode::BAD_REQUEST,
                "{\"code\":-26,\"message\":\"insufficient fee, rejecting replacement abc; new feerate 0.0001 BTC/kvB <= old feerate 0.0002 BTC/kvB\"}"
            ),
            BtcBroadcastOutcome::Rejected(_)
        ));
    }
}
