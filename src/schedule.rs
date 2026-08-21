//! When each tier is due, how much it may cost, and when the loop idles down.
//!
//! Every decision the collection loop makes lives here as a function over data, and none of
//! them reads a clock or sleeps. Time arrives as a parameter — a [`Duration`] since the loop
//! began — so a cadence can be driven through an hour of decisions instantly. A scheduler whose
//! behaviour can only be observed by waiting for it is a scheduler nobody will test, and the
//! project rule against asserting absolute timings (see `AGENTS.md`) makes waiting-based tests
//! useless here anyway: what reproduces is the *decision*, not the millisecond it was taken at.
//!
//! Three tiers, from PRD NF6, assigned by **measured** cost rather than by guesswork:
//!
//! - [`Tier::Fast`] — near-free signals: the process enumeration and the per-process resource
//!   ledger, read through `libproc` with no subprocess (NF5).
//! - [`Tier::Medium`] — the filesystem searches: resolving a recorded transcript namespace onto
//!   a directory that still exists, and sweeping the neighbourhoods repositories live in.
//! - [`Tier::Slow`] — `git` and the Codex transcript index. A full 34-workspace git sweep costs
//!   2.7 s, which is why it is here and why it is read a slice at a time.
//!
//! The budget is a **duty cycle over a window**, not a per-pass wall clock (NF9). A per-pass
//! bound says nothing about the bill: 0.9 s every 2 s is 45% of a core, forever. Per-tier
//! budgets exist too, but only to bound one pass so it cannot starve the others — the figure
//! that has to stay under 1% of a core is the one [`crate::meter`] measures.

use std::time::Duration;

pub use crate::state::Tier;

/// The three tiers in the order a loop should run them: cheapest first.
///
/// Fast first is not cosmetic. When several tiers come due in the same pass, running the cheap
/// one first means the expensive one's cost is never paid before the figures a reader looks at
/// most often have been refreshed.
pub const TIERS: [Tier; 3] = [Tier::Fast, Tier::Medium, Tier::Slow];

/// How often each tier runs.
///
/// Two of these exist — [`Cadence::ACTIVE`] and [`Cadence::IDLE`] — and the loop switches
/// between them on the presence of live sessions (F22). Most of the day there is nothing to
/// poll, and a monitor that polled at working-hours cadence through the night would spend the
/// overwhelming majority of its budget establishing that the machine is asleep.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cadence {
    pub fast: Duration,
    pub medium: Duration,
    pub slow: Duration,
}

impl Cadence {
    /// The cadence with at least one live session on the machine.
    ///
    /// **Derived from measurement, not from what would feel responsive.** These are the numbers
    /// that make NF9's 1%-of-a-core-over-a-minute hold in the *worst* minute rather than on
    /// average, which is a stronger and less flattering reading of it. Measured on the machine
    /// this was built on, at load ~5, release build:
    ///
    /// | Tier | Measured pass | Interval | Worst minute |
    /// | --- | --- | --- | --- |
    /// | fast | ~49 ms | 10 s | 6 passes = 294 ms = 0.49% |
    /// | medium | ~207 ms | 60 s | 1 pass = 207 ms = 0.35% |
    /// | slow | ≤300 ms, by budget | 120 s | 1 pass = 300 ms = 0.50% |
    ///
    /// That worst case sums to under 1.4%, which is not the whole story and is why the figure is
    /// **measured rather than asserted** (NF9): a pass's wall time is not all CPU, the three
    /// worst cases do not coincide, and a slow pass sized by its budget usually costs far less
    /// than it. Measured over a three-minute run on this machine at load ~5, the published duty
    /// cycle was **0.37–0.47% of one core**. Every interval here is at least 60 s except the fast
    /// one, so no window of a minute can contain two medium or two slow passes, which is what
    /// makes even the pessimistic arithmetic possible.
    ///
    /// The consequence to be honest about: a session's row is up to 10 s old and the at-risk panel
    /// is up to 2 minutes old per workspace, published as such. That is the trade the budget
    /// forces, and it is why every figure on disk carries its own age.
    pub const ACTIVE: Cadence = Cadence {
        fast: Duration::from_secs(10),
        medium: Duration::from_secs(60),
        slow: Duration::from_secs(120),
    };

