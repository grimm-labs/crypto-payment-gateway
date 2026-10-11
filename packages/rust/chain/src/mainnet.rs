//! Chain identities checked at boot (`ChainClients::verify_networks`): a
//! provider serving a testnet, a fork or another chain would otherwise
//! "confirm" deposits that do not exist on mainnet.

use domain::Network;

/// `eth_chainId` of Ethereum mainnet.
pub(crate) const ETHEREUM_CHAIN_ID: u64 = 1;
/// Bitcoin mainnet genesis block hash (`GET /block-height/0` on Esplora).
pub(crate) const BITCOIN_GENESIS_HASH: &str =
    "000000000019d6689c085ae165831e934ff763ae46a2a6c172b3f1b60a8ce26f";
/// Solana mainnet-beta genesis hash (`getGenesisHash`).
pub(crate) const SOLANA_GENESIS_HASH: &str = "5eykt4UsFv8P8NJdTREpY1vzqKqZKvdpKuc147dw2N9d";
/// Tron mainnet genesis block id (`wallet/getblockbynum {"num": 0}`).
pub(crate) const TRON_GENESIS_BLOCK_ID: &str =
    "00000000000000001ebf88508a03865c71d452e25f4d51194196a1d22b6653dc";

/// The provider answered, but for another chain. Distinguishable (via
/// `anyhow::Error::downcast_ref`) from a provider that could not be reached.
#[derive(Debug, thiserror::Error)]
#[error("{} provider is not on mainnet: expected {expected}, got {actual}", network.as_api())]
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
        assert!(err.to_string().starts_with("bitcoin provider is not on mainnet"));
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
    fn provider_answers_are_truncated_in_errors() {
        let err = expect_identity(Network::Bitcoin, BITCOIN_GENESIS_HASH, &"x".repeat(10_000)).unwrap_err();
        assert_eq!(err.actual.len(), 80);
    }
}
