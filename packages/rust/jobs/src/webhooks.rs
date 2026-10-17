//! Webhook routing by owner (docs/projects-spec.md §10.1, invariant 5):
//! an event goes to the endpoints of its subject's owner — a payment's or
//! withdrawal's project, the merchant for alerts, merchant-funds
//! withdrawals and non-gateway Lightning sends, both sides of an internal
//! transfer. Every fan-out path and every delivery attempt goes through
//! here. Before the first project exists the filter is off and fan-out
//! behaves as it did without projects (§13).

use anyhow::Result;
use db::WebhookEndpointRow;
use domain::{EventOwnerRule, Owner, WebhookEventType, WebhookSubject};

use crate::{projects, AppState};

/// Who an event's subject belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubjectOwners {
    One(Owner),
    /// An internal transfer: its `from` and `to` sides.
    Both(Owner, Owner),
}

impl SubjectOwners {
    pub fn merchant() -> Self {
        Self::One(Owner::Merchant)
    }

    fn contains(&self, owner: &Owner) -> bool {
        match self {
            Self::One(one) => one == owner,
            Self::Both(from, to) => from == owner || to == owner,
        }
    }

    fn owners(&self) -> Vec<&Owner> {
        match self {
            Self::One(one) => vec![one],
            Self::Both(from, to) if from == to => vec![from],
            Self::Both(from, to) => vec![from, to],
        }
    }
}

/// Whether `endpoint` is owned by (one of) the subject's owners — the
/// pure half of the owner rule.
pub fn endpoint_matches(endpoint: &WebhookEndpointRow, owners: &SubjectOwners) -> bool {
    owners.contains(&endpoint.owner())
}

/// The owners of `event`'s subject, read from the database; `None` when
/// the subject no longer exists (or, for a transfer, is not available).
pub async fn subject_owners(
    state: &AppState,
    event: WebhookEventType,
    subject_id: &str,
) -> Result<Option<SubjectOwners>> {
    match event.owner_rule() {
        EventOwnerRule::Merchant => Ok(Some(SubjectOwners::merchant())),
        EventOwnerRule::TransferSides => match state.db.find_internal_transfer(subject_id).await? {
            Some(transfer) => Ok(Some(SubjectOwners::Both(transfer.from_owner(), transfer.to_owner()))),
            None => Ok(None),
        },
        EventOwnerRule::SubjectOwner => match event.subject() {
            WebhookSubject::Deposit => {
                let Some(deposit) = state.db.find_deposit(subject_id).await? else {
                    return Ok(None);
                };
                address_owner(state, &deposit.deposit_address_id).await
            }
            WebhookSubject::DepositAddress => address_owner(state, subject_id).await,
            WebhookSubject::Withdrawal => Ok(state
                .db
                .find_withdrawal(subject_id)
                .await?
                .map(|row| SubjectOwners::One(row.owner()))),
            // Alerts, payouts and transfers never reach this arm.
            WebhookSubject::LightningPayment | WebhookSubject::Alert | WebhookSubject::Transfer => {
                Ok(Some(SubjectOwners::merchant()))
            }
        },
    }
}

async fn address_owner(state: &AppState, address_id: &str) -> Result<Option<SubjectOwners>> {
    Ok(state
        .db
        .find_deposit_address(address_id)
        .await?
        .map(|address| SubjectOwners::One(Owner::from_project_id(address.project_id.as_deref()))))
}

/// The active endpoints subscribed to `event` that belong to `owners`
/// (the union of both sides for a transfer, deduplicated). Before the first
/// project exists, every active subscribed endpoint.
pub async fn endpoints_for(
    state: &AppState,
    event: WebhookEventType,
    owners: &SubjectOwners,
) -> Result<Vec<WebhookEndpointRow>> {
    if !projects::projects_exist(state).await? {
        return state.db.active_webhook_endpoints_for_event(event, None).await;
    }
    let mut endpoints: Vec<WebhookEndpointRow> = Vec::new();
    for owner in owners.owners() {
        for endpoint in state.db.active_webhook_endpoints_for_event(event, Some(owner)).await? {
            if !endpoints.iter().any(|e| e.id == endpoint.id) {
                endpoints.push(endpoint);
            }
        }
    }
    Ok(endpoints)
}

