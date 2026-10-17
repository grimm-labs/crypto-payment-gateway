//! Withdrawal-side vocabulary: the withdrawal status machine and the enums
//! of the outbound tables (`withdrawals`, `outbound_transactions`, `sweeps`,
//! `address_book_entries`, `alerts`, `api_keys.withdrawalPolicy`).

use serde::{Deserialize, Serialize};

use crate::DomainError;

/// `as_db` (UPPERCASE) / `as_api` (lowercase) / `from_db` / `parse_api` for
/// an enum whose two casings are listed per variant.
macro_rules! string_enum {
    ($(#[$meta:meta])* $name:ident, $error:ident { $($(#[$vmeta:meta])* $variant:ident => ($db:literal, $api:literal)),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        pub enum $name {
            $($(#[$vmeta])* $variant),+
        }

        impl $name {
            pub const ALL: &'static [Self] = &[$(Self::$variant),+];

            pub fn as_db(&self) -> &'static str {
                match self {
                    $(Self::$variant => $db),+
                }
            }

            pub fn as_api(&self) -> &'static str {
                match self {
                    $(Self::$variant => $api),+
                }
            }

            pub fn parse_api(value: &str) -> Result<Self, DomainError> {
                let value = value.to_lowercase();
                Self::ALL
                    .iter()
                    .copied()
                    .find(|v| v.as_api() == value)
                    .ok_or_else(|| DomainError::$error(value.to_string()))
            }

            pub fn from_db(value: &str) -> Result<Self, DomainError> {
                Self::ALL
                    .iter()
                    .copied()
                    .find(|v| v.as_db() == value)
                    .ok_or_else(|| DomainError::$error(value.to_string()))
            }
        }
    };
}

string_enum!(
    /// Lifecycle of a withdrawal (spec §7.3). Lightning goes
    /// `QUEUED → BROADCAST` directly (nothing is signed locally).
    WithdrawalStatus, InvalidStatus {
        /// Above the approval threshold: waits for a second admin. Funds
        /// are reserved.
        PendingApproval => ("PENDING_APPROVAL", "pending_approval"),
        /// Admitted, waiting for a worker. Funds are reserved.
        Queued => ("QUEUED", "queued"),
        /// A signed transaction is stored, not yet broadcast.
        Signed => ("SIGNED", "signed"),
        Broadcast => ("BROADCAST", "broadcast"),
        Confirmed => ("CONFIRMED", "confirmed"),
        Failed => ("FAILED", "failed"),
        Cancelled => ("CANCELLED", "cancelled"),
        /// The fee cap is reached (or an expiry cannot be proven): an admin
        /// bumps or abandons it.
        Stuck => ("STUCK", "stuck"),
    }
);

impl WithdrawalStatus {
    /// Statuses whose amount is reserved against the available balance.
    pub const RESERVING: [Self; 5] = [
        Self::PendingApproval,
        Self::Queued,
        Self::Signed,
        Self::Broadcast,
        Self::Stuck,
    ];

    /// Statuses counted in the rolling 24 h withdrawal limits.
    pub const COUNTED_IN_LIMITS: [Self; 6] = [
        Self::PendingApproval,
        Self::Queued,
        Self::Signed,
        Self::Broadcast,
        Self::Stuck,
        Self::Confirmed,
    ];

    pub fn reserves_funds(&self) -> bool {
        Self::RESERVING.contains(self)
    }

    pub fn is_final(&self) -> bool {
        matches!(self, Self::Confirmed | Self::Failed | Self::Cancelled)
    }

    /// Requester or admin may still cancel it (nothing was signed).
    pub fn is_cancellable(&self) -> bool {
        matches!(self, Self::PendingApproval | Self::Queued)
    }
}

/// What happens to a withdrawal (input of [`next_withdrawal_status`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WithdrawalEvent {
    Approve,
    Reject,
    /// The approval window (72 h) lapsed.
    ExpireApproval,
    Cancel,
    /// A transaction was signed and stored (never for Lightning).
    Sign,
    /// The network (or the Lightning SDK) accepted it.
    Broadcast,
    Confirm,
    Fail,
    /// Fee cap reached / expiry unprovable.
    Stall,
    /// A replacement went out (automatic bump, admin bump, re-sign after a
    /// proven expiry).
    Replace,
    /// Admin gives up once the network proves the tx can no longer land.
    Abandon,
}

