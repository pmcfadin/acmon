//! Seam 14 — the display's decisions, kept out of its drawing.
//!
//! `agtop` is read-only (F26) and refreshes by **polling** the file `amon` writes (F27). Both
//! of those are decisions rather than pictures, and every one of them lives here as a function
//! over data: what a `stat` of the state file means, whether the file has to be re-read, what
//! to say when it is missing or torn, what a keypress does, and what figure to print where a
//! duty cycle would go when nothing published one.
//!
//! Nothing in this module touches a terminal. That is the point: a decision that can only be
//! observed by looking at a screen is a decision nobody will test.
//!
//! ## Freshness, which is the highest-stakes decision here
//!
//! A resident monitor is a new way to produce a calm, plausible, wrong answer: `amon` wedges
//! forty minutes ago and `state.json` still reads as perfectly healthy (PRD §2.2, §6.2). So the
//! display classifies the monitor's presence itself, from the file alone — [`Presence`] — and it
//! rests on **two independent observations**, not one:
//!
//! - **Whether the writer still exists.** The state file records the pid that wrote it, and
//!   `kill(pid, 0)` answers whether that process is still there. A monitor that is gone is
//!   *observable*, not merely inferable from silence.
//! - **How old each tier's facts are.** Every tier carries its own stamp, and the monitor
//!   publishes the cadence it is running at, so "this tier has missed a whole pass" is a
//!   decision over data rather than a guess.
//!
//! Both are parameters here — the clock and the pid check arrive from the caller — because a
//! freshness rule that can only be observed by waiting, or by killing a process, is a rule
//! nobody will test. See [`presence_of`].

use std::path::Path;
use std::time::{Duration, Instant, SystemTime};

use crate::collect::{collect_as, Role, Session};
use crate::liveness::Thresholds;
use crate::meter::tier_name;
use crate::schedule::{Cadence, Pace, Tier, TIERS};
use crate::state::{StateStore, TieredState, STATE_FILE};
use crate::tiers::{
    self, FastPayload, MediumPayload, Published, PublishedMeters, SlowPayload, WorkspaceRow,
};
use crate::{Snapshot, World};

/// How often the state file is `stat`ed.
///
/// A second, and a `stat` — not a filesystem-event watcher. A watcher would gain a fraction of
/// a second and add a class of "the watcher silently stopped delivering" bug to a tool whose
/// whole thesis is that silent background failure is the enemy (F27). A `stat` that fails
/// fails in front of the caller.
pub const POLL_INTERVAL: Duration = Duration::from_secs(1);

/// What a `stat` of the state file found.
///
/// Three answers, never two: a file that cannot be `stat`ed is not an absent file. Reading it
/// as one would turn a permissions problem on the monitor's own directory into "no monitor is
/// running", and the display would then collect for itself and look perfectly healthy doing it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stat {
    /// The file is there, last modified at this instant.
    At(SystemTime),
    /// There is no file at that path.
    Absent,
    /// The path could not be `stat`ed at all, and why.
    Failed(String),
}

/// `stat` the state file. The only filesystem call in this module.
pub fn stat_state_file(path: &Path) -> Stat {
    match std::fs::metadata(path) {
        Ok(metadata) => match metadata.modified() {
            Ok(mtime) => Stat::At(mtime),
            // A file whose modification time this platform will not report cannot be polled
            // by mtime at all. Said rather than papered over with `now`, which would make
            // every poll look like a change and re-read the file forever.
            Err(error) => Stat::Failed(format!(
                "{} has no readable modification time: {error}",
                path.display()
            )),
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Stat::Absent,
        Err(error) => Stat::Failed(format!("{} could not be checked: {error}", path.display())),
    }
}

/// What one poll concluded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Poll {
    /// Read the file: this is the first look at it, or its modification time moved.
    Reread,
    /// Exactly what the last poll found. Nothing is read and nothing is redrawn.
    Unchanged,
    /// There is no state file, and the last poll did not say so.
    Absent,
    /// The state file could not be `stat`ed, and the last poll did not say so.
    Unstattable(String),
}

/// The mtime the last poll saw, and therefore what the next one means.
///
/// Holding the previous observation rather than a "changed?" boolean is what lets an absent
/// file, a torn file and an unchanged file be three outcomes rather than one — and what keeps
/// the display from re-reading, and redrawing, a file that has not moved.
#[derive(Debug, Default)]
pub struct Poller {
    previous: Option<Stat>,
}

impl Poller {
    pub fn new() -> Self {
        Poller::default()
    }

    /// Fold one `stat` into what was already known.
    ///
    /// A file that reappears after being absent is re-read even if its modification time is
    /// one the poller has seen before: what changed is that it exists.
    pub fn observe(&mut self, stat: Stat) -> Poll {
        if self.previous.as_ref() == Some(&stat) {
            return Poll::Unchanged;
        }
        let poll = match &stat {
            Stat::At(_) => Poll::Reread,
            Stat::Absent => Poll::Absent,
            Stat::Failed(why) => Poll::Unstattable(why.clone()),
        };
        self.previous = Some(stat);
        poll
    }
}

/// What the state file said when it was read.
///
/// Four outcomes, never fewer. Each of the three that are not [`StateReading::Published`] is a
/// different way for a display to show a calm, plausible, wrong screen, and they need different
/// sentences: no monitor at all, a monitor with nothing published yet, and a file that cannot be
/// believed are three situations a reader would respond to in three different ways.
#[derive(Debug, Clone, PartialEq)]
pub enum StateReading {
    /// No state file. Nothing has ever published here, so nothing is being recorded or
    /// alerted, and the display has to collect for itself (F28).
    Absent,
    /// A monitor published facts, and this display decoded them.
    ///
    /// Boxed because it is two orders of magnitude larger than every other arm — a whole
    /// collection — and an enum sized by its success case is paid for on every failure too.
    Published(Box<PublishedReading>),
    /// A state file naming a writer, carrying nothing this display can draw from.
    ///
    /// Since #30 this means one thing only: the monitor holds the writer role and has not
    /// published a **fast** pass, which is where the sessions and its own figures live. It is a
    /// real state — every start passes through it — and it is emphatically not "the payloads
    /// cannot be read", which is what it used to mean and is now [`StateReading::Unusable`].
    Unrenderable { writer_pid: u32, why: String },
    /// There is a file and it cannot be believed — torn, truncated, of an unknown version, or
    /// carrying a tier payload this build does not understand. Nothing is taken from it, at all.
    ///
    /// The alternative, taking whatever parsed, is the defect this whole project is against:
    /// half a state file renders as a shorter session list and a shorter at-risk panel, which
    /// is precisely the shape of a healthy screen.
    Unusable(String),
}

