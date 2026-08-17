//! Seam 1 — turning an observation of the world into a snapshot.

use crate::detect::embedded_detectors;
use crate::workspace::{
    namespace_for, recorded_namespace, NamespaceUnmatched, Workspace, WorkspaceUnknown,
};
use crate::world::{
    CodexSession, PathUnavailable, Resources, ResourcesUnavailable, World, WorldError,
};

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

/// Attribution sources for workspace resolution, bundled together so they travel as
/// one unit rather than lengthening every parameter list.
struct AttributionSources {
    /// Claude Code's recorded transcript namespaces.
    claude_namespaces: Result<Vec<String>, WorldError>,
    /// Codex's recently active sessions with their workspaces.
    codex_sessions: Result<Vec<CodexSession>, WorldError>,
}

/// Work out a session's workspace from its working directory and CLI type.
///
/// Four outcomes, and they are deliberately distinct: the workspace is known and has a
/// recorded transcript; it is known and has none; it is not known at all; or the CLI is
/// not one we know how to attribute. Collapsing any two of them would report a directory
/// the session is not in, or none at all, or attribute using the wrong store.
fn workspace_of(
    cli: &str,
    cwd: &Result<String, PathUnavailable>,
    sources: &AttributionSources,
) -> Result<Workspace, WorkspaceUnknown> {
    let path = cwd.as_ref().map_err(WorkspaceUnknown::from)?;

    // Each CLI decides both what its workspace *is* and what identifies its transcript,
    // so the two travel together out of this match.
    let (workspace_path, namespace) = match cli {
        "claude" => {
            // Claude Code records a transcript directory per workspace, so the working
            // directory is the workspace and the namespace is derived from it.
            let namespace = match &sources.claude_namespaces {
                Ok(namespaces) => recorded_namespace(path, namespaces).ok_or_else(|| {
                    NamespaceUnmatched::NotRecorded {
                        mapped: namespace_for(path),
                    }
                }),
                // Could not look, which is not the same as looked and found nothing.
                Err(why) => Err(NamespaceUnmatched::ListingFailed(why.to_string())),
            };
            (path.clone(), namespace)
        }
        "codex" => {
            // Codex records no directory per workspace, so the transcript itself is the
            // authority for where the session is working, and the working directory is
            // only the link to it. Matched case-insensitively because APFS is
            // case-insensitive but case-preserving, and the two sources may therefore
            // disagree about capitalisation while naming one directory.
            match &sources.codex_sessions {
                Ok(sessions) => {
                    match sessions
                        .iter()
                        .find(|session| session.workspace.eq_ignore_ascii_case(path))
                    {
                        // The transcript's spelling, not the process's: this value comes
                        // from the recorded session, which is what makes it the
                        // transcript's answer rather than the kernel's.
                        Some(session) => (session.workspace.clone(), Ok(session.id.clone())),
                        None => (
                            path.clone(),
                            Err(NamespaceUnmatched::NotRecorded {
                                mapped: path.clone(),
                            }),
                        ),
                    }
                }
                Err(why) => (
                    path.clone(),
                    Err(NamespaceUnmatched::ListingFailed(why.to_string())),
                ),
            }
        }
        _ => {
            // A CLI that is neither claude nor codex. Do not fall back to either rule —
            // that would attribute a session using the wrong store. This becomes reachable
            // as soon as anyone adds a detector to detectors.toml (ticket #12 exists to
            // allow that).
            (
                path.clone(),
                Err(NamespaceUnmatched::UnknownCli(cli.to_string())),
            )
        }
    };

    Ok(Workspace {
        path: workspace_path,
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

    // Read both attribution sources once per collection, not once per session: the
    // answers cannot change between sessions, and each is a directory listing or file
    // read that is not free.
    let sources = AttributionSources {
        claude_namespaces: world.recorded_namespaces(),
        codex_sessions: world.codex_sessions(),
    };

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
                workspace: workspace_of(&detector.id, &record.cwd, &sources),
            })
        })
        .collect();

    sessions.sort_by_key(|s| s.pid);
    Ok(Snapshot { sessions })
}
