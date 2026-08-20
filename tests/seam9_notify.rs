//! Seam 9 — notifying when sessions wait or work strands.
//!
//! The failure this seam exists to prevent: a notifier that backgrounded its request and
//! always reported success made a dead channel indistinguishable from a quiet machine. An
//! exhausted quota swallowed a full day of alerts silently.

use std::cell::RefCell;
use std::collections::HashMap;
use std::time::{Duration, SystemTime};

use acmon::liveness::{State, Thresholds};
use acmon::memory::Memory;
use acmon::notify::{AnnouncedSession, AnnouncedSessionState, AnnouncementRecord};
use acmon::vcs::{Unreadable, VcsFacts, WorkspaceState};
use acmon::world::{
    ActivityUnavailable, CodexSession, NotifyConfig, NotifyOutcome, ProcessRecord, ProcessSnapshot,
    ResourceSource, Resources, ResourcesUnavailable, StateRead, Sweep, World, WorldError,
};
use acmon::{collect, Identity};

fn now() -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(1_787_000_000)
}

fn ago(duration: Duration) -> SystemTime {
    now() - duration
}

const _DAY: Duration = Duration::from_secs(86_400);
const OBSERVER: i32 = 4_242;

/// An agent process that will be recognised as one.
fn agent_process(pid: i32, cwd: &str) -> ProcessRecord {
    ProcessRecord {
        pid,
        exe_path: Ok("/Users/pmcfadin/.local/share/claude/versions/2.1.233".to_string()),
        cwd: Ok(cwd.to_string()),
    }
}

fn observer_process() -> ProcessRecord {
    ProcessRecord {
        pid: OBSERVER,
        exe_path: Ok("/usr/bin/acmon".to_string()),
        cwd: Ok("/Users/pmcfadin".to_string()),
    }
}

fn measured_ledger() -> Resources {
    Resources {
        source: ResourceSource::Rusage,
        own_cpu: Ok(Duration::from_secs(100)),
        children_cpu: Ok(Duration::from_secs(500)),
        current_memory: Ok(400_000_000),
        peak_memory: Ok(500_000_000),
        bytes_written: Ok(100_000_000),
    }
}

/// Which channel a delivery went to, recorded in the order the channels were asked.
///
/// The order is the observable that separates "one request per alert" from "one batch per
/// channel". Under the per-alert loop this seam started with, two configured channels
/// interleaved — local, remote, local, remote — because each announcement was carried through
/// both before the next was looked at. A batched run asks one channel about everything it has
/// to say and then the other, which is what allows a channel to overlap the requests inside
/// its own batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Channel {
    Local,
    Remote,
}

struct FakeWorld {
    records: Vec<ProcessRecord>,
    ledgers: HashMap<i32, Result<Resources, ResourcesUnavailable>>,
    namespaces: Vec<String>,
    namespace_activities: HashMap<String, SystemTime>,
    resolutions: HashMap<String, acmon::workspace::NamespaceResolution>,
    facts: HashMap<String, Result<VcsFacts, Unreadable>>,
    roots: HashMap<String, (String, bool)>,
    state: RefCell<Option<String>>,
    config: NotifyConfig,
    /// Log of local notifications: (command, payload)
    local_log: RefCell<Vec<(String, String)>>,
    /// Log of remote notifications: (url, payload)
    remote_log: RefCell<Vec<(String, String)>>,
    /// Outcome to return for local notifications
    local_outcome: NotifyOutcome,
    /// Outcome to return for remote notifications
    remote_outcome: NotifyOutcome,
    /// Which channels were asked, in the order they were asked.
    call_order: RefCell<Vec<Channel>>,
    /// An outcome chosen from the payload itself, when set, in place of `local_outcome`.
    ///
    /// This is what makes per-alert verification testable under batching: a channel that
    /// delivers some of a run's alerts and refuses others must retire exactly the ones that
    /// arrived, and a batch whose outcomes came back against the wrong payloads would retire
    /// the wrong alert while every tally still added up.
    local_outcome_by_payload: Option<fn(&str) -> NotifyOutcome>,
    /// How long each local delivery takes, so the cost of the alerting step is observable.
    local_delay: Duration,
    /// What sweep_for_repositories returns
    sweep: Option<Sweep>,
}

impl FakeWorld {
    fn quiet() -> Self {
        FakeWorld {
            records: vec![observer_process()],
            ledgers: HashMap::new(),
            namespaces: Vec::new(),
            namespace_activities: HashMap::new(),
            resolutions: HashMap::new(),
            facts: HashMap::new(),
            roots: HashMap::new(),
            state: RefCell::new(None),
            config: NotifyConfig::none(),
            local_log: RefCell::new(Vec::new()),
            remote_log: RefCell::new(Vec::new()),
            local_outcome: NotifyOutcome::NoChannelConfigured,
            remote_outcome: NotifyOutcome::NoChannelConfigured,
            call_order: RefCell::new(Vec::new()),
            local_outcome_by_payload: None,
            local_delay: Duration::ZERO,
            sweep: None,
        }
    }

    fn with_agent(mut self, pid: i32, cwd: &str) -> Self {
        self.records.push(agent_process(pid, cwd));
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
        self.discover(path)
    }

    fn with_unreadable_workspace(mut self, path: &str, why: Unreadable) -> Self {
        self.roots
            .insert(path.to_string(), (path.to_string(), false));
        self.facts.insert(path.to_string(), Err(why));
        self.discover(path)
    }

    /// Make the sweep return this workspace, alongside any already added.
    ///
    /// Accumulating rather than replacing, because the cost shape this seam now has to hold to
    /// only shows up with several notable states at once — fourteen is the steady state on the
    /// machine behind the ticket, and one workspace can never demonstrate it.
    fn discover(mut self, path: &str) -> Self {
        let mut sweep = self.sweep.take().unwrap_or_else(|| Sweep {
            repositories: Vec::new(),
            complete: true,
            directories_visited: 0,
        });
        sweep.repositories.push((path.to_string(), false));
        sweep.directories_visited += 1;
        self.sweep = Some(sweep);
        self
    }

