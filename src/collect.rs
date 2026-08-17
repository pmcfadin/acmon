//! Seam 1 — turning an observation of the world into a snapshot.

use std::time::{Duration, SystemTime};

use crate::detect::embedded_detectors;
use crate::liveness::{classify, Observation, Thresholds, Verdict};
use crate::workspace::{
    namespace_for, recorded_namespace, NamespaceUnmatched, Workspace, WorkspaceUnknown,
};
use crate::world::{
    CodexSession, PathUnavailable, Resources, ResourcesUnavailable, World, WorldError,
};

/// A session's identity: either a live process, or a transcript without one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Identity {
    /// Found in the process enumeration.
    Process { pid: i32 },
    /// Found in the transcript store, with no live process. For Claude this is the
    /// namespace directory name; for Codex it is the session id.
    Transcript { recorded_as: String },
}

/// One agent CLI session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    /// How this session was discovered: either from a process, or from a transcript.
    pub identity: Identity,
    /// Which CLI this is, taken from the detector that matched.
    pub cli: String,
    /// What this session has consumed, or why that could not be read.
    ///
    /// A session with an unreadable ledger is still a session, and is still listed. It
    /// is never dropped and never shown as idle.
    pub resources: Result<Resources, ResourcesUnavailable>,
    /// Which directory this session is working in, or why that is unknown.
    pub workspace: Result<Workspace, WorkspaceUnknown>,
    /// Whether this session is working, waiting, stalled, or beyond telling — and which
    /// observation produced that answer, so an inference never reads as an assertion.
    pub liveness: Verdict,
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

/// How long a session's transcript has been silent, or `None` if that cannot be told.
///
/// `None` is not "no silence" — it is the absence of an answer, and the state machine
/// turns it into UNKNOWN rather than into a verdict.
fn silence_of(
    session_workspace: &Result<Workspace, WorkspaceUnknown>,
    cli: &str,
    sources: &AttributionSources,
    world: &dyn World,
    now: SystemTime,
) -> Option<Duration> {
    let workspace = session_workspace.as_ref().ok()?;
    let namespace = workspace.namespace.as_ref().ok()?;

    let last_activity = match cli {
        // Claude Code's namespace is a directory of transcripts; its activity is the
        // newest modification time among them.
        "claude" => world.namespace_activity(namespace).ok()?,
        // Codex's index already reports when each session was last updated, so no
        // further read is needed.
        "codex" => {
            sources
                .codex_sessions
                .as_ref()
                .ok()?
                .iter()
                .find(|candidate| candidate.id == *namespace)?
                .last_activity
        }
        _ => return None,
    };

    // A modification time later than now means the clock and the filesystem disagree,
    // which happens with skew. The transcript changed at or after this instant either
    // way, so the honest reading is "just now" rather than a refusal.
    Some(now.duration_since(last_activity).unwrap_or(Duration::ZERO))
}

/// Whether any process other than the session itself is working in its workspace.
///
/// This is what stops a build or a test run from being mistaken for a dead session:
/// legitimate silence of several minutes was measured, and it is caused by exactly these.
/// A subdirectory counts, because build and test runners commonly chdir into one.
///
/// Costs nothing extra — every process's working directory was already read in the same
/// pass that found the sessions.
fn work_running_in(
    workspace_path: &str,
    session_identity: &Identity,
    records: &[crate::ProcessRecord],
) -> bool {
    let inside = |candidate: &str| {
        candidate.eq_ignore_ascii_case(workspace_path)
            || candidate.len() > workspace_path.len()
                && candidate[..workspace_path.len()].eq_ignore_ascii_case(workspace_path)
                && candidate.as_bytes()[workspace_path.len()] == b'/'
    };

    let session_pid = match session_identity {
        Identity::Process { pid } => Some(*pid),
        Identity::Transcript { .. } => None,
    };

    records.iter().any(|record| {
        (session_pid != Some(record.pid)) && record.cwd.as_deref().map(inside).unwrap_or(false)
    })
}

