//! `amon watch`: one loop, all tiers, idling down — and metering itself.
//!
//! One loop, one writer, one place a verdict came from. Every tier is scheduled **inside this
//! process** (PRD decision 8): launchd is demoted to a babysitter that keeps the process alive
//! and schedules nothing. An earlier draft had one collector driven by two schedulers — the
//! display on a fast cadence and launchd on a slow one — which put two processes on the same
//! state file for no gain and split "which run decided this" across two lifetimes.
//!
//! Everything the loop *decides* lives in [`crate::schedule`], [`crate::meter`] and
//! [`crate::tiers`], as functions over data. What lives here is the part that cannot be a
//! function over data: the real clock, the real sleeping, the lock, the threads, and the signal
//! that ends the run. That division is deliberate — a cadence whose behaviour can only be
//! observed by waiting for it is a cadence nobody will test, and on this class of machine
//! timings vary ~2x between runs, so a test that waited would be asserting noise.
//!
//! ## Why the slow tier runs off this thread
//!
//! A full git sweep costs 2.7 s. Run inline, it would hold the loop past the fast tier's whole
//! interval, and the criterion that a slow tier must never delay a fast one would be unmeetable
//! by arithmetic. So the medium and slow passes run on their own thread — at most one of each in
//! flight — and the loop keeps scheduling while they work. The fast pass stays on this thread
//! because it is the cheap one and because it is the only pass allowed to write.
//!
//! One consequence, from `crate::lock`: a child process inherits a copy of the lock's file
//! descriptor, and a copy keeps the lock alive. `std` opens files `CLOEXEC`, so a `git` launched
//! by a worker thread cannot strand the lock — but that is a property of how the file is opened
//! rather than an accident, and a `Command` built with a pre-exec hook would break it.
//!
//! ## What is published
//!
//! `state.json`, per tier, each with its own timestamp (#25's contract, F21). The fast tier
//! publishes the sessions and the monitor's account of itself; the medium tier publishes what
//! the searches found; the slow tier publishes the workspaces, each with the instant `git` was
//! asked about **that** workspace. See [`crate::tiers`] for the shapes.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime};

use crate::collect::Role;
use crate::liveness::Thresholds;
use crate::lock::{LockRefusal, WatchLock};
use crate::meter::{self, Meter, SelfReport};
use crate::schedule::{Budgets, Pace, Schedule, Tier, TIERS};
use crate::starts::{self, Launch};
use crate::state::{Paths, StateStore, TieredState, STATE_FILE};
use crate::tiers::{self, Observed, Pass};
use crate::RealWorld;

/// How long `amon watch` runs before stopping cleanly, in milliseconds.
///
/// Unset means "until something asks it to stop" — a `SIGTERM` from launchd, or a `Ctrl-C` in a
/// terminal — which is what a resident monitor is. It exists as a variable because a bounded run
/// is the only way to have two real processes contend for one lock in a test, and the only way to
/// let a human watch a few whole cycles and read the result off disk without having to signal
/// anything. It absorbed the earlier `ACMON_WATCH_HOLD_MS`, which held the lock around a loop
/// that did not exist; the loop exists now, and this bounds the loop rather than faking it.
pub const RUN_VARIABLE: &str = "ACMON_WATCH_RUN_MS";

/// Set by the signal handler, read by the loop.
///
/// A resident monitor has to be able to stop without being killed: launchd sends `SIGTERM`, and a
/// human in a terminal sends `SIGINT`. Both must release the lock and leave a state file behind
/// that reads as a clean exit, because the successor reports an unreleased lock as a monitor that
/// died — and a monitor that dies every time it is asked to stop is a crash loop that never
/// happened.
static STOP_REQUESTED: AtomicBool = AtomicBool::new(false);

/// The longest the loop ever sleeps in one go, whatever the cadence says.
///
/// The stop flag is only read at the top of the loop, so this is also the longest it can take to
/// honour a `SIGTERM`. That matters: launchd escalates to `SIGKILL` if a job does not go away, and
/// a `SIGKILL`ed monitor leaves an unreleased lock that its successor correctly reports as a
/// monitor that died — so a sluggish shutdown would manufacture a crash report on every ordinary
/// restart. Waking every second costs one `proc_pid_rusage` read, which is microseconds, and it
/// buys a denser set of samples for the duty cycle at the same time.
const MAX_SLEEP: Duration = Duration::from_secs(1);