/// Every tier the monitor published, decoded, each with the stamp it was carried under.
///
/// The fast tier is not optional here, and that is a decision rather than a convenience: it
/// carries the sessions **and** the monitor's account of itself, so a file without one says
/// nothing about the machine and nothing about the monitor's cost. A reader that treated it as
/// optional would draw an at-risk panel beside an empty session table and a meter row of
/// question marks, which is the shape of an idle machine.
#[derive(Debug, Clone, PartialEq)]
pub struct PublishedReading {
    /// The pid that wrote the file. What [`Writer`] is asked about.
    pub writer_pid: u32,
    pub fast: Box<FastPayload>,
    /// The instant the fast tier's facts were observed at.
    pub fast_stamp: SystemTime,
    /// The searches, when a medium pass has run. `None` while the monitor is warming up.
    pub medium: Option<(Box<MediumPayload>, SystemTime)>,
    /// The workspaces, when a slow pass has run, and the age of the **oldest** of them — which
    /// is what the tier's stamp carries, not the instant the last pass ran (F30).
    pub slow: Option<(Box<SlowPayload>, SystemTime)>,
    /// The monitor's own figures, for the meter row.
    pub meters: PublishedMeters,
}

impl PublishedReading {
    /// The workspaces the slow tier published, or an empty slice while it has published none.
    ///
    /// An empty slice is safe here **only** because nothing downstream reads emptiness as a
    /// clear result: the panel says how many workspaces have never been read, and a monitor with
    /// no slow pass at all is reported as warming up.
    pub fn workspaces(&self) -> &[WorkspaceRow] {
        match &self.slow {
            Some((slow, _)) => &slow.workspaces,
            None => &[],
        }
    }
}

/// The tier count a state file has to carry before it says anything about the machine.
///
/// Named so that the message below and the check that produces it cannot disagree.
const NO_TIERS: usize = 0;

/// Read the state file, in whatever condition it is in.
pub fn read_state_file(store: &StateStore) -> StateReading {
    match store.read_tiered_state(STATE_FILE) {
        Ok(None) => StateReading::Absent,
        Ok(Some(state)) => reading_of(&state),
        Err(why) => StateReading::Unusable(why),
    }
}

/// The same decision, over a state file already in hand.
///
/// Separated so that a test can drive it from a [`TieredState`] it built, and so that the
/// decoding is one function rather than one per caller. Every payload goes through
/// [`crate::tiers::published`] — the schema is owned beside the code that writes it, so a field
/// renamed on the monitor's side fails to compile here rather than turning into an absent figure
/// on a screen.
pub fn reading_of(state: &TieredState) -> StateReading {
    let writer_pid = state.writer_pid();

    // Any tier that is present and cannot be understood condemns the whole file. Taking the
    // tiers that happened to decode would render a shorter session list and a shorter at-risk
    // panel, which is the shape of a healthy screen.
    let decoded = match TIERS
        .iter()
        .map(|tier| tiers::published(state, *tier))
        .collect::<Result<Vec<_>, String>>()
    {
        Ok(decoded) => decoded,
        Err(why) => return StateReading::Unusable(why),
    };

    let mut fast = None;
    let mut medium = None;
    let mut slow = None;
    for entry in decoded.into_iter().flatten() {
        match entry {
            (Published::Fast(payload), stamp) => fast = Some((payload, stamp)),
            (Published::Medium(payload), stamp) => medium = Some((payload, stamp)),
            (Published::Slow(payload), stamp) => slow = Some((payload, stamp)),
        }
    }

    let Some((fast, fast_stamp)) = fast else {
        let tiers = state.tier_count();
        return StateReading::Unrenderable {
            writer_pid,
            why: if tiers == NO_TIERS {
                "it holds the writer role and has completed no pass yet, so it has published no \
                 tier to draw from"
                    .to_string()
            } else {
                format!(
                    "it published {tiers} tier(s) but no fast pass, and the sessions and its own \
                     cost are both in the fast tier"
                )
            },
        };
    };

    let meters =
        match tiers::published_meters(state) {
            Ok(Some(meters)) => meters,
            // Unreachable by construction — the fast payload above is exactly what this reads — and
            // said rather than unwrapped, because an `expect` here would be a panic on a file
            // someone else wrote.
            Ok(None) => return StateReading::Unusable(
                "the fast tier decoded but its self-metering did not, which is a fault in this \
                 build rather than in the file"
                    .to_string(),
            ),
            Err(why) => return StateReading::Unusable(why),
        };

    StateReading::Published(Box::new(PublishedReading {
        writer_pid,
        fast,
        fast_stamp,
        medium,
        slow,
        meters,
    }))
}

// --- Monitor presence: is it there, and how old is what it left? -----------------------------

/// Whether the process that wrote the state file still exists.
///
/// Asked of the kernel rather than inferred from a stamp, because a wedged monitor and a dead one
/// both stop publishing and only one of them will ever publish again. A parameter everywhere it
/// is used, so that every transition below is testable without a process to kill.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Writer {
    /// The pid still refers to a process — which includes a zombie, and that matters: a
    /// `SIGKILL`ed monitor answers to signal 0 until its parent reaps it, so `STALE` is reached
    /// before `DEAD` rather than instead of it.
    Alive,
    /// The pid refers to nothing. Nothing will ever refresh this file.
    Gone,
}

