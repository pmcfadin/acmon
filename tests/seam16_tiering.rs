//! Seam 16 — one loop, all tiers, idling down, and metering itself.
//!
//! The failure this seam exists to prevent: a monitor that becomes the tax it measures. Before
//! this seam, one collection did every observation at once and cost ~2.5 s — against a fast tier
//! budgeted at one second — so any cadence useful enough to watch a session with would have spent
//! a permanent double-digit percentage of a core establishing that nothing had changed. A tool
//! whose thesis is that background overhead goes unnoticed cannot ship that.
//!
//! Everything here is asserted as a **ratio or an invariant**, never as an absolute timing.
//! Measurements on this class of machine vary by roughly 2x between runs (see `AGENTS.md`), so
//! "the fast tier took 900 ms" fails for reasons that have nothing to do with correctness. What
//! reproduces is that the fast tier runs at least as often as the slow one, that idling down
//! reduces the work a window costs, that a tier's stamp advances only when that tier ran, and
//! that a figure which could not be measured is published as absent with a reason rather than as
//! a zero.
//!
//! The loop's decisions are therefore driven directly, as functions over data, with time passed
//! in as a parameter. Only the last two tests spawn the real binary, and they assert what a run
//! *published* rather than how long anything took.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use acmon::collect::Role;
use acmon::liveness::Thresholds;
use acmon::meter::{tier_name, Meter, WINDOW};
use acmon::schedule::{
    resized_slice, stalest_first, Budgets, Cadence, Completion, Coverage, Pace, Schedule, Tier,
    TIERS,
};
use acmon::state::{Paths, StateStore, TieredState, STATE_FILE};
use acmon::tiers::{self, Observed, Published};
use acmon::vcs::{Unreadable, VcsFacts};
use acmon::workspace::NamespaceResolution;
use acmon::world::{
    ActivityUnavailable, CodexSession, LoadAverage, ProcessRecord, ProcessSnapshot, ResourceSource,
    Resources, ResourcesUnavailable, Sweep, World, WorldError,
};

// --- A machine that counts what it was asked --------------------------------------------------
//
// Counters rather than fixtures alone, because the whole claim of a tier is about *which*
// observations a pass makes. A fake that only supplied answers could not tell a fast pass that
// left `git` alone from one that asked it and ignored the reply.
//
// Uses atomics and a mutex rather than `RefCell`: the medium and slow passes run off the loop's
// thread, so a world that is not `Sync` cannot be handed to one.

/// A workspace that really is a repository on the machine these fixtures came from, and one that
/// is not. Both spellings are real; neither is invented.
const WORKSPACE: &str = "/Users/pmcfadin/projects/agentic_coding_monitor";
const OTHER_WORKSPACE: &str = "/Users/pmcfadin/projects/workforceos";
const NAMESPACE: &str = "-Users-pmcfadin-projects-agentic-coding-monitor";

/// An executable path a real Claude Code session was observed at.
const CLAUDE_EXE: &str = "/Users/pmcfadin/.local/share/claude/versions/2.1.233";

struct CountingWorld {
    snapshots: AtomicUsize,
    ledgers: AtomicUsize,
    namespace_listings: AtomicUsize,
    activity_reads: AtomicUsize,
    resolutions: AtomicUsize,
    sweeps: AtomicUsize,
    codex_reads: AtomicUsize,
    /// Every batch of paths `git` was asked about, in order, so coverage is assertable.
    git_batches: Mutex<Vec<Vec<String>>>,
    /// Which pids are in the process table. Changed between passes to make a session appear or go.
    records: Mutex<Vec<ProcessRecord>>,
    /// Paths this fake says are repositories, and how dirty each is.
    repositories: HashMap<String, usize>,
}

impl CountingWorld {
    fn new() -> CountingWorld {
        CountingWorld {
            snapshots: AtomicUsize::new(0),
            ledgers: AtomicUsize::new(0),
            namespace_listings: AtomicUsize::new(0),
            activity_reads: AtomicUsize::new(0),
            resolutions: AtomicUsize::new(0),
            sweeps: AtomicUsize::new(0),
            codex_reads: AtomicUsize::new(0),
            git_batches: Mutex::new(Vec::new()),
            records: Mutex::new(vec![
                ProcessRecord {
                    // The observer must be in its own enumeration or the snapshot is
                    // untrustworthy and no collection is possible at all.
                    pid: std::process::id() as i32,
                    exe_path: Ok("/usr/bin/true".to_string()),
                    cwd: Ok(WORKSPACE.to_string()),
                },
                ProcessRecord {
                    pid: 4242,
                    exe_path: Ok(CLAUDE_EXE.to_string()),
                    cwd: Ok(WORKSPACE.to_string()),
                },
            ]),
            repositories: [(WORKSPACE.to_string(), 3), (OTHER_WORKSPACE.to_string(), 7)]
                .into_iter()
                .collect(),
        }
    }

    /// Take every agent process out of the table, so the next fast pass sees no live session.
    fn all_sessions_gone(&self) {
        self.records
            .lock()
            .expect("uncontended")
            .retain(|record| record.pid == std::process::id() as i32);
    }

    fn git_paths(&self) -> Vec<String> {
        self.git_batches
            .lock()
            .expect("uncontended")
            .iter()
            .flatten()
            .cloned()
            .collect()
    }
}

fn measured_ledger() -> Resources {
    Resources {
        source: ResourceSource::Rusage,
        own_cpu: Ok(Duration::from_secs(12)),
        children_cpu: Ok(Duration::from_secs(300)),
        current_memory: Ok(259_081_656),
        peak_memory: Ok(358_991_288),
        bytes_written: Ok(6_709_248),
    }
}

impl World for CountingWorld {
    fn process_snapshot(&self) -> Result<ProcessSnapshot, WorldError> {
        self.snapshots.fetch_add(1, Ordering::SeqCst);
        Ok(ProcessSnapshot {
            records: self.records.lock().expect("uncontended").clone(),
            observer_pid: std::process::id() as i32,
        })
    }

    fn resources(&self, _pid: i32) -> Result<Resources, ResourcesUnavailable> {
        self.ledgers.fetch_add(1, Ordering::SeqCst);
        Ok(measured_ledger())
    }

    fn recorded_namespaces(&self) -> Result<Vec<String>, WorldError> {
        self.namespace_listings.fetch_add(1, Ordering::SeqCst);
        Ok(vec![NAMESPACE.to_string()])
    }

    fn namespace_activity(&self, _namespace: &str) -> Result<SystemTime, ActivityUnavailable> {
        self.activity_reads.fetch_add(1, Ordering::SeqCst);
        Ok(SystemTime::now())
    }

    fn codex_sessions(&self) -> Result<Vec<CodexSession>, WorldError> {
        self.codex_reads.fetch_add(1, Ordering::SeqCst);
        Ok(Vec::new())
    }