    /// A local channel whose answer depends on which alert it was given.
    fn with_local_channel_answering(mut self, decide: fn(&str) -> NotifyOutcome) -> Self {
        self.config.local_command = Some("echo".to_string());
        self.local_outcome_by_payload = Some(decide);
        self
    }

    /// A local channel that takes a stated amount of time per delivery.
    fn with_local_delay(mut self, delay: Duration) -> Self {
        self.local_delay = delay;
        self
    }

    fn with_local_channel(mut self, command: &str, outcome: NotifyOutcome) -> Self {
        self.config.local_command = Some(command.to_string());
        self.local_outcome = outcome;
        self
    }

    fn with_remote_channel(mut self, url: &str, outcome: NotifyOutcome) -> Self {
        self.config.remote_url = Some(url.to_string());
        self.remote_outcome = outcome;
        self
    }

    fn remembering(self, memory: &Memory) -> Self {
        let text = acmon::memory::serialise(memory);
        *self.state.borrow_mut() = Some(text);
        self
    }

    fn local_payloads(&self) -> Vec<String> {
        self.local_log
            .borrow()
            .iter()
            .map(|(_, payload)| payload.clone())
            .collect()
    }

    fn remote_payloads(&self) -> Vec<String> {
        self.remote_log
            .borrow()
            .iter()
            .map(|(_, payload)| payload.clone())
            .collect()
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
        self.sweep.clone().unwrap_or_else(|| Sweep {
            repositories: Vec::new(),
            complete: true,
            directories_visited: 0,
        })
    }

    fn read_state(&self) -> StateRead {
        match self.state.borrow().clone() {
            Some(contents) => StateRead::Found(contents),
            None => StateRead::Absent,
        }
    }

    fn write_state(&self, contents: &str) -> Result<(), String> {
        *self.state.borrow_mut() = Some(contents.to_string());
        Ok(())
    }

    fn read_notify_config(&self) -> NotifyConfig {
        self.config.clone()
    }

    fn notify_local(&self, command: &str, payload: &str) -> NotifyOutcome {
        self.local_log
            .borrow_mut()
            .push((command.to_string(), payload.to_string()));
        self.call_order.borrow_mut().push(Channel::Local);
        if !self.local_delay.is_zero() {
            std::thread::sleep(self.local_delay);
        }
        match self.local_outcome_by_payload {
            Some(decide) => decide(payload),
            None => self.local_outcome.clone(),
        }
    }

    fn notify_remote(&self, url: &str, payload: &str) -> NotifyOutcome {
        self.remote_log
            .borrow_mut()
            .push((url.to_string(), payload.to_string()));
        self.call_order.borrow_mut().push(Channel::Remote);
        self.remote_outcome.clone()
    }
}

const WORKSPACE: &str = "/Users/pmcfadin/projects/testing";
const NAMESPACE: &str = "-Users-pmcfadin-projects-testing";

// --- Core notification rules ---

#[test]
fn a_session_entering_waiting_state_triggers_a_notification() {
    // A session is ACTIVE, then WAITING. The first WAITING announces.
    let world = FakeWorld::quiet()
        .with_agent(900, WORKSPACE)
        .with_namespace(NAMESPACE, WORKSPACE, ago(Duration::from_secs(15 * 60))) // 15 minutes ago
        .with_workspace(WORKSPACE, 0)
        .with_local_channel("echo", NotifyOutcome::Delivered);

    let snapshot = collect(&world, now(), &Thresholds::default()).expect("collection");

    let session = snapshot
        .sessions
        .iter()
        .find(|s| matches!(s.identity, Identity::Process { pid: 900 }))
        .expect("session found");
    assert_eq!(
        session.liveness.state,
        State::Waiting,
        "15 minutes of silence with a resident process is WAITING"
    );

    let payloads = world.local_payloads();
    assert_eq!(
        payloads.len(),
        1,
        "one notification for the session entering WAITING"
    );
    assert!(
        payloads[0].contains("WAITING"),
        "payload mentions the state; got {:?}",
        payloads[0]
    );
    assert!(
        payloads[0].contains("claude"),
        "payload mentions the CLI; got {:?}",
        payloads[0]
    );
}

#[test]
fn a_workspace_becoming_stranded_triggers_a_notification() {
    let world = FakeWorld::quiet()
        .with_workspace(WORKSPACE, 5)
        .with_local_channel("echo", NotifyOutcome::Delivered);

    let snapshot = collect(&world, now(), &Thresholds::default()).expect("collection");

    let workspace = snapshot
        .workspaces
        .iter()
        .find(|w| w.path == WORKSPACE)
        .expect("workspace found");
    assert_eq!(
        workspace.state,
        WorkspaceState::DirtyStranded,
        "5 uncommitted entries with no session driving is DIRTY-STRANDED"
    );

    let payloads = world.local_payloads();
    assert_eq!(
        payloads.len(),
        1,
        "one notification for the workspace becoming stranded"
    );
    assert!(
        payloads[0].contains("STRANDED"),
        "payload mentions the state; got {:?}",
        payloads[0]
    );
    assert!(
        payloads[0].contains(WORKSPACE),
        "payload mentions the workspace path; got {:?}",
        payloads[0]
    );
}

