//! Withdrawal destinations: syntax, network and amount rules (spec §7.4),
//! and their normalized form — what is stored, hashed into the parameters
//! and compared against the address book. Pure: whether a destination is a
//! gateway address, or a Solana token account of the right mint, needs the
//! database / an RPC and is checked by the caller.

use std::str::FromStr;
use std::time::Duration;

use domain::{DestinationKind, Network};
use sha2::{Digest, Sha256};
use sha3::Keccak256;

/// Longest Lightning address accepted (local part + `@` + domain).
pub const LN_ADDRESS_MAX_LEN: usize = 320;

/// Domain suffixes that only resolve inside a private network (or never).
const LOCAL_TLDS: [&str; 13] = [
    "local", "localhost", "internal", "lan", "home", "localdomain", "arpa", "test", "invalid",
    "example", "onion", "corp", "intranet",
];

/// What the rules depend on beyond the destination itself.
#[derive(Debug, Clone, Copy)]
pub struct DestinationRules<'a> {
    /// Testnet mode: Bitcoin testnet4 addresses and test invoices instead
    /// of mainnet ones.
    pub testnet: bool,
    /// Sending to the token contract itself would burn the funds.
    pub usdt_eth_contract: &'a str,
    /// Unix seconds, for invoice expiry.
    pub now_unix: u64,
}

/// Client-safe reason a destination is refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DestinationError {
    #[error("{0}")]
    Invalid(&'static str),
    #[error("Amount is below the dust limit of this address type ({0} sats)")]
    BelowDust(u64),
    #[error("Invoice amount ({invoice_sats} sats) does not match the requested amount ({requested_sats} sats)")]
    InvoiceAmountMismatch { invoice_sats: u64, requested_sats: u64 },
}

/// Validate and normalize `raw` as a `kind` destination on `network` for
/// `amount_raw` base units.
pub fn normalize_destination(
    network: Network,
    kind: DestinationKind,
    raw: &str,
    amount_raw: u128,
    rules: &DestinationRules<'_>,
) -> Result<String, DestinationError> {
    let raw = raw.trim();
    match (network, kind) {
        (Network::Ethereum, DestinationKind::Address) => normalize_eth_address(raw, rules.usdt_eth_contract),
        (Network::Tron, DestinationKind::Address) => normalize_tron_address(raw),
        (Network::Solana, DestinationKind::Address) => normalize_solana_address(raw),
        (Network::Bitcoin, DestinationKind::Address) => {
            normalize_bitcoin_address(raw, rules.testnet, sats(amount_raw)?)
        }
        (Network::Lightning, DestinationKind::LnAddress) => normalize_ln_address(raw),
        (Network::Lightning, DestinationKind::Bolt11) => {
            validate_bolt11(raw, rules.testnet, sats(amount_raw)?, rules.now_unix)
        }
        (Network::Lightning, DestinationKind::Address) => Err(DestinationError::Invalid(
            "Lightning destinations are Lightning addresses (user@domain) or BOLT11 invoices",
        )),
        _ => Err(DestinationError::Invalid(
            "This destination kind is not supported on this network",
        )),
    }
}

/// The destination kind a raw Lightning destination string is.
pub fn lightning_destination_kind(raw: &str) -> DestinationKind {
    if raw.contains('@') {
        DestinationKind::LnAddress
    } else {
        DestinationKind::Bolt11
    }
}

fn sats(amount_raw: u128) -> Result<u64, DestinationError> {
    u64::try_from(amount_raw).map_err(|_| DestinationError::Invalid("Amount is too large"))
}

