//! Projects (docs/projects-spec.md): the merchant's applications, each
//! owning its merchant keys, deposit addresses, withdrawals, webhook
//! endpoints and address-book entries. Pure vocabulary and rules: the
//! owner of an object, the colour palette, the key-tag and name rules,
//! which events an endpoint may subscribe to depending on its owner, and
//! how a legacy endpoint is split between the first project and the
//! merchant.

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::WebhookEventType;

/// Longest project name (characters, after trim).
pub const PROJECT_NAME_MAX_CHARS: usize = 60;
/// Longest key tag (alphanumeric characters).
pub const PROJECT_KEY_TAG_MAX_LEN: usize = 5;
/// The literal a request sends in `project_id` to name the merchant funds.
pub const MERCHANT_OWNER: &str = "merchant";

/// The 8-colour palette of the dashboard (stored lowercase, CHECKed in SQL).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ProjectColor {
    Blue,
    Emerald,
    Amber,
    Rose,
    Violet,
    Cyan,
    Orange,
    Fuchsia,
}

impl ProjectColor {
    /// Palette order — also the tie-breaking order of [`pick_color`].
    pub const ALL: [Self; 8] = [
        Self::Blue,
        Self::Emerald,
        Self::Amber,
        Self::Rose,
        Self::Violet,
        Self::Cyan,
        Self::Orange,
        Self::Fuchsia,
    ];

    /// Same casing in the database and on the API surface.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Blue => "blue",
            Self::Emerald => "emerald",
            Self::Amber => "amber",
            Self::Rose => "rose",
            Self::Violet => "violet",
            Self::Cyan => "cyan",
            Self::Orange => "orange",
            Self::Fuchsia => "fuchsia",
        }
    }

    pub fn parse(value: &str) -> Result<Self, ProjectValidationError> {
        let value = value.trim().to_lowercase();
        Self::ALL
            .into_iter()
            .find(|color| color.as_str() == value)
            .ok_or(ProjectValidationError::InvalidColor)
    }
}

/// The colour of a new project when none is given: the first palette colour
/// no project uses yet; once all are taken, the least-used one, palette
/// order breaking ties.
pub fn pick_color(used: &[ProjectColor]) -> ProjectColor {
    let mut best = ProjectColor::ALL[0];
    let mut best_count = usize::MAX;
    for color in ProjectColor::ALL {
        let count = used.iter().filter(|c| **c == color).count();
        if count < best_count {
            best = color;
            best_count = count;
        }
    }
    best
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ProjectValidationError {
    #[error("Name is required (at most {PROJECT_NAME_MAX_CHARS} characters)")]
    InvalidName,
    #[error("Key tag must be 1-{PROJECT_KEY_TAG_MAX_LEN} alphanumeric characters (empty to use the global tag)")]
    InvalidKeyTag,
    #[error("Unknown colour: expected one of blue, emerald, amber, rose, violet, cyan, orange, fuchsia")]
    InvalidColor,
    #[error("project_id must be a project UUID or \"merchant\"")]
    InvalidOwner,
}

/// A project name: trimmed, 1–60 characters.
pub fn validate_project_name(name: &str) -> Result<String, ProjectValidationError> {
    let name = name.trim();
    let len = name.chars().count();
    if len == 0 || len > PROJECT_NAME_MAX_CHARS {
        return Err(ProjectValidationError::InvalidName);
    }
    Ok(name.to_string())
}

/// A key tag: `None` when blank (the global tag applies), otherwise 1–5
/// ASCII alphanumeric characters (`_` is the key-format separator).
pub fn validate_key_tag(tag: &str) -> Result<Option<String>, ProjectValidationError> {
    let tag = tag.trim();
    if tag.is_empty() {
        return Ok(None);
    }
    if tag.len() > PROJECT_KEY_TAG_MAX_LEN || !tag.chars().all(|c| c.is_ascii_alphanumeric()) {
        return Err(ProjectValidationError::InvalidKeyTag);
    }
    Ok(Some(tag.to_string()))
}

/// Who an owned object belongs to: one of the merchant's projects, or the
/// merchant itself where the spec allows it (withdrawals, endpoints,
/// address-book entries, transfer sides).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Owner {
    Project(String),
    Merchant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum OwnerKind {
    Project,
    Merchant,
}

impl Owner {
    /// `"projectId"` column value ↔ owner.
    pub fn from_project_id(project_id: Option<&str>) -> Self {
        match project_id {
            Some(id) => Self::Project(id.to_string()),
            None => Self::Merchant,
        }
    }

    pub fn project_id(&self) -> Option<&str> {
        match self {
            Self::Project(id) => Some(id),
            Self::Merchant => None,
        }
    }

    pub fn kind(&self) -> OwnerKind {
        match self {
            Self::Project(_) => OwnerKind::Project,
            Self::Merchant => OwnerKind::Merchant,
        }
    }

    /// A request's `project_id`: a project UUID (canonical lowercase form)
    /// or the literal `"merchant"`.
    pub fn parse_api(value: &str) -> Result<Self, ProjectValidationError> {
        let value = value.trim();
        if value.eq_ignore_ascii_case(MERCHANT_OWNER) {
            return Ok(Self::Merchant);
        }
        parse_uuid(value)
            .map(Self::Project)
            .ok_or(ProjectValidationError::InvalidOwner)
    }
}

impl OwnerKind {
    pub fn as_api(&self) -> &'static str {
        match self {
            Self::Project => "project",
            Self::Merchant => "merchant",
        }
    }
}