#[test]
fn an_unchanged_set_of_notable_states_does_not_re_notify() {
    // The control for the re-entry test below. A session that stays WAITING across two runs
    // announces once, not twice.
    let previous = Memory {
        workspaces: Vec::new(),
        sessions: Vec::new(),
        announcements: AnnouncementRecord {
            sessions: vec![AnnouncedSession {
                cli: "claude".to_string(),
                recorded_as: NAMESPACE.to_string(),
                state: AnnouncedSessionState::Waiting,
            }],
            workspaces: Vec::new(),
        },
    };

    let world = FakeWorld::quiet()
        .with_agent(900, WORKSPACE)
        .with_namespace(NAMESPACE, WORKSPACE, ago(Duration::from_secs(15 * 60)))
        .with_workspace(WORKSPACE, 0)
        .with_local_channel("echo", NotifyOutcome::Delivered)
        .remembering(&previous);

    let snapshot = collect(&world, now(), &Thresholds::default()).expect("collection");

    let session = snapshot
        .sessions
        .iter()
        .find(|s| matches!(s.identity, Identity::Process { pid: 900 }))
        .expect("session");
    assert_eq!(session.liveness.state, State::Waiting);

    let payloads = world.local_payloads();
    assert!(
        payloads.is_empty(),
        "the session was already announced as WAITING, so it is not re-announced; got {:?}",
        payloads
    );
}

#[test]
fn leaving_and_re_entering_a_notable_state_announces_again() {
    // A session was WAITING and announced. It became ACTIVE (the transcript changed). Now it
    // is WAITING again. This second WAITING announces.
    let previous = Memory {
        workspaces: Vec::new(),
        sessions: Vec::new(),
        announcements: AnnouncementRecord {
            sessions: vec![AnnouncedSession {
                cli: "claude".to_string(),
                recorded_as: NAMESPACE.to_string(),
                state: AnnouncedSessionState::Waiting,
            }],
            workspaces: Vec::new(),
        },
    };

    // The session is WAITING again, but we know it left WAITING in between (otherwise the
    // unchanged-state test above would fail). The collection logic doesn't directly observe
    // the transition, but the announcement record being different from the current state
    // implies it left and came back.
    //
    // To make this test realistic: we're simulating that between runs, the session became
    // ACTIVE (dropped from announcements), and now it's WAITING again. Since the previous
    // memory has it announced as WAITING, we need to simulate the intermediate state by
    // removing it from announcements first.
    let intermediate = Memory {
        announcements: AnnouncementRecord {
            sessions: Vec::new(), // Session left WAITING, so dropped from record
            workspaces: Vec::new(),
        },
        ..previous
    };

    let world = FakeWorld::quiet()
        .with_agent(900, WORKSPACE)
        .with_namespace(NAMESPACE, WORKSPACE, ago(Duration::from_secs(15 * 60)))
        .with_workspace(WORKSPACE, 0)
        .with_local_channel("echo", NotifyOutcome::Delivered)
        .remembering(&intermediate);

    let snapshot = collect(&world, now(), &Thresholds::default()).expect("collection");

    let session = snapshot
        .sessions
        .iter()
        .find(|s| matches!(s.identity, Identity::Process { pid: 900 }))
        .expect("session");
    assert_eq!(session.liveness.state, State::Waiting);

    let payloads = world.local_payloads();
    assert_eq!(
        payloads.len(),
        1,
        "the session re-entered WAITING after leaving it, so it announces again"
    );
}

// --- Delivery verification ---

#[test]
fn an_undelivered_alert_is_not_recorded_as_sent() {
    // A delivery fails. The announcement record must not be updated, so the next run tries again.
    let world = FakeWorld::quiet()
        .with_workspace(WORKSPACE, 3)
        .with_local_channel("echo", NotifyOutcome::Failed("command failed".to_string()));

    let snapshot = collect(&world, now(), &Thresholds::default()).expect("collection");

    assert!(
        world.local_payloads().len() == 1,
        "the delivery was attempted"
    );
    assert!(
        snapshot.remembered.notify_health.has_failures(),
        "the failure was recorded"
    );

    // The workspace should NOT be in the announcement record
    assert!(
        snapshot
            .remembered
            .memory
            .announcements
            .workspaces
            .is_empty(),
        "a failed delivery must not update the announcement record, so the next run tries again"
    );
}

#[test]
fn a_failed_alert_is_re_announced_on_the_following_run() {
    // Run 1: delivery fails, not recorded.
    let world1 = FakeWorld::quiet()
        .with_workspace(WORKSPACE, 3)
        .with_local_channel("echo", NotifyOutcome::Failed("command failed".to_string()));

    let snapshot1 = collect(&world1, now(), &Thresholds::default()).expect("first run");
    assert_eq!(world1.local_payloads().len(), 1, "first attempt");

    // Run 2: same state, delivery succeeds this time.
    let world2 = FakeWorld::quiet()
        .with_workspace(WORKSPACE, 3)
        .with_local_channel("echo", NotifyOutcome::Delivered)
        .remembering(&snapshot1.remembered.memory);

    let snapshot2 = collect(&world2, now(), &Thresholds::default()).expect("second run");
    assert_eq!(
        world2.local_payloads().len(),
        1,
        "second attempt happens because the first failed"
    );
    assert!(
        !snapshot2
            .remembered
            .memory
            .announcements
            .workspaces
            .is_empty(),
        "successful delivery updates the record"
    );
}

// --- Cost shape: one batch per channel, not one request per alert (ticket #20) ---
//
// The failure these prevent is not a wrong answer, it is a collection that has not returned.
// Fourteen at-risk workspaces is the steady state on the machine behind the ticket, and the
// first run on any machine announces everything notable at once, so a per-alert loop against a
// channel allowed ten seconds a request could sit for over two minutes inside the alerting
// step. What must survive the fix is #9's rule: an alert is recorded as sent only if it
// actually arrived.

const ALPHA: &str = "/Users/pmcfadin/projects/alpha";
const BETA: &str = "/Users/pmcfadin/projects/beta";
const GAMMA: &str = "/Users/pmcfadin/projects/gamma";