extern "C" fn note_signal(_signal: libc::c_int) {
    // The only thing done in the handler, deliberately. A lock-free store on a bool is what a
    // signal handler is allowed to do; writing the state file from here would deadlock against
    // whatever the loop was doing when the signal arrived.
    STOP_REQUESTED.store(true, Ordering::SeqCst);
}

/// Ask the kernel to tell this process when it is asked to stop.
///
/// Installed by [`watch`] rather than by `main`, so that a caller cannot start the loop without
/// it and end up with a monitor that can only be killed.
fn listen_for_stop() {
    for signal in [libc::SIGTERM, libc::SIGINT] {
        // SAFETY: `note_signal` does nothing but store to an atomic.
        unsafe { libc::signal(signal, note_signal as *const () as libc::sighandler_t) };
    }
}

/// What `amon watch` was asked to do.
#[derive(Debug, Clone)]
pub struct WatchOptions {
    /// `--foreground`, for debugging. Deliberately changes nothing about the lock: two writers
    /// is two writers regardless of intent (F19).
    pub foreground: bool,
    pub paths: Paths,
    /// How long to run, when the run is bounded. `None` is a resident monitor.
    pub run_for: Option<Duration>,
    pub thresholds: Thresholds,
    pub budgets: Budgets,
}

impl WatchOptions {
    /// Resolved from this machine's environment.
    pub fn from_environment(foreground: bool) -> Result<WatchOptions, String> {
        WatchOptions::from_values(
            foreground,
            Paths::from_environment()?,
            std::env::var(RUN_VARIABLE).ok().as_deref(),
        )
    }

    /// The same, with the environment passed in, so it is testable without mutating a
    /// process-wide variable that every other test in the binary shares.
    pub fn from_values(
        foreground: bool,
        paths: Paths,
        run_for: Option<&str>,
    ) -> Result<WatchOptions, String> {
        Ok(WatchOptions {
            foreground,
            paths,
            run_for: run_from_value(run_for)?,
            thresholds: Thresholds::default(),
            budgets: Budgets::DEFAULT,
        })
    }
}

/// Parse the run window, refusing a value it cannot read rather than falling back to zero.
///
/// A mistyped duration that silently became "do not run at all" would turn a deliberate setting
/// into its opposite without saying so — and would leave a LaunchAgent restarting a monitor that
/// exits immediately, forever.
pub fn run_from_value(value: Option<&str>) -> Result<Option<Duration>, String> {
    let Some(text) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };

    text.parse::<u64>()
        .map(|millis| Some(Duration::from_millis(millis)))
        .map_err(|_| format!("{RUN_VARIABLE} must be a whole number of milliseconds, not {text:?}"))
}

/// Why the monitor's run ended.
#[derive(Debug)]
pub enum WatchStopped {
    /// Another writer holds the lock. The refusal names it.
    LockRefused(LockRefusal),
    /// The lock was held, but the state file could not be published.
    StateUnwritable(String),
    /// The run finished and the lock would not release. Reported rather than swallowed: the
    /// next start would be refused by a lock nobody is using, and the reason must not be a
    /// mystery when that happens.
    LockNotReleased(String),
    /// The loop ran and stopped cleanly. The only arm that exits zero.
    ///
    /// Boxed because it is far larger than every failure arm — a whole self-report — and an enum
    /// sized by its success case would be paid for on every refusal too.
    Finished(Box<Finished>),
}

/// What a completed run came to, so the exit says something more than "no error".
#[derive(Debug, Clone, PartialEq)]
pub struct Finished {
    /// Why the loop stopped.
    pub because: StopReason,
    /// How long it ran.
    pub ran_for: Duration,
    /// How many passes each tier completed.
    pub passes: Vec<(String, u64)>,
    /// The monitor's own account of itself as it stopped.
    pub monitor: SelfReport,
}