    fn resolve_namespace(&self, _namespace: &str) -> NamespaceResolution {
        self.resolutions.fetch_add(1, Ordering::SeqCst);
        NamespaceResolution::Resolved(WORKSPACE.to_string())
    }

    fn sweep_for_repositories(&self, _roots: &[String]) -> Sweep {
        self.sweeps.fetch_add(1, Ordering::SeqCst);
        Sweep {
            repositories: vec![
                (WORKSPACE.to_string(), false),
                (OTHER_WORKSPACE.to_string(), false),
            ],
            complete: true,
            directories_visited: 122,
        }
    }

    fn repository_root(&self, path: &str) -> Option<(String, bool)> {
        self.repositories
            .keys()
            .find(|root| path.eq_ignore_ascii_case(root))
            .map(|root| (root.clone(), false))
    }

    fn vcs_facts(&self, path: &str) -> Result<VcsFacts, Unreadable> {
        self.git_batches
            .lock()
            .expect("uncontended")
            .push(vec![path.to_string()]);
        match self.repositories.get(path) {
            Some(entries) => Ok(VcsFacts {
                root: path.to_string(),
                uncommitted_entries: *entries,
                linked_worktree: false,
            }),
            None => Err(Unreadable::NotVersionControlled),
        }
    }

    fn vcs_facts_batch(&self, paths: &[String]) -> Vec<Result<VcsFacts, Unreadable>> {
        self.git_batches
            .lock()
            .expect("uncontended")
            .push(paths.to_vec());
        paths
            .iter()
            .map(|path| match self.repositories.get(path) {
                Some(entries) => Ok(VcsFacts {
                    root: path.clone(),
                    uncommitted_entries: *entries,
                    linked_worktree: false,
                }),
                None => Err(Unreadable::NotVersionControlled),
            })
            .collect()
    }

    fn load_average(&self) -> Result<LoadAverage, String> {
        Ok(LoadAverage {
            one_minute: 5.4,
            five_minute: 4.0,
            fifteen_minute: 4.2,
            cpus: 16,
        })
    }

    fn output_width(&self) -> u16 {
        120
    }
}

/// One pass of a tier over what is already known. The loop's own step, without the loop.
fn pass(
    tier: Tier,
    observed: &Observed,
    world: &CountingWorld,
    at: Duration,
    role: Role,
) -> tiers::Pass {
    tiers::run_pass(
        tier,
        observed,
        world,
        SystemTime::now(),
        at,
        &Thresholds::default(),
        role,
        &Budgets::DEFAULT,
    )
}

/// Run one pass and fold it in, the way the loop does.
fn pass_and_absorb(
    tier: Tier,
    observed: &mut Observed,
    world: &CountingWorld,
    at: Duration,
    sequence: u64,
) -> tiers::Pass {
    let completed = pass(tier, observed, world, at, Role::Display);
    observed.absorb(&completed, sequence);
    completed
}

/// Every tier once, in the order the loop runs them on its first turn.
fn one_full_round(observed: &mut Observed, world: &CountingWorld, at: Duration) {
    for tier in TIERS {
        pass_and_absorb(tier, observed, world, at, 1);
    }
}

// --- Which tiers are due, and when --------------------------------------------------------------

#[test]
fn every_tier_is_due_on_the_first_look_because_nothing_has_been_collected_yet() {
    // A monitor that had collected nothing and published nothing for a whole slow interval would
    // read, on a screen, exactly like a machine with no agents on it.
    let schedule = Schedule::new();
    assert_eq!(
        schedule.due(Duration::ZERO),
        TIERS.to_vec(),
        "with nothing yet collected every tier is due, cheapest first"
    );
}

#[test]
fn the_fast_tier_comes_due_more_often_than_the_medium_one_and_the_medium_more_than_the_slow() {
    // The invariant the whole design rests on, driven through an hour of decisions instantly.
    // Asserted as an ordering rather than as counts, because the intervals are tuned against
    // measurements and are expected to change; their ordering is not.
    for pace in [Pace::Active, Pace::Idle] {
        let mut schedule = Schedule::new();
        schedule.adopt(pace);
        let mut counted: HashMap<Tier, usize> = HashMap::new();

        for second in 0..3_600 {
            let at = Duration::from_secs(second);
            for tier in schedule.due(at) {
                schedule.begun(tier, at);
                *counted.entry(tier).or_default() += 1;
            }
        }

        let fast = counted[&Tier::Fast];
        let medium = counted[&Tier::Medium];
        let slow = counted[&Tier::Slow];
        assert!(
            fast > medium && medium > slow,
            "{pace:?}: an hour gave {fast} fast, {medium} medium and {slow} slow passes; the \
             cheapest tier must run most often"
        );
    }
}

#[test]
fn a_slow_pass_that_ran_long_does_not_push_the_fast_tier_past_its_own_interval() {
    // The criterion, as arithmetic over the schedule: the fast tier's turn is measured from its
    // own last start, so nothing the slow tier does can move it. (What stops the slow tier
    // *occupying* the loop while it runs is that it runs on its own thread — asserted below by
    // the pass counts of a real run.)
    let mut schedule = Schedule::new();
    let cadence = Cadence::ACTIVE;

    schedule.begun(Tier::Fast, Duration::ZERO);
    schedule.begun(Tier::Slow, Duration::ZERO);

    let one_fast_interval_later = cadence.fast;
    assert!(
        schedule.is_due(Tier::Fast, one_fast_interval_later),
        "the fast tier is due one fast interval after its own last pass"
    );
    assert!(
        !schedule.is_due(Tier::Slow, one_fast_interval_later),
        "and the slow tier is not, because its interval is longer"
    );
    assert_eq!(
        schedule.started_at(Tier::Slow),
        Some(Duration::ZERO),
        "a tier's turn is counted from when its pass STARTED, so a pass that ran long does not \
         silently stretch its own cadence"
    );
}

#[test]
fn a_tier_stamp_advances_only_for_the_tier_that_ran() {
    let mut schedule = Schedule::new();
    schedule.begun(Tier::Fast, Duration::from_secs(7));

    assert_eq!(
        schedule.started_at(Tier::Fast),
        Some(Duration::from_secs(7))
    );
    assert_eq!(
        schedule.started_at(Tier::Medium),
        None,
        "the medium tier did not run, so it has no start to report — not a zero one"
    );
    assert_eq!(schedule.started_at(Tier::Slow), None);
}

#[test]
fn the_loop_never_sleeps_longer_than_its_fast_interval_so_a_change_of_pace_is_noticed() {
    let mut schedule = Schedule::new();
    for tier in TIERS {
        schedule.begun(tier, Duration::ZERO);
    }

    let wake = schedule.next_wake(Duration::ZERO);
    assert!(
        wake <= schedule.cadence().fast,
        "waiting out a ten-minute slow interval would mean noticing the morning's first session \
         ten minutes late; got {wake:?}"
    );
}

