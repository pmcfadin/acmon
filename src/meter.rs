//! What the monitor costs, measured by the monitor.
//!
//! F25, NF9, S6 and goal G7: a resident process that cannot state its own duty cycle is exactly
//! what this tool would flag on someone else's machine, and a tool whose thesis is measuring
//! overhead has no business hiding its own.
//!
//! The figure that is budgeted is a **duty cycle over a trailing window**, not a per-pass wall
//! clock. A per-pass bound says nothing about the bill — 0.9 s every 2 s is 45% of a core,
//! forever — so what is measured here is CPU consumed between two samples divided by the wall
//! time between them, which is a fraction of one core and directly comparable with NF9's 1%.
//!
//! Two figures, not one, because they answer different questions:
//!
//! - **duty cycle**: own plus reaped-children CPU over the window, as a fraction of one core.
//!   This is the bill. Children are in it because `git` is a child, and a monitor that excluded
//!   its subprocesses would be understating its cost by most of it — the same mistake this tool
//!   exists to catch in other people's measurements (§2.4 of the mechanics document).
//! - **busy fraction**: wall time spent inside a pass over the window. This is what the loop was
//!   *doing*, and it is larger than the duty cycle whenever a pass was waiting on I/O rather than
//!   computing. Publishing only one of them would make a monitor blocked on a slow disk look
//!   either idle or expensive, and it is neither.
//!
//! Neither is ever reported as zero when it could not be measured. A duty cycle of 0% is a
//! monitor that is running and idle — the one thing a reader would most like to know and the one
//! thing an unmeasured figure does not mean.

use std::collections::VecDeque;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::schedule::{Completion, Tier, TIERS};
use crate::world::{Resources, World};

/// The trailing window the duty cycle is averaged over.
///
/// A minute, because that is the window NF9 states the budget over. Anything shorter makes a
/// single slow pass look like a runaway process; anything longer hides one.
pub const WINDOW: Duration = Duration::from_secs(60);

/// A figure that may not have been measurable, with the reason when it was not.
///
/// NF10 in a type: an unmeasurable value is `null` **plus a reason**, never `0` and never an
/// empty string that reads as a healthy negative result. Serialised as both fields so that a
/// reader parsing the state file cannot accidentally treat the absence as a number.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Measured<T> {
    pub value: Option<T>,
    pub unavailable: Option<String>,
}

impl<T> Measured<T> {
    pub fn known(value: T) -> Measured<T> {
        Measured {
            value: Some(value),
            unavailable: None,
        }
    }

    pub fn unavailable(why: impl Into<String>) -> Measured<T> {
        Measured {
            value: None,
            unavailable: Some(why.into()),
        }
    }

    pub fn from(outcome: Result<T, String>) -> Measured<T> {
        match outcome {
            Ok(value) => Measured::known(value),
            Err(why) => Measured::unavailable(why),
        }
    }
}

/// One tier's most recent pass, as the meter row shows it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PassReport {
    /// Which tier this was.
    pub tier: String,
    /// How long the pass took, in milliseconds.
    pub took_ms: u128,
    /// What it was allowed, in milliseconds.
    pub budget_ms: u128,
    /// Whether it overran, and by how much, in the words a reader is shown.
    pub overran: Option<String>,
    /// How long before now the pass started, in milliseconds. An age, not an instant, because
    /// this figure travels inside another tier's payload and has to carry its own.
    pub age_ms: u128,
}

/// Everything the monitor publishes about itself.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SelfReport {
    /// CPU this process and its reaped children have consumed since it started, in
    /// milliseconds. Cumulative, so a reader can compute their own rate over any two samples.
    pub own_cpu_ms: Measured<u128>,
    /// The window the two rates below are averaged over, in seconds.
    pub window_secs: u64,
    /// CPU over the window as a fraction of one core. The figure NF9 budgets.
    pub duty_cycle: Measured<f64>,
    /// Wall time spent inside a pass over the window, as a fraction of the window.
    pub busy_fraction: Measured<f64>,
    /// The budget the duty cycle is judged against, as a fraction of one core.
    pub budget: f64,
    /// Whether the measured duty cycle is inside that budget.
    ///
    /// Published as a verdict as well as a number so that "is this tool within its own budget"
    /// is answerable without arithmetic — and so that a breach is a fact on the screen rather
    /// than something a reader has to notice.
    pub within_budget: Measured<bool>,
    /// Which cadence the loop is running at, and therefore whether it has idled down (F22).
    pub pace: String,
    /// How many passes each tier has completed since the monitor started.
    pub passes: Vec<(String, u64)>,
    /// Each tier's most recent pass. Absent for a tier that has not run yet, which on a fresh
    /// start is a real state and not a zero.
    pub last_pass: Vec<PassReport>,
}

