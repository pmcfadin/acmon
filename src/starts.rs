//! Every launch records its downtime and whether the last exit was clean.
//!
//! F23, N7 and PRD decision 31. `launchd` `KeepAlive` restarts a monitor that dies, and a
//! restart on its own leaves no trace: `state.json` after a crash loop and `state.json` after
//! an uninterrupted night are the same file with the same shape. A monitor that has been dying
//! and restarting every ten seconds all night therefore reads as a healthy one, and G1 and G2
//! silently stopped holding hours ago.
//!
//! Deliberately **not** answered with a second process watching the first. launchd is the
//! supervisor; a watchdog for the watchdog can die just as quietly, and then there are two
//! silent failures instead of one. Nothing here spawns, signals or supervises anything. The gap
//! is made **visible** instead: one line appended to `starts.jsonl` per launch, saying when the
//! monitor started, how long the machine went unmonitored, and whether the run before it ended
//! on purpose.
//!
//! ## Why the downtime is measured from `state.json`
//!
//! The obvious design is a shutdown record written on the way out, subtracted from the next
//! launch. It is unmeasurable in exactly the case it exists to measure: a `SIGKILL`ed monitor
//! never writes one, so the crash that mattered would be the one launch with no downtime to
//! report. So the gap is measured from the last **state write** — the thing the monitor was
//! doing anyway, every pass, and the last thing it managed before it died.
//!
//! ## Why a clean exit is knowable at all
//!
//! From the lock, not from a flag the dying process was supposed to set. [`crate::lock`] clears
//! the pid record on a clean release and leaves it behind otherwise, so a stale pid in
//! `watch.lock` *is* the evidence that the previous monitor did not exit cleanly — the kernel
//! wrote it by releasing the `flock` without the process getting to tidy up. See
//! [`crate::lock::WatchLock::took_over_from`].
//!
//! ## What "absent" means here
//!
//! A first launch on a machine has no previous exit, and reporting that as *clean* would be the
//! fail-to-zero this project exists to remove: it would assert that a monitor stopped on purpose
//! when no monitor has ever run. An empty lock record is ambiguous on its own — a clean release
//! and a machine that has never been monitored both leave one — so this module resolves the
//! ambiguity against the record file and `state.json`, and says `unknown` where it cannot.

use std::io::Write as _;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use crate::lock::Predecessor;
use crate::meter::Measured;
use crate::state::{StateStore, STATE_FILE};
use crate::world::StateRead;

/// The launch record, in the state directory.
///
/// One JSON object per line, appended. Named here rather than spelled out at each call site so
/// that the monitor that appends it and the reader that reports it cannot disagree about which
/// file they mean.
pub const STARTS_FILE: &str = "starts.jsonl";

/// The schema of one line, carried on the line itself.
///
/// Per record rather than per file, because the file is append-only: an upgrade adds lines in a
/// new version beside lines in an old one, and a version at the top of the file would describe
/// only the oldest of them.
pub const RECORD_VERSION: u32 = 1;

/// How many launches the crash-loop verdict looks back over, this one included.
pub const RECENT_LAUNCHES: usize = 5;

/// A previous run shorter than this did not get going.
///
/// The slow tier's active interval, which is not an arbitrary threshold: a monitor that ran for
/// less than one slow interval never completed a round of all three tiers, so it never reached
/// the state where it publishes a complete picture or announces anything. A run that short is a
/// monitor that died on the way up rather than one that worked and then stopped.
pub const SHORT_RUN: Duration = crate::schedule::Cadence::ACTIVE.slow;

/// How many short runs among [`RECENT_LAUNCHES`] read as a monitor cycling rather than running.
pub const CYCLING_THRESHOLD: usize = 3;

/// When `state.json` was last written, as an instant.
///
/// Distinct from [`crate::launchd::LastWrite`], which is this same reading turned into an *age*
/// against a clock for `amon status`. Both come from here, so there is one answer to "when did
/// the monitor last write" rather than two that can drift.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LastStateWrite {
    At(SystemTime),
    /// Nothing has ever been written here. An answer, and not an instant long ago.
    Never,
    /// The file is there and its modification time could not be read. Never collapsed into
    /// [`LastStateWrite::Never`]: "no monitor has ever run here" and "this machine would not say"
    /// are different facts, and the second one is a fault worth seeing.
    Unreadable(String),
}

