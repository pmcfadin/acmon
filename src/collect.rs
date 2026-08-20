//! Seam 1 — turning an observation of the world into a snapshot.

use std::time::{Duration, SystemTime};

use crate::deliver::DeliveryReport;
use crate::liveness::{classify, Method, Observation, Thresholds, Verdict};
use crate::memory::{self, Degraded, Forgotten, Memory, Reading, Sighting};
use crate::notify;
use crate::vcs::WorkspaceState;
use crate::workspace::{
    namespace_for, recorded_namespace, NamespaceResolution, NamespaceUnmatched, Workspace,
    WorkspaceUnknown,
};
use crate::world::{
    CodexSession, NotifyConfig, NotifyOutcome, PathUnavailable, Resources, ResourcesUnavailable,
    StateRead, World, WorldError,
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
    /// The last reading taken while this session's process was alive, when there is one and
    /// [`Session::resources`] has none.
    ///
    /// This is what makes a session's lifetime totals survive its exit: almost all of an
    /// agent's cost is in its children, and that total is only knowable from the process
    /// that reaped them. Once it is gone the figure exists nowhere else on the machine.
    ///
    /// Present ONLY alongside an `Err` in `resources`. A live reading is never shadowed by a
    /// remembered one, and a caller must never have to work out which of two figures is the
    /// current one.
    pub last_reading: Option<Reading>,
    /// Which directory this session is working in, or why that is unknown.
    pub workspace: Result<Workspace, WorkspaceUnknown>,
    /// Whether this session is working, waiting, stalled, or beyond telling — and which
    /// observation produced that answer, so an inference never reads as an assertion.
    pub liveness: Verdict,
}

/// Why a session's liveness could not be determined.
///
/// [`Method`] already records which rule produced UNKNOWN. That is not the question a reader
/// has. Theirs is what about *this machine* put the verdict out of reach, and above all
/// whether it is a fault that can be fixed or a limit that has to be lived with — because a
/// bare UNKNOWN is identical either way, and the two need opposite responses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LivenessUnknown {
    /// No transcript store is known for this CLI at all, so there is no file whose
    /// modification time could be read and no verdict to be had.
    ///
    /// A **limit**, not a fault: a detector says which executables are an agent and nothing
    /// more, so no amount of configuration buys a state here. Carries the CLI's id, because
    /// the whole point is being able to see which store is missing.
    NoTranscriptStore { cli: String },
    /// A transcript store is known for this CLI, and it did not yield an activity time —
    /// nothing was attributed to this workspace, the store would not be listed, or the
    /// attributed transcript's own modification time could not be read.
    ///
    /// A **fault**: something that exists failed to answer, so it can be investigated and
    /// may well answer on the next run.
    ActivityUnreadable { why: String },
    /// The session's working directory could not be read, so there is nothing to look a
    /// transcript up by. Carries the workspace's own reason rather than restating it.
    WorkspaceUnknown { why: String },
    /// The process enumeration could not be reasoned from, so a missing process is not
    /// evidence of an absent one.
    SnapshotUntrustworthy,
    /// Silent with no resident process, but not for long enough to call it stalled.
    TooSoonToTell,
}

impl LivenessUnknown {
    /// Whether this is a structural limit of what can be observed rather than a fault that
    /// was found.
    ///
    /// The distinction is what a reader acts on. A fault is worth investigating and may clear
    /// by itself; a limit will not clear on any later run, and — because [`Waiting`] is the
    /// only session state ever announced (see [`notify`](crate::notify)) and reaching it needs
    /// a silence measurement — a session held by one is monitored and never alerts. That
    /// consequence is otherwise only knowable from an alert that never arrives.
    ///
    /// [`Waiting`]: crate::liveness::State::Waiting
    pub fn is_structural(&self) -> bool {
        matches!(self, LivenessUnknown::NoTranscriptStore { .. })
    }
}

impl std::fmt::Display for LivenessUnknown {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LivenessUnknown::NoTranscriptStore { cli } => write!(
                f,
                "no transcript store is known for CLI {cli}, so its silence cannot be measured"
            ),
            LivenessUnknown::ActivityUnreadable { why } => {
                write!(
                    f,
                    "its transcript's activity could not be established: {why}"
                )
            }
            LivenessUnknown::WorkspaceUnknown { why } => write!(
                f,
                "its workspace is unknown ({why}), so no transcript can be attributed to it"
            ),
            LivenessUnknown::SnapshotUntrustworthy => write!(
                f,
                "the process enumeration was incomplete, so its absence is not evidence"
            ),
            LivenessUnknown::TooSoonToTell => write!(
                f,
                "it has been silent with no resident process, but not for long enough to call it \
                 stalled"
            ),
        }
    }
}

