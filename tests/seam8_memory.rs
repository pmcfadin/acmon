//! Seam 8 — what survives between runs.
//!
//! The failure this whole seam exists to prevent: a workspace holding uncommitted work is
//! discoverable only while something is running in it, so the moment its session exits it
//! drops out of the report — which is exactly the moment it starts being at risk. Every test
//! here is a variation on "was it still checked" and "was the reason stated".
//!
//! Absolute durations appear only as *inputs* a test chooses. Nothing here asserts how long
//! anything took.

use std::cell::RefCell;
use std::collections::HashMap;
use std::time::{Duration, SystemTime};

use acmon::liveness::Thresholds;
use acmon::memory::{self, Degraded, Memory, Reading, RememberedSession, RememberedWorkspace};
use acmon::vcs::{Unreadable, VcsFacts, WorkspaceState};
use acmon::world::{
    ActivityUnavailable, CodexSession, ResourceSource, Resources, ResourcesUnavailable, StateRead,
    Sweep, Unmeasured,
};
use acmon::{collect, Identity, ProcessRecord, ProcessSnapshot, World, WorldError};

/// A fixed instant every test reasons from, so that "a week ago" is a computable thing rather
/// than something that depends on when the suite ran.
fn now() -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(1_786_987_902)
}

fn ago(duration: Duration) -> SystemTime {
    now() - duration
}

const DAY: Duration = Duration::from_secs(86_400);

/// The pid of the process pretending to be the observer. Every snapshot must contain it or
/// the collection is refused before any of this is reached.
const OBSERVER: i32 = 4_242;

/// The ledger of session 69046, measured in `docs/observability-mechanics.md` §2.6 — the
/// 19.4x child-to-own ratio that makes a lost ledger worth remembering.
fn measured_ledger() -> Resources {
    Resources {
        source: ResourceSource::Rusage,
        own_cpu: Ok(Duration::from_secs(1_669)),
        children_cpu: Ok(Duration::from_secs(32_317)),
        current_memory: Ok(482_000_000),
        peak_memory: Ok(622_000_000),
        bytes_written: Ok(166_000_000),
    }
}

struct FakeWorld {
    records: Vec<ProcessRecord>,
    ledgers: HashMap<i32, Result<Resources, ResourcesUnavailable>>,
    namespaces: Vec<String>,
    namespace_activities: HashMap<String, SystemTime>,
    resolutions: HashMap<String, acmon::workspace::NamespaceResolution>,
    facts: HashMap<String, Result<VcsFacts, Unreadable>>,
    roots: HashMap<String, (String, bool)>,
    /// The state store. `None` is a machine that has never run acmon.
    state: RefCell<Option<String>>,
    /// Set to refuse writes, so a run that cannot store what it found can be observed.
    refuse_writes: bool,
    /// Every path handed to `vcs_facts_batch`, so a test can prove a workspace was really
    /// re-checked rather than merely re-listed.
    checked: RefCell<Vec<String>>,
}

impl FakeWorld {
    /// A machine with nothing on it but the observer.
    fn quiet() -> Self {
        FakeWorld {
            records: vec![ProcessRecord {
                pid: OBSERVER,
                exe_path: Ok("/usr/bin/acmon".to_string()),
                cwd: Ok("/Users/pmcfadin".to_string()),
            }],
            ledgers: HashMap::new(),
            namespaces: Vec::new(),
            namespace_activities: HashMap::new(),
            resolutions: HashMap::new(),
            facts: HashMap::new(),
            roots: HashMap::new(),
            state: RefCell::new(None),
            refuse_writes: false,
            checked: RefCell::new(Vec::new()),
        }
    }

    fn storing(self, contents: &str) -> Self {
        *self.state.borrow_mut() = Some(contents.to_string());
        self
    }

    fn remembering(self, memory: &Memory) -> Self {
        let text = memory::serialise(memory);
        self.storing(&text)
    }

    fn refusing_writes(mut self) -> Self {
        self.refuse_writes = true;
        self
    }

    fn with_agent(mut self, pid: i32, cwd: &str) -> Self {
        self.records.push(ProcessRecord {
            pid,
            // A real Claude Code path from the machine behind the mechanics document. The
            // basename is a version string, which is why the detector matches on the middle
            // of the path rather than the filename.
            exe_path: Ok("/Users/pmcfadin/.local/share/claude/versions/2.1.233".to_string()),
            cwd: Ok(cwd.to_string()),
        });
        self
    }

    fn with_ledger(mut self, pid: i32, ledger: Result<Resources, ResourcesUnavailable>) -> Self {
        self.ledgers.insert(pid, ledger);
        self
    }

    fn with_namespace(mut self, namespace: &str, path: &str, activity: SystemTime) -> Self {
        self.namespaces.push(namespace.to_string());
        self.namespace_activities
            .insert(namespace.to_string(), activity);
        self.resolutions.insert(
            namespace.to_string(),
            acmon::workspace::NamespaceResolution::Resolved(path.to_string()),
        );
        self
    }

    fn with_workspace(mut self, path: &str, uncommitted: usize) -> Self {
        self.roots
            .insert(path.to_string(), (path.to_string(), false));
        self.facts.insert(
            path.to_string(),
            Ok(VcsFacts {
                root: path.to_string(),
                uncommitted_entries: uncommitted,
                linked_worktree: false,
            }),
        );
        self
    }

