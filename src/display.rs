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
//! Two things it deliberately does not do:
//!
//! One thing it deliberately does not do: it does not classify freshness. `FRESH`/`STALE`/
//! `DEAD`/`ABSENT` and the age of each tier's evidence are #30, which is larger and
//! higher-stakes than anything here. Every instant on this screen is stated as the instant it
//! was read at, and nothing here turns one into a verdict about the monitor.

use std::path::Path;
use std::time::{Duration, Instant, SystemTime};

use crate::collect::{collect_as, Role, Session};
use crate::liveness::Thresholds;
use crate::state::{StateStore, STATE_FILE};
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
/// No `Published` arm yet, and not by omission: the payload inside a tier is #27's schema and
/// the age of each tier is #30's. What this ticket owes is that every *other* outcome is
/// distinguishable, because each of them is a way for a display to show a calm, plausible,
/// wrong screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StateReading {
    /// No state file. Nothing has ever published here, so nothing is being recorded or
    /// alerted, and the display has to collect for itself (F28).
    Absent,
    /// A state file naming a writer, carrying nothing this display can draw from.
    ///
    /// Two situations, one outcome, and `why` says which: the monitor holds the writer role
    /// and has collected no tier yet, or it published tiers whose contents no reader here
    /// understands. Both mean the figures on screen cannot come from the monitor.
    Unrenderable { writer_pid: u32, why: String },
    /// There is a file and it cannot be believed — torn, truncated, of an unknown version, or
    /// unreadable. Nothing is taken from it, at all.
    ///
    /// The alternative, taking whatever parsed, is the defect this whole project is against:
    /// half a state file renders as a shorter session list and a shorter at-risk panel, which
    /// is precisely the shape of a healthy screen.
    Unusable(String),
}

/// The tier count a state file has to carry before it says anything about the machine.
///
/// Named so that the message below and the check that produces it cannot disagree.
const NO_TIERS: usize = 0;

/// Read the state file, in whatever condition it is in.
pub fn read_state_file(store: &StateStore) -> StateReading {
    match store.read_tiered_state(STATE_FILE) {
        Ok(None) => StateReading::Absent,
        Ok(Some(state)) => {
            let tiers = state.tier_count();
            StateReading::Unrenderable {
                writer_pid: state.writer_pid(),
                why: if tiers == NO_TIERS {
                    "it holds the writer role and has completed no pass yet, so it has published \
                     no tier to draw from"
                        .to_string()
                } else {
                    format!(
                        "it published {tiers} tier(s) this display has no reader for — the tier \
                         payloads are `acmon::tiers` and reading them is #30"
                    )
                },
            }
        }
        Err(why) => StateReading::Unusable(why),
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
    /// A monitor published this figure, and the display cannot read it yet. Names the work
    /// that will.
    ///
    /// Not "the monitor published nothing": since #27 the monitor meters itself on every pass
    /// and publishes it. What is missing is on this side of the file — the tier payloads
    /// (`acmon::tiers`) need a reader here. Saying it the other way round sent a reader to the
    /// wrong ticket, and blamed a figure's absence on the one component that had produced it.
    NotRead { tracked_as: &'static str },
    /// A monitor published something that could not be believed, so nothing was taken from it.
    Unreadable,
}

impl std::fmt::Display for Unmetered {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Unmetered::NoMonitor => write!(
                formatter,
                "no monitor is recording, so there is none to meter"
            ),
            Unmetered::NotRead { tracked_as } => write!(
                formatter,
                "the monitor published it, but this display cannot read its tier payloads yet \
                 ({tracked_as})"
            ),
            Unmetered::Unreadable => write!(
                formatter,
                "the state file could not be believed, so nothing was taken from it"
            ),
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
    /// current — a gauge is the easiest thing on a screen to read as live. Stated as an
    /// instant, not turned into an age or a `STALE`/`DEAD` verdict: that is #30's, and doing
    /// half of it here would put two different freshness rules on one screen.
    pub taken_at: SystemTime,
}

impl Meters {
    /// The meters for figures the display collected itself.
    ///
    /// Its own overhead it measured, so that is a number. The monitor's duty cycle it cannot
    /// have, and which reason applies depends on what the state file turned out to be.
    pub fn for_own_collection(
        reading: &StateReading,
        overhead: Duration,
        taken_at: SystemTime,
    ) -> Meters {
        Meters {
            overhead: Ok(overhead),
            duty_cycle: Err(match reading {
                StateReading::Absent => Unmetered::NoMonitor,
                StateReading::Unrenderable { .. } => Unmetered::NotRead { tracked_as: "#30" },
                StateReading::Unusable(_) => Unmetered::Unreadable,
            }),
            taken_at,
        }
    }
}

/// What the display draws: the figures, where they came from, and what it cost to have them.
///
/// Assembled away from the terminal so the whole screen is a value a test can hold. The
/// alternative — deciding these things inside the drawing code — is what made the pre-split
/// renderer testable only through a buffer of characters.
#[derive(Debug, Clone, PartialEq)]
pub struct Screen {
    /// What has to be said before any figure is read: where these figures came from, and
    /// anything wrong with the state file.
    ///
    /// Never empty. A screen that says nothing about its own provenance is a screen a reader
    /// will assume came from a running monitor.
    pub notices: Vec<String>,
    /// The figures, or the reason there are none.
    pub facts: Result<Snapshot, String>,
    pub meters: Meters,
}

impl Screen {
    /// The screen for a display that had to collect for itself (F28).
    ///
    /// `taken_at` is stated, not turned into an age: how *old* a fact is, and what that makes
    /// of the monitor, is #30. What this ticket owes is that the screen never implies these
    /// figures came from a monitor when they did not.
    pub fn from_own_collection(
        reading: &StateReading,
        facts: Result<Snapshot, String>,
        overhead: Duration,
        taken_at: SystemTime,
    ) -> Screen {
        let mut notices = vec![match reading {
            StateReading::Absent => format!(
                "NO MONITOR IS RECORDING. There is no state file, so nothing is being recorded \
                 and nothing will be announced — these figures are one read this display took \
                 for itself at {}.",
                crate::isotime::iso8601_from_unix_seconds(crate::isotime::unix_seconds(taken_at))
            ),
            StateReading::Unrenderable { writer_pid, why } => format!(
                "The monitor at pid {writer_pid} is not being drawn: {why}. These figures are \
                 one read this display took for itself at {}.",
                crate::isotime::iso8601_from_unix_seconds(crate::isotime::unix_seconds(taken_at))
            ),
            StateReading::Unusable(reason) => format!(
                "THE STATE FILE COULD NOT BE BELIEVED: {reason}. Nothing was taken from it — \
                 not one row — because half of a state file renders as a short session list \
                 and a short at-risk panel, which is the shape of a healthy screen. These \
                 figures are one read this display took for itself at {}.",
                crate::isotime::iso8601_from_unix_seconds(crate::isotime::unix_seconds(taken_at))
            ),
        }];

        // Said on every screen, not only the ones with something wrong. A reader who has to
        // remember which binary writes and which draws has been handed the wrong screen.
        notices.push(
            "This display is read-only: it wrote nothing and announced nothing (F26).".to_string(),
        );

        Screen {
            notices,
            facts,
            meters: Meters::for_own_collection(reading, overhead, taken_at),
        }
    }
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
