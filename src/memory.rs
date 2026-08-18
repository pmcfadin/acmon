//! Seam 8 — what one run carries forward to the next.
//!
//! Discovery is process-first, and a process that has exited cannot be enumerated. That is
//! not an edge case: it is *precisely* the stranded-work case. The workspace whose loss
//! motivated this project held 27 uncommitted files and no live session, so the session that
//! could have led an observer to it was already gone. A monitor that only ever sees the
//! current instant is structurally unable to report the thing it exists to report.
//!
//! So discovery is cumulative. A workspace seen once keeps being checked, and a session's
//! last resource reading outlives its process, until both have been quiet and clean long
//! enough that forgetting them loses nothing.
//!
//! **This module is pure.** It builds and prunes the remembered set and turns it into text
//! and back; [`World`](crate::World) reads and writes the file. Splitting it that way is
//! what makes the retention rules testable without a filesystem, and the retention rules are
//! where the subtlety is.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::collect::{Identity, Session};
use crate::vcs::WorkspaceState;
use crate::world::Resources;

/// The schema version written into every state file.
///
/// Checked on read. A file from a *newer* acmon is not guessed at: fields this version does
/// not know about would be dropped on the next write, which would silently destroy state a
/// newer build is relying on. Refusing is the smaller loss, and it is reported.
pub const SCHEMA_VERSION: u32 = 1;

/// How long a workspace must stay settled before it is forgotten, by default.
///
/// Seven days. The figure is bounded on both sides by what forgetting costs. Forgetting too
/// early loses the one thing memory adds — the ability to check a workspace whose session is
/// gone — and a week comfortably spans a Friday-to-Monday gap, which is exactly when work is
/// abandoned mid-change. Forgetting too late is cheap but not free: every remembered path is
/// a version-control query on every run.
///
/// Nothing irreversible happens at the boundary. A forgotten workspace is rediscovered the
/// moment any process works in it again; all that is lost is the record of when it was first
/// seen.
pub const DEFAULT_FORGET: Duration = Duration::from_secs(7 * 24 * 3600);

/// The environment variable that overrides [`DEFAULT_FORGET`].
pub const FORGET_VARIABLE: &str = "ACMON_FORGET_SECONDS";

/// One workspace, as it is remembered between runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RememberedWorkspace {
    /// The workspace's repository root, or the observed directory when it is not a
    /// repository — the same identity [`WorkspaceReport`](crate::WorkspaceReport) uses, so
    /// the two cannot disagree about what one workspace is.
    pub path: String,
    /// When this workspace was first observed, across all runs. Never moves forward.
    #[serde(with = "iso")]
    pub first_seen: SystemTime,
    /// When this workspace was most recently observed.
    #[serde(with = "iso")]
    pub last_seen: SystemTime,
    /// Since when this workspace has been *continuously* settled — holding nothing worth
    /// protecting, with nothing driving it. The forgetting clock, and it only runs while
    /// that stays true: anything unsettling the workspace sets this back to `None`, so a
    /// workspace that goes dirty on day six starts its week over rather than being dropped
    /// on day seven.
    ///
    /// `None` means the clock is not running, which is also what a workspace whose state
    /// could not be read gets. An unreadable workspace is never forgotten, because unknown
    /// is not clean.
    #[serde(with = "iso_option", default)]
    pub settled_since: Option<SystemTime>,
}

/// A resource reading, and when it was taken.
///
/// The timestamp is not decoration. A total shown without it cannot be told apart from a
/// current one, and a five-hour-old memory figure presented as live is a plausible wrong
/// answer of exactly the kind this project exists to remove.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reading {
    pub resources: Resources,
    #[serde(with = "iso")]
    pub taken_at: SystemTime,
}

/// One session, as it is remembered between runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RememberedSession {
    /// Which CLI, from the detector that matched.
    pub cli: String,
    /// The transcript this session is recorded under — a namespace for Claude Code, a
    /// session id for Codex. See [`identity_of`] for why this and not the pid.
    pub recorded_as: String,
    #[serde(with = "iso")]
    pub first_seen: SystemTime,
    #[serde(with = "iso")]
    pub last_seen: SystemTime,
    /// The last reading actually taken from this session's process, if one ever was.
    ///
    /// This is what makes a session's lifetime totals survive its exit. `None` means no
    /// reading has ever succeeded for it — never a zeroed [`Reading`], which would report
    /// a session that consumed nothing.
    pub last_reading: Option<Reading>,
}

/// Everything carried between runs.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Memory {
    pub workspaces: Vec<RememberedWorkspace>,
    pub sessions: Vec<RememberedSession>,
    /// What has been announced on earlier runs. Added in schema v1 with `#[serde(default)]`,
    /// so that a state file written before this field existed still parses.
    #[serde(default)]
    pub announcements: crate::notify::AnnouncementRecord,
}