/// `8-4-4-4-12` hexadecimal → canonical lowercase form; `None` otherwise.
pub fn parse_uuid(value: &str) -> Option<String> {
    let value = value.trim();
    if value.len() != 36 {
        return None;
    }
    let bytes = value.as_bytes();
    for (i, byte) in bytes.iter().enumerate() {
        let is_separator = matches!(i, 8 | 13 | 18 | 23);
        if is_separator {
            if *byte != b'-' {
                return None;
            }
        } else if !byte.is_ascii_hexdigit() {
            return None;
        }
    }
    Some(value.to_ascii_lowercase())
}

/// Who receives an event (spec §10.1, invariant 5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventOwnerRule {
    /// The endpoints of the subject's owner (a project, or the merchant for
    /// a merchant-funds withdrawal).
    SubjectOwner,
    /// Merchant endpoints only: alerts and non-gateway Lightning sends.
    Merchant,
    /// The endpoints of both sides of an internal transfer.
    TransferSides,
}

impl WebhookEventType {
    /// The owner rule of the event, exhaustive over every event type.
    pub fn owner_rule(&self) -> EventOwnerRule {
        match self {
            Self::Pending
            | Self::Paid
            | Self::Expired
            | Self::Late
            | Self::WithdrawalPendingApproval
            | Self::WithdrawalBroadcast
            | Self::WithdrawalConfirmed
            | Self::WithdrawalFailed
            | Self::WithdrawalCancelled => EventOwnerRule::SubjectOwner,
            Self::TransferCompleted => EventOwnerRule::TransferSides,
            Self::PayoutSucceeded
            | Self::PayoutFailed
            | Self::GasReserveLow
            | Self::TreasuryCapExceeded
            | Self::AddressBookEntryAdded
            | Self::WithdrawalStuck
            | Self::LedgerShortfall => EventOwnerRule::Merchant,
        }
    }
}

/// Events an endpoint of `owner` may subscribe to (spec §10.2).
pub fn allowed_events(owner: OwnerKind) -> &'static [WebhookEventType] {
    match owner {
        OwnerKind::Project => &[
            WebhookEventType::Pending,
            WebhookEventType::Paid,
            WebhookEventType::Expired,
            WebhookEventType::Late,
            WebhookEventType::WithdrawalPendingApproval,
            WebhookEventType::WithdrawalBroadcast,
            WebhookEventType::WithdrawalConfirmed,
            WebhookEventType::WithdrawalFailed,
            WebhookEventType::WithdrawalCancelled,
            WebhookEventType::TransferCompleted,
        ],
        OwnerKind::Merchant => &[
            WebhookEventType::WithdrawalPendingApproval,
            WebhookEventType::WithdrawalBroadcast,
            WebhookEventType::WithdrawalConfirmed,
            WebhookEventType::WithdrawalFailed,
            WebhookEventType::WithdrawalCancelled,
            WebhookEventType::TransferCompleted,
            WebhookEventType::PayoutSucceeded,
            WebhookEventType::PayoutFailed,
            WebhookEventType::GasReserveLow,
            WebhookEventType::TreasuryCapExceeded,
            WebhookEventType::AddressBookEntryAdded,
            WebhookEventType::WithdrawalStuck,
            WebhookEventType::LedgerShortfall,
        ],
    }
}

/// Whether an endpoint of `owner` may subscribe to `event`.
pub fn is_event_allowed(owner: OwnerKind, event: WebhookEventType) -> bool {
    allowed_events(owner).contains(&event)
}

