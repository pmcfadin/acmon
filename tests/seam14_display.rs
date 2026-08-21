//! Seam 14 — the live display: what it polls, what it says, and what it refuses to do.
//!
//! The failures this seam exists to prevent are all failures of a *calm* screen.
//!
//! 1. A display that writes. `agtop` called the collection library for a year while persisting
//!    the memory file and delivering notifications, and nothing in the code said it should not.
//!    Two writers undo the single-writer guarantee the two-binary split rests on, and an alert
//!    from a foreground UI is redundant with looking at it (F26).
//! 2. A display that shows a torn state file as a short one. Half a `state.json` parses as
//!    fewer sessions and fewer at-risk workspaces, which is exactly the shape of a healthy
//!    screen.
//! 3. A display that re-reads and redraws whatever it is given, so that "is it refreshing?"
//!    and "has the monitor stopped?" become the same unanswerable question.
//! 4. A display that prints `0%` where a duty cycle it never received should be. Zero is a
//!    monitor that is running and idle — the one thing the reader most wants to know, and the
//!    one thing an absent figure never means.
//!
//! Every decision here is asserted as a function over data. Nothing in this file looks at a
//! terminal, and nothing in it needs a human to check.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command as Process;
use std::time::{Duration, Instant, SystemTime};

use acmon::display::{
    command_for, own_collection, read_state_file, stat_state_file, Command, Meters, Poll, Poller,
    Screen, Stat, StateReading, Unmetered, POLL_INTERVAL,
};
use acmon::liveness::{Method, State, Thresholds, Verdict};
use acmon::render::{meter_line, minimum_width, screen_height, screen_to_lines};
use acmon::state::{Paths, StateStore, Tier, TieredState, STATE_FILE};
use acmon::vcs::{Unreadable, VcsFacts, WorkspaceState};
use acmon::workspace::{NamespaceResolution, Workspace, WorkspaceUnknown};
use acmon::world::{
    ActivityUnavailable, CodexSession, NotifyConfig, NotifyOutcome, ProcessRecord, ProcessSnapshot,
    ResourceSource, Resources, ResourcesUnavailable, StateRead, Sweep, World, WorldError,
};
use acmon::{
    collect_as, Identity, Persistence, Remembered, Role, Session, Snapshot, WorkspaceReport,
};

// --- Fixtures -------------------------------------------------------------------------------

const OBSERVER: i32 = 4_242;

/// The instant every fixture here is taken as of.
fn fixture_now() -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(1_787_000_000)
}

/// A width that fits the whole table with room to spare.
///
/// Derived from the minimum rather than stated, so that a column changing width does not turn
/// these into truncation tests.
fn wide() -> u16 {
    minimum_width() + 32
}

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

fn active_verdict() -> Verdict {
    Verdict {
        state: State::Active,
        method: Method::TranscriptChangedRecently,
    }
}

fn session(pid: i32) -> Session {
    Session {
        identity: Identity::Process { pid },
        cli: "claude".to_string(),
        resources: Ok(measured_ledger()),
        last_reading: None,
        workspace: Ok(Workspace {
            path: "/Users/pmcfadin/projects/agentic_coding_monitor".to_string(),
            namespace: Ok("-Users-pmcfadin-projects-agentic-coding-monitor".to_string()),
        }),
        liveness: active_verdict(),
    }
}

/// A snapshot with one session and one clean workspace: nothing wrong, nothing at risk.
fn ordinary_snapshot() -> Snapshot {
    Snapshot {
        taken_at: fixture_now(),
        sessions: vec![session(69_046)],
        workspaces: vec![WorkspaceReport {
            path: "/Users/pmcfadin/projects/agentic_coding_monitor".to_string(),
            state: WorkspaceState::Clean,
            linked_worktree: false,
            uncommitted_entries: Some(0),
        }],
        unlocated: Vec::new(),
        sweep_complete: true,
        remembered: Remembered::none(),
    }
}

/// The rendered screen with its line breaks and padding collapsed to single spaces.
///
/// For asserting on a *sentence*. Every notice is wrapped to the terminal's width, so where a
/// break falls is a property of the width rather than of what was said.
fn prose(screen: &Screen, width: u16) -> String {
    screen_to_lines(screen, width, screen_height(screen, width))
        .join(" ")
        .split_whitespace()
        .collect::<Vec<&str>>()
        .join(" ")
}

/// A directory this test owns, emptied before use.
fn scratch(label: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("acmon-seam14-{}-{label}", std::process::id()));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).expect("a scratch directory");
    path
}

// --- A World that records every mark it is asked to leave ------------------------------------