/// The duty cycle NF9 allows: 1% of one core, averaged over a minute, with sessions live.
pub const BUDGET: f64 = 0.01;

/// One CPU sample: the loop's elapsed time, and cumulative CPU at that moment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CpuSample {
    at: Duration,
    cpu: Duration,
}

/// One completed pass.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Pass {
    tier: Tier,
    at: Duration,
    took: Duration,
    completion: Completion,
}

/// The monitor's measurement of itself.
///
/// Holds samples and computes over them; reads no clock. The loop supplies both the elapsed time
/// and the CPU reading, which is what makes every figure below assertable from a test that
/// drives a whole hour of samples in microseconds.
#[derive(Debug, Clone)]
pub struct Meter {
    window: Duration,
    cpu: VecDeque<CpuSample>,
    passes: VecDeque<Pass>,
    /// The most recent pass per tier, kept beyond the window: the last slow pass may well be
    /// older than a minute, and "no slow pass in the last minute" is not the same fact as "the
    /// slow tier has never run".
    last: [Option<Pass>; 3],
    counts: [u64; 3],
}

impl Default for Meter {
    fn default() -> Self {
        Meter::new(WINDOW)
    }
}

impl Meter {
    pub fn new(window: Duration) -> Meter {
        Meter {
            window,
            cpu: VecDeque::new(),
            passes: VecDeque::new(),
            last: [None, None, None],
            counts: [0, 0, 0],
        }
    }

    /// Record a CPU reading taken at elapsed loop time `at`.
    ///
    /// Samples older than one window are dropped, except that the oldest sample still spanning
    /// the window is kept: the rate needs a point at or before the window's start, and dropping
    /// it would make the measurement cover less time than it claims.
    pub fn sampled(&mut self, at: Duration, cpu: Duration) {
        self.cpu.push_back(CpuSample { at, cpu });
        let horizon = at.saturating_sub(self.window);
        while self.cpu.len() > 2 && self.cpu[1].at <= horizon {
            self.cpu.pop_front();
        }
    }

    /// Record a completed pass.
    pub fn completed(&mut self, tier: Tier, at: Duration, took: Duration, completion: Completion) {
        let pass = Pass {
            tier,
            at,
            took,
            completion,
        };
        self.last[index_of(tier)] = Some(pass.clone());
        self.counts[index_of(tier)] += 1;
        self.passes.push_back(pass);
        let horizon = at.saturating_sub(self.window);
        while self
            .passes
            .front()
            .map(|pass| pass.at < horizon)
            .unwrap_or(false)
        {
            self.passes.pop_front();
        }
    }

    /// How many passes this tier has completed.
    pub fn count(&self, tier: Tier) -> u64 {
        self.counts[index_of(tier)]
    }

    /// CPU over the trailing window as a fraction of one core, or why that is not known yet.
    ///
    /// Needs two samples spanning a measurable interval. Reported as unavailable rather than as
    /// zero until it has them, because a fresh monitor with one sample has not established that
    /// it is cheap — it has established nothing.
    pub fn duty_cycle(&self) -> Result<f64, String> {
        let (first, last) = match (self.cpu.front(), self.cpu.back()) {
            (Some(first), Some(last)) if self.cpu.len() >= 2 => (first, last),
            _ => {
                return Err(format!(
                    "only {} CPU sample(s) have been taken, and a rate needs two",
                    self.cpu.len()
                ))
            }
        };

        let span = last.at.saturating_sub(first.at);
        if span.is_zero() {
            return Err(
                "the two CPU samples were taken at the same instant, so no rate can be \
                 computed from them"
                    .to_string(),
            );
        }

        let spent = last.cpu.saturating_sub(first.cpu);
        Ok(spent.as_secs_f64() / span.as_secs_f64())
    }

    /// The wall time the loop spent inside a pass over the window, as a fraction of it.
    pub fn busy_fraction(&self, at: Duration) -> Result<f64, String> {
        let span = at.min(self.window);
        if span.is_zero() {
            return Err(
                "the loop has not been running long enough to have a busy fraction \
                        yet"
                .to_string(),
            );
        }
        let busy: Duration = self.passes.iter().map(|pass| pass.took).sum();
        Ok(busy.as_secs_f64() / span.as_secs_f64())
    }