/// The withdrawal status machine (spec §7.3). `Err` for a transition the
/// machine does not allow — callers must never write such a status.
pub fn next_withdrawal_status(
    from: WithdrawalStatus,
    event: WithdrawalEvent,
    lightning: bool,
) -> Result<WithdrawalStatus, DomainError> {
    use WithdrawalEvent as E;
    use WithdrawalStatus as S;
    let to = match (from, event) {
        (S::PendingApproval, E::Approve) => S::Queued,
        (S::PendingApproval, E::Reject | E::ExpireApproval | E::Cancel) => S::Cancelled,
        (S::Queued, E::Cancel) => S::Cancelled,
        (S::Queued, E::Sign) if !lightning => S::Signed,
        // Lightning: the SDK accepted the send, nothing was signed here.
        (S::Queued, E::Broadcast) if lightning => S::Broadcast,
        (S::Queued, E::Fail) => S::Failed,
        (S::Signed, E::Broadcast) => S::Broadcast,
        // The stored tx expired unsent (Tron/Solana): signed again.
        (S::Signed, E::Replace) => S::Signed,
        (S::Signed, E::Fail) => S::Failed,
        // Broadcast by an earlier attempt that crashed before recording it.
        (S::Signed, E::Confirm) => S::Confirmed,
        (S::Signed, E::Stall) => S::Stuck,
        (S::Broadcast, E::Confirm) => S::Confirmed,
        (S::Broadcast, E::Fail) => S::Failed,
        // No fee bumping on Lightning.
        (S::Broadcast, E::Stall) if !lightning => S::Stuck,
        (S::Broadcast, E::Replace) if !lightning => S::Broadcast,
        (S::Stuck, E::Replace) => S::Broadcast,
        (S::Stuck, E::Abandon | E::Fail) => S::Failed,
        (S::Stuck, E::Confirm) => S::Confirmed,
        _ => {
            return Err(DomainError::InvalidTransition(format!(
                "{} + {event:?}",
                from.as_api()
            )))
        }
    };
    Ok(to)
}

string_enum!(
    /// How a withdrawal destination is expressed.
    DestinationKind, InvalidDestinationKind {
        /// An on-chain address of the withdrawal's network.
        Address => ("ADDRESS", "address"),
        /// A Lightning address (`user@domain`).
        LnAddress => ("LN_ADDRESS", "ln_address"),
        /// A fixed-amount BOLT11 invoice.
        Bolt11 => ("BOLT11", "bolt11"),
    }
);

string_enum!(
    /// Why the gateway signed a transaction.
    OutboundPurpose, InvalidStatus {
        Withdrawal => ("WITHDRAWAL", "withdrawal"),
        Sweep => ("SWEEP", "sweep"),
        GasTopup => ("GAS_TOPUP", "gas_topup"),
    }
);

string_enum!(
    /// One signed transaction's state (`outbound_transactions.status`).
    OutboundTxStatus, InvalidStatus {
        /// Stored, not broadcast yet (re-broadcast at boot after 30 s).
        Signed => ("SIGNED", "signed"),
        Broadcast => ("BROADCAST", "broadcast"),
        Confirmed => ("CONFIRMED", "confirmed"),
        Failed => ("FAILED", "failed"),
        /// Superseded by a fee bump that got mined.
        Replaced => ("REPLACED", "replaced"),
        /// Proven unable to land (TAPOS / blockhash expired, absent).
        Expired => ("EXPIRED", "expired"),
    }
);

impl OutboundTxStatus {
    /// Still able to land on chain.
    pub fn is_live(&self) -> bool {
        matches!(self, Self::Signed | Self::Broadcast)
    }
}

string_enum!(
    /// A sweep of one deposit address into its treasury.
    SweepStatus, InvalidStatus {
        Planned => ("PLANNED", "planned"),
        /// A gas top-up is on its way to the deposit address.
        TopupSent => ("TOPUP_SENT", "topup_sent"),
        /// Gas is there: the sweep can be signed.
        Ready => ("READY", "ready"),
        Broadcast => ("BROADCAST", "broadcast"),
        Confirmed => ("CONFIRMED", "confirmed"),
        Failed => ("FAILED", "failed"),
        Skipped => ("SKIPPED", "skipped"),
    }
);