    /// The cadence with no live session anywhere.
    ///
    /// Still non-zero, deliberately. The loop has to keep looking, or the first session of the
    /// morning would go unnoticed until something else woke the monitor up — and it has to keep
    /// re-reading the workspaces, because a workspace stranded overnight is *more* at risk, not
    /// less.
    ///
    /// Six times slower on every tier, which is around 0.16% of a core: not zero, and not
    /// pretending to be. Most of the day is spent here.
    pub const IDLE: Cadence = Cadence {
        fast: Duration::from_secs(60),
        medium: Duration::from_secs(360),
        slow: Duration::from_secs(720),
    };

    /// This tier's interval.
    pub fn interval(&self, tier: Tier) -> Duration {
        match tier {
            Tier::Fast => self.fast,
            Tier::Medium => self.medium,
            Tier::Slow => self.slow,
        }
    }
}

/// Whether the machine has agents on it, and therefore how hard the loop works.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pace {
    /// At least one session's process was observed resident. Poll at [`Cadence::ACTIVE`].
    Active,
    /// No session's process was observed. Poll at [`Cadence::IDLE`].
    Idle,
}

impl Pace {
    /// The pace for a pass that observed `live` sessions with a resident process.
    ///
    /// Counted from **observed processes**, never from a liveness verdict. A `WAITING` verdict is
    /// inferred from silence, and idling down on an inference would let one misread transcript
    /// put the monitor to sleep with an agent still running.
    pub fn for_live_sessions(live: usize) -> Pace {
        if live == 0 {
            Pace::Idle
        } else {
            Pace::Active
        }
    }

    pub fn cadence(&self) -> Cadence {
        match self {
            Pace::Active => Cadence::ACTIVE,
            Pace::Idle => Cadence::IDLE,
        }
    }

    /// The word this pace is published under.
    pub fn name(&self) -> &'static str {
        match self {
            Pace::Active => "active",
            Pace::Idle => "idle",
        }
    }
}

/// What one tier's pass may cost before the loop says so.
///
/// A bound on one pass, not the bill. Its job is to stop one tier starving the others and to
/// make an over-long pass a **reported fact** rather than a silent stretch of the cadence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Budgets {
    pub fast: Duration,
    pub medium: Duration,
    pub slow: Duration,
}

impl Budgets {
    /// The budgets the monitor runs with.
    ///
    /// Each is a few times the measured cost of its pass, so an ordinary pass does not report an
    /// overrun and a pass that has genuinely gone wrong does. The fast tier's is the tight one,
    /// because it is the tier that runs on the loop's own thread: anything it spends is time the
    /// loop is not scheduling.
    ///
    /// The slow tier's is load-bearing rather than advisory — it is what sizes the slice of
    /// workspaces one pass reads (see [`resized_slice`]), and therefore what keeps the git sweep
    /// inside the duty cycle. 300 ms against a 120 s interval is 0.5% of one core.
    /// Each is around three times its tier's measured pass, because measurements on this class of
    /// machine vary by roughly 2x between runs (see the rule in `AGENTS.md`). A budget set at the
    /// measured cost would report an overrun on any busy minute, and an overrun that fires in the
    /// ordinary case is one a reader learns to ignore.
    pub const DEFAULT: Budgets = Budgets {
        fast: Duration::from_millis(150),
        medium: Duration::from_millis(800),
        slow: Duration::from_millis(300),
    };

    pub fn budget(&self, tier: Tier) -> Duration {
        match tier {
            Tier::Fast => self.fast,
            Tier::Medium => self.medium,
            Tier::Slow => self.slow,
        }
    }
}

/// Whether a pass finished inside what it was allowed.
///
/// Reported rather than enforced. The tool observes; it does not kill its own work half-done and
/// publish the half — a partial git sweep presented as a whole one is the calm, plausible, wrong
/// answer this project exists to remove. What it does instead is say the pass overran, by how
/// much, and let the slice shrink next time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Completion {
    WithinBudget,
    Overran { budget: Duration, took: Duration },
}

impl Completion {
    /// Judge a finished pass.
    pub fn of(took: Duration, budget: Duration) -> Completion {
        if took > budget {
            Completion::Overran { budget, took }
        } else {
            Completion::WithinBudget
        }
    }

    pub fn overran(&self) -> bool {
        matches!(self, Completion::Overran { .. })
    }

    /// What to say about an overrun, in the words the payload publishes.
    pub fn why(&self) -> Option<String> {
        match self {
            Completion::WithinBudget => None,
            Completion::Overran { budget, took } => Some(format!(
                "the pass took {} ms against a {} ms budget, so the figures it published are \
                 older than the cadence implies",
                took.as_millis(),
                budget.as_millis()
            )),
        }
    }
}

