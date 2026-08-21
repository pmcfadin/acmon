//! Seam 9 — deciding what to announce when sessions wait or work strands.
//!
//! **This module is pure.** It decides *what should be announced*, given this run's snapshot
//! and what was announced before. It does not deliver anything. Delivery is a `World` method.
//! This split is what makes the re-notify rules testable without a network.
//!
//! A workspace or session that leaves a notable state and later re-enters it announces again.
//! An unchanged set of notable states does not re-announce — so a machine that stays waiting
//! does not alert on every run.
//!
//! **The record is on disk, and the first pass after a start is not special.** Dedupe held only
//! in memory dies with the process, and `launchd` `KeepAlive` makes restarts routine, so a
//! resident monitor that restarted would re-announce every condition still true. The obvious fix
//! — suppress the first pass after a start — stops the storm by also swallowing every real
//! alert: a condition that *became* true while the monitor was down **is** a transition, and it
//! is exactly the kind this tool exists to catch. So what suppresses an announcement here is
//! always the record, never the pass number.

use serde::{Deserialize, Serialize};

use crate::collect::Session;
use crate::liveness::State;
use crate::memory;
use crate::vcs::WorkspaceState;
use crate::WorkspaceReport;

/// What notable state a session was last announced in.
///
/// Keyed on `(cli, recorded_as)`, never the pid: the kernel reuses pids, and recognising a
/// session across runs requires its durable identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AnnouncedSessionState {
    Waiting,
}

/// What notable state a workspace was last announced in.
///
/// Keyed on path, matched case-insensitively (APFS is case-insensitive but case-preserving).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AnnouncedWorkspaceState {
    DirtyStranded,
    /// A workspace whose version-control state could not be read. Treated as at-risk by the
    /// precautionary principle. Separated from `DirtyStranded` so that leaving this state
    /// (query succeeds) and re-entering it (query fails again) announces again.
    UnknownAtRisk,
}

/// An announced session, with its identity and state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnnouncedSession {
    pub cli: String,
    pub recorded_as: String,
    pub state: AnnouncedSessionState,
}

/// What has been announced on earlier runs.
///
/// Written to `notified.json` in the state directory, so it survives the process and the
/// re-announcing rules can be enforced across a restart. Uses Vecs instead of HashMaps because
/// HashMap with tuple keys doesn't serialize to JSON.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct AnnouncementRecord {
    /// Sessions that have been announced.
    #[serde(default)]
    pub sessions: Vec<AnnouncedSession>,
    /// Workspaces that have been announced, with their paths and states.
    #[serde(default)]
    pub workspaces: Vec<(String, AnnouncedWorkspaceState)>,
}

impl AnnouncementRecord {
    /// Nothing has been announced. What a first run starts from.
    pub fn empty() -> Self {
        AnnouncementRecord::default()
    }

    /// Whether anything at all is recorded as announced.
    ///
    /// An empty record suppresses nothing, which is why losing one costs a re-announcement
    /// rather than a missed alert.
    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty() && self.workspaces.is_empty()
    }
}

/// The schema version written into `notified.json`.
///
/// Checked on read, and a version this build does not know is **not** guessed at — the same rule
/// [`crate::memory::SCHEMA_VERSION`] follows, for a milder consequence: an unrecognised dedupe
/// record costs one re-announcement of everything currently notable, where an unrecognised
/// record silently reinterpreted could suppress a real alert forever.
pub const NOTIFIED_SCHEMA_VERSION: u32 = 1;

/// Why a run had to rebuild its dedupe record instead of carrying one forward.
///
/// Reported, never merely tolerated. Every arm has the same consequence — everything notable
/// **now** is announced once — and a reader who is not told why has just watched their monitor
/// storm for no stated reason, which is how a person learns to ignore alerts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rebuilt {
    /// No `notified.json`. A first run on this machine, a deleted state directory, or the run
    /// after an upgrade that moved the record out of the memory file.
    NothingRecorded,
    /// A `notified.json` is there and the filesystem would not hand it over.
    Unreadable(String),
    /// It was read and is not the shape this version writes. Carries the parser's complaint.
    Unparsable(String),
    /// It was written by a version of acmon whose dedupe schema this one does not know.
    UnknownVersion { found: u32, understood: u32 },
}