/// When `state.json` was last written, from its modification time.
///
/// The mtime of the file rather than a tier's timestamp, because this is the age of the *write* —
/// the last moment a monitor was demonstrably working — rather than the age of any one fact in it.
pub fn last_state_write(store: &StateStore) -> LastStateWrite {
    let path = store.paths().state_dir().join(STATE_FILE);
    match std::fs::metadata(&path) {
        Ok(metadata) => match metadata.modified() {
            Ok(modified) => LastStateWrite::At(modified),
            Err(error) => LastStateWrite::Unreadable(format!(
                "{} is there, but its modification time could not be read: {error}",
                path.display()
            )),
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => LastStateWrite::Never,
        Err(error) => {
            LastStateWrite::Unreadable(format!("{} could not be examined: {error}", path.display()))
        }
    }
}

/// How long the machine went without a monitor writing anything.
///
/// Four outcomes, none of which is a number when it is not a measurement. A downtime of `0`
/// standing in for "there was nothing to subtract from" would read as a monitor that restarted
/// instantly, which is the most reassuring possible misreading of a machine that has never been
/// monitored at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Downtime {
    /// The gap between the last state write and this launch.
    Since(Duration),
    /// No state has ever been written here, so there is no earlier write to subtract from.
    NoPreviousWrite,
    /// The last state write is stamped *after* this launch. Clock skew, a file restored from a
    /// backup, or a machine whose clock stepped backwards; either way the gap cannot be computed
    /// and a number here would read as a measurement.
    LastWriteAheadOfLaunch(Duration),
    Unreadable(String),
}

impl Downtime {
    /// The gap in seconds, with the reason when there is not one.
    pub fn seconds(&self) -> Measured<f64> {
        match self {
            Downtime::Since(gap) => Measured::known(gap.as_secs_f64()),
            Downtime::NoPreviousWrite => Measured::unavailable(format!(
                "{STATE_FILE} has never been written here, so there is no earlier write to \
                 measure a gap from; this is a first launch and not a downtime of zero"
            )),
            Downtime::LastWriteAheadOfLaunch(ahead) => Measured::unavailable(format!(
                "the last state write is stamped {:.1}s after this launch, so the gap cannot be \
                 computed; this machine's clock appears to have gone backwards",
                ahead.as_secs_f64()
            )),
            Downtime::Unreadable(why) => Measured::unavailable(why.clone()),
        }
    }

    /// The same fact as a sentence, for a log line and for `amon status`.
    pub fn describe(&self) -> String {
        match self.seconds() {
            Measured {
                value: Some(seconds),
                ..
            } => format!("{seconds:.1}s of downtime since the last state write"),
            Measured {
                unavailable: Some(why),
                ..
            } => format!("the downtime is not a figure: {why}"),
            // Unreachable by construction: every arm above yields one or the other.
            Measured { .. } => "the downtime is neither a figure nor a reason, which is a bug in \
                                the launch record"
                .to_string(),
        }
    }
}

/// How long the previous launch ran before its last state write.
///
/// The figure a crash loop is visible in: repeated launches whose predecessors ran for seconds is
/// a monitor being restarted, not a monitor working.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Uptime {
    Ran(Duration),
    /// No previous launch is on record, so there is no run to measure.
    NoPreviousLaunch,
    /// It could not be computed, and why.
    Unknown(String),
}

impl Uptime {
    pub fn seconds(&self) -> Measured<f64> {
        match self {
            Uptime::Ran(ran) => Measured::known(ran.as_secs_f64()),
            Uptime::NoPreviousLaunch => Measured::unavailable(
                "no previous launch is on record here, so there is no run length to state",
            ),
            Uptime::Unknown(why) => Measured::unavailable(why.clone()),
        }
    }