/// Which tiers are due, and when the loop should next wake.
///
/// Holds only the loop's own elapsed time per tier — never an [`std::time::Instant`], which
/// cannot be constructed at an arbitrary value and would make every test here a test that waits.
#[derive(Debug, Clone)]
pub struct Schedule {
    pace: Pace,
    /// When each tier's most recent pass **started**, as elapsed loop time.
    ///
    /// The start, not the finish: a tier that takes 2 s and runs every 60 s runs every 60 s, not
    /// every 62 s, and measuring the interval from the finish would let a slow pass silently
    /// stretch its own cadence.
    started: [Option<Duration>; 3],
}

impl Default for Schedule {
    fn default() -> Self {
        Schedule::new()
    }
}

impl Schedule {
    /// A schedule with nothing yet run, so every tier is due at once.
    ///
    /// [`Pace::Active`] to begin with, deliberately: the first fast pass is what discovers
    /// whether there is anything to watch, and starting idle would delay that decision by the
    /// idle interval on every start.
    pub fn new() -> Schedule {
        Schedule {
            pace: Pace::Active,
            started: [None, None, None],
        }
    }

    pub fn pace(&self) -> Pace {
        self.pace
    }

    pub fn cadence(&self) -> Cadence {
        self.pace.cadence()
    }

    /// Adopt a pace, reporting whether it changed.
    ///
    /// Returns `true` on a change so the loop can say so once rather than every pass. Rising is
    /// immediate and needs no special case: the active intervals are shorter, so a tier whose
    /// last pass was longer ago than the new interval becomes due on the very next look — which
    /// is what "rises on the first detected session" means in practice (F22).
    pub fn adopt(&mut self, pace: Pace) -> bool {
        let changed = self.pace != pace;
        self.pace = pace;
        changed
    }

    /// When this tier's last pass started, as elapsed loop time.
    pub fn started_at(&self, tier: Tier) -> Option<Duration> {
        self.started[index_of(tier)]
    }

    /// Record that a tier's pass has begun.
    pub fn begun(&mut self, tier: Tier, at: Duration) {
        self.started[index_of(tier)] = Some(at);
    }

    /// Whether one tier is due at `at`.
    pub fn is_due(&self, tier: Tier, at: Duration) -> bool {
        match self.started_at(tier) {
            // Never run. Due, whatever the cadence: a monitor that had collected nothing would
            // otherwise publish nothing for a whole interval and read as a quiet machine.
            None => true,
            Some(last) => at.saturating_sub(last) >= self.cadence().interval(tier),
        }
    }

    /// Every tier due at `at`, cheapest first.
    pub fn due(&self, at: Duration) -> Vec<Tier> {
        TIERS
            .iter()
            .copied()
            .filter(|tier| self.is_due(*tier, at))
            .collect()
    }

    /// How long the loop may sleep before the next tier comes due.
    ///
    /// `Duration::ZERO` when something is already due. Never longer than the fast interval, so a
    /// change of pace is noticed within one fast interval rather than at the end of a ten-minute
    /// slow one.
    pub fn next_wake(&self, at: Duration) -> Duration {
        let cadence = self.cadence();
        TIERS
            .iter()
            .map(|tier| match self.started_at(*tier) {
                None => Duration::ZERO,
                Some(last) => cadence
                    .interval(*tier)
                    .saturating_sub(at.saturating_sub(last)),
            })
            .min()
            .unwrap_or(cadence.fast)
            .min(cadence.fast)
    }
}

fn index_of(tier: Tier) -> usize {
    match tier {
        Tier::Fast => 0,
        Tier::Medium => 1,
        Tier::Slow => 2,
    }
}

/// The largest number of repositories one slow pass may ask `git` about.
///
/// The budget is the real limit; this is only a bound on how far the adaptive sizing may run ahead
/// of it. Sixty-four `git status` calls at the measured median of 59 ms is 3.8 s, which is far
/// outside any pass's budget — so this bound is never the thing that binds on a healthy machine,
/// and exists so that a machine whose repositories are all trivially small cannot ramp into a pass
/// large enough to matter before the next measurement halves it.
pub const MAX_SLICE: usize = 64;

