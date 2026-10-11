//! The withdrawal rules of spec §7.4–7.5, as pure functions: who may
//! withdraw what, which destinations a caller may use, limits, the
//! available balance and approval. The admission code (and the worker's
//! re-check, invariant 6) only feeds them facts read from the database and
//! the chain.

use chrono::{DateTime, Utc};
use domain::{DestinationKind, Network, StaffRole, Token, WithdrawalPolicy};

/// Who asks for a withdrawal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Requester {
    Staff {
        id: String,
        role: StaffRole,
    },
    ApiKey {
        id: String,
        policy: WithdrawalPolicy,
        tokens: Vec<Token>,
        /// `None` = every network of the allowed tokens.
        networks: Option<Vec<Network>>,
    },
}

impl Requester {
    pub fn api_key_id(&self) -> Option<&str> {
        match self {
            Self::ApiKey { id, .. } => Some(id),
            Self::Staff { .. } => None,
        }
    }

    pub fn staff_id(&self) -> Option<&str> {
        match self {
            Self::Staff { id, .. } => Some(id),
            Self::ApiKey { .. } => None,
        }
    }
}

/// A refused withdrawal action, with a stable machine-readable code and a
/// client-safe message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuleError {
    /// 403: the caller may not do this at all.
    Forbidden(String),
    /// 422: the request breaks a rule (`code` is part of the message).
    Refused { code: &'static str, message: String },
}

impl RuleError {
    fn refused(code: &'static str, message: impl Into<String>) -> Self {
        Self::Refused {
            code,
            message: message.into(),
        }
    }

    pub fn code(&self) -> &'static str {
        match self {
            Self::Forbidden(_) => "forbidden",
            Self::Refused { code, .. } => code,
        }
    }
}

impl std::fmt::Display for RuleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Forbidden(message) => f.write_str(message),
            Self::Refused { code, message } => write!(f, "{code}: {message}"),
        }
    }
}

/// May `requester` withdraw `token` on `network` at all? Operators never
/// withdraw, gas assets are admin-only, and merchant keys need a policy
/// and the token (and network) in their grant.
pub fn authorize(requester: &Requester, network: Network, token: Token) -> Result<(), RuleError> {
    if !domain::is_withdrawable(network, token) {
        return Err(RuleError::refused(
            "unsupported_asset",
            format!("{} cannot be withdrawn on {}", token.as_api(), network.as_api()),
        ));
    }
    match requester {
        Requester::Staff { role: StaffRole::Admin, .. } => Ok(()),
        Requester::Staff { .. } => Err(RuleError::Forbidden("Admin role required".into())),
        Requester::ApiKey {
            policy,
            tokens,
            networks,
            ..
        } => {
            if *policy == WithdrawalPolicy::None {
                return Err(RuleError::Forbidden("This API key is not allowed to withdraw".into()));
            }
            if token.is_gas_only() {
                return Err(RuleError::Forbidden(format!(
                    "{} can only be withdrawn by an admin",
                    token.as_api()
                )));
            }
            if !tokens.contains(&token) {
                return Err(RuleError::Forbidden(format!(
                    "This API key may not withdraw {}",
                    token.as_api()
                )));
            }
            if networks.as_ref().is_some_and(|networks| !networks.contains(&network)) {
                return Err(RuleError::Forbidden(format!(
                    "This API key may not withdraw on {}",
                    network.as_api()
                )));
            }
            Ok(())
        }
    }
}

/// `approvalRequired = amount ≥ threshold`, whoever asks.
pub fn approval_required(amount: u128, threshold: u128) -> bool {
    amount >= threshold
}

/// The destination policy matrix (spec §7.4): admins may send anywhere below
/// the approval threshold and only to an active book entry at or above it;
/// `BOOK` keys always need an entry; `OPEN` keys may send anywhere (their
/// own limits bound them). BOLT11 invoices are never book entries.
pub fn check_destination_rule(
    requester: &Requester,
    approval_required: bool,
    uses_book_entry: bool,
    kind: DestinationKind,
) -> Result<(), RuleError> {
    if uses_book_entry && kind == DestinationKind::Bolt11 {
        return Err(RuleError::refused(
            "invalid_destination",
            "BOLT11 invoices cannot be address-book entries",
        ));
    }
    let needs_book = match requester {
        Requester::Staff { .. } => approval_required,
        Requester::ApiKey { policy, .. } => *policy == WithdrawalPolicy::Book,
    };
    if needs_book && !uses_book_entry {
        return Err(if approval_required && matches!(requester, Requester::Staff { .. }) {
            RuleError::refused(
                "approval_threshold_requires_address_book",
                "At or above the approval threshold, withdrawals go to an active address-book entry",
            )
        } else {
            RuleError::refused(
                "destination_requires_address_book",
                "This API key may only withdraw to active address-book entries",
            )
        });
    }
    Ok(())
}