/// Why the loop stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    /// A `SIGTERM` or `SIGINT` arrived.
    Signalled,
    /// The run window given in [`RUN_VARIABLE`] elapsed.
    RunWindowElapsed,
}

impl std::fmt::Display for StopReason {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StopReason::Signalled => write!(formatter, "it was asked to stop"),
            StopReason::RunWindowElapsed => {
                write!(formatter, "the run window in {RUN_VARIABLE} elapsed")
            }
        }
    }
}

/// Run the monitor's lifecycle: take the lock, drive every tier, release.
///
/// `notice` receives each thing worth saying as it happens rather than at the end, because a
/// resident monitor that took over a dead predecessor's lock must say so at the moment it
/// happens, not once its run is over.
pub fn watch(options: &WatchOptions, notice: &mut dyn FnMut(&str)) -> WatchStopped {
    let state_dir = options.paths.state_dir().to_path_buf();

    // Before the first write, always. Anything written before this point could be written by
    // two processes at once, which is the whole failure.
    let lock = match WatchLock::acquire(&state_dir) {
        Ok(lock) => lock,
        Err(refusal) => return WatchStopped::LockRefused(refusal),
    };

    if let Some(predecessor) = lock.took_over_from() {
        notice(&format!(
            "took over the state lock from pid {}, which {} — it did not release it, so its \
             run ended without a clean exit",
            predecessor.pid,
            if predecessor.still_running {
                "is still running but does not hold the lock"
            } else {
                "is no longer running"
            }
        ));
    }
    if let Some(reason) = lock.unreadable_record() {
        notice(&format!(
            "the previous lock record could not be read: {reason}. The lock is this process's \
             now, but who held it last is not known"
        ));
    }

    notice(&format!(
        "holding the state lock {} as pid {}{}",
        lock.path().display(),
        lock.holder_pid(),
        if options.foreground {
            " (foreground; the lock applies here exactly as it does under launchd)"
        } else {
            ""
        }
    ));

    let store = StateStore::new(options.paths.clone());

    // Before the first state write, and it has to be: the downtime is the gap to the *previous*
    // monitor's last write, and writing first would collapse every downtime this record exists to
    // publish (F23). The lock's account of its predecessor is the evidence for whether that run
    // ended on purpose, so this is also the earliest moment the question is answerable.
    let launch = record_launch(&store, &lock, notice);

    let mut state = TieredState::new(lock.holder_pid());

    // Published before the first pass, so a reader can see who holds the writer role — and see,
    // from the absence of any tier, that no fact has been collected yet.
    if let Err(reason) = store.write_tiered_state(STATE_FILE, &state) {
        // Release before reporting, so a start that failed for an unrelated reason does not
        // leave a lock behind that reads as a crash.
        if let Err(release) = lock.release() {
            return WatchStopped::StateUnwritable(format!("{reason} (and then {release})"));
        }
        return WatchStopped::StateUnwritable(reason);
    }

    notice(&format!(
        "published {} naming pid {} as the sole writer; no tier has been collected yet",
        state_dir.join(STATE_FILE).display(),
        lock.holder_pid()
    ));

    listen_for_stop();
    STOP_REQUESTED.store(false, Ordering::SeqCst);

    let world = RealWorld::with_state_dir(&state_dir);
    let outcome = run_loop(options, &world, &store, &mut state, &launch, notice);

    if let Err(reason) = lock.release() {
        return WatchStopped::LockNotReleased(reason);
    }

    match outcome {
        Ok(finished) => WatchStopped::Finished(Box::new(finished)),
        Err(reason) => WatchStopped::StateUnwritable(reason),
    }
}

