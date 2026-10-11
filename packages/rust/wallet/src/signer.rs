//! The signing interface (ADR 0003): the only code in the workspace that
//! ever touches a private key. Transaction building lives in `chain`,
//! orchestration in `jobs`; both only see addresses, public keys and
//! signatures.
//!
//! Keys are derived from the seed on every call and wiped on drop
//! (`k256::SecretKey` and `ed25519_dalek::SigningKey` zeroize themselves,
//! the intermediate seed bytes are `Zeroizing`).

use domain::Network;
use ed25519_dalek::{Signer as _, SigningKey as Ed25519SigningKey};
use k256::ecdsa::SigningKey as Secp256k1SigningKey;

use crate::WalletError;

/// Which key signs: a deposit address's or a network's treasury.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KeyRef {
    Deposit { network: Network, index: u32 },
    Treasury(Network),
}

/// secp256k1 signature with its recovery id (0 or 1), low-S normalized.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecoverableSignature {
    pub r: [u8; 32],
    pub s: [u8; 32],
    pub recovery_id: u8,
}

impl RecoverableSignature {
    /// `r ‖ s ‖ v` with `v = recovery_id + 27` — the 65-byte form Tron
    /// transactions carry (TronWeb's encoding; java-tron also accepts 0/1).
    pub fn to_tron_bytes(&self) -> [u8; 65] {
        let mut out = [0u8; 65];
        out[..32].copy_from_slice(&self.r);
        out[32..64].copy_from_slice(&self.s);
        out[64] = self.recovery_id + 27;
        out
    }

    /// EIP-1559 `y_parity`.
    pub fn y_parity(&self) -> bool {
        self.recovery_id == 1
    }
}

/// What a P2WPKH input's witness needs: the DER signature (without the
/// sighash-type byte) and the compressed public key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct P2wpkhWitness {
    pub signature_der: Vec<u8>,
    pub public_key: [u8; 33],
}

/// Signs on behalf of the gateway. Implemented in-process by
/// `WalletService`; an external signer or KMS would be another
/// implementation (ADR 0003).
pub trait Signer: Send + Sync {
    /// The on-chain address of `key` — for a Solana deposit, its USDT token
    /// account (the deposit address), for the Solana treasury, its owner.
    fn address(&self, key: KeyRef) -> Result<String, WalletError>;

    /// Raw public key: 33-byte compressed secp256k1 (Ethereum, Tron,
    /// Bitcoin) or the 32-byte ed25519 owner key (Solana).
    fn public_key(&self, key: KeyRef) -> Result<Vec<u8>, WalletError>;

    /// secp256k1 over a 32-byte digest, low-S, with recovery id
    /// (Ethereum: keccak-256 of the signing payload; Tron: sha256 of
    /// `raw_data`).
    fn sign_secp256k1_recoverable(
        &self,
        key: KeyRef,
        digest: [u8; 32],
    ) -> Result<RecoverableSignature, WalletError>;

    /// BIP143 sighash → DER signature + compressed public key.
    fn sign_p2wpkh(&self, key: KeyRef, sighash: [u8; 32]) -> Result<P2wpkhWitness, WalletError>;

    /// ed25519 over the Solana message bytes (ed25519-dalek 2 — never
    /// `solana_sdk::Keypair`, see `deny.toml`).
    fn sign_ed25519(&self, key: KeyRef, message: &[u8]) -> Result<[u8; 64], WalletError>;
}

/// RFC 6979 deterministic, low-S (k256 normalizes and adjusts the recovery
/// id accordingly).
pub(crate) fn sign_recoverable(
    key: &Secp256k1SigningKey,
    digest: &[u8; 32],
) -> Result<RecoverableSignature, WalletError> {
    let (signature, recovery_id) = key
        .sign_prehash_recoverable(digest)
        .map_err(|e| WalletError::Signing(e.to_string()))?;
    let bytes = signature.to_bytes();
    let mut r = [0u8; 32];
    let mut s = [0u8; 32];
    r.copy_from_slice(&bytes[..32]);
    s.copy_from_slice(&bytes[32..]);
    Ok(RecoverableSignature {
        r,
        s,
        recovery_id: recovery_id.to_byte(),
    })
}

