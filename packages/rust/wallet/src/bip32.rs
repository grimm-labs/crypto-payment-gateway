//! Minimal BIP32 private-key derivation over secp256k1 (`k256` +
//! HMAC-SHA512). Replaces `tiny-hderive`, which pulled `libsecp256k1 0.3`
//! (RUSTSEC-2021-0076, unmaintained: RUSTSEC-2025-0161) into every
//! secp256k1 derivation.
//!
//! Only what the wallet needs: the master key from a BIP39 seed, private →
//! private child derivation (hardened and non-hardened) and path parsing.
//! Pinned by the official BIP32 test vectors below and by the golden
//! addresses in `lib.rs`: the output must stay byte-for-byte identical.

use std::fmt;

use hmac::{Hmac, Mac};
use k256::elliptic_curve::sec1::ToEncodedPoint;
use k256::elliptic_curve::PrimeField;
use k256::{FieldBytes, NonZeroScalar, Scalar, SecretKey};
use sha2::Sha512;
use zeroize::{Zeroize, Zeroizing};

use crate::WalletError;

type HmacSha512 = Hmac<Sha512>;

const HARDENED_BIT: u32 = 1 << 31;

/// One path component; values ≥ 2^31 are hardened.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ChildNumber(u32);

impl ChildNumber {
    pub(crate) fn is_hardened(self) -> bool {
        self.0 & HARDENED_BIT != 0
    }

    /// `ser32(i)`.
    pub(crate) fn to_be_bytes(self) -> [u8; 4] {
        self.0.to_be_bytes()
    }
}

/// Parses `m/44'/60'/0'/0/7`: starts with `m`, `'` marks a hardened index,
/// every index is plain decimal below 2^31. Anything else (empty segment,
/// sign, `h` suffix, overflow) is rejected rather than guessed.
pub(crate) fn parse_path(path: &str) -> Result<Vec<ChildNumber>, WalletError> {
    let invalid = || WalletError::Derivation(format!("invalid derivation path: {path}"));
    let mut segments = path.split('/');
    if segments.next() != Some("m") {
        return Err(invalid());
    }
    segments
        .map(|segment| parse_child(segment).ok_or_else(invalid))
        .collect()
}

fn parse_child(segment: &str) -> Option<ChildNumber> {
    let (digits, hardened) = match segment.strip_suffix('\'') {
        Some(digits) => (digits, true),
        None => (segment, false),
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let index: u32 = digits.parse().ok()?;
    if index & HARDENED_BIT != 0 {
        return None;
    }
    Some(ChildNumber(if hardened {
        index | HARDENED_BIT
    } else {
        index
    }))
}

/// HMAC-SHA512 over the concatenation of `parts`, in a buffer wiped on drop.
pub(crate) fn hmac_sha512(key: &[u8], parts: &[&[u8]]) -> Result<Zeroizing<[u8; 64]>, WalletError> {
    let mut mac =
        HmacSha512::new_from_slice(key).map_err(|e| WalletError::Derivation(e.to_string()))?;
    for part in parts {
        mac.update(part);
    }
    let mut digest = mac.finalize().into_bytes();
    let mut out = Zeroizing::new([0u8; 64]);
    out.copy_from_slice(&digest);
    digest[..].zeroize();
    Ok(out)
}

/// A BIP32 extended private key. Both halves are secret: `SecretKey` wipes
/// itself on drop, the chain code sits in `Zeroizing`.
pub(crate) struct ExtendedPrivateKey {
    secret_key: SecretKey,
    chain_code: Zeroizing<[u8; 32]>,
}

impl fmt::Debug for ExtendedPrivateKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ExtendedPrivateKey([REDACTED])")
    }
}

impl ExtendedPrivateKey {
    /// Derives the key at `path` from a BIP39 seed.
    pub(crate) fn derive(seed: &[u8], path: &[ChildNumber]) -> Result<Self, WalletError> {
        let mut key = Self::master(seed)?;
        for &child in path {
            key = key.child(child)?;
        }
        Ok(key)
    }