/// Append this launch to `starts.jsonl` and say what it came to, out loud, at the moment it happens.
///
/// A launch that cannot be recorded does not stop the monitor. Refusing to start over a diary would
/// leave the machine unmonitored on the strength of a file — the same argument that makes a stale
/// lock a takeover rather than a refusal — so the failure is said here, published in every fast
/// pass, and the loop runs.
fn record_launch(store: &StateStore, lock: &WatchLock, notice: &mut dyn FnMut(&str)) -> Launch {
    let launch = starts::record(
        store,
        SystemTime::now(),
        lock.holder_pid(),
        lock.took_over_from(),
        lock.unreadable_record(),
    );

    notice(&format!(
        "launch {} recorded in {}: {}. {}",
        match launch.record.launches.value {
            Some(number) => number.to_string(),
            None => "of unknown number".to_string(),
        },
        starts::path(store).display(),
        launch.record.downtime_secs.value.map_or_else(
            || format!(
                "the downtime is not a figure: {}",
                launch
                    .record
                    .downtime_secs
                    .unavailable
                    .clone()
                    .unwrap_or_else(|| "and no reason was given, which is a bug".to_string())
            ),
            |seconds| format!("{seconds:.1}s of downtime since the last state write")
        ),
        launch.record.previous_exit_why,
    ));

    if let Some(cycling) = &launch.record.cycling {
        notice(&format!("this monitor is cycling: {cycling}"));
    }

    if let Some(why) = &launch.not_recorded {
        notice(&format!(
            "this launch could not be appended to {}, so the durable record now has a launch \
             missing from it: {why}",
            starts::path(store).display()
        ));
    }

    launch
}