/// A fake world whose only interesting property is that it remembers being written to.
///
/// The two effectful calls in a collection are the state write and the notification, and the
/// point of this fake is that both are observable. A fake that merely accepted them would let
/// the read-only rule pass by not being tested.
struct RecordingWorld {
    records: Vec<ProcessRecord>,
    namespaces: Vec<String>,
    activities: HashMap<String, SystemTime>,
    resolutions: HashMap<String, NamespaceResolution>,
    facts: HashMap<String, Result<VcsFacts, Unreadable>>,
    roots: HashMap<String, (String, bool)>,
    sweep: Sweep,
    config: NotifyConfig,
    writes: RefCell<Vec<String>>,
    notifications: RefCell<Vec<String>>,
}

impl RecordingWorld {
    /// A machine with one live session and one workspace holding uncommitted work that nothing
    /// is driving — so a collection has both a row to show and something notable to announce.
    fn with_a_stranded_workspace() -> Self {
        let stranded = "/Users/pmcfadin/projects/presto_testing".to_string();
        let mut world = RecordingWorld {
            records: vec![
                ProcessRecord {
                    pid: OBSERVER,
                    exe_path: Ok("/usr/bin/agtop".to_string()),
                    cwd: Ok("/Users/pmcfadin".to_string()),
                },
                ProcessRecord {
                    pid: 69_046,
                    exe_path: Ok("/Users/pmcfadin/.local/share/claude/versions/2.1.233".to_string()),
                    cwd: Ok("/Users/pmcfadin/projects/agentic_coding_monitor".to_string()),
                },
            ],
            namespaces: vec!["-Users-pmcfadin-projects-agentic-coding-monitor".to_string()],
            activities: HashMap::new(),
            resolutions: HashMap::new(),
            facts: HashMap::new(),
            roots: HashMap::new(),
            sweep: Sweep {
                repositories: vec![(stranded.clone(), false)],
                complete: true,
                directories_visited: 1,
            },
            config: NotifyConfig {
                local_command: Some("true".to_string()),
                remote_url: None,
                unusable: None,
            },
            writes: RefCell::new(Vec::new()),
            notifications: RefCell::new(Vec::new()),
        };
        world.activities.insert(
            "-Users-pmcfadin-projects-agentic-coding-monitor".to_string(),
            fixture_now(),
        );
        world.resolutions.insert(
            "-Users-pmcfadin-projects-agentic-coding-monitor".to_string(),
            NamespaceResolution::Resolved(
                "/Users/pmcfadin/projects/agentic_coding_monitor".to_string(),
            ),
        );
        world
            .roots
            .insert(stranded.clone(), (stranded.clone(), false));
        world.facts.insert(
            stranded.clone(),
            Ok(VcsFacts {
                root: stranded,
                uncommitted_entries: 28,
                linked_worktree: false,
            }),
        );
        world
    }
}

impl World for RecordingWorld {
    fn output_width(&self) -> u16 {
        120
    }

    fn process_snapshot(&self) -> Result<ProcessSnapshot, WorldError> {
        Ok(ProcessSnapshot {
            records: self.records.clone(),
            observer_pid: OBSERVER,
        })
    }

    fn resources(&self, _pid: i32) -> Result<Resources, ResourcesUnavailable> {
        Ok(measured_ledger())
    }

    fn recorded_namespaces(&self) -> Result<Vec<String>, WorldError> {
        Ok(self.namespaces.clone())
    }

