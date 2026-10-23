mod bip32;
pub mod signer;

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

pub use signer::{KeyRef, P2wpkhWitness, RecoverableSignature, Signer};

const USDT_SOLANA: &str = "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB";

#[derive(Debug, Error)]
pub enum WalletError {
    #[error("WALLET_MNEMONIC must be exactly 12 or 24 words")]
    InvalidWordCount,
    #[error("WALLET_MNEMONIC is not a valid BIP39 phrase")]
    InvalidMnemonic,
    #[error("Failed to derive address: {0}")]
    Derivation(String),
    #[error("Signing failed: {0}")]
    Signing(String),
}

/// What the derivations depend on beyond the seed: the Bitcoin network (BIP84
/// coin type `0'` and `bc` on mainnet, `1'` and `tb` on testnet4 — deposits
/// and treasury alike) and the USDT mint whose token accounts are the Solana
/// deposit addresses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalletChains {
    pub bitcoin_testnet: bool,
    pub solana_usdt_mint: String,
}

impl Default for WalletChains {
    fn default() -> Self {
        Self {
            bitcoin_testnet: false,
            solana_usdt_mint: USDT_SOLANA.into(),
        }
    }
}

/// Keeps only the BIP39 seed (never the phrase), computed once at startup.
/// Clones share it; it is wiped when the last clone drops.
#[derive(Clone)]
pub struct WalletService {
    seed: Arc<Zeroizing<[u8; 64]>>,
    chains: Arc<WalletChains>,
}

impl fmt::Debug for WalletService {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("WalletService { seed: [REDACTED] }")
    }
}

impl WalletService {
    /// Mainnet derivations.
    pub fn from_mnemonic(mnemonic: &str) -> Result<Self, WalletError> {
        Self::with_chains(mnemonic, WalletChains::default())
    }

    pub fn with_chains(mnemonic: &str, chains: WalletChains) -> Result<Self, WalletError> {
        // `Mnemonic` wipes its words on drop (bip39 `zeroize` feature).
        let mut seed = parse_mnemonic(mnemonic)?.to_seed("");
        let wallet = Self {
            seed: Arc::new(Zeroizing::new(seed)),
            chains: Arc::new(chains),
        };
        seed.zeroize();
        Ok(wallet)
    }

    /// The account a deposit key's tokens sit on: the address itself,
    /// except on Solana where it is the owner's USDT associated token
    /// account (what Solana deposit rows from before spec §4.4 store as
    /// their address).
    pub fn derive_address(&self, network: Network, index: u32) -> Result<String, WalletError> {
        self.address(KeyRef::Deposit { network, index })
    }

    /// The address a payer is shown for deposit key `index` (spec §4.4):
    /// on Solana the owner's wallet address (`m/44'/501'/{index}'/0'`) —
    /// the payer's wallet creates the token account (`derive_address`) as
    /// it sends —, elsewhere `derive_address`.
    pub fn payment_address(&self, network: Network, index: u32) -> Result<String, WalletError> {
        match network {
            Network::Solana => {
                Ok(bs58::encode(self.solana_owner(KeyRef::Deposit { network, index })?).into_string())
            }
            _ => self.derive_address(network, index),
        }
    }

    /// The treasury address of `network` (ADR 0002) — for Solana, the owner
    /// (fee payer); its USDT sits on `solana_usdt_token_account`.
    pub fn treasury_address(&self, network: Network) -> Result<String, WalletError> {
        self.address(KeyRef::Treasury(network))
    }

    /// The USDT associated token account of a Solana key (the deposit
    /// address itself for a deposit key).
    pub fn solana_usdt_token_account(&self, key: KeyRef) -> Result<String, WalletError> {
        let owner = Pubkey::new_from_array(self.solana_owner(key)?);
        let mint: Pubkey = self
            .chains
            .solana_usdt_mint
            .parse()
            .map_err(|e: solana_sdk::pubkey::ParsePubkeyError| {
                WalletError::Derivation(e.to_string())
            })?;
        Ok(get_associated_token_address(&owner, &mint).to_string())
    }

    fn bitcoin_coin_type(&self) -> u32 {
        if self.chains.bitcoin_testnet {
            1
        } else {
            0
        }
    }

