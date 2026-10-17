use rust_decimal::{Decimal, RoundingStrategy};
use serde::{Deserialize, Serialize};
use thiserror::Error;

mod ledger;
mod project;
mod withdrawal;

pub use ledger::*;
pub use project::*;
pub use withdrawal::*;

pub const USDT_DECIMALS: u32 = 6;
/// 1 BTC = 100 000 000 sats.
pub const BTC_DECIMALS: u32 = 8;
/// 1 ETH = 10^18 wei.
pub const ETH_DECIMALS: u32 = 18;
/// 1 TRX = 1 000 000 sun.
pub const TRX_DECIMALS: u32 = 6;
/// 1 SOL = 10^9 lamports.
pub const SOL_DECIMALS: u32 = 9;
/// Total bitcoin supply in sats — the upper bound of any BTC amount.
pub const BTC_MAX_SATS: u64 = 2_100_000_000_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Network {
    Tron,
    Solana,
    Ethereum,
    Bitcoin,
    /// BOLT11 invoices issued through the Breez SDK; the row's `address`
    /// holds the payment request.
    Lightning,
}

impl Network {
    pub const ALL: [Self; 5] = [
        Self::Tron,
        Self::Ethereum,
        Self::Solana,
        Self::Bitcoin,
        Self::Lightning,
    ];

    /// Tokens accepted as payments on this network (see `supports`).
    pub fn payment_tokens(&self) -> &'static [Token] {
        match self {
            Self::Tron | Self::Solana => &[Token::Usdt],
            Self::Ethereum => &[Token::Usdt, Token::Eth],
            Self::Bitcoin | Self::Lightning => &[Token::Btc],
        }
    }

    /// The native coin paying this network's fees; Lightning has none (its
    /// fees are paid in BTC from the Spark balance, see `fee_token`).
    pub fn gas_token(&self) -> Option<Token> {
        match self {
            Self::Ethereum => Some(Token::Eth),
            Self::Tron => Some(Token::Trx),
            Self::Solana => Some(Token::Sol),
            Self::Bitcoin => Some(Token::Btc),
            Self::Lightning => None,
        }
    }

    /// What a withdrawal's fee is paid in.
    pub fn fee_token(&self) -> Token {
        self.gas_token().unwrap_or(Token::Btc)
    }

    /// Tokens that can leave the gateway on this network: the payment tokens
    /// plus the gas token, deduplicated.
    pub fn withdrawable_tokens(&self) -> &'static [Token] {
        match self {
            Self::Tron => &[Token::Usdt, Token::Trx],
            Self::Ethereum => &[Token::Usdt, Token::Eth],
            Self::Solana => &[Token::Usdt, Token::Sol],
            Self::Bitcoin | Self::Lightning => &[Token::Btc],
        }
    }

    pub fn as_db(&self) -> &'static str {
        match self {
            Self::Tron => "TRON",
            Self::Solana => "SOLANA",
            Self::Ethereum => "ETHEREUM",
            Self::Bitcoin => "BITCOIN",
            Self::Lightning => "LIGHTNING",
        }
    }

    pub fn as_api(&self) -> &'static str {
        match self {
            Self::Tron => "tron",
            Self::Solana => "solana",
            Self::Ethereum => "ethereum",
            Self::Bitcoin => "bitcoin",
            Self::Lightning => "lightning",
        }
    }

    pub fn parse_api(value: &str) -> Result<Self, DomainError> {
        match value.to_lowercase().as_str() {
            "tron" => Ok(Self::Tron),
            "solana" => Ok(Self::Solana),
            "ethereum" => Ok(Self::Ethereum),
            "bitcoin" => Ok(Self::Bitcoin),
            "lightning" => Ok(Self::Lightning),
            _ => Err(DomainError::UnsupportedNetwork(value.to_string())),
        }
    }

    pub fn from_db(value: &str) -> Result<Self, DomainError> {
        match value {
            "TRON" => Ok(Self::Tron),
            "SOLANA" => Ok(Self::Solana),
            "ETHEREUM" => Ok(Self::Ethereum),
            "BITCOIN" => Ok(Self::Bitcoin),
            "LIGHTNING" => Ok(Self::Lightning),
            _ => Err(DomainError::UnsupportedNetwork(value.to_string())),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Token {
    Usdt,
    Btc,
    Eth,
    /// Gas asset only (Tron fees): never accepted as a payment.
    Trx,
    /// Gas asset only (Solana fees): never accepted as a payment.
    Sol,
}

impl Token {
    pub const ALL: [Self; 5] = [Self::Usdt, Self::Btc, Self::Eth, Self::Trx, Self::Sol];

    pub fn as_db(&self) -> &'static str {
        match self {
            Self::Usdt => "USDT",
            Self::Btc => "BTC",
            Self::Eth => "ETH",
            Self::Trx => "TRX",
            Self::Sol => "SOL",
        }
    }

    /// Token strings are uppercase on the API surface too.
    pub fn as_api(&self) -> &'static str {
        self.as_db()
    }

    pub fn parse_api(value: &str) -> Result<Self, DomainError> {
        Self::ALL
            .into_iter()
            .find(|token| value.eq_ignore_ascii_case(token.as_db()))
            .ok_or_else(|| DomainError::UnsupportedToken(value.to_string()))
    }

    pub fn from_db(value: &str) -> Result<Self, DomainError> {
        Self::parse_api(value)
    }

    /// Base-unit exponent: `amount_raw = amount * 10^decimals`.
    pub fn decimals(&self) -> u32 {
        match self {
            Self::Usdt => USDT_DECIMALS,
            Self::Btc => BTC_DECIMALS,
            Self::Eth => ETH_DECIMALS,
            Self::Trx => TRX_DECIMALS,
            Self::Sol => SOL_DECIMALS,
        }
    }

    /// A gas asset held only to pay fees: withdrawable by admins, never a
    /// payment.
    pub fn is_gas_only(&self) -> bool {
        matches!(self, Self::Trx | Self::Sol)
    }
}

/// Whether `token` can be received on `network`: USDT on the three account
/// networks, native ETH on Ethereum, BTC on bitcoin and lightning. Gas-only
/// assets (TRX, SOL) are never payments.
pub fn supports(network: Network, token: Token) -> bool {
    network.payment_tokens().contains(&token)
}