// --- Idling down, and rising again --------------------------------------------------------------

#[test]
fn idling_down_reduces_the_work_a_window_costs_on_every_tier() {
    // F22 as a ratio. Most of the day there is nothing to poll, and the figure that matters is
    // how many passes an idle hour costs compared with a busy one — never how long a pass took.
    let mut busy: HashMap<Tier, usize> = HashMap::new();
    let mut quiet: HashMap<Tier, usize> = HashMap::new();

    for (pace, counted) in [(Pace::Active, &mut busy), (Pace::Idle, &mut quiet)] {
        let mut schedule = Schedule::new();
        schedule.adopt(pace);
        for second in 0..3_600 {
            let at = Duration::from_secs(second);
            for tier in schedule.due(at) {
                schedule.begun(tier, at);
                *counted.entry(tier).or_default() += 1;
            }
        }
    }

    for tier in TIERS {
        assert!(
            quiet[&tier] < busy[&tier],
            "idling down must cost strictly less work on the {} tier: {} passes idle against {} \
             active",
            tier_name(tier),
            quiet[&tier],
            busy[&tier]
        );
    }
}

#[test]
fn no_live_session_idles_the_cadence_down_and_the_first_one_seen_raises_it_at_once() {
    // The rise has to be immediate, not "at the end of the idle interval". A session that starts
    // while the monitor is idling must be picked up on the next look, or the display shows an
    // empty table for up to a whole idle interval while an agent is working.
    let mut schedule = Schedule::new();
    assert!(
        schedule.adopt(Pace::for_live_sessions(0)),
        "the pace changed"
    );
    assert_eq!(schedule.cadence(), Cadence::IDLE);

    // An idle fast pass has just happened, and one active interval has gone by since.
    let last = Duration::from_secs(1_000);
    schedule.begun(Tier::Fast, last);
    let a_moment_later = last + Cadence::ACTIVE.fast;
    assert!(
        !schedule.is_due(Tier::Fast, a_moment_later),
        "while idle, one active interval is not yet a turn"
    );

    assert!(
        schedule.adopt(Pace::for_live_sessions(1)),
        "the pace changed"
    );
    assert!(
        schedule.is_due(Tier::Fast, a_moment_later),
        "the moment a session is seen, the fast tier is due at the active interval — the rise \
         needs no special case because the interval it is measured against just got shorter"
    );
}

#[test]
fn the_pace_is_decided_by_observed_processes_and_not_by_an_inferred_verdict() {
    // Idling down on an inference would let one misread transcript put the monitor to sleep with
    // an agent still running. So the count comes from sessions found IN the process enumeration.
    let world = CountingWorld::new();
    let mut observed = Observed::default();
    one_full_round(&mut observed, &world, Duration::ZERO);

    let with_session = pass(Tier::Fast, &observed, &world, Duration::ZERO, Role::Display);
    let live = tiers::live_sessions(with_session.snapshot.as_ref().expect("a collection"));
    assert!(
        live >= 1,
        "the fixture has a live Claude process, so it must be counted as a live session"
    );
    assert_eq!(Pace::for_live_sessions(live), Pace::Active);

    world.all_sessions_gone();
    let without = pass(Tier::Fast, &observed, &world, Duration::ZERO, Role::Display);
    let live = tiers::live_sessions(without.snapshot.as_ref().expect("a collection"));
    assert_eq!(
        live, 0,
        "with the agent process gone from the table, no session is live"
    );
    assert_eq!(Pace::for_live_sessions(live), Pace::Idle);
}

// --- What each tier actually observes ----------------------------------------------------------

#[test]
fn a_fast_pass_reads_the_process_table_and_asks_git_nothing() {
    // The tier assignment, asserted by what was asked rather than by what it cost. `git` is the
    // 2.7-second observation; a fast pass that touched it would be the whole defect back again.
    let world = CountingWorld::new();
    let mut observed = Observed::default();
    one_full_round(&mut observed, &world, Duration::ZERO);

    let snapshots_before = world.snapshots.load(Ordering::SeqCst);
    let git_before = world.git_batches.lock().expect("uncontended").len();
    let activity_before = world.activity_reads.load(Ordering::SeqCst);

    pass_and_absorb(
        Tier::Fast,
        &mut observed,
        &world,
        Duration::from_secs(10),
        2,
    );

    assert_eq!(
        world.snapshots.load(Ordering::SeqCst),
        snapshots_before + 1,
        "a fast pass enumerates processes: that is its whole job"
    );
    assert_eq!(
        world.git_batches.lock().expect("uncontended").len(),
        git_before,
        "and it asks git nothing at all — a full sweep costs 2.7 s and belongs in the slow tier"
    );
    assert_eq!(
        world.activity_reads.load(Ordering::SeqCst),
        activity_before,
        "nor does it re-read the transcript stores: 91 of those listings measured 60 ms, which is \
         why they are the medium tier's"
    );
}

#[test]
fn a_slow_pass_asks_git_and_does_not_re_enumerate_the_process_table() {
    let world = CountingWorld::new();
    let mut observed = Observed::default();
    one_full_round(&mut observed, &world, Duration::ZERO);

    let snapshots_before = world.snapshots.load(Ordering::SeqCst);
    let git_before = world.git_batches.lock().expect("uncontended").len();

    pass_and_absorb(
        Tier::Slow,
        &mut observed,
        &world,
        Duration::from_secs(120),
        2,
    );

    assert!(
        world.git_batches.lock().expect("uncontended").len() > git_before,
        "a slow pass is the one that asks git"
    );
    assert_eq!(
        world.snapshots.load(Ordering::SeqCst),
        snapshots_before,
        "and it reasons about the processes the fast tier already enumerated, at the fast tier's \
         stamp, rather than enumerating them again"
    );
}

#[test]
fn a_medium_pass_reads_the_searches_and_neither_the_process_table_nor_git() {
    let world = CountingWorld::new();
    let mut observed = Observed::default();
    one_full_round(&mut observed, &world, Duration::ZERO);

    let snapshots_before = world.snapshots.load(Ordering::SeqCst);
    let git_before = world.git_batches.lock().expect("uncontended").len();
    let sweeps_before = world.sweeps.load(Ordering::SeqCst);
    let activity_before = world.activity_reads.load(Ordering::SeqCst);

    pass_and_absorb(
        Tier::Medium,
        &mut observed,
        &world,
        Duration::from_secs(60),
        2,
    );

    assert!(
        world.sweeps.load(Ordering::SeqCst) > sweeps_before,
        "the sweep is the medium tier's"
    );
    assert!(
        world.activity_reads.load(Ordering::SeqCst) > activity_before,
        "and so is the transcript activity read, by measured cost"
    );
    assert_eq!(world.snapshots.load(Ordering::SeqCst), snapshots_before);
    assert_eq!(
        world.git_batches.lock().expect("uncontended").len(),
        git_before
    );
}

