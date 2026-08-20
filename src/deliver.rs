//! Getting a run's alerts out without paying one timeout per alert.
//!
//! Delivery is still **verified per alert**. This module is a scheduler, not a channel: it
//! is generic over the function that actually delivers one payload, and it returns one
//! outcome per payload in the payloads' own order. Nothing here can turn a request that was
//! merely dispatched into a message that arrived — which is the guarantee ticket #9 exists
//! for, and the one this module was written not to spend.
//!
//! What it does change is the shape of the cost. A steady state of fourteen at-risk
//! workspaces used to mean fourteen sequential requests at up to ten seconds each, and the
//! first run on a machine is the run where *every* notable state fires at once. So a
//! channel's deliveries for one run go out concurrently, under a single total budget, and
//! anything the budget did not reach is reported as [`NotifyOutcome::NotAttempted`] with the
//! reason. Never silently capped: a silent cap in an alerting path reads as "nothing to
//! report", which is the failure this project exists to remove.
//!
//! It touches two operating-system facilities — threads and the monotonic clock — which is
//! why it is worth saying that it describes neither the machine being observed nor a channel.
//! `world` remains the only module that knows how a notification is actually sent.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use crate::world::NotifyOutcome;

/// How long one delivery attempt is allowed to take.
///
/// Also the budget for a whole channel's deliveries in one run — see [`Bounds::standard`].
pub const REQUEST_BUDGET: Duration = Duration::from_secs(10);

/// How many deliveries to one channel are in flight at once.
///
/// The same bound `vcs_facts_batch` uses, for the same reason: enough to stop a run's cost
/// scaling with the number of alerts, few enough that the observer does not become a load
/// source on the machine it is measuring.
pub const CONCURRENCY: usize = 8;

/// What a batch of deliveries is allowed to spend.
///
/// A struct rather than two positional arguments because a transposed pair of numbers here
/// would silently reduce the concurrency to one and hand a channel a worker-count worth of
/// nanoseconds, and both mistakes look like a healthy run that simply announced nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bounds {
    /// How many deliveries may be in flight at once. Clamped to at least one.
    pub workers: usize,
    /// How long the whole batch may take, measured from the first dispatch.
    pub budget: Duration,
}

impl Bounds {
    /// What a real channel gets: [`CONCURRENCY`] in flight, and one request's budget for
    /// the lot.
    ///
    /// The rule the budget encodes: **a channel's whole alerting step for one run costs no
    /// more than one of its requests was already allowed to cost.** A dead endpoint is then
    /// one timeout per run rather than one per alert, and what did not fit is stated.
    pub fn standard() -> Self {
        Bounds {
            workers: CONCURRENCY,
            budget: REQUEST_BUDGET,
        }
    }
}

/// What one channel's batch of deliveries did, and what it cost.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveryReport {
    /// One outcome per payload, in the payloads' own order.
    ///
    /// Always the same length as the payloads it was asked about. An alert the batch never
    /// reached is [`NotifyOutcome::NotAttempted`] rather than a missing entry, because a
    /// caller that has to guess which alerts an answer belongs to cannot keep the promise
    /// that an undelivered alert is re-announced.
    pub outcomes: Vec<NotifyOutcome>,
    /// Wall time the batch took, from a monotonic clock.
    ///
    /// The alerting step is the one part of a collection that waits on something outside the
    /// machine, so its cost has to be attributable rather than buried in the total.
    pub cost: Duration,
}

impl DeliveryReport {
    /// A batch that was never asked to deliver anything.
    pub fn nothing_asked() -> Self {
        DeliveryReport {
            outcomes: Vec::new(),
            cost: Duration::ZERO,
        }
    }

    /// How many of the outcomes are of a given shape, for tallying.
    pub fn count(&self, matching: impl Fn(&NotifyOutcome) -> bool) -> usize {
        self.outcomes.iter().filter(|o| matching(o)).count()
    }
}

/// The outcome of an alert the budget was spent before reaching.
fn budget_spent(budget: Duration) -> NotifyOutcome {
    NotifyOutcome::NotAttempted(format!(
        "this run's alerting budget of {budget:?} for the channel was spent before this alert \
         was dispatched"
    ))
}

/// The outcome of an alert nothing ever reported back about.
///
/// Every slot starts here, so that a delivery thread which dies without answering leaves an
/// alert looking unsent rather than sent. Re-announcing an alert that did arrive is a
/// nuisance; recording one that did not as sent is the defect.
fn never_reported() -> NotifyOutcome {
    NotifyOutcome::NotAttempted(
        "no delivery reported back for this alert, so nothing can be said to have arrived"
            .to_string(),
    )
}

/// Deliver a run's payloads one at a time, under a total budget.
///
/// The default a [`crate::world::World`] gets. Sequential, so it needs nothing of the
/// deliverer but that it be callable — a fixture-driven fake need not be `Sync`. The budget
/// still applies: the guarantee that a run does not spend its alert count times a timeout
/// belongs to every implementation, not only to the concurrent one.
pub fn sequentially<F>(payloads: &[String], budget: Duration, deliver: F) -> DeliveryReport
where
    F: Fn(&str) -> NotifyOutcome,
{
    let started = Instant::now();
    let mut outcomes = Vec::with_capacity(payloads.len());

    for payload in payloads {
        if started.elapsed() >= budget {
            outcomes.push(budget_spent(budget));
        } else {
            outcomes.push(deliver(payload));
        }
    }

    DeliveryReport {
        outcomes,
        cost: started.elapsed(),
    }
}

/// Deliver a run's payloads concurrently, under a total budget.
///
/// Workers pull from a shared cursor rather than being handed a fixed slice each, so one slow
/// delivery holds up only itself. Before every dispatch the budget is re-checked; once it is
/// spent the remaining alerts are drained and reported as not attempted, so the returned
/// outcomes still line up one-for-one with the payloads.
pub fn in_parallel<F>(payloads: &[String], bounds: Bounds, deliver: F) -> DeliveryReport
where
    F: Fn(&str) -> NotifyOutcome + Sync,
{
    let started = Instant::now();
    if payloads.is_empty() {
        return DeliveryReport::nothing_asked();
    }

    let mut outcomes: Vec<NotifyOutcome> = payloads.iter().map(|_| never_reported()).collect();
    let workers = bounds.workers.clamp(1, payloads.len());
    let budget = bounds.budget;
    let next = AtomicUsize::new(0);

    std::thread::scope(|scope| {
        let mut handles = Vec::new();

        for _ in 0..workers {
            let next = &next;
            let deliver = &deliver;
            handles.push(scope.spawn(move || {
                let mut mine: Vec<(usize, NotifyOutcome)> = Vec::new();
                loop {
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    if index >= payloads.len() {
                        break;
                    }
                    // Drained rather than abandoned: every index this worker claims gets an
                    // answer, even after the budget is gone.
                    if started.elapsed() >= budget {
                        mine.push((index, budget_spent(budget)));
                        continue;
                    }
                    mine.push((index, deliver(&payloads[index])));
                }
                mine
            }));
        }

        for handle in handles {
            // A panicking delivery loses that worker's outcomes, including any that did
            // arrive, and those alerts are re-announced next run. The other direction —
            // assuming a thread that died had delivered what it claimed — is the one that
            // loses an alert for good.
            if let Ok(mine) = handle.join() {
                for (index, outcome) in mine {
                    outcomes[index] = outcome;
                }
            }
        }
    });

    DeliveryReport {
        outcomes,
        cost: started.elapsed(),
    }
}