    /// `m`: `I = HMAC-SHA512(key = "Bitcoin seed", seed)`, `IL` is the key,
    /// `IR` the chain code. `parse256(IL)` = 0 or ≥ n is an invalid master
    /// key (`SecretKey::from_bytes` rejects both).
    pub(crate) fn master(seed: &[u8]) -> Result<Self, WalletError> {
        let i = hmac_sha512(b"Bitcoin seed", &[seed])?;
        let (il, chain_code) = split(&i);
        let secret_key = SecretKey::from_bytes(&il)
            .map_err(|_| WalletError::Derivation("BIP32 master key is invalid".into()))?;
        Ok(Self {
            secret_key,
            chain_code,
        })
    }

    /// CKDpriv. Hardened: `I = HMAC-SHA512(c_par, 0x00 || ser256(k_par) ||
    /// ser32(i))`; normal: `HMAC-SHA512(c_par, serP(point(k_par)) ||
    /// ser32(i))`. Then `k_i = parse256(IL) + k_par (mod n)`, `c_i = IR`.
    pub(crate) fn child(&self, child: ChildNumber) -> Result<Self, WalletError> {
        let index = child.to_be_bytes();
        let i = if child.is_hardened() {
            let parent = Zeroizing::new(self.secret_key.to_bytes());
            hmac_sha512(&self.chain_code[..], &[&[0u8], &parent[..], &index])?
        } else {
            let public = self.secret_key.public_key().to_encoded_point(true);
            hmac_sha512(&self.chain_code[..], &[public.as_bytes(), &index])?
        };
        let (il, chain_code) = split(&i);
        // BIP32 says to skip to i + 1 on an invalid child (probability
        // < 2^-127); fail instead: a deposit index must map to one key.
        let secret_key = tweak_add(&self.secret_key, &il).ok_or_else(|| {
            WalletError::Derivation(format!("BIP32 child {} is invalid", child.0))
        })?;
        Ok(Self {
            secret_key,
            chain_code,
        })
    }

    pub(crate) fn secret_key(&self) -> &SecretKey {
        &self.secret_key
    }
}

/// `k_i = parse256(IL) + k_par (mod n)`; `None` when `parse256(IL) ≥ n` or
/// `k_i = 0`, the two cases that make a BIP32 child key invalid.
fn tweak_add(parent: &SecretKey, il: &FieldBytes) -> Option<SecretKey> {
    let tweak: Option<Scalar> = Scalar::from_repr(*il).into();
    let mut tweak = tweak?;
    let mut parent = parent.to_nonzero_scalar();
    let mut sum = tweak + *parent;
    tweak.zeroize();
    parent.zeroize();
    let scalar: Option<NonZeroScalar> = NonZeroScalar::new(sum).into();
    sum.zeroize();
    let mut scalar = scalar?;
    let secret_key = SecretKey::from(&scalar);
    scalar.zeroize();
    Some(secret_key)
}