pub(crate) fn sign_p2wpkh_with(
    key: &Secp256k1SigningKey,
    sighash: &[u8; 32],
) -> Result<P2wpkhWitness, WalletError> {
    use k256::ecdsa::signature::hazmat::PrehashSigner;
    let signature: k256::ecdsa::Signature = key
        .sign_prehash(sighash)
        .map_err(|e| WalletError::Signing(e.to_string()))?;
    // Standardness (BIP62/BIP146) requires low-S; k256 already produces it.
    let signature = signature.normalize_s().unwrap_or(signature);
    let mut public_key = [0u8; 33];
    public_key.copy_from_slice(key.verifying_key().to_encoded_point(true).as_bytes());
    Ok(P2wpkhWitness {
        signature_der: signature.to_der().as_bytes().to_vec(),
        public_key,
    })
}

pub(crate) fn sign_ed25519_with(key: &Ed25519SigningKey, message: &[u8]) -> [u8; 64] {
    key.sign(message).to_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use k256::ecdsa::{RecoveryId, Signature, VerifyingKey};

    fn secp_key(hex_key: &str) -> Secp256k1SigningKey {
        Secp256k1SigningKey::from_slice(&hex::decode(hex_key).unwrap()).unwrap()
    }

    #[test]
    fn ed25519_matches_rfc8032_test_1() {
        // RFC 8032 §7.1, TEST 1 (empty message).
        let secret: [u8; 32] = hex::decode(
            "9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60",
        )
        .unwrap()
        .try_into()
        .unwrap();
        let key = Ed25519SigningKey::from_bytes(&secret);
        assert_eq!(
            hex::encode(key.verifying_key().to_bytes()),
            "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a"
        );
        assert_eq!(
            hex::encode(sign_ed25519_with(&key, b"")),
            "e5564300c360ac729086e2cc806e828a84877f1eb8e5d974d873e065224901555fb8821590a33bacc61e39701cf9b46bd25bf5f0595bbe24655141438e7a100b"
        );
    }

    #[test]
    fn p2wpkh_signature_matches_the_bip143_native_example() {
        // BIP143 "Native P2WPKH": second input, sighash c37af311…, signed
        // with RFC 6979 — the BIP's signed transaction carries exactly this
        // DER signature (followed by the SIGHASH_ALL byte 01).
        let key = secp_key("619c335025c7f4012e556c2a58b2506e30b8511b53ade95ea316fd8c3286feb9");
        let sighash: [u8; 32] =
            hex::decode("c37af31116d1b27caf68aae9e3ac82f1477929014d5b917657d0eb49478cb670")
                .unwrap()
                .try_into()
                .unwrap();
        let witness = sign_p2wpkh_with(&key, &sighash).unwrap();
        assert_eq!(
            hex::encode(&witness.signature_der),
            "304402203609e17b84f6a7d30c80bfa610b5b4542f32a8a0d5447a12fb1366d7f01cc44a0220573a954c4518331561406f90300e8f3358f51928d43c212a8caed02de67eebee"
        );
        assert_eq!(
            hex::encode(witness.public_key),
            "025476c2e83188368da1ff3e292e7acafcdb3566bb0ad253f62fc70f07aeee6357"
        );
    }

    #[test]
    fn recoverable_signatures_are_low_s_and_recover_the_signer() {
        let key = secp_key("4646464646464646464646464646464646464646464646464646464646464646");
        for seed in 0u8..32 {
            let digest = [seed; 32];
            let signature = sign_recoverable(&key, &digest).unwrap();
            let mut bytes = [0u8; 64];
            bytes[..32].copy_from_slice(&signature.r);
            bytes[32..].copy_from_slice(&signature.s);
            let parsed = Signature::from_slice(&bytes).unwrap();
            assert!(parsed.normalize_s().is_none(), "high S for digest {seed}");
            let recovered = VerifyingKey::recover_from_prehash(
                &digest,
                &parsed,
                RecoveryId::from_byte(signature.recovery_id).unwrap(),
            )
            .unwrap();
            assert_eq!(&recovered, key.verifying_key());
        }
    }

    #[test]
    fn tron_signature_bytes_carry_v_27_or_28() {
        let signature = RecoverableSignature {
            r: [1; 32],
            s: [2; 32],
            recovery_id: 1,
        };
        let bytes = signature.to_tron_bytes();
        assert_eq!(bytes[64], 28);
        assert_eq!(&bytes[..32], &[1; 32]);
        assert!(signature.y_parity());
    }
}
