//! Pure liveness state machine for classifying agent sessions.
//!
//! A session can be actively working, waiting for human input, stalled and likely dead,
//! or honestly unknown. The verdict is based on measurable evidence — how long since the
//! transcript last changed, whether the session process is still resident, and whether
//! other work is running in the workspace — and every state where the conclusion rests on
//! inference rather than direct observation is labelled as such.
//!
//! **This module is pure.** It owns no clock, reads no files, enumerates no processes.
//! Everything arrives as arguments, which makes the logic deterministic and testable
//! without timing races or fixture files.
//!
//! ## Why these thresholds
//!
//! The defaults are justified from measured silence distributions in
//! `docs/observability-mechanics.md` §3.3. Real agent work exhibits legitimate silence of
//! several minutes when builds, tests, or other workspace tasks are running, and sessions
//! have resumed after gaps exceeding eight hours. The thresholds sit above normal
//! interaction cadence while staying below the measured resumption ceiling — though that
//! ceiling itself is bounded by the sampling window, so even a twelve-hour stall threshold
//! cannot guarantee a session is permanently dead. The one thing that can is the absence
//! of a resident process, which is directly observable.

use std::time::Duration;

/// A session's liveness state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// The session is actively working. Its transcript changed recently.
    Active,
    /// The session is probably waiting for human input. The transcript is stale, but the
    /// process is still resident, or work is running in the workspace.
    Waiting,
    /// The session is probably dead. The transcript is stale past the stall threshold, and
    /// the process is absent.
    Stalled,
    /// The session's state cannot be determined. Either the necessary observations could
    /// not be made, or the available evidence is insufficient to reach a verdict.
    Unknown,
}

impl State {
    /// The word this state is reported under, on screen and on disk.
    ///
    /// One function so the two cannot drift. A state file saying `ACTIVE` where the display says
    /// something else would make a reader comparing the two doubt both.
    pub fn label(&self) -> &'static str {
        match self {
            State::Active => "ACTIVE",
            State::Waiting => "WAITING",
            State::Stalled => "STALLED",
            State::Unknown => "UNKNOWN",
        }
    }
}

/// How a verdict was reached.
///
/// Asserted verdicts rest on direct observations — the transcript changed, or it did not.
/// Inferred verdicts rest on guesses about human intent: silence plus a resident process
/// suggests waiting, but the human might have walked away permanently while the process
/// continues idling. Callers must be able to distinguish the two.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    /// The transcript's last-modified time could not be established at all.
    TranscriptActivityUnknown,
    /// The process snapshot cannot establish absence, so a missing process is not evidence.
    SnapshotCannotEstablishAbsence,
    /// The transcript changed recently enough to assert activity.
    TranscriptChangedRecently,
    /// The process is resident but the transcript is stale past the quiet threshold.
    /// Inferred: the process could be idling after the human abandoned it.
    ProcessResidentButSilent,
    /// Work is running in the workspace, which is legitimate silence.
    /// Inferred: the work might have stalled, but the session is likely waiting for it.
    WorkRunningInWorkspace,
    /// The process is absent and silence exceeds the stall threshold.
    NoProcessAndSilencePastStall,
    /// The process is absent but the stall threshold has not yet passed. Not enough
    /// evidence to call it stalled; not enough to call it active.
    ProcessAbsentBeforeStallThreshold,
}

impl Method {
    /// Whether this verdict was inferred from circumstantial evidence rather than asserted
    /// from a direct observation.
    ///
    /// Only two verdicts are inferred: `ProcessResidentButSilent` and
    /// `WorkRunningInWorkspace`. Both rest on silence plus something else being true, and
    /// both guess that the human intends to return. Everything else is either directly
    /// observed (the transcript changed, the process is gone) or an admission that the
    /// observation could not be made.
    pub fn is_inferred(&self) -> bool {
        matches!(
            self,
            Method::ProcessResidentButSilent | Method::WorkRunningInWorkspace
        )
    }
}

impl std::fmt::Display for Method {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Method::TranscriptActivityUnknown => write!(f, "transcript unknown"),
            Method::SnapshotCannotEstablishAbsence => write!(f, "snapshot untrustworthy"),
            Method::TranscriptChangedRecently => write!(f, "transcript changed"),
            Method::ProcessResidentButSilent => write!(f, "process resident, silent"),
            Method::WorkRunningInWorkspace => write!(f, "work running"),
            Method::NoProcessAndSilencePastStall => write!(f, "no process, stale"),
            Method::ProcessAbsentBeforeStallThreshold => write!(f, "process absent, unsure"),
        }
    }
}

/// A liveness verdict: the state, and how it was reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Verdict {
    pub state: State,
    pub method: Method,
}