    fn namespace_activity(&self, namespace: &str) -> Result<SystemTime, ActivityUnavailable> {
        self.activities
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

    fn resolve_namespace(&self, namespace: &str) -> NamespaceResolution {
        self.resolutions
            .get(namespace)
            .cloned()
            .unwrap_or(NamespaceResolution::NoLongerExists)
    }

    fn sweep_for_repositories(&self, _roots: &[String]) -> Sweep {
        self.sweep.clone()
    }

    fn read_state(&self) -> StateRead {
        StateRead::Absent
    }

    fn write_state(&self, contents: &str) -> Result<(), String> {
        self.writes.borrow_mut().push(contents.to_string());
        Ok(())
    }

    fn read_notify_config(&self) -> NotifyConfig {
        self.config.clone()
    }

    fn notify_local(&self, _command: &str, payload: &str) -> NotifyOutcome {
        self.notifications.borrow_mut().push(payload.to_string());
        NotifyOutcome::Delivered
    }
}

// --- Read-only, in fact and not only in intent ----------------------------------------------

#[test]
fn a_collection_made_for_the_display_writes_nothing_and_asks_no_channel_anything() {
    // The carried-forward gap from #26. The lock made `amon` the sole writer of the state
    // *directory*; this is what makes the display stop writing the pre-split memory file and
    // stop delivering, which is what the lock was protecting all along.
    let world = RecordingWorld::with_a_stranded_workspace();

    let snapshot = collect_as(&world, fixture_now(), &Thresholds::default(), Role::Display)
        .expect("a collection over a trustworthy snapshot");

    assert!(
        world.writes.borrow().is_empty(),
        "the display wrote state: {:?}",
        world.writes.borrow()
    );
    assert!(
        world.notifications.borrow().is_empty(),
        "the display delivered a notification: {:?}",
        world.notifications.borrow()
    );
    assert!(
        snapshot.remembered.notify_health.notable > 0,
        "this fixture has to have something notable in it, or the assertion above passes for \
         the wrong reason"
    );
}

#[test]
fn a_collection_made_for_the_monitor_still_writes_and_still_notifies() {
    // The other half, and the reason it is a separate test: a role flag that silenced both
    // roles would pass the test above and break the product.
    let world = RecordingWorld::with_a_stranded_workspace();

    collect_as(&world, fixture_now(), &Thresholds::default(), Role::Monitor)
        .expect("a collection over a trustworthy snapshot");

    assert_eq!(
        world.writes.borrow().len(),
        1,
        "the monitor writes its state once per pass"
    );
    assert!(
        !world.notifications.borrow().is_empty(),
        "the monitor announces what is notable"
    );
}

#[test]
fn the_display_says_it_wrote_nothing_rather_than_warning_that_it_failed_to() {
    // A write that was never going to happen is not a write that failed. Reported as a failure
    // it would put "the next run will start with no history" on a screen every second, and a
    // warning a reader learns to ignore is a warning that is not there when it matters.
    let world = RecordingWorld::with_a_stranded_workspace();

    let snapshot = collect_as(&world, fixture_now(), &Thresholds::default(), Role::Display)
        .expect("a collection");

    assert!(
        matches!(
            snapshot.remembered.persisted,
            Persistence::NotAttempted { .. }
        ),
        "got {:?}",
        snapshot.remembered.persisted
    );

    let screen = Screen::from_own_collection(
        &StateReading::Absent,
        Ok(snapshot),
        Duration::from_millis(9),
        fixture_now(),
    );
    let text = prose(&screen, wide());

    assert!(
        text.contains("Nothing was written"),
        "the screen must say the display wrote nothing; got:\n{text}"
    );
    assert!(
        text.contains("Nothing was announced"),
        "and that it announced nothing; got:\n{text}"
    );
    assert!(
        !text.contains("next run will start with no history"),
        "a display's silence must not be dressed as a monitor's failure; got:\n{text}"
    );
}

#[test]
fn a_display_screen_still_reports_alerts_a_monitor_did_not_attempt() {
    // #20 must survive this ticket. A display collecting for itself never attempts an alert, so
    // the "not sent" account has nothing to say about it — but the moment a monitor's figures
    // are what is on screen, the warning has to come back, unchanged.
    let mut snapshot = ordinary_snapshot();
    snapshot.remembered.notify_health.read_only = None;
    snapshot.remembered.notify_health.notable = 6;
    snapshot.remembered.notify_health.local_not_attempted = 6;
    snapshot.remembered.notify_health.not_attempted_reason =
        Some("the alerting step ran out of its budget".to_string());
    snapshot.remembered.notify_health.config = NotifyConfig {
        local_command: Some("say".to_string()),
        remote_url: None,
        unusable: None,
    };

    let screen = Screen::from_own_collection(
        &StateReading::Absent,
        Ok(snapshot),
        Duration::from_millis(9),
        fixture_now(),
    );
    let text = prose(&screen, wide());

    assert!(
        text.contains("NOT SENT") && text.contains("ran out of its budget"),
        "an alert that was never offered to a channel is a silent cap, and the screen has to \
         name it; got:\n{text}"
    );
}

#[test]
fn a_display_screen_still_states_why_a_liveness_verdict_could_not_be_reached() {
    // #22 must survive this ticket too. A bare UNKNOWN is the one verdict a reader can neither
    // act on nor account for.
    let mut snapshot = ordinary_snapshot();
    snapshot.sessions = vec![Session {
        identity: Identity::Process { pid: 4_711 },
        cli: "cursor-agent".to_string(),
        resources: Ok(measured_ledger()),
        last_reading: None,
        workspace: Err(WorkspaceUnknown::PermissionDenied),
        liveness: Verdict {
            state: State::Unknown,
            method: Method::TranscriptActivityUnknown,
        },
    }];

    let screen = Screen::from_own_collection(
        &StateReading::Absent,
        Ok(snapshot),
        Duration::from_millis(9),
        fixture_now(),
    );
    let text = prose(&screen, wide());

    assert!(
        text.contains("UNKNOWN"),
        "the state belongs on the row; got:\n{text}"
    );
    assert!(
        text.contains("workspace is unknown"),
        "and the reason it could not be determined belongs beside it; got:\n{text}"
    );
}

// --- Polling: `stat`, and re-read only on a change ------------------------------------------

#[test]
fn the_poll_interval_is_a_second_and_the_state_file_is_stated_rather_than_watched() {
    // The interval is a stated requirement (F27, ~1 s), and the deliberate absence of a
    // filesystem-event watcher is the load-bearing half: a watcher would gain a fraction of a
    // second and add a class of "it silently stopped delivering" bug to a tool whose thesis is
    // that silent background failure is the enemy.
    assert_eq!(POLL_INTERVAL, Duration::from_secs(1));
}

#[test]
fn a_state_file_whose_modification_time_has_not_moved_is_not_read_again() {
    let directory = scratch("unchanged");
    let file = directory.join(STATE_FILE);
    std::fs::write(&file, b"{}").expect("a state file");

    let mut poller = Poller::new();

    assert_eq!(
        poller.observe(stat_state_file(&file)),
        Poll::Reread,
        "the first look at a file is always a read"
    );
    assert_eq!(
        poller.observe(stat_state_file(&file)),
        Poll::Unchanged,
        "an unmoved modification time is the whole reason to poll rather than re-read"
    );

    let _ = std::fs::remove_dir_all(&directory);
}

#[test]
fn a_state_file_that_has_been_rewritten_is_read_again() {
    let directory = scratch("rewritten");
    let file = directory.join(STATE_FILE);
    std::fs::write(&file, b"{}").expect("a state file");

    let mut poller = Poller::new();
    assert_eq!(poller.observe(stat_state_file(&file)), Poll::Reread);

    // Stated rather than slept for. The behaviour under test is "a different mtime is a
    // re-read", and sleeping past the filesystem's timestamp granularity would make this a
    // test about how long that granularity is.
    let later = SystemTime::now() + Duration::from_secs(30);
    assert_eq!(
        poller.observe(Stat::At(later)),
        Poll::Reread,
        "a state file the monitor rewrote has to be read again, or the display never refreshes"
    );
    assert_eq!(
        poller.observe(Stat::At(later)),
        Poll::Unchanged,
        "and then stop being read, until it moves again"
    );

    let _ = std::fs::remove_dir_all(&directory);
}

#[test]
fn an_absent_state_file_is_reported_once_and_not_mistaken_for_an_unchanged_one() {
    let directory = scratch("absent");
    let file = directory.join(STATE_FILE);

    let mut poller = Poller::new();

    assert_eq!(
        poller.observe(stat_state_file(&file)),
        Poll::Absent,
        "there is no file, and that is a different answer from `nothing changed`"
    );
    assert_eq!(
        poller.observe(stat_state_file(&file)),
        Poll::Unchanged,
        "a file that is still absent has not changed, and a display that redrew every second \
         over it would burn the duty cycle it exists to report"
    );

    let _ = std::fs::remove_dir_all(&directory);
}

#[test]
fn a_state_file_that_cannot_be_checked_is_reported_rather_than_read_as_absent() {
    // The failure this prevents: a permissions mistake on the monitor's own directory reads as
    // "no monitor is running", and the display then collects for itself and looks healthy doing
    // it. Driven through a real unreadable directory, because the distinction lives in the
    // mapping from `errno` and a stubbed `errno` would be a test of the stub.
    let directory = scratch("unstattable");
    let closed = directory.join("closed");
    std::fs::create_dir_all(&closed).expect("a directory");
    let file = closed.join(STATE_FILE);
    std::fs::write(&file, b"{}").expect("a state file");

    let mut permissions = std::fs::metadata(&closed).expect("metadata").permissions();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        permissions.set_mode(0o000);
    }
    std::fs::set_permissions(&closed, permissions).expect("closing the directory");