/// Whether `token` can be withdrawn on `network` (payment tokens + the gas
/// token).
pub fn is_withdrawable(network: Network, token: Token) -> bool {
    network.withdrawable_tokens().contains(&token)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DepositAddressStatus {
    Pending,
    Paid,
    Expired,
}

impl DepositAddressStatus {
    pub fn as_db(&self) -> &'static str {
        match self {
            Self::Pending => "PENDING",
            Self::Paid => "PAID",
            Self::Expired => "EXPIRED",
        }
    }

    pub fn as_api(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Paid => "paid",
            Self::Expired => "expired",
        }
    }

    pub fn parse_api(value: &str) -> Result<Self, DomainError> {
        match value.to_lowercase().as_str() {
            "pending" => Ok(Self::Pending),
            "paid" => Ok(Self::Paid),
            "expired" => Ok(Self::Expired),
            _ => Err(DomainError::InvalidStatus(value.to_string())),
        }
    }

    pub fn from_db(value: &str) -> Result<Self, DomainError> {
        match value {
            "PENDING" => Ok(Self::Pending),
            "PAID" => Ok(Self::Paid),
            "EXPIRED" => Ok(Self::Expired),
            _ => Err(DomainError::InvalidStatus(value.to_string())),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DepositStatus {
    Detected,
    Confirmed,
}

impl DepositStatus {
    pub fn as_db(&self) -> &'static str {
        match self {
            Self::Detected => "DETECTED",
            Self::Confirmed => "CONFIRMED",
        }
    }

    pub fn as_api(&self) -> &'static str {
        match self {
            Self::Detected => "detected",
            Self::Confirmed => "confirmed",
        }
    }

    pub fn from_db(value: &str) -> Result<Self, DomainError> {
        match value {
            "DETECTED" => Ok(Self::Detected),
            "CONFIRMED" => Ok(Self::Confirmed),
            _ => Err(DomainError::InvalidStatus(value.to_string())),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WebhookDeliveryStatus {
    Pending,
    Delivered,
    Failed,
}

impl WebhookDeliveryStatus {
    pub fn as_db(&self) -> &'static str {
        match self {
            Self::Pending => "PENDING",
            Self::Delivered => "DELIVERED",
            Self::Failed => "FAILED",
        }
    }

    pub fn as_api(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Delivered => "delivered",
            Self::Failed => "failed",
        }
    }

    pub fn parse_api(value: &str) -> Result<Self, DomainError> {
        match value.to_lowercase().as_str() {
            "pending" => Ok(Self::Pending),
            "delivered" => Ok(Self::Delivered),
            "failed" => Ok(Self::Failed),
            _ => Err(DomainError::InvalidStatus(value.to_string())),
        }
    }

    pub fn from_db(value: &str) -> Result<Self, DomainError> {
        match value {
            "PENDING" => Ok(Self::Pending),
            "DELIVERED" => Ok(Self::Delivered),
            "FAILED" => Ok(Self::Failed),
            _ => Err(DomainError::InvalidStatus(value.to_string())),
        }
    }
}

/// Event a webhook endpoint can subscribe to. Address lifecycle events mirror
/// `DepositAddressStatus`: PENDING fires at address creation, PAID at deposit
/// confirmation, EXPIRED when the address expires unpaid. Payout events
/// report the final outcome of Lightning wallet sends the gateway did not
/// make as a withdrawal; withdrawal events follow a withdrawal's lifecycle;
/// alert events report operational conditions (gas, caps, address book,
/// stuck withdrawals).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WebhookEventType {
    Pending,
    Paid,
    Expired,
    PayoutSucceeded,
    PayoutFailed,
    WithdrawalPendingApproval,
    WithdrawalBroadcast,
    WithdrawalConfirmed,
    WithdrawalFailed,
    WithdrawalCancelled,
    GasReserveLow,
    TreasuryCapExceeded,
    AddressBookEntryAdded,
    WithdrawalStuck,
    /// An internal transfer between two owners (both sides are notified).
    TransferCompleted,
    /// The project balances of an asset exceed what the gateway holds.
    LedgerShortfall,
}

/// What a webhook delivery is about — decides which `webhook_deliveries`
/// column holds its subject id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebhookSubject {
    /// `deposits.id` (PAID).
    Deposit,
    /// `deposit_addresses.id` (PENDING / EXPIRED).
    DepositAddress,
    /// SDK payment id (payout events).
    LightningPayment,
    /// `withdrawals.id`.
    Withdrawal,
    /// `alerts.id`.
    Alert,
    /// `internal_transfers.id`.
    Transfer,
}

impl WebhookEventType {
    pub const ALL: [Self; 16] = [
        Self::Pending,
        Self::Paid,
        Self::Expired,
        Self::PayoutSucceeded,
        Self::PayoutFailed,
        Self::WithdrawalPendingApproval,
        Self::WithdrawalBroadcast,
        Self::WithdrawalConfirmed,
        Self::WithdrawalFailed,
        Self::WithdrawalCancelled,
        Self::GasReserveLow,
        Self::TreasuryCapExceeded,
        Self::AddressBookEntryAdded,
        Self::WithdrawalStuck,
        Self::TransferCompleted,
        Self::LedgerShortfall,
    ];