#[test]
fn each_channel_is_asked_about_a_whole_run_before_the_other_channel_is_asked_at_all() {
    // The observable that separates the two cost shapes. A per-alert loop carries each
    // announcement through every channel before looking at the next, so two channels
    // interleave; a batched run hands one channel everything it has to say. Only the second
    // shape lets a channel overlap its own requests or bound what the lot may cost.
    let world = FakeWorld::quiet()
        .with_workspace(ALPHA, 3)
        .with_workspace(BETA, 4)
        .with_workspace(GAMMA, 5)
        .with_local_channel("echo", NotifyOutcome::Delivered)
        .with_remote_channel("https://example.com/notify", NotifyOutcome::Delivered);

    let snapshot = collect(&world, now(), &Thresholds::default()).expect("collection");

    assert_eq!(
        snapshot.remembered.notify_health.notable, 3,
        "three stranded workspaces are three notable states"
    );
    assert_eq!(
        *world.call_order.borrow(),
        vec![
            Channel::Local,
            Channel::Local,
            Channel::Local,
            Channel::Remote,
            Channel::Remote,
            Channel::Remote
        ],
        "each channel is asked once for the whole run, not once per alert"
    );
}

#[test]
fn a_batched_run_retires_exactly_the_alerts_that_arrived_and_no_others() {
    // The guarantee #9 bought, under the new cost shape. A batch whose outcomes came back
    // against the wrong payloads would retire the wrong alert while every tally still added up
    // — and the alert that never arrived would never be announced again.
    let world = FakeWorld::quiet()
        .with_workspace(ALPHA, 3)
        .with_workspace(BETA, 4)
        .with_workspace(GAMMA, 5)
        .with_local_channel_answering(|payload| {
            if payload.contains("beta") {
                NotifyOutcome::Failed("this one was refused".to_string())
            } else {
                NotifyOutcome::Delivered
            }
        });

    let snapshot = collect(&world, now(), &Thresholds::default()).expect("collection");

    let health = &snapshot.remembered.notify_health;
    assert_eq!((health.local_delivered, health.local_failed), (2, 1));

    let announced: Vec<String> = snapshot
        .remembered
        .memory
        .announcements
        .workspaces
        .iter()
        .map(|(path, _)| path.clone())
        .collect();
    assert_eq!(
        announced,
        vec![ALPHA.to_string(), GAMMA.to_string()],
        "the two that arrived are recorded and the one that was refused is not — by identity, \
         not by count"
    );
}

#[test]
fn an_alert_the_channel_was_never_given_is_not_recorded_as_sent() {
    // The trap this ticket had to avoid: taking delivery off the critical path by bounding it,
    // and then booking the alerts that did not fit as announced. That is the same
    // fire-and-forget with optimistic bookkeeping that #9 exists to remove, wearing a budget.
    let world = FakeWorld::quiet()
        .with_workspace(ALPHA, 3)
        .with_workspace(BETA, 4)
        .with_local_channel(
            "echo",
            NotifyOutcome::NotAttempted("the run's alerting budget was spent".to_string()),
        );

    let snapshot = collect(&world, now(), &Thresholds::default()).expect("collection");

    assert!(
        snapshot
            .remembered
            .memory
            .announcements
            .workspaces
            .is_empty(),
        "an alert nobody was given must not be recorded as announced"
    );

    // Run again against the state the first run left, with a channel that now answers.
    let again = FakeWorld::quiet()
        .with_workspace(ALPHA, 3)
        .with_workspace(BETA, 4)
        .with_local_channel("echo", NotifyOutcome::Delivered)
        .remembering(&snapshot.remembered.memory);

    let second = collect(&again, now(), &Thresholds::default()).expect("second run");
    assert_eq!(
        again.local_payloads().len(),
        2,
        "both unattempted alerts are announced on the following run"
    );
    assert_eq!(
        second.remembered.memory.announcements.workspaces.len(),
        2,
        "and are recorded once they arrive"
    );
}

#[test]
fn an_alert_that_was_never_attempted_is_counted_apart_from_one_the_channel_refused() {
    // Both are re-announced, and only one is evidence the channel is broken. A reader deciding
    // whether their notifier still works has to be able to tell them apart — and a run that
    // announced two of four strandings must not read as a run that had two to announce.
    let world = FakeWorld::quiet()
        .with_workspace(ALPHA, 3)
        .with_workspace(BETA, 4)
        .with_local_channel(
            "echo",
            NotifyOutcome::NotAttempted("the run's alerting budget was spent".to_string()),
        );

    let health = collect(&world, now(), &Thresholds::default())
        .expect("collection")
        .remembered
        .notify_health;

    assert_eq!(health.notable, 2, "two states were worth announcing");
    assert_eq!(health.local_not_attempted, 2, "and neither was sent");
    assert_eq!(
        health.local_failed, 0,
        "a channel that was never asked did not fail"
    );
    assert!(
        !health.has_failures() && health.has_unattempted(),
        "the two conditions are separately reportable"
    );
    let why = health
        .not_attempted_reason
        .as_deref()
        .expect("a count with no reason is a silent cap wearing a number");
    assert!(
        why.contains("budget"),
        "the reason has to say what stopped it; got {why:?}"
    );
}

#[test]
fn a_run_with_nothing_notable_asks_no_channel_anything() {
    // The counterweight, and the reason a quiet machine costs nothing to alert on: no spawn,
    // no request, no wait. Both channels are configured and neither is touched.
    let world = FakeWorld::quiet()
        .with_workspace(ALPHA, 0)
        .with_local_channel("echo", NotifyOutcome::Delivered)
        .with_remote_channel("https://example.com/notify", NotifyOutcome::Delivered);

    let snapshot = collect(&world, now(), &Thresholds::default()).expect("collection");

    assert_eq!(snapshot.remembered.notify_health.notable, 0);
    assert!(
        world.call_order.borrow().is_empty(),
        "a clean workspace is not worth waking a notifier for; got {:?}",
        world.call_order.borrow()
    );
    assert_eq!(
        snapshot.remembered.notify_health.delivery_cost,
        Duration::ZERO,
        "and a run that asked nothing spent nothing asking"
    );
}