    let observed = stat_state_file(&file);

    // Restored before asserting, so a failure does not leave an unreadable directory behind.
    let mut permissions = std::fs::metadata(&closed).expect("metadata").permissions();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        permissions.set_mode(0o755);
    }
    std::fs::set_permissions(&closed, permissions).expect("reopening the directory");

    match &observed {
        Stat::Failed(why) => assert!(
            why.contains(STATE_FILE),
            "the reason has to name the path that could not be checked; got {why}"
        ),
        other => panic!("an unreadable directory must not read as an absent file; got {other:?}"),
    }

    let mut poller = Poller::new();
    assert!(
        matches!(poller.observe(observed), Poll::Unstattable(_)),
        "and the poll says so rather than falling through to `absent`"
    );

    let _ = std::fs::remove_dir_all(&directory);
}

// --- What the state file turned out to be ---------------------------------------------------

fn store_in(directory: &std::path::Path) -> StateStore {
    let directory = directory.to_str().expect("utf-8");
    StateStore::new(
        Paths::from_values(Some(directory), Some(directory), None)
            .expect("explicit directories need no home"),
    )
}

#[test]
fn no_state_file_at_all_reads_as_absent_rather_than_as_a_failure() {
    let directory = scratch("nofile");

    assert_eq!(
        read_state_file(&store_in(&directory)),
        StateReading::Absent,
        "a machine where nothing has ever published is the first thing a fresh install hits, \
         and it is an answer rather than a fault"
    );

    let _ = std::fs::remove_dir_all(&directory);
}