impl Writer {
    /// Ask the kernel about a pid, without disturbing it.
    ///
    /// Signal 0 performs the existence and permission checks and delivers nothing. **This is the
    /// only thing this tool will ever do with a monitor's pid.** However dead it decides the
    /// monitor is, it does not signal it, restart it, or tidy up after it (N1): a stalled
    /// monitor holding a lock has to be looked at by a human.
    ///
    /// One honest limitation, and the reason the verdict is never the only thing on screen: a pid
    /// can be reused, so `Alive` is an observation about a pid rather than a proof that *this*
    /// monitor lives. The ages are shown beside it precisely so a reader can see a live pid that
    /// has published nothing for an hour.
    pub fn of(pid: u32) -> Writer {
        // Never signal 0's process group. `kill(0, 0)` addresses every process in the caller's
        // own group rather than a pid, so a file naming 0 as its writer must not be turned into
        // a signal to the whole terminal session. It names no process, so nothing is there.
        if pid == 0 {
            return Writer::Gone;
        }
        if crate::real_world::process_exists(pid as i32) {
            Writer::Alive
        } else {
            Writer::Gone
        }
    }
}

/// How far past its own interval a tier's stamp may be before the screen stops trusting it.
///
/// Two intervals, because one is not late. A tier stamped exactly one interval ago is a tier
/// **due right now** — the ordinary state of the fast tier for most of every interval — and a
/// screen that flipped to `STALE` at that boundary would blink the word every ten seconds and
/// teach a reader to ignore it. Two intervals means a pass that should have happened did not,
/// which is a fact about the monitor rather than about scheduling jitter.
///
/// The cost of the choice, stated rather than hidden: staleness is reported after the monitor has
/// missed a whole pass of the tier concerned, not the instant it is late. At the active cadence
/// that is 20 s for the fast tier.
pub const MISSED_A_PASS: u32 = 2;

/// How old one tier's facts are, and whether that tier is still publishing.
///
/// **Two ages, not one, and the difference is the whole of F30.** For the fast and medium tiers
/// they are the same instant. For the slow tier they are not: it reads a slice of workspaces per
/// pass, so its stamp is the age of the *oldest* workspace reading it published — twenty to
/// thirty minutes on a machine with seventy repositories — while its last pass may have been
/// seconds ago. Judging the monitor's health by the evidence age would report every healthy
/// monitor as stale; judging the evidence by the pass age would present a twenty-minute-old git
/// fact as seconds old. Both are needed, so both are here.
#[derive(Debug, Clone, PartialEq)]
pub struct TierAge {
    pub tier: Tier,
    pub name: &'static str,
    /// How long ago this tier's most recent pass **started**, or why that is not known.
    pub since_pass: Result<Duration, String>,
    /// How old the oldest fact this tier published is, or why that is not known.
    pub since_evidence: Result<Duration, String>,
    /// How often this tier runs at the cadence the monitor says it is keeping.
    pub interval: Duration,
    /// Set when this tier has missed a whole pass, with the sentence that says so.
    pub overdue: Option<String>,
}

/// Every tier's age, and what to judge them against.
#[derive(Debug, Clone, PartialEq)]
pub struct Ages {
    /// The instant every age here was computed against.
    ///
    /// Carried rather than left implicit so that the rest of the screen — a workspace's own
    /// evidence age, a remembered figure's age — is aged against the *same* instant. Two clocks
    /// read a millisecond apart would put two slightly different ages for one fact on one screen.
    pub now: SystemTime,
    pub tiers: Vec<TierAge>,
    /// The cadence word the monitor published: `active`, or `idle` when it has idled down (F22).
    pub pace: String,
    /// The cadence the ages were judged against, which is the one that word names.
    pub cadence: Cadence,
    /// Set when the monitor published a pace word this build does not know.
    ///
    /// Judged against the **idle** cadence in that case, which is the slower one: an unknown
    /// word must not manufacture a staleness warning, and it is said out loud so a reader is not
    /// left to wonder which intervals the screen used.
    pub pace_unknown: bool,
    /// Whether the monitor had still to complete its first round of every tier.
    ///
    /// From the payload's own `every_tier_has_run`, not inferred from a missing tier: a warming-up
    /// monitor and a monitor whose medium tier has died look identical from a missing payload
    /// alone, and they are opposite facts.
    pub warming_up: bool,
}

impl Ages {
    /// The oldest fact on the screen, whichever tier it came from.
    ///
    /// What the whole-screen mark states on `STALE` and `DEAD`: a reader deciding whether to
    /// trust anything at all needs the worst age, not the best.
    pub fn oldest_evidence(&self) -> Option<Duration> {
        self.tiers
            .iter()
            .filter_map(|age| age.since_evidence.as_ref().ok().copied())
            .max()
    }

    /// This tier's ages, if it is in the list.
    pub fn of(&self, tier: Tier) -> Option<&TierAge> {
        self.tiers.iter().find(|age| age.tier == tier)
    }

    /// Every tier that has missed a pass, in cadence order.
    pub fn overdue(&self) -> Vec<&TierAge> {
        self.tiers
            .iter()
            .filter(|age| age.overdue.is_some())
            .collect()
    }
}

/// How long ago `then` was, or why that is not a duration.
///
/// A stamp in the future is a clock that moved backwards between the monitor's write and this
/// read. Said, rather than clamped to zero: zero would present the least trustworthy stamp on the
/// screen as the freshest thing on it.
fn age_of(now: SystemTime, then: SystemTime) -> Result<Duration, String> {
    now.duration_since(then).map_err(|_| {
        "stamped in the future — the clock moved backwards between the write and this read, so \
         nothing here has a knowable age"
            .to_string()
    })
}