    pub fn as_db(&self) -> &'static str {
        match self {
            Self::Pending => "PENDING",
            Self::Paid => "PAID",
            Self::Expired => "EXPIRED",
            Self::PayoutSucceeded => "PAYOUT_SUCCEEDED",
            Self::PayoutFailed => "PAYOUT_FAILED",
            Self::WithdrawalPendingApproval => "WITHDRAWAL_PENDING_APPROVAL",
            Self::WithdrawalBroadcast => "WITHDRAWAL_BROADCAST",
            Self::WithdrawalConfirmed => "WITHDRAWAL_CONFIRMED",
            Self::WithdrawalFailed => "WITHDRAWAL_FAILED",
            Self::WithdrawalCancelled => "WITHDRAWAL_CANCELLED",
            Self::GasReserveLow => "GAS_RESERVE_LOW",
            Self::TreasuryCapExceeded => "TREASURY_CAP_EXCEEDED",
            Self::AddressBookEntryAdded => "ADDRESS_BOOK_ENTRY_ADDED",
            Self::WithdrawalStuck => "WITHDRAWAL_STUCK",
            Self::TransferCompleted => "TRANSFER_COMPLETED",
            Self::LedgerShortfall => "LEDGER_SHORTFALL",
        }
    }

    pub fn as_api(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Paid => "paid",
            Self::Expired => "expired",
            Self::PayoutSucceeded => "payout_succeeded",
            Self::PayoutFailed => "payout_failed",
            Self::WithdrawalPendingApproval => "withdrawal_pending_approval",
            Self::WithdrawalBroadcast => "withdrawal_broadcast",
            Self::WithdrawalConfirmed => "withdrawal_confirmed",
            Self::WithdrawalFailed => "withdrawal_failed",
            Self::WithdrawalCancelled => "withdrawal_cancelled",
            Self::GasReserveLow => "gas_reserve_low",
            Self::TreasuryCapExceeded => "treasury_cap_exceeded",
            Self::AddressBookEntryAdded => "address_book_entry_added",
            Self::WithdrawalStuck => "withdrawal_stuck",
            Self::TransferCompleted => "transfer_completed",
            Self::LedgerShortfall => "ledger_shortfall",
        }
    }

    /// `event_type` value sent in the webhook payload. `deposit_confirmed`
    /// predates event subscriptions and must not change (existing receivers).
    pub fn payload_event_type(&self) -> &'static str {
        match self {
            Self::Pending => "payment_pending",
            Self::Paid => "deposit_confirmed",
            Self::Expired => "payment_expired",
            other => other.as_api(),
        }
    }

    pub fn subject(&self) -> WebhookSubject {
        match self {
            Self::Paid => WebhookSubject::Deposit,
            Self::Pending | Self::Expired => WebhookSubject::DepositAddress,
            Self::PayoutSucceeded | Self::PayoutFailed => WebhookSubject::LightningPayment,
            Self::WithdrawalPendingApproval
            | Self::WithdrawalBroadcast
            | Self::WithdrawalConfirmed
            | Self::WithdrawalFailed
            | Self::WithdrawalCancelled => WebhookSubject::Withdrawal,
            Self::GasReserveLow
            | Self::TreasuryCapExceeded
            | Self::AddressBookEntryAdded
            | Self::WithdrawalStuck
            | Self::LedgerShortfall => WebhookSubject::Alert,
            Self::TransferCompleted => WebhookSubject::Transfer,
        }
    }

    /// Payout events are keyed on a Lightning wallet payment id, not on a
    /// deposit address.
    pub fn is_payout(&self) -> bool {
        self.subject() == WebhookSubject::LightningPayment
    }

    /// The webhook an alert of this type fires.
    pub fn for_alert(alert: AlertType) -> Self {
        match alert {
            AlertType::GasReserveLow => Self::GasReserveLow,
            AlertType::TreasuryCapExceeded => Self::TreasuryCapExceeded,
            AlertType::AddressBookEntryAdded => Self::AddressBookEntryAdded,
            AlertType::WithdrawalStuck => Self::WithdrawalStuck,
            AlertType::LedgerShortfall => Self::LedgerShortfall,
        }
    }

    pub fn parse_api(value: &str) -> Result<Self, DomainError> {
        let value = value.to_lowercase();
        Self::ALL
            .into_iter()
            .find(|event| event.as_api() == value)
            .ok_or(DomainError::InvalidStatus(value))
    }

    pub fn from_db(value: &str) -> Result<Self, DomainError> {
        Self::ALL
            .into_iter()
            .find(|event| event.as_db() == value)
            .ok_or_else(|| DomainError::InvalidStatus(value.to_string()))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ApiKeyStatus {
    Active,
    Revoked,
    Expired,
}

impl ApiKeyStatus {
    pub fn as_db(&self) -> &'static str {
        match self {
            Self::Active => "ACTIVE",
            Self::Revoked => "REVOKED",
            Self::Expired => "EXPIRED",
        }
    }

    pub fn as_api(&self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Revoked => "revoked",
            Self::Expired => "expired",
        }
    }

    pub fn parse_api(value: &str) -> Result<Self, DomainError> {
        match value.to_lowercase().as_str() {
            "active" => Ok(Self::Active),
            "revoked" => Ok(Self::Revoked),
            "expired" => Ok(Self::Expired),
            _ => Err(DomainError::InvalidStatus(value.to_string())),
        }
    }

    pub fn from_db(value: &str) -> Result<Self, DomainError> {
        match value {
            "ACTIVE" => Ok(Self::Active),
            "REVOKED" => Ok(Self::Revoked),
            "EXPIRED" => Ok(Self::Expired),
            _ => Err(DomainError::InvalidStatus(value.to_string())),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StaffRole {
    Admin,
    Operator,
}

impl StaffRole {
    pub fn as_db(&self) -> &'static str {
        match self {
            Self::Admin => "ADMIN",
            Self::Operator => "OPERATOR",
        }
    }

    pub fn as_api(&self) -> &'static str {
        match self {
            Self::Admin => "admin",
            Self::Operator => "operator",
        }
    }

    pub fn parse_api(value: &str) -> Result<Self, DomainError> {
        match value.to_lowercase().as_str() {
            "admin" => Ok(Self::Admin),
            "operator" => Ok(Self::Operator),
            _ => Err(DomainError::InvalidRole(value.to_string())),
        }
    }

    pub fn from_db(value: &str) -> Result<Self, DomainError> {
        match value {
            "ADMIN" => Ok(Self::Admin),
            "OPERATOR" => Ok(Self::Operator),
            _ => Err(DomainError::InvalidRole(value.to_string())),
        }
    }
}

/// Which public HTTP surface a recorded request hit. Used by the traffic
/// log so the dashboard's own polling can be told apart from merchant calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ApiSurface {
    Merchant,
    Admin,
}

impl ApiSurface {
    pub fn as_db(&self) -> &'static str {
        match self {
            Self::Merchant => "MERCHANT",
            Self::Admin => "ADMIN",
        }
    }

    pub fn as_api(&self) -> &'static str {
        match self {
            Self::Merchant => "merchant",
            Self::Admin => "admin",
        }
    }

    pub fn parse_api(value: &str) -> Result<Self, DomainError> {
        match value.to_lowercase().as_str() {
            "merchant" => Ok(Self::Merchant),
            "admin" => Ok(Self::Admin),
            _ => Err(DomainError::InvalidSurface(value.to_string())),
        }
    }

    pub fn from_db(value: &str) -> Result<Self, DomainError> {
        match value {
            "MERCHANT" => Ok(Self::Merchant),
            "ADMIN" => Ok(Self::Admin),
            _ => Err(DomainError::InvalidSurface(value.to_string())),
        }
    }

    /// Classify a request path. `None` for anything that is not a public
    /// API route (health checks, unknown prefixes) — those are not recorded.
    pub fn from_path(path: &str) -> Option<Self> {
        if path == "/api/admin/v1" || path.starts_with("/api/admin/v1/") {
            Some(Self::Admin)
        } else if path == "/api/v1" || path.starts_with("/api/v1/") {
            Some(Self::Merchant)
        } else {
            None
        }
    }
}

/// HTTP status class used to filter the traffic log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusClass {
    Success,
    Redirect,
    ClientError,
    ServerError,
}

impl StatusClass {
    pub fn as_api(&self) -> &'static str {
        match self {
            Self::Success => "2xx",
            Self::Redirect => "3xx",
            Self::ClientError => "4xx",
            Self::ServerError => "5xx",
        }
    }

    pub fn parse_api(value: &str) -> Result<Self, DomainError> {
        match value.to_lowercase().as_str() {
            "2xx" => Ok(Self::Success),
            "3xx" => Ok(Self::Redirect),
            "4xx" => Ok(Self::ClientError),
            "5xx" => Ok(Self::ServerError),
            _ => Err(DomainError::InvalidStatus(value.to_string())),
        }
    }

    /// Inclusive status-code bounds of the class.
    pub fn range(&self) -> (i16, i16) {
        match self {
            Self::Success => (200, 299),
            Self::Redirect => (300, 399),
            Self::ClientError => (400, 499),
            Self::ServerError => (500, 599),
        }
    }

    pub fn of(status: u16) -> Option<Self> {
        match status {
            200..=299 => Some(Self::Success),
            300..=399 => Some(Self::Redirect),
            400..=499 => Some(Self::ClientError),
            500..=599 => Some(Self::ServerError),
            _ => None,
        }
    }
}

/// Trailing time window of the traffic overview, with its chart bucket size
/// (30 / 60 / 96 points respectively).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrafficWindow {
    M15,
    H1,
    H24,
}