#[test]
fn a_state_file_with_no_tiers_names_its_writer_and_the_work_that_will_fill_them() {
    // Exactly what `amon watch` publishes today: the writer pid, and no tier, because the
    // collection loop is #27. A display that drew that as an empty table would be reporting a
    // machine with no agents on it.
    let directory = scratch("tierless");
    let store = store_in(&directory);
    store
        .write_tiered_state(STATE_FILE, &TieredState::new(31_337))
        .expect("publishing a state file");

    match read_state_file(&store) {
        StateReading::Unrenderable { writer_pid, why } => {
            assert_eq!(writer_pid, 31_337, "the writer has to be named");
            assert!(
                why.contains("#27"),
                "and a reader has to be told what will fill the tiers; got {why}"
            );
        }
        other => panic!("expected an unrenderable state file, got {other:?}"),
    }

    let _ = std::fs::remove_dir_all(&directory);
}

#[test]
fn a_state_file_with_tiers_this_display_cannot_read_says_so_rather_than_drawing_nothing() {
    // The forward-looking half of the same rule: once #27 publishes tier payloads, a display
    // with no reader for them must say that, not report a quiet machine.
    let directory = scratch("unknown-tiers");
    let store = store_in(&directory);
    let mut state = TieredState::new(31_337);
    state.set_tier_data(
        Tier::Fast,
        serde_json::json!({"sessions": []}),
        fixture_now(),
    );
    store
        .write_tiered_state(STATE_FILE, &state)
        .expect("publishing a state file");

    match read_state_file(&store) {
        StateReading::Unrenderable { why, .. } => assert!(
            why.contains("1 tier"),
            "the count of what could not be read is the fact a reader acts on; got {why}"
        ),
        other => panic!("expected an unrenderable state file, got {other:?}"),
    }

    let _ = std::fs::remove_dir_all(&directory);
}

#[test]
fn a_torn_state_file_is_reported_as_such_and_nothing_is_taken_from_it() {
    // A crash mid-write on a filesystem without atomic renames, or a hand-edit. The bytes are
    // the front half of a real state file, which is what makes it dangerous: it looks like a
    // state file.
    let directory = scratch("torn");
    std::fs::write(
        directory.join(STATE_FILE),
        br#"{"version": 1, "writer_pid": 31337, "tiers": {"Fast": {"timestamp": "2026-08-"#,
    )
    .expect("a torn state file");

    let reading = read_state_file(&store_in(&directory));

    match &reading {
        StateReading::Unusable(why) => assert!(
            why.contains("parse"),
            "the parser's own complaint has to survive, so the file can be inspected rather \
             than just deleted; got {why}"
        ),
        other => panic!("a torn file must not read as an absent or an empty one; got {other:?}"),
    }

    let screen = Screen::from_own_collection(
        &reading,
        Ok(ordinary_snapshot()),
        Duration::from_millis(9),
        fixture_now(),
    );
    let text = prose(&screen, wide());

    assert!(
        text.contains("COULD NOT BE BELIEVED"),
        "the screen has to say the file is untrustworthy; got:\n{text}"
    );
    assert!(
        text.contains("Nothing was taken from it"),
        "and that nothing was salvaged from it — half a state file renders as a short session \
         list, which is the shape of a healthy screen; got:\n{text}"
    );

    let _ = std::fs::remove_dir_all(&directory);
}

#[test]
fn a_state_file_from_an_unknown_version_is_unusable_rather_than_partly_believed() {
    let directory = scratch("version");
    std::fs::write(
        directory.join(STATE_FILE),
        br#"{"version": 99, "writer_pid": 31337, "tiers": {}}"#,
    )
    .expect("a state file from the future");

    match read_state_file(&store_in(&directory)) {
        StateReading::Unusable(why) => assert!(
            why.contains("99"),
            "the version nobody understood is the fact worth reporting; got {why}"
        ),
        other => panic!("expected an unusable state file, got {other:?}"),
    }

    let _ = std::fs::remove_dir_all(&directory);
}

// --- The meters: what this tool costs, beside what it measured ------------------------------

#[test]
fn a_duty_cycle_nobody_published_prints_its_reason_where_the_number_would_be() {
    let line = meter_line(&Meters {
        overhead: Ok(Duration::from_millis(2_470)),
        duty_cycle: Err(Unmetered::NoMonitor),
    });

    assert!(
        !line.contains('%'),
        "0% is a monitor that is running and idle, which is the one thing this does not mean; \
         got:\n{line}"
    );
    assert!(
        line.contains("no monitor is recording"),
        "the reason goes where the figure would be; got:\n{line}"
    );
    assert!(
        line.contains("2.5s"),
        "the overhead the display did measure is still a figure; got:\n{line}"
    );
}

