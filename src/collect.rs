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
    /// The process enumeration did not contain the process that produced it, so it
    /// died part-way. Its contents prove nothing — in particular, the absence of
    /// sessions in it does not mean there are none.
    UntrustworthySnapshot { observer_pid: i32 },
}

/// Collect a snapshot of the agent sessions on this machine.
pub fn collect(world: &dyn World) -> Result<Snapshot, CollectError> {
    let observation = world.process_snapshot().map_err(CollectError::World)?;

    // Check the observation against itself before drawing any conclusion from it.
    // Reasoning from absence is only safe once we know we could see anything at all.
    if !observation.contains_observer() {
        return Err(CollectError::UntrustworthySnapshot {
            observer_pid: observation.observer_pid,
        });
    }

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