impl TrafficWindow {
    pub fn as_api(&self) -> &'static str {
        match self {
            Self::M15 => "15m",
            Self::H1 => "1h",
            Self::H24 => "24h",
        }
    }

    pub fn parse_api(value: &str) -> Result<Self, DomainError> {
        match value.to_lowercase().as_str() {
            "15m" => Ok(Self::M15),
            "1h" => Ok(Self::H1),
            "24h" => Ok(Self::H24),
            _ => Err(DomainError::InvalidWindow(value.to_string())),
        }
    }

    pub fn secs(&self) -> i64 {
        match self {
            Self::M15 => 15 * 60,
            Self::H1 => 60 * 60,
            Self::H24 => 24 * 60 * 60,
        }
    }

    pub fn bucket_secs(&self) -> i64 {
        match self {
            Self::M15 => 30,
            Self::H1 => 60,
            Self::H24 => 15 * 60,
        }
    }
}

#[derive(Debug, Clone)]
pub struct DetectedTransfer {
    pub deposit_address_id: String,
    pub tx_hash: String,
    pub amount_raw: String,
    pub confirmations: i32,
}

#[derive(Debug, Error)]
pub enum DomainError {
    #[error("Unsupported network: {0}")]
    UnsupportedNetwork(String),
    #[error("Unsupported token: {0}. Supported tokens: USDT, BTC, ETH, TRX, SOL.")]
    UnsupportedToken(String),
    #[error("Invalid status: {0}")]
    InvalidStatus(String),
    #[error("Invalid amount: {0}")]
    InvalidAmount(String),
    #[error("Invalid role: {0}")]
    InvalidRole(String),
    #[error("Invalid surface: {0}")]
    InvalidSurface(String),
    #[error("Invalid window: {0}")]
    InvalidWindow(String),
    #[error("Invalid destination kind: {0}")]
    InvalidDestinationKind(String),
    #[error("Transition not allowed: {0}")]
    InvalidTransition(String),
}

/// Integer string in token base units → decimal amount (never via floats).
pub fn raw_to_decimal(raw: &str, token: Token) -> Result<Decimal, DomainError> {
    let raw_int: i128 = raw
        .parse()
        .map_err(|_| DomainError::InvalidAmount(raw.to_string()))?;
    Decimal::try_from_i128_with_scale(raw_int, token.decimals())
        .map_err(|_| DomainError::InvalidAmount(raw.to_string()))
}

/// Format a raw base-unit integer string as a trimmed decimal string.
pub fn raw_to_decimal_string(raw: &str, token: Token) -> Result<String, DomainError> {
    let value = raw_to_decimal(raw, token)?;
    Ok(trim_decimal(value))
}

/// Decimal amount → integer string in token base units. Precision beyond the
/// token's decimals is truncated (callers store amounts via `round_to_token`).
/// Built from the mantissa rather than by multiplying: 10^18 times a large
/// ETH amount overflows `Decimal` (≈ 7.9e28).
pub fn decimal_to_raw(amount: Decimal, token: Token) -> String {
    let decimals = token.decimals();
    let truncated = amount.round_dp_with_strategy(decimals, RoundingStrategy::ToZero);
    let mantissa = truncated.mantissa();
    if mantissa == 0 {
        return "0".into();
    }
    let zeros = decimals.saturating_sub(truncated.scale()) as usize;
    format!("{mantissa}{}", "0".repeat(zeros))
}

/// Base units → trimmed decimal string (`1500000000` lamports → `1.5`),
/// exact for the whole `u128` range.
pub fn raw_u128_to_string(raw: u128, token: Token) -> String {
    let digits = raw.to_string();
    let decimals = token.decimals() as usize;
    if decimals == 0 {
        return digits;
    }
    let padded = format!("{digits:0>width$}", width = decimals + 1);
    let (int, frac) = padded.split_at(padded.len() - decimals);
    let frac = frac.trim_end_matches('0');
    if frac.is_empty() {
        int.to_string()
    } else {
        format!("{int}.{frac}")
    }
}

/// A client-supplied decimal amount (`"0.05"`) → base units, strictly:
/// plain digits with an optional fraction, at most the token's decimals
/// (never rounded), greater than zero, within `u128`.
pub fn parse_amount_raw(value: &str, token: Token) -> Result<u128, DomainError> {
    let invalid = || DomainError::InvalidAmount(value.to_string());
    let (int, frac) = match value.split_once('.') {
        Some((int, frac)) => (int, frac),
        None => (value, ""),
    };
    let digits_only = |part: &str| part.bytes().all(|b| b.is_ascii_digit());
    if int.is_empty()
        || !digits_only(int)
        || !digits_only(frac)
        || (value.contains('.') && frac.is_empty())
        || frac.len() > token.decimals() as usize
    {
        return Err(invalid());
    }
    let scale = 10u128.pow(token.decimals());
    let int: u128 = int.parse().map_err(|_| invalid())?;
    let frac_raw: u128 = if frac.is_empty() {
        0
    } else {
        let padded = format!("{frac:0<width$}", width = token.decimals() as usize);
        padded.parse().map_err(|_| invalid())?
    };
    let raw = int
        .checked_mul(scale)
        .and_then(|raw| raw.checked_add(frac_raw))
        .ok_or_else(invalid)?;
    if raw == 0 {
        return Err(invalid());
    }
    Ok(raw)
}

/// Decimal string (plain or scientific notation, as provider JSON numbers
/// print) → integer string in token base units, without float arithmetic.
pub fn decimal_str_to_raw(value: &str, token: Token) -> Result<String, DomainError> {
    let amount = value
        .parse::<Decimal>()
        .or_else(|_| Decimal::from_scientific(value))
        .map_err(|_| DomainError::InvalidAmount(value.to_string()))?;
    // Provider data: an absurd value is an error, never an overflow panic.
    round_to_token(amount, token)
        .checked_mul(Decimal::from(10u64.pow(token.decimals())))
        .map(|scaled| scaled.trunc().normalize().to_string())
        .ok_or_else(|| DomainError::InvalidAmount(value.to_string()))
}

/// Round to the token's decimals the way a Postgres `NUMERIC(_, decimals)`
/// column would (half away from zero) — keeps USDT amounts identical to what
/// the former DECIMAL(18,6) columns stored.
pub fn round_to_token(amount: Decimal, token: Token) -> Decimal {
    amount.round_dp_with_strategy(token.decimals(), RoundingStrategy::MidpointAwayFromZero)
}

/// Fixed-scale rendering at the token's decimals (`25.000000` USDT,
/// `0.00150000` BTC) — what webhook receivers have always been sent.
pub fn format_amount(amount: Decimal, token: Token) -> String {
    let mut value = round_to_token(amount, token);
    value.rescale(token.decimals());
    value.to_string()
}

/// Parse a satoshi amount: plain ASCII digits only, 1 ≤ sats ≤ total supply.
pub fn parse_sats(value: &str) -> Result<u64, DomainError> {
    let invalid = || DomainError::InvalidAmount(value.to_string());
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
        return Err(invalid());
    }
    let sats: u64 = value.parse().map_err(|_| invalid())?;
    if sats == 0 || sats > BTC_MAX_SATS {
        return Err(invalid());
    }
    Ok(sats)
}