impl Session {
    /// Why this session's liveness could not be determined, or `None` when it was.
    ///
    /// Derived rather than stored, because everything it needs was already recorded by the
    /// collection. That was the defect #22 reports: the reason existed, travelled as far as
    /// [`Workspace::namespace`], and was then dropped by the only code that could have shown
    /// it.
    pub fn liveness_unknown(&self) -> Option<LivenessUnknown> {
        match self.liveness.method {
            Method::TranscriptActivityUnknown => Some(match &self.workspace {
                // No workspace, so nothing to look a transcript up by. The workspace column
                // carries this reason already; it is repeated against the state because a
                // reader looking at UNKNOWN should not have to work out which other column
                // happens to explain it.
                Err(why) => LivenessUnknown::WorkspaceUnknown {
                    why: why.to_string(),
                },
                Ok(workspace) => match &workspace.namespace {
                    // The limit. A detector names executables; nothing in one says where the
                    // CLI keeps its conversation log, so this arm is reachable for every CLI
                    // the tool has no store for — which is every CLI a user adds (#12).
                    Err(NamespaceUnmatched::UnknownCli(cli)) => {
                        LivenessUnknown::NoTranscriptStore { cli: cli.clone() }
                    }
                    Err(why) => LivenessUnknown::ActivityUnreadable {
                        why: why.to_string(),
                    },
                    // A namespace was matched and the silence still is not known, so the
                    // store answered about the workspace and not about the transcript. Said
                    // as exactly that and no more: which read failed is not recorded on the
                    // session, and naming one would be a guess.
                    Ok(namespace) => LivenessUnknown::ActivityUnreadable {
                        why: format!("the last activity of {namespace} could not be read"),
                    },
                },
            }),
            Method::SnapshotCannotEstablishAbsence => Some(LivenessUnknown::SnapshotUntrustworthy),
            Method::ProcessAbsentBeforeStallThreshold => Some(LivenessUnknown::TooSoonToTell),
            // The remaining methods reached a verdict from an observation, so there is
            // nothing to explain. Listed rather than caught by a wildcard, so that a new
            // method has to be classified here deliberately instead of silently becoming a
            // state with no reason.
            Method::TranscriptChangedRecently
            | Method::ProcessResidentButSilent
            | Method::WorkRunningInWorkspace
            | Method::NoProcessAndSilencePastStall => None,
        }
    }
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

/// What this run remembered from earlier ones, and what it did with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Remembered {
    /// The remembered set as this run leaves it — what the next run will start from.
    pub memory: Memory,
    /// Why the state left by earlier runs could not be used, when it could not.
    ///
    /// `None` covers both a state that was read and a first run with none stored. Neither is
    /// a degradation, and the difference between them is not worth reporting; the difference
    /// between "read it" and "lost it" very much is.
    pub unusable: Option<Degraded>,
    /// Whether this run's state was stored for the next one.
    ///
    /// An `Err` has to be surfaced. A run that collects perfectly and fails to persist looks
    /// identical to one that succeeded, right up until the next run starts blind and reports
    /// a shorter at-risk list.
    pub persisted: Result<(), String>,
    /// Workspaces dropped from memory this run because they had been settled past the
    /// retention period.
    pub forgotten: Vec<Forgotten>,
    /// The retention period the pruning above was done with. Carried so that a report of
    /// what was forgotten can state the rule that forgot it, rather than a bare count.
    pub retention: Duration,
    /// What notification channels are configured, and their health.
    pub notify_health: NotifyHealth,
    /// What detector configuration was active this run.
    pub detector_config: crate::world::DetectorConfig,
}

impl Remembered {
    /// The state of a run that remembered nothing and stored nothing.
    pub fn none() -> Self {
        Remembered {
            memory: Memory::empty(),
            unusable: None,
            persisted: Ok(()),
            forgotten: Vec::new(),
            retention: crate::memory::DEFAULT_FORGET,
            notify_health: NotifyHealth::none(),
            detector_config: crate::world::DetectorConfig::embedded_only(),
        }
    }
}

