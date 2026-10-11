mod bip32;

use std::fmt;
use std::sync::Arc;

use bip39::{Language, Mnemonic};
use domain::Network;
use ed25519_dalek::SigningKey;
use k256::ecdsa::SigningKey as EthSigningKey;
use ripemd::Ripemd160;
use sha2::{Digest, Sha256};
use sha3::Keccak256;
use solana_sdk::pubkey::Pubkey;
use spl_associated_token_account::get_associated_token_address;
use thiserror::Error;
use zeroize::{Zeroize, Zeroizing};

const USDT_SOLANA: &str = "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB";

#[derive(Debug, Error)]
pub enum WalletError {
    #[error("WALLET_MNEMONIC must be exactly 12 or 24 words")]
    InvalidWordCount,
    #[error("WALLET_MNEMONIC is not a valid BIP39 phrase")]
    InvalidMnemonic,
    #[error("Failed to derive address: {0}")]
    Derivation(String),
}

/// Keeps only the BIP39 seed (never the phrase), computed once at startup.
/// Clones share it; it is wiped when the last clone drops.
#[derive(Clone)]
pub struct WalletService {
    seed: Arc<Zeroizing<[u8; 64]>>,
}

impl fmt::Debug for WalletService {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("WalletService { seed: [REDACTED] }")
    }
}

impl WalletService {
    pub fn from_mnemonic(mnemonic: &str) -> Result<Self, WalletError> {
        // `Mnemonic` wipes its words on drop (bip39 `zeroize` feature).
        let mut seed = parse_mnemonic(mnemonic)?.to_seed("");
        let wallet = Self {
            seed: Arc::new(Zeroizing::new(seed)),
        };
        seed.zeroize();
        Ok(wallet)
    }

    pub fn derive_address(&self, network: Network, index: u32) -> Result<String, WalletError> {
        match network {
            Network::Ethereum => self.derive_ethereum(index),
            Network::Tron => self.derive_tron(index),
            Network::Solana => self.derive_solana_ata(index),
            Network::Bitcoin => self.derive_bitcoin(index),
            Network::Lightning => Err(WalletError::Derivation(
                "lightning has no derived address".into(),
            )),
        }
    }

    /// BIP84 native SegWit: P2WPKH bech32 (`bc1q…`) at `m/84'/0'/0'/0/{index}`.
    fn derive_bitcoin(&self, index: u32) -> Result<String, WalletError> {
        let sk = self.derive_secp256k1_key(&format!("m/84'/0'/0'/0/{index}"))?;
        p2wpkh_address(&sk)
    }

    fn derive_ethereum(&self, index: u32) -> Result<String, WalletError> {
        let sk = self.derive_eth_signing_key(index)?;
        Ok(eth_address_checksummed(&sk))
    }

    fn derive_tron(&self, index: u32) -> Result<String, WalletError> {
        let sk = self.derive_eth_signing_key(index)?;
        Ok(tron_address_from_key(&sk))
    }

    fn derive_solana_ata(&self, index: u32) -> Result<String, WalletError> {
        let path = format!("m/44'/501'/{index}'/0'");
        let derived = derive_ed25519_slip10(self.seed.as_slice(), &path)?;
        let signing_key = SigningKey::from_bytes(&derived);
        let pubkey = Pubkey::new_from_array(signing_key.verifying_key().to_bytes());
        let mint: Pubkey = USDT_SOLANA
            .parse()
            .map_err(|e: solana_sdk::pubkey::ParsePubkeyError| {
                WalletError::Derivation(e.to_string())
            })?;
        let ata = get_associated_token_address(&pubkey, &mint);
        Ok(ata.to_string())
    }

    fn derive_eth_signing_key(&self, index: u32) -> Result<EthSigningKey, WalletError> {
        self.derive_secp256k1_key(&format!("m/44'/60'/0'/0/{index}"))
    }

    /// Generic BIP32 secp256k1 derivation (shared by ETH/Tron and Bitcoin).
    fn derive_secp256k1_key(&self, path: &str) -> Result<EthSigningKey, WalletError> {
        let path = bip32::parse_path(path)?;
        let xprv = bip32::ExtendedPrivateKey::derive(self.seed.as_slice(), &path)?;
        Ok(EthSigningKey::from(xprv.secret_key()))
    }
}

pub fn validate_mnemonic(mnemonic: &str) -> Result<(), WalletError> {
    parse_mnemonic(mnemonic).map(|_| ())
}