/// Which limit a check is against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitScope {
    /// `asset_settings` (every caller, every network of the asset).
    Asset,
    /// `api_key_withdrawal_limits` of the calling key.
    ApiKey,
}

/// Per-withdrawal and rolling-24 h limits. `0` disables withdrawals of the
/// asset (or for the key).
pub fn check_limit(
    scope: LimitScope,
    token: Token,
    amount: u128,
    max_per_withdrawal: u128,
    daily_max: u128,
    used_last_24h: u128,
) -> Result<(), RuleError> {
    let (code, who) = match scope {
        LimitScope::Asset => ("limit_exceeded", ""),
        LimitScope::ApiKey => ("key_limit_exceeded", " for this API key"),
    };
    let render = |raw: u128| format!("{} {}", domain::raw_u128_to_string(raw, token), token.as_api());
    if max_per_withdrawal == 0 || daily_max == 0 {
        return Err(RuleError::refused(
            "withdrawals_disabled",
            format!("Withdrawals of {} are disabled{who} (limit set to 0)", token.as_api()),
        ));
    }
    if amount > max_per_withdrawal {
        return Err(RuleError::refused(
            code,
            format!("Amount exceeds the per-withdrawal limit{who} of {}", render(max_per_withdrawal)),
        ));
    }
    if used_last_24h.saturating_add(amount) > daily_max {
        return Err(RuleError::refused(
            code,
            format!(
                "Daily limit{who} reached: {} of {} already withdrawn or reserved in the last 24 h",
                render(used_last_24h),
                render(daily_max)
            ),
        ));
    }
    Ok(())
}

/// What one (network, token) of a treasury can still send.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Balance {
    /// Confirmed spendable balance (at the confirmation depth).
    pub confirmed: u128,
    /// Σ amounts of reserving withdrawals of this (network, token).
    pub reserved: u128,
    /// Σ estimated fees of reserving withdrawals paid in this token (gas
    /// token only).
    pub reserved_fees: u128,
    /// `gasReserveFloorRaw` (gas token only).
    pub floor: u128,
}

impl Balance {
    /// Confirmed − reservations − fees reserved in it − the gas floor.
    pub fn available(&self) -> u128 {
        self.confirmed
            .saturating_sub(self.reserved)
            .saturating_sub(self.reserved_fees)
            .saturating_sub(self.floor)
    }
}

/// Funds check: `amount` of the token and `fee` of the network's fee
/// token. When both are the same asset they come out of the same balance.
pub fn check_funds(
    token: Token,
    amount: u128,
    token_balance: &Balance,
    fee: u128,
    fee_balance: Option<&Balance>,
) -> Result<(), RuleError> {
    let insufficient = |what: &str| {
        RuleError::refused(
            "insufficient_funds",
            format!("Insufficient {what} available for this withdrawal"),
        )
    };
    match fee_balance {
        // Fee in the same asset (ETH, TRX, SOL, BTC): amount + fee.
        None => {
            if amount.saturating_add(fee) > token_balance.available() {
                return Err(insufficient(token.as_api()));
            }
        }
        Some(fee_balance) => {
            if amount > token_balance.available() {
                return Err(insufficient(token.as_api()));
            }
            if fee > fee_balance.available() {
                return Err(insufficient("gas reserve"));
            }
        }
    }
    Ok(())
}

/// The fee a withdrawal may end up paying: 3 × its quote (at least 1 base
/// unit, so a zero quote never forbids every fee).
pub fn fee_cap(quote: u128) -> u128 {
    quote.saturating_mul(config::WITHDRAWAL_FEE_CAP_MULTIPLIER).max(1)
}