/// Splits `I` into `IL` (key material) and `IR` (chain code), both wiped
/// on drop.
fn split(i: &[u8; 64]) -> (Zeroizing<FieldBytes>, Zeroizing<[u8; 32]>) {
    let mut il = Zeroizing::new(FieldBytes::default());
    il.copy_from_slice(&i[..32]);
    let mut ir = Zeroizing::new([0u8; 32]);
    ir.copy_from_slice(&i[32..]);
    (il, ir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ripemd::Ripemd160;
    use sha2::{Digest, Sha256};

    const XPRV_VERSION: [u8; 4] = [0x04, 0x88, 0xAD, 0xE4];
    const XPUB_VERSION: [u8; 4] = [0x04, 0x88, 0xB2, 0x1E];

    fn base58check(payload: &[u8]) -> String {
        let checksum = Sha256::digest(Sha256::digest(payload));
        let mut data = payload.to_vec();
        data.extend_from_slice(&checksum[..4]);
        bs58::encode(data).into_string()
    }

    /// BIP32 serialization of the key at `path` (xprv, xpub). Depth, parent
    /// fingerprint and child number are recomputed here; the derivation code
    /// itself only carries the key and the chain code.
    fn serialize(seed: &[u8], path: &str) -> (String, String) {
        let children = parse_path(path).unwrap();
        let key = ExtendedPrivateKey::derive(seed, &children).unwrap();
        let (fingerprint, child_number) = match children.split_last() {
            None => ([0u8; 4], 0),
            Some((last, parents)) => {
                let parent = ExtendedPrivateKey::derive(seed, parents).unwrap();
                let point = parent.secret_key.public_key().to_encoded_point(true);
                let hash160 = Ripemd160::digest(Sha256::digest(point.as_bytes()));
                let mut fingerprint = [0u8; 4];
                fingerprint.copy_from_slice(&hash160[..4]);
                (fingerprint, last.0)
            }
        };
        let header = |version: [u8; 4]| {
            let mut data = version.to_vec();
            data.push(u8::try_from(children.len()).unwrap());
            data.extend_from_slice(&fingerprint);
            data.extend_from_slice(&child_number.to_be_bytes());
            data.extend_from_slice(&key.chain_code[..]);
            data
        };
        let mut xprv = header(XPRV_VERSION);
        xprv.push(0);
        xprv.extend_from_slice(&key.secret_key.to_bytes());
        let mut xpub = header(XPUB_VERSION);
        xpub.extend_from_slice(
            key.secret_key
                .public_key()
                .to_encoded_point(true)
                .as_bytes(),
        );
        (base58check(&xprv), base58check(&xpub))
    }

    fn check_vector(seed_hex: &str, chains: &[(&str, &str, &str)]) {
        let seed = hex::decode(seed_hex).unwrap();
        for (path, xpub, xprv) in chains {
            let (got_xprv, got_xpub) = serialize(&seed, path);
            assert_eq!(&got_xprv, xprv, "xprv at {path}");
            assert_eq!(&got_xpub, xpub, "xpub at {path}");
        }
    }

    // Test vectors 1–4 verbatim from BIP32
    // (github.com/bitcoin/bips/blob/master/bip-0032.mediawiki, "Test Vectors").

    #[test]
    fn bip32_test_vector_1() {
        check_vector(
            "000102030405060708090a0b0c0d0e0f",
            &[
                (
                    "m",
                    "xpub661MyMwAqRbcFtXgS5sYJABqqG9YLmC4Q1Rdap9gSE8NqtwybGhePY2gZ29ESFjqJoCu1Rupje8YtGqsefD265TMg7usUDFdp6W1EGMcet8",
                    "xprv9s21ZrQH143K3QTDL4LXw2F7HEK3wJUD2nW2nRk4stbPy6cq3jPPqjiChkVvvNKmPGJxWUtg6LnF5kejMRNNU3TGtRBeJgk33yuGBxrMPHi",
                ),
                (
                    "m/0'",
                    "xpub68Gmy5EdvgibQVfPdqkBBCHxA5htiqg55crXYuXoQRKfDBFA1WEjWgP6LHhwBZeNK1VTsfTFUHCdrfp1bgwQ9xv5ski8PX9rL2dZXvgGDnw",
                    "xprv9uHRZZhk6KAJC1avXpDAp4MDc3sQKNxDiPvvkX8Br5ngLNv1TxvUxt4cV1rGL5hj6KCesnDYUhd7oWgT11eZG7XnxHrnYeSvkzY7d2bhkJ7",
                ),
                (
                    "m/0'/1",
                    "xpub6ASuArnXKPbfEwhqN6e3mwBcDTgzisQN1wXN9BJcM47sSikHjJf3UFHKkNAWbWMiGj7Wf5uMash7SyYq527Hqck2AxYysAA7xmALppuCkwQ",
                    "xprv9wTYmMFdV23N2TdNG573QoEsfRrWKQgWeibmLntzniatZvR9BmLnvSxqu53Kw1UmYPxLgboyZQaXwTCg8MSY3H2EU4pWcQDnRnrVA1xe8fs",
                ),
                (
                    "m/0'/1/2'",
                    "xpub6D4BDPcP2GT577Vvch3R8wDkScZWzQzMMUm3PWbmWvVJrZwQY4VUNgqFJPMM3No2dFDFGTsxxpG5uJh7n7epu4trkrX7x7DogT5Uv6fcLW5",
                    "xprv9z4pot5VBttmtdRTWfWQmoH1taj2axGVzFqSb8C9xaxKymcFzXBDptWmT7FwuEzG3ryjH4ktypQSAewRiNMjANTtpgP4mLTj34bhnZX7UiM",
                ),
                (
                    "m/0'/1/2'/2",
                    "xpub6FHa3pjLCk84BayeJxFW2SP4XRrFd1JYnxeLeU8EqN3vDfZmbqBqaGJAyiLjTAwm6ZLRQUMv1ZACTj37sR62cfN7fe5JnJ7dh8zL4fiyLHV",
                    "xprvA2JDeKCSNNZky6uBCviVfJSKyQ1mDYahRjijr5idH2WwLsEd4Hsb2Tyh8RfQMuPh7f7RtyzTtdrbdqqsunu5Mm3wDvUAKRHSC34sJ7in334",
                ),
                (
                    "m/0'/1/2'/2/1000000000",
                    "xpub6H1LXWLaKsWFhvm6RVpEL9P4KfRZSW7abD2ttkWP3SSQvnyA8FSVqNTEcYFgJS2UaFcxupHiYkro49S8yGasTvXEYBVPamhGW6cFJodrTHy",
                    "xprvA41z7zogVVwxVSgdKUHDy1SKmdb533PjDz7J6N6mV6uS3ze1ai8FHa8kmHScGpWmj4WggLyQjgPie1rFSruoUihUZREPSL39UNdE3BBDu76",
                ),
            ],
        );
    }

    #[test]
    fn bip32_test_vector_2() {
        check_vector(
            "fffcf9f6f3f0edeae7e4e1dedbd8d5d2cfccc9c6c3c0bdbab7b4b1aeaba8a5a29f9c999693908d8a8784817e7b7875726f6c696663605d5a5754514e4b484542",
            &[
                (
                    "m",
                    "xpub661MyMwAqRbcFW31YEwpkMuc5THy2PSt5bDMsktWQcFF8syAmRUapSCGu8ED9W6oDMSgv6Zz8idoc4a6mr8BDzTJY47LJhkJ8UB7WEGuduB",
                    "xprv9s21ZrQH143K31xYSDQpPDxsXRTUcvj2iNHm5NUtrGiGG5e2DtALGdso3pGz6ssrdK4PFmM8NSpSBHNqPqm55Qn3LqFtT2emdEXVYsCzC2U",
                ),
                (
                    "m/0",
                    "xpub69H7F5d8KSRgmmdJg2KhpAK8SR3DjMwAdkxj3ZuxV27CprR9LgpeyGmXUbC6wb7ERfvrnKZjXoUmmDznezpbZb7ap6r1D3tgFxHmwMkQTPH",
                    "xprv9vHkqa6EV4sPZHYqZznhT2NPtPCjKuDKGY38FBWLvgaDx45zo9WQRUT3dKYnjwih2yJD9mkrocEZXo1ex8G81dwSM1fwqWpWkeS3v86pgKt",
                ),
                (
                    "m/0/2147483647'",
                    "xpub6ASAVgeehLbnwdqV6UKMHVzgqAG8Gr6riv3Fxxpj8ksbH9ebxaEyBLZ85ySDhKiLDBrQSARLq1uNRts8RuJiHjaDMBU4Zn9h8LZNnBC5y4a",
                    "xprv9wSp6B7kry3Vj9m1zSnLvN3xH8RdsPP1Mh7fAaR7aRLcQMKTR2vidYEeEg2mUCTAwCd6vnxVrcjfy2kRgVsFawNzmjuHc2YmYRmagcEPdU9",
                ),
                (
                    "m/0/2147483647'/1",
                    "xpub6DF8uhdarytz3FWdA8TvFSvvAh8dP3283MY7p2V4SeE2wyWmG5mg5EwVvmdMVCQcoNJxGoWaU9DCWh89LojfZ537wTfunKau47EL2dhHKon",
                    "xprv9zFnWC6h2cLgpmSA46vutJzBcfJ8yaJGg8cX1e5StJh45BBciYTRXSd25UEPVuesF9yog62tGAQtHjXajPPdbRCHuWS6T8XA2ECKADdw4Ef",
                ),
                (
                    "m/0/2147483647'/1/2147483646'",
                    "xpub6ERApfZwUNrhLCkDtcHTcxd75RbzS1ed54G1LkBUHQVHQKqhMkhgbmJbZRkrgZw4koxb5JaHWkY4ALHY2grBGRjaDMzQLcgJvLJuZZvRcEL",
                    "xprvA1RpRA33e1JQ7ifknakTFpgNXPmW2YvmhqLQYMmrj4xJXXWYpDPS3xz7iAxn8L39njGVyuoseXzU6rcxFLJ8HFsTjSyQbLYnMpCqE2VbFWc",
                ),
                (
                    "m/0/2147483647'/1/2147483646'/2",
                    "xpub6FnCn6nSzZAw5Tw7cgR9bi15UV96gLZhjDstkXXxvCLsUXBGXPdSnLFbdpq8p9HmGsApME5hQTZ3emM2rnY5agb9rXpVGyy3bdW6EEgAtqt",
                    "xprvA2nrNbFZABcdryreWet9Ea4LvTJcGsqrMzxHx98MMrotbir7yrKCEXw7nadnHM8Dq38EGfSh6dqA9QWTyefMLEcBYJUuekgW4BYPJcr9E7j",
                ),
            ],
        );
    }

    /// Leading zeros in the private key must be kept (bitcore-lib#47).
    #[test]
    fn bip32_test_vector_3() {
        check_vector(
            "4b381541583be4423346c643850da4b320e46a87ae3d2a4e6da11eba819cd4acba45d239319ac14f863b8d5ab5a0d0c64d2e8a1e7d1457df2e5a3c51c73235be",
            &[
                (
                    "m",
                    "xpub661MyMwAqRbcEZVB4dScxMAdx6d4nFc9nvyvH3v4gJL378CSRZiYmhRoP7mBy6gSPSCYk6SzXPTf3ND1cZAceL7SfJ1Z3GC8vBgp2epUt13",
                    "xprv9s21ZrQH143K25QhxbucbDDuQ4naNntJRi4KUfWT7xo4EKsHt2QJDu7KXp1A3u7Bi1j8ph3EGsZ9Xvz9dGuVrtHHs7pXeTzjuxBrCmmhgC6",
                ),
                (
                    "m/0'",
                    "xpub68NZiKmJWnxxS6aaHmn81bvJeTESw724CRDs6HbuccFQN9Ku14VQrADWgqbhhTHBaohPX4CjNLf9fq9MYo6oDaPPLPxSb7gwQN3ih19Zm4Y",
                    "xprv9uPDJpEQgRQfDcW7BkF7eTya6RPxXeJCqCJGHuCJ4GiRVLzkTXBAJMu2qaMWPrS7AANYqdq6vcBcBUdJCVVFceUvJFjaPdGZ2y9WACViL4L",
                ),
            ],
        );
    }

    /// Leading zeros in the hardened-derivation input (btcutil#172).
    #[test]
    fn bip32_test_vector_4() {
        check_vector(
            "3ddd5602285899a946114506157c7997e5444528f3003f6134712147db19b678",
            &[
                (
                    "m",
                    "xpub661MyMwAqRbcGczjuMoRm6dXaLDEhW1u34gKenbeYqAix21mdUKJyuyu5F1rzYGVxyL6tmgBUAEPrEz92mBXjByMRiJdba9wpnN37RLLAXa",
                    "xprv9s21ZrQH143K48vGoLGRPxgo2JNkJ3J3fqkirQC2zVdk5Dgd5w14S7fRDyHH4dWNHUgkvsvNDCkvAwcSHNAQwhwgNMgZhLtQC63zxwhQmRv",
                ),
                (
                    "m/0'",
                    "xpub69AUMk3qDBi3uW1sXgjCmVjJ2G6WQoYSnNHyzkmdCHEhSZ4tBok37xfFEqHd2AddP56Tqp4o56AePAgCjYdvpW2PU2jbUPFKsav5ut6Ch1m",
                    "xprv9vB7xEWwNp9kh1wQRfCCQMnZUEG21LpbR9NPCNN1dwhiZkjjeGRnaALmPXCX7SgjFTiCTT6bXes17boXtjq3xLpcDjzEuGLQBM5ohqkao9G",
                ),
                (
                    "m/0'/1'",
                    "xpub6BJA1jSqiukeaesWfxe6sNK9CCGaujFFSJLomWHprUL9DePQ4JDkM5d88n49sMGJxrhpjazuXYWdMf17C9T5XnxkopaeS7jGk1GyyVziaMt",
                    "xprv9xJocDuwtYCMNAo3Zw76WENQeAS6WGXQ55RCy7tDJ8oALr4FWkuVoHJeHVAcAqiZLE7Je3vZJHxspZdFHfnBEjHqU5hG1Jaj32dVoS6XLT1",
                ),
            ],
        );
    }

    #[test]
    fn parses_paths() {
        assert_eq!(parse_path("m").unwrap(), vec![]);
        assert_eq!(
            parse_path("m/44'/60'/0'/0/7").unwrap(),
            vec![
                ChildNumber(44 | HARDENED_BIT),
                ChildNumber(60 | HARDENED_BIT),
                ChildNumber(HARDENED_BIT),
                ChildNumber(0),
                ChildNumber(7),
            ]
        );
        assert_eq!(
            parse_path("m/2147483647'/2147483647").unwrap(),
            vec![ChildNumber(u32::MAX), ChildNumber(HARDENED_BIT - 1)]
        );
        for bad in [
            "",
            "/0",
            "M/0",
            "0/1",
            "m/",
            "m//0",
            "m/0/",
            "m/'",
            "m/0''",
            "m/0h",
            "m/+1",
            "m/-1",
            "m/ 1",
            "m/1 ",
            "m/2147483648",
            "m/2147483648'",
            "m/4294967296",
            "m/0x10",
        ] {
            assert!(parse_path(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    fn field_bytes(hex_str: &str) -> FieldBytes {
        let bytes: [u8; 32] = hex::decode(hex_str).unwrap().try_into().unwrap();
        FieldBytes::from(bytes)
    }

    /// The secp256k1 group order n and values around it.
    const N: &str = "fffffffffffffffffffffffffffffffebaaedce6af48a03bbfd25e8cd0364141";
    const N_MINUS_1: &str = "fffffffffffffffffffffffffffffffebaaedce6af48a03bbfd25e8cd0364140";
    const N_MINUS_2: &str = "fffffffffffffffffffffffffffffffebaaedce6af48a03bbfd25e8cd036413f";
    const ONE: &str = "0000000000000000000000000000000000000000000000000000000000000001";
    const TWO: &str = "0000000000000000000000000000000000000000000000000000000000000002";

    #[test]
    fn tweak_add_rejects_invalid_children() {
        let one = SecretKey::from_bytes(&field_bytes(ONE)).unwrap();
        // parse256(IL) ≥ n → invalid.
        assert!(tweak_add(&one, &field_bytes(N)).is_none());
        assert!(tweak_add(&one, &FieldBytes::from([0xff; 32])).is_none());
        // parse256(IL) + k_par = n ≡ 0 → invalid.
        assert!(tweak_add(&one, &field_bytes(N_MINUS_1)).is_none());
        // Valid edges: IL = 0 keeps the parent, and the sum wraps mod n.
        let same = tweak_add(&one, &FieldBytes::default()).unwrap();
        assert_eq!(same.to_bytes(), field_bytes(ONE));
        let two = SecretKey::from_bytes(&field_bytes(TWO)).unwrap();
        let wrapped = tweak_add(&two, &field_bytes(N_MINUS_1)).unwrap();
        assert_eq!(wrapped.to_bytes(), field_bytes(ONE));
        let below = tweak_add(&one, &field_bytes(N_MINUS_2)).unwrap();
        assert_eq!(below.to_bytes(), field_bytes(N_MINUS_1));
    }

    #[test]
    fn master_key_bounds() {
        // `master` relies on `SecretKey::from_bytes` refusing 0 and ≥ n.
        assert!(SecretKey::from_bytes(&FieldBytes::default()).is_err());
        assert!(SecretKey::from_bytes(&field_bytes(N)).is_err());
        assert!(SecretKey::from_bytes(&field_bytes(N_MINUS_1)).is_ok());
    }

    #[test]
    fn debug_is_redacted() {
        let key = ExtendedPrivateKey::master(&[7u8; 32]).unwrap();
        let shown = format!("{key:?}");
        assert_eq!(shown, "ExtendedPrivateKey([REDACTED])");
        assert!(!shown.contains(&hex::encode(key.secret_key.to_bytes())));
    }
}