/// 20-byte hex, all lowercase/uppercase or with a valid EIP-55 checksum;
/// never the zero address or the USDT contract. Returns EIP-55.
pub fn normalize_eth_address(raw: &str, usdt_contract: &str) -> Result<String, DestinationError> {
    let invalid = DestinationError::Invalid("Invalid Ethereum address");
    let hex = raw.strip_prefix("0x").ok_or(invalid.clone())?;
    if hex.len() != 40 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(invalid);
    }
    let lower = hex.to_ascii_lowercase();
    let checksummed = eip55(&lower);
    let mixed_case = hex.bytes().any(|b| b.is_ascii_lowercase()) && hex.bytes().any(|b| b.is_ascii_uppercase());
    if mixed_case && checksummed[2..] != *hex {
        return Err(DestinationError::Invalid("Invalid Ethereum address checksum"));
    }
    if lower.bytes().all(|b| b == b'0') {
        return Err(DestinationError::Invalid("The zero address is not a destination"));
    }
    if checksummed.eq_ignore_ascii_case(usdt_contract) {
        return Err(DestinationError::Invalid(
            "The USDT contract is not a destination (the funds would be lost)",
        ));
    }
    Ok(checksummed)
}

/// EIP-55 mixed-case checksum of a lowercase 40-hex-digit address.
pub fn eip55(lower_hex: &str) -> String {
    let hash = Keccak256::digest(lower_hex.as_bytes());
    let mut out = String::with_capacity(42);
    out.push_str("0x");
    for (i, c) in lower_hex.chars().enumerate() {
        let nibble = if i % 2 == 0 { hash[i / 2] >> 4 } else { hash[i / 2] & 0x0f };
        out.push(if c.is_ascii_alphabetic() && nibble >= 8 { c.to_ascii_uppercase() } else { c });
    }
    out
}

/// Base58check with the `0x41` mainnet prefix.
pub fn normalize_tron_address(raw: &str) -> Result<String, DestinationError> {
    tron_decode(raw).map(|bytes| tron_encode(&bytes))
}

/// `T…` base58check → the 21 bytes (`0x41` ‖ 20-byte account).
pub fn tron_decode(raw: &str) -> Result<[u8; 21], DestinationError> {
    let invalid = DestinationError::Invalid("Invalid Tron address");
    let bytes = bs58::decode(raw).into_vec().map_err(|_| invalid.clone())?;
    if bytes.len() != 25 || bytes[0] != 0x41 {
        return Err(invalid);
    }
    let checksum = Sha256::digest(Sha256::digest(&bytes[..21]));
    if checksum[..4] != bytes[21..] {
        return Err(DestinationError::Invalid("Invalid Tron address checksum"));
    }
    let mut out = [0u8; 21];
    out.copy_from_slice(&bytes[..21]);
    Ok(out)
}

/// 21 bytes (`0x41` ‖ account) → `T…`.
pub fn tron_encode(bytes: &[u8; 21]) -> String {
    let checksum = Sha256::digest(Sha256::digest(bytes));
    let mut payload = bytes.to_vec();
    payload.extend_from_slice(&checksum[..4]);
    bs58::encode(payload).into_string()
}

/// A base58 32-byte public key (a wallet or a token account — told apart
/// by an RPC lookup at admission).
pub fn normalize_solana_address(raw: &str) -> Result<String, DestinationError> {
    let bytes = bs58::decode(raw)
        .into_vec()
        .map_err(|_| DestinationError::Invalid("Invalid Solana address"))?;
    let bytes: [u8; 32] = bytes
        .try_into()
        .map_err(|_| DestinationError::Invalid("Invalid Solana address"))?;
    Ok(bs58::encode(bytes).into_string())
}

/// Smallest output an address type relays (Bitcoin Core dust at the
/// default 3 sat/vB dust relay fee).
pub fn bitcoin_dust_limit(address_type: bitcoin::AddressType) -> u64 {
    match address_type {
        bitcoin::AddressType::P2pkh => 546,
        bitcoin::AddressType::P2sh => 540,
        bitcoin::AddressType::P2wpkh => 294,
        bitcoin::AddressType::P2wsh | bitcoin::AddressType::P2tr => 330,
        _ => 546,
    }
}