#[test]
fn the_cost_of_the_alerting_step_is_reported_rather_than_absorbed_into_the_run() {
    // Ticket #10 meters this tool against a one-second fast tier, and alerting is the one part
    // of a collection that waits on something outside the machine. A cost it cannot name is a
    // cost it cannot budget. Asserted as a floor derived from the delay this test injected —
    // never as a wall-clock figure, which varies by about 2x between runs on this machine.
    const PER_DELIVERY: Duration = Duration::from_millis(40);

    let world = FakeWorld::quiet()
        .with_workspace(ALPHA, 3)
        .with_workspace(BETA, 4)
        .with_local_channel("echo", NotifyOutcome::Delivered)
        .with_local_delay(PER_DELIVERY);

    let health = collect(&world, now(), &Thresholds::default())
        .expect("collection")
        .remembered
        .notify_health;

    assert_eq!(health.local_delivered, 2, "both alerts were delivered");
    assert!(
        health.delivery_cost >= 2 * PER_DELIVERY,
        "two deliveries that each took {PER_DELIVERY:?} cannot have cost less than both of \
         them; got {:?}",
        health.delivery_cost
    );
}

// --- The delivery scheduler itself (ticket #20) ---

fn alerts(count: usize) -> Vec<String> {
    (0..count)
        .map(|i| format!("Workspace /Users/pmcfadin/projects/w{i} is STRANDED"))
        .collect()
}

#[test]
fn delivering_a_run_concurrently_costs_a_fraction_of_delivering_it_one_at_a_time() {
    // The whole point of the ticket, as a ratio rather than a wall-clock figure: the same
    // channel, the same alerts, the same per-delivery cost, scheduled two ways. An absolute
    // assertion here would fail for reasons unrelated to correctness.
    const PER_DELIVERY: Duration = Duration::from_millis(60);
    const GENEROUS: Duration = Duration::from_secs(60);
    let payloads = alerts(8);

    let channel = |_: &str| {
        std::thread::sleep(PER_DELIVERY);
        NotifyOutcome::Delivered
    };

    let one_at_a_time = acmon::deliver::sequentially(&payloads, GENEROUS, channel);
    let overlapped = acmon::deliver::in_parallel(
        &payloads,
        acmon::deliver::Bounds {
            workers: 8,
            budget: GENEROUS,
        },
        channel,
    );

    // Assert success before believing either measurement.
    for report in [&one_at_a_time, &overlapped] {
        assert_eq!(report.outcomes.len(), payloads.len());
        assert!(
            report.outcomes.iter().all(|o| o.delivered()),
            "every alert was delivered in both schedules; got {:?}",
            report.outcomes
        );
    }

    assert!(
        overlapped.cost * 2 < one_at_a_time.cost,
        "eight overlapped deliveries must not cost what eight sequential ones do; overlapped \
         {:?} against sequential {:?}",
        overlapped.cost,
        one_at_a_time.cost
    );
}

#[test]
fn alerts_the_budget_did_not_reach_are_reported_as_not_attempted_and_never_dropped() {
    // A silent cap in an alerting path reads as "nothing to report". Every alert the batch was
    // given comes back with an outcome, and the ones nobody was told about say why.
    const PER_DELIVERY: Duration = Duration::from_millis(400);
    const BUDGET: Duration = Duration::from_millis(150);
    const WORKERS: usize = 3;
    let payloads = alerts(12);

    let report = acmon::deliver::in_parallel(
        &payloads,
        acmon::deliver::Bounds {
            workers: WORKERS,
            budget: BUDGET,
        },
        |_| {
            std::thread::sleep(PER_DELIVERY);
            NotifyOutcome::Delivered
        },
    );

    assert_eq!(
        report.outcomes.len(),
        payloads.len(),
        "nothing is dropped: one outcome per alert, always"
    );
    let delivered = report.count(|o| o.delivered());
    let unattempted = report.count(|o| o.not_attempted());
    assert_eq!(
        delivered + unattempted,
        payloads.len(),
        "and every outcome is one or the other — a channel that answered nothing did not fail; \
         got {:?}",
        report.outcomes
    );
    assert!(
        delivered <= WORKERS,
        "only the deliveries already in flight when the budget ran out can have completed; \
         {delivered} of {} did",
        payloads.len()
    );
    for outcome in report.outcomes.iter().filter(|o| o.not_attempted()) {
        let why = outcome.why().expect("a stated reason");
        assert!(
            why.contains("budget"),
            "an unsent alert names what stopped it; got {why:?}"
        );
    }
    assert!(
        report.cost < 4 * PER_DELIVERY,
        "the run stopped at its budget instead of working through all twelve; cost {:?} \
         against {:?} per delivery",
        report.cost,
        PER_DELIVERY
    );
}

#[test]
fn the_sequential_fallback_also_stops_at_the_budget() {
    // The default every World gets. The guarantee that a run does not spend its alert count
    // times a timeout belongs to every implementation, not only the concurrent one — a fake or
    // a future World that did not override the batch must not be the slow path that hangs a
    // collection.
    const PER_DELIVERY: Duration = Duration::from_millis(300);
    const BUDGET: Duration = Duration::from_millis(120);
    let payloads = alerts(12);

    let report = acmon::deliver::sequentially(&payloads, BUDGET, |_| {
        std::thread::sleep(PER_DELIVERY);
        NotifyOutcome::Delivered
    });

    assert_eq!(report.outcomes.len(), payloads.len());
    assert!(
        report.count(|o| o.delivered()) <= 1,
        "the first delivery already outran the budget; got {:?}",
        report.outcomes
    );
    assert_eq!(
        report.count(|o| o.not_attempted()),
        payloads.len() - report.count(|o| o.delivered()),
        "and the rest are stated as unsent rather than silently skipped"
    );
    assert!(
        report.cost < 3 * PER_DELIVERY,
        "cost {:?} shows it did not work through all twelve",
        report.cost
    );
}