/// The silence thresholds that drive liveness classification.
///
/// A silence shorter than `quiet` is active work. A silence longer than `quiet` but with
/// a resident process or live workspace activity is waiting. A silence longer than `stall`
/// with no resident process is stalled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Thresholds {
    /// How long a transcript can be silent before the session is no longer considered
    /// actively working. Silence past this threshold downgrades to WAITING if the process
    /// is still resident, or to UNKNOWN if the process is absent but the stall threshold
    /// has not yet passed.
    pub quiet: Duration,
    /// How long a transcript can be silent, with no resident process, before the session
    /// is considered stalled. This is not a guarantee that the session is permanently dead
    /// — only that it has been silent and process-absent for long enough to be worth
    /// investigating.
    pub stall: Duration,
    /// How long a workspace must stay settled — holding nothing worth protecting, with
    /// nothing driving it — before it is dropped from the state carried between runs.
    ///
    /// Not a liveness threshold, and it is here rather than in
    /// [`memory`](crate::memory) because this struct is what a collection is already
    /// configured with. Splitting the tool's two configurable durations across two
    /// structures would mean two places to look and two to keep consistent. The rule it
    /// drives lives in `memory`; only the value lives here.
    pub forget: Duration,
}

impl Default for Thresholds {
    /// Default thresholds justified from measured silence in real agent sessions.
    ///
    /// Measured data from `docs/observability-mechanics.md` §3.3:
    ///
    /// - Post-assistant silence (i.e. a human has been asked something and has not replied)
    ///   is p90 8.2 s, p99 3.9 minutes. A `quiet` threshold of **10 minutes** sits well
    ///   above normal interaction cadence, so a WAITING verdict is not noise.
    ///
    /// - The longest observed silence followed by resumption was 8.1 hours after an
    ///   assistant record, and 9.3 hours overall. A `stall` threshold of **12 hours** is
    ///   above both.
    ///
    /// **Limit of the data:** the 9.3 hour maximum is bounded by a three-day sampling
    /// window, so it structurally cannot contain a session left open over a weekend and
    /// resumed on Monday (which would show a gap exceeding 60 hours). Twelve hours is
    /// therefore not a safety guarantee derived from data — it is a floor above what was
    /// seen in the sample. The weight in a STALLED verdict is carried by the absence of a
    /// resident process, which is observed rather than inferred.
    /// - The retention period is not derived from silence at all; see
    ///   [`DEFAULT_FORGET`](crate::memory::DEFAULT_FORGET) for what bounds it.
    fn default() -> Self {
        Thresholds {
            quiet: Duration::from_secs(10 * 60),   // 10 minutes
            stall: Duration::from_secs(12 * 3600), // 12 hours
            forget: crate::memory::DEFAULT_FORGET,
        }
    }
}

/// The environment variables that override the defaults.
pub const QUIET_THRESHOLD_VARIABLE: &str = "ACMON_QUIET_SECONDS";
pub const STALL_THRESHOLD_VARIABLE: &str = "ACMON_STALL_SECONDS";

impl Thresholds {
    /// Build thresholds from two optional textual values, falling back to the defaults.
    ///
    /// A value that is present but unreadable is an **error**, never a silent fall back to
    /// the default. Someone who sets a threshold and gets the default anyway would be
    /// reading verdicts produced by a rule they think they replaced — a plausible wrong
    /// answer of exactly the kind this project exists to remove.
    ///
    /// Pure, so it can be tested without touching the process environment, which is global
    /// and would make tests race each other.
    pub fn from_values(
        quiet: Option<&str>,
        stall: Option<&str>,
        forget: Option<&str>,
    ) -> Result<Self, String> {
        let defaults = Thresholds::default();
        let parse = |value: Option<&str>, name: &str, fallback: Duration| match value {
            None => Ok(fallback),
            Some(text) => text
                .trim()
                .parse::<u64>()
                .map(Duration::from_secs)
                .map_err(|e| format!("{name} is {text:?}, which is not a number of seconds: {e}")),
        };

        let quiet = parse(quiet, QUIET_THRESHOLD_VARIABLE, defaults.quiet)?;
        let stall = parse(stall, STALL_THRESHOLD_VARIABLE, defaults.stall)?;
        // The retention rule belongs to `memory`, so the reading of its value does too.
        let forget = crate::memory::retention_from_value(forget)?;

        // A stall threshold below the quiet one would make the states inconsistent: a
        // session could be past "probably dead" while still inside "probably working".
        if stall < quiet {
            return Err(format!(
                "{STALL_THRESHOLD_VARIABLE} ({}s) is below {QUIET_THRESHOLD_VARIABLE} ({}s), \
                 which would put a session past stalled while still counting as active",
                stall.as_secs(),
                quiet.as_secs()
            ));
        }
        Ok(Thresholds {
            quiet,
            stall,
            forget,
        })
    }