/// How many workspaces the next slow pass should read, given what the last one cost.
///
/// Adaptive rather than a constant, because the per-workspace cost of `git status` is not a
/// property of this tool: it depends on the size of the repository, on whether the filesystem
/// cache is warm, and on what else is running. A fixed slice tuned on an idle machine is a slice
/// that blows the budget on a busy one.
///
/// Three regimes:
///
/// - **Over budget**: halve. The direction that has to be decisive, because the budget is what
///   keeps the git sweep inside the duty cycle.
/// - **Within a factor of two of the budget**: leave it alone. Hunting around a target that moves
///   with the machine's load would make the slice oscillate for no gain.
/// - **Under half the budget**: a quarter more.
///
/// Gentle growth is deliberate and was learned by measuring. A rule that leapt when a pass came in
/// far under budget looked obviously right — most passes cost a few milliseconds against 300 —
/// and produced a pass of 1791 ms, because the cheap passes it extrapolated from had been reading
/// candidates that were not repositories at all. The slice now counts only the reads that cost
/// (see [`crate::tiers::SlowFacts`]), which makes the extrapolation sound; growing gently on top of
/// that is what keeps it sound when one repository is fifty times the size of the last.
pub fn resized_slice(previous: usize, took: Duration, budget: Duration) -> usize {
    let previous = previous.max(1);
    if took > budget {
        return (previous / 2).max(1);
    }
    if took * 2 > budget {
        return previous;
    }
    (previous + (previous / 4).max(1)).min(MAX_SLICE)
}

/// Which workspaces the next slow pass should read: the stalest first.
///
/// `age` gives, for each candidate, how long ago it was last read, or `None` if it has never
/// been read. Never-read candidates come first — a workspace that has just appeared is the one
/// nothing is known about — and the rest follow oldest-first, so no workspace can be starved by
/// a set larger than one slice.
///
/// Returns indices into `age`, so the caller does not have to match paths up again.
pub fn stalest_first(age: &[Option<Duration>], size: usize) -> Vec<usize> {
    let mut order: Vec<usize> = (0..age.len()).collect();
    // Sorted by (never-read before read, oldest first), and by index within a tie so that the
    // choice is deterministic — an arbitrary tie-break would make a slice's contents depend on
    // the sort's internals, and a test of coverage would be a test of `sort_by`.
    order.sort_by(|left, right| match (age[*left], age[*right]) {
        (None, None) => left.cmp(right),
        (None, Some(_)) => std::cmp::Ordering::Less,
        (Some(_), None) => std::cmp::Ordering::Greater,
        // Oldest first, so the larger age sorts earlier.
        (Some(left_age), Some(right_age)) => right_age.cmp(&left_age).then_with(|| left.cmp(right)),
    });
    order.truncate(size);
    order
}

/// The slice of a candidate list one pass covers, as a coverage report.
///
/// Published rather than kept: "12 of 70 workspaces were read this pass" is the fact that stops
/// a reader believing the at-risk panel was refreshed in full.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Coverage {
    /// How many candidates there were.
    pub total: usize,
    /// How many this pass actually read.
    pub read: usize,
    /// How many have never been read at all, after this pass.
    pub never_read: usize,
}

impl Coverage {
    /// Whether every candidate has been read at least once.
    ///
    /// The condition the loop waits for before it will announce anything about a workspace: a
    /// workspace nobody has looked at is not evidence of anything, and alerting on it would make
    /// every restart an alert storm.
    pub fn complete(&self) -> bool {
        self.never_read == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tier_that_has_never_run_is_due_however_long_the_loop_has_been_up() {
        let schedule = Schedule::new();
        for tier in TIERS {
            assert!(
                schedule.is_due(tier, Duration::ZERO),
                "{tier:?} has never run, so it is due at once"
            );
        }
    }

    #[test]
    fn the_stalest_workspace_is_read_before_one_that_was_read_recently() {
        // Two read, one never. The never-read one first, then the older of the two.
        let age = [
            Some(Duration::from_secs(10)),
            None,
            Some(Duration::from_secs(600)),
        ];
        assert_eq!(stalest_first(&age, 3), vec![1, 2, 0]);
    }

    #[test]
    fn a_slice_that_overran_its_budget_is_halved_and_one_well_inside_it_grows() {
        let budget = Duration::from_millis(1_000);
        assert_eq!(resized_slice(16, Duration::from_millis(1_400), budget), 8);
        // Inside the budget but not by half: leave it alone rather than hunting.
        assert_eq!(resized_slice(16, Duration::from_millis(800), budget), 16);
        // Under half the budget: creep up.
        assert_eq!(resized_slice(16, Duration::from_millis(250), budget), 20);
        assert_eq!(
            resized_slice(MAX_SLICE, Duration::ZERO, budget),
            MAX_SLICE,
            "and never past the bound, however cheap the pass looked"
        );
        // Never zero, whatever it cost. A slice of nothing never reads anything again.
        assert_eq!(resized_slice(1, Duration::from_secs(30), budget), 1);
    }
}
