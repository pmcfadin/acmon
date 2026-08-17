//! Seam 1 — turning an observation of the world into a snapshot.

use crate::detect::embedded_detectors;
use crate::workspace::{
    namespace_for, recorded_namespace, NamespaceUnmatched, Workspace, WorkspaceUnknown,
};
use crate::world::{PathUnavailable, Resources, ResourcesUnavailable, World, WorldError};

/// One agent CLI process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub pid: i32,
    /// Which CLI this is, taken from the detector that matched.
    pub cli: String,
    /// What this session has consumed, or why that could not be read.
    ///
    /// A session with an unreadable ledger is still a session, and is still listed. It
    /// is never dropped and never shown as idle.
    pub resources: Result<Resources, ResourcesUnavailable>,
    /// Which directory this session is working in, or why that is unknown.
    pub workspace: Result<Workspace, WorkspaceUnknown>,
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
    UntrustworthySnapshot {
        observer_pid: i32,
    },
}

impl std::fmt::Display for CollectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CollectError::World(err) => write!(f, "{}", err),
            CollectError::UntrustworthySnapshot { observer_pid } => {
                write!(
                    f,
                    "process snapshot incomplete (observer {} not in its own result)",
                    observer_pid
                )
            }
        }
    }
}

impl std::error::Error for CollectError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            CollectError::World(err) => Some(err),
            CollectError::UntrustworthySnapshot { .. } => None,
        }
    }
}

/// Work out a session's workspace from its working directory.
///
/// Three outcomes, and they are deliberately distinct: the workspace is known and has a
/// recorded transcript; it is known and has none; or it is not known at all. Collapsing
/// any two of them would report a directory the session is not in, or none at all.
fn workspace_of(
    cwd: &Result<String, PathUnavailable>,
    recorded: &Result<Vec<String>, WorldError>,
) -> Result<Workspace, WorkspaceUnknown> {
    let path = cwd.as_ref().map_err(WorkspaceUnknown::from)?;

    let namespace = match recorded {
        Ok(namespaces) => {
            recorded_namespace(path, namespaces).ok_or_else(|| NamespaceUnmatched::NotRecorded {
                mapped: namespace_for(path),
            })
        }
        // Could not look, which is not the same as looked and found nothing.
        Err(why) => Err(NamespaceUnmatched::ListingFailed(why.to_string())),
    };

    Ok(Workspace {
        path: path.clone(),
        namespace,
    })
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

    // Listed once, not once per session: the answer cannot change between sessions in a
    // single collection, and a directory listing is not free.
    let recorded = world.recorded_namespaces();

    let mut sessions: Vec<Session> = observation
        .records
        .iter()
        .filter_map(|record| {
            let exe = record.exe_path.as_ref().ok()?;
            let detector = detectors.iter().find(|d| d.matches(exe))?;
            Some(Session {
                pid: record.pid,
                cli: detector.id.clone(),
                resources: world.resources(record.pid),
                workspace: workspace_of(&record.cwd, &recorded),
            })
        })
        .collect();

    sessions.sort_by_key(|s| s.pid);
    Ok(Snapshot { sessions })
}