    /// Whether the previous run was short enough to read as a monitor that died on the way up.
    ///
    /// `None` when the run length is not known, never `false`: an unknown run length reported as
    /// "not short" is how a crash loop hides behind a fault in its own record.
    pub fn was_short(&self) -> Option<bool> {
        match self {
            Uptime::Ran(ran) => Some(*ran < SHORT_RUN),
            Uptime::NoPreviousLaunch | Uptime::Unknown(_) => None,
        }
    }
}

/// Whether the monitor before this one stopped on purpose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreviousExit {
    /// There was no monitor before this one. Not clean — there is nothing to have been clean.
    Absent,
    /// The predecessor released the lock, which only a monitor on its way out does.
    Clean,
    /// The predecessor left its pid in the lock record: it did not release, so its run ended
    /// without a clean exit. A `SIGKILL`ed monitor is exactly this.
    Unclean { pid: u32, still_running: bool },
    /// It could not be determined, and why. Never reported as clean, which would invent a
    /// shutdown, and never as unclean, which would invent a crash.
    Unknown(String),
}

impl PreviousExit {
    /// The four words that go on disk, named so that a reader comparing against one cannot
    /// misspell it and get a silent `false`.
    pub const ABSENT: &'static str = "absent";
    pub const CLEAN: &'static str = "clean";
    pub const UNCLEAN: &'static str = "unclean";
    pub const UNKNOWN: &'static str = "unknown";

    /// The word that goes on disk.
    pub fn label(&self) -> &'static str {
        match self {
            PreviousExit::Absent => PreviousExit::ABSENT,
            PreviousExit::Clean => PreviousExit::CLEAN,
            PreviousExit::Unclean { .. } => PreviousExit::UNCLEAN,
            PreviousExit::Unknown(_) => PreviousExit::UNKNOWN,
        }
    }

    /// The same fact in a sentence.
    pub fn describe(&self) -> String {
        match self {
            PreviousExit::Absent => format!(
                "there is no previous exit: no launch is on record here and {STATE_FILE} has \
                 never been written, so nothing has ever monitored this machine from this state \
                 directory"
            ),
            PreviousExit::Clean => {
                "the monitor before this one released the state lock, so it stopped on purpose"
                    .to_string()
            }
            PreviousExit::Unclean { pid, still_running } => format!(
                "the monitor before this one did not exit cleanly: pid {pid} left its record in \
                 the state lock and {}",
                if *still_running {
                    "is still running without holding it, which is stranger still — a recycled \
                     pid, or something that wrote the record without taking the lock"
                } else {
                    "is no longer running"
                }
            ),
            PreviousExit::Unknown(why) => {
                format!("whether the previous exit was clean is not known: {why}")
            }
        }
    }

    /// Whether it was clean, when that is known at all.
    ///
    /// `None` for both [`PreviousExit::Absent`] and [`PreviousExit::Unknown`] — the two states a
    /// boolean cannot hold, and the two a boolean would quietly turn into `false`.
    pub fn clean(&self) -> Option<bool> {
        match self {
            PreviousExit::Clean => Some(true),
            PreviousExit::Unclean { .. } => Some(false),
            PreviousExit::Absent | PreviousExit::Unknown(_) => None,
        }
    }
}