fn parse_mnemonic(mnemonic: &str) -> Result<Mnemonic, WalletError> {
    let normalized = mnemonic.trim();
    let words = normalized.split_whitespace().count();
    if words != 12 && words != 24 {
        return Err(WalletError::InvalidWordCount);
    }
    Mnemonic::parse_in_normalized(Language::English, normalized)
        .map_err(|_| WalletError::InvalidMnemonic)
}

fn eth_address_checksummed(sk: &EthSigningKey) -> String {
    let public = sk.verifying_key();
    let uncompressed = public.to_encoded_point(false);
    let bytes = uncompressed.as_bytes();
    let hash = Keccak256::digest(&bytes[1..]);
    let addr = hex::encode(&hash[12..]);
    to_checksum_address(&addr)
}

fn to_checksum_address(addr_hex_lower: &str) -> String {
    let hash = Keccak256::digest(addr_hex_lower.as_bytes());
    let mut out = String::from("0x");
    for (i, c) in addr_hex_lower.chars().enumerate() {
        let byte = hash[i / 2];
        let nibble = if i % 2 == 0 { byte >> 4 } else { byte & 0x0f };
        if c.is_ascii_alphabetic() && nibble >= 8 {
            out.push(c.to_ascii_uppercase());
        } else {
            out.push(c);
        }
    }
    out
}

fn tron_address_from_key(sk: &EthSigningKey) -> String {
    let public = sk.verifying_key();
    let uncompressed = public.to_encoded_point(false);
    let bytes = uncompressed.as_bytes();
    let hash = Keccak256::digest(&bytes[1..]);
    let mut payload = Vec::with_capacity(21);
    payload.push(0x41);
    payload.extend_from_slice(&hash[12..]);
    let checksum = Sha256::digest(Sha256::digest(&payload));
    payload.extend_from_slice(&checksum[..4]);
    bs58::encode(payload).into_string()
}

/// `bech32(hrp="bc", witness v0, hash160(compressed pubkey))`.
fn p2wpkh_address(sk: &EthSigningKey) -> Result<String, WalletError> {
    let compressed = sk.verifying_key().to_encoded_point(true);
    let hash160 = Ripemd160::digest(Sha256::digest(compressed.as_bytes()));
    bech32::segwit::encode(bech32::hrp::BC, bech32::segwit::VERSION_0, &hash160)
        .map_err(|e| WalletError::Derivation(e.to_string()))
}