#[test]
fn every_outcome_comes_back_against_the_alert_it_belongs_to() {
    // Concurrency reorders completions, so the mapping from alert to outcome cannot be the
    // order things finished in. If it were, a batch would retire the alert that happened to
    // finish where a delivered one used to be — verification that lands on the wrong alert is
    // worse than none, because it looks correct.
    let payloads = alerts(20);

    let report = acmon::deliver::in_parallel(
        &payloads,
        acmon::deliver::Bounds {
            workers: 6,
            budget: Duration::from_secs(60),
        },
        |payload| {
            if payload.ends_with('7') {
                NotifyOutcome::Failed(payload.to_string())
            } else {
                NotifyOutcome::Delivered
            }
        },
    );

    assert_eq!(report.outcomes.len(), payloads.len());
    for (payload, outcome) in payloads.iter().zip(report.outcomes.iter()) {
        let expected = if payload.ends_with('7') {
            NotifyOutcome::Failed(payload.clone())
        } else {
            NotifyOutcome::Delivered
        };
        assert_eq!(
            outcome, &expected,
            "the outcome for {payload:?} landed on the wrong alert"
        );
    }
}

#[test]
fn a_run_with_no_alerts_starts_no_delivery_at_all() {
    let report = acmon::deliver::in_parallel(&[], acmon::deliver::Bounds::standard(), |_| {
        panic!("a run with nothing to announce must not touch a channel")
    });

    assert!(report.outcomes.is_empty());
    assert_eq!(report.cost, Duration::ZERO);
}

// --- Independent channel delivery ---

#[test]
fn the_local_notification_mechanism_is_configurable_and_its_absence_does_not_prevent_remote_delivery(
) {
    // Only a remote channel is configured. It delivers.
    let world = FakeWorld::quiet()
        .with_workspace(WORKSPACE, 2)
        .with_remote_channel("https://example.com/notify", NotifyOutcome::Delivered);

    let snapshot = collect(&world, now(), &Thresholds::default()).expect("collection");

    assert!(
        world.local_payloads().is_empty(),
        "no local channel configured, so no local delivery"
    );
    assert_eq!(world.remote_payloads().len(), 1, "remote channel delivered");
    assert!(
        !snapshot
            .remembered
            .memory
            .announcements
            .workspaces
            .is_empty(),
        "remote delivery alone is sufficient to record the announcement"
    );
}

#[test]
fn both_channels_can_deliver_the_same_announcement() {
    let world = FakeWorld::quiet()
        .with_workspace(WORKSPACE, 1)
        .with_local_channel("echo", NotifyOutcome::Delivered)
        .with_remote_channel("https://example.com/notify", NotifyOutcome::Delivered);

    let snapshot = collect(&world, now(), &Thresholds::default()).expect("collection");

    assert_eq!(world.local_payloads().len(), 1, "local delivered");
    assert_eq!(world.remote_payloads().len(), 1, "remote delivered");
    assert_eq!(
        snapshot.remembered.notify_health.local_delivered, 1,
        "health tracks local success"
    );
    assert_eq!(
        snapshot.remembered.notify_health.remote_delivered, 1,
        "health tracks remote success"
    );
}

#[test]
fn if_one_channel_fails_but_another_succeeds_the_announcement_is_still_recorded() {
    // Local fails, remote succeeds. The announcement should be recorded because SOME channel
    // delivered it.
    let world = FakeWorld::quiet()
        .with_workspace(WORKSPACE, 1)
        .with_local_channel("echo", NotifyOutcome::Failed("local failed".to_string()))
        .with_remote_channel("https://example.com/notify", NotifyOutcome::Delivered);

    let snapshot = collect(&world, now(), &Thresholds::default()).expect("collection");

    assert_eq!(snapshot.remembered.notify_health.local_failed, 1);
    assert_eq!(snapshot.remembered.notify_health.remote_delivered, 1);
    assert!(
        !snapshot
            .remembered
            .memory
            .announcements
            .workspaces
            .is_empty(),
        "one success is enough to record the announcement"
    );
}

// --- Privacy ---

#[test]
fn notifications_carry_names_and_states_only_never_prompt_text_or_process_arguments() {
    // This test structurally enforces the privacy constraint: the payload is built from
    // Session and WorkspaceReport, which have no fields for conversation content or process
    // arguments. Assert the property anyway.
    let world = FakeWorld::quiet()
        .with_agent(900, WORKSPACE)
        .with_namespace(NAMESPACE, WORKSPACE, ago(Duration::from_secs(15 * 60)))
        .with_workspace(WORKSPACE, 3)
        .with_local_channel("echo", NotifyOutcome::Delivered);

    let _ = collect(&world, now(), &Thresholds::default()).expect("collection");

    let payloads = world.local_payloads();
    for payload in &payloads {
        // Process arguments are never read, so this is a structural check.
        // The payload should contain: CLI id, namespace/pid, state, workspace path, entry count.
        assert!(
            !payload.contains("--flag") && !payload.contains("arg1"),
            "payload must not contain process arguments; got {:?}",
            payload
        );
        // Conversation content is never read, so this is also structural.
        assert!(
            !payload.contains("user:") && !payload.contains("assistant:"),
            "payload must not contain conversation markers; got {:?}",
            payload
        );
    }
}

// --- Channel health reporting ---