/// One line of `starts.jsonl`, and the same shape the state file publishes.
///
/// Flat, named, and made of strings and [`Measured`] figures rather than of this crate's internal
/// enums — the same rule the tier payloads follow, for the same reason: a reader must be able to
/// tell a schema change from a machine that has gone quiet.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StartRecord {
    /// The schema of this line. See [`RECORD_VERSION`].
    pub version: u32,
    /// When the monitor launched, ISO 8601.
    pub started_at: String,
    /// The process that launched.
    pub pid: u32,
    /// Which launch this is in this state directory, counting from 1 and including this one.
    ///
    /// The restart count a display shows. [`Measured`] rather than a bare number because a record
    /// file that cannot be read leaves the count genuinely unknown, and a `0` there would say the
    /// monitor has never started while it was starting.
    pub launches: Measured<u64>,
    /// Seconds between the last state write and this launch: how long nothing was being recorded.
    pub downtime_secs: Measured<f64>,
    /// `absent`, `clean`, `unclean` or `unknown`.
    pub previous_exit: String,
    /// The same fact in a sentence, so a line of this file is readable without the source.
    pub previous_exit_why: String,
    /// The predecessor's pid, when the lock record named one.
    pub previous_pid: Option<u32>,
    /// How long the previous launch ran before its last state write, in seconds.
    pub previous_uptime_secs: Measured<f64>,
    /// Whether that run was shorter than [`SHORT_RUN`]. `null` when its length is not known.
    pub previous_run_was_short: Option<bool>,
    /// How many launches the crash-loop verdict was judged over, this one included.
    pub launches_considered: usize,
    /// How many of those followed a run shorter than [`SHORT_RUN`].
    pub short_runs: usize,
    /// How many of those followed an unclean exit.
    pub unclean_exits: usize,
    /// Set when the record reads as a monitor cycling rather than running, in the words a human
    /// is shown. `null` is not "healthy" — it is "this does not read as a crash loop", which for
    /// a first launch is all that can be said.
    pub cycling: Option<String>,
}

impl StartRecord {
    /// Whether this record describes a monitor that is cycling.
    pub fn is_cycling(&self) -> bool {
        self.cycling.is_some()
    }
}

/// What this launch came to, carried for the run's lifetime.
///
/// The facts are decided in memory before the append is attempted, so an append that fails costs
/// the durable record and not the report: a monitor whose state directory went read-only still
/// says on screen that it took over from a monitor that died.
#[derive(Debug, Clone, PartialEq)]
pub struct Launch {
    pub record: StartRecord,
    /// Why the record could not be appended, when it could not.
    pub not_recorded: Option<String>,
}

/// What `starts.jsonl` says.
#[derive(Debug, Clone, PartialEq)]
pub enum History {
    /// There is no record file, or it holds no records: nothing has ever launched here.
    NothingRecorded,
    /// How many launches are on record, and the most recent of them, oldest first.
    Recorded {
        launches: u64,
        recent: Vec<StartRecord>,
    },
    /// The file is there and could not be read or understood. Never an empty history: a record
    /// nobody can read must not report as a machine nothing has ever run on.
    Unreadable(String),
}

impl History {
    /// The most recent record, when there is one.
    pub fn last(&self) -> Option<&StartRecord> {
        match self {
            History::Recorded { recent, .. } => recent.last(),
            History::NothingRecorded | History::Unreadable(_) => None,
        }
    }

    /// How many launches are on record, with the reason when that is not knowable.
    pub fn launches(&self) -> Measured<u64> {
        match self {
            History::Recorded { launches, .. } => Measured::known(*launches),
            History::NothingRecorded => Measured::known(0),
            History::Unreadable(why) => Measured::unavailable(why.clone()),
        }
    }

    /// Whether this is an answer rather than a failure to get one.
    pub fn determined(&self) -> bool {
        !matches!(self, History::Unreadable(_))
    }
}

/// Read the launch record.
///
/// Never fails: an unreadable file is one of the answers, because a monitor must not refuse to
/// start over its own diary. A line that does not parse makes the whole history unreadable rather
/// than being skipped — a skipped line is a launch that silently never happened, and this file
/// exists precisely so that launches stop being silent.
pub fn history(store: &StateStore) -> History {
    let path = store.paths().state_dir().join(STARTS_FILE);
    let text = match store.read_text(STARTS_FILE) {
        StateRead::Found(text) => text,
        StateRead::Absent => return History::NothingRecorded,
        StateRead::Unreadable(why) => return History::Unreadable(why),
    };

    let mut records: Vec<StartRecord> = Vec::new();
    let mut launches: u64 = 0;
    for (number, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<StartRecord>(line) {
            // A line whose fields happen to fit but whose version this build does not know is
            // refused, the same way `state.rs` refuses an unknown state version. A newer record
            // read as if it were this one would be a downtime and a verdict computed from fields
            // that mean something else — figures that look exactly like measurements.
            Ok(record) if record.version != RECORD_VERSION => {
                return History::Unreadable(format!(
                    "line {} of {} is a launch record of version {} and this build understands \
                     version {RECORD_VERSION}",
                    number + 1,
                    path.display(),
                    record.version
                ))
            }
            Ok(record) => {
                launches += 1;
                records.push(record);
            }
            Err(error) => {
                return History::Unreadable(format!(
                    "line {} of {} is not a launch record this tool wrote: {error}",
                    number + 1,
                    path.display()
                ))
            }
        }
    }

    if launches == 0 {
        return History::NothingRecorded;
    }

    // Only the tail is kept. The verdict looks back a fixed number of launches, and holding a
    // year of them in memory to answer a question about the last five would make the monitor's
    // own footprint grow with its uptime.
    let keep = RECENT_LAUNCHES.saturating_sub(1).max(1);
    let recent = records.split_off(records.len().saturating_sub(keep));
    History::Recorded { launches, recent }
}