impl SweepStatus {
    pub fn is_final(&self) -> bool {
        matches!(self, Self::Confirmed | Self::Failed | Self::Skipped)
    }
}

string_enum!(
    /// What a merchant key may withdraw to (`api_keys.withdrawalPolicy`).
    WithdrawalPolicy, InvalidStatus {
        /// No withdrawals.
        None => ("NONE", "none"),
        /// Active address-book entries only.
        Book => ("BOOK", "book"),
        /// Any valid destination within the key's own limits.
        Open => ("OPEN", "open"),
    }
);

string_enum!(
    /// Operational alerts (`alerts.type`), also webhook events.
    AlertType, InvalidStatus {
        GasReserveLow => ("GAS_RESERVE_LOW", "gas_reserve_low"),
        TreasuryCapExceeded => ("TREASURY_CAP_EXCEEDED", "treasury_cap_exceeded"),
        AddressBookEntryAdded => ("ADDRESS_BOOK_ENTRY_ADDED", "address_book_entry_added"),
        WithdrawalStuck => ("WITHDRAWAL_STUCK", "withdrawal_stuck"),
        /// Project balances of an asset exceed the holdings (3 consecutive
        /// checks).
        LedgerShortfall => ("LEDGER_SHORTFALL", "ledger_shortfall"),
    }
);

/// Why a withdrawal was cancelled (`withdrawals.cancelReason`). An outflow
/// freeze is never a reason: frozen withdrawals wait.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CancelReason {
    Rejected,
    Expired,
    Cancelled,
}