/// The loop: what is due, what it cost, what to publish, when to sleep.
///
/// Split from [`watch`] so that the lifecycle around it — lock, publish, release — is readable
/// without the scheduling, and so that the scheduling is readable without the lifecycle.
fn run_loop(
    options: &WatchOptions,
    world: &RealWorld,
    store: &StateStore,
    state: &mut TieredState,
    launch: &Launch,
    notice: &mut dyn FnMut(&str),
) -> Result<Finished, String> {
    let began = Instant::now();
    let own_pid = std::process::id() as i32;

    let mut schedule = Schedule::new();
    let mut meter = Meter::default();
    let mut observed = Observed::default();
    let mut sequence: [u64; 3] = [0, 0, 0];
    let mut in_flight: [bool; 3] = [false, false, false];
    let mut stopped_because: Option<StopReason> = None;
    let mut said_cpu_unreadable = false;
    let mut said_still_running: [bool; 3] = [false, false, false];

    notice(&format!(
        "the loop is running: fast every {}s, medium every {}s, slow every {}s while sessions \
         are live, idling to {}s / {}s / {}s when none are (F22). Its own cost is measured as a \
         duty cycle over {}s and published with everything else it measures (F25).",
        crate::schedule::Cadence::ACTIVE.fast.as_secs(),
        crate::schedule::Cadence::ACTIVE.medium.as_secs(),
        crate::schedule::Cadence::ACTIVE.slow.as_secs(),
        crate::schedule::Cadence::IDLE.fast.as_secs(),
        crate::schedule::Cadence::IDLE.medium.as_secs(),
        crate::schedule::Cadence::IDLE.slow.as_secs(),
        meter::WINDOW.as_secs(),
    ));

    // Scoped threads rather than `'static` ones: the workers borrow the real world and the
    // options, and the scope guarantees they are finished before either goes away. A detached
    // worker still holding a `git` child while the lock released would be a second writer's
    // worth of trouble for no gain.
    let finished = std::thread::scope(|scope| -> Result<Finished, String> {
        let (completed, finished_passes) = mpsc::channel::<Pass>();

        loop {
            let at = began.elapsed();

            // The monitor's own cost, sampled every turn of the loop so the trailing window is
            // covered whatever the cadence is doing.
            match meter::own_cpu(world, own_pid) {
                Ok(cpu) => meter.sampled(at, cpu),
                // Said once, not once a second: a monitor that cannot read its own ledger cannot
                // answer for itself, which is the whole of G7 — but a log line every second would
                // bury it. The duty cycle keeps reporting itself as unavailable for as long as it
                // is, so the condition is never only in the log.
                Err(why) => {
                    if !said_cpu_unreadable {
                        said_cpu_unreadable = true;
                        notice(&format!(
                            "the monitor's own CPU could not be read, so it cannot state its own \
                             duty cycle: {why}"
                        ));
                    }
                }
            }

            if STOP_REQUESTED.load(Ordering::SeqCst) {
                stopped_because = Some(StopReason::Signalled);
            } else if options.run_for.is_some_and(|window| at >= window) {
                stopped_because = Some(StopReason::RunWindowElapsed);
            }
            if stopped_because.is_some() {
                break;
            }

            for tier in schedule.due(at) {
                if tier == Tier::Fast {
                    schedule.begun(tier, at);
                    let role = if observed.every_tier_has_run() {
                        Role::Monitor
                    } else {
                        Role::Display
                    };
                    let pass = tiers::run_pass(
                        tier,
                        &observed,
                        world,
                        SystemTime::now(),
                        at,
                        &options.thresholds,
                        role,
                        &options.budgets,
                    );
                    publish(
                        pass,
                        &mut observed,
                        &mut sequence,
                        &mut meter,
                        &mut schedule,
                        state,
                        store,
                        &options.budgets,
                        launch,
                        notice,
                    )?;
                    continue;
                }

                // A pass of this tier is still running. Not started again, and not silently
                // skipped either: an interval that is being missed because the work does not fit
                // inside it is a fact about this machine, and it is the fact that explains why a
                // tier's stamp is older than its cadence.
                if in_flight[index_of(tier)] {
                    // Said once per episode, not once a second. A tier whose pass has genuinely
                    // wedged would otherwise fill the log with the same line until someone killed
                    // the monitor, burying everything else it had to say.
                    if !said_still_running[index_of(tier)] {
                        said_still_running[index_of(tier)] = true;
                        notice(&format!(
                            "the {} tier is still running its previous pass, so this one was not \
                             started; its facts are older than its {}s interval implies",
                            meter::tier_name(tier),
                            schedule.cadence().interval(tier).as_secs()
                        ));
                    }
                    continue;
                }

                schedule.begun(tier, at);
                in_flight[index_of(tier)] = true;
                said_still_running[index_of(tier)] = false;

                let handback = completed.clone();
                let facts = observed.clone();
                let thresholds = options.thresholds;
                let budgets = options.budgets;
                scope.spawn(move || {
                    // Always read-only. The medium and slow tiers observe; the fast tier, which
                    // runs on the loop's own thread, is the only pass that writes state or asks
                    // a notification channel anything. That is what keeps one writer one writer
                    // even though three tiers are in flight.
                    let pass = tiers::run_pass(
                        tier,
                        &facts,
                        world,
                        SystemTime::now(),
                        at,
                        &thresholds,
                        Role::Display,
                        &budgets,
                    );
                    // A send that fails means the loop has stopped and is no longer listening,
                    // which is not an error: the pass's facts are simply not wanted.
                    let _ = handback.send(pass);
                });
            }

            // Sleep until the next tier is due — or until a worker hands back a pass, whichever
            // comes first. `recv_timeout` rather than `sleep` so that a slow pass finishing does
            // not sit unpublished until the next fast interval.
            let wake = schedule.next_wake(began.elapsed()).min(MAX_SLEEP);
            let handed_back = if wake.is_zero() {
                finished_passes.try_recv().ok()
            } else {
                finished_passes.recv_timeout(wake).ok()
            };

            if let Some(pass) = handed_back {
                in_flight[index_of(pass.tier)] = false;
                publish(
                    pass,
                    &mut observed,
                    &mut sequence,
                    &mut meter,
                    &mut schedule,
                    state,
                    store,
                    &options.budgets,
                    launch,
                    notice,
                )?;
            }
        }

        // Anything still in flight is drained before the run is reported, so a pass that was paid
        // for is published rather than thrown away at the door.
        //
        // The loop's own sender is dropped first, so this ends by disconnection once every worker
        // has finished — including a worker that panicked, whose sender is dropped as it unwinds.
        // Counting the in-flight tiers and blocking on that many receives would hang the shutdown
        // on the one pass that never answered.
        drop(completed);
        while let Ok(pass) = finished_passes.recv() {
            in_flight[index_of(pass.tier)] = false;
            publish(
                pass,
                &mut observed,
                &mut sequence,
                &mut meter,
                &mut schedule,
                state,
                store,
                &options.budgets,
                launch,
                notice,
            )?;
        }

        let at = began.elapsed();
        Ok(Finished {
            because: stopped_because.unwrap_or(StopReason::Signalled),
            ran_for: at,
            passes: TIERS
                .iter()
                .map(|tier| (meter::tier_name(*tier).to_string(), meter.count(*tier)))
                .collect(),
            monitor: meter.report(at, schedule.pace().name(), &options.budgets),
        })
    })?;

    notice(&format!(
        "stopping after {:.1}s because {}",
        finished.ran_for.as_secs_f64(),
        finished.because
    ));

    Ok(finished)
}