/// Append one record, atomically enough that a crash cannot leave half a line behind.
///
/// `O_APPEND` and a single `write` of one short line, rather than the write-temp-then-rename this
/// crate uses for whole-file state. Rewriting the file to add a line would put the entire launch
/// history at risk of the very crash the history exists to record.
pub fn append(store: &StateStore, record: &StartRecord) -> Result<(), String> {
    let state_dir = store.paths().state_dir();
    std::fs::create_dir_all(state_dir).map_err(|error| {
        format!(
            "the state directory {} could not be created: {error}",
            state_dir.display()
        )
    })?;

    let path = state_dir.join(STARTS_FILE);
    let mut line = serde_json::to_string(record)
        .map_err(|error| format!("the launch record could not be serialised: {error}"))?;
    line.push('\n');

    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|error| format!("{} could not be opened to append: {error}", path.display()))?;

    file.write_all(line.as_bytes())
        .map_err(|error| format!("the launch record could not be written: {error}"))?;
    file.sync_all()
        .map_err(|error| format!("the launch record could not be flushed to disk: {error}"))
}

/// Decide what this launch has to say, from what is on disk. Reads no clock and touches no file.
///
/// Every input is a parameter, including the launch instant, so that the arithmetic below — two
/// subtractions over stamps — is assertable without a test having to sleep for a gap it wants to
/// measure.
pub fn decide(
    at: SystemTime,
    pid: u32,
    last_write: &LastStateWrite,
    predecessor: Option<&Predecessor>,
    unreadable_lock_record: Option<&str>,
    history: &History,
) -> StartRecord {
    let downtime = downtime_from(at, last_write);
    let previous_exit = previous_exit_from(predecessor, unreadable_lock_record, history, &downtime);
    let previous_uptime = previous_uptime_from(last_write, history);
    let previous_run_was_short = previous_uptime.was_short();

    let recent: &[StartRecord] = match history {
        History::Recorded { recent, .. } => recent,
        History::NothingRecorded | History::Unreadable(_) => &[],
    };

    // This launch counts towards its own verdict: the run that just ended is the newest evidence
    // there is, and leaving it out would mean the crash that prompted the restart could only be
    // seen one launch later.
    let launches_considered = recent.len() + 1;
    let short_runs = recent
        .iter()
        .filter(|record| record.previous_run_was_short == Some(true))
        .count()
        + usize::from(previous_run_was_short == Some(true));
    let unclean_exits = recent
        .iter()
        .filter(|record| record.previous_exit == PreviousExit::UNCLEAN)
        .count()
        + usize::from(matches!(previous_exit, PreviousExit::Unclean { .. }));

    let cycling = (short_runs >= CYCLING_THRESHOLD).then(|| {
        format!(
            "{short_runs} of the last {launches_considered} launches followed a run shorter than \
             {}s, which is less than one slow-tier interval: this monitor is being restarted, not \
             running. {unclean_exits} of those exits {} unclean.",
            SHORT_RUN.as_secs(),
            if unclean_exits == 1 { "was" } else { "were" }
        )
    });

    StartRecord {
        version: RECORD_VERSION,
        started_at: crate::isotime::iso8601_from_unix_seconds(crate::isotime::unix_seconds(at)),
        pid,
        launches: added_one(history.launches()),
        downtime_secs: downtime.seconds(),
        previous_exit: previous_exit.label().to_string(),
        previous_exit_why: previous_exit.describe(),
        previous_pid: predecessor.map(|predecessor| predecessor.pid),
        previous_uptime_secs: previous_uptime.seconds(),
        previous_run_was_short,
        launches_considered,
        short_runs,
        unclean_exits,
        cycling,
    }
}