/// Whether any process is working in a workspace that maps to a given namespace.
///
/// For transcript-derived Claude sessions, we can't reverse the namespace mapping to get
/// a path, but we can check if any process's cwd maps forward to the namespace.
fn work_running_in_namespace(
    namespace: &str,
    records: &[crate::ProcessRecord],
    detectors: &[crate::Detector],
) -> bool {
    records.iter().any(|record| {
        // Only consider non-agent processes as "work". An agent process in that
        // workspace is a session, not work.
        let is_agent = record
            .exe_path
            .as_ref()
            .ok()
            .and_then(|exe| detectors.iter().find(|d| d.matches(exe)))
            .is_some();

        !is_agent
            && record
                .cwd
                .as_ref()
                .ok()
                .map(|cwd| namespace_for(cwd).eq_ignore_ascii_case(namespace))
                .unwrap_or(false)
    })
}

/// Discover sessions from transcript stores that have no live process.
///
/// Only transcripts active within a reasonable window are candidates. The window is
/// 2x the stall threshold to catch sessions that are solidly stalled while still
/// bounding the work — a transcript silent for 25 hours on a 12-hour threshold is
/// genuinely abandoned and need not be checked every collection.
fn transcript_derived_sessions(
    sources: &AttributionSources,
    process_sessions: &[Session],
    observation: &crate::ProcessSnapshot,
    world: &dyn World,
    now: SystemTime,
    thresholds: &Thresholds,
    detectors: &[crate::Detector],
) -> Vec<Session> {
    let mut transcript_sessions = Vec::new();

    // Only transcripts active within this window are candidates, which is what bounds the
    // work — otherwise every namespace ever recorded would be a candidate forever.
    //
    // Twice the stall threshold, not once: a session becomes STALLED the moment its silence
    // passes `stall`, so a window equal to `stall` would exclude it at exactly the instant
    // it became worth reporting, and STALLED would be unreachable again.
    //
    // The ceiling has a consequence worth knowing: a session silent for longer than this
    // window drops out of the table entirely rather than staying STALLED. It is reported
    // while it is news and then it is gone. Remembering it for longer means persisting
    // state between runs, which is ticket #8.
    let discovery_window = thresholds.stall * 2;

    // Claude Code transcripts: one namespace directory per workspace.
    if let Ok(namespaces) = &sources.claude_namespaces {
        for namespace in namespaces {
            // Skip if a process-derived session already claims this namespace.
            let already_claimed = process_sessions.iter().any(|session| {
                session.cli == "claude"
                    && session
                        .workspace
                        .as_ref()
                        .ok()
                        .and_then(|w| w.namespace.as_ref().ok())
                        .map(|n| n == namespace)
                        .unwrap_or(false)
            });
            if already_claimed {
                continue;
            }

            // Only transcripts active within the discovery window are candidates.
            let last_activity = match world.namespace_activity(namespace) {
                Ok(time) => time,
                Err(_) => continue, // Cannot determine activity, skip it.
            };
            let silence = now.duration_since(last_activity).unwrap_or(Duration::ZERO);
            if silence <= discovery_window {
                let identity = Identity::Transcript {
                    recorded_as: namespace.clone(),
                };

                // A transcript-derived Claude session has no derivable workspace path,
                // because the namespace mapping is not invertible. Report the reason.
                let workspace = Err(WorkspaceUnknown::NotInvertible);

                // For checking if work is running, we check if any non-agent process has
                // a cwd that maps to this namespace.
                let work_running =
                    work_running_in_namespace(namespace, &observation.records, detectors);

                let liveness = classify(
                    &Observation {
                        silence: Some(silence),
                        process_resident: false,
                        work_running_in_workspace: work_running,
                        snapshot_trustworthy: true,
                    },
                    thresholds,
                );

                transcript_sessions.push(Session {
                    identity,
                    cli: "claude".to_string(),
                    resources: Err(ResourcesUnavailable::ProcessExited),
                    workspace,
                    liveness,
                });
            }
        }
    }

    // Codex transcripts: session index reports workspace and last_activity.
    if let Ok(codex_sessions) = &sources.codex_sessions {
        for codex_session in codex_sessions {
            // Skip if a process-derived session already claims this session id.
            let already_claimed = process_sessions.iter().any(|session| {
                session.cli == "codex"
                    && session
                        .workspace
                        .as_ref()
                        .ok()
                        .and_then(|w| w.namespace.as_ref().ok())
                        .map(|n| n == &codex_session.id)
                        .unwrap_or(false)
            });
            if already_claimed {
                continue;
            }

            // Only sessions active within the discovery window are candidates.
            let silence = now
                .duration_since(codex_session.last_activity)
                .unwrap_or(Duration::ZERO);
            if silence <= discovery_window {
                let identity = Identity::Transcript {
                    recorded_as: codex_session.id.clone(),
                };

                // A transcript-derived Codex session has its workspace recorded in the
                // transcript, so it is known.
                let workspace = Ok(Workspace {
                    path: codex_session.workspace.clone(),
                    namespace: Ok(codex_session.id.clone()),
                });

                let liveness = classify(
                    &Observation {
                        silence: Some(silence),
                        process_resident: false,
                        work_running_in_workspace: workspace
                            .as_ref()
                            .ok()
                            .map(|w| work_running_in(&w.path, &identity, &observation.records))
                            .unwrap_or(false),
                        snapshot_trustworthy: true,
                    },
                    thresholds,
                );

                transcript_sessions.push(Session {
                    identity,
                    cli: "codex".to_string(),
                    resources: Err(ResourcesUnavailable::ProcessExited),
                    workspace,
                    liveness,
                });
            }
        }
    }

    transcript_sessions
}