/// Every tier's age, as of `now`.
pub fn ages_of(reading: &PublishedReading, now: SystemTime) -> Ages {
    let pace = reading.fast.monitor.pace.clone();
    let understood = Pace::from_name(&pace);
    let cadence = understood.unwrap_or(Pace::Idle).cadence();
    let warming_up = !reading.fast.pass.every_tier_has_run;

    let tiers = TIERS
        .iter()
        .map(|tier| {
            let interval = cadence.interval(*tier);
            // The pass instant comes from the tier's own envelope and the evidence instant from
            // the file's stamp for that tier. For the slow tier those are deliberately different
            // instants; see `TierAge`.
            let (started_at, stamp) = match tier {
                Tier::Fast => (
                    Some(reading.fast.pass.started_at.clone()),
                    Some(reading.fast_stamp),
                ),
                Tier::Medium => (
                    reading
                        .medium
                        .as_ref()
                        .map(|(payload, _)| payload.pass.started_at.clone()),
                    reading.medium.as_ref().map(|(_, stamp)| *stamp),
                ),
                Tier::Slow => (
                    reading
                        .slow
                        .as_ref()
                        .map(|(payload, _)| payload.pass.started_at.clone()),
                    reading.slow.as_ref().map(|(_, stamp)| *stamp),
                ),
            };

            let not_published = || {
                format!(
                    "the {} tier has published nothing, so it has no age",
                    tier_name(*tier)
                )
            };

            let since_pass = match &started_at {
                Some(text) => crate::isotime::unix_seconds_from_iso8601(text)
                    .map_err(|why| {
                        format!(
                            "the {} tier's pass is stamped {text:?}, which cannot be read as a \
                             time: {why}",
                            tier_name(*tier)
                        )
                    })
                    .and_then(|seconds| {
                        age_of(now, crate::isotime::time_from_unix_seconds(seconds))
                    }),
                None => Err(not_published()),
            };
            let since_evidence = match stamp {
                Some(stamp) => age_of(now, stamp),
                None => Err(not_published()),
            };

            // Overdue is judged on the **pass**, never on the evidence. See `TierAge`.
            let overdue = match (&since_pass, warming_up) {
                (Ok(age), _) if *age > interval * MISSED_A_PASS => Some(format!(
                    "the {} tier last ran {} ago against a {} s interval, so it has missed at \
                     least one whole pass",
                    tier_name(*tier),
                    crate::render::format_age(*age),
                    interval.as_secs()
                )),
                // Nothing published and the monitor says it is past its first round: a tier that
                // was there and is not any more.
                (Err(_), false) if started_at.is_none() => Some(format!(
                    "the {} tier has published nothing, and the monitor reports having completed \
                     a round of every tier — so this tier's facts are missing rather than pending",
                    tier_name(*tier)
                )),
                _ => None,
            };

            TierAge {
                tier: *tier,
                name: tier_name(*tier),
                since_pass,
                since_evidence,
                interval,
                overdue,
            }
        })
        .collect();

    Ages {
        now,
        tiers,
        pace,
        cadence,
        pace_unknown: understood.is_none(),
        warming_up,
    }
}

/// Why no monitor's facts are on this screen.
///
/// Three situations that all end with the display collecting for itself, and they must not read
/// alike: one is an ordinary fresh install, one is a monitor that has just started, and one is a
/// file that has gone wrong.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Absence {
    /// There is no state file at all.
    NoStateFile,
    /// A monitor holds the writer role and has published nothing drawable yet.
    NothingPublished { writer_pid: u32, why: String },
    /// There is a file and it could not be believed.
    Unbelievable(String),
}

/// What the display concluded about the monitor, from the state file alone.
///
/// The four verdicts of PRD §4, and which observation each rests on is not incidental — see
/// [`Presence::rests_on`]. `Dead` outranks `Stale`, which outranks `Fresh`: a pid that is gone is
/// a monitor that will never publish again, however young the file it left behind.
#[derive(Debug, Clone, PartialEq)]
pub enum Presence {
    /// Every tier published inside its own cadence, and the writer is still there.
    Fresh { writer_pid: u32, ages: Ages },
    /// A tier has missed a whole pass. The writer still exists, so it may yet catch up.
    Stale { writer_pid: u32, ages: Ages },
    /// The pid that wrote this file no longer exists. Nothing here will be refreshed.
    ///
    /// `ages` is `None` for a monitor that died before publishing a fast pass, which is a crash
    /// on startup and the loudest thing this screen can say.
    Dead {
        writer_pid: u32,
        ages: Option<Box<Ages>>,
    },
    /// No monitor's facts are on this screen. `agtop` is doing its own live read (F28).
    Absent(Absence),
}

impl Presence {
    /// The word the whole screen is marked with.
    pub fn label(&self) -> &'static str {
        match self {
            Presence::Fresh { .. } => "FRESH",
            Presence::Stale { .. } => "STALE",
            Presence::Dead { .. } => "DEAD",
            Presence::Absent(_) => "ABSENT",
        }
    }

    /// Which observation this verdict rests on, in words.
    ///
    /// NF11 asks output to distinguish verified measurement, inference and assumption. This is
    /// that distinction for the verdict itself: `DEAD` is an observation about a pid, `STALE` is
    /// arithmetic over published stamps, and neither is a guess — but they are not the same kind
    /// of evidence and a reader deciding what to go and look at needs to know which they have.
    pub fn rests_on(&self) -> &'static str {
        match self {
            Presence::Fresh { .. } => {
                "the writer pid still exists and every tier published inside its own cadence"
            }
            Presence::Stale { .. } => {
                "the tier stamps in the file, against the cadence the monitor published — the \
                 writer pid does still exist"
            }
            Presence::Dead { .. } => {
                "asking the kernel about the writer pid the file records, not on the age of the \
                 file"
            }
            Presence::Absent(_) => "there being no monitor's facts to age",
        }
    }

    /// The ages behind this verdict, when there are any.
    pub fn ages(&self) -> Option<&Ages> {
        match self {
            Presence::Fresh { ages, .. } | Presence::Stale { ages, .. } => Some(ages),
            Presence::Dead { ages, .. } => ages.as_deref(),
            Presence::Absent(_) => None,
        }
    }

    /// Whether the figures on this screen can be trusted to describe the machine now.
    ///
    /// `false` marks the **whole screen**, never a row (F29). A stale file is uniformly
    /// untrustworthy, and marking rows individually would imply the unmarked ones were verified.
    pub fn trustworthy(&self) -> bool {
        matches!(self, Presence::Fresh { .. } | Presence::Absent(_))
    }
}