impl Memory {
    /// Nothing remembered. What a first run starts from, and what a state file that could
    /// not be understood degrades to.
    pub fn empty() -> Self {
        Memory::default()
    }

    pub fn is_empty(&self) -> bool {
        self.workspaces.is_empty() && self.sessions.is_empty()
    }

    /// The last reading remembered for a session, if there is one.
    pub fn reading_for(&self, cli: &str, recorded_as: &str) -> Option<&Reading> {
        self.sessions
            .iter()
            .find(|session| session.cli == cli && session.recorded_as == recorded_as)
            .and_then(|session| session.last_reading.as_ref())
    }

    /// What is remembered about a workspace, matched the way workspace paths are matched
    /// everywhere else in this crate: case-insensitively, because APFS is case-insensitive
    /// but case-preserving and the same workspace arrives spelled differently from
    /// different sources.
    pub fn workspace(&self, path: &str) -> Option<&RememberedWorkspace> {
        self.workspaces
            .iter()
            .find(|remembered| remembered.path.eq_ignore_ascii_case(path))
    }
}

/// Why the state left by earlier runs could not be used.
///
/// Carried out of the collection and reported, never merely logged. The whole value of
/// remembering is that a workspace nothing is running in still gets checked; a run that
/// silently lost that history would present a *shorter* at-risk list as though it were the
/// whole one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Degraded {
    /// The file exists and the filesystem would not hand it over.
    Unreadable(String),
    /// The file was read and is not the shape this version writes. Carries what the parser
    /// objected to.
    Unparsable(String),
    /// The file was written by a version of acmon whose schema this one does not know.
    UnknownVersion { found: u32, understood: u32 },
}

impl std::fmt::Display for Degraded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Degraded::Unreadable(why) => {
                write!(f, "remembered state could not be read ({why})")
            }
            Degraded::Unparsable(why) => {
                write!(f, "remembered state could not be understood ({why})")
            }
            Degraded::UnknownVersion { found, understood } => write!(
                f,
                "remembered state is schema version {found}, and this acmon understands {understood}"
            ),
        }
    }
}

/// One workspace this run dropped from memory, and why.
///
/// Returned rather than discarded so that a run can say what it stopped watching. Pruning
/// is correct, but a safety net that quietly shrinks is indistinguishable from one that is
/// working.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Forgotten {
    pub path: String,
    /// How long it had been settled when it was dropped.
    pub settled_for: Duration,
}

/// One workspace as this run observed it, reduced to the single fact memory needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sighting {
    pub path: String,
    /// Whether this workspace holds nothing worth protecting *and* nothing is driving it.
    pub settled: bool,
}

impl Sighting {
    /// Reduce an observed workspace to what memory needs.
    ///
    /// Settled means both halves of "clean and quiet":
    ///
    /// - **Clean** — `CLEAN`, or an `UNKNOWN` that is an answer rather than a failure: a
    ///   directory under no version control, or one whose path is gone, has nothing left to
    ///   lose. An `UNKNOWN` that is a read *failure* is not clean, because we cannot tell,
    ///   and a workspace we cannot tell about is never forgotten.
    /// - **Quiet** — nothing is driving it. A clean workspace with an agent working in it is
    ///   not settled: its state is about to change, and its clock should not be running.
    ///
    /// `DIRTY-DRIVEN` fails both halves and is excluded explicitly rather than by relying on
    /// `driven` being passed as true, so the rule holds even if the two are ever computed
    /// from different observations.
    pub fn of(path: String, state: &WorkspaceState, driven: bool) -> Self {
        let clean = !state.at_risk() && *state != WorkspaceState::DirtyDriven;
        Sighting {
            path,
            settled: clean && !driven,
        }
    }
}

/// The durable identity of a session: the transcript it is recorded under.
///
/// Returns `(cli, recorded_as)`, or `None` for a session with no transcript attribution —
/// those cannot be recognised again next run, and inventing an identity for them would
/// merge two unrelated sessions.
///
/// **A pid is deliberately not an identity.** The kernel reuses them, and this is the one
/// place in the crate where a value has to be recognised across time rather than within one
/// enumeration. A reused pid carrying the previous occupant's lifetime CPU total would be
/// wrong by hours while looking entirely ordinary. The transcript identity is stable by
/// construction: it is what a process-derived session's namespace resolves to and what a
/// transcript-derived one is named by, which is what lets the same session be tracked
/// across the moment its process exits.
pub fn identity_of(session: &Session) -> Option<(&str, &str)> {
    let recorded_as = match &session.identity {
        Identity::Transcript { recorded_as } => recorded_as.as_str(),
        Identity::Process { .. } => session
            .workspace
            .as_ref()
            .ok()?
            .namespace
            .as_ref()
            .ok()?
            .as_str(),
    };
    Some((session.cli.as_str(), recorded_as))
}