/// Channel health and configuration status for notifications.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotifyHealth {
    /// The notification configuration that was active this run.
    pub config: NotifyConfig,
    /// How many announcements this run decided were worth making.
    ///
    /// Counted before any delivery is attempted, which is the whole point of counting it
    /// separately: with no channel configured, every `delivered` and `failed` tally below
    /// stays at zero because nothing is ever tried. Reasoning about whether alerting was
    /// wanted from those tallies alone would make "nothing to say" and "nowhere to say it"
    /// the same observation, and the second needs reporting.
    pub notable: usize,
    /// How many announcements were delivered via local channel.
    pub local_delivered: usize,
    /// How many local deliveries failed.
    pub local_failed: usize,
    /// How many announcements were never offered to the configured local channel.
    ///
    /// Counted apart from `local_failed` because they say different things: a failure is
    /// evidence about the channel, and this is evidence about the run. Folding them together
    /// would make a healthy notifier that ran out of time look broken, and — worse — would let
    /// a run that alerted about four of fourteen strandings read as one that had four to
    /// report.
    pub local_not_attempted: usize,
    /// How many announcements were delivered via remote channel.
    pub remote_delivered: usize,
    /// How many remote deliveries failed.
    pub remote_failed: usize,
    /// How many announcements were never offered to the configured remote channel.
    pub remote_not_attempted: usize,
    /// Why alerts went unattempted, when any did.
    ///
    /// The first reason reported this run. A count on its own would be a silent cap wearing a
    /// number: "six alerts were not sent" tells a reader nothing they can act on, and this is
    /// an alerting path, where the absence of an alert is read as the absence of a problem.
    pub not_attempted_reason: Option<String>,
    /// Wall time this run spent delivering, across both channels.
    ///
    /// Attributable on purpose. The alerting step is the only part of a collection that waits
    /// on something outside the machine, so the self-metering the display carries has to be
    /// able to name it rather than absorb it into the total. `ZERO` when nothing was
    /// attempted, which is the honest measurement of a run that asked no channel anything.
    pub delivery_cost: Duration,
}

impl NotifyHealth {
    /// No channels configured, nothing attempted.
    pub fn none() -> Self {
        NotifyHealth {
            config: NotifyConfig::none(),
            notable: 0,
            local_delivered: 0,
            local_failed: 0,
            local_not_attempted: 0,
            remote_delivered: 0,
            remote_failed: 0,
            remote_not_attempted: 0,
            not_attempted_reason: None,
            delivery_cost: Duration::ZERO,
        }
    }

    /// Whether any deliveries failed this run.
    pub fn has_failures(&self) -> bool {
        self.local_failed > 0 || self.remote_failed > 0
    }

    /// How many channel deliveries were never attempted this run, across both channels.
    pub fn not_attempted(&self) -> usize {
        self.local_not_attempted + self.remote_not_attempted
    }

    /// Whether anything went unattempted this run.
    pub fn has_unattempted(&self) -> bool {
        self.not_attempted() > 0
    }
}

/// Everything observed in one collection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    /// The instant this collection was taken as of — the `now` it was given.
    ///
    /// Recorded because a snapshot now carries figures of two different ages: those read
    /// during this collection, and those an earlier run read. Without the instant the
    /// collection belongs to, the age of a remembered figure is not computable from the
    /// snapshot, and something downstream would have to reach for a clock of its own and get
    /// a slightly different answer.
    pub taken_at: SystemTime,
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
    /// What earlier runs contributed to this one, and whether this one was stored.
    pub remembered: Remembered,
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