/// Classify the monitor from the file and one question to the kernel.
///
/// `now` and `writer` are both parameters: the clock so that an age is a figure computed from
/// stamps rather than a duration something slept for, and the pid check so that every transition
/// below can be driven without killing a process. The real display supplies
/// [`Writer::of`]`(reading.writer_pid)`.
pub fn presence_of(reading: &StateReading, now: SystemTime, writer: Writer) -> Presence {
    match reading {
        StateReading::Absent => Presence::Absent(Absence::NoStateFile),
        StateReading::Unusable(why) => Presence::Absent(Absence::Unbelievable(why.clone())),
        StateReading::Unrenderable { writer_pid, why } => match writer {
            // A monitor that died before its first fast pass. `DEAD` rather than `ABSENT`,
            // because a crash on startup under a `KeepAlive` LaunchAgent is a crash loop, and
            // "no monitor is recording" would describe it as a machine nobody had set up.
            Writer::Gone => Presence::Dead {
                writer_pid: *writer_pid,
                ages: None,
            },
            Writer::Alive => Presence::Absent(Absence::NothingPublished {
                writer_pid: *writer_pid,
                why: why.clone(),
            }),
        },
        StateReading::Published(published) => {
            let ages = ages_of(published, now);
            let writer_pid = published.writer_pid;
            match writer {
                Writer::Gone => Presence::Dead {
                    writer_pid,
                    ages: Some(Box::new(ages)),
                },
                Writer::Alive if ages.overdue().is_empty() => Presence::Fresh { writer_pid, ages },
                Writer::Alive => Presence::Stale { writer_pid, ages },
            }
        }
    }
}

// --- What a session cost, and therefore where its row goes ---------------------------------

/// What a session cost the machine, as far as it can be known.
///
/// Two arms, never one number. A session whose child CPU could not be read has no cost to
/// compare with anything, and folding that into `0` would rank the least knowable session as
/// the cheapest on screen — which is the position rows are dropped from when the terminal is
/// too short (NF10, F54).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cost {
    /// The child-CPU figure the row shows: read this run, or remembered from the last reading
    /// taken before the process exited.
    Measured(Duration),
    /// No child-CPU figure at all, and why. Never a duration.
    Unmeasurable(String),
}

/// What a session's row will show in its CHILD CPU column, as a cost.
///
/// The precedence is the row's precedence — a live reading, else a remembered one, else the
/// reason there is neither. Sorting on anything else would order the table by a figure that is
/// not the one printed in it, and the reader would have no way to see that.
pub fn cost_of(session: &Session) -> Cost {
    let from = |figure: &Result<Duration, crate::world::Unmeasured>| match figure {
        Ok(cpu) => Cost::Measured(*cpu),
        Err(why) => Cost::Unmeasurable(why.to_string()),
    };
    match (&session.resources, &session.last_reading) {
        (Ok(resources), _) => from(&resources.children_cpu),
        (Err(_), Some(reading)) => from(&reading.resources.children_cpu),
        (Err(why), None) => Cost::Unmeasurable(why.to_string()),
    }
}

/// The order rows are drawn in: the session costing the machine most, first.
///
/// Child CPU descending, because that is the quantity this project's thesis rests on — one
/// session spent 1,669 s in its own process and 32,317 s in its children, and a table ordered
/// by pid says nothing about which session is doing that (F55). Fixed, with no keys that change
/// it: see [`command_for`], where the absence of sort keybindings is a requirement rather than
/// an omission.
///
/// **Deterministic, including ties.** The sort is stable and the collection it is given is
/// already ordered by identity, so two sessions with the same child CPU keep that order and the
/// output reproduces run to run. A tie broken arbitrarily is a flaky test waiting to happen.
///
/// A session whose child CPU is unmeasurable sorts **above every measured one**, and that is
/// the stated position NF10 asks for. It is not the top because such a session is important;
/// it is the top because the bottom is where rows are dropped from, and an absent cost that
/// sorted low would be a session quietly ranked as cheap by a figure nobody has.
pub fn in_cost_order(sessions: &[Session]) -> Vec<&Session> {
    let mut ordered: Vec<&Session> = sessions.iter().collect();
    ordered.sort_by_key(|session| match cost_of(session) {
        // Two keys, not one: the first separates the unmeasurable from the measured, so no
        // duration can ever be compared against a stand-in for an absent one.
        Cost::Unmeasurable(_) => (0u8, std::cmp::Reverse(Duration::ZERO)),
        Cost::Measured(cpu) => (1u8, std::cmp::Reverse(cpu)),
    });
    ordered
}

/// The same cost, for a session the monitor published.
///
/// One rule, two shapes of input. The published row has already resolved the live-then-remembered
/// precedence — `children_cpu_ms` is whichever figure the row shows and `remembered_at` says
/// which — so the only thing left is that an absent figure must never become a zero.
pub fn published_cost_of(row: &crate::tiers::SessionRow) -> Cost {
    match (&row.children_cpu_ms.value, &row.children_cpu_ms.unavailable) {
        (Some(millis), _) => Cost::Measured(Duration::from_millis(*millis as u64)),
        (None, Some(why)) => Cost::Unmeasurable(why.clone()),
        // A payload with neither a figure nor a reason is a fault in the monitor's own
        // publishing. Reported as such rather than sorted as free.
        (None, None) => Cost::Unmeasurable(
            "the monitor published neither a child-CPU figure nor a reason for its absence"
                .to_string(),
        ),
    }
}