/// SLIP-0010 ed25519 derivation matching `ed25519-hd-key` (hardened only):
/// `I = HMAC-SHA512(key = "ed25519 seed", seed)`, then per index
/// `I = HMAC-SHA512(IR, 0x00 || IL || ser32(i))`; the key is the final `IL`.
fn derive_ed25519_slip10(seed: &[u8], path: &str) -> Result<Zeroizing<[u8; 32]>, WalletError> {
    let mut i = bip32::hmac_sha512(b"ed25519 seed", &[seed])?;
    for child in bip32::parse_path(path)? {
        if !child.is_hardened() {
            return Err(WalletError::Derivation(
                "ed25519 slip10 only supports hardened derivation".into(),
            ));
        }
        let (key, chain) = i.split_at(32);
        i = bip32::hmac_sha512(chain, &[&[0u8], key, &child.to_be_bytes()])?;
    }

    let mut key = Zeroizing::new([0u8; 32]);
    key.copy_from_slice(&i[..32]);
    Ok(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_MNEMONIC: &str =
        "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";

    #[test]
    fn validates_word_count() {
        assert!(matches!(
            validate_mnemonic("short phrase"),
            Err(WalletError::InvalidWordCount)
        ));
    }

    #[test]
    fn debug_never_prints_the_seed() {
        let wallet = WalletService::from_mnemonic(TEST_MNEMONIC).unwrap();
        let clone = wallet.clone();
        assert!(
            Arc::ptr_eq(&wallet.seed, &clone.seed),
            "clones share one seed"
        );
        let shown = format!("{wallet:?}");
        assert_eq!(shown, "WalletService { seed: [REDACTED] }");
        assert!(!shown.contains(&hex::encode(&wallet.seed[..8])));
    }

    #[test]
    fn derives_ethereum_golden() {
        let wallet = WalletService::from_mnemonic(TEST_MNEMONIC).unwrap();
        assert_eq!(
            wallet.derive_address(Network::Ethereum, 0).unwrap(),
            "0x9858EfFD232B4033E47d90003D41EC34EcaEda94"
        );
        assert_eq!(
            wallet.derive_address(Network::Ethereum, 1).unwrap(),
            "0x6Fac4D18c912343BF86fa7049364Dd4E424Ab9C0"
        );
    }

    #[test]
    fn derives_tron_golden() {
        let wallet = WalletService::from_mnemonic(TEST_MNEMONIC).unwrap();
        assert_eq!(
            wallet.derive_address(Network::Tron, 0).unwrap(),
            "TPrkFhZ8LH8Mruco8vXyA496TaeFBrbmeU"
        );
        assert_eq!(
            wallet.derive_address(Network::Tron, 1).unwrap(),
            "TL9gR3f4FgNa9VZAsPt6xuKXPrfWAUJ5MW"
        );
    }

    #[test]
    fn derives_bitcoin_bip84_golden() {
        // BIP84 reference vectors for the standard test mnemonic.
        let wallet = WalletService::from_mnemonic(TEST_MNEMONIC).unwrap();
        assert_eq!(
            wallet.derive_address(Network::Bitcoin, 0).unwrap(),
            "bc1qcr8te4kr609gcawutmrza0j4xv80jy8z306fyu"
        );
        assert_eq!(
            wallet.derive_address(Network::Bitcoin, 1).unwrap(),
            "bc1qnjg0jd8228aq7egyzacy8cys3knf9xvrerkf9g"
        );
    }

    #[test]
    fn lightning_has_no_derived_address() {
        let wallet = WalletService::from_mnemonic(TEST_MNEMONIC).unwrap();
        match wallet.derive_address(Network::Lightning, 0) {
            Err(WalletError::Derivation(msg)) => {
                assert_eq!(msg, "lightning has no derived address")
            }
            other => panic!("expected a derivation error, got {other:?}"),
        }
    }

    #[test]
    fn derives_solana_deterministic() {
        let wallet = WalletService::from_mnemonic(TEST_MNEMONIC).unwrap();
        let a0 = wallet.derive_address(Network::Solana, 0).unwrap();
        let a1 = wallet.derive_address(Network::Solana, 1).unwrap();
        assert_ne!(a0, a1);
        assert!(a0.len() > 30);
        assert_eq!(wallet.derive_address(Network::Solana, 0).unwrap(), a0);
    }

    /// Owner keys at `m/44'/501'/{i}'/0'` (the path Phantom/Solflare use) for
    /// the BIP39 test mnemonic, cross-checked against independent sources:
    /// - index 0 `HAgk14Jp…` (pubkey `f0362762…`): python-hdwallet README
    ///   (github.com/hdwallet-io/python-hdwallet, commit 7803b4a0, "Solana
    ///   (SOL) wallet" output for this mnemonic and path) and the txtx test
    ///   `test_mnemonic_to_solana_keypair` (github.com/solana-foundation/txtx,
    ///   addons/svm/core/src/signers/secret_key.rs, commit 972a7b68);
    /// - index 1 `Hh8QwFUA…` (pubkey `f8029acf…`): Keystone firmware test
    ///   (github.com/KeystoneHQ/keystone3-firmware,
    ///   rust/apps/solana/src/address.rs, commit 8a08e232, `m/44'/501'/1'/0'`).
    ///
    /// Both owners and their USDT ATAs were also recomputed with an
    /// independent pure-Python implementation (PBKDF2 seed, SLIP-0010,
    /// RFC 8032 ed25519, `find_program_address` with the SPL Token and ATA
    /// program ids). Changing any value here changes issued deposit addresses.
    #[test]
    fn derives_solana_golden() {
        let seed = Mnemonic::parse_in_normalized(Language::English, TEST_MNEMONIC)
            .unwrap()
            .to_seed("");
        for (index, owner) in [
            (0, "HAgk14JpMQLgt6rVgv7cBQFJWFto5Dqxi472uT3DKpqk"),
            (1, "Hh8QwFUA6MtVu1qAoq12ucvFHNwCcVTV7hpWjeY1Hztb"),
        ] {
            let derived = derive_ed25519_slip10(&seed, &format!("m/44'/501'/{index}'/0'")).unwrap();
            let pubkey = SigningKey::from_bytes(&derived).verifying_key().to_bytes();
            assert_eq!(bs58::encode(pubkey).into_string(), owner, "owner {index}");
        }

        let wallet = WalletService::from_mnemonic(TEST_MNEMONIC).unwrap();
        assert_eq!(
            wallet.derive_address(Network::Solana, 0).unwrap(),
            "6pXYguUixhfdydHDMgsvSy3sAWjStL2eEYh4KeYJAUDk"
        );
        assert_eq!(
            wallet.derive_address(Network::Solana, 1).unwrap(),
            "BZnBhSRppxtYwkMzjn6brVCEVqPgx7zeRhGuJaPmuv2V"
        );
    }
}