/// What a pending approval allows `approver_id` (an active admin) to do
/// at `now`.
pub fn check_approver(
    requested_by_staff: Option<&str>,
    approver_id: &str,
    self_approval_not_before: Option<DateTime<Utc>>,
    approval_expires_at: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> Result<(), RuleError> {
    if approval_expires_at.is_some_and(|deadline| now > deadline) {
        return Err(RuleError::refused("approval_expired", "This approval request has expired"));
    }
    if requested_by_staff == Some(approver_id) {
        return match self_approval_not_before {
            Some(not_before) if now >= not_before => Ok(()),
            Some(not_before) => Err(RuleError::Forbidden(format!(
                "As the only active admin you may approve your own withdrawal after {}",
                not_before.to_rfc3339()
            ))),
            None => Err(RuleError::Forbidden(
                "Another admin must approve this withdrawal".into(),
            )),
        };
    }
    Ok(())
}

/// SHA-256 of the normalized parameters — what a replayed Idempotency-Key
/// must match.
pub fn params_hash(
    network: Network,
    token: Token,
    destination: &str,
    amount_raw: u128,
    comment: Option<&str>,
) -> String {
    auth::sha256_hex(&format!(
        "{}\n{}\n{destination}\n{amount_raw}\n{}",
        network.as_db(),
        token.as_db(),
        comment.unwrap_or_default()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn admin() -> Requester {
        Requester::Staff { id: "s1".into(), role: StaffRole::Admin }
    }

    fn key(policy: WithdrawalPolicy, tokens: &[Token], networks: Option<&[Network]>) -> Requester {
        Requester::ApiKey {
            id: "k1".into(),
            policy,
            tokens: tokens.to_vec(),
            networks: networks.map(<[Network]>::to_vec),
        }
    }

    #[test]
    fn operators_never_withdraw_and_gas_assets_are_admin_only() {
        let operator = Requester::Staff { id: "s2".into(), role: StaffRole::Operator };
        assert!(matches!(authorize(&operator, Network::Bitcoin, Token::Btc), Err(RuleError::Forbidden(_))));
        assert!(authorize(&admin(), Network::Tron, Token::Trx).is_ok());
        assert!(authorize(&admin(), Network::Solana, Token::Sol).is_ok());
        let open = key(WithdrawalPolicy::Open, &[Token::Usdt, Token::Btc, Token::Eth], None);
        assert!(matches!(authorize(&open, Network::Tron, Token::Trx), Err(RuleError::Forbidden(_))));
        assert!(authorize(&open, Network::Ethereum, Token::Eth).is_ok());
    }

    #[test]
    fn keys_need_a_policy_the_token_and_the_network() {
        let none = key(WithdrawalPolicy::None, &[Token::Usdt], None);
        assert!(matches!(authorize(&none, Network::Tron, Token::Usdt), Err(RuleError::Forbidden(_))));
        let usdt_only = key(WithdrawalPolicy::Book, &[Token::Usdt], None);
        assert!(authorize(&usdt_only, Network::Solana, Token::Usdt).is_ok());
        assert!(matches!(authorize(&usdt_only, Network::Bitcoin, Token::Btc), Err(RuleError::Forbidden(_))));
        // The migrated Lightning-only scope: BTC, but not on chain.
        let lightning = key(WithdrawalPolicy::Open, &[Token::Btc], Some(&[Network::Lightning]));
        assert!(authorize(&lightning, Network::Lightning, Token::Btc).is_ok());
        assert!(matches!(authorize(&lightning, Network::Bitcoin, Token::Btc), Err(RuleError::Forbidden(_))));
        // An asset the network does not carry.
        assert_eq!(authorize(&admin(), Network::Tron, Token::Eth).unwrap_err().code(), "unsupported_asset");
    }

    #[test]
    fn destination_matrix() {
        use DestinationKind::*;
        let book = key(WithdrawalPolicy::Book, &[Token::Usdt], None);
        let open = key(WithdrawalPolicy::Open, &[Token::Usdt], None);
        // Admin: free below the threshold, book entry at/above.
        assert!(check_destination_rule(&admin(), false, false, Address).is_ok());
        assert_eq!(
            check_destination_rule(&admin(), true, false, Address).unwrap_err().code(),
            "approval_threshold_requires_address_book"
        );
        assert!(check_destination_rule(&admin(), true, true, Address).is_ok());
        // BOOK keys: always a book entry.
        assert_eq!(
            check_destination_rule(&book, false, false, Address).unwrap_err().code(),
            "destination_requires_address_book"
        );
        assert!(check_destination_rule(&book, true, true, LnAddress).is_ok());
        // OPEN keys: anywhere, approval or not.
        assert!(check_destination_rule(&open, true, false, Address).is_ok());
        assert!(check_destination_rule(&open, false, false, Bolt11).is_ok());
        // BOLT11 only where "any destination" applies.
        assert_eq!(check_destination_rule(&book, false, false, Bolt11).unwrap_err().code(), "destination_requires_address_book");
        assert_eq!(check_destination_rule(&admin(), false, true, Bolt11).unwrap_err().code(), "invalid_destination");
    }

    #[test]
    fn approval_threshold_is_inclusive() {
        assert!(approval_required(5_000, 5_000));
        assert!(!approval_required(4_999, 5_000));
    }

    #[test]
    fn limits() {
        assert!(check_limit(LimitScope::Asset, Token::Usdt, 100, 100, 1_000, 900).is_ok());
        assert_eq!(check_limit(LimitScope::Asset, Token::Usdt, 101, 100, 1_000, 0).unwrap_err().code(), "limit_exceeded");
        assert_eq!(check_limit(LimitScope::Asset, Token::Usdt, 100, 100, 1_000, 901).unwrap_err().code(), "limit_exceeded");
        assert_eq!(check_limit(LimitScope::ApiKey, Token::Btc, 1, 100, 1_000, 1_000).unwrap_err().code(), "key_limit_exceeded");
        assert_eq!(check_limit(LimitScope::Asset, Token::Eth, 1, 0, 1_000, 0).unwrap_err().code(), "withdrawals_disabled");
        assert_eq!(check_limit(LimitScope::ApiKey, Token::Eth, 1, 10, 0, 0).unwrap_err().code(), "withdrawals_disabled");
        // Messages render the token amount, not base units.
        let err = check_limit(LimitScope::Asset, Token::Usdt, 20_000_000_000, 10_000_000_000, 50_000_000_000, 0).unwrap_err();
        assert!(err.to_string().contains("10000 USDT"), "{err}");
    }

    #[test]
    fn available_balance_subtracts_reservations_fees_and_the_gas_floor() {
        let eth = Balance { confirmed: 10_000, reserved: 2_000, reserved_fees: 300, floor: 500 };
        assert_eq!(eth.available(), 7_200);
        let empty = Balance { confirmed: 100, reserved: 200, ..Balance::default() };
        assert_eq!(empty.available(), 0);
    }

    #[test]
    fn funds_check_counts_the_fee_in_its_own_asset() {
        let usdt = Balance { confirmed: 1_000, reserved: 400, ..Balance::default() };
        let eth = Balance { confirmed: 100, reserved_fees: 40, floor: 50, ..Balance::default() };
        // 600 USDT available; 10 ETH of gas available.
        assert!(check_funds(Token::Usdt, 600, &usdt, 10, Some(&eth)).is_ok());
        assert_eq!(check_funds(Token::Usdt, 601, &usdt, 10, Some(&eth)).unwrap_err().code(), "insufficient_funds");
        assert_eq!(check_funds(Token::Usdt, 600, &usdt, 11, Some(&eth)).unwrap_err().code(), "insufficient_funds");
        // ETH withdrawal: amount + fee from the same balance.
        let eth = Balance { confirmed: 1_000, ..Balance::default() };
        assert!(check_funds(Token::Eth, 990, &eth, 10, None).is_ok());
        assert!(check_funds(Token::Eth, 991, &eth, 10, None).is_err());
    }

    #[test]
    fn approval_rules() {
        let now = Utc::now();
        let later = now + Duration::hours(1);
        // Another admin may approve.
        assert!(check_approver(Some("s1"), "s2", None, Some(later), now).is_ok());
        // Merchant-requested: any admin.
        assert!(check_approver(None, "s2", None, Some(later), now).is_ok());
        // The requester only when single-admin and after the delay.
        assert!(matches!(check_approver(Some("s1"), "s1", None, Some(later), now), Err(RuleError::Forbidden(_))));
        assert!(matches!(
            check_approver(Some("s1"), "s1", Some(now + Duration::minutes(5)), Some(later), now),
            Err(RuleError::Forbidden(_))
        ));
        assert!(check_approver(Some("s1"), "s1", Some(now - Duration::seconds(1)), Some(later), now).is_ok());
        // Expired.
        assert_eq!(
            check_approver(Some("s1"), "s2", None, Some(now - Duration::seconds(1)), now).unwrap_err().code(),
            "approval_expired"
        );
    }

    #[test]
    fn params_hash_covers_every_parameter() {
        let base = params_hash(Network::Ethereum, Token::Usdt, "0xA", 1, None);
        assert_eq!(base, params_hash(Network::Ethereum, Token::Usdt, "0xA", 1, None));
        assert_ne!(base, params_hash(Network::Tron, Token::Usdt, "0xA", 1, None));
        assert_ne!(base, params_hash(Network::Ethereum, Token::Eth, "0xA", 1, None));
        assert_ne!(base, params_hash(Network::Ethereum, Token::Usdt, "0xB", 1, None));
        assert_ne!(base, params_hash(Network::Ethereum, Token::Usdt, "0xA", 2, None));
        assert_ne!(base, params_hash(Network::Ethereum, Token::Usdt, "0xA", 1, Some("x")));
    }

    #[test]
    fn fee_cap_is_three_quotes() {
        assert_eq!(fee_cap(1_000), 3_000);
        assert_eq!(fee_cap(0), 1);
        assert_eq!(fee_cap(u128::MAX), u128::MAX);
    }
}