impl CancelReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Rejected => "rejected",
            Self::Expired => "expired",
            Self::Cancelled => "cancelled",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use WithdrawalEvent as E;
    use WithdrawalStatus as S;

    fn next(from: S, event: E) -> Option<S> {
        next_withdrawal_status(from, event, false).ok()
    }

    fn next_ln(from: S, event: E) -> Option<S> {
        next_withdrawal_status(from, event, true).ok()
    }

    #[test]
    fn approval_paths() {
        assert_eq!(next(S::PendingApproval, E::Approve), Some(S::Queued));
        assert_eq!(next(S::PendingApproval, E::Reject), Some(S::Cancelled));
        assert_eq!(next(S::PendingApproval, E::ExpireApproval), Some(S::Cancelled));
        assert_eq!(next(S::PendingApproval, E::Cancel), Some(S::Cancelled));
        // Nothing is signed or sent before approval.
        for event in [E::Sign, E::Broadcast, E::Confirm, E::Stall, E::Replace, E::Abandon] {
            assert_eq!(next(S::PendingApproval, event), None, "{event:?}");
        }
        assert_eq!(next(S::Queued, E::Approve), None);
    }

    #[test]
    fn on_chain_happy_path() {
        assert_eq!(next(S::Queued, E::Sign), Some(S::Signed));
        assert_eq!(next(S::Signed, E::Broadcast), Some(S::Broadcast));
        assert_eq!(next(S::Broadcast, E::Confirm), Some(S::Confirmed));
        // On-chain withdrawals are always signed before they are broadcast.
        assert_eq!(next(S::Queued, E::Broadcast), None);
    }

    #[test]
    fn lightning_skips_signed() {
        assert_eq!(next_ln(S::Queued, E::Broadcast), Some(S::Broadcast));
        assert_eq!(next_ln(S::Queued, E::Sign), None);
        assert_eq!(next_ln(S::Broadcast, E::Confirm), Some(S::Confirmed));
        assert_eq!(next_ln(S::Broadcast, E::Fail), Some(S::Failed));
        // No fee bumping on Lightning.
        assert_eq!(next_ln(S::Broadcast, E::Stall), None);
        assert_eq!(next_ln(S::Broadcast, E::Replace), None);
    }

    #[test]
    fn cancellation_only_before_signing() {
        assert_eq!(next(S::Queued, E::Cancel), Some(S::Cancelled));
        for from in [S::Signed, S::Broadcast, S::Stuck, S::Confirmed, S::Failed, S::Cancelled] {
            assert_eq!(next(from, E::Cancel), None, "{from:?}");
            assert!(!from.is_cancellable());
        }
        assert!(S::Queued.is_cancellable() && S::PendingApproval.is_cancellable());
    }

    #[test]
    fn stuck_paths() {
        assert_eq!(next(S::Broadcast, E::Stall), Some(S::Stuck));
        assert_eq!(next(S::Broadcast, E::Replace), Some(S::Broadcast));
        assert_eq!(next(S::Stuck, E::Replace), Some(S::Broadcast));
        assert_eq!(next(S::Stuck, E::Abandon), Some(S::Failed));
        // A stuck transaction can still land, or revert.
        assert_eq!(next(S::Stuck, E::Confirm), Some(S::Confirmed));
        assert_eq!(next(S::Stuck, E::Fail), Some(S::Failed));
        // Abandon is only for stuck withdrawals.
        assert_eq!(next(S::Broadcast, E::Abandon), None);
        // A signed-but-unsent tx that expired is re-signed (stays SIGNED).
        assert_eq!(next(S::Signed, E::Replace), Some(S::Signed));
        // A signed tx the network keeps refusing is stuck too.
        assert_eq!(next(S::Signed, E::Stall), Some(S::Stuck));
    }

    #[test]
    fn a_signed_withdrawal_found_mined_is_confirmed() {
        // Crash after the broadcast, before it was recorded: tracking finds
        // the receipt while the withdrawal still reads SIGNED.
        assert_eq!(next(S::Signed, E::Confirm), Some(S::Confirmed));
    }

    #[test]
    fn failures() {
        // Before broadcast (re-check failed, insufficient funds…).
        assert_eq!(next(S::Queued, E::Fail), Some(S::Failed));
        assert_eq!(next(S::Signed, E::Fail), Some(S::Failed));
        // After broadcast (reverted, provably dropped).
        assert_eq!(next(S::Broadcast, E::Fail), Some(S::Failed));
        assert_eq!(next(S::PendingApproval, E::Fail), None);
    }

    #[test]
    fn final_statuses_never_move() {
        for from in [S::Confirmed, S::Failed, S::Cancelled] {
            assert!(from.is_final());
            for event in [
                E::Approve, E::Reject, E::ExpireApproval, E::Cancel, E::Sign, E::Broadcast,
                E::Confirm, E::Fail, E::Stall, E::Replace, E::Abandon,
            ] {
                assert_eq!(next(from, event), None, "{from:?} + {event:?}");
            }
        }
    }

    #[test]
    fn reservations_and_limits() {
        for status in S::RESERVING {
            assert!(status.reserves_funds());
            assert!(S::COUNTED_IN_LIMITS.contains(&status));
        }
        assert!(!S::Confirmed.reserves_funds());
        assert!(S::COUNTED_IN_LIMITS.contains(&S::Confirmed));
        for status in [S::Failed, S::Cancelled] {
            assert!(!status.reserves_funds());
            assert!(!S::COUNTED_IN_LIMITS.contains(&status));
        }
    }

    #[test]
    fn casings_roundtrip() {
        for status in S::ALL {
            assert_eq!(S::from_db(status.as_db()).unwrap(), *status);
            assert_eq!(S::parse_api(status.as_api()).unwrap(), *status);
        }
        assert_eq!(S::PendingApproval.as_db(), "PENDING_APPROVAL");
        assert!(S::from_db("pending_approval").is_err());
        for kind in DestinationKind::ALL {
            assert_eq!(DestinationKind::from_db(kind.as_db()).unwrap(), *kind);
            assert_eq!(DestinationKind::parse_api(kind.as_api()).unwrap(), *kind);
        }
        assert_eq!(DestinationKind::LnAddress.as_db(), "LN_ADDRESS");
        for policy in WithdrawalPolicy::ALL {
            assert_eq!(WithdrawalPolicy::parse_api(policy.as_api()).unwrap(), *policy);
        }
        assert_eq!(OutboundPurpose::GasTopup.as_db(), "GAS_TOPUP");
        assert_eq!(SweepStatus::TopupSent.as_db(), "TOPUP_SENT");
        assert_eq!(AlertType::from_db("GAS_RESERVE_LOW").unwrap(), AlertType::GasReserveLow);
        assert!(OutboundTxStatus::Signed.is_live() && !OutboundTxStatus::Expired.is_live());
    }
}