/// What a channel reported about one alert in a batch.
///
/// A `World` that answered about fewer alerts than it was asked about has broken the batch
/// contract, and the missing answers are treated as not attempted. Never as delivered: the
/// alternative is retiring an alert on the strength of an answer nobody gave, and indexing
/// past the end would take the whole collection down over a channel's bug.
fn outcome_for(report: &DeliveryReport, index: usize) -> NotifyOutcome {
    report.outcomes.get(index).cloned().unwrap_or_else(|| {
        NotifyOutcome::NotAttempted(format!(
            "the channel answered about {} alerts and said nothing about this one, which was \
             number {}",
            report.outcomes.len(),
            index + 1
        ))
    })
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
                    // Filled in below, once for every session, from what earlier runs read.
                    last_reading: None,
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
                    // Filled in below, once for every session, from what earlier runs read.
                    last_reading: None,
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

    // What earlier runs left behind. Read before anything is concluded, because the
    // remembered workspaces are part of what this run has to check — a workspace whose
    // session exited days ago is invisible to every observational source below, and is
    // exactly the case that loses work.
    let (previous, unusable) = match world.read_state() {
        StateRead::Found(text) => memory::parse(&text),
        // Nothing stored yet. An answer, not a degradation: a first run has no history.
        StateRead::Absent => (Memory::empty(), None),
        StateRead::Unreadable(why) => (Memory::empty(), Some(Degraded::Unreadable(why))),
    };

    // Read the detector configuration. This layers user-supplied detectors over the embedded
    // defaults, so a fifth agent CLI can be recognised without waiting for a release.
    let detector_config = world.read_detector_config();
    let detectors = &detector_config.detectors;

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
                // Filled in below, once for every session, from what earlier runs read.
                last_reading: None,
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
        detectors,
    );

    let mut sessions = process_sessions;
    sessions.extend(transcript_sessions);

    // Give any session whose ledger could not be read now the last one that WAS read.
    //
    // Done here, in one pass over the assembled list, rather than at each of the three places
    // a `Session` is built: the invariant is that a remembered figure never shadows a live
    // one, and an invariant enforced in three places is an invariant that will eventually
    // hold in two.
    for session in &mut sessions {
        if session.resources.is_ok() {
            continue;
        }
        // Copied out before the assignment because the identity borrows the session.
        let identity = memory::identity_of(session)
            .map(|(cli, recorded)| (cli.to_string(), recorded.to_string()));
        if let Some((cli, recorded_as)) = identity {
            session.last_reading = previous.reading_for(&cli, &recorded_as).cloned();
        }
    }

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

    // Source 6: every workspace an earlier run saw.
    //
    // This is the source that makes the safety net durable rather than instantaneous. All
    // five sources above start from something observable NOW — a process, a transcript, a
    // directory near a repository someone is working in — and a workspace whose session
    // exited and whose neighbourhood nobody is working in satisfies none of them. That
    // workspace is not a corner case; it is the one that loses work.
    //
    // Added AFTER the sweep roots are derived, deliberately: a remembered workspace has to be
    // re-checked, but it does not need to drag its whole neighbourhood into the sweep every
    // run. Its spelling is added last so a freshly observed spelling of the same path wins
    // the case-insensitive deduplication below.
    candidate_paths.extend(previous.workspaces.iter().map(|w| w.path.clone()));

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
    let mut workspaces: Vec<WorkspaceReport> = Vec::new();
    // What each workspace contributes to memory, gathered in the same pass that classifies it
    // so that the state a workspace is reported in and the state it is remembered in cannot
    // be two different answers.
    let mut sightings: Vec<Sighting> = Vec::new();

    for ((path, linked_from_root), facts) in unique_workspace_candidates
        .iter()
        .zip(vcs_facts_results.iter())
    {
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
        sightings.push(Sighting::of(path.clone(), &state, session_driving));

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

        workspaces.push(WorkspaceReport {
            path: path.clone(),
            state,
            linked_worktree,
            uncommitted_entries,
        });
    }

    // --- What this run hands to the next ---
    //
    // Last, because it folds in everything above: the workspaces as classified, and the
    // sessions as read. `previous` is consumed here, so nothing after this point can still be
    // reasoning from the old state.
    let memory = memory::remember(previous, &sightings, &sessions, now);
    let (memory_after_forgetting, forgotten) = memory::forget(memory, now, thresholds.forget);

    // --- Notifications ---
    //
    // Decided based on this run's observations and what was announced before. Delivery is
    // verified: only outcomes that actually succeeded update the announcement record, so an
    // undelivered alert is re-announced on the following run.
    //
    // Each channel is asked **once for the whole run** rather than once per alert. A steady
    // state of fourteen at-risk workspaces used to be fourteen sequential requests at up to
    // ten seconds each, and a first run — where nothing has been announced yet, so everything
    // notable fires at once — is the worst case of that. What the channel does with the batch
    // is its business; what it owes back is one verified outcome per alert, in order.
    let config = world.read_notify_config();
    let (announcements, updated_announcements) = notify::decide(
        &sessions,
        &workspaces,
        &memory_after_forgetting.announcements,
    );

    let payloads: Vec<String> = announcements.iter().map(|a| a.payload()).collect();

    // Nothing notable means no channel is asked anything at all, so neither the local command
    // nor the remote endpoint is touched on a quiet machine.
    let local_report = config
        .local_command
        .as_ref()
        .filter(|_| !payloads.is_empty())
        .map(|command| world.notify_local_batch(command, &payloads));
    let remote_report = config
        .remote_url
        .as_ref()
        .filter(|_| !payloads.is_empty())
        .map(|url| world.notify_remote_batch(url, &payloads));

    let mut local_delivered = 0;
    let mut local_failed = 0;
    let mut local_not_attempted = 0;
    let mut remote_delivered = 0;
    let mut remote_failed = 0;
    let mut remote_not_attempted = 0;
    let mut not_attempted_reason: Option<String> = None;

    // Track which announcements were actually delivered, so only those update the record.
    let mut successfully_announced = updated_announcements.clone();

    for (index, announcement) in announcements.iter().enumerate() {
        let mut delivered_somewhere = false;

        // Local channel
        if let Some(report) = &local_report {
            match outcome_for(report, index) {
                NotifyOutcome::Delivered => {
                    local_delivered += 1;
                    delivered_somewhere = true;
                }
                NotifyOutcome::Failed(_) => {
                    local_failed += 1;
                }
                NotifyOutcome::NotAttempted(why) => {
                    local_not_attempted += 1;
                    not_attempted_reason.get_or_insert(why);
                }
                NotifyOutcome::NoChannelConfigured => {}
            }
        }

        // Remote channel
        if let Some(report) = &remote_report {
            match outcome_for(report, index) {
                NotifyOutcome::Delivered => {
                    remote_delivered += 1;
                    delivered_somewhere = true;
                }
                NotifyOutcome::Failed(_) => {
                    remote_failed += 1;
                }
                NotifyOutcome::NotAttempted(why) => {
                    remote_not_attempted += 1;
                    not_attempted_reason.get_or_insert(why);
                }
                NotifyOutcome::NoChannelConfigured => {}
            }
        }

        // If neither channel delivered, remove this announcement from the successful set so
        // it will be re-announced next run. An alert that was never attempted lands here too:
        // "not sent" and "sent and refused" differ in what they say about the channel, not in
        // what may be recorded as announced.
        if !delivered_somewhere {
            match announcement {
                notify::Announcement::SessionWaiting {
                    cli, recorded_as, ..
                } => {
                    successfully_announced
                        .sessions
                        .retain(|a| !(a.cli == *cli && a.recorded_as == *recorded_as));
                }
                notify::Announcement::WorkspaceStranded { path, .. }
                | notify::Announcement::WorkspaceUnknownAtRisk { path, .. } => {
                    // Case-insensitively, matching how `notify::decide` looks the path up and
                    // how workspace paths are compared everywhere else in this crate. Two
                    // spellings of one path here would record an alert that was never
                    // delivered as sent, and it would never be announced again.
                    successfully_announced
                        .workspaces
                        .retain(|(p, _)| !p.eq_ignore_ascii_case(path));
                }
            }
        }
    }

    // Only update the announcement record with what actually got delivered.
    let mut memory_with_announcements = memory_after_forgetting;
    memory_with_announcements.announcements = successfully_announced;

    let persisted = world.write_state(&memory::serialise(&memory_with_announcements));

    Ok(Snapshot {
        taken_at: now,
        sessions,
        workspaces,
        unlocated,
        sweep_complete: sweep.complete,
        remembered: Remembered {
            memory: memory_with_announcements,
            unusable,
            persisted,
            forgotten,
            retention: thresholds.forget,
            notify_health: NotifyHealth {
                config,
                notable: announcements.len(),
                local_delivered,
                local_failed,
                local_not_attempted,
                remote_delivered,
                remote_failed,
                remote_not_attempted,
                not_attempted_reason,
                // Summed rather than maximised: the two channels are asked one after the
                // other, so the run really did wait for both.
                delivery_cost: local_report.as_ref().map(|r| r.cost).unwrap_or_default()
                    + remote_report.as_ref().map(|r| r.cost).unwrap_or_default(),
            },
            detector_config,
        },
    })
}