    /// Derivation path of a secp256k1 key (Ethereum, Tron, Bitcoin).
    /// Deposits: `m/44'/60'/0'/0/{i}` (ETH/Tron share keys) and BIP84
    /// `m/84'/{coin}'/0'/0/{i}`. Treasuries (ADR 0002): account `1'`, which
    /// no deposit index can reach.
    fn secp256k1_path(&self, key: KeyRef) -> Result<String, WalletError> {
        let coin = self.bitcoin_coin_type();
        match key {
            KeyRef::Deposit {
                network: Network::Ethereum | Network::Tron,
                index,
            } => Ok(format!("m/44'/60'/0'/0/{index}")),
            KeyRef::Deposit {
                network: Network::Bitcoin,
                index,
            } => Ok(format!("m/84'/{coin}'/0'/0/{index}")),
            KeyRef::Treasury(Network::Ethereum | Network::Tron) => Ok("m/44'/60'/1'/0/0".into()),
            KeyRef::Treasury(Network::Bitcoin) => Ok(format!("m/84'/{coin}'/1'/0/0")),
            other => Err(WalletError::Derivation(format!("{other:?} is not a secp256k1 key"))),
        }
    }

    /// Solana owners: deposits `m/44'/501'/{i}'/0'`, treasury
    /// `m/44'/501'/0'/1'` (the `1'` change branch deposits never use).
    fn ed25519_path(key: KeyRef) -> Result<String, WalletError> {
        match key {
            KeyRef::Deposit {
                network: Network::Solana,
                index,
            } => Ok(format!("m/44'/501'/{index}'/0'")),
            KeyRef::Treasury(Network::Solana) => Ok("m/44'/501'/0'/1'".into()),
            other => Err(WalletError::Derivation(format!("{other:?} is not an ed25519 key"))),
        }
    }

    fn secp256k1_key(&self, key: KeyRef) -> Result<EthSigningKey, WalletError> {
        self.derive_secp256k1_key(&self.secp256k1_path(key)?)
    }

    fn ed25519_key(&self, key: KeyRef) -> Result<SigningKey, WalletError> {
        let derived = derive_ed25519_slip10(self.seed.as_slice(), &Self::ed25519_path(key)?)?;
        Ok(SigningKey::from_bytes(&derived))
    }

    fn solana_owner(&self, key: KeyRef) -> Result<[u8; 32], WalletError> {
        Ok(self.ed25519_key(key)?.verifying_key().to_bytes())
    }

    /// Generic BIP32 secp256k1 derivation (shared by ETH/Tron and Bitcoin).
    fn derive_secp256k1_key(&self, path: &str) -> Result<EthSigningKey, WalletError> {
        let path = bip32::parse_path(path)?;
        let xprv = bip32::ExtendedPrivateKey::derive(self.seed.as_slice(), &path)?;
        Ok(EthSigningKey::from(xprv.secret_key()))
    }
}

fn lightning_has_no_key() -> WalletError {
    WalletError::Derivation("lightning has no derived address".into())
}

impl Signer for WalletService {
    fn address(&self, key: KeyRef) -> Result<String, WalletError> {
        let network = match key {
            KeyRef::Deposit { network, .. } | KeyRef::Treasury(network) => network,
        };
        match network {
            Network::Ethereum => Ok(eth_address_checksummed(&self.secp256k1_key(key)?)),
            Network::Tron => Ok(tron_address_from_key(&self.secp256k1_key(key)?)),
            Network::Bitcoin => p2wpkh_address(&self.secp256k1_key(key)?, self.chains.bitcoin_testnet),
            Network::Solana => match key {
                // Deposit addresses are USDT token accounts.
                KeyRef::Deposit { .. } => self.solana_usdt_token_account(key),
                KeyRef::Treasury(_) => Ok(bs58::encode(self.solana_owner(key)?).into_string()),
            },
            Network::Lightning => Err(lightning_has_no_key()),
        }
    }

    fn public_key(&self, key: KeyRef) -> Result<Vec<u8>, WalletError> {
        match key {
            KeyRef::Deposit {
                network: Network::Solana,
                ..
            }
            | KeyRef::Treasury(Network::Solana) => Ok(self.solana_owner(key)?.to_vec()),
            KeyRef::Deposit {
                network: Network::Lightning,
                ..
            }
            | KeyRef::Treasury(Network::Lightning) => Err(lightning_has_no_key()),
            _ => Ok(self
                .secp256k1_key(key)?
                .verifying_key()
                .to_encoded_point(true)
                .as_bytes()
                .to_vec()),
        }
    }

    fn sign_secp256k1_recoverable(
        &self,
        key: KeyRef,
        digest: [u8; 32],
    ) -> Result<RecoverableSignature, WalletError> {
        signer::sign_recoverable(&self.secp256k1_key(key)?, &digest)
    }

    fn sign_p2wpkh(&self, key: KeyRef, sighash: [u8; 32]) -> Result<P2wpkhWitness, WalletError> {
        signer::sign_p2wpkh_with(&self.secp256k1_key(key)?, &sighash)
    }