/// Published sessions in the order their rows are drawn: costliest first, unmeasurable above all.
///
/// The same ordering rule as [`in_cost_order`], and for the same reason (NF10, F54, F55): the
/// bottom of the table is what a short terminal drops, so a session with no cost at all must not
/// be ranked as a cheap one.
pub fn published_in_cost_order(
    rows: &[crate::tiers::SessionRow],
) -> Vec<&crate::tiers::SessionRow> {
    let mut ordered: Vec<&crate::tiers::SessionRow> = rows.iter().collect();
    ordered.sort_by_key(|row| match published_cost_of(row) {
        Cost::Unmeasurable(_) => (0u8, std::cmp::Reverse(Duration::ZERO)),
        Cost::Measured(cpu) => (1u8, std::cmp::Reverse(cpu)),
    });
    ordered
}

/// How many published sessions have no child-CPU figure to be ordered by.
pub fn published_sessions_without_a_cost(rows: &[crate::tiers::SessionRow]) -> usize {
    rows.iter()
        .filter(|row| matches!(published_cost_of(row), Cost::Unmeasurable(_)))
        .count()
}

/// How many of a snapshot's sessions have no child-CPU figure to be ordered by.
///
/// The screen states this rather than leaving the reader to work out why the top rows carry a
/// reason where a total should be.
pub fn sessions_without_a_cost(sessions: &[Session]) -> usize {
    sessions
        .iter()
        .filter(|session| matches!(cost_of(session), Cost::Unmeasurable(_)))
        .count()
}

/// Why a meter has no figure.
///
/// Never a zero. A duty cycle of 0% is a monitor that is running and idle, which is the one
/// thing a reader would most like to know and the one thing none of these arms mean.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unmetered {
    /// Nothing has published here, so there is no monitor whose cost could be stated (F28).
    NoMonitor,
    /// A monitor holds the writer role and has not published a pass carrying this figure yet.
    ///
    /// Replaces the arm that used to say "this display cannot read its tier payloads yet (#30)".
    /// Since #30 it can, so that reason would be a lie; what remains is the genuinely
    /// warming-up monitor, which is a state every start passes through.
    NothingPublishedYet,
    /// A monitor published something that could not be believed, so nothing was taken from it.
    Unreadable,
    /// The monitor published this figure as absent, and this is the reason it gave.
    ///
    /// Carried through verbatim rather than re-worded here. The monitor is the only thing that
    /// knows why it could not read its own ledger, and a display that substituted its own guess
    /// would be inventing a diagnosis.
    Reported(String),
}

impl std::fmt::Display for Unmetered {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Unmetered::NoMonitor => write!(
                formatter,
                "no monitor is recording, so there is none to meter"
            ),
            Unmetered::NothingPublishedYet => write!(
                formatter,
                "the monitor has not published a pass carrying this figure yet"
            ),
            Unmetered::Unreadable => write!(
                formatter,
                "the state file could not be believed, so nothing was taken from it"
            ),
            Unmetered::Reported(why) => write!(formatter, "{why}"),
        }
    }
}

/// What this tool costs, shown beside what it measured.
///
/// First-class figures rather than a debug line (F33, G7): a resident process that cannot state
/// its own duty cycle is exactly what this tool would flag on someone else's machine.
#[derive(Debug, Clone, PartialEq)]
pub struct Meters {
    /// Wall time the collection behind the figures on screen took.
    pub overhead: Result<Duration, Unmetered>,
    /// The monitor's duty cycle over its trailing window, as a fraction of 1.
    pub duty_cycle: Result<f64, Unmetered>,
    /// The instant these figures were taken as of.
    ///
    /// Carried so the meter row can state its own evidence rather than implying the gauges are
    /// current — a gauge is the easiest thing on a screen to read as live. The row prints the
    /// instant *and*, since #30, its age beside the screen's freshness mark: a duty cycle
    /// measured by a monitor that has since died is not a fact about now.
    pub taken_at: SystemTime,
}

impl Meters {
    /// The meters for figures the display collected itself.
    ///
    /// Its own overhead it measured, so that is a number. The monitor's duty cycle it cannot
    /// have, and which reason applies depends on why there were no published facts.
    pub fn for_own_collection(
        absence: &Absence,
        overhead: Duration,
        taken_at: SystemTime,
    ) -> Meters {
        Meters {
            overhead: Ok(overhead),
            duty_cycle: Err(match absence {
                Absence::NoStateFile => Unmetered::NoMonitor,
                Absence::NothingPublished { .. } => Unmetered::NothingPublishedYet,
                Absence::Unbelievable(_) => Unmetered::Unreadable,
            }),
            taken_at,
        }
    }

    /// The meters the monitor published.
    ///
    /// Both figures come across as `Result`s already, each `Err` carrying the monitor's own
    /// sentence, so nothing here has to invent a reason for an absent gauge — and nothing here
    /// can turn one into a zero.
    pub fn from_published(published: &PublishedMeters) -> Meters {
        Meters {
            overhead: published.overhead.clone().map_err(Unmetered::Reported),
            duty_cycle: published.duty_cycle.clone().map_err(Unmetered::Reported),
            taken_at: published.taken_at,
        }
    }
}

/// Whose figures are on the screen.
///
/// Three arms, because "the monitor's" and "my own" are different claims about the same numbers
/// and a reader has to be able to tell them apart. The published arm is what makes the two
/// binaries one tool: the display draws what `amon` measured, at the age `amon` stamped it.
#[derive(Debug, Clone, PartialEq)]
pub enum Facts {
    /// One collection this display made for itself, because no monitor's facts were available
    /// (F28). Untiered, and therefore expensive: it is made once per run and reused.
    Own(Box<Snapshot>),
    /// What the monitor published, tier by tier, each with its own age.
    Monitor(Box<PublishedReading>),
    /// No figures at all, and why. Never an empty table.
    None(String),
}

impl Facts {
    /// Whether there are no figures to draw.
    ///
    /// What decides `agtop --once`'s exit status: a screen with no figures on it is a stated
    /// failure, not a rendering, and a pipeline must not read it as a quiet machine.
    pub fn are_absent(&self) -> bool {
        matches!(self, Facts::None(_))
    }
}