    /// Read the thresholds this machine is configured with.
    pub fn from_environment() -> Result<Self, String> {
        Thresholds::from_values(
            std::env::var(QUIET_THRESHOLD_VARIABLE).ok().as_deref(),
            std::env::var(STALL_THRESHOLD_VARIABLE).ok().as_deref(),
            std::env::var(crate::memory::FORGET_VARIABLE)
                .ok()
                .as_deref(),
        )
    }
}

/// The observations needed to classify a session's liveness.
///
/// Everything is an argument — no clocks, no I/O, no process enumeration happens here. The
/// caller provides what was observed, and the classifier produces a verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observation {
    /// How long since this session's transcript last changed. `None` when that could not
    /// be established at all — the file was unreadable, the modification time was
    /// unavailable, or the current time is unknown.
    pub silence: Option<Duration>,
    /// Whether the session's own process was present in the enumeration. A process being
    /// absent is only meaningful when the enumeration is trustworthy; see
    /// `snapshot_trustworthy`.
    pub process_resident: bool,
    /// Whether some other process is doing work in this session's workspace — a build, a
    /// test run, a linter. Measured legitimate silence of several minutes exists precisely
    /// because of these. If work is running, the session is likely waiting for it to
    /// complete, not stalled.
    pub work_running_in_workspace: bool,
    /// Whether the process enumeration can be reasoned from. An incomplete enumeration
    /// cannot establish that something is absent — only that it was not seen, which might
    /// be an enumeration failure rather than a true absence.
    pub snapshot_trustworthy: bool,
}

/// Classify a session's liveness from what was observed.
///
/// Evaluated in order; the first matching condition wins. The order is load-bearing: row 2
/// must come before any reasoning from `!process_resident`, or an untrustworthy snapshot
/// could produce a false STALLED verdict. Row 5 must come before row 6, or live workspace
/// activity could be misclassified as stalled.
///
/// | Condition | State | Method |
/// |-----------|-------|--------|
/// | `silence` is `None` | UNKNOWN | transcript activity unknown |
/// | `!snapshot_trustworthy && !process_resident` | UNKNOWN | snapshot cannot establish absence |
/// | `silence <= quiet` | ACTIVE | transcript changed recently |
/// | `process_resident` | WAITING | process resident but silent |
/// | `work_running_in_workspace` | WAITING | work running in workspace |
/// | `silence > stall` | STALLED | no process and silence past stall |
/// | otherwise | UNKNOWN | process absent before stall threshold |
pub fn classify(observation: &Observation, thresholds: &Thresholds) -> Verdict {
    // Row 1: If we cannot tell how long the transcript has been silent, we know nothing.
    //
    // Bound here rather than checked and unwrapped separately: the row order below is
    // load-bearing, and a separate unwrap would become a panic the moment someone inserted
    // a row between the check and the use.
    let Some(silence) = observation.silence else {
        return Verdict {
            state: State::Unknown,
            method: Method::TranscriptActivityUnknown,
        };
    };

    // Row 2: If the snapshot is untrustworthy and the process is not resident, we cannot
    // conclude that the process is absent — it might have been missed by the enumeration.
    // This must come before any row that reasons from `!process_resident`.
    if !observation.snapshot_trustworthy && !observation.process_resident {
        return Verdict {
            state: State::Unknown,
            method: Method::SnapshotCannotEstablishAbsence,
        };
    }

    // Row 3: The transcript changed recently. This is an assertion, not an inference.
    if silence <= thresholds.quiet {
        return Verdict {
            state: State::Active,
            method: Method::TranscriptChangedRecently,
        };
    }

    // Row 4: The transcript is stale past the quiet threshold, but the process is still
    // resident. Inferred: the session is probably waiting for human input, but the human
    // might have walked away while the process continues idling.
    if observation.process_resident {
        return Verdict {
            state: State::Waiting,
            method: Method::ProcessResidentButSilent,
        };
    }

    // Row 5: Work is running in the workspace. Inferred: the session is probably waiting
    // for that work to complete. This must come before row 6, or live workspace activity
    // could be misclassified as stalled.
    if observation.work_running_in_workspace {
        return Verdict {
            state: State::Waiting,
            method: Method::WorkRunningInWorkspace,
        };
    }

    // Row 6: The process is absent and silence exceeds the stall threshold. This is the
    // strongest evidence that the session is dead, though even this cannot be certain —
    // the threshold is bounded by the sampling window that produced it.
    if silence > thresholds.stall {
        return Verdict {
            state: State::Stalled,
            method: Method::NoProcessAndSilencePastStall,
        };
    }

    // Row 7: The process is absent, but the stall threshold has not yet passed. Not enough
    // evidence to call it stalled; not enough to call it active. Honest admission that we
    // do not know.
    Verdict {
        state: State::Unknown,
        method: Method::ProcessAbsentBeforeStallThreshold,
    }
}
