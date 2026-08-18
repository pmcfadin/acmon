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
        // Make the sweep return this workspace so it gets discovered
        let sweep = Sweep {
            repositories: vec![(path.to_string(), false)],
            complete: true,
            directories_visited: 1,
        };
        self.sweep = Some(sweep);
        self
    }

    fn with_unreadable_workspace(mut self, path: &str, why: Unreadable) -> Self {
        self.roots
            .insert(path.to_string(), (path.to_string(), false));
        self.facts.insert(path.to_string(), Err(why));
        // Make the sweep return this workspace so it gets discovered
        let sweep = Sweep {
            repositories: vec![(path.to_string(), false)],
            complete: true,
            directories_visited: 1,
        };
        self.sweep = Some(sweep);
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
        self.local_outcome.clone()
    }

    fn notify_remote(&self, url: &str, payload: &str) -> NotifyOutcome {
        self.remote_log
            .borrow_mut()
            .push((url.to_string(), payload.to_string()));
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