/// What the display draws: the figures, where they came from, and what it cost to have them.
///
/// Assembled away from the terminal so the whole screen is a value a test can hold. The
/// alternative — deciding these things inside the drawing code — is what made the pre-split
/// renderer testable only through a buffer of characters.
#[derive(Debug, Clone, PartialEq)]
pub struct Screen {
    /// What has to be said before any figure is read: where these figures came from, how old
    /// they are, and anything wrong with the state file.
    ///
    /// Never empty. A screen that says nothing about its own provenance is a screen a reader
    /// will assume came from a running monitor.
    pub notices: Vec<String>,
    /// What the display concluded about the monitor. Marks the whole screen (F29).
    pub presence: Presence,
    /// The figures, whose they are, or the reason there are none.
    pub facts: Facts,
    pub meters: Meters,
}

impl Screen {
    /// The screen for a display that had to collect for itself (F28).
    ///
    /// Reached for exactly three states, all of them [`Presence::Absent`] or a monitor that died
    /// before publishing anything: there is nothing of the monitor's to draw, so the display
    /// draws its own read and says so in the first sentence on the screen.
    pub fn from_own_collection(
        presence: &Presence,
        facts: Result<Snapshot, String>,
        overhead: Duration,
        taken_at: SystemTime,
    ) -> Screen {
        let mine = format!(
            "These figures are one read this display took for itself at {}.",
            crate::isotime::iso8601_from_unix_seconds(crate::isotime::unix_seconds(taken_at))
        );

        let absence = match presence {
            Presence::Absent(absence) => absence.clone(),
            // A monitor whose pid is gone and which published nothing. There is no absence
            // reason on the file's side, so the meter row's is the warming-up one; the notice
            // below says the louder thing.
            _ => Absence::NothingPublished {
                writer_pid: 0,
                why: "it published no drawable tier".to_string(),
            },
        };

        // Every screen opens with the word the whole screen is marked with — `ABSENT` here, and
        // `FRESH`/`STALE`/`DEAD` on a published one. The word is the label a reader learns to look
        // for, so it is on the screen and not only in the enum.
        let said = match presence {
            Presence::Absent(Absence::NoStateFile) => format!(
                "NO MONITOR IS RECORDING. There is no state file, so nothing is being recorded \
                 and nothing will be announced. {mine}"
            ),
            Presence::Absent(Absence::NothingPublished { writer_pid, why }) => format!(
                "the monitor at pid {writer_pid} is not being drawn: {why}. Nothing it has \
                 measured is on this screen. {mine}"
            ),
            Presence::Absent(Absence::Unbelievable(reason)) => format!(
                "THE STATE FILE COULD NOT BE BELIEVED: {reason}. Nothing was taken from it — \
                 not one row — because half of a state file renders as a short session list \
                 and a short at-risk panel, which is the shape of a healthy screen. {mine}"
            ),
            Presence::Dead { writer_pid, .. } => format!(
                "THE MONITOR IS DEAD: pid {writer_pid} wrote the state file and no longer \
                 exists, and it published no facts before it went — so nothing is being \
                 recorded and nothing will be announced. Under a KeepAlive LaunchAgent a \
                 monitor that dies before its first pass is a crash loop, not a machine nobody \
                 set up. {mine}"
            ),
            // Unreachable: a `FRESH` or `STALE` monitor has published facts, which is the other
            // constructor. Stated rather than panicked on, because a wrong sentence is
            // survivable and a display that aborts is not.
            _ => format!(
                "this screen was assembled from this display's own read while the monitor was \
                 classified as it is above, which is a fault in this build rather than in the \
                 file. {mine}"
            ),
        };
        let mut notices = vec![format!("{} — {said}", presence.label())];

        notices.extend(read_only_notice());

        Screen {
            notices,
            presence: presence.clone(),
            facts: match facts {
                Ok(snapshot) => Facts::Own(Box::new(snapshot)),
                Err(why) => Facts::None(why),
            },
            meters: Meters::for_own_collection(&absence, overhead, taken_at),
        }
    }

    /// The screen for facts a monitor published.
    ///
    /// Nothing is collected here: every figure came off disk, at the age its tier stamped it.
    /// The whole screen carries the freshness mark, and on `STALE` or `DEAD` it carries the age
    /// of the oldest fact on it — never a per-row judgement, because a stale file is uniformly
    /// untrustworthy and marking rows individually would imply the unmarked ones were verified
    /// (F29).
    pub fn from_published(presence: &Presence, reading: &PublishedReading) -> Screen {
        let mut notices = vec![monitor_notice(presence, reading)];
        notices.extend(pace_notices(presence));
        notices.extend(read_only_notice());

        Screen {
            notices,
            presence: presence.clone(),
            facts: Facts::Monitor(Box::new(reading.clone())),
            meters: Meters::from_published(&reading.meters),
        }
    }
}