#[test]
fn a_duty_cycle_that_was_published_is_shown_as_a_percentage() {
    let line = meter_line(&Meters {
        overhead: Ok(Duration::from_millis(120)),
        duty_cycle: Ok(0.043),
    });

    assert!(
        line.contains("4.3%"),
        "a figure that exists is printed as one; got:\n{line}"
    );
}

#[test]
fn which_reason_a_missing_duty_cycle_carries_depends_on_what_the_state_file_was() {
    // Three silences that need three different responses: start a monitor, wait for #27, or go
    // and look at a file that is damaged.
    for (reading, expected) in [
        (StateReading::Absent, "no monitor is recording"),
        (
            StateReading::Unrenderable {
                writer_pid: 1,
                why: "no tier yet".to_string(),
            },
            "published no self-metering",
        ),
        (
            StateReading::Unusable("torn".to_string()),
            "could not be believed",
        ),
    ] {
        let meters = Meters::for_own_collection(&reading, Duration::from_millis(1));
        let line = meter_line(&meters);
        assert!(
            line.contains(expected),
            "for {reading:?} the meter line should contain {expected:?}; got:\n{line}"
        );
    }
}

#[test]
fn the_meters_are_on_the_same_screen_as_the_figures_they_qualify() {
    // F33: first-class, alongside the data — not in a log, not behind a flag.
    let screen = Screen::from_own_collection(
        &StateReading::Absent,
        Ok(ordinary_snapshot()),
        Duration::from_millis(2_470),
        fixture_now(),
    );
    let text = prose(&screen, wide());

    assert!(
        text.contains("METERS") && text.contains("collection overhead"),
        "the meters belong on the screen; got:\n{text}"
    );
    assert!(
        text.contains("duty cycle"),
        "including the monitor's duty cycle; got:\n{text}"
    );
    assert!(
        text.contains("69046"),
        "and the figures they qualify have to be on it too, or they qualify nothing; got:\n{text}"
    );
}

#[test]
fn the_display_reports_an_overhead_it_actually_timed() {
    // Not an absolute timing — a ratio between two clocks over the same work. The reported
    // overhead cannot exceed the wall time of the call that produced it, and a figure of zero
    // would mean nothing was timed at all.
    let world = RecordingWorld::with_a_stranded_workspace();

    let started = Instant::now();
    let (facts, overhead) = own_collection(&world, fixture_now(), &Thresholds::default());
    let observed = started.elapsed();

    assert!(facts.is_ok(), "the collection has to have succeeded first");
    assert!(
        overhead <= observed,
        "reported overhead {overhead:?} exceeds the {observed:?} the call actually took"
    );
    assert!(
        overhead > Duration::ZERO,
        "an overhead of exactly zero is a figure nobody measured"
    );
}

// --- The screen itself ---------------------------------------------------------------------

#[test]
fn the_rows_on_the_screen_are_sessions() {
    // F31. One row per session, identified by what identifies a session.
    let mut snapshot = ordinary_snapshot();
    snapshot.sessions = vec![session(69_046), session(264)];

    let screen = Screen::from_own_collection(
        &StateReading::Absent,
        Ok(snapshot),
        Duration::from_millis(9),
        fixture_now(),
    );
    let text = prose(&screen, wide());

    assert!(
        text.contains("69046") && text.contains("264"),
        "every session gets a row; got:\n{text}"
    );
    assert!(
        text.contains("2 agent session(s)"),
        "and the screen says how many it is showing; got:\n{text}"
    );
}

#[test]
fn the_at_risk_panel_is_on_the_screen_when_nothing_is_at_risk() {
    // F32. "0 at risk" is information, and an absent panel reads as "did not check".
    let screen = Screen::from_own_collection(
        &StateReading::Absent,
        Ok(ordinary_snapshot()),
        Duration::from_millis(9),
        fixture_now(),
    );
    let text = prose(&screen, wide());

    assert!(
        text.contains("at risk — 0 of 1 workspaces"),
        "the panel is always visible, with its denominator; got:\n{text}"
    );
    assert!(
        text.contains("No workspaces at risk"),
        "and says so in words, so that an empty list reads as checked-and-clear; got:\n{text}"
    );
}

