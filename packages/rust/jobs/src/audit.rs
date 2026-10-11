//! Append-only audit trail of security-relevant actions (`audit_log`).

use db::{AuditActor, NewAuditEntry};
use serde_json::Value;
use tracing::{error, info};

use crate::AppState;

/// Who is acting, and from where — built by the HTTP layer from the
/// authenticated identity and the resolved client IP.
#[derive(Debug, Clone)]
pub struct Actor {
    pub audit: AuditActor,
    pub ip: Option<String>,
}

impl Actor {
    pub fn system() -> Self {
        Self {
            audit: AuditActor::System,
            ip: None,
        }
    }
}

/// Record one action. Never fails the caller: a write error is logged (the
/// action itself already happened). `details` must not carry secrets.
pub async fn record(
    state: &AppState,
    actor: &Actor,
    action: &str,
    target: Option<(&str, &str)>,
    details: Value,
) {
    let entry = NewAuditEntry {
        actor: actor.audit.clone(),
        action,
        target_type: target.map(|(kind, _)| kind),
        target_id: target.map(|(_, id)| id),
        details,
        ip: actor.ip.as_deref(),
    };
    match state.db.insert_audit(&entry).await {
        Ok(()) => info!(
            action,
            actor = ?entry.actor,
            target = ?target,
            "audit"
        ),
        Err(err) => error!("Audit log write failed for {action}: {err:#}"),
    }
}