/// Any standard address of the gateway's Bitcoin network (P2PKH, P2SH,
/// P2WPKH, P2WSH, P2TR), the amount at or above its dust limit.
pub fn normalize_bitcoin_address(raw: &str, testnet: bool, amount_sats: u64) -> Result<String, DestinationError> {
    let address = parse_bitcoin_address(raw, testnet)?;
    let address_type = address
        .address_type()
        .ok_or(DestinationError::Invalid("Unsupported Bitcoin address type"))?;
    let dust = bitcoin_dust_limit(address_type);
    if amount_sats < dust {
        return Err(DestinationError::BelowDust(dust));
    }
    Ok(address.to_string())
}

/// A Bitcoin address checked against the gateway's network.
pub fn parse_bitcoin_address(raw: &str, testnet: bool) -> Result<bitcoin::Address, DestinationError> {
    let network = if testnet {
        bitcoin::Network::Testnet
    } else {
        bitcoin::Network::Bitcoin
    };
    bitcoin::Address::from_str(raw)
        .map_err(|_| DestinationError::Invalid("Invalid Bitcoin address"))?
        .require_network(network)
        .map_err(|_| DestinationError::Invalid("Bitcoin address of another network"))
}

/// `user@domain` (LUD-16): one `@`, a user part of letters, digits and
/// `-_.`, and a public DNS name — the SDK fetches
/// `https://domain/.well-known/lnurlp/user`, so IP literals and local names
/// are refused. Normalized to lowercase.
pub fn normalize_ln_address(raw: &str) -> Result<String, DestinationError> {
    if is_lightning_address(raw) {
        Ok(raw.to_ascii_lowercase())
    } else {
        Err(DestinationError::Invalid(
            "Lightning destinations are Lightning addresses (user@domain) or BOLT11 invoices",
        ))
    }
}

pub fn is_lightning_address(value: &str) -> bool {
    if value.len() > LN_ADDRESS_MAX_LEN {
        return false;
    }
    let Some((user, domain)) = value.split_once('@') else {
        return false;
    };
    let user_ok = !user.is_empty()
        && user
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c));
    let labels: Vec<&str> = domain.split('.').collect();
    let labels_ok = labels.len() >= 2
        && labels.iter().all(|label| {
            !label.is_empty()
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        });
    // A public TLD is alphabetic — this also rules out IPv4 literals.
    let tld = labels.last().copied().unwrap_or_default().to_ascii_lowercase();
    let tld_ok = tld.len() >= 2
        && tld.chars().all(|c| c.is_ascii_alphabetic())
        && !LOCAL_TLDS.contains(&tld.as_str());
    user_ok && labels_ok && tld_ok
}

/// A BOLT11 invoice of the gateway's network, with an amount equal to the
/// requested one, not expired. Amountless invoices are refused: the amount
/// a withdrawal sends is always the requested one. Normalized to lowercase.
pub fn validate_bolt11(
    raw: &str,
    testnet: bool,
    amount_sats: u64,
    now_unix: u64,
) -> Result<String, DestinationError> {
    use lightning_invoice::{Bolt11Invoice, Currency};
    let normalized = raw.to_ascii_lowercase();
    let invoice = Bolt11Invoice::from_str(&normalized)
        .map_err(|_| DestinationError::Invalid("Invalid BOLT11 invoice"))?;
    let network_ok = match invoice.currency() {
        Currency::Bitcoin => !testnet,
        Currency::BitcoinTestnet | Currency::Regtest | Currency::Signet => testnet,
        _ => false,
    };
    if !network_ok {
        return Err(DestinationError::Invalid("BOLT11 invoice of another network"));
    }
    let Some(amount_msat) = invoice.amount_milli_satoshis() else {
        return Err(DestinationError::Invalid(
            "Amountless BOLT11 invoices are not accepted",
        ));
    };
    if amount_msat % 1000 != 0 || amount_msat / 1000 != amount_sats {
        return Err(DestinationError::InvoiceAmountMismatch {
            invoice_sats: amount_msat / 1000,
            requested_sats: amount_sats,
        });
    }
    let expires_at = invoice
        .duration_since_epoch()
        .saturating_add(invoice.expiry_time());
    if expires_at <= Duration::from_secs(now_unix) {
        return Err(DestinationError::Invalid("BOLT11 invoice has expired"));
    }
    Ok(normalized)
}