    fn with_unreadable_workspace(mut self, path: &str, why: Unreadable) -> Self {
        self.roots
            .insert(path.to_string(), (path.to_string(), false));
        self.facts.insert(path.to_string(), Err(why));
        self
    }

    fn stored(&self) -> Option<String> {
        self.state.borrow().clone()
    }
}

impl World for FakeWorld {
    fn output_width(&self) -> u16 {
        120
    }

    fn process_snapshot(&self) -> Result<ProcessSnapshot, WorldError> {
        Ok(ProcessSnapshot {
            records: self.records.clone(),
            observer_pid: OBSERVER,
        })
    }

    fn resources(&self, pid: i32) -> Result<Resources, ResourcesUnavailable> {
        self.ledgers
            .get(&pid)
            .cloned()
            .unwrap_or_else(|| Ok(measured_ledger()))
    }

    fn recorded_namespaces(&self) -> Result<Vec<String>, WorldError> {
        Ok(self.namespaces.clone())
    }

    fn namespace_activity(&self, namespace: &str) -> Result<SystemTime, ActivityUnavailable> {
        self.namespace_activities
            .get(namespace)
            .copied()
            .ok_or(ActivityUnavailable::NotRecorded)
    }

    fn codex_sessions(&self) -> Result<Vec<CodexSession>, WorldError> {
        Ok(Vec::new())
    }

    fn repository_root(&self, path: &str) -> Option<(String, bool)> {
        self.roots.get(path).cloned()
    }

    fn vcs_facts(&self, path: &str) -> Result<VcsFacts, Unreadable> {
        self.facts
            .get(path)
            .cloned()
            .unwrap_or(Err(Unreadable::NotVersionControlled))
    }

    fn resolve_namespace(&self, namespace: &str) -> acmon::workspace::NamespaceResolution {
        self.resolutions
            .get(namespace)
            .cloned()
            .unwrap_or(acmon::workspace::NamespaceResolution::NoLongerExists)
    }

    fn sweep_for_repositories(&self, _roots: &[String]) -> Sweep {
        Sweep {
            repositories: Vec::new(),
            complete: true,
            directories_visited: 0,
        }
    }

    fn vcs_facts_batch(&self, paths: &[String]) -> Vec<Result<VcsFacts, Unreadable>> {
        self.checked.borrow_mut().extend(paths.iter().cloned());
        paths.iter().map(|p| self.vcs_facts(p)).collect()
    }

    fn read_state(&self) -> StateRead {
        match self.state.borrow().clone() {
            Some(contents) => StateRead::Found(contents),
            None => StateRead::Absent,
        }
    }

    fn write_state(&self, contents: &str) -> Result<(), String> {
        if self.refuse_writes {
            return Err("the state store is read-only".to_string());
        }
        *self.state.borrow_mut() = Some(contents.to_string());
        Ok(())
    }
}

/// A workspace remembered from an earlier run, with no forgetting clock running.
fn remembered(path: &str, first_seen: SystemTime, last_seen: SystemTime) -> RememberedWorkspace {
    RememberedWorkspace {
        path: path.to_string(),
        first_seen,
        last_seen,
        settled_since: None,
    }
}

const STRANDED: &str = "/Users/pmcfadin/projects/presto_testing";

// --- The reason this ticket exists ---

#[test]
fn a_workspace_seen_before_is_still_checked_once_its_session_has_gone() {
    // The whole point. Nothing is running, no transcript names it, and the sweep finds
    // nothing — every observational source is blind to this directory. It is in the report
    // only because an earlier run wrote it down, and it is holding 28 uncommitted files: the
    // real pile measured on this machine, and the same shape as the 27-file loss behind this
    // project.
    let world = FakeWorld::quiet()
        .with_workspace(STRANDED, 28)
        .remembering(&Memory {
            workspaces: vec![remembered(STRANDED, ago(7 * DAY), ago(DAY))],
            sessions: Vec::new(),
        });

    let snapshot =
        collect(&world, now(), &Thresholds::default()).expect("collection should succeed");

    let report = snapshot
        .workspaces
        .iter()
        .find(|w| w.path == STRANDED)
        .unwrap_or_else(|| {
            panic!(
                "a remembered workspace must still be reported; got {:?}",
                snapshot.workspaces
            )
        });
    assert_eq!(
        report.state,
        WorkspaceState::DirtyStranded,
        "28 uncommitted entries with nothing driving them is the at-risk state"
    );
    assert_eq!(report.uncommitted_entries, Some(28));

    assert!(
        world.checked.borrow().iter().any(|p| p == STRANDED),
        "it must be re-QUERIED, not merely re-listed — a remembered path carried through \
         without asking version control about it again would report last week's state as \
         though it were today's; asked about {:?}",
        world.checked.borrow()
    );
}

