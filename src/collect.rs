//! Seam 1 — turning an observation of the world into a snapshot.

use crate::detect::embedded_detectors;
use crate::world::{World, WorldError};

/// One agent CLI process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub pid: i32,
    /// Which CLI this is, taken from the detector that matched.
    pub cli: String,
}

/// Everything observed in one collection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub sessions: Vec<Session>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CollectError {
    World(WorldError),
}

/// Collect a snapshot of the agent sessions on this machine.
pub fn collect(world: &dyn World) -> Result<Snapshot, CollectError> {
    let observation = world.process_snapshot().map_err(CollectError::World)?;
    let detectors = embedded_detectors();

    let mut sessions: Vec<Session> = observation
        .records
        .iter()
        .filter_map(|record| {
            let exe = record.exe_path.as_deref()?;
            let detector = detectors.iter().find(|d| d.matches(exe))?;
            Some(Session {
                pid: record.pid,
                cli: detector.id.clone(),
            })
        })
        .collect();

    sessions.sort_by_key(|s| s.pid);
    Ok(Snapshot { sessions })
}