#[cfg(test)]
mod tests {
    use super::*;
    use config::USDT_ETH;

    const NOW: u64 = 1_800_000_000;

    fn rules(testnet: bool) -> DestinationRules<'static> {
        DestinationRules {
            testnet,
            usdt_eth_contract: USDT_ETH,
            now_unix: NOW,
        }
    }

    #[test]
    fn ethereum_addresses_are_checksummed() {
        // EIP-55 reference vectors.
        for checksummed in [
            "0x5aAeb6053F3E94C9b9A09f33669435E7Ef1BeAed",
            "0xfB6916095ca1df60bB79Ce92cE3Ea74c37c5d359",
            "0xdbF03B407c01E7cD3CBea99509d93f8DDDC8C6FB",
        ] {
            assert_eq!(normalize_eth_address(checksummed, USDT_ETH).unwrap(), checksummed);
            assert_eq!(
                normalize_eth_address(&checksummed.to_ascii_lowercase(), USDT_ETH).unwrap(),
                checksummed
            );
            let upper = format!("0x{}", checksummed[2..].to_ascii_uppercase());
            assert_eq!(normalize_eth_address(&upper, USDT_ETH).unwrap(), checksummed);
        }
        // One case flipped: bad checksum.
        let err = normalize_eth_address("0x5aaeb6053F3E94C9b9A09f33669435E7Ef1BeAed", USDT_ETH).unwrap_err();
        assert_eq!(err, DestinationError::Invalid("Invalid Ethereum address checksum"));
        for bad in [
            "5aAeb6053F3E94C9b9A09f33669435E7Ef1BeAed",
            "0x5aAeb6053F3E94C9b9A09f33669435E7Ef1BeAe",
            "0x5aAeb6053F3E94C9b9A09f33669435E7Ef1BeAedd",
            "0xZaAeb6053F3E94C9b9A09f33669435E7Ef1BeAed",
            "0x0000000000000000000000000000000000000000",
        ] {
            assert!(normalize_eth_address(bad, USDT_ETH).is_err(), "{bad}");
        }
        assert!(normalize_eth_address(&USDT_ETH.to_ascii_lowercase(), USDT_ETH).is_err());
    }

    #[test]
    fn tron_addresses_need_the_mainnet_prefix_and_checksum() {
        let address = "TR7NHqjeKQxGTCi8q8ZY4pL8otSzgjLj6t";
        assert_eq!(normalize_tron_address(address).unwrap(), address);
        let bytes = tron_decode(address).unwrap();
        assert_eq!(hex::encode(bytes), "41a614f803b6fd780986a42c78ec9c7f77e6ded13c");
        assert_eq!(tron_encode(&bytes), address);
        // Last character changed: checksum fails.
        assert!(normalize_tron_address("TR7NHqjeKQxGTCi8q8ZY4pL8otSzgjLj6u").is_err());
        // A Bitcoin P2PKH address is base58check too, with another prefix.
        assert!(normalize_tron_address("1A1zP1eP5QGefi2DMPTfTL5SLmv7DivfNa").is_err());
        assert!(normalize_tron_address("0xa614f803b6fd780986a42c78ec9c7f77e6ded13c").is_err());
    }

    #[test]
    fn solana_addresses_are_32_byte_keys() {
        let owner = "HAgk14JpMQLgt6rVgv7cBQFJWFto5Dqxi472uT3DKpqk";
        assert_eq!(normalize_solana_address(owner).unwrap(), owner);
        assert!(normalize_solana_address("HAgk14JpMQLgt6rVgv7cBQFJWFto5Dqx").is_err());
        assert!(normalize_solana_address(&format!("{owner}{owner}")).is_err());
        assert!(normalize_solana_address("0OIl").is_err());
        assert!(normalize_solana_address("").is_err());
    }

    #[test]
    fn bitcoin_accepts_every_standard_type_above_dust() {
        for (address, dust) in [
            ("1A1zP1eP5QGefi2DMPTfTL5SLmv7DivfNa", 546),
            ("3J98t1WpEZ73CNmQviecrnyiWrnqRhWNLy", 540),
            ("bc1qcr8te4kr609gcawutmrza0j4xv80jy8z306fyu", 294),
            ("bc1qrp33g0q5c5txsp9arysrx4k6zdkfs4nce4xj0gdcccefvpysxf3qccfmv3", 330),
            ("bc1p5d7rjq7g6rdk2yhzks9smlaqtedr4dekq08ge8ztwac72sfr9rusxg3297", 330),
        ] {
            assert_eq!(normalize_bitcoin_address(address, false, dust).unwrap(), address);
            assert_eq!(
                normalize_bitcoin_address(address, false, dust - 1).unwrap_err(),
                DestinationError::BelowDust(dust),
                "{address}"
            );
        }
        // bech32 is case-insensitive; normalized to lowercase.
        assert_eq!(
            normalize_bitcoin_address("BC1QCR8TE4KR609GCAWUTMRZA0J4XV80JY8Z306FYU", false, 1000).unwrap(),
            "bc1qcr8te4kr609gcawutmrza0j4xv80jy8z306fyu"
        );
    }

    #[test]
    fn bitcoin_addresses_of_another_network_are_refused() {
        let testnet = "tb1q6rz28mcfaxtmd6v789l9rrlrusdprr9pqcpvkl";
        assert!(normalize_bitcoin_address(testnet, false, 10_000).is_err());
        assert_eq!(normalize_bitcoin_address(testnet, true, 10_000).unwrap(), testnet);
        assert!(normalize_bitcoin_address("bc1qcr8te4kr609gcawutmrza0j4xv80jy8z306fyu", true, 10_000).is_err());
        assert!(normalize_bitcoin_address("not-an-address", false, 10_000).is_err());
    }

    #[test]
    fn lightning_addresses_are_public_user_at_domain() {
        assert_eq!(normalize_ln_address("Alice@Example.com").unwrap(), "alice@example.com");
        for bad in [
            "alice", "@example.com", "alice@", "alice@localhost", "alice@@example.com",
            "a b@example.com", "alice+tips@example.com", "alice@127.0.0.1", "alice@10.0.0.5",
            "alice@printer.local", "alice@db.internal", "alice@node.test", "alice@x.onion",
        ] {
            assert!(normalize_ln_address(bad).is_err(), "{bad}");
        }
        assert!(!is_lightning_address(&format!("{}@example.com", "a".repeat(400))));
    }

    // BOLT #11 examples (also lightning-invoice's own test vectors).
    /// "Please send $3 for a cup of coffee…": 2500u = 250 000 sats,
    /// timestamp 1496314658, expires 60 s later.
    const COFFEE: &str = "lnbc2500u1pvjluezsp5zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zygspp5qqqsyqcyq5rqwzqfqqqsyqcyq5rqwzqfqqqsyqcyq5rqwzqfqypqdq5xysxxatsyp3k7enxv4jsxqzpu9qrsgquk0rl77nj30yxdy8j9vdx85fkpmdla2087ne0xh8nhedh8w27kyke0lp53ut353s06fv3qfegext0eh0ymjpf39tuven09sam30g4vgpfna3rh";
    /// Donation invoice: no amount.
    const AMOUNTLESS: &str = "lnbc1pvjluezsp5zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zygspp5qqqsyqcyq5rqwzqfqqqsyqcyq5rqwzqfqqqsyqcyq5rqwzqfqypqdpl2pkx2ctnv5sxxmmwwd5kgetjypeh2ursdae8g6twvus8g6rfwvs8qun0dfjkxaq9qrsgq357wnc5r2ueh7ck6q93dj32dlqnls087fxdwk8qakdyafkq3yap9us6v52vjjsrvywa6rt52cm9r9zqt8r2t7mlcwspyetp5h2tztugp9lfyql";
    /// Testnet, 20m = 2 000 000 sats.
    const TESTNET: &str = "lntb20m1pvjluezsp5zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zygshp58yjmdan79s6qqdhdzgynm4zwqd5d7xmw5fk98klysy043l2ahrqspp5qqqsyqcyq5rqwzqfqqqsyqcyq5rqwzqfqqqsyqcyq5rqwzqfqypqfpp3x9et2e20v6pu37c5d9vax37wxq72un989qrsgqdj545axuxtnfemtpwkc45hx9d2ft7x04mt8q7y6t0k2dge9e7h8kpy9p34ytyslj3yu569aalz2xdk8xkd7ltxqld94u8h2esmsmacgpghe9k8";
    const COFFEE_ISSUED_AT: u64 = 1_496_314_658;

    #[test]
    fn bolt11_must_carry_the_requested_amount_and_be_live() {
        let live = COFFEE_ISSUED_AT + 10;
        assert_eq!(validate_bolt11(COFFEE, false, 250_000, live).unwrap(), COFFEE);
        assert_eq!(
            validate_bolt11(&COFFEE.to_ascii_uppercase(), false, 250_000, live).unwrap(),
            COFFEE
        );
        assert_eq!(
            validate_bolt11(COFFEE, false, 250_001, live).unwrap_err(),
            DestinationError::InvoiceAmountMismatch { invoice_sats: 250_000, requested_sats: 250_001 }
        );
        assert_eq!(
            validate_bolt11(COFFEE, false, 250_000, COFFEE_ISSUED_AT + 60).unwrap_err(),
            DestinationError::Invalid("BOLT11 invoice has expired")
        );
        assert_eq!(
            validate_bolt11(AMOUNTLESS, false, 1_000, live).unwrap_err(),
            DestinationError::Invalid("Amountless BOLT11 invoices are not accepted")
        );
        assert_eq!(
            validate_bolt11(TESTNET, false, 2_000_000, live).unwrap_err(),
            DestinationError::Invalid("BOLT11 invoice of another network")
        );
        // LNURL strings are not invoices.
        assert!(validate_bolt11(
            "lnurl1dp68gurn8ghj7um9wfmxjcm99e3k7mf0v9cxj0m385ekvcenxc6r2c35xvukxefcv5mkvv34x5ekzd3ev56nyd3hxqurzepexejxxepnxscrvwfnv9nxzcn9xq6xyefhvgcxxcmyxymnserxfq5fns",
            false, 1_000, live
        )
        .is_err());
    }

    #[test]
    fn destinations_dispatch_on_network_and_kind() {
        let r = rules(false);
        assert!(normalize_destination(Network::Lightning, DestinationKind::Address, "alice@example.com", 1, &r).is_err());
        assert!(normalize_destination(Network::Ethereum, DestinationKind::LnAddress, "alice@example.com", 1, &r).is_err());
        assert!(normalize_destination(Network::Bitcoin, DestinationKind::Bolt11, COFFEE, 250_000, &r).is_err());
        assert_eq!(
            normalize_destination(Network::Tron, DestinationKind::Address, " TR7NHqjeKQxGTCi8q8ZY4pL8otSzgjLj6t ", 1, &r).unwrap(),
            "TR7NHqjeKQxGTCi8q8ZY4pL8otSzgjLj6t"
        );
        assert_eq!(lightning_destination_kind("alice@example.com"), DestinationKind::LnAddress);
        assert_eq!(lightning_destination_kind(COFFEE), DestinationKind::Bolt11);
        // BTC amounts beyond u64 are refused, not wrapped.
        assert!(normalize_destination(
            Network::Bitcoin,
            DestinationKind::Address,
            "bc1qcr8te4kr609gcawutmrza0j4xv80jy8z306fyu",
            u128::from(u64::MAX) + 1,
            &r
        )
        .is_err());
    }
}
