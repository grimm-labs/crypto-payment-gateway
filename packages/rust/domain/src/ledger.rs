//! The project balance (docs/projects-spec.md §7): a ledger figure per
//! (project, network, token), always recomputed from source rows
//! (invariant 4). The arithmetic lives here, pure; the database returns
//! the components.

use crate::Token;

/// Σ of the source rows of one (project, network, token), in base units.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BalanceComponents {
    /// `CONFIRMED` deposits (the amount actually received).
    pub deposits: u128,
    /// Unattributed funds: `CONFIRMED` sweeps on swept networks, the scan
    /// surplus (received − deposits, floored at 0) on Bitcoin.
    pub unattributed: u128,
    /// `CONFIRMED` withdrawals.
    pub withdrawn: u128,
    /// Fees of `CONFIRMED` and `FAILED` withdrawals paid in the withdrawn
    /// asset (`token = feeToken`).
    pub fees: u128,
    pub transfers_in: u128,
    pub transfers_out: u128,
    /// Σ of the reserving withdrawals: amount, plus the fee cap (or the
    /// estimate) when the fee is paid in the same asset.
    pub reserved: u128,
    /// `DETECTED` deposits — shown, never counted.
    pub pending: u128,
}

/// What a project has brought in and not taken out of one asset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProjectBalance {
    pub components: BalanceComponents,
    /// deposits + unattributed + transfers_in − withdrawn − fees −
    /// transfers_out. Negative only after admin actions (a withdrawal
    /// attributed to the project, a bumped fee cap).
    pub balance: i128,
    pub reserved: u128,
    /// balance − reserved.
    pub available: i128,
}

impl ProjectBalance {
    pub fn from_components(components: BalanceComponents) -> Self {
        let c = components;
        let credits = i128::try_from(c.deposits).unwrap_or(i128::MAX)
            .saturating_add(i128::try_from(c.unattributed).unwrap_or(i128::MAX))
            .saturating_add(i128::try_from(c.transfers_in).unwrap_or(i128::MAX));
        let debits = i128::try_from(c.withdrawn).unwrap_or(i128::MAX)
            .saturating_add(i128::try_from(c.fees).unwrap_or(i128::MAX))
            .saturating_add(i128::try_from(c.transfers_out).unwrap_or(i128::MAX));
        let balance = credits.saturating_sub(debits);
        let available = balance.saturating_sub(i128::try_from(c.reserved).unwrap_or(i128::MAX));
        Self {
            components,
            balance,
            reserved: c.reserved,
            available,
        }
    }

    /// What a withdrawal of `amount` reserves against the project balance:
    /// the amount, plus `fee_cap` when the fee is paid in the withdrawn
    /// asset (spec §7.1, §8.3).
    pub fn reservation_for(amount: u128, token: Token, fee_token: Token, fee_cap: u128) -> u128 {
        if token == fee_token {
            amount.saturating_add(fee_cap)
        } else {
            amount
        }
    }

    /// Whether `available` covers a withdrawal reserving `needed` (spec
    /// §8.3: `available ≥ amount + fee cap when same asset`).
    pub fn covers(&self, needed: u128) -> bool {
        i128::try_from(needed).map(|needed| self.available >= needed).unwrap_or(false)
    }
}

/// The reservation of one reserving withdrawal in the project ledger: its
/// amount, plus the fee cap — or the estimate when no cap was recorded —
/// when the fee is paid in the same asset.
pub fn reserved_amount(
    amount: u128,
    token: Token,
    fee_token: Token,
    fee_cap: Option<u128>,
    fee_estimate: Option<u128>,
) -> u128 {
    ProjectBalance::reservation_for(amount, token, fee_token, fee_cap.or(fee_estimate).unwrap_or(0))
}

/// Bitcoin unattributed funds of one address: what the scan saw received
/// at depth beyond its confirmed deposits, never negative (spec §7.1).
pub fn btc_unattributed(received: Option<u128>, confirmed_deposits: u128) -> u128 {
    received.unwrap_or(0).saturating_sub(confirmed_deposits)
}

