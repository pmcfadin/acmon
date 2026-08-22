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
    ages_of, command_for, cost_of, in_cost_order, own_collection, presence_of, published_cost_of,
    published_in_cost_order, read_state_file, reading_of, stat_state_file, Absence, Command, Cost,
    Facts, Meters, Poll, Poller, Presence, PublishedReading, Screen, Stat, StateReading, TierAge,
    Unmetered, Writer, MISSED_A_PASS, POLL_INTERVAL,
};
use acmon::liveness::{Method, State, Thresholds, Verdict};
use acmon::meter::{Measured, PassReport, SelfReport};
use acmon::render::{meter_row, minimum_width, screen_height, screen_to_lines};
use acmon::schedule::Cadence;
use acmon::state::{Paths, StateStore, Tier, TieredState, STATE_FILE};
use acmon::tiers::{
    FastPayload, MediumPayload, NotifyRow, PassEnvelope, SessionRow, SlowPayload, WorkspaceRow,
};
use acmon::vcs::{Unreadable, VcsFacts, WorkspaceState};
use acmon::workspace::{NamespaceResolution, Workspace, WorkspaceUnknown};
use acmon::world::{
    ActivityUnavailable, CodexSession, LoadAverage, NotifyConfig, NotifyOutcome, ProcessRecord,
    ProcessSnapshot, ResourceSource, Resources, ResourcesUnavailable, StateRead, Sweep, Unmeasured,
    World, WorldError,
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
    prose_at(screen, width, screen_height(screen, width))
}

/// The same, at a height the caller chose — including one too short to hold everything.
fn prose_at(screen: &Screen, width: u16, height: u16) -> String {
    screen_to_lines(screen, width, height)
        .join(" ")
        .split_whitespace()
        .collect::<Vec<&str>>()
        .join(" ")
}

/// The verdict a display reaches with no state file at all: `ABSENT`, and its own read.
fn no_monitor() -> Presence {
    Presence::Absent(Absence::NoStateFile)
}