/// What a legacy endpoint (no owner yet) becomes when the first project
/// attaches the existing data (spec §13).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LegacyEndpointSplit {
    /// Attached to the project with every one of its events.
    Project(Vec<WebhookEventType>),
    /// Stays a merchant endpoint with every one of its events.
    Merchant(Vec<WebhookEventType>),
    /// Attached to the project with `project` events, and a new merchant
    /// endpoint (same url, secret and activity) subscribed to `merchant`.
    Both {
        project: Vec<WebhookEventType>,
        merchant: Vec<WebhookEventType>,
    },
}

/// Classify a legacy endpoint by its subscriptions. Payment events belong
/// to the project, alert and payout events to the merchant; the withdrawal
/// and transfer events exist on both sides and are kept on both, so a
/// receiver keeps getting every withdrawal. An endpoint subscribed to
/// shared events only is split for the same reason: attached to one owner
/// alone, it would stop hearing about the other owner's withdrawals.
pub fn split_legacy_endpoint(events: &[WebhookEventType]) -> LegacyEndpointSplit {
    let project: Vec<WebhookEventType> = events
        .iter()
        .copied()
        .filter(|e| is_event_allowed(OwnerKind::Project, *e))
        .collect();
    let merchant: Vec<WebhookEventType> = events
        .iter()
        .copied()
        .filter(|e| is_event_allowed(OwnerKind::Merchant, *e))
        .collect();
    let project_only = events.iter().any(|e| !is_event_allowed(OwnerKind::Merchant, *e));
    let merchant_only = events.iter().any(|e| !is_event_allowed(OwnerKind::Project, *e));
    match (project_only, merchant_only) {
        (true, false) => LegacyEndpointSplit::Project(project),
        (false, true) => LegacyEndpointSplit::Merchant(merchant),
        _ => LegacyEndpointSplit::Both { project, merchant },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use WebhookEventType as E;

    #[test]
    fn colours_roundtrip_and_parse_loosely() {
        for color in ProjectColor::ALL {
            assert_eq!(ProjectColor::parse(color.as_str()).unwrap(), color);
        }
        assert_eq!(ProjectColor::parse(" Rose ").unwrap(), ProjectColor::Rose);
        assert_eq!(ProjectColor::parse("red"), Err(ProjectValidationError::InvalidColor));
    }

    #[test]
    fn palette_auto_assignment() {
        assert_eq!(pick_color(&[]), ProjectColor::Blue);
        assert_eq!(pick_color(&[ProjectColor::Blue]), ProjectColor::Emerald);
        assert_eq!(pick_color(&[ProjectColor::Emerald, ProjectColor::Blue]), ProjectColor::Amber);
        // Every colour taken once: the least used wins, palette order breaking ties.
        let all = ProjectColor::ALL.to_vec();
        assert_eq!(pick_color(&all), ProjectColor::Blue);
        let mut twice_blue = all.clone();
        twice_blue.push(ProjectColor::Blue);
        assert_eq!(pick_color(&twice_blue), ProjectColor::Emerald);
        let mut all_but_cyan_twice: Vec<ProjectColor> = all.iter().chain(all.iter()).copied().collect();
        all_but_cyan_twice.retain(|c| *c != ProjectColor::Cyan);
        all_but_cyan_twice.push(ProjectColor::Cyan);
        assert_eq!(pick_color(&all_but_cyan_twice), ProjectColor::Cyan);
    }

    #[test]
    fn names_are_trimmed_and_bounded() {
        assert_eq!(validate_project_name("  Shop  ").unwrap(), "Shop");
        assert_eq!(validate_project_name("é".repeat(60).as_str()).unwrap().chars().count(), 60);
        assert_eq!(validate_project_name("   "), Err(ProjectValidationError::InvalidName));
        assert_eq!(validate_project_name(&"x".repeat(61)), Err(ProjectValidationError::InvalidName));
    }

    #[test]
    fn key_tags_follow_the_api_key_format() {
        assert_eq!(validate_key_tag("").unwrap(), None);
        assert_eq!(validate_key_tag("  ").unwrap(), None);
        assert_eq!(validate_key_tag(" shop1 ").unwrap(), Some("shop1".into()));
        for bad in ["shop_1", "shoppy", "é", "a b"] {
            assert_eq!(validate_key_tag(bad), Err(ProjectValidationError::InvalidKeyTag), "{bad}");
        }
    }

    #[test]
    fn owners_parse_as_uuid_or_merchant() {
        assert_eq!(Owner::parse_api("merchant").unwrap(), Owner::Merchant);
        assert_eq!(Owner::parse_api(" MERCHANT ").unwrap(), Owner::Merchant);
        assert_eq!(
            Owner::parse_api("6F1C7A8E-2F43-4D55-9A8B-1C2D3E4F5A6B").unwrap(),
            Owner::Project("6f1c7a8e-2f43-4d55-9a8b-1c2d3e4f5a6b".into())
        );
        for bad in ["", "project", "6f1c7a8e2f434d559a8b1c2d3e4f5a6b", "6f1c7a8e-2f43-4d55-9a8b-1c2d3e4f5a6g"] {
            assert_eq!(Owner::parse_api(bad), Err(ProjectValidationError::InvalidOwner), "{bad}");
        }
        assert_eq!(Owner::from_project_id(None), Owner::Merchant);
        assert_eq!(Owner::from_project_id(Some("p1")).project_id(), Some("p1"));
        assert_eq!(Owner::Merchant.kind(), OwnerKind::Merchant);
        assert_eq!(Owner::Project("p1".into()).kind(), OwnerKind::Project);
    }

    #[test]
    fn every_event_has_an_owner_rule_consistent_with_the_subscriptions() {
        for event in E::ALL {
            let rule = event.owner_rule();
            let project_ok = is_event_allowed(OwnerKind::Project, event);
            let merchant_ok = is_event_allowed(OwnerKind::Merchant, event);
            match rule {
                // Merchant-only events are never offered to a project.
                EventOwnerRule::Merchant => assert!(merchant_ok && !project_ok, "{event:?}"),
                // Both sides of a transfer can be either owner.
                EventOwnerRule::TransferSides => assert!(merchant_ok && project_ok, "{event:?}"),
                EventOwnerRule::SubjectOwner => assert!(project_ok, "{event:?}"),
            }
        }
        // Payment subjects always belong to a project; withdrawals may be
        // merchant funds.
        for event in [E::Pending, E::Paid, E::Expired, E::Late] {
            assert!(!is_event_allowed(OwnerKind::Merchant, event));
        }
        for event in [E::WithdrawalPendingApproval, E::WithdrawalBroadcast, E::WithdrawalConfirmed, E::WithdrawalFailed, E::WithdrawalCancelled] {
            assert!(is_event_allowed(OwnerKind::Merchant, event));
            assert!(is_event_allowed(OwnerKind::Project, event));
        }
        assert_eq!(allowed_events(OwnerKind::Project).len(), 10);
        assert_eq!(allowed_events(OwnerKind::Merchant).len(), 13);
    }

    #[test]
    fn legacy_endpoints_are_split_by_their_subscriptions() {
        assert_eq!(
            split_legacy_endpoint(&[E::Paid, E::Pending]),
            LegacyEndpointSplit::Project(vec![E::Paid, E::Pending])
        );
        assert_eq!(
            split_legacy_endpoint(&[E::Paid, E::WithdrawalConfirmed]),
            LegacyEndpointSplit::Project(vec![E::Paid, E::WithdrawalConfirmed])
        );
        assert_eq!(
            split_legacy_endpoint(&[E::GasReserveLow, E::PayoutFailed]),
            LegacyEndpointSplit::Merchant(vec![E::GasReserveLow, E::PayoutFailed])
        );
        // Mixed: the shared events go to both sides.
        assert_eq!(
            split_legacy_endpoint(&[E::Paid, E::WithdrawalBroadcast, E::TransferCompleted, E::WithdrawalStuck]),
            LegacyEndpointSplit::Both {
                project: vec![E::Paid, E::WithdrawalBroadcast, E::TransferCompleted],
                merchant: vec![E::WithdrawalBroadcast, E::TransferCompleted, E::WithdrawalStuck],
            }
        );
        // Shared events only: both owners keep notifying it.
        assert_eq!(
            split_legacy_endpoint(&[E::WithdrawalConfirmed]),
            LegacyEndpointSplit::Both {
                project: vec![E::WithdrawalConfirmed],
                merchant: vec![E::WithdrawalConfirmed],
            }
        );
    }

    #[test]
    fn uuids_are_canonicalized() {
        assert_eq!(parse_uuid("6F1C7A8E-2F43-4D55-9A8B-1C2D3E4F5A6B").as_deref(), Some("6f1c7a8e-2f43-4d55-9a8b-1c2d3e4f5a6b"));
        assert!(parse_uuid("not-a-uuid").is_none());
    }
}