/// Fold this run's observations into what was already remembered.
///
/// `first_seen` is preserved for anything already known and set to `now` for anything new;
/// `last_seen` moves to `now` for everything observed. An entry that was *not* observed this
/// run is kept untouched — its `last_seen` stays where it was, which is what stops an
/// unobserved workspace from ageing into "settled" without anyone having checked it.
pub fn remember(
    previous: Memory,
    sightings: &[Sighting],
    sessions: &[Session],
    now: SystemTime,
) -> Memory {
    let Memory {
        workspaces: previous_workspaces,
        sessions: previous_sessions,
        announcements,
    } = previous;

    let mut workspaces: Vec<RememberedWorkspace> = Vec::new();

    for sighting in sightings {
        let known = previous_workspaces
            .iter()
            .find(|remembered| remembered.path.eq_ignore_ascii_case(&sighting.path));

        // Only ever advanced from an already-running clock, so the recorded instant is when
        // the workspace *became* settled rather than when it was last seen to be.
        let settled_since = match (sighting.settled, known.and_then(|k| k.settled_since)) {
            (true, Some(since)) => Some(since),
            (true, None) => Some(now),
            (false, _) => None,
        };

        workspaces.push(RememberedWorkspace {
            path: sighting.path.clone(),
            first_seen: known.map(|k| k.first_seen).unwrap_or(now),
            last_seen: now,
            settled_since,
        });
    }

    // Anything remembered that this run did not observe stays exactly as it was. This is
    // reachable: a candidate whose version-control query is still in flight when the batch
    // hits its budget produces no sighting at all, and dropping it would forget a workspace
    // on the strength of a timeout.
    for remembered in previous_workspaces {
        let already_present = workspaces
            .iter()
            .any(|kept| kept.path.eq_ignore_ascii_case(&remembered.path));
        if !already_present {
            workspaces.push(remembered);
        }
    }

    let mut remembered_sessions: Vec<RememberedSession> = Vec::new();

    for session in sessions {
        let Some((cli, recorded_as)) = identity_of(session) else {
            continue;
        };
        let known = previous_sessions
            .iter()
            .find(|remembered| remembered.cli == cli && remembered.recorded_as == recorded_as);

        // A reading is only recorded when one was actually taken this run. When it was not,
        // the previous reading is carried forward with its ORIGINAL timestamp — restamping
        // it to `now` would turn a stale figure into an apparently fresh one, which is worse
        // than not remembering it at all.
        let last_reading = match &session.resources {
            Ok(resources) => Some(Reading {
                resources: resources.clone(),
                taken_at: now,
            }),
            Err(_) => known.and_then(|k| k.last_reading.clone()),
        };

        remembered_sessions.push(RememberedSession {
            cli: cli.to_string(),
            recorded_as: recorded_as.to_string(),
            first_seen: known.map(|k| k.first_seen).unwrap_or(now),
            last_seen: now,
            last_reading,
        });
    }

    // A session that has dropped out of the discovery window is no longer listed, and its
    // totals go with it. That is deliberate: memory exists to keep *workspaces* under watch,
    // and an unbounded session ledger would grow by one entry per session forever.
    Memory {
        workspaces,
        sessions: remembered_sessions,
        announcements,
    }
}

/// Drop workspaces that have been settled for longer than `retention`.
///
/// Only settled entries are eligible, and the clock is `settled_since` rather than
/// `last_seen`: a workspace that has been sitting dirty and unattended for a month is the
/// most at-risk thing this tool can find, so age alone must never be grounds for forgetting
/// it. Returns what was dropped, so the run can say so.
pub fn forget(memory: Memory, now: SystemTime, retention: Duration) -> (Memory, Vec<Forgotten>) {
    let Memory {
        workspaces,
        sessions,
        announcements,
    } = memory;

    let mut kept = Vec::new();
    let mut forgotten = Vec::new();

    for workspace in workspaces {
        // `duration_since` fails when `settled_since` is in the future, which happens if the
        // clock moved backwards between runs. Zero is the honest answer there — the workspace
        // has not provably been settled for any length of time — and it keeps the entry.
        let settled_for = workspace
            .settled_since
            .map(|since| now.duration_since(since).unwrap_or(Duration::ZERO));

        match settled_for {
            Some(settled_for) if settled_for > retention => forgotten.push(Forgotten {
                path: workspace.path,
                settled_for,
            }),
            _ => kept.push(workspace),
        }
    }

    (
        Memory {
            workspaces: kept,
            sessions,
            announcements,
        },
        forgotten,
    )
}