#[test]
fn an_observation_no_tier_has_made_yet_is_reported_as_unknown_and_names_the_tier_that_owes_it() {
    // The first fast pass of a fresh monitor: nothing else has run. Every workspace is genuinely
    // unknown, and it must say so — an empty at-risk panel here would be a monitor reporting a
    // clean machine before it had looked at one.
    let world = CountingWorld::new();
    let observed = Observed::default();

    let first = pass(Tier::Fast, &observed, &world, Duration::ZERO, Role::Display);
    let snapshot = first.snapshot.expect("the process table is readable");

    assert!(
        !snapshot.workspaces.is_empty(),
        "the workspaces are still discovered from the processes the fast tier saw"
    );
    for workspace in &snapshot.workspaces {
        assert_eq!(
            workspace.state,
            acmon::vcs::WorkspaceState::Unknown(Unreadable::NotYetRead),
            "before any slow pass, {} has not been asked about",
            workspace.path
        );
        assert!(
            !workspace.state.at_risk(),
            "and it is NOT at-risk: git has not been asked, so there is no evidence — alerting \
             here would make every start of the monitor an alert storm"
        );
        assert_eq!(
            workspace.uncommitted_entries, None,
            "never Some(0) standing in for `we have not looked`"
        );
    }
}

#[test]
fn a_workspace_is_at_risk_once_git_has_actually_been_asked_about_it() {
    // The complement of the test above: the reason `NotYetRead` is safe to treat as not-at-risk is
    // that the slow tier does reach it, and the verdict then appears.
    let world = CountingWorld::new();
    let mut observed = Observed::default();
    one_full_round(&mut observed, &world, Duration::ZERO);

    let after = pass(
        Tier::Slow,
        &observed,
        &world,
        Duration::from_secs(120),
        Role::Display,
    );
    let snapshot = after.snapshot.expect("a collection");

    let stranded: Vec<&str> = snapshot
        .workspaces
        .iter()
        .filter(|workspace| workspace.state == acmon::vcs::WorkspaceState::DirtyStranded)
        .map(|workspace| workspace.path.as_str())
        .collect();

    assert!(
        stranded.contains(&OTHER_WORKSPACE),
        "the workspace with uncommitted work and no session in it is stranded once git has \
         answered; stranded set was {stranded:?}"
    );
}

// --- Reading a bounded slice, stalest first ----------------------------------------------------

#[test]
fn the_slow_tier_reads_the_workspaces_it_has_never_read_before_ones_it_read_recently() {
    let age = [
        Some(Duration::from_secs(30)),
        None,
        Some(Duration::from_secs(900)),
        None,
    ];

    assert_eq!(
        stalest_first(&age, 2),
        vec![1, 3],
        "the two never read come first: a workspace nothing is known about is the one to ask about"
    );
    assert_eq!(
        stalest_first(&age, 4),
        vec![1, 3, 2, 0],
        "and the ones that have been read follow oldest-first, so nothing can be starved"
    );
}

#[test]
fn a_slice_that_overran_its_budget_shrinks_and_one_well_inside_it_grows() {
    // The mechanism that keeps the git sweep inside the duty cycle without a constant tuned on an
    // idle machine. Asserted as ratios of the previous slice, never against a wall clock.
    let budget = Duration::from_millis(300);

    let shrunk = resized_slice(20, budget * 2, budget);
    assert!(
        shrunk < 20,
        "a pass that took twice its budget must ask about fewer workspaces next time; got {shrunk}"
    );
    let grown = resized_slice(20, budget / 4, budget);
    assert!(
        grown > 20,
        "and one that came in at a quarter of its budget can afford more; got {grown}"
    );
    assert!(
        resized_slice(1, budget * 100, budget) >= 1,
        "never to zero: a slice of nothing would never read another workspace again"
    );
}

#[test]
fn coverage_is_incomplete_until_every_workspace_has_been_read_at_least_once() {
    assert!(
        !Coverage {
            total: 70,
            read: 16,
            never_read: 54
        }
        .complete(),
        "54 workspaces nobody has looked at is not an exhaustive at-risk panel"
    );
    assert!(
        Coverage {
            total: 70,
            read: 16,
            never_read: 0
        }
        .complete(),
        "having read every workspace at some point is the steady state, whatever one pass covered"
    );
}

#[test]
fn a_workspace_left_out_of_a_slice_keeps_the_facts_and_the_age_of_when_it_was_last_read() {
    // The point of stamping per workspace rather than per tier. Two rows in one payload can
    // legitimately be minutes apart in age, and one stamp for the pass would misdescribe all but
    // the newest — which is exactly what F30 forbids.
    let world = CountingWorld::new();
    let mut observed = Observed::default();
    one_full_round(&mut observed, &world, Duration::ZERO);
    let asked_first = world.git_paths();

    // A second slow pass, much later.
    let second = pass_and_absorb(
        Tier::Slow,
        &mut observed,
        &world,
        Duration::from_secs(600),
        2,
    );

    let payload = tiers::slow_payload(&second, 2, &observed);
    let stamped: Vec<&acmon::tiers::WorkspaceRow> = payload
        .workspaces
        .iter()
        .filter(|row| row.observed_at.is_some())
        .collect();

    assert!(
        !stamped.is_empty(),
        "a workspace git has answered about carries the instant it was asked"
    );
    assert!(
        !asked_first.is_empty(),
        "the first round must actually have asked about something, or this proves nothing"
    );
}

// --- Metering itself --------------------------------------------------------------------------

#[test]
fn a_duty_cycle_that_cannot_be_measured_yet_is_absent_with_a_reason_and_never_zero() {
    // A duty cycle of 0% is a monitor that is running and idle — the one thing a reader would most
    // like to know, and the one thing "not measured yet" does not mean.
    let meter = Meter::default();
    let report = meter.report(Duration::ZERO, "active", &Budgets::DEFAULT);

    assert_eq!(report.duty_cycle.value, None);
    assert!(
        report
            .duty_cycle
            .unavailable
            .as_deref()
            .is_some_and(|why| !why.is_empty()),
        "and it carries the reason, which is what makes it different from a zero"
    );
    assert_eq!(
        report.within_budget.value, None,
        "a verdict about a figure nobody has is not `true`"
    );
}