/// Fold a completed pass into what is known, publish its tier, and adopt the pace it implies.
///
/// One function for all three tiers, deliberately: a tier published by its own code path is a
/// tier whose stamp can drift from the others'.
#[allow(clippy::too_many_arguments)]
fn publish(
    pass: Pass,
    observed: &mut Observed,
    sequence: &mut [u64; 3],
    meter: &mut Meter,
    schedule: &mut Schedule,
    state: &mut TieredState,
    store: &StateStore,
    budgets: &Budgets,
    launch: &Launch,
    notice: &mut dyn FnMut(&str),
) -> Result<(), String> {
    let tier = pass.tier;
    sequence[index_of(tier)] += 1;
    let sequence_number = sequence[index_of(tier)];

    meter.completed(tier, pass.at, pass.took, pass.completion);
    observed.absorb(&pass, sequence_number);

    if let Some(why) = pass.completion.why() {
        notice(&format!(
            "the {} tier overran: {why}",
            meter::tier_name(tier)
        ));
    }

    // The pace is decided by the fast tier alone. It is the tier that observes processes, and
    // idling down on anything else would mean idling down on an inference.
    if tier == Tier::Fast {
        if let Ok(snapshot) = &pass.snapshot {
            let pace = Pace::for_live_sessions(tiers::live_sessions(snapshot));
            if schedule.adopt(pace) {
                notice(&format!(
                    "cadence is now {} ({} live session(s)): fast every {}s",
                    pace.name(),
                    tiers::live_sessions(snapshot),
                    pace.cadence().fast.as_secs()
                ));
            }
        }
    }

    let payload = match tier {
        Tier::Fast => {
            let report = meter.report(pass.at, schedule.pace().name(), budgets);
            serde_json::to_value(tiers::fast_payload(
                &pass,
                sequence_number,
                observed,
                report,
                launch,
            ))
        }
        Tier::Medium => {
            serde_json::to_value(tiers::medium_payload(&pass, sequence_number, observed))
        }
        Tier::Slow => serde_json::to_value(tiers::slow_payload(&pass, sequence_number, observed)),
    }
    .map_err(|error| {
        format!(
            "the {} tier's payload could not be serialised: {error}",
            meter::tier_name(tier)
        )
    })?;

    state.set_tier_data(tier, payload, tiers::stamp_for(&pass, observed));

    // Written whole, atomically, every pass. A reader therefore never sees a file in which one
    // tier has been updated and another has not.
    store.write_tiered_state(STATE_FILE, state).map_err(|why| {
        format!(
            "the {} tier's pass could not be published, so nothing is being recorded: {why}",
            meter::tier_name(tier)
        )
    })?;

    if let Err(error) = &pass.snapshot {
        notice(&format!(
            "the {} tier's collection failed: {error}",
            meter::tier_name(tier)
        ));
    }

    Ok(())
}

fn index_of(tier: Tier) -> usize {
    match tier {
        Tier::Fast => 0,
        Tier::Medium => 1,
        Tier::Slow => 2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_absent_run_window_means_run_until_asked_to_stop() {
        assert_eq!(run_from_value(None).expect("absent is fine"), None);
        assert_eq!(
            run_from_value(Some("   ")).expect("blank reads as absent"),
            None
        );
    }

    #[test]
    fn a_run_window_that_cannot_be_read_is_refused_rather_than_treated_as_zero() {
        let reason = run_from_value(Some("a while")).expect_err("not a number");
        assert!(reason.contains(RUN_VARIABLE), "{reason}");
        assert!(reason.contains("a while"), "{reason}");
    }

    #[test]
    fn a_run_window_is_read_as_milliseconds() {
        assert_eq!(
            run_from_value(Some("1500")).expect("a number"),
            Some(Duration::from_millis(1500))
        );
    }
}