impl std::fmt::Display for Rebuilt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Rebuilt::NothingRecorded => write!(
                f,
                "no record of what has already been announced was found, so this run started \
                 with an empty one"
            ),
            Rebuilt::Unreadable(why) => {
                write!(
                    f,
                    "the record of what has already been announced could not be read ({why})"
                )
            }
            Rebuilt::Unparsable(why) => write!(
                f,
                "the record of what has already been announced could not be understood ({why})"
            ),
            Rebuilt::UnknownVersion { found, understood } => write!(
                f,
                "the record of what has already been announced is schema version {found}, and \
                 this acmon understands {understood}"
            ),
        }
    }
}

/// `notified.json` as it is stored: the version beside the data, never implied by its shape.
#[derive(Debug, Serialize, Deserialize)]
struct NotifiedFile {
    version: u32,
    #[serde(flatten)]
    record: AnnouncementRecord,
}

/// Turn the dedupe record into the text of `notified.json`.
///
/// Pretty-printed, because a reader who is being told "this was already announced, so you got no
/// alert" has to be able to check that claim by hand.
pub fn serialise(record: &AnnouncementRecord) -> String {
    let file = NotifiedFile {
        version: NOTIFIED_SCHEMA_VERSION,
        record: record.clone(),
    };
    // Cannot fail: every field is a string or an enum of unit variants. An `expect` rather than
    // an empty file, because writing an empty record over a good one would silently re-announce
    // everything on the next run with nothing to say why.
    serde_json::to_string_pretty(&file).expect("the dedupe record is always serialisable")
}

/// Read `notified.json`, degrading to an empty record **with a stated reason**.
///
/// **Never partially applied.** Half a dedupe record is worse than none: the entries that
/// survived would suppress their alerts while the ones that did not would fire, and nothing
/// would say which had happened.
pub fn parse(text: &str) -> (AnnouncementRecord, Option<Rebuilt>) {
    let file: NotifiedFile = match serde_json::from_str(text) {
        Ok(file) => file,
        Err(error) => {
            return (
                AnnouncementRecord::empty(),
                Some(Rebuilt::Unparsable(error.to_string())),
            )
        }
    };

    if file.version != NOTIFIED_SCHEMA_VERSION {
        return (
            AnnouncementRecord::empty(),
            Some(Rebuilt::UnknownVersion {
                found: file.version,
                understood: NOTIFIED_SCHEMA_VERSION,
            }),
        );
    }

    (file.record, None)
}

/// One notable thing to announce.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Announcement {
    SessionWaiting {
        cli: String,
        recorded_as: String,
        workspace_path: Option<String>,
    },
    WorkspaceStranded {
        path: String,
        /// How many entries version control reported. `None` is never a stand-in for zero —
        /// see the payload below for what an absent count says instead.
        uncommitted_entries: Option<usize>,
    },
    WorkspaceUnknownAtRisk {
        path: String,
        reason: String,
    },
}

impl Announcement {
    /// The notification payload, for delivery.
    ///
    /// **Privacy constraint:** Carries names and states only. Never prompt text, conversation
    /// content, or process arguments. This tool never reads conversation content at all, so
    /// the constraint is enforced structurally — but the payload still has to be kept clean.
    pub fn payload(&self) -> String {
        match self {
            Announcement::SessionWaiting {
                cli,
                recorded_as,
                workspace_path,
            } => {
                let workspace = workspace_path
                    .as_ref()
                    .map(|p| format!(" in {}", p))
                    .unwrap_or_default();
                format!("Session {} {} is WAITING{}", cli, recorded_as, workspace)
            }
            Announcement::WorkspaceStranded {
                path,
                uncommitted_entries,
            } => match uncommitted_entries {
                Some(count) => {
                    format!("Workspace {path} is STRANDED with {count} uncommitted entries")
                }
                // Never "with 0 uncommitted entries". An alert whose whole purpose is to say
                // work is at risk, reporting none at risk, is the calm plausible wrong answer
                // this project exists to remove — and the reader would reasonably ignore it.
                None => format!(
                    "Workspace {path} is STRANDED, and how many entries are uncommitted could \
                     not be read"
                ),
            },
            Announcement::WorkspaceUnknownAtRisk { path, reason } => {
                format!(
                    "Workspace {} is at risk (version control: {})",
                    path, reason
                )
            }
        }
    }
}

/// Notable state of a session right now.
fn notable_session_state(session: &Session) -> Option<AnnouncedSessionState> {
    match session.liveness.state {
        State::Waiting => Some(AnnouncedSessionState::Waiting),
        _ => None,
    }
}