#[test]
fn the_duty_cycle_is_cpu_over_the_wall_time_it_was_measured_across() {
    // The one arithmetic assertion in this file, and it is not a measurement: both inputs are
    // given. 600 ms of CPU across a 60 s window is 1% of one core on any machine.
    let mut meter = Meter::default();
    meter.sampled(Duration::ZERO, Duration::ZERO);
    meter.sampled(WINDOW, Duration::from_millis(600));

    let duty = meter.duty_cycle().expect("two samples span a window");
    assert!(
        (duty - 0.01).abs() < 1e-9,
        "expected 1% of a core, got {duty}"
    );

    let report = meter.report(WINDOW, "active", &Budgets::DEFAULT);
    assert_eq!(
        report.within_budget.value,
        Some(true),
        "1% is the budget, so exactly 1% is inside it — and the verdict is published beside the \
         number so a breach is a fact rather than something a reader has to work out"
    );

    // Twice the CPU in the same window is over.
    let mut over = Meter::default();
    over.sampled(Duration::ZERO, Duration::ZERO);
    over.sampled(WINDOW, Duration::from_millis(1_200));
    assert_eq!(
        over.report(WINDOW, "active", &Budgets::DEFAULT)
            .within_budget
            .value,
        Some(false)
    );
}

#[test]
fn each_tier_s_last_pass_is_reported_with_its_own_age_even_when_older_than_the_window() {
    // A slow pass is legitimately older than the trailing window. "No slow pass in the last
    // minute" and "the slow tier has never run" are different facts, and a meter row that could
    // not tell them apart would report a working tier as an absent one.
    let mut meter = Meter::default();
    meter.completed(
        Tier::Slow,
        Duration::from_secs(1),
        Duration::from_millis(250),
        Completion::WithinBudget,
    );
    meter.completed(
        Tier::Fast,
        Duration::from_secs(600),
        Duration::from_millis(40),
        Completion::WithinBudget,
    );

    let report = meter.report(Duration::from_secs(600), "active", &Budgets::DEFAULT);
    let slow = report
        .last_pass
        .iter()
        .find(|pass| pass.tier == "slow")
        .expect("a slow pass that ran ten minutes ago is still the last slow pass");

    assert!(
        slow.age_ms > WINDOW.as_millis(),
        "and its age says how long ago it was: {} ms",
        slow.age_ms
    );
    assert_eq!(meter.count(Tier::Slow), 1);
    assert_eq!(
        meter.count(Tier::Medium),
        0,
        "a tier that has not run has run no passes — that zero is a count, not a measurement"
    );
}

#[test]
fn a_pass_that_overran_its_budget_says_so_in_words_rather_than_being_averaged_away() {
    let overran = Completion::of(Duration::from_millis(900), Duration::from_millis(300));
    assert!(overran.overran());
    let why = overran.why().expect("an overrun carries its reason");
    assert!(
        why.contains("900") && why.contains("300"),
        "the reason has to carry both figures, or a reader cannot tell a near miss from a \
         threefold breach; got {why}"
    );
    assert_eq!(
        Completion::of(Duration::from_millis(10), Duration::from_millis(300)).why(),
        None,
        "and an ordinary pass says nothing, so the message means something when it appears"
    );
}

// --- What is published, and what reads it back -------------------------------------------------

#[test]
fn each_tier_is_published_under_its_own_stamp_and_only_the_tier_that_ran_moves() {
    // F21 and F30 through the real state store: three tiers, three timestamps, and a pass of one
    // tier leaves the others' stamps exactly where they were.
    let directory = scratch("stamps");
    let store = store_in(&directory);
    let world = CountingWorld::new();

    let mut state = TieredState::new(std::process::id());
    let mut observed = Observed::default();

    for (index, tier) in TIERS.iter().enumerate() {
        let completed = pass_and_absorb(
            *tier,
            &mut observed,
            &world,
            Duration::from_secs(index as u64),
            1,
        );
        state.set_tier_data(
            *tier,
            payload_for(&completed, 1, &observed),
            tiers::stamp_for(&completed, &observed),
        );
        store
            .write_tiered_state(STATE_FILE, &state)
            .expect("publishing");
    }

    let published = store
        .read_tiered_state(STATE_FILE)
        .expect("readable")
        .expect("present");
    let before: Vec<SystemTime> = TIERS
        .iter()
        .map(|tier| published.tier_timestamp(*tier).expect("every tier ran"))
        .collect();

    // Now one more medium pass, and nothing else.
    std::thread::sleep(Duration::from_millis(1_100));
    let again = pass_and_absorb(
        Tier::Medium,
        &mut observed,
        &world,
        Duration::from_secs(60),
        2,
    );
    state.set_tier_data(
        Tier::Medium,
        payload_for(&again, 2, &observed),
        tiers::stamp_for(&again, &observed),
    );
    store
        .write_tiered_state(STATE_FILE, &state)
        .expect("publishing");

    let after = store
        .read_tiered_state(STATE_FILE)
        .expect("readable")
        .expect("present");

    assert_eq!(
        after.tier_timestamp(Tier::Fast),
        Some(before[0]),
        "the fast tier did not run, so its stamp must not move — a stamp that advanced without a \
         pass would present old facts as fresh ones"
    );
    assert!(
        after.tier_timestamp(Tier::Medium).expect("published") > before[1],
        "and the tier that ran carries a newer stamp"
    );
    assert_eq!(
        after.tier_timestamp(Tier::Slow),
        Some(before[2]),
        "the slow tier did not run either"
    );

    let _ = std::fs::remove_dir_all(&directory);
}

