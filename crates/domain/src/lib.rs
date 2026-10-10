use rust_decimal::{Decimal, RoundingStrategy};
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const USDT_DECIMALS: u32 = 6;
/// 1 BTC = 100 000 000 sats.
pub const BTC_DECIMALS: u32 = 8;
/// Total bitcoin supply in sats — the upper bound of any BTC amount.
pub const BTC_MAX_SATS: u64 = 2_100_000_000_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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

    /// The one token received on this network (see `supports`).
    pub fn token(&self) -> Token {
        match self {
            Self::Tron | Self::Ethereum | Self::Solana => Token::Usdt,
            Self::Bitcoin | Self::Lightning => Token::Btc,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Token {
    Usdt,
    Btc,
}

impl Token {
    pub fn as_db(&self) -> &'static str {
        match self {
            Self::Usdt => "USDT",
            Self::Btc => "BTC",
        }
    }

    /// Token strings are uppercase on the API surface too.
    pub fn as_api(&self) -> &'static str {
        self.as_db()
    }

    pub fn parse_api(value: &str) -> Result<Self, DomainError> {
        if value.eq_ignore_ascii_case("USDT") {
            Ok(Self::Usdt)
        } else if value.eq_ignore_ascii_case("BTC") {
            Ok(Self::Btc)
        } else {
            Err(DomainError::UnsupportedToken(value.to_string()))
        }
    }

    pub fn from_db(value: &str) -> Result<Self, DomainError> {
        Self::parse_api(value)
    }

    /// Base-unit exponent: `amount_raw = amount * 10^decimals`.
    pub fn decimals(&self) -> u32 {
        match self {
            Self::Usdt => USDT_DECIMALS,
            Self::Btc => BTC_DECIMALS,
        }
    }
}

/// Whether `token` can be received on `network`: USDT on the three account
/// networks, BTC on bitcoin and lightning.
pub fn supports(network: Network, token: Token) -> bool {
    network.token() == token
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
/// report the final outcome of every outgoing Lightning wallet payment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WebhookEventType {
    Pending,
    Paid,
    Expired,
    PayoutSucceeded,
    PayoutFailed,
}

impl WebhookEventType {
    pub const ALL: [Self; 5] = [
        Self::Pending,
        Self::Paid,
        Self::Expired,
        Self::PayoutSucceeded,
        Self::PayoutFailed,
    ];

    pub fn as_db(&self) -> &'static str {
        match self {
            Self::Pending => "PENDING",
            Self::Paid => "PAID",
            Self::Expired => "EXPIRED",
            Self::PayoutSucceeded => "PAYOUT_SUCCEEDED",
            Self::PayoutFailed => "PAYOUT_FAILED",
        }
    }

    pub fn as_api(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Paid => "paid",
            Self::Expired => "expired",
            Self::PayoutSucceeded => "payout_succeeded",
            Self::PayoutFailed => "payout_failed",
        }
    }

    /// `event_type` value sent in the webhook payload. `deposit_confirmed`
    /// predates event subscriptions and must not change (existing receivers).
    pub fn payload_event_type(&self) -> &'static str {
        match self {
            Self::Pending => "payment_pending",
            Self::Paid => "deposit_confirmed",
            Self::Expired => "payment_expired",
            Self::PayoutSucceeded => "payout_succeeded",
            Self::PayoutFailed => "payout_failed",
        }
    }

    /// Payout events are keyed on a Lightning wallet payment id, not on a
    /// deposit address.
    pub fn is_payout(&self) -> bool {
        matches!(self, Self::PayoutSucceeded | Self::PayoutFailed)
    }

    pub fn parse_api(value: &str) -> Result<Self, DomainError> {
        match value.to_lowercase().as_str() {
            "pending" => Ok(Self::Pending),
            "paid" => Ok(Self::Paid),
            "expired" => Ok(Self::Expired),
            "payout_succeeded" => Ok(Self::PayoutSucceeded),
            "payout_failed" => Ok(Self::PayoutFailed),
            _ => Err(DomainError::InvalidStatus(value.to_string())),
        }
    }

    pub fn from_db(value: &str) -> Result<Self, DomainError> {
        match value {
            "PENDING" => Ok(Self::Pending),
            "PAID" => Ok(Self::Paid),
            "EXPIRED" => Ok(Self::Expired),
            "PAYOUT_SUCCEEDED" => Ok(Self::PayoutSucceeded),
            "PAYOUT_FAILED" => Ok(Self::PayoutFailed),
            _ => Err(DomainError::InvalidStatus(value.to_string())),
        }
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
    #[error("Unsupported token: {0}. Supported tokens: USDT, BTC.")]
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
pub fn decimal_to_raw(amount: Decimal, token: Token) -> String {
    let scaled = amount * Decimal::from(10u64.pow(token.decimals()));
    scaled.trunc().normalize().to_string()
}

/// Decimal string (plain or scientific notation, as provider JSON numbers
/// print) → integer string in token base units, without float arithmetic.
pub fn decimal_str_to_raw(value: &str, token: Token) -> Result<String, DomainError> {
    let amount = value
        .parse::<Decimal>()
        .or_else(|_| Decimal::from_scientific(value))
        .map_err(|_| DomainError::InvalidAmount(value.to_string()))?;
    Ok(decimal_to_raw(round_to_token(amount, token), token))
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
        assert!(Token::parse_api("ETH").is_err());
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
    }

    #[test]
    fn each_network_receives_its_own_token() {
        assert_eq!(Network::Tron.token(), Token::Usdt);
        assert_eq!(Network::Ethereum.token(), Token::Usdt);
        assert_eq!(Network::Solana.token(), Token::Usdt);
        assert_eq!(Network::Bitcoin.token(), Token::Btc);
        assert_eq!(Network::Lightning.token(), Token::Btc);
        for network in Network::ALL {
            assert!(supports(network, network.token()));
        }
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
        assert_eq!(WebhookEventType::ALL.len(), 5);
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
