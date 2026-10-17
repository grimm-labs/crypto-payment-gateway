//! The deprecated Lightning payout API (`POST /lightning/payments`), kept
//! one release as an alias: a payout is a Lightning withdrawal to a
//! Lightning address (spec §7.14). Legacy `payout_requests` rows stay as
//! read-only history; new sends never write them.

use domain::{Network, Token};
use serde_json::json;

use crate::withdrawals::{DestinationInput, WithdrawalInput};

/// `Sunset` of the alias (RFC 8594, HTTP-date): removed in the next release.
pub const PAYOUT_ALIAS_SUNSET: &str = "Thu, 31 Dec 2026 23:59:59 GMT";

/// The withdrawal a legacy payout request stands for.
pub fn legacy_payout_input(ln_address: &str, amount_sats: u64, comment: Option<String>) -> WithdrawalInput {
    WithdrawalInput {
        network: Network::Lightning,
        token: Token::Btc,
        amount_raw: u128::from(amount_sats),
        destination: DestinationInput::Raw(ln_address.to_string()),
        reference: None,
        comment,
        metadata: json!({ "source": "lightning_payments_alias" }),
        id_is_idempotency_key: true,
        owner: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_payout_is_a_lightning_withdrawal_in_sats() {
        let input = legacy_payout_input("alice@example.com", 150_000, Some("thanks".into()));
        assert_eq!(input.network, Network::Lightning);
        assert_eq!(input.token, Token::Btc);
        assert_eq!(input.amount_raw, 150_000);
        assert_eq!(input.destination, DestinationInput::Raw("alice@example.com".into()));
        assert_eq!(input.comment.as_deref(), Some("thanks"));
        // Same SDK key as the former flow (the Idempotency-Key).
        assert!(input.id_is_idempotency_key);
    }
}
