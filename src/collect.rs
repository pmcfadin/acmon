//! Seam 1 — turning an observation of the world into a snapshot.

use std::time::{Duration, SystemTime};

use crate::detect::embedded_detectors;
use crate::liveness::{classify, Observation, Thresholds, Verdict};
use crate::vcs::WorkspaceState;
use crate::workspace::{
    namespace_for, recorded_namespace, NamespaceResolution, NamespaceUnmatched, Workspace,
    WorkspaceUnknown,
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

/// One workspace, as the at-risk panel needs to see it.
///
/// A workspace is a directory an agent works in. Being a git repository — or a linked
/// worktree of one — is an *attribute* recorded here, never a precondition for appearing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceReport {
    /// The repository root when one was found, otherwise the directory as observed.
    ///
    /// The root rather than the observed directory, because several processes working in
    /// different subdirectories of one repository are in ONE workspace, and reporting them
    /// separately would inflate the panel with duplicates of the same risk.
    pub path: String,
    /// Whether this workspace holds work that exists nowhere else, and whether anything is
    /// driving it.
    pub state: WorkspaceState,
    /// A linked worktree rather than a repository's primary working tree.
    ///
    /// Recorded because it is true and because it tells a human where the real `.git` is —
    /// and because two thirds of the git workspaces on the machine behind
    /// `docs/observability-mechanics.md` §4.6 are linked worktrees, so a design that
    /// treated them as a special case would ignore the majority of them.
    pub linked_worktree: bool,
    /// How many entries version control reported as uncommitted.
    ///
    /// `None` exactly when `state` is [`WorkspaceState::Unknown`], whose reason is the
    /// explanation. It is never `Some(0)` standing in for "could not tell".
    pub uncommitted_entries: Option<usize>,
}

/// Everything observed in one collection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub sessions: Vec<Session>,
    /// Every workspace that was located, whatever its state.
    ///
    /// Deliberately includes CLEAN workspaces. The at-risk panel has to be able to say how
    /// many workspaces it checked, because an empty panel must read as "checked and clear"
    /// rather than as possibly broken.
    pub workspaces: Vec<WorkspaceReport>,
    /// Recorded transcript namespaces that could not be turned into a directory, and what
    /// the search concluded about each.
    ///
    /// Never silently dropped. A workspace whose path could not be established has an
    /// unknown version-control state, and unknown is not clean. Of 109 namespaces on the
    /// machine behind the mechanics document, 77 land here — mostly deleted worktrees and
    /// expired temporary directories.
    pub unlocated: Vec<(String, NamespaceResolution)>,
    /// Whether the directory sweep that finds workspaces ran to completion.
    ///
    /// `false` means coverage is partial and the panel must say so. A truncated list of
    /// at-risk workspaces presented as exhaustive is the calm, plausible, wrong answer this
    /// project exists to remove.
    pub sweep_complete: bool,
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