#[test]
fn a_run_with_no_memory_cannot_see_it_at_all() {
    // The control for the test above. Without the state file the same machine reports
    // nothing, which is what makes memory the load-bearing part rather than an optimisation.
    let world = FakeWorld::quiet().with_workspace(STRANDED, 28);

    let snapshot =
        collect(&world, now(), &Thresholds::default()).expect("collection should succeed");

    assert!(
        !snapshot.workspaces.iter().any(|w| w.path == STRANDED),
        "without history this workspace is unreachable — if this starts passing, the test \
         above has stopped proving anything"
    );
}

// --- First and last seen ---

#[test]
fn first_seen_is_preserved_across_runs_while_last_seen_moves() {
    let first = ago(30 * DAY);
    let world = FakeWorld::quiet()
        .with_workspace(STRANDED, 3)
        .remembering(&Memory {
            workspaces: vec![remembered(STRANDED, first, ago(DAY))],
            sessions: Vec::new(),
        });

    let snapshot =
        collect(&world, now(), &Thresholds::default()).expect("collection should succeed");

    let kept = snapshot
        .remembered
        .memory
        .workspace(STRANDED)
        .expect("still remembered");
    assert_eq!(
        kept.first_seen, first,
        "first_seen records when a workspace entered the picture; moving it forward would \
         make every workspace look newly discovered and destroy the only evidence of how \
         long work has been sitting there"
    );
    assert_eq!(
        kept.last_seen,
        now(),
        "last_seen is when it was last checked"
    );
}

#[test]
fn a_newly_discovered_workspace_is_first_seen_now() {
    let world = FakeWorld::quiet()
        .with_agent(900, STRANDED)
        .with_workspace(STRANDED, 1);

    let snapshot =
        collect(&world, now(), &Thresholds::default()).expect("collection should succeed");

    let entry = snapshot
        .remembered
        .memory
        .workspace(STRANDED)
        .expect("a workspace observed now is remembered");
    assert_eq!(entry.first_seen, now());
    assert_eq!(entry.last_seen, now());
}

// --- Forgetting: only when clean AND quiet, and only after the period ---

#[test]
fn a_workspace_holding_uncommitted_work_is_never_forgotten_however_long_it_sits() {
    // The dangerous inversion. Age is the *reason* to keep watching a stranded workspace, so
    // a retention rule keyed on how long ago it was seen would delete precisely the entries
    // that matter, and the panel would get shorter as the risk got older.
    let ancient = ago(365 * DAY);
    let memory = Memory {
        workspaces: vec![RememberedWorkspace {
            path: STRANDED.to_string(),
            first_seen: ancient,
            last_seen: ancient,
            settled_since: None,
        }],
        sessions: Vec::new(),
    };

    let (kept, forgotten) = memory::forget(memory, now(), DAY);

    assert!(
        forgotten.is_empty(),
        "nothing settled, so nothing to forget"
    );
    assert!(
        kept.workspace(STRANDED).is_some(),
        "a workspace with no forgetting clock running is kept indefinitely"
    );
}

#[test]
fn a_settled_workspace_is_forgotten_only_once_the_period_has_actually_passed() {
    // Both sides of the boundary, because a rule that fires early loses history and one that
    // never fires grows the state file without bound. The period is an input here, not an
    // assertion about elapsed time.
    let retention = 7 * DAY;

    for (settled_for, should_forget) in [
        (retention - Duration::from_secs(1), false),
        (retention, false),
        (retention + Duration::from_secs(1), true),
    ] {
        let memory = Memory {
            workspaces: vec![RememberedWorkspace {
                path: STRANDED.to_string(),
                first_seen: ago(100 * DAY),
                last_seen: now(),
                settled_since: Some(ago(settled_for)),
            }],
            sessions: Vec::new(),
        };

        let (kept, forgotten) = memory::forget(memory, now(), retention);

        assert_eq!(
            !forgotten.is_empty(),
            should_forget,
            "settled for {settled_for:?} against a {retention:?} period"
        );
        assert_eq!(kept.workspaces.is_empty(), should_forget);
    }
}

#[test]
fn the_forgetting_period_is_configurable_and_a_bad_value_is_refused() {
    assert_eq!(
        Thresholds::from_values(None, None, Some("3600"))
            .expect("a readable value")
            .forget,
        Duration::from_secs(3_600)
    );
    assert_eq!(
        Thresholds::from_values(None, None, None)
            .expect("nothing configured")
            .forget,
        memory::DEFAULT_FORGET,
        "an unset period must give exactly the documented default"
    );

    for bad in ["", "seven days", "7d", "-1", "1.5", "  "] {
        assert!(
            Thresholds::from_values(None, None, Some(bad)).is_err(),
            "{bad:?} is not a number of seconds and must be refused rather than quietly \
             replaced by the default, which would prune by a rule the operator thinks they \
             replaced"
        );
    }
}

#[test]
fn going_dirty_again_restarts_the_forgetting_clock() {
    // Otherwise a workspace clean for six days and dirtied on the seventh is dropped the
    // next morning, while holding work — the worst possible moment to stop watching it.
    let previously_settled = Memory {
        workspaces: vec![RememberedWorkspace {
            path: STRANDED.to_string(),
            first_seen: ago(30 * DAY),
            last_seen: ago(DAY),
            settled_since: Some(ago(6 * DAY)),
        }],
        sessions: Vec::new(),
    };

    let dirty_now = [acmon::memory::Sighting::of(
        STRANDED.to_string(),
        &WorkspaceState::DirtyStranded,
        false,
    )];
    let merged = memory::remember(previously_settled, &dirty_now, &[], now());

    assert_eq!(
        merged.workspace(STRANDED).expect("kept").settled_since,
        None,
        "an unsettled workspace has no clock running"
    );
    let (kept, forgotten) = memory::forget(merged, now(), 7 * DAY);
    assert!(forgotten.is_empty());
    assert!(kept.workspace(STRANDED).is_some());
}