/// Notable state of a workspace right now.
///
/// `Unknown(QueryFailed)` and `Unknown(TimedOut)` are at-risk by the precautionary principle:
/// version control exists but would not answer, so the workspace might hold uncommitted work
/// and we cannot tell. A workspace we cannot read about is worth alerting on, because losing
/// work to a query timeout would be the exact failure mode this tool exists to prevent.
///
/// `Unknown(NotVersionControlled)` and `Unknown(PathGone)` are NOT at-risk: the first says
/// there is no version control, so there is nothing to lose; the second says the path is
/// already gone, so there is nothing left to protect. Both are answers, not read failures.
fn notable_workspace_state(workspace: &WorkspaceReport) -> Option<AnnouncedWorkspaceState> {
    match &workspace.state {
        WorkspaceState::DirtyStranded => Some(AnnouncedWorkspaceState::DirtyStranded),
        WorkspaceState::Unknown(crate::vcs::Unreadable::QueryFailed(_))
        | WorkspaceState::Unknown(crate::vcs::Unreadable::TimedOut) => {
            Some(AnnouncedWorkspaceState::UnknownAtRisk)
        }
        _ => None,
    }
}

/// What to announce, given this run's observations and what was announced before.
///
/// Returns the announcements to deliver and the updated record for the next run.
///
/// A workspace or session that leaves a notable state and later re-enters it announces again.
/// An unchanged set of notable states does not re-announce.
///
/// The returned record describes what is notable **now**, which is not yet what may be
/// remembered. Delivery has not been attempted at this point, and an alert that fails to
/// deliver must not be recorded as sent — so the caller removes the undelivered ones before
/// the record is stored. Keeping that decision at the call site is deliberate: this function
/// cannot know what was delivered, and a record written here would be a claim about the
/// future.
pub fn decide(
    sessions: &[Session],
    workspaces: &[WorkspaceReport],
    previous: &AnnouncementRecord,
) -> (Vec<Announcement>, AnnouncementRecord) {
    let mut announcements = Vec::new();
    let mut updated_sessions = Vec::new();
    let mut updated_workspaces = Vec::new();

    // Sessions
    for session in sessions {
        let Some((cli, recorded_as)) = memory::identity_of(session) else {
            continue;
        };

        if let Some(current_state) = notable_session_state(session) {
            let previously_announced = previous
                .sessions
                .iter()
                .find(|a| a.cli == cli && a.recorded_as == recorded_as)
                .map(|a| &a.state);

            // Announce if this is a new notable state OR if we left and re-entered it
            if previously_announced != Some(&current_state) {
                announcements.push(Announcement::SessionWaiting {
                    cli: cli.to_string(),
                    recorded_as: recorded_as.to_string(),
                    workspace_path: session.workspace.as_ref().ok().map(|w| w.path.clone()),
                });
            }
            // Record that we've observed this state, whether we announced or not
            updated_sessions.push(AnnouncedSession {
                cli: cli.to_string(),
                recorded_as: recorded_as.to_string(),
                state: current_state,
            });
        }
        // If the session is no longer in a notable state, drop it from the record
    }

    // Workspaces
    for workspace in workspaces {
        if let Some(current_state) = notable_workspace_state(workspace) {
            // Matched case-insensitively
            let previously_announced = previous
                .workspaces
                .iter()
                .find(|(path, _)| path.eq_ignore_ascii_case(&workspace.path))
                .map(|(_, state)| state);

            // Announce if this is a new notable state OR if we left and re-entered it
            if previously_announced != Some(&current_state) {
                match current_state {
                    AnnouncedWorkspaceState::DirtyStranded => {
                        announcements.push(Announcement::WorkspaceStranded {
                            path: workspace.path.clone(),
                            uncommitted_entries: workspace.uncommitted_entries,
                        });
                    }
                    AnnouncedWorkspaceState::UnknownAtRisk => {
                        let reason = match &workspace.state {
                            WorkspaceState::Unknown(r) => r.to_string(),
                            _ => "unknown".to_string(),
                        };
                        announcements.push(Announcement::WorkspaceUnknownAtRisk {
                            path: workspace.path.clone(),
                            reason,
                        });
                    }
                }
            }
            // Record that we've observed this state
            updated_workspaces.push((workspace.path.clone(), current_state));
        }
        // If the workspace is no longer in a notable state, drop it from the record
    }

    let updated_record = AnnouncementRecord {
        sessions: updated_sessions,
        workspaces: updated_workspaces,
    };

    (announcements, updated_record)
}