/// Whether a candidate path lies inside a workspace path.
///
/// `candidate` is inside `workspace_path` when it equals it or is a subdirectory of it.
/// Compared case-insensitively because APFS is case-insensitive but case-preserving.
///
/// Extracted from `work_running_in` and reused by workspace classification, because a
/// workspace counted as driven by one rule and stranded by the other would be the
/// Duplicated Code smell — and worse, the two copies could drift so that detection and
/// classification disagree about what "inside" means.
fn is_inside(candidate: &str, workspace_path: &str) -> bool {
    candidate.eq_ignore_ascii_case(workspace_path)
        || candidate.len() > workspace_path.len()
            && candidate[..workspace_path.len()].eq_ignore_ascii_case(workspace_path)
            && candidate.as_bytes()[workspace_path.len()] == b'/'
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
    let session_pid = match session_identity {
        Identity::Process { pid } => Some(*pid),
        Identity::Transcript { .. } => None,
    };

    records.iter().any(|record| {
        (session_pid != Some(record.pid))
            && record
                .cwd
                .as_deref()
                .map(|cwd| is_inside(cwd, workspace_path))
                .unwrap_or(false)
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

                // A transcript-derived Claude session's workspace comes from resolving the
                // namespace. The namespace mapping is not invertible (three characters
                // collapse to `-`), so this is done as a verified search over directories
                // that actually exist.
                let workspace = match world.resolve_namespace(namespace) {
                    NamespaceResolution::Resolved(path) => Ok(Workspace {
                        path,
                        namespace: Ok(namespace.clone()),
                    }),
                    NamespaceResolution::Ambiguous(candidates) => {
                        Err(WorkspaceUnknown::Ambiguous {
                            candidates: candidates.len(),
                        })
                    }
                    NamespaceResolution::NoLongerExists => Err(WorkspaceUnknown::WorkspaceGone),
                    NamespaceResolution::SearchExhausted => Err(WorkspaceUnknown::SearchIncomplete),
                };

                // For checking if work is running: when the workspace resolved to a path, use
                // that directly; when it did not, fall back to checking if any process's cwd
                // maps forward to the namespace.
                let work_running = workspace
                    .as_ref()
                    .ok()
                    .map(|w| work_running_in(&w.path, &identity, &observation.records))
                    .unwrap_or_else(|| {
                        work_running_in_namespace(namespace, &observation.records, detectors)
                    });

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
pub fn collect(
    world: &dyn World,
    now: SystemTime,
    thresholds: &Thresholds,
) -> Result<Snapshot, CollectError> {
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
                thresholds,
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
        thresholds,
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

    // --- Workspace discovery and classification ---
    //
    // Deliberately NOT bounded by the liveness discovery window that bounds session
    // discovery. A workspace that has been stranded for a week is *more* at risk, not less.
    // This is what makes the panel a durable safety net even though a stalled session drops
    // out of the session table after the window.

    // Source 1: Each session's own workspace path.
    // Source 2: Every observed process working directory.
    // Both land in the same set because a process's cwd might not be any session's workspace.
    let mut candidate_paths: Vec<String> = sessions
        .iter()
        .filter_map(|s| s.workspace.as_ref().ok().map(|w| w.path.clone()))
        .chain(
            observation
                .records
                .iter()
                .filter_map(|r| r.cwd.as_ref().ok().cloned()),
        )
        .collect();

    // Source 3: Each recorded Claude namespace, resolved via `resolve_namespace`.
    // Namespaces that do NOT resolve are remembered separately as unlocated.
    let mut unlocated = Vec::new();
    if let Ok(namespaces) = &sources.claude_namespaces {
        for namespace in namespaces {
            match world.resolve_namespace(namespace) {
                NamespaceResolution::Resolved(path) => {
                    candidate_paths.push(path);
                }
                resolution => {
                    // A namespace that did not resolve goes into `unlocated`. Never silently
                    // drop it: a workspace whose path could not be established has an unknown
                    // version-control state, and unknown is not clean.
                    unlocated.push((namespace.clone(), resolution));
                }
            }
        }
    }

    // Source 4: Each Codex-recorded session workspace.
    if let Ok(codex_sessions_list) = &sources.codex_sessions {
        for codex_session in codex_sessions_list {
            candidate_paths.push(codex_session.workspace.clone());
        }
    }

    // Observational discovery alone — the first four sources — was measured to find 8 dirty
    // workspaces on the target machine. The sweep below finds 14, adding 6 more, including
    // `presto_testing` with 28 uncommitted entries — the largest pile of at-risk work on the
    // machine and the same shape as the 27-file loss that motivated this project. The sweep
    // is not an optimisation; it is what makes the safety net honest.

    // Source 5: A sweep of the neighbourhoods the known repositories live in.
    //
    // The roots are the parent directories of those candidates that turned out to **be
    // repositories** — not of every candidate. That distinction is load-bearing and was
    // found by running it: many candidates are ordinary directories such as the home folder
    // and `/private/tmp`, and sweeping *their* parents walks most of the disk. Measured with
    // every candidate's parent, the sweep exhausted its budget and had to report partial
    // coverage; derived from repositories only, it visits 122 directories, finds 70
    // workspaces in 9 ms, and completes. No configuration is required either way.
    //
    // `repository_root` is asked again here rather than threaded down from above: it is a
    // handful of `stat` calls, and duplicating its answer in a second structure is how the
    // two would come to disagree.
    let mut sweep_roots: Vec<String> = candidate_paths
        .iter()
        .filter_map(|path| world.repository_root(path).map(|(root, _)| root))
        .filter_map(|repository| {
            let parent = std::path::Path::new(&repository).parent()?;
            let parent_str = parent.to_str()?;
            // Never sweep `/` itself.
            if parent_str.is_empty() || parent_str == "/" {
                None
            } else {
                Some(parent_str.to_string())
            }
        })
        .collect();
    sweep_roots.sort();
    sweep_roots.dedup();

    let sweep = world.sweep_for_repositories(&sweep_roots);
    candidate_paths.extend(sweep.repositories.iter().map(|(path, _)| path.clone()));

    // Deduplicate candidates by path, **case-insensitively**, because APFS is
    // case-insensitive but case-preserving and the same workspace arrives spelled
    // differently from different sources. Keep the first spelling seen.
    let mut seen_lowercase: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut unique_candidates = Vec::new();
    for candidate in candidate_paths {
        let lowercase = candidate.to_lowercase();
        if seen_lowercase.insert(lowercase) {
            unique_candidates.push(candidate);
        }
    }

    // Map each candidate path through `repository_root`. When it names a root, the **root**
    // is the workspace: several processes in different subdirectories of one repository are
    // ONE workspace, and listing them separately would inflate the panel with duplicates of
    // one risk.
    //
    // When `repository_root` finds nothing, **still keep the path as a candidate.** Being a
    // worktree is treated as an attribute of a workspace, not a precondition for discovering
    // one. It will classify as `Unknown(NotVersionControlled)`, which is not at risk.
    let workspace_candidates: Vec<(String, bool)> = unique_candidates
        .iter()
        .map(|candidate| {
            world
                .repository_root(candidate)
                .unwrap_or_else(|| (candidate.clone(), false))
        })
        .collect();

    // Deduplicate again by root, case-insensitively, keeping the first spelling.
    seen_lowercase.clear();
    let mut unique_workspace_candidates = Vec::new();
    for (root, linked) in workspace_candidates {
        let lowercase = root.to_lowercase();
        if seen_lowercase.insert(lowercase) {
            unique_workspace_candidates.push((root, linked));
        }
    }

    // Sort for stable output.
    unique_workspace_candidates.sort_by(|a, b| a.0.cmp(&b.0));

    // Call `vcs_facts_batch` **once** with all candidate paths — not `vcs_facts` in a loop.
    // It is concurrent, and the sequential cost was measured at 5.0 s for 70 workspaces.
    let workspace_paths: Vec<String> = unique_workspace_candidates
        .iter()
        .map(|(path, _)| path.clone())
        .collect();
    let vcs_facts_results = world.vcs_facts_batch(&workspace_paths);

    // For each candidate, compute `session_driving`: whether any session whose identity is
    // `Identity::Process { .. }` has a workspace path lying **within** this workspace.
    //
    // This rests on a live process rather than on the liveness verdict, because process
    // residence is directly observed, whereas a WAITING verdict is inferred from silence,
    // so a DIRTY-DRIVEN classification never depends on a guess.
    let workspaces: Vec<WorkspaceReport> = unique_workspace_candidates
        .iter()
        .zip(vcs_facts_results.iter())
        .map(|((path, linked_from_root), facts)| {
            let session_driving = sessions.iter().any(|session| {
                matches!(&session.identity, Identity::Process { .. })
                    && session
                        .workspace
                        .as_ref()
                        .ok()
                        .map(|w| is_inside(&w.path, path))
                        .unwrap_or(false)
            });

            let state = crate::vcs::classify(facts, session_driving);

            // `linked_worktree` from the facts when they are `Ok`, otherwise from whatever
            // `repository_root` reported for that candidate, otherwise `false`.
            let linked_worktree = facts
                .as_ref()
                .map(|f| f.linked_worktree)
                .unwrap_or(*linked_from_root);

            // `uncommitted_entries` as `Some(n)` only when the facts are `Ok` — it must be
            // `None` whenever `state` is `Unknown`, and never `Some(0)` standing in for
            // "could not tell".
            let uncommitted_entries = facts.as_ref().ok().map(|f| f.uncommitted_entries);

            WorkspaceReport {
                path: path.clone(),
                state,
                linked_worktree,
                uncommitted_entries,
            }
        })
        .collect();

    Ok(Snapshot {
        sessions,
        workspaces,
        unlocated,
        sweep_complete: sweep.complete,
    })
}