    /// The whole self-report, ready to publish.
    pub fn report(
        &self,
        at: Duration,
        pace: &str,
        budgets: &crate::schedule::Budgets,
    ) -> SelfReport {
        let duty = self.duty_cycle();
        SelfReport {
            own_cpu_ms: Measured::from(
                self.cpu
                    .back()
                    .map(|sample| sample.cpu.as_millis())
                    .ok_or_else(|| "no CPU reading has been taken yet".to_string()),
            ),
            window_secs: self.window.as_secs(),
            duty_cycle: Measured::from(duty.clone()),
            busy_fraction: Measured::from(self.busy_fraction(at)),
            budget: BUDGET,
            within_budget: Measured::from(duty.map(|duty| duty <= BUDGET)),
            pace: pace.to_string(),
            passes: TIERS
                .iter()
                .map(|tier| (tier_name(*tier).to_string(), self.count(*tier)))
                .collect(),
            last_pass: TIERS
                .iter()
                .filter_map(|tier| self.last[index_of(*tier)].as_ref())
                .map(|pass| PassReport {
                    tier: tier_name(pass.tier).to_string(),
                    took_ms: pass.took.as_millis(),
                    budget_ms: budgets.budget(pass.tier).as_millis(),
                    overran: pass.completion.why(),
                    age_ms: at.saturating_sub(pass.at).as_millis(),
                })
                .collect(),
        }
    }
}

/// The word a tier is published under.
///
/// Lower case and stable, because it goes on disk. The `Tier` enum's own serialisation is #25's
/// contract for the state file's tier *keys*; this is the name used inside a payload, and the two
/// are deliberately allowed to differ rather than one being changed to suit the other.
pub fn tier_name(tier: Tier) -> &'static str {
    match tier {
        Tier::Fast => "fast",
        Tier::Medium => "medium",
        Tier::Slow => "slow",
    }
}

fn index_of(tier: Tier) -> usize {
    match tier {
        Tier::Fast => 0,
        Tier::Medium => 1,
        Tier::Slow => 2,
    }
}

/// This process's own CPU, children included, or why it could not be read.
///
/// Asked of the same [`World`] and the same reader every other process's ledger is asked of, on
/// purpose: the monitor is measured by exactly the mechanism it measures agents with, including
/// the mach-tick conversion. A separate code path here could be wrong in a way that made the
/// monitor look cheap while every other figure on the screen was right.
pub fn own_cpu(world: &dyn World, pid: i32) -> Result<Duration, String> {
    let resources: Resources = world
        .resources(pid)
        .map_err(|why| format!("the monitor's own resource ledger could not be read: {why}"))?;

    let own = resources
        .own_cpu
        .as_ref()
        .map_err(|why| format!("the monitor's own CPU was not reported: {why}"))?;

    // Children matter more than own CPU here: `git` and `ps` are children, and a monitor that
    // reported only its own would be understating its cost by most of it. A reader that cannot
    // supply the children's figure is reported rather than treated as having supplied zero.
    let children = resources
        .children_cpu
        .as_ref()
        .map_err(|why| format!("the monitor's children's CPU was not reported: {why}"))?;

    Ok(*own + *children)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_cpu_sample_yields_no_duty_cycle_rather_than_a_zero_one() {
        let mut meter = Meter::default();
        meter.sampled(Duration::ZERO, Duration::ZERO);

        let why = meter.duty_cycle().expect_err("a rate needs two samples");
        assert!(why.contains("two"), "{why}");
    }

    #[test]
    fn the_duty_cycle_is_cpu_over_the_span_of_the_samples_it_was_measured_across() {
        // A ratio, asserted exactly, because both inputs are given rather than measured: 600 ms
        // of CPU across 60 s of wall time is 1% of one core, and that arithmetic is not
        // machine-dependent.
        let mut meter = Meter::default();
        meter.sampled(Duration::ZERO, Duration::ZERO);
        meter.sampled(Duration::from_secs(60), Duration::from_millis(600));

        let duty = meter.duty_cycle().expect("two samples");
        assert!(
            (duty - 0.01).abs() < 1e-9,
            "600 ms of CPU in 60 s is 1% of one core; got {duty}"
        );
    }
}