pub fn is_amount_within_tolerance(
    actual: &str,
    expected: &str,
    tolerance_percent: f64,
) -> bool {
    let Ok(actual_value) = actual.parse::<Decimal>() else {
        return false;
    };
    let Ok(expected_value) = expected.parse::<Decimal>() else {
        return false;
    };

    let tolerance_ratio = Decimal::try_from(tolerance_percent / 100.0).unwrap_or(Decimal::ZERO);
    let tolerance = expected_value * tolerance_ratio;
    let diff = if actual_value > expected_value {
        actual_value - expected_value
    } else {
        expected_value - actual_value
    };
    diff <= tolerance
}

pub fn required_confirmations(network: Network, tron: i32, eth: i32, btc: i32) -> i32 {
    match network {
        Network::Tron => tron,
        Network::Ethereum => eth,
        Network::Bitcoin => btc,
        Network::Solana => 1,
        // A settled Lightning payment is final — no notion of confirmations.
        Network::Lightning => 1,
    }
}

/// Whether `ip` is a globally routable unicast address — the only kind the
/// gateway may connect to on a caller's behalf (webhook endpoints, Lightning
/// addresses). Loopback, private, link-local (cloud metadata), CGNAT,
/// unique-local, multicast, documentation, benchmarking and reserved ranges
/// are refused, as are IPv4-mapped/NAT64 forms of them.
pub fn is_public_ip(ip: std::net::IpAddr) -> bool {
    use std::net::IpAddr;
    match ip {
        IpAddr::V4(v4) => is_public_ipv4(v4),
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_public_ipv4(v4);
            }
            let segments = v6.segments();
            // NAT64 (64:ff9b::/96) embeds an IPv4 destination.
            if segments[..6] == [0x64, 0xff9b, 0, 0, 0, 0] {
                let [a, b] = segments[6].to_be_bytes();
                let [c, d] = segments[7].to_be_bytes();
                return is_public_ipv4(std::net::Ipv4Addr::new(a, b, c, d));
            }
            !(v6.is_unspecified()
                || v6.is_loopback()
                || v6.is_multicast()
                // IPv4-compatible (deprecated) ::/96
                || segments[..6] == [0, 0, 0, 0, 0, 0]
                // unique local fc00::/7
                || (segments[0] & 0xfe00) == 0xfc00
                // link-local fe80::/10
                || (segments[0] & 0xffc0) == 0xfe80
                // site-local (deprecated) fec0::/10
                || (segments[0] & 0xffc0) == 0xfec0
                // documentation 2001:db8::/32
                || (segments[0] == 0x2001 && segments[1] == 0x0db8)
                // discard-only 100::/64
                || segments[..4] == [0x100, 0, 0, 0])
        }
    }
}

fn is_public_ipv4(ip: std::net::Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !(a == 0                                   // "this network"
        || a == 10                             // private
        || a == 127                            // loopback
        || (a == 100 && (64..=127).contains(&b)) // CGNAT 100.64/10
        || (a == 169 && b == 254)              // link-local / cloud metadata
        || (a == 172 && (16..=31).contains(&b)) // private
        || (a == 192 && b == 0 && c == 0)      // IETF protocol assignments
        || (a == 192 && b == 0 && c == 2)      // TEST-NET-1
        || (a == 192 && b == 168)              // private
        || (a == 198 && (18..=19).contains(&b)) // benchmarking
        || (a == 198 && b == 51 && c == 100)   // TEST-NET-2
        || (a == 203 && b == 0 && c == 113)    // TEST-NET-3
        || a >= 224) // multicast, reserved, broadcast
}