/// Read the record, decide what this launch says, append it — in that order.
///
/// The order is the whole mechanism. Both readings happen before this launch has written anything:
/// the downtime is the gap to the *previous* monitor's last write, and a first state write done
/// first would collapse every downtime to nothing.
pub fn record(
    store: &StateStore,
    at: SystemTime,
    pid: u32,
    predecessor: Option<&Predecessor>,
    unreadable_lock_record: Option<&str>,
) -> Launch {
    let history = history(store);
    let last_write = last_state_write(store);
    let record = decide(
        at,
        pid,
        &last_write,
        predecessor,
        unreadable_lock_record,
        &history,
    );
    let not_recorded = append(store, &record).err();
    Launch {
        record,
        not_recorded,
    }
}

/// The launch a first start in an untouched state directory produces.
///
/// For a caller that has to name a launch and is not the monitor — a test, or any code assembling
/// a payload outside a monitored run. It fabricates nothing: it is what [`decide`] returns when
/// nothing has ever been written and no lock record was left behind.
pub fn first_launch(at: SystemTime, pid: u32) -> Launch {
    Launch {
        record: decide(
            at,
            pid,
            &LastStateWrite::Never,
            None,
            None,
            &History::NothingRecorded,
        ),
        not_recorded: None,
    }
}

fn downtime_from(at: SystemTime, last_write: &LastStateWrite) -> Downtime {
    match last_write {
        LastStateWrite::Never => Downtime::NoPreviousWrite,
        LastStateWrite::Unreadable(why) => Downtime::Unreadable(format!(
            "the last state write could not be established, so the downtime cannot be measured: \
             {why}"
        )),
        LastStateWrite::At(written) => match at.duration_since(*written) {
            Ok(gap) => Downtime::Since(gap),
            Err(error) => Downtime::LastWriteAheadOfLaunch(error.duration()),
        },
    }
}

/// Resolve the cleared-versus-stale lock record against the rest of the state directory.
///
/// A cleared record means "the last holder released", and an absent one means "no holder has ever
/// recorded itself" — but they are the same empty file, so the lock alone cannot tell a normal
/// restart from a machine that has never been monitored. The record file and `state.json` settle
/// it, and where they cannot, the answer is `unknown` rather than the comfortable one.
fn previous_exit_from(
    predecessor: Option<&Predecessor>,
    unreadable_lock_record: Option<&str>,
    history: &History,
    downtime: &Downtime,
) -> PreviousExit {
    if let Some(why) = unreadable_lock_record {
        return PreviousExit::Unknown(format!(
            "the previous holder's record in the state lock could not be read ({why}), so it is \
             not known whether it released the lock or died holding it"
        ));
    }

    if let Some(predecessor) = predecessor {
        return PreviousExit::Unclean {
            pid: predecessor.pid,
            still_running: predecessor.still_running,
        };
    }

    // The lock record is clear. Whether that is a clean release or a directory nobody has ever
    // monitored depends on whether anything has ever run here.
    match history {
        History::Recorded { .. } => PreviousExit::Clean,
        History::Unreadable(why) => PreviousExit::Unknown(format!(
            "the state lock was released cleanly, but the launch record could not be read \
             ({why}), so which run released it is not known"
        )),
        History::NothingRecorded => match downtime {
            Downtime::NoPreviousWrite => PreviousExit::Absent,
            _ => PreviousExit::Unknown(format!(
                "no launch is on record in this state directory, yet {STATE_FILE} has been \
                 written here, so a monitor ran before this record file existed and its exit \
                 cannot be vouched for"
            )),
        },
    }
}