/// Render a signed base-unit amount at token scale (`-` prefix when
/// negative), through the unsigned helper.
pub fn signed_raw_to_string(value: i128, token: Token) -> String {
    let magnitude = crate::raw_u128_to_string(value.unsigned_abs(), token);
    if value < 0 {
        format!("-{magnitude}")
    } else {
        magnitude
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn balance(components: BalanceComponents) -> ProjectBalance {
        ProjectBalance::from_components(components)
    }

    #[test]
    fn balance_is_credits_minus_debits_and_available_subtracts_reservations() {
        let b = balance(BalanceComponents {
            deposits: 1_000,
            unattributed: 50,
            transfers_in: 200,
            withdrawn: 300,
            fees: 10,
            transfers_out: 100,
            reserved: 400,
            pending: 999,
        });
        assert_eq!(b.balance, 840);
        assert_eq!(b.reserved, 400);
        assert_eq!(b.available, 440);
        // Pending deposits are informational only.
        assert_eq!(balance(BalanceComponents { pending: 1, ..Default::default() }).balance, 0);
    }

    #[test]
    fn balances_go_negative_after_admin_actions() {
        // An admin withdrawal attributed to a project that never earned it.
        let b = balance(BalanceComponents { withdrawn: 500, fees: 20, ..Default::default() });
        assert_eq!(b.balance, -520);
        assert_eq!(b.available, -520);
        assert!(!b.covers(1));
        // A reservation can push `available` below zero while `balance` is
        // still positive.
        let b = balance(BalanceComponents { deposits: 100, reserved: 150, ..Default::default() });
        assert_eq!(b.balance, 100);
        assert_eq!(b.available, -50);
        assert!(!b.covers(0));
    }

    #[test]
    fn the_fee_counts_only_when_paid_in_the_withdrawn_asset() {
        // ETH on Ethereum, BTC on bitcoin/lightning: amount + cap.
        assert_eq!(ProjectBalance::reservation_for(1_000, Token::Eth, Token::Eth, 30), 1_030);
        assert_eq!(ProjectBalance::reservation_for(1_000, Token::Btc, Token::Btc, 30), 1_030);
        // USDT on Ethereum pays its fee in ETH: the project is not charged.
        assert_eq!(ProjectBalance::reservation_for(1_000, Token::Usdt, Token::Eth, 30), 1_000);
        assert_eq!(ProjectBalance::reservation_for(1_000, Token::Usdt, Token::Trx, 30), 1_000);
    }

    #[test]
    fn reservations_use_the_cap_and_fall_back_to_the_estimate() {
        assert_eq!(reserved_amount(100, Token::Btc, Token::Btc, Some(30), Some(10)), 130);
        assert_eq!(reserved_amount(100, Token::Btc, Token::Btc, None, Some(10)), 110);
        assert_eq!(reserved_amount(100, Token::Btc, Token::Btc, None, None), 100);
        assert_eq!(reserved_amount(100, Token::Usdt, Token::Eth, Some(30), Some(10)), 100);
    }

    #[test]
    fn covers_compares_against_available() {
        let b = balance(BalanceComponents { deposits: 1_000, reserved: 400, ..Default::default() });
        assert!(b.covers(600));
        assert!(!b.covers(601));
        assert!(b.covers(0));
    }

    #[test]
    fn bitcoin_unattributed_is_floored_at_zero() {
        assert_eq!(btc_unattributed(Some(150_000), 100_000), 50_000);
        assert_eq!(btc_unattributed(Some(100_000), 100_000), 0);
        // A lagging scan (less than the deposits) never produces a debit.
        assert_eq!(btc_unattributed(Some(90_000), 100_000), 0);
        assert_eq!(btc_unattributed(None, 100_000), 0);
    }

    #[test]
    fn signed_rendering_keeps_the_token_scale() {
        assert_eq!(signed_raw_to_string(-25_500_000, Token::Usdt), "-25.5");
        assert_eq!(signed_raw_to_string(150_000, Token::Btc), "0.0015");
        assert_eq!(signed_raw_to_string(0, Token::Eth), "0");
    }
}