#[test]
fn a_clock_already_running_is_not_restarted_by_a_second_clean_sighting() {
    // The other half of the rule. If each clean run reset the clock to `now`, the period
    // would never elapse and nothing would ever be forgotten.
    let settled_five_days_ago = ago(5 * DAY);
    let previous = Memory {
        workspaces: vec![RememberedWorkspace {
            path: STRANDED.to_string(),
            first_seen: ago(30 * DAY),
            last_seen: ago(DAY),
            settled_since: Some(settled_five_days_ago),
        }],
        sessions: Vec::new(),
    };

    let still_clean = [acmon::memory::Sighting::of(
        STRANDED.to_string(),
        &WorkspaceState::Clean,
        false,
    )];
    let merged = memory::remember(previous, &still_clean, &[], now());

    assert_eq!(
        merged.workspace(STRANDED).expect("kept").settled_since,
        Some(settled_five_days_ago),
        "the clock records when it BECAME settled, not when it was last seen to be"
    );
}

#[test]
fn a_clean_workspace_with_something_driving_it_is_not_settled() {
    let sighting = acmon::memory::Sighting::of(STRANDED.to_string(), &WorkspaceState::Clean, true);

    assert!(
        !sighting.settled,
        "clean is only half of it: an agent is working here, so its state is about to change \
         and its clock has no business running"
    );
}

#[test]
fn a_workspace_version_control_would_not_answer_about_is_not_treated_as_clean() {
    // Unknown is not clean. Forgetting on a failed query would let a timeout quietly delete
    // history, and the entry it deleted would be one nobody could vouch for.
    for why in [
        Unreadable::QueryFailed("index.lock exists".to_string()),
        Unreadable::TimedOut,
    ] {
        let sighting = acmon::memory::Sighting::of(
            STRANDED.to_string(),
            &WorkspaceState::Unknown(why.clone()),
            false,
        );
        assert!(
            !sighting.settled,
            "{why:?} means we could not tell, and a workspace we cannot tell about is never \
             forgotten"
        );
    }
}

#[test]
fn a_directory_that_never_had_version_control_settles_and_is_eventually_dropped() {
    // The counterweight to the test above, and what stops the state file growing forever:
    // `NotVersionControlled` and `PathGone` are ANSWERS, not failures. There is nothing to
    // lose in either, so both settle.
    for state in [
        WorkspaceState::Unknown(Unreadable::NotVersionControlled),
        WorkspaceState::Unknown(Unreadable::PathGone),
        WorkspaceState::Clean,
    ] {
        let sighting = acmon::memory::Sighting::of(STRANDED.to_string(), &state, false);
        assert!(sighting.settled, "{state:?} holds nothing worth protecting");
    }
}

#[test]
fn a_remembered_workspace_version_control_refuses_is_kept_and_reported() {
    // The two rules meeting: a failed query is at-risk, and a workspace we could not read is
    // never forgotten. Both have to hold through a whole collection, not just in the pure
    // classification — otherwise an `index.lock` left behind by a crashed agent is enough to
    // quietly retire the entry that was watching that agent's work.
    let world = FakeWorld::quiet()
        .with_unreadable_workspace(STRANDED, Unreadable::QueryFailed("index.lock".to_string()))
        .remembering(&Memory {
            workspaces: vec![RememberedWorkspace {
                path: STRANDED.to_string(),
                first_seen: ago(60 * DAY),
                last_seen: ago(DAY),
                // Settled long ago, and about to stop being so.
                settled_since: Some(ago(50 * DAY)),
            }],
            sessions: Vec::new(),
        });

    let snapshot = collect(&world, now(), &Thresholds::default()).expect("collection");

    let report = snapshot
        .workspaces
        .iter()
        .find(|w| w.path == STRANDED)
        .expect("still reported");
    assert!(
        report.state.at_risk(),
        "a workspace version control would not answer about might be holding work"
    );
    assert_eq!(
        report.uncommitted_entries, None,
        "and its count is absent rather than 0, which would read as clean"
    );
    assert!(
        snapshot.remembered.forgotten.is_empty(),
        "the clock has to have been stopped by the failed read, not left running from before"
    );
    assert!(
        snapshot.remembered.memory.workspace(STRANDED).is_some(),
        "so the next run checks it again"
    );
}