/// The endpoints of `event`'s subject, resolved from the database; `None`
/// when the subject is gone.
pub async fn endpoints_for_subject(
    state: &AppState,
    event: WebhookEventType,
    subject_id: &str,
) -> Result<Option<Vec<WebhookEndpointRow>>> {
    match subject_owners(state, event, subject_id).await? {
        Some(owners) => Ok(Some(endpoints_for(state, event, &owners).await?)),
        None => Ok(None),
    }
}

/// Why an endpoint will not receive a delivery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ineligible {
    /// Deleted, inactive, or no longer subscribed to the event.
    EndpointUnavailable,
    /// Another owner's endpoint.
    OwnerMismatch,
    SubjectGone,
}

/// The check `deliver_webhook` and the manual retries run before every
/// attempt: the endpoint exists, is active, subscribed to `event`, and is
/// owned by the subject's owner (once projects exist).
pub async fn check_endpoint(
    state: &AppState,
    endpoint_id: &str,
    event: WebhookEventType,
    subject_id: &str,
) -> Result<Result<WebhookEndpointRow, Ineligible>> {
    let endpoint = match state.db.find_webhook_endpoint(endpoint_id).await? {
        Some(endpoint) if endpoint.is_active && endpoint.accepts_event(event) => endpoint,
        _ => return Ok(Err(Ineligible::EndpointUnavailable)),
    };
    if !projects::projects_exist(state).await? {
        return Ok(Ok(endpoint));
    }
    let Some(owners) = subject_owners(state, event, subject_id).await? else {
        return Ok(Err(Ineligible::SubjectGone));
    };
    if !endpoint_matches(&endpoint, &owners) {
        return Ok(Err(Ineligible::OwnerMismatch));
    }
    Ok(Ok(endpoint))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn endpoint(project_id: Option<&str>) -> WebhookEndpointRow {
        let now = chrono::Utc::now();
        WebhookEndpointRow {
            id: "e".into(),
            url: "https://example.com/hook".into(),
            is_active: true,
            events: vec!["PAID".into()],
            secret: None,
            created_by_id: "s".into(),
            project_id: project_id.map(str::to_string),
            created_at: now,
            updated_at: now,
        }
    }

    #[test]
    fn endpoints_follow_the_subject_owner() {
        let p1 = Owner::Project("p1".into());
        let p2 = Owner::Project("p2".into());
        assert!(endpoint_matches(&endpoint(Some("p1")), &SubjectOwners::One(p1.clone())));
        assert!(!endpoint_matches(&endpoint(Some("p2")), &SubjectOwners::One(p1.clone())));
        assert!(!endpoint_matches(&endpoint(None), &SubjectOwners::One(p1.clone())));
        // Merchant subjects (alerts, payouts, merchant-funds withdrawals).
        assert!(endpoint_matches(&endpoint(None), &SubjectOwners::merchant()));
        assert!(!endpoint_matches(&endpoint(Some("p1")), &SubjectOwners::merchant()));
        // A transfer notifies both sides, nobody else.
        let both = SubjectOwners::Both(p1.clone(), Owner::Merchant);
        assert!(endpoint_matches(&endpoint(Some("p1")), &both));
        assert!(endpoint_matches(&endpoint(None), &both));
        assert!(!endpoint_matches(&endpoint(Some("p2")), &both));
        assert_eq!(SubjectOwners::Both(p1.clone(), p2.clone()).owners().len(), 2);
        assert_eq!(SubjectOwners::Both(p1.clone(), p1).owners().len(), 1);
    }
}