/// The retention period this machine is configured with.
///
/// A value that is present but unreadable is an **error**, never a silent fall back to the
/// default — the same rule as the liveness thresholds, for the same reason: someone who set
/// a retention period and got the default anyway would be reading a state file pruned by a
/// rule they believe they replaced.
pub fn retention_from_value(value: Option<&str>) -> Result<Duration, String> {
    match value {
        None => Ok(DEFAULT_FORGET),
        Some(text) => text
            .trim()
            .parse::<u64>()
            .map(Duration::from_secs)
            .map_err(|e| {
                format!("{FORGET_VARIABLE} is {text:?}, which is not a number of seconds: {e}")
            }),
    }
}

/// The state file, as it is stored.
///
/// The version sits alongside the data rather than being implied by its shape, so a file
/// from a future acmon is recognised as such instead of parsing into a subset of itself.
#[derive(Debug, Serialize, Deserialize)]
struct StateFile {
    version: u32,
    #[serde(flatten)]
    memory: Memory,
}

/// Turn memory into the text of a state file.
///
/// Pretty-printed with ISO timestamps because a human has to be able to check it. Every
/// figure this tool prints is meant to be verifiable by hand, and the remembered ones are
/// the only figures that cannot be re-derived by looking at the machine.
pub fn serialise(memory: &Memory) -> String {
    let file = StateFile {
        version: SCHEMA_VERSION,
        memory: memory.clone(),
    };
    // Cannot fail: every field is a string, a number, or a `Result` of those. An `expect`
    // rather than a silent empty file, because writing an empty state file over a good one
    // would discard the history without saying so.
    serde_json::to_string_pretty(&file).expect("remembered state is always serialisable")
}

/// Read a state file, degrading to empty with a stated reason rather than to empty alone.
///
/// **Never partially applied.** A file that parses into some entries and then fails is
/// discarded whole: half a remembered set presented as the whole one is a shorter at-risk
/// list that reads as a safer machine.
pub fn parse(text: &str) -> (Memory, Option<Degraded>) {
    let file: StateFile = match serde_json::from_str(text) {
        Ok(file) => file,
        Err(error) => {
            return (
                Memory::empty(),
                Some(Degraded::Unparsable(error.to_string())),
            )
        }
    };

    if file.version != SCHEMA_VERSION {
        return (
            Memory::empty(),
            Some(Degraded::UnknownVersion {
                found: file.version,
                understood: SCHEMA_VERSION,
            }),
        );
    }

    (file.memory, None)
}

/// Seconds since the Unix epoch, for a time that may precede it.
fn unix_seconds(time: SystemTime) -> i64 {
    match time.duration_since(UNIX_EPOCH) {
        Ok(since) => since.as_secs() as i64,
        // Before the epoch. Should not occur, but negating is the correct reading rather
        // than clamping to zero, which would silently move the time to 1970.
        Err(before) => -(before.duration().as_secs() as i64),
    }
}

/// A time from seconds since the Unix epoch, for a value that may be negative.
fn time_from_unix_seconds(seconds: i64) -> SystemTime {
    if seconds >= 0 {
        UNIX_EPOCH + Duration::from_secs(seconds as u64)
    } else {
        UNIX_EPOCH - Duration::from_secs(seconds.unsigned_abs())
    }
}

/// Timestamps as ISO 8601, so the state file can be read without a converter.
mod iso {
    use std::time::SystemTime;

    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(time: &SystemTime, serializer: S) -> Result<S::Ok, S::Error> {
        crate::isotime::iso8601_from_unix_seconds(super::unix_seconds(*time)).serialize(serializer)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<SystemTime, D::Error> {
        let text = String::deserialize(deserializer)?;
        crate::isotime::unix_seconds_from_iso8601(&text)
            .map(super::time_from_unix_seconds)
            .map_err(serde::de::Error::custom)
    }
}

/// The same, for a timestamp that may be absent.
mod iso_option {
    use std::time::SystemTime;

    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(
        time: &Option<SystemTime>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        time.map(|t| crate::isotime::iso8601_from_unix_seconds(super::unix_seconds(t)))
            .serialize(serializer)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<SystemTime>, D::Error> {
        let text = Option::<String>::deserialize(deserializer)?;
        match text {
            None => Ok(None),
            Some(text) => crate::isotime::unix_seconds_from_iso8601(&text)
                .map(|seconds| Some(super::time_from_unix_seconds(seconds)))
                .map_err(serde::de::Error::custom),
        }
    }
}
