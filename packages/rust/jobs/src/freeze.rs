//! The outflow freeze (spec §7.11): while it holds, no withdrawal is
//! admitted, nothing new is signed or broadcast, sweeps and gas top-ups
//! stop. Quotes, approvals and cancellations keep working, and an already
//! public (BROADCAST) transaction may still be re-broadcast.

use serde_json::json;
use tracing::{error, warn};

use crate::audit::{self, Actor};
use crate::AppState;

/// Frozen by `OUTFLOWS_FROZEN=true` or the dashboard setting. An unreadable
/// setting reads as frozen: failing closed is the only safe answer.
pub async fn outflows_frozen(state: &AppState) -> bool {
    if state.config.outflows_frozen {
        return true;
    }
    match state.db.outflows_frozen().await {
        Ok(frozen) => frozen,
        Err(err) => {
            error!("Outflow freeze state unreadable — treated as frozen: {err:#}");
            true
        }
    }
}

#[derive(Debug)]
pub enum FreezeError {
    /// `OUTFLOWS_FROZEN=true`: the setting cannot lift the freeze.
    ForcedByEnvironment,
    Internal(anyhow::Error),
}

impl From<anyhow::Error> for FreezeError {
    fn from(value: anyhow::Error) -> Self {
        Self::Internal(value)
    }
}

/// Set the freeze (any staff member may freeze; lifting it is an
/// admin + TOTP action checked by the HTTP layer).
pub async fn set_outflows_frozen(state: &AppState, actor: &Actor, frozen: bool) -> Result<(), FreezeError> {
    if !frozen && state.config.outflows_frozen {
        return Err(FreezeError::ForcedByEnvironment);
    }
    state.db.set_outflows_frozen(frozen).await?;
    if frozen {
        warn!(actor = ?actor.audit, "OUTFLOWS FROZEN — no withdrawal, sweep or top-up leaves the gateway");
    } else {
        warn!(actor = ?actor.audit, "Outflows unfrozen");
    }
    audit::record(
        state,
        actor,
        if frozen { "outflows.frozen" } else { "outflows.unfrozen" },
        None,
        json!({}),
    )
    .await;
    Ok(())
}