    fn sign_ed25519(&self, key: KeyRef, message: &[u8]) -> Result<[u8; 64], WalletError> {
        Ok(signer::sign_ed25519_with(&self.ed25519_key(key)?, message))
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

/// `bech32(hrp="bc", witness v0, hash160(compressed pubkey))` (`tb` on
/// testnet).
fn p2wpkh_address(sk: &EthSigningKey, testnet: bool) -> Result<String, WalletError> {
    let compressed = sk.verifying_key().to_encoded_point(true);
    let hash160 = Ripemd160::digest(Sha256::digest(compressed.as_bytes()));
    let hrp = if testnet { bech32::hrp::TB } else { bech32::hrp::BC };
    bech32::segwit::encode(hrp, bech32::segwit::VERSION_0, &hash160)
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
        // A payer is shown the owner (its wallet creates the token account).
        assert_eq!(
            wallet.payment_address(Network::Solana, 0).unwrap(),
            "HAgk14JpMQLgt6rVgv7cBQFJWFto5Dqxi472uT3DKpqk"
        );
        assert_eq!(
            wallet.payment_address(Network::Solana, 1).unwrap(),
            "Hh8QwFUA6MtVu1qAoq12ucvFHNwCcVTV7hpWjeY1Hztb"
        );
        // Every other network is shown the derived address.
        for network in [Network::Ethereum, Network::Tron, Network::Bitcoin] {
            assert_eq!(wallet.payment_address(network, 3).unwrap(), wallet.derive_address(network, 3).unwrap());
        }
        assert!(wallet.payment_address(Network::Lightning, 0).is_err());
    }

    /// Treasury addresses of the test mnemonic (ADR 0002). Changing any of
    /// these strands every treasury already funded on that path.
    #[test]
    fn derives_treasury_golden() {
        let wallet = WalletService::from_mnemonic(TEST_MNEMONIC).unwrap();
        assert_eq!(
            wallet.treasury_address(Network::Ethereum).unwrap(),
            "0x78839F6054d7ed13918bAe0473BA31b1Ca9D7265"
        );
        // Same key as the Ethereum treasury, Tron encoding.
        assert_eq!(
            wallet.treasury_address(Network::Tron).unwrap(),
            "TLxRmNgk8aegM7DSs1JELYxG2REizYZnnX"
        );
        assert_eq!(
            wallet.public_key(KeyRef::Treasury(Network::Tron)).unwrap(),
            wallet.public_key(KeyRef::Treasury(Network::Ethereum)).unwrap()
        );
        assert_eq!(
            wallet.treasury_address(Network::Bitcoin).unwrap(),
            "bc1qku0qh0mc00y8tk0n65x2tqw4trlspak0fnjmfz"
        );
        assert_eq!(
            wallet.treasury_address(Network::Solana).unwrap(),
            "GKreMsHvt8A79VApjboYDq3J4ZCXSJRYYQk9BscMbi1H"
        );
        assert_eq!(
            wallet
                .solana_usdt_token_account(KeyRef::Treasury(Network::Solana))
                .unwrap(),
            "5MhgGS7CwMTdc9F4VBEMLQFD9ErfsaqMyRdEzSTCRxDK"
        );
        assert!(wallet.treasury_address(Network::Lightning).is_err());
    }

    /// The secp256k1 treasury keys, re-derived with rust-bitcoin's BIP32
    /// (an implementation independent from `bip32.rs`).
    #[test]
    fn treasury_keys_match_an_independent_bip32() {
        use bitcoin::bip32::{DerivationPath, Xpriv};
        use bitcoin::secp256k1::Secp256k1;
        use std::str::FromStr;

        let seed = Mnemonic::parse_in_normalized(Language::English, TEST_MNEMONIC)
            .unwrap()
            .to_seed("");
        let secp = Secp256k1::new();
        let master = Xpriv::new_master(bitcoin::NetworkKind::Main, &seed).unwrap();
        let wallet = WalletService::from_mnemonic(TEST_MNEMONIC).unwrap();
        for (path, key) in [
            ("m/44'/60'/1'/0/0", KeyRef::Treasury(Network::Ethereum)),
            ("m/84'/0'/1'/0/0", KeyRef::Treasury(Network::Bitcoin)),
            ("m/44'/60'/0'/0/7", KeyRef::Deposit { network: Network::Tron, index: 7 }),
            ("m/84'/0'/0'/0/3", KeyRef::Deposit { network: Network::Bitcoin, index: 3 }),
        ] {
            let derived = master
                .derive_priv(&secp, &DerivationPath::from_str(path).unwrap())
                .unwrap();
            let expected = derived.private_key.public_key(&secp).serialize();
            assert_eq!(wallet.public_key(key).unwrap(), expected.to_vec(), "{path}");
        }
    }

    #[test]
    fn testnet_bitcoin_uses_coin_type_one() {
        let wallet = WalletService::with_chains(
            TEST_MNEMONIC,
            WalletChains {
                bitcoin_testnet: true,
                ..WalletChains::default()
            },
        )
        .unwrap();
        // BIP84 reference vector: m/84'/1'/0'/0/0 of the test mnemonic.
        assert_eq!(
            wallet.derive_address(Network::Bitcoin, 0).unwrap(),
            "tb1q6rz28mcfaxtmd6v789l9rrlrusdprr9pqcpvkl"
        );
        assert_eq!(
            wallet.treasury_address(Network::Bitcoin).unwrap(),
            "tb1qp7shgcwx3mpzgxjvff0d77vuhchcldzfy60x6s"
        );
        // Account-based networks do not depend on the Bitcoin network.
        assert_eq!(
            wallet.derive_address(Network::Ethereum, 0).unwrap(),
            "0x9858EfFD232B4033E47d90003D41EC34EcaEda94"
        );
    }

    #[test]
    fn no_deposit_index_reaches_a_treasury() {
        let wallet = WalletService::from_mnemonic(TEST_MNEMONIC).unwrap();
        for network in [Network::Ethereum, Network::Tron, Network::Bitcoin, Network::Solana] {
            let treasury = wallet.treasury_address(network).unwrap();
            let treasury_key = wallet.public_key(KeyRef::Treasury(network)).unwrap();
            for index in 0..64 {
                let key = KeyRef::Deposit { network, index };
                assert_ne!(wallet.address(key).unwrap(), treasury, "{network:?} #{index}");
                assert_ne!(wallet.public_key(key).unwrap(), treasury_key, "{network:?} #{index}");
            }
        }
    }

    #[test]
    fn signatures_come_from_the_addressed_key() {
        use k256::ecdsa::{RecoveryId, Signature, VerifyingKey};
        let wallet = WalletService::from_mnemonic(TEST_MNEMONIC).unwrap();

        // Tron treasury: sha256 digest, recover the address from the
        // signature.
        let key = KeyRef::Treasury(Network::Tron);
        let digest: [u8; 32] = Sha256::digest(b"tron raw_data").into();
        let signature = wallet.sign_secp256k1_recoverable(key, digest).unwrap();
        let mut rs = [0u8; 64];
        rs[..32].copy_from_slice(&signature.r);
        rs[32..].copy_from_slice(&signature.s);
        let recovered = VerifyingKey::recover_from_prehash(
            &digest,
            &Signature::from_slice(&rs).unwrap(),
            RecoveryId::from_byte(signature.recovery_id).unwrap(),
        )
        .unwrap();
        let point = recovered.to_encoded_point(false);
        let hash = Keccak256::digest(&point.as_bytes()[1..]);
        let mut payload = vec![0x41];
        payload.extend_from_slice(&hash[12..]);
        let checksum = Sha256::digest(Sha256::digest(&payload));
        payload.extend_from_slice(&checksum[..4]);
        assert_eq!(bs58::encode(payload).into_string(), wallet.address(key).unwrap());

        // Bitcoin: the witness public key is the address's key.
        let witness = wallet
            .sign_p2wpkh(KeyRef::Deposit { network: Network::Bitcoin, index: 0 }, [7; 32])
            .unwrap();
        assert_eq!(
            witness.public_key.to_vec(),
            wallet
                .public_key(KeyRef::Deposit { network: Network::Bitcoin, index: 0 })
                .unwrap()
        );

        // Solana: verifies under the owner key.
        use ed25519_dalek::Verifier;
        let key = KeyRef::Deposit { network: Network::Solana, index: 1 };
        let signature = wallet.sign_ed25519(key, b"message").unwrap();
        let owner: [u8; 32] = wallet.public_key(key).unwrap().try_into().unwrap();
        ed25519_dalek::VerifyingKey::from_bytes(&owner)
            .unwrap()
            .verify(b"message", &ed25519_dalek::Signature::from_bytes(&signature))
            .unwrap();
        assert_eq!(
            bs58::encode(owner).into_string(),
            "Hh8QwFUA6MtVu1qAoq12ucvFHNwCcVTV7hpWjeY1Hztb"
        );

        // Lightning has no key.
        assert!(wallet
            .sign_ed25519(KeyRef::Treasury(Network::Lightning), b"x")
            .is_err());
    }
}