fn trim_decimal(value: Decimal) -> String {
    let s = value.normalize().to_string();
    if s.contains('.') {
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn huge_provider_amounts_are_errors_not_panics() {
        assert!(decimal_str_to_raw("1e27", Token::Usdt).is_err());
        assert_eq!(decimal_str_to_raw("25.5", Token::Usdt).unwrap(), "25500000");
    }

    #[test]
    fn only_globally_routable_ips_are_public() {
        for ip in [
            "0.0.0.0", "10.1.2.3", "127.0.0.1", "100.64.0.1", "169.254.169.254",
            "172.16.0.1", "172.31.255.255", "192.0.0.1", "192.0.2.1", "192.168.1.1",
            "198.18.0.1", "198.51.100.7", "203.0.113.9", "224.0.0.1", "255.255.255.255",
            "::", "::1", "::ffff:127.0.0.1", "::ffff:169.254.169.254", "64:ff9b::a9fe:a9fe",
            "fc00::1", "fd12:3456::1", "fe80::1", "ff02::1", "2001:db8::1", "::127.0.0.1",
        ] {
            assert!(!is_public_ip(ip.parse().unwrap()), "{ip} must not be public");
        }
        for ip in ["1.1.1.1", "8.8.8.8", "172.32.0.1", "100.128.0.1", "2606:4700::1111", "::ffff:8.8.8.8"] {
            assert!(is_public_ip(ip.parse().unwrap()), "{ip} must be public");
        }
    }

    #[test]
    fn amount_within_tolerance() {
        assert!(is_amount_within_tolerance("25", "25.00", 1.0));
        assert!(is_amount_within_tolerance("25.20", "25.00", 1.0));
        assert!(!is_amount_within_tolerance("26", "25.00", 1.0));
    }

    #[test]
    fn raw_conversion() {
        assert_eq!(raw_to_decimal("25000000", Token::Usdt).unwrap(), dec!(25));
        assert_eq!(raw_to_decimal_string("25000000", Token::Usdt).unwrap(), "25");
    }

    #[test]
    fn raw_conversion_uses_token_decimals() {
        assert_eq!(raw_to_decimal("150000", Token::Btc).unwrap(), dec!(0.0015));
        assert_eq!(raw_to_decimal_string("150000", Token::Btc).unwrap(), "0.0015");
        assert_eq!(raw_to_decimal("100000000", Token::Btc).unwrap(), dec!(1));
        assert_eq!(raw_to_decimal("1", Token::Btc).unwrap(), dec!(0.00000001));
        assert_eq!(raw_to_decimal("150000", Token::Usdt).unwrap(), dec!(0.15));
    }

    #[test]
    fn raw_conversion_rejects_garbage_without_panicking() {
        assert!(raw_to_decimal("12.5", Token::Usdt).is_err());
        assert!(raw_to_decimal("", Token::Btc).is_err());
        // Beyond rust_decimal's 96-bit mantissa: an error, never a panic.
        assert!(raw_to_decimal("100000000000000000000000000000000", Token::Usdt).is_err());
    }

    #[test]
    fn token_decimals() {
        assert_eq!(Token::Usdt.decimals(), 6);
        assert_eq!(Token::Btc.decimals(), 8);
    }

    #[test]
    fn decimal_to_raw_is_inverse_of_raw_to_decimal() {
        assert_eq!(decimal_to_raw(dec!(0.0015), Token::Btc), "150000");
        assert_eq!(decimal_to_raw(dec!(25.5), Token::Usdt), "25500000");
        assert_eq!(decimal_to_raw(dec!(25.000000), Token::Usdt), "25000000");
        assert_eq!(decimal_to_raw(dec!(21000000), Token::Btc), "2100000000000000");
    }

    #[test]
    fn decimal_strings_convert_to_base_units() {
        assert_eq!(decimal_str_to_raw("25.1", Token::Usdt).unwrap(), "25100000");
        assert_eq!(decimal_str_to_raw("0.000001", Token::Usdt).unwrap(), "1");
        assert_eq!(decimal_str_to_raw("1e-6", Token::Usdt).unwrap(), "1");
        assert_eq!(decimal_str_to_raw("0.0015", Token::Btc).unwrap(), "150000");
        assert!(decimal_str_to_raw("abc", Token::Usdt).is_err());
    }

    #[test]
    fn format_amount_uses_token_scale() {
        // USDT keeps the historical 6-decimal rendering even once read from
        // the widened DECIMAL(24,8) column.
        assert_eq!(format_amount(dec!(25.00000000), Token::Usdt), "25.000000");
        assert_eq!(format_amount(dec!(25), Token::Usdt), "25.000000");
        assert_eq!(format_amount(dec!(0.0015), Token::Btc), "0.00150000");
    }

    #[test]
    fn round_to_token_matches_postgres_numeric_rounding() {
        assert_eq!(round_to_token(dec!(1.0000005), Token::Usdt), dec!(1.000001));
        assert_eq!(round_to_token(dec!(1.0000025), Token::Usdt), dec!(1.000003));
        assert_eq!(round_to_token(dec!(1.00000049), Token::Usdt), dec!(1.000000));
        assert_eq!(round_to_token(dec!(0.123456789), Token::Btc), dec!(0.12345679));
    }

    #[test]
    fn parse_sats_accepts_integer_strings_within_supply() {
        assert_eq!(parse_sats("150000").unwrap(), 150_000);
        assert_eq!(parse_sats("1").unwrap(), 1);
        assert_eq!(parse_sats("2100000000000000").unwrap(), 2_100_000_000_000_000);
        assert!(parse_sats("0").is_err());
        assert!(parse_sats("2100000000000001").is_err());
        assert!(parse_sats("-5").is_err());
        assert!(parse_sats("+5").is_err());
        assert!(parse_sats("1.5").is_err());
        assert!(parse_sats("1e3").is_err());
        assert!(parse_sats(" 15").is_err());
        assert!(parse_sats("").is_err());
        assert!(parse_sats("99999999999999999999999").is_err());
    }

    #[test]
    fn network_roundtrip() {
        assert_eq!(Network::parse_api("tron").unwrap().as_db(), "TRON");
        assert_eq!(Network::from_db("ETHEREUM").unwrap().as_api(), "ethereum");
    }

    #[test]
    fn bitcoin_and_lightning_casings_roundtrip() {
        assert_eq!(Network::parse_api("bitcoin").unwrap(), Network::Bitcoin);
        assert_eq!(Network::parse_api("lightning").unwrap(), Network::Lightning);
        assert_eq!(Network::Bitcoin.as_db(), "BITCOIN");
        assert_eq!(Network::Lightning.as_db(), "LIGHTNING");
        assert_eq!(Network::from_db("BITCOIN").unwrap().as_api(), "bitcoin");
        assert_eq!(Network::from_db("LIGHTNING").unwrap().as_api(), "lightning");
        assert!(Network::from_db("bitcoin").is_err());
    }

    #[test]
    fn btc_token_casings_roundtrip() {
        assert_eq!(Token::parse_api("BTC").unwrap(), Token::Btc);
        assert_eq!(Token::parse_api("btc").unwrap(), Token::Btc);
        assert_eq!(Token::Btc.as_db(), "BTC");
        assert_eq!(Token::Btc.as_api(), "BTC");
        assert_eq!(Token::from_db("BTC").unwrap(), Token::Btc);
        assert_eq!(Token::from_db("USDT").unwrap(), Token::Usdt);
        assert!(Token::parse_api("DOGE").is_err());
    }

    #[test]
    fn supports_matrix() {
        for network in [Network::Tron, Network::Ethereum, Network::Solana] {
            assert!(supports(network, Token::Usdt), "{network:?} must accept USDT");
            assert!(!supports(network, Token::Btc), "{network:?} must reject BTC");
        }
        for network in [Network::Bitcoin, Network::Lightning] {
            assert!(supports(network, Token::Btc), "{network:?} must accept BTC");
            assert!(!supports(network, Token::Usdt), "{network:?} must reject USDT");
        }
        // Native ETH on Ethereum only.
        assert!(supports(Network::Ethereum, Token::Eth));
        for network in [Network::Tron, Network::Solana, Network::Bitcoin, Network::Lightning] {
            assert!(!supports(network, Token::Eth), "{network:?} must reject ETH");
        }
        // Gas assets are never payments.
        for network in Network::ALL {
            assert!(!supports(network, Token::Trx), "{network:?} must reject TRX");
            assert!(!supports(network, Token::Sol), "{network:?} must reject SOL");
        }
    }

    #[test]
    fn payment_tokens_per_network() {
        assert_eq!(Network::Tron.payment_tokens(), &[Token::Usdt]);
        assert_eq!(Network::Ethereum.payment_tokens(), &[Token::Usdt, Token::Eth]);
        assert_eq!(Network::Solana.payment_tokens(), &[Token::Usdt]);
        assert_eq!(Network::Bitcoin.payment_tokens(), &[Token::Btc]);
        assert_eq!(Network::Lightning.payment_tokens(), &[Token::Btc]);
        for network in Network::ALL {
            for token in network.payment_tokens() {
                assert!(supports(network, *token));
            }
        }
    }

    #[test]
    fn gas_and_withdrawable_tokens_per_network() {
        assert_eq!(Network::Ethereum.gas_token(), Some(Token::Eth));
        assert_eq!(Network::Tron.gas_token(), Some(Token::Trx));
        assert_eq!(Network::Solana.gas_token(), Some(Token::Sol));
        assert_eq!(Network::Bitcoin.gas_token(), Some(Token::Btc));
        assert_eq!(Network::Lightning.gas_token(), None);

        // Payment tokens + gas token, deduplicated (ETH is both on Ethereum).
        assert_eq!(Network::Ethereum.withdrawable_tokens(), &[Token::Usdt, Token::Eth]);
        assert_eq!(Network::Tron.withdrawable_tokens(), &[Token::Usdt, Token::Trx]);
        assert_eq!(Network::Solana.withdrawable_tokens(), &[Token::Usdt, Token::Sol]);
        assert_eq!(Network::Bitcoin.withdrawable_tokens(), &[Token::Btc]);
        assert_eq!(Network::Lightning.withdrawable_tokens(), &[Token::Btc]);
        for network in Network::ALL {
            for token in network.payment_tokens() {
                assert!(network.withdrawable_tokens().contains(token));
            }
            if let Some(gas) = network.gas_token() {
                assert!(network.withdrawable_tokens().contains(&gas));
            }
        }
        assert!(is_withdrawable(Network::Tron, Token::Trx));
        assert!(!is_withdrawable(Network::Ethereum, Token::Trx));

        // Fees: the gas token, BTC for Lightning.
        assert_eq!(Network::Lightning.fee_token(), Token::Btc);
        assert_eq!(Network::Tron.fee_token(), Token::Trx);
    }

    #[test]
    fn gas_assets_are_flagged() {
        assert!(Token::Trx.is_gas_only());
        assert!(Token::Sol.is_gas_only());
        for token in [Token::Usdt, Token::Btc, Token::Eth] {
            assert!(!token.is_gas_only(), "{token:?}");
        }
    }

    #[test]
    fn every_token_casing_roundtrips() {
        for token in Token::ALL {
            assert_eq!(Token::from_db(token.as_db()).unwrap(), token);
            assert_eq!(Token::parse_api(&token.as_api().to_lowercase()).unwrap(), token);
        }
        assert_eq!(Token::Eth.as_db(), "ETH");
        assert_eq!(Token::Trx.as_api(), "TRX");
        assert_eq!(Token::parse_api("sol").unwrap(), Token::Sol);
        let err = Token::parse_api("DOGE").unwrap_err().to_string();
        for token in Token::ALL {
            assert!(err.contains(token.as_api()), "{err}");
        }
    }

    #[test]
    fn token_decimals_cover_the_new_assets() {
        assert_eq!(Token::Eth.decimals(), ETH_DECIMALS);
        assert_eq!(ETH_DECIMALS, 18);
        assert_eq!(Token::Trx.decimals(), TRX_DECIMALS);
        assert_eq!(TRX_DECIMALS, 6);
        assert_eq!(Token::Sol.decimals(), SOL_DECIMALS);
        assert_eq!(SOL_DECIMALS, 9);
    }

    #[test]
    fn wei_conversions_are_exact_without_overflow() {
        assert_eq!(raw_to_decimal("1", Token::Eth).unwrap(), dec!(0.000000000000000001));
        assert_eq!(raw_to_decimal("1000000000000000000", Token::Eth).unwrap(), dec!(1));
        assert_eq!(
            raw_to_decimal("1000000000000000000000000", Token::Eth).unwrap(),
            dec!(1000000)
        );
        assert_eq!(decimal_to_raw(dec!(0.000000000000000001), Token::Eth), "1");
        assert_eq!(decimal_to_raw(dec!(1), Token::Eth), "1000000000000000000");
        assert_eq!(decimal_to_raw(dec!(1000000), Token::Eth), "1000000000000000000000000");
        assert_eq!(decimal_to_raw(dec!(0.05), Token::Eth), "50000000000000000");
        // Far beyond what Decimal can hold once scaled: exact, no panic.
        assert_eq!(
            decimal_to_raw(dec!(100000000000000000000), Token::Eth),
            "100000000000000000000000000000000000000"
        );
        assert_eq!(decimal_to_raw(dec!(0), Token::Eth), "0");
        assert_eq!(format_amount(dec!(0.05), Token::Eth), "0.050000000000000000");
        assert_eq!(raw_u128_to_string(1, Token::Eth), "0.000000000000000001");
        assert_eq!(raw_u128_to_string(10u128.pow(24), Token::Eth), "1000000");
        assert_eq!(raw_u128_to_string(1_500_000_000, Token::Sol), "1.5");
        assert_eq!(raw_u128_to_string(0, Token::Trx), "0");
        assert_eq!(raw_u128_to_string(u128::MAX, Token::Eth), "340282366920938463463.374607431768211455");
    }

    #[test]
    fn decimal_amounts_parse_to_base_units_exactly() {
        assert_eq!(parse_amount_raw("0.05", Token::Eth).unwrap(), 50_000_000_000_000_000);
        assert_eq!(parse_amount_raw("1", Token::Eth).unwrap(), 10u128.pow(18));
        assert_eq!(parse_amount_raw("0.000000000000000001", Token::Eth).unwrap(), 1);
        assert_eq!(parse_amount_raw("25.5", Token::Usdt).unwrap(), 25_500_000);
        assert_eq!(parse_amount_raw("100000", Token::Eth).unwrap(), 10u128.pow(23));
        assert_eq!(parse_amount_raw("0.1", Token::Sol).unwrap(), 100_000_000);
        for bad in [
            "", "0", "0.0", "-1", "+1", "1e3", "1.", ".5", "1.2.3", " 1", "abc",
            // More decimals than the token has: refused, never rounded.
            "0.0000001",
            // Overflows u128.
            "999999999999999999999999999999999999999999",
        ] {
            assert!(parse_amount_raw(bad, Token::Usdt).is_err(), "{bad:?}");
        }
        assert!(parse_amount_raw("0.0000000000000000001", Token::Eth).is_err());
    }

    #[test]
    fn required_confirmations_per_network() {
        assert_eq!(required_confirmations(Network::Tron, 19, 12, 3), 19);
        assert_eq!(required_confirmations(Network::Ethereum, 19, 12, 3), 12);
        assert_eq!(required_confirmations(Network::Solana, 19, 12, 3), 1);
        assert_eq!(required_confirmations(Network::Bitcoin, 19, 12, 3), 3);
        assert_eq!(required_confirmations(Network::Bitcoin, 19, 12, 6), 6);
        // A settled Lightning payment is final.
        assert_eq!(required_confirmations(Network::Lightning, 19, 12, 3), 1);
    }

    #[test]
    fn api_key_status_roundtrip() {
        assert_eq!(ApiKeyStatus::from_db("ACTIVE").unwrap().as_api(), "active");
        assert_eq!(ApiKeyStatus::from_db("REVOKED").unwrap().as_api(), "revoked");
        assert_eq!(ApiKeyStatus::from_db("EXPIRED").unwrap().as_api(), "expired");
        assert_eq!(ApiKeyStatus::parse_api("Expired").unwrap().as_db(), "EXPIRED");
        assert!(ApiKeyStatus::from_db("expired").is_err());
        assert!(ApiKeyStatus::parse_api("disabled").is_err());
    }

    #[test]
    fn staff_role_roundtrip() {
        assert_eq!(StaffRole::parse_api("admin").unwrap().as_db(), "ADMIN");
        assert_eq!(StaffRole::parse_api("Operator").unwrap().as_db(), "OPERATOR");
        assert_eq!(StaffRole::from_db("ADMIN").unwrap().as_api(), "admin");
        assert_eq!(StaffRole::from_db("OPERATOR").unwrap().as_api(), "operator");
        assert!(StaffRole::from_db("admin").is_err());
        assert!(StaffRole::parse_api("viewer").is_err());
    }

    #[test]
    fn api_surface_roundtrip_and_paths() {
        assert_eq!(ApiSurface::parse_api("Merchant").unwrap().as_db(), "MERCHANT");
        assert_eq!(ApiSurface::from_db("ADMIN").unwrap().as_api(), "admin");
        assert!(ApiSurface::from_db("admin").is_err());
        assert!(ApiSurface::parse_api("all").is_err());

        assert_eq!(ApiSurface::from_path("/api/v1/balances"), Some(ApiSurface::Merchant));
        assert_eq!(ApiSurface::from_path("/api/v1"), Some(ApiSurface::Merchant));
        assert_eq!(ApiSurface::from_path("/api/admin/v1/stats"), Some(ApiSurface::Admin));
        assert_eq!(ApiSurface::from_path("/api/admin/v1"), Some(ApiSurface::Admin));
        assert_eq!(ApiSurface::from_path("/healthz"), None);
        assert_eq!(ApiSurface::from_path("/readyz"), None);
        assert_eq!(ApiSurface::from_path("/api/v2/x"), None);
        assert_eq!(ApiSurface::from_path("/api/v10/x"), None);
    }

    #[test]
    fn status_class_boundaries() {
        assert_eq!(StatusClass::of(199), None);
        assert_eq!(StatusClass::of(200), Some(StatusClass::Success));
        assert_eq!(StatusClass::of(299), Some(StatusClass::Success));
        assert_eq!(StatusClass::of(300), Some(StatusClass::Redirect));
        assert_eq!(StatusClass::of(399), Some(StatusClass::Redirect));
        assert_eq!(StatusClass::of(400), Some(StatusClass::ClientError));
        assert_eq!(StatusClass::of(499), Some(StatusClass::ClientError));
        assert_eq!(StatusClass::of(500), Some(StatusClass::ServerError));
        assert_eq!(StatusClass::of(599), Some(StatusClass::ServerError));
        assert_eq!(StatusClass::of(600), None);
        assert_eq!(StatusClass::parse_api("4XX").unwrap().range(), (400, 499));
        assert_eq!(StatusClass::ServerError.as_api(), "5xx");
        assert!(StatusClass::parse_api("1xx").is_err());
    }

    #[test]
    fn traffic_window_buckets_divide_evenly() {
        for window in [TrafficWindow::M15, TrafficWindow::H1, TrafficWindow::H24] {
            assert_eq!(window.secs() % window.bucket_secs(), 0);
            assert!(window.secs() / window.bucket_secs() <= 100);
            assert_eq!(TrafficWindow::parse_api(window.as_api()).unwrap(), window);
        }
        assert!(TrafficWindow::parse_api("7d").is_err());
    }

    #[test]
    fn payout_event_casings_roundtrip() {
        assert_eq!(WebhookEventType::PayoutSucceeded.as_db(), "PAYOUT_SUCCEEDED");
        assert_eq!(WebhookEventType::PayoutFailed.as_db(), "PAYOUT_FAILED");
        assert_eq!(
            WebhookEventType::from_db("PAYOUT_SUCCEEDED").unwrap().as_api(),
            "payout_succeeded"
        );
        assert_eq!(
            WebhookEventType::parse_api("payout_failed").unwrap(),
            WebhookEventType::PayoutFailed
        );
        assert_eq!(
            WebhookEventType::PayoutSucceeded.payload_event_type(),
            "payout_succeeded"
        );
        assert_eq!(WebhookEventType::PayoutFailed.payload_event_type(), "payout_failed");
        assert!(WebhookEventType::from_db("payout_failed").is_err());
        assert_eq!(WebhookEventType::ALL.len(), 16);
    }

    #[test]
    fn withdrawal_and_alert_events_roundtrip_and_name_their_subject() {
        for event in WebhookEventType::ALL {
            assert_eq!(WebhookEventType::from_db(event.as_db()).unwrap(), event);
            assert_eq!(WebhookEventType::parse_api(event.as_api()).unwrap(), event);
        }
        for (event, db, api) in [
            (WebhookEventType::WithdrawalPendingApproval, "WITHDRAWAL_PENDING_APPROVAL", "withdrawal_pending_approval"),
            (WebhookEventType::WithdrawalBroadcast, "WITHDRAWAL_BROADCAST", "withdrawal_broadcast"),
            (WebhookEventType::WithdrawalConfirmed, "WITHDRAWAL_CONFIRMED", "withdrawal_confirmed"),
            (WebhookEventType::WithdrawalFailed, "WITHDRAWAL_FAILED", "withdrawal_failed"),
            (WebhookEventType::WithdrawalCancelled, "WITHDRAWAL_CANCELLED", "withdrawal_cancelled"),
            (WebhookEventType::GasReserveLow, "GAS_RESERVE_LOW", "gas_reserve_low"),
            (WebhookEventType::TreasuryCapExceeded, "TREASURY_CAP_EXCEEDED", "treasury_cap_exceeded"),
            (WebhookEventType::AddressBookEntryAdded, "ADDRESS_BOOK_ENTRY_ADDED", "address_book_entry_added"),
            (WebhookEventType::WithdrawalStuck, "WITHDRAWAL_STUCK", "withdrawal_stuck"),
            (WebhookEventType::TransferCompleted, "TRANSFER_COMPLETED", "transfer_completed"),
            (WebhookEventType::LedgerShortfall, "LEDGER_SHORTFALL", "ledger_shortfall"),
        ] {
            assert_eq!(event.as_db(), db);
            assert_eq!(event.as_api(), api);
            // Payload names equal the API names for the new events.
            assert_eq!(event.payload_event_type(), api);
        }
        assert_eq!(WebhookEventType::Paid.subject(), WebhookSubject::Deposit);
        assert_eq!(WebhookEventType::Expired.subject(), WebhookSubject::DepositAddress);
        assert_eq!(WebhookEventType::PayoutFailed.subject(), WebhookSubject::LightningPayment);
        assert_eq!(WebhookEventType::WithdrawalBroadcast.subject(), WebhookSubject::Withdrawal);
        assert_eq!(WebhookEventType::WithdrawalStuck.subject(), WebhookSubject::Alert);
        assert_eq!(WebhookEventType::LedgerShortfall.subject(), WebhookSubject::Alert);
        assert_eq!(WebhookEventType::TransferCompleted.subject(), WebhookSubject::Transfer);
        for alert in AlertType::ALL {
            let event = WebhookEventType::for_alert(*alert);
            assert_eq!(event.subject(), WebhookSubject::Alert);
            assert_eq!(event.as_db(), alert.as_db());
        }
        assert!(!WebhookEventType::WithdrawalFailed.is_payout());
    }

    #[test]
    fn payout_events_are_not_address_events() {
        assert!(WebhookEventType::PayoutSucceeded.is_payout());
        assert!(WebhookEventType::PayoutFailed.is_payout());
        for event in [
            WebhookEventType::Pending,
            WebhookEventType::Paid,
            WebhookEventType::Expired,
        ] {
            assert!(!event.is_payout());
        }
        // The historical names are untouched.
        assert_eq!(WebhookEventType::Paid.payload_event_type(), "deposit_confirmed");
    }

    #[test]
    fn webhook_status_roundtrip() {
        assert_eq!(
            WebhookDeliveryStatus::from_db("PENDING").unwrap().as_api(),
            "pending"
        );
        assert_eq!(
            WebhookDeliveryStatus::from_db("DELIVERED").unwrap().as_api(),
            "delivered"
        );
        assert_eq!(
            WebhookDeliveryStatus::from_db("FAILED").unwrap().as_api(),
            "failed"
        );
        assert!(WebhookDeliveryStatus::from_db("delivered").is_err());
    }
}