#[test]
fn what_the_monitor_publishes_is_what_the_reader_decodes() {
    // The schema is one thing, owned in one place. A payload the monitor writes and no reader can
    // decode is the state `agtop` reports today as "tiers this display has no reader for", and it
    // is worth exactly nothing on a screen.
    let directory = scratch("roundtrip");
    let store = store_in(&directory);
    let world = CountingWorld::new();

    let mut state = TieredState::new(std::process::id());
    let mut observed = Observed::default();
    let mut meter = Meter::default();
    meter.sampled(Duration::ZERO, Duration::ZERO);
    meter.sampled(WINDOW, Duration::from_millis(300));

    // Every tier once, then a second fast pass — which is the order the loop runs them in, and
    // the reason the fast payload can report all three tiers' pass durations: it is rebuilt on
    // every fast pass from a meter the other two have already reported into.
    let mut passes = Vec::new();
    for tier in TIERS {
        let completed = pass_and_absorb(tier, &mut observed, &world, Duration::ZERO, 1);
        meter.completed(tier, Duration::ZERO, completed.took, completed.completion);
        passes.push(completed);
    }
    passes.push(pass_and_absorb(
        Tier::Fast,
        &mut observed,
        &world,
        Cadence::ACTIVE.fast,
        2,
    ));
    meter.completed(
        Tier::Fast,
        Cadence::ACTIVE.fast,
        passes[3].took,
        passes[3].completion,
    );

    for completed in &passes {
        let payload = match completed.tier {
            Tier::Fast => serde_json::to_value(tiers::fast_payload(
                completed,
                1,
                &observed,
                meter.report(WINDOW, "active", &Budgets::DEFAULT),
                &a_first_launch(),
            )),
            Tier::Medium => serde_json::to_value(tiers::medium_payload(completed, 1, &observed)),
            Tier::Slow => serde_json::to_value(tiers::slow_payload(completed, 1, &observed)),
        }
        .expect("a payload is serialisable");
        state.set_tier_data(
            completed.tier,
            payload,
            tiers::stamp_for(completed, &observed),
        );
    }
    store
        .write_tiered_state(STATE_FILE, &state)
        .expect("publishing");

    let read_back = store
        .read_tiered_state(STATE_FILE)
        .expect("readable")
        .expect("present");

    match tiers::published(&read_back, Tier::Fast).expect("the fast payload decodes") {
        Some((Published::Fast(payload), _)) => {
            assert!(
                !payload.sessions.is_empty(),
                "the sessions the monitor saw survive the round trip"
            );
            assert_eq!(
                payload.silence_read_by, "medium",
                "and the payload says which tier's stamp the liveness evidence ages against"
            );
        }
        other => panic!("expected a fast payload, got {other:?}"),
    }
    assert!(matches!(
        tiers::published(&read_back, Tier::Slow).expect("decodes"),
        Some((Published::Slow(_), _))
    ));
    assert!(matches!(
        tiers::published(&read_back, Tier::Medium).expect("decodes"),
        Some((Published::Medium(_), _))
    ));

    let meters = tiers::published_meters(&read_back)
        .expect("the monitor's own figures decode")
        .expect("they were published");
    assert!(
        meters.overhead.is_ok(),
        "the collection overhead is a figure the display draws, so it has to come back as one: \
         {:?}",
        meters.overhead
    );
    assert_eq!(
        meters.duty_cycle,
        Ok(0.005),
        "300 ms of CPU over a 60 s window is 0.5% of one core, and it survives the round trip"
    );
    assert_eq!(
        meters.per_tier.len(),
        TIERS.len(),
        "every tier's pass duration is published, not just the cheapest one"
    );

    let _ = std::fs::remove_dir_all(&directory);
}

#[test]
fn a_tier_payload_the_reader_cannot_understand_is_an_error_rather_than_an_absent_figure() {
    // Half a state file renders as a shorter session list and a shorter at-risk panel, which is
    // the shape of a healthy screen. So a payload that does not decode must not read as "no tier".
    let directory = scratch("undecodable");
    let store = store_in(&directory);

    let mut state = TieredState::new(31_337);
    state.set_tier_data(
        Tier::Fast,
        serde_json::json!({"sessions": "all of them"}),
        SystemTime::now(),
    );
    store
        .write_tiered_state(STATE_FILE, &state)
        .expect("publishing");

    let read_back = store
        .read_tiered_state(STATE_FILE)
        .expect("readable")
        .expect("present");

    let why = tiers::published(&read_back, Tier::Fast).expect_err("this payload cannot be read");
    assert!(
        why.contains("fast"),
        "the reason must name the tier; got {why}"
    );
    assert_eq!(
        tiers::published(&read_back, Tier::Slow).expect("no slow payload is not a fault"),
        None,
        "a tier that has not been published is absent, which is a different thing and reads as one"
    );

    let _ = std::fs::remove_dir_all(&directory);
}

#[test]
fn a_warming_up_pass_writes_nothing_and_says_that_is_why_it_announced_nothing() {
    // The role gate from #10, used by the loop: until every tier has observed something, the
    // collection is read-only. Writing a memory file assembled before the searches and the git
    // reads had happened would throw away exactly the remembered workspaces that make the safety
    // net durable.
    let world = CountingWorld::new();
    let observed = Observed::default();

    let warming = pass(Tier::Fast, &observed, &world, Duration::ZERO, Role::Display);
    let payload = tiers::fast_payload(
        &warming,
        1,
        &observed,
        Meter::default().report(Duration::ZERO, "active", &Budgets::DEFAULT),
        &a_first_launch(),
    );

    assert!(
        !payload.pass.announcing,
        "a read-only pass is not announcing, and the payload says so"
    );
    assert!(
        !payload.pass.every_tier_has_run,
        "and it says the round is not complete, so a reader knows why"
    );
    assert!(
        payload.notify.read_only.is_some(),
        "the reason nothing was announced is on disk, not inferred from a zero count"
    );
}

#[test]
fn every_pass_records_what_the_machine_as_a_whole_was_carrying() {
    // "Measured under load; treat as upper bounds." A sample taken at load 26 is meaningless, and
    // afterwards there is no way to tell which samples those were unless the load travelled with
    // them.
    let world = CountingWorld::new();
    let mut observed = Observed::default();

    for tier in TIERS {
        let completed = pass_and_absorb(tier, &mut observed, &world, Duration::ZERO, 1);
        let envelope = tiers::envelope(&completed, 1, &observed);
        let load = envelope
            .load
            .value
            .expect("this machine reports its load average");
        assert_eq!(
            load.cpus, 16,
            "with the core count, or the figure means nothing"
        );
        assert!(
            load.per_cpu() > 0.0,
            "the {} tier's pass must record the load it was taken under",
            tier_name(tier)
        );
    }
}

#[test]
fn a_world_that_cannot_read_the_machines_load_says_so_rather_than_reporting_an_idle_machine() {
    // The default implementation refuses, and that default is what a fixture-driven fake gets. A
    // plausible zero here would describe an idle machine — the one reading that would make every
    // other figure in the sample look trustworthy.
    struct NoMachine;
    impl World for NoMachine {
        fn process_snapshot(&self) -> Result<ProcessSnapshot, WorldError> {
            Err(WorldError::ProcessEnumeration("no machine".to_string()))
        }
        fn resources(&self, _pid: i32) -> Result<Resources, ResourcesUnavailable> {
            Err(ResourcesUnavailable::ProcessExited)
        }
        fn recorded_namespaces(&self) -> Result<Vec<String>, WorldError> {
            Ok(Vec::new())
        }
        fn namespace_activity(&self, _n: &str) -> Result<SystemTime, ActivityUnavailable> {
            Err(ActivityUnavailable::NotRecorded)
        }
        fn codex_sessions(&self) -> Result<Vec<CodexSession>, WorldError> {
            Ok(Vec::new())
        }
        fn output_width(&self) -> u16 {
            120
        }
        fn repository_root(&self, _path: &str) -> Option<(String, bool)> {
            None
        }
        fn vcs_facts(&self, _path: &str) -> Result<VcsFacts, Unreadable> {
            Err(Unreadable::NotVersionControlled)
        }
        fn resolve_namespace(&self, _n: &str) -> NamespaceResolution {
            NamespaceResolution::NoLongerExists
        }
        fn sweep_for_repositories(&self, _roots: &[String]) -> Sweep {
            Sweep {
                repositories: Vec::new(),
                complete: true,
                directories_visited: 0,
            }
        }
    }

    let why = NoMachine
        .load_average()
        .expect_err("a World with no machine has no load average");
    assert!(!why.is_empty(), "and it says so in words");
}