#[test]
fn a_monitor_with_no_channels_configured_reports_that_visibly() {
    // No channels at all. The health should show this, and a warning should appear if there
    // were notable states to announce.
    let world = FakeWorld::quiet().with_workspace(WORKSPACE, 1);

    let snapshot = collect(&world, now(), &Thresholds::default()).expect("collection");

    assert!(!snapshot.remembered.notify_health.config.has_any());
    // The render module will print a warning when notable states exist but no channels are
    // configured. That's tested in the render tests.
}

#[test]
fn delivery_status_is_checked_and_channel_health_is_reported() {
    let world = FakeWorld::quiet()
        .with_workspace(WORKSPACE, 1)
        .with_local_channel("echo", NotifyOutcome::Failed("local failed".to_string()))
        .with_remote_channel("https://example.com/notify", NotifyOutcome::Delivered);

    let snapshot = collect(&world, now(), &Thresholds::default()).expect("collection");

    let health = &snapshot.remembered.notify_health;
    assert_eq!(health.local_failed, 1);
    assert_eq!(health.remote_delivered, 1);
    assert!(health.has_failures(), "at least one failure occurred");
}

// --- Unknown at-risk workspaces ---

#[test]
fn a_workspace_version_control_refuses_is_at_risk_and_announces() {
    // QueryFailed and TimedOut are at-risk by the precautionary principle.
    let world = FakeWorld::quiet()
        .with_unreadable_workspace(WORKSPACE, Unreadable::QueryFailed("index.lock".to_string()))
        .with_local_channel("echo", NotifyOutcome::Delivered);

    let snapshot = collect(&world, now(), &Thresholds::default()).expect("collection");

    let workspace = snapshot
        .workspaces
        .iter()
        .find(|w| w.path == WORKSPACE)
        .expect("workspace");
    assert!(
        workspace.state.at_risk(),
        "a workspace we cannot read about is at risk"
    );

    let payloads = world.local_payloads();
    assert_eq!(
        payloads.len(),
        1,
        "an at-risk unknown state triggers a notification"
    );
    assert!(
        payloads[0].contains("at risk"),
        "payload mentions the risk; got {:?}",
        payloads[0]
    );
}

#[test]
fn not_version_controlled_and_path_gone_do_not_announce() {
    // These are answers, not failures. They hold nothing to lose.
    for why in [Unreadable::NotVersionControlled, Unreadable::PathGone] {
        let world = FakeWorld::quiet()
            .with_unreadable_workspace(WORKSPACE, why.clone())
            .with_local_channel("echo", NotifyOutcome::Delivered);

        let snapshot = collect(&world, now(), &Thresholds::default()).expect("collection");

        let workspace = snapshot
            .workspaces
            .iter()
            .find(|w| w.path == WORKSPACE)
            .expect("workspace");
        assert!(
            !workspace.state.at_risk(),
            "{:?} is an answer, not at risk",
            why
        );

        assert!(
            world.local_payloads().is_empty(),
            "{:?} should not trigger a notification; got {:?}",
            why,
            world.local_payloads()
        );
    }
}

// --- Schema compatibility ---

#[test]
fn a_state_file_written_before_announcements_existed_still_parses() {
    // The announcements field was added with #[serde(default)], so a v1 file that lacks it
    // should still parse.
    let old_state = r#"{
  "version": 1,
  "workspaces": [],
  "sessions": []
}"#;

    let (memory, degraded) = acmon::memory::parse(old_state);
    assert_eq!(
        degraded, None,
        "a v1 file without announcements still parses"
    );
    assert!(
        memory.announcements.sessions.is_empty() && memory.announcements.workspaces.is_empty(),
        "the default is an empty announcement record"
    );
}

#[test]
fn the_schema_version_stays_at_1_after_adding_announcements() {
    // The ticket says "leave SCHEMA_VERSION at 1" because the field has #[serde(default)].
    assert_eq!(
        acmon::memory::SCHEMA_VERSION,
        1,
        "adding announcements with #[serde(default)] does not bump the schema version"
    );
}

// --- The real configuration file on disk ---
//
// These drive `RealWorld` against scratch files. The distinction they establish — a config
// that is absent versus one that cannot be understood — is invisible in the channel tallies,
// because both deliver nothing. Only the stated reason separates them.