#[test]
fn the_at_risk_panel_lists_the_stranded_workspaces_it_found() {
    let mut snapshot = ordinary_snapshot();
    snapshot.workspaces.push(WorkspaceReport {
        path: "/Users/pmcfadin/projects/presto_testing".to_string(),
        state: WorkspaceState::DirtyStranded,
        linked_worktree: false,
        uncommitted_entries: Some(28),
    });

    let screen = Screen::from_own_collection(
        &StateReading::Absent,
        Ok(snapshot),
        Duration::from_millis(9),
        fixture_now(),
    );
    let text = prose(&screen, wide());

    assert!(
        text.contains("DIRTY-STRANDED") && text.contains("presto_testing") && text.contains("28"),
        "the reason this project exists is a stranded pile of uncommitted work, named and \
         counted; got:\n{text}"
    );
}

#[test]
fn a_screen_with_no_figures_says_so_instead_of_drawing_an_empty_table() {
    let screen = Screen::from_own_collection(
        &StateReading::Absent,
        Err("process snapshot incomplete (observer 4242 not in its own result)".to_string()),
        Duration::from_millis(9),
        fixture_now(),
    );
    let text = prose(&screen, wide());

    assert!(
        text.contains("NO FIGURES") && text.contains("observer 4242"),
        "the reason there is nothing to show is the whole content of that screen; got:\n{text}"
    );
    assert!(
        !text.contains("OWN CPU"),
        "an empty table is indistinguishable from a machine with no agents on it; got:\n{text}"
    );
}

#[test]
fn every_screen_states_where_its_figures_came_from() {
    // A screen that says nothing about its own provenance is a screen a reader will assume came
    // from a running monitor.
    for reading in [
        StateReading::Absent,
        StateReading::Unrenderable {
            writer_pid: 31_337,
            why: "no tier yet".to_string(),
        },
        StateReading::Unusable("torn".to_string()),
    ] {
        let screen = Screen::from_own_collection(
            &reading,
            Ok(ordinary_snapshot()),
            Duration::from_millis(9),
            fixture_now(),
        );

        assert!(
            !screen.notices.is_empty(),
            "no screen may be silent about its provenance"
        );
        let text = prose(&screen, wide());
        assert!(
            text.contains("this display took for itself"),
            "for {reading:?} the screen has to say the figures are its own read; got:\n{text}"
        );
        assert!(
            text.contains("read-only"),
            "and that it is read-only, so nobody looks for the alert it did not send; \
             got:\n{text}"
        );
    }
}

#[test]
fn a_screen_with_no_monitor_says_that_nothing_is_being_recorded_or_alerted() {
    // F28, and the first thing a fresh `brew install` hits.
    let screen = Screen::from_own_collection(
        &StateReading::Absent,
        Ok(ordinary_snapshot()),
        Duration::from_millis(9),
        fixture_now(),
    );
    let text = prose(&screen, wide());

    assert!(text.contains("NO MONITOR IS RECORDING"), "got:\n{text}");
    assert!(
        text.contains("nothing will be announced"),
        "a display that says nothing about the alerts nobody is sending is how a whole day of \
         them goes missing; got:\n{text}"
    );
}

#[test]
fn the_same_screen_fits_whatever_width_the_terminal_is() {
    // The resize invariant, asserted as one: the display is redrawn at the new size, and no
    // line may overflow it. Absolute widths are not asserted — only that every line fits, at
    // every width from the narrowest the table can be drawn in upwards.
    let screen = Screen::from_own_collection(
        &StateReading::Unusable("torn".to_string()),
        Ok(ordinary_snapshot()),
        Duration::from_millis(2_470),
        fixture_now(),
    );

    for width in [minimum_width(), minimum_width() + 17, minimum_width() + 120] {
        let lines = screen_to_lines(&screen, width, screen_height(&screen, width));
        for line in &lines {
            assert!(
                line.chars().count() <= width as usize,
                "a line of {} characters does not fit {width} columns: {line}",
                line.chars().count()
            );
        }
        let text = prose(&screen, width);
        assert!(
            text.contains("METERS"),
            "the meters survive every width; got at {width}:\n{text}"
        );
        assert!(
            text.contains("COULD NOT BE BELIEVED"),
            "and so does the notice; got at {width}:\n{text}"
        );
    }
}

#[test]
fn a_terminal_too_narrow_for_the_numbers_still_says_why() {
    // Inherited from the table and asserted here so the screen wrapper cannot lose it: a
    // truncated CPU total is a plausible wrong answer, so none is printed.
    let screen = Screen::from_own_collection(
        &StateReading::Absent,
        Ok(ordinary_snapshot()),
        Duration::from_millis(9),
        fixture_now(),
    );
    let narrow = minimum_width() - 1;
    let text = prose(&screen, narrow);

    assert!(
        text.contains("columns"),
        "the refusal names what it needs; got:\n{text}"
    );
    assert!(
        !text.contains("69046"),
        "and prints no figure it could not print in full; got:\n{text}"
    );
}

// --- What a keypress does -------------------------------------------------------------------