// --- The real monitor -------------------------------------------------------------------------
//
// One test that runs the built binary, because the thing being asserted — three tiers driven from
// one process, the slow one not delaying the fast one, and a duty cycle the monitor measured
// itself — is not observable from any part of it in isolation.

#[test]
fn amon_watch_drives_every_tier_from_one_loop_and_publishes_what_each_one_cost() {
    let state_dir = scratch("watch");

    // Long enough for several fast passes and at least one of every tier, bounded so the test
    // does not have to signal anything. What is asserted below is the *ratio* of pass counts and
    // the presence of each figure — never how long a pass took.
    let output = Command::new(env!("CARGO_BIN_EXE_amon"))
        .arg("watch")
        .env(acmon::state::STATE_DIR_VARIABLE, &state_dir)
        .env(acmon::watch::RUN_VARIABLE, "26000")
        .env(acmon::state::CONFIG_DIR_VARIABLE, state_dir.join("config"))
        // No notification configuration, so nothing is delivered anywhere by a test run.
        .env("ACMON_NOTIFY_CONFIG", state_dir.join("no-such-notify.toml"))
        .stdin(Stdio::null())
        .output()
        .expect("amon is built and runnable");

    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
        output.status.success(),
        "a monitor that ran its window and stopped cleanly exits zero; stderr was:\n{stderr}"
    );

    let store = store_in(&state_dir);
    let state = store
        .read_tiered_state(STATE_FILE)
        .expect("the published state file is readable")
        .expect("the monitor published one");

    // Every tier, published by this process and no other.
    assert_eq!(
        state.tier_count(),
        TIERS.len(),
        "all three tiers were driven"
    );
    for tier in TIERS {
        assert!(
            state.tier_timestamp(tier).is_some(),
            "the {} tier must carry its own timestamp, not the file's",
            tier_name(tier)
        );
    }

    let (fast, medium, slow) = (
        decode_fast(&state),
        decode_medium(&state),
        decode_slow(&state),
    );

    // The invariant, from the counts the run itself published: the cheap tier ran more often than
    // the expensive one. This is what "a slow tier running long never delays a fast tier" looks
    // like from outside — a 2.7 s git sweep on the loop's own thread could not have produced it.
    let count = |name: &str| {
        fast.monitor
            .passes
            .iter()
            .find(|(tier, _)| tier == name)
            .map(|(_, count)| *count)
            .expect("every tier is accounted for")
    };
    assert!(
        count("fast") > count("medium") && count("medium") >= count("slow"),
        "pass counts must follow the cadence: {} fast, {} medium, {} slow",
        count("fast"),
        count("medium"),
        count("slow")
    );

    // It metered itself, and the figures are figures rather than reasons.
    let duty = fast
        .monitor
        .duty_cycle
        .value
        .unwrap_or_else(|| panic!("no duty cycle: {:?}", fast.monitor.duty_cycle.unavailable));
    assert!(
        duty > 0.0,
        "a running monitor consumed some CPU, so a duty cycle of exactly zero would mean the \
         measurement is not being taken"
    );
    assert_eq!(
        fast.monitor.within_budget.value,
        Some(duty <= fast.monitor.budget),
        "the verdict published beside the number has to agree with it"
    );
    assert_eq!(
        fast.monitor.last_pass.len(),
        TIERS.len(),
        "every tier's pass duration is published (F25), not just the loop's total"
    );

    // What the tiering bought, as a ratio of the run's own published figures rather than as a
    // threshold. Before this seam, one collection made every observation at once, so its cost was
    // the sum of all three passes and it was paid at whatever the collection cadence was. Dividing
    // each pass by its own interval instead is the whole mechanism, and the ratio between the two
    // holds whatever the machine is doing — which a threshold does not.
    let took = |name: &str| {
        fast.monitor
            .last_pass
            .iter()
            .find(|pass| pass.tier == name)
            .map(|pass| pass.took_ms as f64)
            .expect("every tier published its last pass")
    };
    let per_second = |cost: f64, interval: Duration| cost / interval.as_millis() as f64;
    let untiered = per_second(
        took("fast") + took("medium") + took("slow"),
        Cadence::ACTIVE.fast,
    );
    let tiered = per_second(took("fast"), Cadence::ACTIVE.fast)
        + per_second(took("medium"), Cadence::ACTIVE.medium)
        + per_second(took("slow"), Cadence::ACTIVE.slow);
    assert!(
        tiered * 2.0 < untiered,
        "tiering has to cut the standing cost by more than half, or it is not worth the three \
         cadences: the same observations at one cadence would be {:.2}% of the loop's time and \
         tiered they are {:.2}%",
        untiered * 100.0,
        tiered * 100.0
    );

    // And NF9's absolute figure — against the **steady-state** cost, which is what `tiered` above
    // is: each tier's measured pass divided by its own interval.
    //
    // Deliberately not against the run's own published duty cycle, and the reason is worth
    // knowing. That figure is a trailing window, and a run short enough to belong in a test suite
    // covers less than the slow tier's interval — so the one medium and one slow pass every run
    // makes at startup are divided by 26 seconds instead of by their 60 and 120, which
    // over-weights them by a factor of two to five. Measured: this 26-second run publishes around
    // 1%, while the same build left running for three minutes publishes 0.37–0.47%. Asserting
    // against the short window would be asserting against a warm-up transient, and lengthening
    // the test past 130 seconds to avoid it would put two minutes on every suite run.
    //
    // `tiered` is also the stricter of the two: it is wall time per interval, and wall time is
    // never less than the CPU the duty cycle measures.
    //
    // Judged only when the machine was not oversubscribed. `docs/observability-mechanics.md` §5 is
    // explicit that timings taken at load 26 are meaningless, and the load is recorded with every
    // sample for exactly this decision — running the whole suite oversubscribes 16 cores, and a
    // budget asserted then would fail for reasons that have nothing to do with the cadence. The
    // figures are printed either way, so a run that could not judge still says what it saw.
    let load = fast
        .pass
        .load
        .value
        .expect("the pass recorded the machine load");
    println!(
        "steady-state cost {:.3}% of a core from published pass times; the run's own trailing \
         window says {:.3}% (budget {:.1}%) at load {:.1} on {} cores",
        tiered * 100.0,
        duty * 100.0,
        fast.monitor.budget * 100.0,
        load.one_minute,
        load.cpus
    );
    // Judged only against an optimised build. `cargo test` compiles this crate in debug, and a
    // debug monitor is not the artefact NF9 is a budget for: measured on this machine the same
    // cadence costs 0.413% of a core built `--release` and just over 1% built for tests. Asserting
    // the budget here would fail on the profile nobody ships, which is the "absolute figure in a
    // test" mistake wearing a cost-budget hat. The ratio above is asserted either way, under any
    // load and any profile.
    if load.per_cpu() <= 1.0 && !cfg!(debug_assertions) {
        assert!(
            tiered <= fast.monitor.budget,
            "with sessions live and the machine not oversubscribed, the steady-state cost must be \
             inside NF9's budget of {:.1}% of one core; the published pass times put it at {:.3}% \
             at load {:.1} on {} cores. If this fails, the cadence in `schedule.rs` is wrong for \
             this machine — not the test.",
            fast.monitor.budget * 100.0,
            tiered * 100.0,
            load.one_minute,
            load.cpus
        );
    } else {
        println!(
            "no absolute budget was judged — only the ratio above, which holds under any load and \
             any profile. Load was {:.1} on {} cores{}. To judge NF9 for real, measure the \
             optimised binary: `cargo build --release --bin amon` then run it with \
             ACMON_WATCH_RUN_MS set and read the duty cycle it reports.",
            load.one_minute,
            load.cpus,
            if cfg!(debug_assertions) {
                ", and this is a debug build"
            } else {
                ""
            }
        );
    }

    // Load travels with every pass.
    for envelope in [&fast.pass, &medium.pass, &slow.pass] {
        assert!(
            envelope.load.value.is_some(),
            "the {} tier must record the machine load its sample was taken under; reason given \
             was {:?}",
            envelope.tier,
            envelope.load.unavailable
        );
    }

    // The at-risk panel's evidence is stamped per workspace, and what has not been read is
    // counted rather than presented as clean.
    assert!(
        !slow.workspaces.is_empty(),
        "the slow tier found workspaces on this machine"
    );
    let read = slow
        .workspaces
        .iter()
        .filter(|row| row.observed_at.is_some())
        .count();
    assert!(
        read > 0,
        "and it actually asked git about some of them within its budget"
    );
    assert_eq!(
        slow.workspaces
            .iter()
            .filter(|row| row.observed_at.is_none())
            .count(),
        slow.never_read,
        "the pending count must agree with the rows that carry no reading — a count that drifted \
         from the rows would be a silent cap wearing a number"
    );
    for row in &slow.workspaces {
        if row.observed_at.is_none() {
            assert!(
                !row.at_risk,
                "{} has never been read, so it cannot be at-risk",
                row.path
            );
        }
    }

    let _ = std::fs::remove_dir_all(&state_dir);
}