/// The verdict for a state file that could not be believed.
fn unbelievable(why: &str) -> Presence {
    Presence::Absent(Absence::Unbelievable(why.to_string()))
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
        &no_monitor(),
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
        &no_monitor(),
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
        &no_monitor(),
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
fn a_state_file_with_no_tiers_names_its_writer_and_says_it_has_collected_nothing() {
    // What `amon watch` publishes in the moment between taking the lock and completing its first
    // pass: the writer pid, and no tier. A display that drew that as an empty table would be
    // reporting a machine with no agents on it — and it is a state that still exists now that the
    // loop is built, because the writer is published before anything is collected.
    let directory = scratch("tierless");
    let store = store_in(&directory);
    store
        .write_tiered_state(STATE_FILE, &TieredState::new(31_337))
        .expect("publishing a state file");

    match read_state_file(&store) {
        StateReading::Unrenderable { writer_pid, why } => {
            assert_eq!(writer_pid, 31_337, "the writer has to be named");
            assert!(
                why.contains("no pass"),
                "and a reader has to be told that nothing has been collected rather than that \
                 nothing is there; got {why}"
            );
        }
        other => panic!("expected an unrenderable state file, got {other:?}"),
    }

    let _ = std::fs::remove_dir_all(&directory);
}

#[test]
fn a_tier_payload_this_display_cannot_decode_condemns_the_whole_file() {
    // The other half of the same rule, and the shape of it changed with #30: the display now has
    // a reader for every tier, so a payload it cannot decode is no longer "a tier I have no
    // reader for" — it is a file that cannot be believed. Taking the tiers that happened to
    // decode would render a shorter session list and a shorter at-risk panel, which is the shape
    // of a healthy screen.
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
        StateReading::Unusable(why) => assert!(
            why.contains("fast"),
            "the reason has to name the tier whose payload could not be read; got {why}"
        ),
        other => panic!("expected an unusable state file, got {other:?}"),
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
        &presence_of(&reading, fixture_now(), Writer::Gone),
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

/// The meter row at a width that fits it, as one string.
fn meters_as_text(meters: &Meters) -> String {
    meter_row(meters, wide()).join(" ")
}

/// Just the line the gauges themselves are drawn on.
///
/// Asserted on separately from the rest of the row because the legend underneath explains what
/// the markers in a bar mean, and therefore contains every one of them.
fn gauge_line(meters: &Meters) -> String {
    let lines = meter_row(meters, wide());
    lines
        .iter()
        .find(|line| line.contains('['))
        .cloned()
        .unwrap_or_else(|| panic!("no gauges in:\n{}", lines.join("\n")))
}

#[test]
fn a_duty_cycle_nobody_published_prints_its_reason_where_the_number_would_be() {
    let line = meters_as_text(&Meters {
        overhead: Ok(Duration::from_millis(2_470)),
        duty_cycle: Err(Unmetered::NoMonitor),
        taken_at: fixture_now(),
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
    let line = meters_as_text(&Meters {
        overhead: Ok(Duration::from_millis(120)),
        duty_cycle: Ok(0.043),
        taken_at: fixture_now(),
    });

    assert!(
        line.contains("4.3%"),
        "a figure that exists is printed as one; got:\n{line}"
    );
}

#[test]
fn every_meter_in_the_row_is_a_gauge_with_a_bar_of_its_own() {
    // Decision 37: a row rather than a sentence, so that v2's machine-tax figures — XProtect,
    // Jamf, Gatekeeper, Zscaler — move into it instead of forcing a redesign of the top of the
    // screen. A bar per figure is the shape that makes that true.
    let row = gauge_line(&Meters {
        overhead: Ok(Duration::from_millis(400)),
        duty_cycle: Ok(0.4),
        taken_at: fixture_now(),
    });

    assert!(
        row.contains("collection overhead"),
        "the overhead is one of the gauges; got:\n{row}"
    );
    assert!(
        row.contains("amon duty cycle"),
        "both meters belong on the one row, so they read as a row; got:\n{row}"
    );
    assert_eq!(
        row.matches('[').count(),
        2,
        "one bar per figure, not one bar for the pair; got:\n{row}"
    );
    assert!(
        row.contains("||||"),
        "a gauge at four tenths of its scale draws a bar; got:\n{row}"
    );
}

#[test]
fn a_gauge_with_no_figure_is_never_drawn_as_an_empty_bar() {
    // An empty bar is what zero looks like, and zero is the one thing an absent figure never
    // means. The reason for the absence is on its own line, in full: it is a sentence, and a
    // gauge's figure column is eight columns wide.
    let meters = Meters {
        overhead: Ok(Duration::from_millis(400)),
        duty_cycle: Err(Unmetered::NothingPublishedYet),
        taken_at: fixture_now(),
    };
    let text = meters_as_text(&meters);
    let row = gauge_line(&meters);
    let duty = row
        .split("amon duty cycle")
        .nth(1)
        .expect("the duty gauge's own cell");

    assert!(
        duty.contains('?') && duty.contains("absent"),
        "an absent figure's bar has to be visibly not a measurement; got:\n{row}"
    );
    assert!(
        !duty.contains('|'),
        "and must not draw any of the fill a measured figure draws; got:\n{row}"
    );
    assert!(
        text.contains("has not published a pass carrying this figure yet"),
        "the reason is stated in full rather than squeezed into the cell; got:\n{text}"
    );
}

#[test]
fn a_figure_past_the_end_of_its_scale_is_marked_rather_than_quietly_pegged() {
    // The live case, not a hypothetical: the whole collection takes about 2.5 s against a
    // one-second refresh interval. A bar silently full would report that as a collection which
    // exactly filled the interval.
    let over = gauge_line(&Meters {
        overhead: Ok(POLL_INTERVAL * 3),
        duty_cycle: Ok(1.0),
        taken_at: fixture_now(),
    });
    let exactly_full = gauge_line(&Meters {
        overhead: Ok(POLL_INTERVAL),
        duty_cycle: Ok(1.0),
        taken_at: fixture_now(),
    });

    assert!(
        over.contains('>'),
        "past the scale has to look different from at the scale; got:\n{over}"
    );
    assert!(
        !exactly_full.contains('>'),
        "and a figure exactly at its scale is not past it; got:\n{exactly_full}"
    );
    assert!(
        over.contains("3.0s"),
        "the figure itself is never clamped, whatever the bar does; got:\n{over}"
    );
}

#[test]
fn the_meter_row_states_the_scale_each_bar_is_drawn_against() {
    // A picture of a ratio whose denominator is unstated is not a measurement.
    let text = meters_as_text(&Meters {
        overhead: Ok(Duration::from_millis(400)),
        duty_cycle: Ok(0.4),
        taken_at: fixture_now(),
    });

    assert!(
        text.contains("refresh interval"),
        "the overhead bar's denominator; got:\n{text}"
    );
    assert!(
        text.contains("wall time"),
        "and the duty cycle's; got:\n{text}"
    );
}

#[test]
fn the_meter_row_states_the_instant_its_figures_were_taken_as_of() {
    // A gauge is the easiest thing on a screen to read as live. What the instant makes of the
    // monitor — FRESH, STALE, DEAD — is #30; stating the instant is not.
    let text = meters_as_text(&Meters {
        overhead: Ok(Duration::from_millis(400)),
        duty_cycle: Err(Unmetered::NoMonitor),
        taken_at: fixture_now(),
    });

    // `fixture_now()` is 1 787 000 000 seconds after the epoch, which is this instant in UTC.
    assert!(
        text.contains("2026-08-17T20:53:20Z"),
        "the row says when its figures were read; got:\n{text}"
    );
    assert!(
        text.contains("as of"),
        "and says that is what the instant is; got:\n{text}"
    );
}

#[test]
fn no_line_of_the_meter_row_runs_off_the_side_of_any_terminal() {
    // Including terminals too narrow for one gauge, which is where the refusal to draw the
    // table is printed — the row is above that refusal and has to fit beside it.
    let meters = Meters {
        overhead: Ok(Duration::from_millis(2_470)),
        duty_cycle: Err(Unmetered::NoMonitor),
        taken_at: fixture_now(),
    };

    // From the width of a timestamp upwards: `wrap_words` breaks between words and not inside
    // one, so a terminal narrower than the longest single word on the screen is beyond what any
    // line-breaking here can promise — and it is far below the width at which the table refuses
    // to draw at all.
    for width in [21, 30, 38, 39, 40, 79, minimum_width(), wide()] {
        for line in meter_row(&meters, width) {
            assert!(
                line.chars().count() <= width as usize,
                "a meter line of {} characters does not fit {width} columns: {line}",
                line.chars().count()
            );
        }
        assert!(
            meter_row(&meters, width).join(" ").contains("2.5s"),
            "and the figure survives every width, unshortened, at {width}"
        );
    }
}

#[test]
fn which_reason_a_missing_duty_cycle_carries_depends_on_why_there_were_no_published_figures() {
    // Three silences that need three different responses: start a monitor, wait a few seconds
    // for one that has just started, or go and look at a file that is damaged. None of them is
    // "this display cannot read the payloads yet", which is what the middle one used to say and
    // which #30 made false.
    for (absence, expected) in [
        (Absence::NoStateFile, "no monitor is recording"),
        (
            Absence::NothingPublished {
                writer_pid: 1,
                why: "no tier yet".to_string(),
            },
            "has not published a pass carrying this figure yet",
        ),
        (
            Absence::Unbelievable("torn".to_string()),
            "could not be believed",
        ),
    ] {
        let meters = Meters::for_own_collection(&absence, Duration::from_millis(1), fixture_now());
        let line = meters_as_text(&meters);
        assert!(
            line.contains(expected),
            "for {absence:?} the meter line should contain {expected:?}; got:\n{line}"
        );
    }
}

#[test]
fn the_meters_are_on_the_same_screen_as_the_figures_they_qualify() {
    // F33: first-class, alongside the data — not in a log, not behind a flag.
    let screen = Screen::from_own_collection(
        &no_monitor(),
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

// --- The order rows are in: what costs, first ------------------------------------------------

/// A session whose children have consumed exactly this much CPU.
fn session_costing(pid: i32, children_cpu: Duration) -> Session {
    let mut session = session(pid);
    session.resources = Ok(Resources {
        children_cpu: Ok(children_cpu),
        ..measured_ledger()
    });
    session
}

/// A session whose ledger was read and whose child CPU was not in it.
///
/// The `ps` fallback reader is blind to it, so this is the ordinary shape of an unmeasurable
/// cost rather than a contrived one.
fn session_with_no_child_cpu(pid: i32) -> Session {
    let mut session = session(pid);
    session.resources = Ok(Resources {
        children_cpu: Err(Unmeasured::NotReportedBy(ResourceSource::Ps)),
        ..measured_ledger()
    });
    session
}

/// A session whose only child-CPU figure is the one an earlier run wrote down.
fn session_remembering(pid: i32, children_cpu: Duration) -> Session {
    let mut session = session(pid);
    session.resources = Err(ResourcesUnavailable::ProcessExited);
    session.last_reading = Some(acmon::memory::Reading {
        resources: Resources {
            children_cpu: Ok(children_cpu),
            ..measured_ledger()
        },
        taken_at: fixture_now() - Duration::from_secs(600),
    });
    session
}

/// The pids of a snapshot's sessions, in the order the display draws them.
fn drawn_order(sessions: &[Session]) -> Vec<String> {
    in_cost_order(sessions)
        .into_iter()
        .map(|session| match &session.identity {
            Identity::Process { pid } => pid.to_string(),
            Identity::Transcript { recorded_as } => recorded_as.clone(),
        })
        .collect()
}

/// Where a string first appears in a rendered screen.
fn line_of(lines: &[String], needle: &str) -> usize {
    lines
        .iter()
        .position(|line| line.contains(needle))
        .unwrap_or_else(|| panic!("{needle:?} is not on this screen:\n{}", lines.join("\n")))
}

#[test]
fn the_session_costing_the_machine_most_is_drawn_first() {
    // F55. Ordering by pid, as the first implementation did, carries no information: the one
    // session that spent 32,317 s in its children is the answer to the question this tool
    // exists to ask, and it belongs at the top.
    let sessions = vec![
        session_costing(11, Duration::from_secs(19)),
        session_costing(22, Duration::from_secs(32_317)),
        session_costing(33, Duration::from_secs(600)),
    ];

    assert_eq!(
        drawn_order(&sessions),
        vec!["22", "33", "11"],
        "child CPU descending, whatever order the collection produced"
    );
}

#[test]
fn a_row_is_ordered_by_the_child_cpu_figure_it_actually_shows() {
    // A remembered figure is a measurement — the only one that will ever exist for a session
    // whose process has gone, since only the process that reaped the children could report it.
    // Sorting on a live reading alone would order the table by a figure that is not the one
    // printed in it, and the reader would have no way to see that.
    let sessions = vec![
        session_costing(11, Duration::from_secs(60)),
        session_remembering(22, Duration::from_secs(8_000)),
    ];

    assert_eq!(
        cost_of(&sessions[1]),
        Cost::Measured(Duration::from_secs(8_000)),
        "the remembered figure is the cost this row has"
    );
    assert_eq!(drawn_order(&sessions), vec!["22", "11"]);
}

#[test]
fn a_child_cpu_nobody_could_measure_is_never_ordered_as_the_cheapest() {
    // NF10, and the reason it matters here rather than merely being tidy: the cheap end of this
    // table is what a terminal too short drops. A cost of `None` folded into `0` would rank the
    // least knowable session as the first to disappear.
    let sessions = vec![
        session_costing(11, Duration::from_secs(1)),
        session_with_no_child_cpu(22),
        session_costing(33, Duration::from_secs(32_317)),
    ];

    assert!(
        matches!(cost_of(&sessions[1]), Cost::Unmeasurable(_)),
        "an absent figure is not a duration"
    );
    assert_eq!(
        drawn_order(&sessions),
        vec!["22", "33", "11"],
        "the session with no figure sorts above every session that has one"
    );
}

#[test]
fn the_screen_states_where_a_session_with_no_child_cpu_figure_was_put() {
    // "A stated position", not merely a defensible one. A reader seeing a reason where the
    // largest total should be would otherwise conclude the table is ordered by something else.
    let mut snapshot = ordinary_snapshot();
    snapshot.sessions = vec![
        session_costing(11, Duration::from_secs(1)),
        session_with_no_child_cpu(22),
    ];

    let screen = Screen::from_own_collection(
        &no_monitor(),
        Ok(snapshot),
        Duration::from_millis(9),
        fixture_now(),
    );
    let text = prose(&screen, wide());

    assert!(
        text.contains("ordered by child CPU, descending"),
        "the order itself is stated; got:\n{text}"
    );
    assert!(
        text.contains("listed FIRST rather than last"),
        "and so is where an absent figure goes; got:\n{text}"
    );
}

#[test]
fn sessions_that_cost_the_same_keep_the_order_the_collection_gave_them() {
    // Determinism including ties, which the collection's own sort exists for: a tie broken
    // arbitrarily is a flaky test waiting to happen. The collection hands the display processes
    // by pid, so equal costs come out in pid order — and come out that way every run.
    let sessions = vec![
        session_costing(264, Duration::from_secs(600)),
        session_costing(2_880, Duration::from_secs(600)),
        session_costing(5_333, Duration::from_secs(600)),
    ];

    let once = drawn_order(&sessions);
    assert_eq!(once, vec!["264", "2880", "5333"], "the order it was given");
    for _ in 0..8 {
        assert_eq!(
            drawn_order(&sessions),
            once,
            "and the same order every time it is asked"
        );
    }
}

#[test]
fn the_table_draws_its_rows_in_the_order_the_sort_decided() {
    // The sort is only worth anything if the drawing obeys it, and this is the one assertion
    // that ties the two together.
    let mut snapshot = ordinary_snapshot();
    snapshot.sessions = vec![
        session_costing(11, Duration::from_secs(19)),
        session_costing(22, Duration::from_secs(32_317)),
        session_costing(33, Duration::from_secs(600)),
    ];

    let screen = Screen::from_own_collection(
        &no_monitor(),
        Ok(snapshot),
        Duration::from_millis(9),
        fixture_now(),
    );
    let lines = screen_to_lines(&screen, wide(), screen_height(&screen, wide()));

    assert!(
        line_of(&lines, "8h58m") < line_of(&lines, "10m00s"),
        "the costliest row precedes the middling one; got:\n{}",
        lines.join("\n")
    );
    assert!(
        line_of(&lines, "10m00s") < line_of(&lines, "19.0s"),
        "and the middling one precedes the cheapest; got:\n{}",
        lines.join("\n")
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
        &no_monitor(),
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
        &no_monitor(),
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
        &no_monitor(),
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
        &no_monitor(),
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
    for presence in [
        no_monitor(),
        Presence::Absent(Absence::NothingPublished {
            writer_pid: 31_337,
            why: "no tier yet".to_string(),
        }),
        unbelievable("torn"),
    ] {
        let screen = Screen::from_own_collection(
            &presence,
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
            "for {presence:?} the screen has to say the figures are its own read; got:\n{text}"
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
        &no_monitor(),
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
        &unbelievable("torn"),
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
        &no_monitor(),
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

// --- A screen too short: what goes, and what says so -----------------------------------------

/// A session whose CLI has no transcript store at all, so its state can never be determined and
/// it can therefore never be announced (#22) — and whose child CPU the coarse reader could not
/// see either, so it has no cost to be ordered by.
///
/// Both at once because both are true of the same session on this machine: a user-configured
/// fifth CLI is the case with no store, and the fallback reader is what answers when `rusage`
/// refuses. It is the row that must survive every cut.
fn session_with_no_transcript_store(pid: i32) -> Session {
    Session {
        identity: Identity::Process { pid },
        cli: "cursor-agent".to_string(),
        resources: Ok(Resources {
            children_cpu: Err(Unmeasured::NotReportedBy(ResourceSource::Ps)),
            ..measured_ledger()
        }),
        last_reading: None,
        workspace: Ok(Workspace {
            path: "/Users/pmcfadin/projects/testing".to_string(),
            namespace: Err(acmon::workspace::NamespaceUnmatched::UnknownCli(
                "cursor-agent".to_string(),
            )),
        }),
        liveness: Verdict {
            state: State::Unknown,
            method: Method::TranscriptActivityUnknown,
        },
    }
}

/// The costs the crowded snapshot's sessions carry, cheapest first, as the row shows them.
const CROWDED_COSTS: [(i32, u64, &str); 6] = [
    (101, 19, "19.0s"),
    (202, 90, "1m30s"),
    (303, 600, "10m00s"),
    (404, 4_000, "1h06m"),
    (505, 20_000, "5h33m"),
    (606, 32_317, "8h58m"),
];

/// A screen that does not fit a short terminal, carrying everything that must survive one.
///
/// Six sessions of distinct cost, a seventh whose state can never be determined (#22), five
/// stranded workspaces, alerts that never reached a channel (#20), and a dedupe record that
/// could not be read (#29). Every one of those warnings is a thing a reader acts on, and a
/// screen that dropped one to save a row would be the failure this project exists to remove.
fn crowded_snapshot() -> Snapshot {
    let mut snapshot = ordinary_snapshot();
    snapshot.sessions = CROWDED_COSTS
        .iter()
        .map(|(pid, cost, _)| session_costing(*pid, Duration::from_secs(*cost)))
        .collect();
    snapshot
        .sessions
        .push(session_with_no_transcript_store(900));

    snapshot.workspaces.extend((1..=5).map(|n| WorkspaceReport {
        path: format!("/Users/pmcfadin/projects/stranded-{n}"),
        state: WorkspaceState::DirtyStranded,
        linked_worktree: false,
        uncommitted_entries: Some(n * 3),
    }));

    snapshot.remembered.notify_health.notable = 6;
    snapshot.remembered.notify_health.local_not_attempted = 6;
    snapshot.remembered.notify_health.not_attempted_reason =
        Some("the alerting step ran out of its budget".to_string());
    snapshot.remembered.notified.rebuilt = Some(acmon::notify::Rebuilt::Unreadable(
        "notified.json could not be read".to_string(),
    ));
    snapshot
}

fn crowded_screen() -> Screen {
    Screen::from_own_collection(
        &no_monitor(),
        Ok(crowded_snapshot()),
        Duration::from_millis(2_470),
        fixture_now(),
    )
}

/// Whether the screen says its own bottom is cut — the last resort, below which no promise
/// about what is visible can be kept.
fn says_the_bottom_is_cut(text: &str) -> bool {
    text.contains("ITS BOTTOM IS CUT")
}

#[test]
fn a_terminal_too_short_drops_the_cheapest_sessions_and_says_how_many() {
    // F54. The tool's own numbers, from the mechanics document: ten sessions and eleven at-risk
    // workspaces need about thirty rows, so a twenty-four-row pane is the common case rather
    // than the edge — and refusing to draw at all there would be the "fail to zero" this
    // project exists to remove, aimed at our own display.
    let screen = crowded_screen();
    let width = wide();
    let full = screen_height(&screen, width);
    let text = prose_at(&screen, width, full - 1);

    assert!(
        text.contains("sessions not shown"),
        "a cut the reader is not told about is the whole failure here; got:\n{text}"
    );
    assert!(
        text.contains("8h58m"),
        "the costliest session is the one kept; got:\n{text}"
    );
    assert!(
        !text.contains("19.0s"),
        "and the cheapest is the one dropped; got:\n{text}"
    );
}

#[test]
fn the_count_of_sessions_not_shown_is_the_number_actually_dropped() {
    // The number is the whole content of the statement. A count that drifted from what was cut
    // would be worse than no count at all: it reads as an accounting of the screen.
    let screen = crowded_screen();
    let width = wide();
    let full = screen_height(&screen, width);
    // Six sessions carry a cost figure; a seventh has none and therefore sorts above all of
    // them, so it is the last row to go rather than the first.
    let total = 7;

    let mut heights_that_stated_a_count = 0;
    for height in 1..=full {
        let text = prose_at(&screen, width, height);
        let Some(stated) = text
            .split_once(" sessions not shown")
            .and_then(|(before, _)| before.rsplit('+').next()?.trim().parse::<usize>().ok())
        else {
            continue;
        };
        heights_that_stated_a_count += 1;

        let dropped = if text.contains("OWN CPU") {
            CROWDED_COSTS
                .iter()
                .filter(|(_, _, figure)| !text.contains(figure))
                .count()
        } else {
            total
        };
        assert_eq!(
            stated, dropped,
            "at height {height} the screen says {stated} sessions are not shown and {dropped} \
             are; got:\n{text}"
        );
    }
    assert!(
        heights_that_stated_a_count > 5,
        "this fixture is meant to be cut at most of these heights"
    );
}

#[test]
fn the_rows_that_survive_a_short_terminal_are_the_costliest_ones_in_order() {
    // Asserted as an invariant over every height rather than at one, because "cheapest first"
    // is a property of the whole ladder: at each height the set on screen is a prefix of the
    // order, never a sample of it.
    let screen = crowded_screen();
    let width = wide();
    let full = screen_height(&screen, width);

    for height in 1..=full {
        let text = prose_at(&screen, width, height);
        let shown: Vec<&str> = CROWDED_COSTS
            .iter()
            .rev()
            .map(|(_, _, figure)| *figure)
            .filter(|figure| text.contains(*figure))
            .collect();
        let expected: Vec<&str> = CROWDED_COSTS
            .iter()
            .rev()
            .map(|(_, _, figure)| *figure)
            .take(shown.len())
            .collect();
        assert_eq!(
            shown, expected,
            "at height {height} the rows on screen must be the costliest ones, in order; \
             got:\n{text}"
        );
        if text.contains("OWN CPU") {
            // `ps-blind` is printed where that row's child CPU would be, and nowhere else on
            // this screen — the footer's note about the same session names it by pid instead.
            assert!(
                text.contains("ps-blind"),
                "and the session with no cost figure at all is never the one dropped, at height \
                 {height}; got:\n{text}"
            );
        }
    }
}

#[test]
fn the_at_risk_panel_is_whole_at_every_height_that_can_hold_it() {
    // F32. The panel is the reason this project exists — three sessions' work was lost in one
    // day, and a workspace holding 27 uncommitted files was deleted minutes after sitting
    // unflagged. It is never the thing that gives way, and where the terminal is too short even
    // for it, the screen says so at the top instead of quietly showing four of five rows.
    let screen = crowded_screen();
    let width = wide();
    let full = screen_height(&screen, width);

    for height in 1..=full {
        let text = prose_at(&screen, width, height);
        if says_the_bottom_is_cut(&text) {
            continue;
        }
        for n in 1..=5 {
            assert!(
                text.contains(&format!("stranded-{n}")),
                "at height {height} the panel dropped a stranded workspace without saying the \
                 screen was cut; got:\n{text}"
            );
        }
        assert!(
            text.contains("at risk — 5 of 6 workspaces"),
            "and the panel keeps its denominator, at height {height}; got:\n{text}"
        );
    }
}

#[test]
fn every_warning_survives_a_terminal_too_short_for_the_table() {
    // #20, #22 and #29 all put a warning on this screen, and each of them is the difference
    // between a machine a reader can account for and one they cannot. A session row is
    // recoverable by lengthening the terminal; a warning nobody saw is not.
    let screen = crowded_screen();
    let width = wide();
    let full = screen_height(&screen, width);

    for height in 1..=full {
        let text = prose_at(&screen, width, height);
        if says_the_bottom_is_cut(&text) {
            continue;
        }
        for warning in [
            "no transcript store is known for CLI cursor-agent",
            "never announced",
            "NOT SENT",
            "ran out of its budget",
            "NOTHING was deduped",
        ] {
            assert!(
                text.contains(warning),
                "at height {height} the screen lost {warning:?}, which no cut is allowed to \
                 take; got:\n{text}"
            );
        }
    }
}

#[test]
fn a_table_with_no_room_for_one_row_is_not_drawn_as_headings_with_nothing_under_them() {
    // The plausible wrong answer in its purest form: PID, CLI, STATE and five empty columns is
    // exactly what a machine with no agents running on it looks like.
    let screen = crowded_screen();
    let width = wide();
    let full = screen_height(&screen, width);

    let heights: Vec<u16> = (1..=full)
        .filter(|height| prose_at(&screen, width, *height).contains("not one row fits"))
        .collect();
    assert!(
        !heights.is_empty(),
        "this fixture is meant to reach the height where no row fits at all"
    );

    for height in heights {
        let text = prose_at(&screen, width, height);
        assert!(
            !text.contains("OWN CPU"),
            "at height {height} the column headings are drawn over nothing; got:\n{text}"
        );
        assert!(
            text.contains("+7 sessions not shown"),
            "and every session it is not showing is accounted for; got:\n{text}"
        );
    }
}

#[test]
fn a_terminal_far_too_short_says_it_is_cut_rather_than_cutting_silently() {
    // The floor of the ladder. There is a height below which something has to be lost — the
    // requirement is that the screen says so, in the one place a clip cannot reach.
    let screen = crowded_screen();
    let lines = screen_to_lines(&screen, wide(), 6);
    let text = lines.join(" ");

    assert!(says_the_bottom_is_cut(&text), "got:\n{}", lines.join("\n"));
    assert!(
        lines[0].contains("TOO SHORT"),
        "and says it on the first line, because the bottom is what is missing; got:\n{}",
        lines.join("\n")
    );
    assert!(
        text.contains("+7 sessions not shown"),
        "the notice keeps its own count even here, where it is most of the screen; got:\n{}",
        lines.join("\n")
    );
}

#[test]
fn no_line_runs_off_the_side_at_any_height() {
    // The two axes are independent: what a short terminal drops must not be paid for by a line
    // that overflows a narrow one.
    let screen = crowded_screen();

    for width in [minimum_width(), minimum_width() + 23, wide()] {
        let full = screen_height(&screen, width);
        for height in [1, 6, 12, full / 2, full - 1, full, full + 10] {
            for line in screen_to_lines(&screen, width, height) {
                assert!(
                    line.chars().count() <= width as usize,
                    "at {width}x{height} a line of {} characters does not fit: {line}",
                    line.chars().count()
                );
            }
        }
    }
}

#[test]
fn a_terminal_tall_enough_for_everything_says_nothing_about_being_short() {
    // The notice is a warning, and a warning printed when nothing is wrong is a warning a
    // reader learns to skip.
    let screen = crowded_screen();
    let width = wide();
    let full = screen_height(&screen, width);

    for height in [full, full + 1, full + 40] {
        let text = prose_at(&screen, width, height);
        assert!(
            !text.contains("TOO SHORT") && !text.contains("not shown"),
            "at height {height} nothing was dropped, so nothing should be said; got:\n{text}"
        );
        for (_, _, figure) in CROWDED_COSTS {
            assert!(
                text.contains(figure),
                "and every row is on screen at height {height}; got:\n{text}"
            );
        }
    }
}

#[test]
fn no_figure_in_a_surviving_row_is_shortened_to_help_a_short_terminal() {
    // The width rule is unchanged by the height rule: a number is printed in full or its row is
    // not drawn. A truncated CPU total is a plausible wrong answer, and a screen that shortened
    // one to fit another row in would be trading the reason it exists for a row.
    let screen = crowded_screen();
    let width = wide();
    let full = screen_height(&screen, width);
    let text = prose_at(&screen, width, full - 1);

    assert!(
        text.contains("8h58m") && text.contains("27m49s"),
        "the costliest row's figures are whole; got:\n{text}"
    );
    assert!(
        text.contains("482 MB") && text.contains("622 MB") && text.contains("166 MB"),
        "and so are its byte counts; got:\n{text}"
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
    //
    // The order the table is drawn in is therefore fixed: child CPU, descending, stated in the
    // table's own title. A single correct order costs a reader nothing, and it is what keeps
    // `agtop --once` emitting the same content as the live view.
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

// --- How old the facts are, and whether the monitor is still there ---------------------------
//
// The failure this section exists to prevent is the one PRD §2.2 opens with, reproduced inside
// this tool: `amon` wedges forty minutes ago and `state.json` still reads as perfectly healthy.
//
// Every test here drives the decision as a function over stamps and a clock. **Nothing sleeps.**
// An age is a figure computed from two instants, so ageing a fixture by waiting would assert the
// scheduler's accuracy rather than the display's arithmetic — and on this class of machine that
// varies by 2x between runs. The pid check is a parameter for the same reason: a transition that
// could only be tested by killing a process is a transition nobody would test.

/// The instants these fixtures use, relative to [`fixture_now`].
fn ago(seconds: u64) -> SystemTime {
    fixture_now() - Duration::from_secs(seconds)
}

fn iso(time: SystemTime) -> String {
    acmon::isotime::iso8601_from_unix_seconds(acmon::isotime::unix_seconds(time))
}

/// A state file a monitor could have written, with every stamp under the test's control.
///
/// Built from the payload structs rather than by running a collection, deliberately: what is
/// asserted below is what a display concludes from *stamps*, and no collection can be asked to
/// have happened forty minutes ago. That the payload structs are what the monitor really writes
/// is `seam16`'s round-trip test, not this one's.
struct Publication {
    writer_pid: u32,
    pace: String,
    every_tier_has_run: bool,
    /// When each tier's most recent pass started. `None` for a tier that has not published.
    fast_pass: SystemTime,
    medium_pass: Option<SystemTime>,
    slow_pass: Option<SystemTime>,
    /// The oldest workspace reading the slow tier published — its **stamp**, which is emphatically
    /// not its last pass. On a machine with seventy repositories these are twenty minutes apart.
    slow_evidence: SystemTime,
    sessions: Vec<SessionRow>,
    workspaces: Vec<WorkspaceRow>,
    never_read: usize,
}

impl Publication {
    /// A healthy monitor: every tier published at `fixture_now`, one session, one clean
    /// workspace, nothing warming up.
    fn healthy() -> Publication {
        Publication {
            writer_pid: 31_337,
            pace: "active".to_string(),
            every_tier_has_run: true,
            fast_pass: fixture_now(),
            medium_pass: Some(fixture_now()),
            slow_pass: Some(fixture_now()),
            slow_evidence: fixture_now(),
            sessions: vec![published_session()],
            workspaces: vec![published_workspace(
                "/Users/pmcfadin/projects/agentic_coding_monitor",
                "CLEAN",
                false,
                Some(0),
                Some(fixture_now()),
            )],
            never_read: 0,
        }
    }

    fn into_state(self) -> TieredState {
        let mut state = TieredState::new(self.writer_pid);
        state.set_tier_data(
            Tier::Fast,
            // The one place in this file that names every field of a published payload, so a
            // field added to the contract shows up here and nowhere else.
            serde_json::to_value(FastPayload {
                pass: envelope("fast", self.fast_pass, self.every_tier_has_run),
                sessions: self.sessions,
                monitor: self_report(&self.pace),
                notify: NotifyRow {
                    notable: 0,
                    delivered: 0,
                    failed: 0,
                    not_attempted: 0,
                    reason: None,
                    read_only: None,
                },
                silence_read_by: "medium".to_string(),
            })
            .expect("a fast payload serialises"),
            self.fast_pass,
        );
        if let Some(at) = self.medium_pass {
            state.set_tier_data(
                Tier::Medium,
                serde_json::to_value(MediumPayload {
                    pass: envelope("medium", at, self.every_tier_has_run),
                    directories_visited: 812,
                    sweep_complete: true,
                    repositories_found: 34,
                    unlocated: Vec::new(),
                })
                .expect("a medium payload serialises"),
                at,
            );
        }
        if let Some(at) = self.slow_pass {
            let at_risk = self.workspaces.iter().filter(|row| row.at_risk).count();
            state.set_tier_data(
                Tier::Slow,
                serde_json::to_value(SlowPayload {
                    pass: envelope("slow", at, self.every_tier_has_run),
                    at_risk,
                    workspaces: self.workspaces,
                    read_this_pass: 5,
                    never_read: self.never_read,
                    next_slice: 5,
                })
                .expect("a slow payload serialises"),
                // The tier's stamp is the oldest evidence in it, which is what `tiers::stamp_for`
                // publishes for this tier and the whole reason F30 needs two ages.
                self.slow_evidence,
            );
        }
        state
    }

    /// The reading a display gets from this file.
    fn read(self) -> PublishedReading {
        match reading_of(&self.into_state()) {
            StateReading::Published(reading) => *reading,
            other => panic!("this fixture has to decode as published facts; got {other:?}"),
        }
    }
}

fn envelope(tier: &str, started_at: SystemTime, every_tier_has_run: bool) -> PassEnvelope {
    PassEnvelope {
        tier: tier.to_string(),
        sequence: 7,
        started_at: iso(started_at),
        took_ms: 49,
        budget_ms: 150,
        overran: None,
        load: Measured::known(LoadAverage {
            one_minute: 4.9,
            five_minute: 5.2,
            fifteen_minute: 4.4,
            cpus: 16,
        }),
        every_tier_has_run,
        announcing: every_tier_has_run,
    }
}

fn self_report(pace: &str) -> SelfReport {
    SelfReport {
        own_cpu_ms: Measured::known(412),
        window_secs: 60,
        duty_cycle: Measured::known(0.004_13),
        busy_fraction: Measured::known(0.006),
        budget: 0.01,
        within_budget: Measured::known(true),
        pace: pace.to_string(),
        passes: vec![("fast".to_string(), 7)],
        last_pass: vec![PassReport {
            tier: "fast".to_string(),
            took_ms: 49,
            budget_ms: 150,
            overran: None,
            age_ms: 0,
        }],
    }
}

/// One session as the monitor publishes it: measured, active, in a known workspace.
fn published_session() -> SessionRow {
    SessionRow {
        pid: Some(69_046),
        recorded_as: None,
        cli: "claude".to_string(),
        state: "ACTIVE".to_string(),
        method: "the transcript changed recently".to_string(),
        inferred: false,
        unknown_why: None,
        unknown_is_structural: false,
        workspace: Measured::known("/Users/pmcfadin/projects/agentic_coding_monitor".to_string()),
        own_cpu_ms: Measured::known(1_669_000),
        children_cpu_ms: Measured::known(32_317_000),
        current_memory_bytes: Measured::known(482_000_000),
        peak_memory_bytes: Measured::known(622_000_000),
        bytes_written: Measured::known(166_000_000),
        source: Measured::known("proc_pid_rusage".to_string()),
        remembered_at: None,
    }
}

fn published_workspace(
    path: &str,
    state: &str,
    at_risk: bool,
    entries: Option<usize>,
    observed_at: Option<SystemTime>,
) -> WorkspaceRow {
    WorkspaceRow {
        path: path.to_string(),
        state: state.to_string(),
        at_risk,
        unknown_why: if state == "UNKNOWN" {
            Some("not read yet".to_string())
        } else {
            None
        },
        uncommitted_entries: entries,
        linked_worktree: false,
        observed_at: observed_at.map(iso),
    }
}

/// The screen a display draws from a published file, at `now`, with the writer's liveness given.
fn published_screen(reading: &PublishedReading, now: SystemTime, writer: Writer) -> Screen {
    let presence = presence_of(
        &StateReading::Published(Box::new(reading.clone())),
        now,
        writer,
    );
    Screen::from_published(&presence, reading)
}

#[test]
fn a_monitor_publishing_inside_every_interval_is_fresh_and_says_what_that_rests_on() {
    let reading = Publication::healthy().read();
    let screen = published_screen(&reading, fixture_now(), Writer::Alive);

    assert_eq!(screen.presence.label(), "FRESH");
    assert!(screen.presence.trustworthy());

    let text = prose(&screen, wide());
    assert!(
        text.contains("FRESH — the monitor at pid 31337 is running"),
        "the screen names the monitor it is drawing; got:\n{text}"
    );
    assert!(
        text.contains(
            "the writer pid still exists and every tier published inside its own \
                       cadence"
        ),
        "and which two observations the verdict rests on — a verdict whose evidence is unstated \
         is an assertion; got:\n{text}"
    );
    assert!(
        text.contains("69046"),
        "and it draws the monitor's own session rows rather than collecting for itself; \
         got:\n{text}"
    );
    assert!(
        !text.contains("this display took for itself"),
        "a screen drawn from published facts must never claim they are its own; got:\n{text}"
    );
}

#[test]
fn a_tier_that_has_missed_a_whole_pass_marks_the_whole_screen_and_never_a_row() {
    // F29. A stale file is uniformly untrustworthy: every row in it came out of the same write,
    // so marking rows individually would imply the unmarked ones had been re-verified.
    let mut publication = Publication::healthy();
    // The fast tier has not run for well past the point where a pass was missed. Expressed as a
    // multiple of the interval it is judged against, so the fixture cannot drift from the rule.
    let missed = Cadence::ACTIVE.fast * (MISSED_A_PASS + 1);
    publication.fast_pass = fixture_now() - missed;
    let reading = publication.read();

    let screen = published_screen(&reading, fixture_now(), Writer::Alive);
    assert_eq!(screen.presence.label(), "STALE");
    assert!(
        !screen.presence.trustworthy(),
        "a stale screen may not be described as trustworthy"
    );

    let lines = screen_to_lines(&screen, wide(), screen_height(&screen, wide()));
    let mark = lines
        .iter()
        .position(|line| line.contains("THIS WHOLE SCREEN IS STALE"))
        .expect("the whole-screen mark has to be on the screen");
    let first_row = lines
        .iter()
        .position(|line| line.contains("69046"))
        .expect("the rows are still drawn — a stale fact is still a fact, said as one");
    assert!(
        mark < first_row,
        "the mark has to be above everything it is about, at line {mark} against {first_row}"
    );
    assert!(
        !lines[first_row].contains("STALE"),
        "no row carries a freshness judgement of its own; got:\n{}",
        lines[first_row]
    );
}

#[test]
fn a_dead_monitor_is_never_drawn_as_a_live_one_however_fresh_the_file_it_left() {
    // The `SIGKILL` case at the far end: the pid is reaped a moment after the last write, so the
    // stamps are seconds old and the monitor is gone. Age alone would call this FRESH.
    let reading = Publication::healthy().read();
    let screen = published_screen(&reading, fixture_now(), Writer::Gone);

    assert_eq!(
        screen.presence.label(),
        "DEAD",
        "a pid that no longer exists outranks every stamp in the file: nothing will refresh it"
    );
    let text = prose(&screen, wide());
    assert!(
        text.contains("THE MONITOR IS DEAD"),
        "and the screen leads with it; got:\n{text}"
    );
    assert!(
        text.contains("asking the kernel about the writer pid"),
        "stating that this verdict is an observation about a pid rather than arithmetic over \
         stamps; got:\n{text}"
    );
    assert!(
        text.contains("nothing is being recorded") && text.contains("will be announced"),
        "a reader has to be told that alerts have stopped too; got:\n{text}"
    );
}

#[test]
fn a_monitor_that_is_gone_and_one_that_is_merely_slow_are_told_apart_in_words() {
    // The two sentences a reader most needs to be able to distinguish. Both exist, and neither
    // is a shortened version of the other.
    let mut slow = Publication::healthy();
    slow.fast_pass = fixture_now() - Cadence::ACTIVE.fast * (MISSED_A_PASS + 1);
    let stale = prose(
        &published_screen(&slow.read(), fixture_now(), Writer::Alive),
        wide(),
    );
    let dead = prose(
        &published_screen(&Publication::healthy().read(), fixture_now(), Writer::Gone),
        wide(),
    );

    assert!(
        stale.contains("still exists") && stale.contains("may yet catch up"),
        "a stale monitor is alive, and the screen has to say a refresh is still possible; \
         got:\n{stale}"
    );
    assert!(
        !stale.contains("CORPSE") && !stale.contains("THE MONITOR IS DEAD"),
        "and must not be described as dead; got:\n{stale}"
    );
    assert!(
        dead.contains("EVERY FIGURE BELOW IS A CORPSE") && dead.contains("will ever be refreshed"),
        "a dead monitor's screen has to say the figures will never improve; got:\n{dead}"
    );
    assert!(
        !dead.contains("may yet catch up"),
        "and must not offer the hope a stale one does; got:\n{dead}"
    );
}

#[test]
fn the_screen_goes_fresh_then_stale_then_dead_as_the_same_file_ages_and_its_writer_goes() {
    // S8 as an ordering over stamps rather than a stopwatch: one file, three clocks, and the pid
    // check flipped once. No process is killed and nothing sleeps.
    let reading = Publication::healthy().read();
    let interval = Cadence::ACTIVE.fast;

    let verdicts: Vec<&'static str> = [
        // Inside its interval, and one interval old — a tier due right now is not a late one.
        (interval, Writer::Alive),
        (
            interval * MISSED_A_PASS + Duration::from_secs(1),
            Writer::Alive,
        ),
        (
            interval * MISSED_A_PASS + Duration::from_secs(1),
            Writer::Gone,
        ),
    ]
    .iter()
    .map(|(age, writer)| {
        published_screen(&reading, fixture_now() + *age, *writer)
            .presence
            .label()
    })
    .collect();

    assert_eq!(
        verdicts,
        vec!["FRESH", "STALE", "DEAD"],
        "the same file has to pass through all three as it ages and its writer goes"
    );
}

#[test]
fn every_tier_s_age_is_drawn_from_that_tier_s_own_stamp() {
    // F30. Three tiers, three ages, and the relationship between them is what reproduces — never
    // an absolute figure.
    let mut publication = Publication::healthy();
    publication.fast_pass = ago(5);
    publication.medium_pass = Some(ago(50));
    publication.slow_pass = Some(ago(12));
    publication.slow_evidence = ago(1_500);
    let reading = publication.read();

    let ages = ages_of(&reading, fixture_now());
    let age = |tier: Tier| -> TierAge { ages.of(tier).expect("every tier published").clone() };

    assert_eq!(age(Tier::Fast).since_evidence, Ok(Duration::from_secs(5)));
    assert_eq!(
        age(Tier::Medium).since_evidence,
        Ok(Duration::from_secs(50))
    );
    assert!(
        age(Tier::Slow).since_evidence.clone().expect("published")
            > age(Tier::Medium).since_evidence.clone().expect("published"),
        "the slow tier's evidence is the oldest thing in the file, and its age has to say so"
    );
    assert!(
        age(Tier::Slow).since_pass.clone().expect("published")
            < age(Tier::Slow).since_evidence.clone().expect("published"),
        "and its last pass is far younger than its oldest fact — one number for both would have \
         to be wrong about one of them"
    );
    assert_eq!(
        ages.oldest_evidence(),
        Some(Duration::from_secs(1_500)),
        "the oldest fact on the screen is what a whole-screen mark has to state"
    );

    let text = prose(
        &published_screen(&reading, fixture_now(), Writer::Alive),
        wide(),
    );
    for tier in ["fast", "medium", "slow"] {
        assert!(
            text.contains(&format!("{tier} last pass")),
            "each tier's age is on the screen, drawn from its own stamp; {tier} is missing \
             from:\n{text}"
        );
    }
}

#[test]
fn a_session_s_liveness_ages_against_the_medium_tier_rather_than_the_fast_one() {
    // The correction #27 handed forward. The rows are the fast tier's, but a WAITING rests on
    // transcript silence read by the *medium* tier, so ageing that evidence against the fast
    // stamp would report ten-minute-old silence as ten seconds old.
    let mut publication = Publication::healthy();
    publication.fast_pass = ago(3);
    publication.medium_pass = Some(ago(45));
    let reading = publication.read();

    assert_eq!(reading.fast.silence_read_by, "medium");
    let text = prose(
        &published_screen(&reading, fixture_now(), Writer::Alive),
        wide(),
    );
    assert!(
        text.contains("rests on transcript activity read by the medium tier"),
        "the screen has to name the tier a state's evidence ages against; got:\n{text}"
    );
    assert!(
        text.contains("not as old as this table's"),
        "and say plainly that it is not the table's own age; got:\n{text}"
    );
}

#[test]
fn the_at_risk_panel_states_the_slow_tier_s_evidence_age_and_not_the_newest_write() {
    // S13, and the criterion in words: a workspace committed 50 s ago must not appear at-risk
    // under a 1 s stamp. The panel is fed by the slowest tier, so its age is that tier's oldest
    // reading — never the fast tier's, however recently the file was rewritten.
    let mut publication = Publication::healthy();
    publication.fast_pass = fixture_now();
    publication.slow_pass = Some(ago(30));
    publication.slow_evidence = ago(1_320);
    publication.workspaces = vec![published_workspace(
        "/Users/pmcfadin/projects/workforceos",
        "DIRTY-STRANDED",
        true,
        Some(27),
        Some(ago(1_320)),
    )];
    let reading = publication.read();

    let text = prose(
        &published_screen(&reading, fixture_now(), Writer::Alive),
        wide(),
    );
    let expected = acmon::render::format_age(Duration::from_secs(1_320));

    assert!(
        text.contains(&format!("slow-tier evidence, oldest {expected} old")),
        "the panel states its own evidence age beside it; got:\n{text}"
    );
    assert!(
        text.contains(&format!("{expected} /Users/pmcfadin/projects/workforceos")),
        "and each row carries the age of the reading it rests on, not the file's; got:\n{text}"
    );
    assert!(
        !text.contains("oldest 0s old"),
        "the panel must never take the fast tier's freshness for its own; got:\n{text}"
    );
}

#[test]
fn a_workspace_the_slow_tier_has_never_read_is_counted_and_never_called_clean() {
    let mut publication = Publication::healthy();
    publication.workspaces = vec![published_workspace(
        "/Users/pmcfadin/projects/never-looked-at",
        "UNKNOWN",
        false,
        None,
        None,
    )];
    publication.never_read = 1;
    let reading = publication.read();

    let text = prose(
        &published_screen(&reading, fixture_now(), Writer::Alive),
        wide(),
    );
    assert!(
        text.contains("1 workspace(s) have never been read") && text.contains("NOT exhaustive"),
        "a panel that has not finished looking may not present itself as a clear result; \
         got:\n{text}"
    );
    assert!(
        !text.contains("all checked workspaces are clean"),
        "and must not describe an unread workspace as a clean one; got:\n{text}"
    );
    assert!(
        text.contains("not read yet"),
        "and the state it is accounted under is the monitor's own reason rather than a silence; \
         got:\n{text}"
    );
    assert!(
        text.contains("1 of 1 could not be read"),
        "with the denominator said out loud, because 0 at risk means nothing when nothing was \
         checked; got:\n{text}"
    );
}

#[test]
fn an_idled_down_monitor_is_not_called_stale_for_keeping_its_own_slower_cadence() {
    // F22: with no session live the loop idles down, and the fast tier legitimately runs once a
    // minute. Judged against the active cadence the same file would read as STALE — which is a
    // monitor doing exactly what it was told to, reported as broken.
    let mut publication = Publication::healthy();
    publication.pace = "idle".to_string();
    publication.fast_pass = fixture_now() - Cadence::ACTIVE.fast * (MISSED_A_PASS + 1);
    let reading = publication.read();

    let presence = presence_of(
        &StateReading::Published(Box::new(reading.clone())),
        fixture_now(),
        Writer::Alive,
    );
    assert_eq!(presence.label(), "FRESH");
    assert_eq!(
        presence
            .ages()
            .expect("published")
            .of(Tier::Fast)
            .expect("published")
            .interval,
        Cadence::IDLE.fast,
        "the ages are judged against the cadence the monitor said it was keeping"
    );
}

#[test]
fn a_cadence_word_this_build_does_not_know_is_said_out_loud_rather_than_guessed() {
    let mut publication = Publication::healthy();
    publication.pace = "hibernating".to_string();
    let reading = publication.read();
    let screen = published_screen(&reading, fixture_now(), Writer::Alive);

    let ages = screen.presence.ages().expect("published");
    assert!(ages.pace_unknown);
    assert_eq!(
        ages.cadence,
        Cadence::IDLE,
        "an unknown word is judged against the slower cadence, so it cannot manufacture a \
         staleness warning"
    );
    let text = prose(&screen, wide());
    assert!(
        text.contains("hibernating") && text.contains("does not know"),
        "and it is never silent about having done so; got:\n{text}"
    );
}

#[test]
fn a_tier_stamped_in_the_future_has_no_age_rather_than_a_fresh_one() {
    // A clock that moved backwards between the monitor's write and this read. Zero would present
    // the least trustworthy stamp on the screen as the freshest thing on it.
    let mut publication = Publication::healthy();
    publication.medium_pass = Some(fixture_now() + Duration::from_secs(600));
    let reading = publication.read();

    let ages = ages_of(&reading, fixture_now());
    let medium = ages.of(Tier::Medium).expect("published");
    let why = medium
        .since_pass
        .clone()
        .expect_err("a stamp in the future is not an age");
    assert!(
        why.contains("clock moved backwards"),
        "and the reason says why rather than showing 0s; got {why}"
    );
    assert!(
        prose(
            &published_screen(&reading, fixture_now(), Writer::Alive),
            wide()
        )
        .contains("clock moved backwards"),
        "and it reaches the screen"
    );
}

#[test]
fn a_monitor_that_died_before_publishing_anything_is_dead_rather_than_absent() {
    // Under a KeepAlive LaunchAgent (#11) this is a crash loop, and "no monitor is recording"
    // would describe it as a machine nobody had set up.
    let state = TieredState::new(31_337);
    let reading = reading_of(&state);
    assert!(matches!(reading, StateReading::Unrenderable { .. }));

    let alive = presence_of(&reading, fixture_now(), Writer::Alive);
    assert_eq!(
        alive.label(),
        "ABSENT",
        "a monitor that has just started and holds the writer role is warming up, not dead"
    );

    let gone = presence_of(&reading, fixture_now(), Writer::Gone);
    assert_eq!(gone.label(), "DEAD");
    let text = prose(
        &Screen::from_own_collection(
            &gone,
            Ok(ordinary_snapshot()),
            Duration::from_millis(9),
            fixture_now(),
        ),
        wide(),
    );
    assert!(
        text.contains("THE MONITOR IS DEAD") && text.contains("crash loop"),
        "the screen has to name what a monitor dying before its first pass means; got:\n{text}"
    );
    assert!(
        text.contains("this display took for itself"),
        "and still say whose the figures on it are, because they are not the monitor's; \
         got:\n{text}"
    );
}

#[test]
fn asking_whether_a_writer_is_alive_never_signals_anything_and_pid_zero_is_no_process() {
    // The tool observes; it never acts (N1). `kill(0, 0)` addresses the caller's whole process
    // group, so a state file naming 0 as its writer must not become a signal to the terminal
    // session this display is running in.
    assert_eq!(Writer::of(0), Writer::Gone);
    assert_eq!(
        Writer::of(std::process::id()),
        Writer::Alive,
        "and this test's own process is observably alive, which is what makes the negative \
         meaningful"
    );
}

#[test]
fn published_rows_are_ordered_by_child_cpu_with_an_unmeasurable_cost_first() {
    // The same rule as a locally collected table (NF10, F54, F55): the bottom of the table is
    // what a short terminal drops, so a session with no cost at all must not be ranked cheap.
    let cheap = SessionRow {
        pid: Some(1),
        children_cpu_ms: Measured::known(1_000),
        ..published_session()
    };
    let dear = SessionRow {
        pid: Some(2),
        children_cpu_ms: Measured::known(90_000),
        ..published_session()
    };
    let unknown = SessionRow {
        pid: Some(3),
        children_cpu_ms: Measured::unavailable("the ledger could not be read"),
        ..published_session()
    };

    let rows = vec![cheap, dear, unknown];
    let order: Vec<Option<i32>> = published_in_cost_order(&rows)
        .into_iter()
        .map(|row| row.pid)
        .collect();
    assert_eq!(order, vec![Some(3), Some(2), Some(1)]);
    assert!(matches!(published_cost_of(&rows[2]), Cost::Unmeasurable(_)));
}

#[test]
fn a_remembered_published_figure_is_marked_and_carries_its_own_age() {
    // Almost all of an agent's cost is in its children, and once the process is gone that total
    // exists nowhere but in what the monitor wrote down. Showing it is the point; showing it
    // without its age would make a memory indistinguishable from a measurement.
    let mut publication = Publication::healthy();
    publication.sessions = vec![SessionRow {
        pid: None,
        recorded_as: Some("-Users-pmcfadin-projects-acmon".to_string()),
        remembered_at: Some(iso(ago(3_600))),
        ..published_session()
    }];
    let reading = publication.read();
    let text = prose(
        &published_screen(&reading, fixture_now(), Writer::Alive),
        wide(),
    );

    assert!(
        text.contains("last read 1h00m before that pass"),
        "the age is stated against the pass that published it, so the monitor's own staleness is \
         not folded into it; got:\n{text}"
    );
    assert!(
        text.contains("A figure marked * is the last reading"),
        "and the marker is explained; got:\n{text}"
    );
}

#[test]
fn a_published_screen_shows_the_monitor_s_own_duty_cycle_rather_than_a_reason_for_its_absence() {
    // The gap this ticket closed. Before it, a monitor could be publishing 0.4% of a core while
    // the display printed "this display cannot read its tier payloads yet" where the figure
    // should be — and collected for itself at 2.5 s a pass to fill the table.
    let reading = Publication::healthy().read();
    let screen = published_screen(&reading, fixture_now(), Writer::Alive);

    assert_eq!(screen.meters.duty_cycle, Ok(0.004_13));
    let text = prose(&screen, wide());
    assert!(
        text.contains("0.4%"),
        "the monitor's measured duty cycle is on the screen as a figure; got:\n{text}"
    );
    assert!(
        !text.contains("absent"),
        "with no gauge left standing in for one; got:\n{text}"
    );
    assert!(
        matches!(screen.facts, Facts::Monitor(_)),
        "and the figures beside it are the monitor's too"
    );
}

#[test]
fn no_screen_is_ever_blank_or_a_bare_error_whatever_the_monitor_turned_out_to_be() {
    // The criterion a fresh `brew install` hits first, asserted across every verdict at once. A
    // blank screen or a one-line error would be the "fail to zero" this project exists to remove.
    let reading = Publication::healthy().read();
    let screens = vec![
        (
            "FRESH",
            published_screen(&reading, fixture_now(), Writer::Alive),
        ),
        (
            "DEAD",
            published_screen(&reading, fixture_now(), Writer::Gone),
        ),
        (
            "ABSENT",
            Screen::from_own_collection(
                &no_monitor(),
                Ok(ordinary_snapshot()),
                Duration::from_millis(9),
                fixture_now(),
            ),
        ),
        (
            "ABSENT",
            Screen::from_own_collection(
                &unbelievable("the file is torn"),
                Err("the collection failed too".to_string()),
                Duration::from_millis(9),
                fixture_now(),
            ),
        ),
    ];

    for (expected, screen) in screens {
        assert_eq!(screen.presence.label(), expected);
        let text = prose(&screen, wide());
        assert!(
            text.starts_with(expected),
            "the word the screen is marked with is the first thing on it, not only a value in an \
             enum; for {expected} got:\n{text}"
        );
        assert!(
            text.split_whitespace().count() > 40,
            "a screen that says almost nothing is the failure this project exists to remove; \
             for {expected} got:\n{text}"
        );
        assert!(
            text.contains("read-only"),
            "and every screen says which binary it is; for {expected} got:\n{text}"
        );
    }
}

#[test]
fn a_published_screen_fits_every_width_and_every_height_without_losing_its_freshness_mark() {
    // The mark is the one line on this screen that may never be clipped: it is what makes every
    // figure below it readable. It is drawn above everything it qualifies for exactly that
    // reason, and a short terminal cuts from the bottom.
    let mut publication = Publication::healthy();
    publication.fast_pass = fixture_now() - Cadence::ACTIVE.fast * (MISSED_A_PASS + 1);
    let reading = publication.read();
    let screen = published_screen(&reading, fixture_now(), Writer::Alive);

    for width in [minimum_width(), minimum_width() + 24, minimum_width() + 96] {
        let full = screen_height(&screen, width);
        for height in [3, full / 2, full] {
            let lines = screen_to_lines(&screen, width, height);
            for line in &lines {
                assert!(
                    line.chars().count() <= width as usize,
                    "a line ran off the side at {width}x{height}: {line:?}"
                );
            }
            assert!(
                lines.iter().any(|line| line.contains("STALE")),
                "the freshness mark survives {width}x{height}; got:\n{}",
                lines.join("\n")
            );
        }
    }
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
fn agtop_draws_a_live_monitor_as_fresh_and_the_same_file_as_dead_once_its_writer_is_gone() {
    // S8, end to end, with two real processes. Nothing here is a fixture: `amon watch` publishes
    // a real `state.json` and `agtop --once` reads it twice — once while the monitor holds the
    // writer role, and once after it has exited and its pid has been reaped.
    //
    // The waits below are waits for a *process*, not assertions about timing. What is asserted is
    // that the same file reads as FRESH with its writer alive and DEAD without it.
    let directory = scratch("live-then-dead");
    let state_directory = directory.join("state");
    let memory = directory.join("memory.json");
    let notify_config = directory.join("no-such-notify.toml");

    let mut monitor = Process::new(env!("CARGO_BIN_EXE_amon"))
        .arg("watch")
        .env(acmon::state::STATE_DIR_VARIABLE, &state_directory)
        .env(acmon::state::CONFIG_DIR_VARIABLE, directory.join("config"))
        .env(acmon::real_world::STATE_VARIABLE, &memory)
        .env(acmon::real_world::NOTIFY_CONFIG_VARIABLE, &notify_config)
        .env(acmon::watch::RUN_VARIABLE, "4000")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("amon is built and runnable");

    let display = |label: &str| -> String {
        let output = Process::new(env!("CARGO_BIN_EXE_agtop"))
            .arg("--once")
            .env(acmon::state::STATE_DIR_VARIABLE, &state_directory)
            .env(acmon::state::CONFIG_DIR_VARIABLE, directory.join("config"))
            .env(acmon::real_world::STATE_VARIABLE, &memory)
            .env(acmon::real_world::NOTIFY_CONFIG_VARIABLE, &notify_config)
            .output()
            .expect("agtop is built and runnable");
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            !text.trim().is_empty(),
            "agtop said nothing at all while {label}"
        );
        text
    };

    // Wait for the monitor to publish a fast pass. Bounded, and a timeout is a failure rather
    // than a skip: a test that quietly stopped waiting would assert nothing.
    let store = store_in(&state_directory);
    //
    // The wait is for *this* child to name itself the writer, not merely for a decodable file. A
    // state directory that has held an earlier monitor still names that one until the new pass
    // lands, and taking the first pid the file offers would age a screen against a process that
    // died before this test started.
    let deadline = Instant::now() + Duration::from_secs(20);
    let writer_pid = loop {
        if let StateReading::Published(reading) = read_state_file(&store) {
            if reading.writer_pid == monitor.id() {
                break reading.writer_pid;
            }
        }
        assert!(
            Instant::now() < deadline,
            "the monitor published no fast pass of its own before the deadline"
        );
        std::thread::sleep(Duration::from_millis(50));
    };

    let live = display("the monitor was running");
    assert!(
        live.contains("FRESH — the monitor at pid") && live.contains(&writer_pid.to_string()),
        "a running monitor's own figures are drawn and marked fresh; got:\n{live}"
    );
    assert!(
        !live.contains("this display took for itself"),
        "and nothing was collected by the display while a monitor was publishing; got:\n{live}"
    );

    // Its own run window ends it, so nothing is signalled: the tool observes and never acts, and
    // this test does not get an exception. `wait` reaps it, which is what makes the pid go.
    let finished = monitor.wait().expect("the monitor exits on its own");
    assert!(
        finished.success(),
        "the monitor has to have stopped cleanly, or the second screen would be asserting \
         something else: {finished:?}"
    );

    let dead = display("the monitor was gone");
    assert!(
        dead.contains("THE MONITOR IS DEAD") && dead.contains("CORPSE"),
        "the same file, with its writer gone, must never read as a live monitor; got:\n{dead}"
    );
    assert!(
        dead.contains(&writer_pid.to_string()),
        "and the screen names the pid it asked about; got:\n{dead}"
    );
    assert!(
        !dead.contains("FRESH — the monitor"),
        "at no point are dead facts rendered as current; got:\n{dead}"
    );

    let _ = std::fs::remove_dir_all(&directory);
}

#[test]
fn agtop_once_prints_the_meter_row_as_plain_lines() {
    // F34: one pass emits the same content as the full-screen view. The row is gauges, and a
    // gauge that only existed inside a `ratatui` widget could not be piped, diffed or pasted
    // into an issue — which is the whole reason the one-shot mode is not a fallback.
    let directory = scratch("once-meters");

    let output = Process::new(env!("CARGO_BIN_EXE_agtop"))
        .arg("--once")
        .env(acmon::state::STATE_DIR_VARIABLE, directory.join("state"))
        .env(acmon::state::CONFIG_DIR_VARIABLE, directory.join("config"))
        .output()
        .expect("running agtop");

    // Either stream, because a collection that failed still has to say what it cost trying.
    let printed = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(
        printed.contains("collection overhead") && printed.contains("amon duty cycle"),
        "both meters, in plain text; got:\n{printed}"
    );
    assert!(
        printed.contains('[') && (printed.contains('|') || printed.contains('?')),
        "and drawn as gauges rather than as a sentence; got:\n{printed}"
    );
    assert!(
        printed.contains("refresh interval"),
        "with the scale its bars are drawn against; got:\n{printed}"
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