#[test]
fn what_was_forgotten_is_reported_rather_than_silently_dropped() {
    let world = FakeWorld::quiet()
        .with_workspace(STRANDED, 0)
        .remembering(&Memory {
            workspaces: vec![RememberedWorkspace {
                path: STRANDED.to_string(),
                first_seen: ago(100 * DAY),
                last_seen: ago(DAY),
                settled_since: Some(ago(90 * DAY)),
            }],
            sessions: Vec::new(),
        });

    let snapshot =
        collect(&world, now(), &Thresholds::default()).expect("collection should succeed");

    assert_eq!(
        snapshot.remembered.forgotten.len(),
        1,
        "a workspace settled for ninety days against a seven-day period is dropped"
    );
    assert_eq!(snapshot.remembered.forgotten[0].path, STRANDED);
    assert!(
        snapshot.remembered.memory.workspace(STRANDED).is_none(),
        "and it is really gone from what the next run will start with"
    );
}

#[test]
fn an_unobserved_workspace_is_kept_exactly_as_it_was() {
    // Reachable whenever a version-control query does not come back: the workspace produces
    // no sighting, and treating a missing sighting as a clean one would let a timeout start
    // the forgetting clock on a workspace nobody looked at.
    let previous = Memory {
        workspaces: vec![remembered(STRANDED, ago(30 * DAY), ago(2 * DAY))],
        sessions: Vec::new(),
    };

    let merged = memory::remember(previous.clone(), &[], &[], now());

    assert_eq!(
        merged.workspaces, previous.workspaces,
        "not seen this run means not touched this run — including last_seen, which must not \
         advance for a workspace that was never checked"
    );
}

// --- A session's lifetime totals outliving its process ---

/// The namespace and workspace of a session that will exit between the two runs below.
const WORKED_IN: &str = "/Users/pmcfadin/projects/agentic_coding_monitor";
const NAMESPACE: &str = "-Users-pmcfadin-projects-agentic-coding-monitor";

#[test]
fn a_sessions_totals_survive_its_process() {
    // Almost all of an agent's cost is in its children, and only the process that reaped
    // them can report the total — so once it exits, that figure exists nowhere on the machine
    // except in what an earlier run wrote down.
    let first_run = FakeWorld::quiet()
        .with_agent(900, WORKED_IN)
        .with_namespace(NAMESPACE, WORKED_IN, ago(Duration::from_secs(30)))
        .with_workspace(WORKED_IN, 2);

    let earlier = collect(&first_run, ago(DAY), &Thresholds::default()).expect("first run");
    assert!(
        earlier
            .sessions
            .iter()
            .any(|s| s.resources.as_ref() == Ok(&measured_ledger())),
        "the first run must actually read the ledger, or the second proves nothing"
    );

    // The same machine a day later, with the agent gone. Its transcript is still recent
    // enough to be discovered, which is how the session is recognised at all.
    let second_run = FakeWorld::quiet()
        .with_namespace(NAMESPACE, WORKED_IN, ago(DAY))
        .with_workspace(WORKED_IN, 2)
        .storing(&first_run.stored().expect("the first run stored its state"));

    let later = collect(&second_run, now(), &Thresholds::default()).expect("second run");

    let session = later
        .sessions
        .iter()
        .find(|s| matches!(&s.identity, Identity::Transcript { recorded_as } if recorded_as == NAMESPACE))
        .unwrap_or_else(|| panic!("the exited session should still be listed; got {:?}", later.sessions));

    assert_eq!(
        session.resources,
        Err(ResourcesUnavailable::ProcessExited),
        "the live reading is honestly absent — the process is gone"
    );
    let reading = session
        .last_reading
        .as_ref()
        .expect("but the last reading taken before it exited is remembered");
    assert_eq!(reading.resources, measured_ledger());
    assert_eq!(
        reading.taken_at,
        ago(DAY),
        "stamped when it was READ, not when it was recalled — restamping it would present a \
         day-old total as a current one"
    );
}

#[test]
fn a_remembered_reading_never_shadows_a_live_one() {
    let world = FakeWorld::quiet()
        .with_agent(900, WORKED_IN)
        .with_namespace(NAMESPACE, WORKED_IN, ago(Duration::from_secs(30)))
        .with_workspace(WORKED_IN, 0)
        .remembering(&Memory {
            workspaces: Vec::new(),
            sessions: vec![RememberedSession {
                cli: "claude".to_string(),
                recorded_as: NAMESPACE.to_string(),
                first_seen: ago(30 * DAY),
                last_seen: ago(DAY),
                last_reading: Some(Reading {
                    resources: measured_ledger(),
                    taken_at: ago(DAY),
                }),
            }],
        });

    let snapshot = collect(&world, now(), &Thresholds::default()).expect("collection");

    let session = snapshot
        .sessions
        .iter()
        .find(|s| matches!(s.identity, Identity::Process { pid: 900 }))
        .expect("the live session is listed");
    assert!(session.resources.is_ok(), "it was read successfully now");
    assert_eq!(
        session.last_reading, None,
        "a live figure and a remembered one must never both be present: a caller would have \
         to guess which is current, and rendering would show both"
    );
}