/// The whole-screen mark: what the monitor is, and what that makes of the figures below.
///
/// Four sentences, deliberately different from each other. "The monitor is alive and this tier is
/// simply slow" and "the monitor is gone and everything here is a corpse" are opposite facts, and
/// a reader who cannot tell them apart from the top line has been handed the calm, plausible,
/// wrong screen this project exists to remove.
///
/// What these sentences deliberately do **not** carry: whether the monitor before this one exited
/// cleanly, and whether this one is cycling. #28 publishes both inside the fast payload's launch
/// record, already worded for a reader — `previous_exit_why` and `cycling` — and republishes them
/// unchanged on every fast pass, so they are exactly as fresh as the monitor is and must not be
/// given an age of their own. Drawing those two strings verbatim beside a `DEAD` verdict is the
/// obvious next sharpening of this notice; writing different words here for the same fact is not.
fn monitor_notice(presence: &Presence, reading: &PublishedReading) -> String {
    let oldest = |ages: &Ages| match ages.oldest_evidence() {
        Some(age) => format!("{} old", crate::render::format_age(age)),
        None => "of no knowable age".to_string(),
    };

    match presence {
        Presence::Fresh { writer_pid, ages } => format!(
            "FRESH — the monitor at pid {writer_pid} is running at the {} cadence and every tier \
             has published inside its own interval. The figures below are the monitor's, not this \
             display's; the oldest of them is {}. Verdict rests on {}.",
            ages.pace,
            oldest(ages),
            presence.rests_on()
        ),
        Presence::Stale { writer_pid, ages } => format!(
            "STALE — THIS WHOLE SCREEN IS STALE. The monitor at pid {writer_pid} still exists, \
             but {}. \
             Nothing below has been re-verified: the oldest fact on this screen is {}, and every \
             row is as old as its tier says rather than as old as this screen. The monitor may \
             yet catch up — it is alive — but until it publishes again nothing here describes \
             the machine now. Verdict rests on {}.",
            ages.overdue()
                .iter()
                .filter_map(|age| age.overdue.clone())
                .collect::<Vec<String>>()
                .join("; and "),
            oldest(ages),
            presence.rests_on()
        ),
        Presence::Dead { writer_pid, ages } => {
            let age = match ages.as_deref() {
                Some(ages) => oldest(ages),
                None => "of no knowable age".to_string(),
            };
            format!(
                "DEAD — THE MONITOR IS DEAD AND EVERY FIGURE BELOW IS A CORPSE. Pid {writer_pid} \
                 wrote \
                 this file and no longer exists, so nothing here will ever be refreshed, nothing \
                 is being recorded, and nothing will be announced — the oldest fact on this \
                 screen is {age} and it will only get older. This display will not restart it: \
                 the tool observes and never acts, and a monitor holding a lock has to be looked \
                 at by a human. Verdict rests on {}.",
                presence.rests_on()
            )
        }
        // Unreachable: published facts are never `ABSENT`. Said rather than panicked on.
        Presence::Absent(_) => format!(
            "ABSENT — the monitor at pid {} published these figures and this display classified \
             it as ABSENT, which is a fault in this build rather than in the file.",
            reading.writer_pid
        ),
    }
}

/// What has to be said about the cadence the ages were judged against, when anything does.
///
/// Silent in the ordinary case. A monitor keeping a cadence this build knows, past its first
/// round, needs no line here — the per-tier intervals are on screen already.
fn pace_notices(presence: &Presence) -> Vec<String> {
    let Some(ages) = presence.ages() else {
        return Vec::new();
    };
    let mut notices = Vec::new();
    if ages.pace_unknown {
        notices.push(format!(
            "The monitor published its cadence as {:?}, which this build does not know. Every age \
             below was judged against the idle cadence, which is the slower one — an unknown word \
             must not manufacture a staleness warning, and it must not be silent either.",
            ages.pace
        ));
    }
    if ages.warming_up {
        notices.push(
            "The monitor reports that it has not yet completed a round of every tier, so it is \
             warming up: a tier with no age below is pending rather than missing, and the monitor \
             is not announcing anything yet."
                .to_string(),
        );
    }
    notices
}

/// Said on every screen, not only the ones with something wrong.
///
/// A reader who has to remember which binary writes and which draws has been handed the wrong
/// screen.
fn read_only_notice() -> Vec<String> {
    vec!["This display is read-only: it wrote nothing and announced nothing (F26).".to_string()]
}

/// Collect once, read-only, and measure what that cost.
///
/// The cost is measured here rather than by the caller because it is one of the figures on
/// screen (F33), and a display that reports an overhead it did not time would be reporting a
/// guess. [`Role::Display`] is what makes it read-only: no state is written and no channel is
/// asked anything.
pub fn own_collection(
    world: &dyn World,
    now: SystemTime,
    thresholds: &Thresholds,
) -> (Result<Snapshot, String>, Duration) {
    let started = Instant::now();
    let outcome = collect_as(world, now, thresholds, Role::Display);
    let overhead = started.elapsed();
    (outcome.map_err(|error| error.to_string()), overhead)
}

/// What the display does about something that happened while it was open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// Leave, restoring the terminal.
    Quit,
    /// Draw again at the size the terminal is now.
    Redraw,
    /// Nothing. Deliberately the answer to every key but the ones that leave.
    Ignore,
}

/// What a terminal event means to the display.
///
/// Every key but the quits is ignored, and that is a requirement rather than an omission
/// (F55): interactive sorting would place this display inside `htop`'s interaction model, and
/// a reader who feels they are in `htop` reaches for F9 — which N1 forbids this tool from ever
/// honouring. There is nothing here that acts on a session, because there is nothing here that
/// *can*. The single order [`in_cost_order`] draws is the answer to the question the tool
/// exists to ask, so there is nothing for a sort key to buy either.
pub fn command_for(event: &ratatui::crossterm::event::Event) -> Command {
    use ratatui::crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};

    match event {
        // A redraw, not a relayout decision. `ratatui` re-measures the terminal on the next
        // draw, and the drawing code fits the table to whatever height it finds.
        Event::Resize(_, _) => Command::Redraw,
        Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
            KeyCode::Char('q') | KeyCode::Char('Q') | KeyCode::Esc => Command::Quit,
            KeyCode::Char('c') | KeyCode::Char('d')
                if key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                Command::Quit
            }
            _ => Command::Ignore,
        },
        _ => Command::Ignore,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unchanged_modification_time_is_not_a_reason_to_read_the_file() {
        let mut poller = Poller::new();
        let mtime = Stat::At(SystemTime::UNIX_EPOCH + Duration::from_secs(1_787_000_000));

        assert_eq!(poller.observe(mtime.clone()), Poll::Reread);
        assert_eq!(poller.observe(mtime), Poll::Unchanged);
    }

    #[test]
    fn a_file_that_reappears_is_read_again_even_at_a_modification_time_already_seen() {
        // The monitor's writes are atomic renames, so a state file can legitimately be
        // replaced by one stamped earlier. What changed is that there is a file at all.
        let mut poller = Poller::new();
        let mtime = Stat::At(SystemTime::UNIX_EPOCH + Duration::from_secs(1_787_000_000));

        assert_eq!(poller.observe(mtime.clone()), Poll::Reread);
        assert_eq!(poller.observe(Stat::Absent), Poll::Absent);
        assert_eq!(poller.observe(mtime), Poll::Reread);
    }
}