#[test]
fn a_collection_leaves_the_git_index_of_an_observed_repository_untouched() {
    // NF7, as behaviour rather than as a grep for a flag. Plain `git status` refreshes and
    // rewrites the index when its cached stat data is stale, which takes a lock the agent working
    // in that repository needs — and would make the observer a participant. `--no-optional-locks`
    // is what stops it, and this is what would notice its removal.
    let repository = scratch("observed-repo");
    std::fs::create_dir_all(&repository).expect("create the repository directory");

    let git = |arguments: &[&str]| {
        let status = Command::new("git")
            .args(arguments)
            .current_dir(&repository)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("git is installed");
        assert!(status.success(), "git {arguments:?} failed");
    };

    git(&["init", "--quiet"]);
    git(&["config", "user.email", "test@example.invalid"]);
    git(&["config", "user.name", "Seam 16"]);
    std::fs::write(repository.join("tracked.txt"), "one\n").expect("write a file");
    git(&["add", "tracked.txt"]);
    git(&["commit", "--quiet", "-m", "one"]);
    // Settle the index, so any later rewrite is caused by what this test does next.
    git(&["status", "--porcelain"]);

    // Make the index's cached stat data stale without changing the file's content. This is
    // precisely the condition under which git wants to rewrite the index.
    let tracked = repository.join("tracked.txt");
    std::fs::write(&tracked, "one\n").expect("rewrite with identical content");

    let index = repository.join(".git").join("index");
    let before = std::fs::metadata(&index)
        .expect("the index exists")
        .modified()
        .expect("its modification time is readable");

    let world = acmon::RealWorld::new();
    let facts = world.vcs_facts(repository.to_str().expect("utf-8"));
    assert!(
        facts.is_ok(),
        "the observation itself has to have succeeded, or an untouched index proves nothing: \
         {facts:?}"
    );

    let after = std::fs::metadata(&index)
        .expect("the index still exists")
        .modified()
        .expect("readable");
    assert_eq!(
        before, after,
        "observing a repository must not write its index: that is a lock the agent working there \
         needs, and taking it makes the observer a participant"
    );

    let _ = std::fs::remove_dir_all(&repository);
}

// --- Helpers ----------------------------------------------------------------------------------

fn scratch(name: &str) -> PathBuf {
    let directory =
        std::env::temp_dir().join(format!("acmon-seam16-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).expect("create the scratch directory");
    directory
}

fn store_in(directory: &Path) -> StateStore {
    let directory = directory.to_str().expect("utf-8");
    StateStore::new(
        Paths::from_values(Some(directory), Some(directory), None)
            .expect("explicit directories need no home"),
    )
}

fn payload_for(completed: &tiers::Pass, sequence: u64, observed: &Observed) -> serde_json::Value {
    match completed.tier {
        Tier::Fast => serde_json::to_value(tiers::fast_payload(
            completed,
            sequence,
            observed,
            Meter::default().report(Duration::ZERO, "active", &Budgets::DEFAULT),
            &a_first_launch(),
        )),
        Tier::Medium => serde_json::to_value(tiers::medium_payload(completed, sequence, observed)),
        Tier::Slow => serde_json::to_value(tiers::slow_payload(completed, sequence, observed)),
    }
    .expect("a payload is serialisable")
}

/// The launch a fast payload assembled outside a monitored run has to name.
///
/// Not a stand-in: it is what `acmon::starts` decides for a state directory nothing has ever
/// written in, which is what these payload tests are assembling one for. Seam 17 owns the launch
/// record itself.
fn a_first_launch() -> acmon::starts::Launch {
    acmon::starts::first_launch(SystemTime::now(), std::process::id())
}

fn decode_fast(state: &TieredState) -> acmon::tiers::FastPayload {
    match tiers::published(state, Tier::Fast).expect("the fast payload decodes") {
        Some((Published::Fast(payload), _)) => *payload,
        other => panic!("expected a fast payload, got {other:?}"),
    }
}

fn decode_medium(state: &TieredState) -> acmon::tiers::MediumPayload {
    match tiers::published(state, Tier::Medium).expect("the medium payload decodes") {
        Some((Published::Medium(payload), _)) => *payload,
        other => panic!("expected a medium payload, got {other:?}"),
    }
}

fn decode_slow(state: &TieredState) -> acmon::tiers::SlowPayload {
    match tiers::published(state, Tier::Slow).expect("the slow payload decodes") {
        Some((Published::Slow(payload), _)) => *payload,
        other => panic!("expected a slow payload, got {other:?}"),
    }
}