#[test]
fn a_reused_pid_cannot_inherit_another_sessions_totals() {
    // The kernel reuses pids, and this is the one place in the crate where something has to
    // be recognised across time rather than within one enumeration. A remembered ledger keyed
    // on the pid would attach one session's 9-hour child total to an unrelated process, and
    // the row would look entirely ordinary.
    const OTHER: &str = "/Users/pmcfadin/projects/WorkforceOS";
    const OTHER_NAMESPACE: &str = "-Users-pmcfadin-projects-WorkforceOS";

    let world = FakeWorld::quiet()
        .with_agent(900, OTHER)
        .with_ledger(900, Err(ResourcesUnavailable::ProcessExited))
        .with_namespace(OTHER_NAMESPACE, OTHER, ago(Duration::from_secs(30)))
        .with_workspace(OTHER, 0)
        .remembering(&Memory {
            workspaces: Vec::new(),
            // Same pid last time, a different session.
            sessions: vec![RememberedSession {
                cli: "claude".to_string(),
                recorded_as: NAMESPACE.to_string(),
                first_seen: ago(30 * DAY),
                last_seen: ago(DAY),
                last_reading: Some(Reading {
                    resources: measured_ledger(),
                    taken_at: ago(DAY),
                }),
            }],
        });

    let snapshot = collect(&world, now(), &Thresholds::default()).expect("collection");

    let session = snapshot
        .sessions
        .iter()
        .find(|s| matches!(s.identity, Identity::Process { pid: 900 }))
        .expect("the session is listed");
    assert_eq!(
        session.last_reading, None,
        "pid 900 is a different session now, so it inherits nothing"
    );
}

#[test]
fn a_reading_that_could_not_be_taken_carries_the_previous_one_forward_unchanged() {
    // Otherwise a single failed read erases a total that can never be re-measured.
    let taken_at = ago(3 * DAY);
    let previous = Memory {
        workspaces: Vec::new(),
        sessions: vec![RememberedSession {
            cli: "claude".to_string(),
            recorded_as: NAMESPACE.to_string(),
            first_seen: ago(30 * DAY),
            last_seen: ago(DAY),
            last_reading: Some(Reading {
                resources: measured_ledger(),
                taken_at,
            }),
        }],
    };

    let session = acmon::Session {
        identity: Identity::Transcript {
            recorded_as: NAMESPACE.to_string(),
        },
        cli: "claude".to_string(),
        resources: Err(ResourcesUnavailable::ProcessExited),
        last_reading: None,
        workspace: Ok(acmon::workspace::Workspace {
            path: WORKED_IN.to_string(),
            namespace: Ok(NAMESPACE.to_string()),
        }),
        liveness: acmon::liveness::classify(
            &acmon::liveness::Observation {
                silence: Some(DAY),
                process_resident: false,
                work_running_in_workspace: false,
                snapshot_trustworthy: true,
            },
            &Thresholds::default(),
        ),
    };

    let merged = memory::remember(previous, &[], &[session], now());

    let carried = merged
        .reading_for("claude", NAMESPACE)
        .expect("the reading survives a run that could not take a new one");
    assert_eq!(carried.taken_at, taken_at, "with its original timestamp");
    assert_eq!(carried.resources, measured_ledger());
}

// --- Reading the file: absent, corrupt, or from another version ---

#[test]
fn a_machine_with_no_state_file_starts_empty_and_says_nothing_about_it() {
    let world = FakeWorld::quiet();

    let snapshot = collect(&world, now(), &Thresholds::default()).expect("collection");

    assert_eq!(
        snapshot.remembered.unusable, None,
        "a first run has nothing to remember, which is an answer and not a degradation — \
         warning about it would put a scary line on every new machine"
    );
    assert!(snapshot.remembered.persisted.is_ok());
}

#[test]
fn a_corrupt_state_file_degrades_to_empty_with_a_stated_reason() {
    // Three shapes of damage, all reachable: a truncated file from a crash mid-write on some
    // other filesystem, a hand-edit, and a file that is valid JSON of the wrong shape.
    for damaged in [
        r#"{"version": 1, "workspaces": [{"path": "/Users/pmcfa"#,
        "not json at all",
        r#"{"version": 1, "workspaces": "should be a list", "sessions": []}"#,
    ] {
        let world = FakeWorld::quiet().storing(damaged);

        let snapshot = collect(&world, now(), &Thresholds::default())
            .expect("a damaged state file must not stop the collection");

        let Some(Degraded::Unparsable(why)) = &snapshot.remembered.unusable else {
            panic!(
                "{damaged:?} must degrade with a stated reason, got {:?}",
                snapshot.remembered.unusable
            );
        };
        assert!(
            !why.trim().is_empty(),
            "the reason has to say something — a blank one is the same as none"
        );
    }
}