fn key(code: ratatui::crossterm::event::KeyCode) -> ratatui::crossterm::event::Event {
    use ratatui::crossterm::event::{Event, KeyEvent, KeyModifiers};
    Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
}

fn control(code: ratatui::crossterm::event::KeyCode) -> ratatui::crossterm::event::Event {
    use ratatui::crossterm::event::{Event, KeyEvent, KeyModifiers};
    Event::Key(KeyEvent::new(code, KeyModifiers::CONTROL))
}

#[test]
fn the_keys_that_leave_are_the_only_keys_that_do_anything() {
    use ratatui::crossterm::event::KeyCode;

    for code in [KeyCode::Char('q'), KeyCode::Char('Q'), KeyCode::Esc] {
        assert_eq!(
            command_for(&key(code)),
            Command::Quit,
            "{code:?} should leave"
        );
    }
    for code in [KeyCode::Char('c'), KeyCode::Char('d')] {
        assert_eq!(
            command_for(&control(code)),
            Command::Quit,
            "ctrl-{code:?} should leave"
        );
    }
}

#[test]
fn no_key_sorts_the_table_or_acts_on_a_session() {
    // Not an omission — a requirement (F55, N1). Interactive sorting places this display inside
    // `htop`'s interaction model, and a reader who feels they are in `htop` reaches for F9,
    // which this tool must never honour: a stalled session holding uncommitted work has to be
    // inspected by a human before anything touches it.
    use ratatui::crossterm::event::KeyCode;

    for code in [
        KeyCode::Char('c'),
        KeyCode::Char('m'),
        KeyCode::Char('p'),
        KeyCode::Char('k'),
        KeyCode::Char('>'),
        KeyCode::Char('<'),
        KeyCode::F(6),
        KeyCode::F(9),
        KeyCode::Enter,
    ] {
        assert_eq!(
            command_for(&key(code)),
            Command::Ignore,
            "{code:?} must do nothing at all"
        );
    }
}

#[test]
fn a_resize_asks_for_another_pass() {
    use ratatui::crossterm::event::Event;

    assert_eq!(command_for(&Event::Resize(80, 24)), Command::Redraw);
}

// --- The binary, where the rule has to hold in fact -----------------------------------------

#[test]
fn agtop_leaves_no_mark_where_the_monitor_would_write_one() {
    // The one test that would have caught the gap #26 carried forward: `agtop` persisted the
    // pre-split memory file and could deliver, because it called the collection library in the
    // monitor's role. Nothing short of running the binary proves it stopped.
    let directory = scratch("readonly-binary");
    let memory_file = directory.join("memory.json");
    let state_directory = directory.join("state");
    let notified = directory.join("notified");
    let notify_config = directory.join("notify.toml");
    std::fs::write(
        &notify_config,
        format!("local_command = \"touch {}\"\n", notified.display()),
    )
    .expect("a notification channel");

    let output = Process::new(env!("CARGO_BIN_EXE_agtop"))
        .arg("--once")
        .env(acmon::real_world::STATE_VARIABLE, &memory_file)
        .env(acmon::real_world::NOTIFY_CONFIG_VARIABLE, &notify_config)
        .env(acmon::state::STATE_DIR_VARIABLE, &state_directory)
        .env(acmon::state::CONFIG_DIR_VARIABLE, directory.join("config"))
        .output()
        .expect("running agtop");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    // Asserted whatever the machine's state turned out to be. A collection that failed still
    // must not have written anything.
    assert!(
        !memory_file.exists(),
        "agtop wrote the memory file at {}; it is read-only",
        memory_file.display()
    );
    assert!(
        !state_directory.exists(),
        "agtop created the monitor's state directory at {}; the monitor is the sole writer \
         there",
        state_directory.display()
    );
    assert!(
        !notified.exists(),
        "agtop delivered a notification: a foreground UI announcing what it is already \
         showing is redundant, and the monitor is the only notifier"
    );
    assert!(
        !stdout.trim().is_empty() || !stderr.trim().is_empty(),
        "and it still has to have said something"
    );

    let _ = std::fs::remove_dir_all(&directory);
}

#[test]
fn agtop_refuses_the_screen_when_there_is_no_terminal_to_take() {
    // stdout is a pipe here, as it is in every test. Escape sequences and an alternate screen
    // written into one produce a file nobody can read, so the refusal names the mode that
    // exists for this.
    let directory = scratch("no-terminal");

    let output = Process::new(env!("CARGO_BIN_EXE_agtop"))
        .env(acmon::state::STATE_DIR_VARIABLE, directory.join("state"))
        .output()
        .expect("running agtop");

    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        !output.status.success(),
        "a display that drew nothing must not report success"
    );
    assert!(
        stderr.contains("--once"),
        "the refusal has to name the mode that works here; got:\n{stderr}"
    );

    let _ = std::fs::remove_dir_all(&directory);
}