fn scratch_config(name: &str, contents: Option<&str>) -> std::path::PathBuf {
    let directory = std::env::temp_dir().join(format!("acmon-seam9-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).expect("scratch directory");
    let path = directory.join("notify.toml");
    if let Some(text) = contents {
        std::fs::write(&path, text).expect("write the config");
    }
    path
}

#[test]
fn a_machine_with_no_notification_config_is_not_reported_as_broken() {
    let path = scratch_config("absent", None);
    let config = acmon::RealWorld::with_notify_config(&path).read_notify_config();

    assert_eq!(
        config.unusable, None,
        "never having configured alerting is a choice, not a fault, and warning about it on \
         every run would train a reader to ignore the warning"
    );
    assert!(!config.has_any());

    let _ = std::fs::remove_dir_all(path.parent().expect("parent"));
}

#[test]
fn a_malformed_notification_config_carries_the_specific_error() {
    // The whole point of #9's second paragraph: a channel that delivers nothing must not look
    // like a machine with nothing to say. A typo here silently disables alerting, and the run
    // that needed the alert is the run that will not get one.
    let path = scratch_config(
        "malformed",
        Some("local_command = \nremote_url = ]]not toml"),
    );
    let config = acmon::RealWorld::with_notify_config(&path).read_notify_config();

    let why = config
        .unusable
        .as_ref()
        .expect("a config that cannot be parsed must say so, not quietly deliver nothing");
    assert!(
        why.contains("notify.toml"),
        "the reason must name the file, or nobody knows where to look; got {why:?}"
    );
    assert!(
        why.len() > "notify.toml".len() + 8,
        "and must carry the parser's own complaint rather than only the filename; got {why:?}"
    );
    assert!(
        !config.has_any(),
        "a config that could not be understood configures no channels"
    );

    let _ = std::fs::remove_dir_all(path.parent().expect("parent"));
}

#[test]
fn a_well_formed_notification_config_is_read_and_reports_no_problem() {
    // The control. Without it, a reader could not tell whether the two tests above pass
    // because the parser is strict or because it rejects everything.
    let path = scratch_config(
        "valid",
        Some("local_command = \"terminal-notifier -message -\"\nremote_url = \"https://example.invalid/hook\"\n"),
    );
    let config = acmon::RealWorld::with_notify_config(&path).read_notify_config();

    assert_eq!(config.unusable, None);
    assert_eq!(
        config.local_command.as_deref(),
        Some("terminal-notifier -message -")
    );
    assert_eq!(
        config.remote_url.as_deref(),
        Some("https://example.invalid/hook")
    );

    let _ = std::fs::remove_dir_all(path.parent().expect("parent"));
}

#[test]
fn a_configured_but_blank_channel_counts_as_absent_rather_than_as_a_command() {
    // An empty string is a command that would run the shell with nothing in it and succeed,
    // which would record every alert as delivered while sending none.
    let path = scratch_config(
        "blank",
        Some("local_command = \"   \"\nremote_url = \"\"\n"),
    );
    let config = acmon::RealWorld::with_notify_config(&path).read_notify_config();

    assert_eq!(config.unusable, None, "a blank value is not malformed");
    assert!(
        !config.has_any(),
        "but it configures nothing — a channel that always succeeds and sends nothing is the \
         worst of both states"
    );

    let _ = std::fs::remove_dir_all(path.parent().expect("parent"));
}

// --- The real local channel, running real commands (ticket #20) ---
//
// These drive `RealWorld`, because the concurrency and the per-request bound live in the
// implementation that actually spawns something. Every assertion below is a ratio against a
// figure this test injected, never a wall-clock threshold: measurements on this class of
// machine vary by roughly 2x between runs.

/// How long the tests below let a delivery — and a whole run's deliveries — take.
///
/// A quarter second rather than the real ten, so a suite that has to prove what happens when a
/// channel will not answer is still a suite somebody runs.
const SHORT_BUDGET: Duration = Duration::from_millis(250);

#[test]
fn many_local_deliveries_do_not_cost_one_wait_each() {
    const PER_DELIVERY: &str = "sleep 0.4";
    let world = acmon::RealWorld::new();

    // One delivery first, to establish what a single wait costs on this machine right now —
    // and to assert it succeeded before any timing is believed.
    let started = std::time::Instant::now();
    let single_outcome = world.notify_local(PER_DELIVERY, "Workspace /w is STRANDED");
    let single = started.elapsed();
    assert_eq!(
        single_outcome,
        NotifyOutcome::Delivered,
        "the command has to succeed before its timing means anything"
    );

    let payloads = alerts(12);
    let report = world.notify_local_batch(PER_DELIVERY, &payloads);

    assert_eq!(report.outcomes.len(), payloads.len());
    assert!(
        report.outcomes.iter().all(|o| o.delivered()),
        "all twelve arrived; got {:?}",
        report.outcomes
    );
    assert!(
        report.cost * 3 < single * payloads.len() as u32,
        "twelve deliveries must not cost twelve waits; the batch took {:?} where one took \
         {single:?}",
        report.cost
    );
}

#[test]
fn a_local_command_that_never_exits_is_reported_as_a_failure_rather_than_waited_on() {
    // Before this ticket the wait was unbounded. A notifier that never exits — a helper waiting
    // on a dialog, a command left in the config that reads stdin forever — stopped the
    // collection returning at all, and a monitor that has not returned is not monitoring.
    let world = acmon::RealWorld::with_notify_request_budget(SHORT_BUDGET);

    let started = std::time::Instant::now();
    let outcome = world.notify_local("sleep 30", "Workspace /w is STRANDED");
    let elapsed = started.elapsed();

    assert!(
        outcome.failed(),
        "a command that would not finish delivered nothing; got {outcome:?}"
    );
    let why = outcome.why().expect("a stated reason");
    assert!(
        why.contains("did not exit"),
        "and says so, rather than blaming the payload; got {why:?}"
    );
    assert!(
        elapsed < 10 * SHORT_BUDGET,
        "it gave up near its own budget instead of waiting out the command; took {elapsed:?} \
         against a budget of {SHORT_BUDGET:?}"
    );
}

#[test]
fn a_run_whose_local_channel_hangs_states_what_it_did_not_send() {
    // The shape of the worst case in the ticket: a channel that answers nothing, and more
    // notable states than one run's budget can carry. What must not happen is a run that sits
    // for alert-count times the budget, and what must not happen instead is a run that quietly
    // forgets the alerts it never sent.
    let world = acmon::RealWorld::with_notify_request_budget(SHORT_BUDGET);
    let payloads = alerts(12);

    let report = world.notify_local_batch("sleep 30", &payloads);

    assert_eq!(
        report.outcomes.len(),
        payloads.len(),
        "every alert comes back with an outcome"
    );
    assert_eq!(
        report.count(|o| o.delivered()),
        0,
        "a command that was killed delivered nothing"
    );
    assert!(
        report.count(|o| o.not_attempted()) >= 1,
        "the budget ran out before the last of twelve, and that is stated; got {:?}",
        report.outcomes
    );
    assert_eq!(
        report.count(|o| o.failed()) + report.count(|o| o.not_attempted()),
        payloads.len(),
        "refused and never-sent between them account for all twelve"
    );
    assert!(
        report.cost * 4 < payloads.len() as u32 * SHORT_BUDGET,
        "the run cost about one budget, not twelve; took {:?}",
        report.cost
    );
}