#[test]
fn a_state_file_that_cannot_be_read_is_not_mistaken_for_an_absent_one() {
    // The two collapse into the same empty memory, so only the stated reason distinguishes
    // them — and the difference matters: one means "nothing to remember", the other means
    // "history exists and this run is not using it".
    struct Unreadable;
    impl World for Unreadable {
        fn output_width(&self) -> u16 {
            120
        }
        fn process_snapshot(&self) -> Result<ProcessSnapshot, WorldError> {
            Ok(ProcessSnapshot {
                records: vec![ProcessRecord {
                    pid: OBSERVER,
                    exe_path: Ok("/usr/bin/acmon".to_string()),
                    cwd: Ok("/Users/pmcfadin".to_string()),
                }],
                observer_pid: OBSERVER,
            })
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
        fn repository_root(&self, _path: &str) -> Option<(String, bool)> {
            None
        }
        fn vcs_facts(&self, _path: &str) -> Result<VcsFacts, acmon::vcs::Unreadable> {
            Err(acmon::vcs::Unreadable::NotVersionControlled)
        }
        fn resolve_namespace(&self, _n: &str) -> acmon::workspace::NamespaceResolution {
            acmon::workspace::NamespaceResolution::NoLongerExists
        }
        fn sweep_for_repositories(&self, _roots: &[String]) -> Sweep {
            Sweep {
                repositories: Vec::new(),
                complete: true,
                directories_visited: 0,
            }
        }
        fn read_state(&self) -> StateRead {
            StateRead::Unreadable("Permission denied (os error 13)".to_string())
        }
        fn write_state(&self, _contents: &str) -> Result<(), String> {
            Ok(())
        }
    }

    let snapshot = collect(&Unreadable, now(), &Thresholds::default()).expect("collection");

    assert!(
        matches!(
            &snapshot.remembered.unusable,
            Some(Degraded::Unreadable(why)) if why.contains("Permission denied")
        ),
        "the filesystem's own words must survive to the report, got {:?}",
        snapshot.remembered.unusable
    );
}

#[test]
fn a_state_file_from_a_newer_acmon_is_refused_rather_than_read_in_part() {
    // Parsing what we recognise and dropping the rest would silently destroy the newer
    // build's state the next time this one wrote the file.
    let from_the_future = r#"{"version": 99, "workspaces": [], "sessions": []}"#;

    let (memory, degraded) = memory::parse(from_the_future);

    assert!(memory.is_empty());
    assert!(
        matches!(
            degraded,
            Some(Degraded::UnknownVersion {
                found: 99,
                understood: _
            })
        ),
        "got {degraded:?}"
    );
}

#[test]
fn a_run_that_could_not_store_its_state_says_so() {
    let world = FakeWorld::quiet().refusing_writes();

    let snapshot = collect(&world, now(), &Thresholds::default())
        .expect("failing to store state must not fail the collection");

    assert!(
        snapshot.remembered.persisted.is_err(),
        "a run that collected perfectly and stored nothing looks identical to one that \
         worked, right up until the next run starts blind"
    );
}

// --- The file itself ---

#[test]
fn state_survives_a_round_trip_through_the_file_format() {
    let original = Memory {
        workspaces: vec![RememberedWorkspace {
            path: STRANDED.to_string(),
            first_seen: ago(30 * DAY),
            last_seen: now(),
            settled_since: Some(ago(2 * DAY)),
        }],
        sessions: vec![RememberedSession {
            cli: "claude".to_string(),
            recorded_as: NAMESPACE.to_string(),
            first_seen: ago(10 * DAY),
            last_seen: now(),
            last_reading: Some(Reading {
                resources: Resources {
                    source: ResourceSource::Ps,
                    own_cpu: Ok(Duration::from_secs(637)),
                    // A reason, not a figure. It has to come back as the same reason: a
                    // remembered `ps-blind` that reappeared as `0` would report a session
                    // whose children consumed nothing.
                    children_cpu: Err(Unmeasured::NotReportedBy(ResourceSource::Ps)),
                    current_memory: Ok(419_000_000),
                    peak_memory: Err(Unmeasured::NotReportedBy(ResourceSource::Ps)),
                    bytes_written: Err(Unmeasured::PermissionDenied),
                },
                taken_at: ago(Duration::from_secs(3_600)),
            }),
        }],
    };

    let (parsed, degraded) = memory::parse(&memory::serialise(&original));

    assert_eq!(degraded, None);
    assert_eq!(parsed, original);
}

#[test]
fn the_state_file_is_readable_by_a_human() {
    // Not decoration. Every figure this tool prints is meant to be checkable by hand, and the
    // remembered ones are the only figures that cannot be re-derived by looking at the
    // machine — so if the file is unreadable, they are unverifiable.
    let text = memory::serialise(&Memory {
        workspaces: vec![RememberedWorkspace {
            path: STRANDED.to_string(),
            first_seen: SystemTime::UNIX_EPOCH + Duration::from_secs(1_786_987_902),
            last_seen: SystemTime::UNIX_EPOCH + Duration::from_secs(1_786_987_902),
            settled_since: None,
        }],
        sessions: Vec::new(),
    });

    assert!(
        text.contains("2026-08-17T17:31:42Z"),
        "timestamps must be written as dates, not epoch integers; got:\n{text}"
    );
    assert!(
        text.contains(STRANDED),
        "and paths as themselves; got:\n{text}"
    );
    assert!(
        text.lines().count() > 3,
        "pretty-printed, so a human can find a line in it; got:\n{text}"
    );
}

// --- The real file on disk ---
//
// These drive `RealWorld` against a temporary directory rather than a fake. The atomicity
// requirement is a property of `rename(2)`, not of any logic above it, so a fake cannot
// establish it — and it is the one part of this seam whose failure is silent: a torn file
// does not fail to parse, it parses as FEWER remembered workspaces.

/// A directory that is this test's alone, removed on the way out.
fn scratch(name: &str) -> std::path::PathBuf {
    let directory = std::env::temp_dir().join(format!("acmon-seam8-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&directory);
    directory
}

/// A state file big enough that a partial write would be observable — a torn 200-byte file is
/// easy to miss, a torn 100 kB one is not.
fn a_large_memory(entries: usize) -> Memory {
    Memory {
        workspaces: (0..entries)
            .map(|n| RememberedWorkspace {
                path: format!("/Users/pmcfadin/projects/repository-number-{n:04}"),
                first_seen: ago(30 * DAY),
                last_seen: now(),
                settled_since: Some(ago(Duration::from_secs(n as u64))),
            })
            .collect(),
        sessions: Vec::new(),
    }
}

#[test]
fn the_state_file_is_created_along_with_the_directory_it_lives_in() {
    let directory = scratch("creates");
    let path = directory.join("state.json");
    let world = acmon::RealWorld::with_state_file(&path);

    assert_eq!(
        world.read_state(),
        StateRead::Absent,
        "a directory that does not exist holds no state, which is an answer rather than a \
         failure — treating it as one would make every first run look broken"
    );

    world
        .write_state(&memory::serialise(&a_large_memory(3)))
        .expect("acmon must be able to create its own state directory");

    let StateRead::Found(text) = world.read_state() else {
        panic!("what was written must come back");
    };
    let (parsed, degraded) = memory::parse(&text);
    assert_eq!(degraded, None);
    assert_eq!(parsed.workspaces.len(), 3);

    let _ = std::fs::remove_dir_all(&directory);
}

#[test]
fn a_concurrent_reader_never_observes_a_half_written_state_file() {
    // The requirement, stated as the failure it prevents: two acmon runs at once is the
    // expected case, because the tool is meant to be left open in a pane while another run is
    // invoked by hand. If the second could read a truncated file it would silently report a
    // shorter at-risk list than the machine has, and nothing about the output would say so.
    let directory = scratch("atomic");
    let path = directory.join("state.json");
    std::fs::create_dir_all(&directory).expect("scratch directory");

    let writer_world = acmon::RealWorld::with_state_file(&path);
    // Two sizes, alternating, so a stale read is distinguishable from a torn one: a reader
    // that never saw both entry counts would prove nothing about the writes overlapping.
    let small = memory::serialise(&a_large_memory(40));
    let large = memory::serialise(&a_large_memory(400));
    writer_world
        .write_state(&small)
        .expect("the first write must land");

    let stop = std::sync::atomic::AtomicBool::new(false);
    let reads = std::sync::atomic::AtomicUsize::new(0);
    let sizes_seen = std::sync::Mutex::new(std::collections::HashSet::new());

    std::thread::scope(|scope| {
        let reader = scope.spawn(|| {
            let reader_world = acmon::RealWorld::with_state_file(&path);
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                match reader_world.read_state() {
                    StateRead::Found(text) => {
                        let (memory, degraded) = memory::parse(&text);
                        assert_eq!(
                            degraded,
                            None,
                            "a reader observed a state file mid-write; it parsed as {:?} \
                             workspaces",
                            memory.workspaces.len()
                        );
                        sizes_seen
                            .lock()
                            .expect("no panic held this lock")
                            .insert(memory.workspaces.len());
                        reads.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                    // The file exists throughout — it is replaced, never removed — so
                    // neither of these may happen.
                    other => panic!("the state file must never vanish during a replace: {other:?}"),
                }
            }
        });

        for round in 0..300 {
            let contents = if round % 2 == 0 { &small } else { &large };
            writer_world.write_state(contents).expect("write");
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        reader.join().expect("the reader must not have panicked");
    });

    assert!(
        reads.load(std::sync::atomic::Ordering::Relaxed) > 0,
        "the reader never read anything, so it proved nothing — assert the observation \
         happened before believing what it says"
    );
    let seen = sizes_seen.lock().expect("lock");
    assert!(
        seen.contains(&40) && seen.contains(&400),
        "the reader must have seen both whole versions, or the writes and reads did not \
         actually overlap and this test is not exercising the replace; saw {seen:?}"
    );

    let leftovers: Vec<_> = std::fs::read_dir(&directory)
        .expect("listable")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .filter(|name| name != "state.json")
        .collect();
    assert!(
        leftovers.is_empty(),
        "a replace must leave nothing behind; 300 rounds left {leftovers:?}"
    );

    let _ = std::fs::remove_dir_all(&directory);
}

#[test]
fn a_state_file_that_cannot_be_written_reports_where_and_why() {
    // A path whose parent cannot be created, because a FILE stands where the directory would
    // have to go. The message has to name the path: "could not store state" alone leaves
    // nobody anywhere to look.
    let directory = scratch("blocked");
    std::fs::create_dir_all(&directory).expect("scratch directory");
    let blocking_file = directory.join("not-a-directory");
    std::fs::write(&blocking_file, b"in the way").expect("write the obstacle");

    let world = acmon::RealWorld::with_state_file(blocking_file.join("state.json"));

    let refused = world
        .write_state("{}")
        .expect_err("writing under a plain file cannot succeed");
    assert!(
        refused.contains("not-a-directory"),
        "the reason must name the path it failed at; got {refused:?}"
    );

    let _ = std::fs::remove_dir_all(&directory);
}
