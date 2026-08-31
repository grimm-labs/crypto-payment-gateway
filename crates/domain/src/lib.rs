use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const USDT_DECIMALS: u32 = 6;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Network {
    Tron,
    Solana,
    Ethereum,
}

impl Network {
    pub fn as_db(&self) -> &'static str {
        match self {
            Self::Tron => "TRON",
            Self::Solana => "SOLANA",
            Self::Ethereum => "ETHEREUM",
        }
    }

    pub fn as_api(&self) -> &'static str {
        match self {
            Self::Tron => "tron",
            Self::Solana => "solana",
            Self::Ethereum => "ethereum",
        }
    }

    pub fn parse_api(value: &str) -> Result<Self, DomainError> {
        match value.to_lowercase().as_str() {
            "tron" => Ok(Self::Tron),
            "solana" => Ok(Self::Solana),
            "ethereum" => Ok(Self::Ethereum),
            _ => Err(DomainError::UnsupportedNetwork(value.to_string())),
        }
    }

    pub fn from_db(value: &str) -> Result<Self, DomainError> {
        match value {
            "TRON" => Ok(Self::Tron),
            "SOLANA" => Ok(Self::Solana),
            "ETHEREUM" => Ok(Self::Ethereum),
            _ => Err(DomainError::UnsupportedNetwork(value.to_string())),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Token {
    Usdt,
}

impl Token {
    pub fn as_db(&self) -> &'static str {
        "USDT"
    }

    pub fn as_api(&self) -> &'static str {
        "USDT"
    }

    pub fn parse_api(value: &str) -> Result<Self, DomainError> {
        if value.eq_ignore_ascii_case("USDT") {
            Ok(Self::Usdt)
        } else {
            Err(DomainError::UnsupportedToken(value.to_string()))
        }
    }

    pub fn from_db(value: &str) -> Result<Self, DomainError> {
        Self::parse_api(value)
    }
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

/// Address lifecycle event a webhook endpoint can subscribe to. Mirrors
/// `DepositAddressStatus`: PENDING fires at address creation, PAID at deposit
/// confirmation, EXPIRED when the address expires unpaid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WebhookEventType {
    Pending,
    Paid,
    Expired,
}

impl WebhookEventType {
    pub const ALL: [Self; 3] = [Self::Pending, Self::Paid, Self::Expired];

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

    /// `event_type` value sent in the webhook payload. `deposit_confirmed`
    /// predates event subscriptions and must not change (existing receivers).
    pub fn payload_event_type(&self) -> &'static str {
        match self {
            Self::Pending => "payment_pending",
            Self::Paid => "deposit_confirmed",
            Self::Expired => "payment_expired",
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
    #[error("Unsupported token: {0}. Only USDT is supported.")]
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

pub fn raw_to_decimal(raw: &str) -> Result<Decimal, DomainError> {
    let raw_int: i128 = raw
        .parse()
        .map_err(|_| DomainError::InvalidAmount(raw.to_string()))?;
    Ok(Decimal::from_i128_with_scale(raw_int, USDT_DECIMALS))
}

/// Format raw integer string (6 decimals) as decimal string without float.
pub fn raw_to_decimal_string(raw: &str) -> Result<String, DomainError> {
    let value = raw_to_decimal(raw)?;
    Ok(trim_decimal(value))
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

pub fn required_confirmations(
    network: Network,
    tron: i32,
    eth: i32,
) -> i32 {
    match network {
        Network::Tron => tron,
        Network::Ethereum => eth,
        Network::Solana => 1,
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
        assert_eq!(raw_to_decimal("25000000").unwrap(), dec!(25));
        assert_eq!(raw_to_decimal_string("25000000").unwrap(), "25");
    }

    #[test]
    fn network_roundtrip() {
        assert_eq!(Network::parse_api("tron").unwrap().as_db(), "TRON");
        assert_eq!(Network::from_db("ETHEREUM").unwrap().as_api(), "ethereum");
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