fn previous_uptime_from(last_write: &LastStateWrite, history: &History) -> Uptime {
    let Some(previous) = history.last() else {
        return match history {
            History::Unreadable(why) => Uptime::Unknown(format!(
                "the launch record could not be read, so the previous run has no start to measure \
                 from: {why}"
            )),
            _ => Uptime::NoPreviousLaunch,
        };
    };

    let started = match crate::isotime::unix_seconds_from_iso8601(&previous.started_at) {
        Ok(seconds) => crate::isotime::time_from_unix_seconds(seconds),
        Err(why) => {
            return Uptime::Unknown(format!(
                "the previous launch is stamped {:?}, which is not a time this tool wrote \
                 ({why}), so its run length cannot be computed",
                previous.started_at
            ))
        }
    };

    match last_write {
        LastStateWrite::Never => Uptime::Unknown(format!(
            "a launch is on record but {STATE_FILE} is not there, so the previous run has no last \
             write to measure to"
        )),
        LastStateWrite::Unreadable(why) => Uptime::Unknown(format!(
            "the previous run's last write could not be established: {why}"
        )),
        LastStateWrite::At(written) => match written.duration_since(started) {
            Ok(ran) => Uptime::Ran(ran),
            Err(error) => Uptime::Unknown(format!(
                "the previous launch is stamped {:.1}s after the last state write, so its run \
                 length cannot be computed; this machine's clock appears to have gone backwards",
                error.duration().as_secs_f64()
            )),
        },
    }
}

/// This launch, added to the count on record — carrying the reason forward when there is no count.
fn added_one(recorded: Measured<u64>) -> Measured<u64> {
    match recorded {
        Measured {
            value: Some(launches),
            ..
        } => Measured::known(launches + 1),
        Measured {
            unavailable: Some(why),
            ..
        } => Measured::unavailable(format!(
            "the launches already on record could not be counted, so this one cannot be numbered: \
             {why}"
        )),
        Measured { .. } => Measured::unavailable(
            "the launches on record are neither a count nor a reason, which is a bug in the \
             launch record",
        ),
    }
}

/// The one place the record file's path is spelled, for a message that has to name it.
pub fn path(store: &StateStore) -> PathBuf {
    store.paths().state_dir().join(STARTS_FILE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unclean_exit_is_never_reported_as_clean_and_never_as_absent() {
        let exit = PreviousExit::Unclean {
            pid: 4242,
            still_running: false,
        };
        assert_eq!(exit.label(), "unclean");
        assert_eq!(exit.clean(), Some(false));
        assert!(exit.describe().contains("4242"), "{}", exit.describe());
    }

    #[test]
    fn an_absent_previous_exit_is_not_a_boolean_at_all() {
        // The whole point of the criterion: `false` would read as a crash and `true` as a
        // shutdown, and a first launch had neither.
        assert_eq!(PreviousExit::Absent.clean(), None);
        assert_eq!(PreviousExit::Unknown("no idea".to_string()).clean(), None);
    }

    #[test]
    fn a_downtime_that_cannot_be_measured_carries_a_reason_instead_of_a_zero() {
        for downtime in [
            Downtime::NoPreviousWrite,
            Downtime::LastWriteAheadOfLaunch(Duration::from_secs(5)),
            Downtime::Unreadable("the disk said no".to_string()),
        ] {
            let seconds = downtime.seconds();
            assert_eq!(seconds.value, None, "{downtime:?}");
            assert!(seconds.unavailable.is_some(), "{downtime:?}");
        }
    }

    #[test]
    fn an_unknown_run_length_is_not_a_short_run() {
        assert_eq!(Uptime::NoPreviousLaunch.was_short(), None);
        assert_eq!(Uptime::Unknown("who knows".to_string()).was_short(), None);
        assert_eq!(Uptime::Ran(Duration::from_secs(1)).was_short(), Some(true));
        assert_eq!(Uptime::Ran(SHORT_RUN * 2).was_short(), Some(false));
    }
}
