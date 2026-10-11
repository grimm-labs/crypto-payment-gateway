//! Chain identities checked at boot (`ChainClients::verify_networks`): a
//! provider serving a testnet, a fork or another chain would otherwise
//! "confirm" deposits that do not exist on mainnet — and in testnet mode
//! (`CHAIN_MODE=testnet`), a mainnet provider must not be used either.

use config::ChainMode;
use domain::Network;

/// Bitcoin mainnet genesis block hash (`GET /block-height/0` on Esplora).
pub(crate) const BITCOIN_GENESIS_HASH: &str =
    "000000000019d6689c085ae165831e934ff763ae46a2a6c172b3f1b60a8ce26f";
/// Solana mainnet-beta genesis hash (`getGenesisHash`).
pub(crate) const SOLANA_GENESIS_HASH: &str = "5eykt4UsFv8P8NJdTREpY1vzqKqZKvdpKuc147dw2N9d";
/// Tron mainnet genesis block id (`wallet/getblockbynum {"num": 0}`).
pub(crate) const TRON_GENESIS_BLOCK_ID: &str =
    "00000000000000001ebf88508a03865c71d452e25f4d51194196a1d22b6653dc";

/// Bitcoin testnet4 genesis block hash.
pub(crate) const BITCOIN_TESTNET4_GENESIS_HASH: &str =
    "00000000da84f2bafbbc53dee25a72ae507ff4914b867c565be350b0da8bf043";
/// Solana devnet genesis hash.
pub(crate) const SOLANA_DEVNET_GENESIS_HASH: &str = "EtWTRABZaYq6iMfeYKouRu166VU2xqa1wcaWoxPkrZBG";
/// Tron Nile testnet genesis block id.
pub(crate) const TRON_NILE_GENESIS_BLOCK_ID: &str =
    "0000000000000000d698d4192c56cb6be724a558448e2684802de4d6cd8690dc";

/// The identity `network`'s provider must answer in `mode` (testnet values
/// checked against the public endpoints on 2026-10-11). `None` for
/// Ethereum, whose identity is its chain id (`config::ChainParams`).
pub(crate) fn expected_genesis(network: Network, mode: ChainMode) -> Option<&'static str> {
    match (network, mode) {
        (Network::Bitcoin, ChainMode::Mainnet) => Some(BITCOIN_GENESIS_HASH),
        (Network::Bitcoin, ChainMode::Testnet) => Some(BITCOIN_TESTNET4_GENESIS_HASH),
        (Network::Solana, ChainMode::Mainnet) => Some(SOLANA_GENESIS_HASH),
        (Network::Solana, ChainMode::Testnet) => Some(SOLANA_DEVNET_GENESIS_HASH),
        (Network::Tron, ChainMode::Mainnet) => Some(TRON_GENESIS_BLOCK_ID),
        (Network::Tron, ChainMode::Testnet) => Some(TRON_NILE_GENESIS_BLOCK_ID),
        _ => None,
    }
}

/// The provider answered, but for another chain. Distinguishable (via
/// `anyhow::Error::downcast_ref`) from a provider that could not be reached.
#[derive(Debug, thiserror::Error)]
#[error("{} provider is not on the expected chain: expected {expected}, got {actual}", network.as_api())]
pub struct WrongChainError {
    pub network: Network,
    pub expected: String,
    /// What the provider answered, truncated.
    pub actual: String,
}

/// Compare a provider's chain identity (genesis hash, chain id) to mainnet's.
/// Hex identities compare case-insensitively; base58 ones must match exactly.
pub(crate) fn expect_identity(
    network: Network,
    expected: &str,
    actual: &str,
) -> Result<(), WrongChainError> {
    let actual = actual.trim();
    let matches = match network {
        Network::Solana => actual == expected,
        _ => actual.eq_ignore_ascii_case(expected),
    };
    if matches {
        return Ok(());
    }
    Err(WrongChainError {
        network,
        expected: expected.to_string(),
        actual: actual.chars().take(80).collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mainnet_identities_match() {
        assert!(expect_identity(Network::Bitcoin, BITCOIN_GENESIS_HASH, &format!("{BITCOIN_GENESIS_HASH}\n")).is_ok());
        assert!(expect_identity(Network::Tron, TRON_GENESIS_BLOCK_ID, &TRON_GENESIS_BLOCK_ID.to_uppercase()).is_ok());
        assert!(expect_identity(Network::Solana, SOLANA_GENESIS_HASH, SOLANA_GENESIS_HASH).is_ok());
    }

    #[test]
    fn other_chains_are_rejected() {
        // Bitcoin testnet3, Solana devnet, Tron Nile genesis.
        let err = expect_identity(
            Network::Bitcoin,
            BITCOIN_GENESIS_HASH,
            "000000000933ea01ad0ee984209779baaec3ced90fa3f408719526f8d77f4943",
        )
        .unwrap_err();
        assert!(err.to_string().starts_with("bitcoin provider is not on the expected chain"));
        assert!(expect_identity(
            Network::Solana,
            SOLANA_GENESIS_HASH,
            "EtWTRABZaYq6iMfeYKouRu166VU2xqa1wcaWoxPkrZBG"
        )
        .is_err());
        // base58 is case-sensitive.
        assert!(expect_identity(Network::Solana, SOLANA_GENESIS_HASH, &SOLANA_GENESIS_HASH.to_lowercase()).is_err());
        assert!(expect_identity(
            Network::Tron,
            TRON_GENESIS_BLOCK_ID,
            "0000000000000000d698d4192c56cb6be724a558448e2684802de4d6cd8690dc"
        )
        .is_err());
        assert!(expect_identity(Network::Tron, TRON_GENESIS_BLOCK_ID, "").is_err());
    }

    #[test]
    fn each_mode_expects_its_own_chain() {
        for network in [Network::Bitcoin, Network::Solana, Network::Tron] {
            let mainnet = expected_genesis(network, ChainMode::Mainnet).unwrap();
            let testnet = expected_genesis(network, ChainMode::Testnet).unwrap();
            assert_ne!(mainnet, testnet, "{network:?}");
            // A mainnet provider fails the testnet check and vice versa.
            assert!(expect_identity(network, testnet, mainnet).is_err());
        }
        assert_eq!(expected_genesis(Network::Ethereum, ChainMode::Testnet), None);
    }

    #[test]
    fn provider_answers_are_truncated_in_errors() {
        let err = expect_identity(Network::Bitcoin, BITCOIN_GENESIS_HASH, &"x".repeat(10_000)).unwrap_err();
        assert_eq!(err.actual.len(), 80);
    }
}