/// Collect a snapshot of the agent sessions on this machine.
///
/// `now` is injected rather than read from a clock here, so that a liveness verdict is
/// deterministic under test rather than depending on when the test happened to run.
pub fn collect(world: &dyn World, now: SystemTime) -> Result<Snapshot, CollectError> {
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

    let thresholds = Thresholds::default();

    // Sessions from processes — discovered by scanning the process table.
    let process_sessions: Vec<Session> = observation
        .records
        .iter()
        .filter_map(|record| {
            let exe = record.exe_path.as_ref().ok()?;
            let detector = detectors.iter().find(|d| d.matches(exe))?;
            let workspace = workspace_of(&detector.id, &record.cwd, &sources);
            let identity = Identity::Process { pid: record.pid };

            let liveness = classify(
                &Observation {
                    silence: silence_of(&workspace, &detector.id, &sources, world, now),
                    // This session was found *in* the enumeration, so its process was
                    // observed to be there.
                    process_resident: true,
                    work_running_in_workspace: workspace
                        .as_ref()
                        .ok()
                        .map(|w| work_running_in(&w.path, &identity, &observation.records))
                        .unwrap_or(false),
                    // The enumeration was checked against itself above, and a collection
                    // over an untrustworthy one never gets this far.
                    snapshot_trustworthy: true,
                },
                &thresholds,
            );

            Some(Session {
                identity: identity.clone(),
                cli: detector.id.clone(),
                resources: world.resources(record.pid),
                workspace,
                liveness,
            })
        })
        .collect();

    // Sessions from transcripts — discovered by scanning the transcript stores.
    // Only transcripts active within the stall threshold are candidates, and only
    // those not already claimed by a process-derived session.
    let transcript_sessions = transcript_derived_sessions(
        &sources,
        &process_sessions,
        &observation,
        world,
        now,
        &thresholds,
        &detectors,
    );

    let mut sessions = process_sessions;
    sessions.extend(transcript_sessions);

    // Sort by identity for stable output: processes by pid, transcripts by recorded_as.
    sessions.sort_by(|a, b| match (&a.identity, &b.identity) {
        (Identity::Process { pid: a_pid }, Identity::Process { pid: b_pid }) => a_pid.cmp(b_pid),
        (Identity::Transcript { recorded_as: a }, Identity::Transcript { recorded_as: b }) => {
            a.cmp(b)
        }
        (Identity::Process { .. }, Identity::Transcript { .. }) => std::cmp::Ordering::Less,
        (Identity::Transcript { .. }, Identity::Process { .. }) => std::cmp::Ordering::Greater,
    });

    Ok(Snapshot { sessions })
}
