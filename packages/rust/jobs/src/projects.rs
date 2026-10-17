//! Projects (docs/projects-spec.md): whether the instance runs with
//! projects yet. Before the first project exists the instance behaves as it
//! did without them (legacy keys unscoped, webhook fan-out unfiltered, no
//! project balance check); every route that creates an owned object refuses
//! with "Create a project first".

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::Result;
use config::PROJECTS_EXIST_CACHE_SECS;

use crate::AppState;

/// Once a project exists one always will (the last one cannot be deleted),
/// so a positive answer is remembered for the life of the process; a
/// negative one is re-checked after `PROJECTS_EXIST_CACHE_SECS`.
static PROJECTS_KNOWN: AtomicBool = AtomicBool::new(false);
static LAST_NEGATIVE: Mutex<Option<Instant>> = Mutex::new(None);

/// Whether at least one project exists (cached, see above).
pub async fn projects_exist(state: &AppState) -> Result<bool> {
    if PROJECTS_KNOWN.load(Ordering::Relaxed) {
        return Ok(true);
    }
    let fresh = LAST_NEGATIVE
        .lock()
        .ok()
        .and_then(|last| *last)
        .is_some_and(|at| at.elapsed() < Duration::from_secs(PROJECTS_EXIST_CACHE_SECS));
    if fresh {
        return Ok(false);
    }
    let exists = state.db.count_projects().await? > 0;
    note_projects_exist(exists);
    Ok(exists)
}

/// Record what a fresh read of the `projects` table said (the create
/// handler calls it right away, so the first project is seen at once by
/// the instance that created it).
pub fn note_projects_exist(exists: bool) {
    if exists {
        PROJECTS_KNOWN.store(true, Ordering::Relaxed);
    } else if let Ok(mut last) = LAST_NEGATIVE.lock() {
        *last = Some(Instant::now());
    }
}

/// `Err` while no project exists: the caller answers `409 Create a project
/// first` (spec §12).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NoProjectYet;

impl std::fmt::Display for NoProjectYet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Create a project first")
    }
}

pub async fn require_any_project(state: &AppState) -> Result<Result<(), NoProjectYet>> {
    Ok(if projects_exist(state).await? {
        Ok(())
    } else {
        Err(NoProjectYet)
    })
}
