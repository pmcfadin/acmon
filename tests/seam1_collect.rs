//! Seam 1 — `collect` over an injected `World`.
//!
//! Every fixture here is a real observation from a developer machine, captured with
//! `proc_pidpath`. Nothing is invented: the executable paths, the version-string
//! basenames, and the near-miss processes all occurred.

use std::collections::HashMap;
use std::time::Duration;

use acmon::workspace::{NamespaceUnmatched, WorkspaceUnknown};
use acmon::world::{ResourceSource, Resources, ResourcesUnavailable, Unmeasured};
use acmon::{collect, CollectError, ProcessRecord, ProcessSnapshot, World, WorldError};

struct FakeWorld {
    snapshot: Result<ProcessSnapshot, WorldError>,
    /// Per-pid resource readings. A pid with no entry gets [`measured_ledger`], so a
    /// test only has to state the readings it actually asserts on.
    ledgers: HashMap<i32, Result<Resources, ResourcesUnavailable>>,
    /// The transcript namespaces this machine is pretending to have recorded.
    namespaces: Result<Vec<String>, WorldError>,
}

/// Namespaces that genuinely exist in `~/.claude/projects` on the machine these
/// fixtures came from — including one recorded with capitals its live cwd does not have.
fn recorded_namespaces() -> Vec<String> {
    [
        "-Users-pmcfadin-projects-agentic-coding-monitor",
        "-Users-pmcfadin-projects-WorkforceOS",
        "-Users-pmcfadin-projects-workforceos-mvp",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

impl FakeWorld {
    fn with(records: Vec<ProcessRecord>, observer_pid: i32) -> Self {
        FakeWorld {
            snapshot: Ok(ProcessSnapshot {
                records,
                observer_pid,
            }),
            ledgers: HashMap::new(),
            namespaces: Ok(recorded_namespaces()),
        }
    }

    fn with_error(error: WorldError) -> Self {
        FakeWorld {
            snapshot: Err(error),
            ledgers: HashMap::new(),
            namespaces: Ok(recorded_namespaces()),
        }
    }

    fn ledger(mut self, pid: i32, reading: Result<Resources, ResourcesUnavailable>) -> Self {
        self.ledgers.insert(pid, reading);
        self
    }

    fn without_namespace_listing(mut self, why: &str) -> Self {
        self.namespaces = Err(WorldError::NamespaceListing(why.to_string()));
        self
    }
}

/// The ledger of session 69046, measured and recorded in
/// `docs/observability-mechanics.md` §2.6 — real numbers from a real session, including
/// the 19.4x child-to-own CPU ratio that motivates this ticket.
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

/// The ledger of session 264 from the same table — the opposite shape, a session that
/// does most of its work itself.
fn measured_ledger_of_a_session_that_delegates_little() -> Resources {
    Resources {
        source: ResourceSource::Rusage,
        own_cpu: Ok(Duration::from_secs(637)),
        children_cpu: Ok(Duration::from_secs(101)),
        current_memory: Ok(419_000_000),
        peak_memory: Ok(587_000_000),
        bytes_written: Ok(34_000_000),
    }
}

impl World for FakeWorld {
    fn output_width(&self) -> u16 {
        // Pinned, so render tests are deterministic regardless of the real terminal.
        80
    }

    fn process_snapshot(&self) -> Result<ProcessSnapshot, WorldError> {
        self.snapshot.clone()
    }

    fn resources(&self, pid: i32) -> Result<Resources, ResourcesUnavailable> {
        self.ledgers
            .get(&pid)
            .cloned()
            .unwrap_or_else(|| Ok(measured_ledger()))
    }

    fn recorded_namespaces(&self) -> Result<Vec<String>, WorldError> {
        self.namespaces.clone()
    }
}

/// A record in a directory that really exists on the machine these fixtures came from.
/// Tests that care which directory it is use [`rec_in`].
fn rec(pid: i32, exe: &str) -> ProcessRecord {
    rec_in(pid, exe, "/Users/pmcfadin/projects/agentic_coding_monitor")
}

fn rec_in(pid: i32, exe: &str, cwd: &str) -> ProcessRecord {
    ProcessRecord {
        pid,
        exe_path: Ok(exe.to_string()),
        cwd: Ok(cwd.to_string()),
    }
}

fn rec_unreadable(pid: i32) -> ProcessRecord {
    use acmon::PathUnavailable;
    ProcessRecord {
        pid,
        exe_path: Err(PathUnavailable::PermissionDenied),
        cwd: Err(PathUnavailable::PermissionDenied),
    }
}

/// Captured from a real machine: live Claude sessions of both builds, the real Codex
/// CLI, and processes a careless detector would misclassify.
///
/// Every path here was observed via `proc_pidpath`. The pids were observed too, but
/// they are inherently ephemeral — nothing in these tests depends on a pid being
/// currently live, only on paths being real and distinguishable.
fn captured_process_table() -> Vec<ProcessRecord> {
    vec![
        // --- nine genuine Claude Code sessions. Note the basename is a VERSION STRING.
        rec(59245, "/Users/pmcfadin/.local/share/claude/versions/2.1.233"),
        rec(24555, "/Users/pmcfadin/.local/share/claude/versions/2.1.233"),
        rec(24013, "/Users/pmcfadin/.local/share/claude/versions/2.1.233"),
        rec(23783, "/Users/pmcfadin/.local/share/claude/versions/2.1.233"),
        rec(23453, "/Users/pmcfadin/.local/share/claude/versions/2.1.233"),
        rec(23147, "/Users/pmcfadin/.local/share/claude/versions/2.1.233"),
        rec(22706, "/Users/pmcfadin/.local/share/claude/versions/2.1.233"),
        rec(21988, "/Users/pmcfadin/.local/share/claude/versions/2.1.233"),
        rec(53278, "/Users/pmcfadin/.local/share/claude/versions/2.1.233"),
        // --- the real Codex CLI. Not a Claude session; ticket #5 will claim it.
        rec(59293, "/opt/homebrew/lib/node_modules/@openai/codex/node_modules/@openai/codex-darwin-arm64/vendor/aarch64-apple-darwin/bin/codex"),
        // --- near misses, all observed, none of them agent sessions
        rec(60099, "/opt/homebrew/lib/node_modules/@openai/codex/node_modules/@openai/codex-darwin-arm64/vendor/aarch64-apple-darwin/bin/codex-code-mode-host"),
        rec(62507, "/Users/pmcfadin/.codex/computer-use/Codex Computer Use.app/Contents/SharedSupport/SkyComputerUseClient.app/Contents/MacOS/SkyComputerUseClient"),
        rec(63202, "/Applications/Cursor.app/Contents/Frameworks/Cursor Helper.app/Contents/MacOS/Cursor Helper"),
        rec(21023, "/Users/pmcfadin/.local/share/cursor-agent/versions/2026.05.01-eea359f/node"),
        // --- Agent SDK's bundled Claude CLI — a distinct build from the versioned one
        // FIXME(pids): 89000/89001 are placeholders, not observations. Replace
        // when an Agent SDK process is running. The PATHS below are genuine.
        rec(89000, "/Users/pmcfadin/.claude/security/agent-sdk-venv/lib/python3.14/site-packages/claude_agent_sdk/_bundled/claude"),
        rec(89001, "/Users/pmcfadin/.claude/security/agent-sdk-venv/lib/python3.14/site-packages/claude_agent_sdk/_bundled/claude"),
        // --- the observing process itself, captured from a real test run
        rec(88429, "/Users/pmcfadin/projects/agentic_coding_monitor/target/debug/deps/seam1_collect-de79cf24515e8113"),
    ]
}

#[test]
fn lists_every_live_claude_session() {
    let world = FakeWorld::with(captured_process_table(), 88429);

    let snapshot = collect(&world).expect("collection should succeed");

    assert_eq!(
        snapshot.sessions.len(),
        11,
        "expected nine versioned Claude sessions plus two Agent SDK bundled sessions, got {:?}",
        snapshot
            .sessions
            .iter()
            .map(|s| (s.pid, s.cli.as_str()))
            .collect::<Vec<_>>()
    );
    assert!(
        snapshot.sessions.iter().all(|s| s.cli == "claude"),
        "ticket #2 recognises only Claude; Codex arrives in #5"
    );
}

#[test]
fn a_snapshot_missing_its_own_observer_is_a_failure_not_an_idle_machine() {
    // The enumeration died part-way, so the observer is absent from its own results.
    // Nine Claude sessions are genuinely running, but this snapshot cannot see them.
    let truncated: Vec<ProcessRecord> = captured_process_table()
        .into_iter()
        .filter(|r| r.pid != 88429)
        .collect();
    let world = FakeWorld::with(truncated, 88429);

    let result = collect(&world);

    assert!(
        matches!(result, Err(CollectError::UntrustworthySnapshot { .. })),
        "a snapshot lacking its own observer must be rejected, not reported as \
         a machine with no sessions; got {result:?}"
    );
}

#[test]
fn a_machine_with_no_agent_sessions_reports_zero_rather_than_failing() {
    // The observer is present, so the snapshot is trustworthy. It simply contains no
    // agent CLI. That must be reported as zero sessions, not as an error — otherwise
    // "nothing running" and "cannot tell" collapse into the same answer.
    let world = FakeWorld::with(
        vec![rec(
            88429,
            "/Users/pmcfadin/projects/acmon/target/debug/acmon",
        )],
        88429,
    );

    let snapshot = collect(&world).expect("a trustworthy but empty snapshot is not an error");

    assert_eq!(snapshot.sessions.len(), 0);
}

#[test]
fn processes_with_unreadable_paths_are_excluded_but_reason_is_recorded() {
    // A process whose path cannot be read must not become a session, but the record
    // must carry a reason rather than collapsing to None with no explanation.
    let world = FakeWorld::with(
        vec![
            rec_unreadable(12345), // unreadable, ignored as a session
            rec(
                88429,
                "/Users/pmcfadin/projects/agentic_coding_monitor/target/debug/acmon",
            ),
        ],
        88429,
    );

    let snapshot = collect(&world).expect("collection should succeed");

    // The unreadable process is not a session
    assert_eq!(snapshot.sessions.len(), 0);

    // But the ProcessRecord should carry a reason for why it's unreadable
    let observation = world.process_snapshot().expect("snapshot");
    let unreadable_record = observation.records.iter().find(|r| r.pid == 12345).unwrap();
    assert!(
        unreadable_record.exe_path.is_err(),
        "an unreadable path must be Err with a reason, not None"
    );
}

#[test]
fn each_session_carries_the_readings_of_its_own_pid() {
    // Two sessions of opposite shape. The point is not that the numbers arrive but
    // that they arrive on the right row: a monitor that reads the ledger of one pid
    // and prints it against another is worse than one that prints nothing.
    let world = FakeWorld::with(
        vec![
            rec(
                69046,
                "/Users/pmcfadin/.local/share/claude/versions/2.1.233",
            ),
            rec(264, "/Users/pmcfadin/.local/share/claude/versions/2.1.233"),
            rec(88429, "/Users/pmcfadin/projects/acmon/target/debug/acmon"),
        ],
        88429,
    )
    .ledger(69046, Ok(measured_ledger()))
    .ledger(
        264,
        Ok(measured_ledger_of_a_session_that_delegates_little()),
    );

    let snapshot = collect(&world).expect("collection should succeed");

    let heavy_delegator = snapshot
        .sessions
        .iter()
        .find(|s| s.pid == 69046)
        .expect("session 69046");
    let self_worker = snapshot
        .sessions
        .iter()
        .find(|s| s.pid == 264)
        .expect("session 264");

    assert_eq!(
        heavy_delegator.resources.as_ref().unwrap().children_cpu,
        Ok(Duration::from_secs(32_317))
    );
    assert_eq!(
        self_worker.resources.as_ref().unwrap().children_cpu,
        Ok(Duration::from_secs(101))
    );
}

#[test]
fn a_session_whose_ledger_cannot_be_read_is_still_listed_with_a_reason() {
    // The session was detected, so it exists and must appear. What must not happen is
    // that it silently vanishes from the table, or appears with zeroes — an idle
    // session and an unreadable one would then look identical.
    let world = FakeWorld::with(
        vec![
            rec(
                69046,
                "/Users/pmcfadin/.local/share/claude/versions/2.1.233",
            ),
            rec(88429, "/Users/pmcfadin/projects/acmon/target/debug/acmon"),
        ],
        88429,
    )
    .ledger(69046, Err(ResourcesUnavailable::ProcessExited));

    let snapshot = collect(&world).expect("collection should succeed");

    assert_eq!(
        snapshot.sessions.len(),
        1,
        "the session must still be listed"
    );
    assert_eq!(
        snapshot.sessions[0].resources,
        Err(ResourcesUnavailable::ProcessExited)
    );
}

#[test]
fn a_coarser_reading_keeps_what_it_has_and_says_why_the_rest_is_missing() {
    // The fallback source for a process owned by another user reports cumulative own
    // CPU and resident size, and nothing else. The figures it cannot see must carry
    // that as their reason rather than defaulting to zero, which would report a
    // process with busy children as having none.
    let coarse = Resources {
        source: ResourceSource::Ps,
        own_cpu: Ok(Duration::from_secs(42)),
        children_cpu: Err(Unmeasured::NotReportedBy(ResourceSource::Ps)),
        current_memory: Ok(12_000_000),
        peak_memory: Err(Unmeasured::NotReportedBy(ResourceSource::Ps)),
        bytes_written: Err(Unmeasured::NotReportedBy(ResourceSource::Ps)),
    };
    let world = FakeWorld::with(
        vec![
            rec(
                69046,
                "/Users/pmcfadin/.local/share/claude/versions/2.1.233",
            ),
            rec(88429, "/Users/pmcfadin/projects/acmon/target/debug/acmon"),
        ],
        88429,
    )
    .ledger(69046, Ok(coarse));

    let snapshot = collect(&world).expect("collection should succeed");
    let reading = snapshot.sessions[0].resources.as_ref().unwrap();

    assert_eq!(reading.own_cpu, Ok(Duration::from_secs(42)));
    assert_eq!(
        reading.children_cpu,
        Err(Unmeasured::NotReportedBy(ResourceSource::Ps)),
        "an unreadable child total must state its reason, never read as zero"
    );
}

#[test]
fn world_errors_propagate_as_collect_errors() {
    use acmon::WorldError;

    let world = FakeWorld::with_error(WorldError::ProcessEnumeration(
        "process table unreadable".to_string(),
    ));

    let result = collect(&world);

    assert!(
        matches!(result, Err(CollectError::World(_))),
        "world errors must propagate through collect; got {result:?}"
    );
}

/// A record whose executable is readable — so it is detected as a session — but whose
/// working directory is not. That combination is what makes the workspace unknown.
fn rec_without_cwd(pid: i32, exe: &str) -> ProcessRecord {
    use acmon::PathUnavailable;
    ProcessRecord {
        pid,
        exe_path: Ok(exe.to_string()),
        cwd: Err(PathUnavailable::PermissionDenied),
    }
}

const CLAUDE_EXE: &str = "/Users/pmcfadin/.local/share/claude/versions/2.1.233";

#[test]
fn each_session_shows_the_directory_it_is_working_in() {
    // As with the resource ledger, the point is that the value lands on the right row.
    // Both directories and both namespaces below exist on a real machine.
    let world = FakeWorld::with(
        vec![
            rec_in(
                69046,
                CLAUDE_EXE,
                "/Users/pmcfadin/projects/agentic_coding_monitor",
            ),
            rec_in(264, CLAUDE_EXE, "/Users/pmcfadin/projects/workforceos"),
            rec(88429, "/Users/pmcfadin/projects/acmon/target/debug/acmon"),
        ],
        88429,
    );

    let snapshot = collect(&world).expect("collection should succeed");
    let find = |pid: i32| {
        snapshot
            .sessions
            .iter()
            .find(|s| s.pid == pid)
            .unwrap_or_else(|| panic!("session {pid}"))
            .workspace
            .as_ref()
            .expect("a readable cwd yields a workspace")
            .clone()
    };

    assert_eq!(
        find(69046).path,
        "/Users/pmcfadin/projects/agentic_coding_monitor"
    );
    assert_eq!(find(264).path, "/Users/pmcfadin/projects/workforceos");
}

#[test]
fn a_workspace_is_attributed_to_the_namespace_recorded_for_it() {
    // The underscore and capitalisation rules, exercised through collection rather than
    // through the mapping function alone: this is where a wrong rule would have shown
    // up as a session with no transcript.
    let world = FakeWorld::with(
        vec![
            rec_in(
                69046,
                CLAUDE_EXE,
                "/Users/pmcfadin/projects/agentic_coding_monitor",
            ),
            rec_in(264, CLAUDE_EXE, "/Users/pmcfadin/projects/workforceos"),
            rec(88429, "/Users/pmcfadin/projects/acmon/target/debug/acmon"),
        ],
        88429,
    );

    let snapshot = collect(&world).expect("collection should succeed");
    let namespace_of = |pid: i32| {
        snapshot
            .sessions
            .iter()
            .find(|s| s.pid == pid)
            .unwrap()
            .workspace
            .as_ref()
            .unwrap()
            .namespace
            .clone()
    };

    assert_eq!(
        namespace_of(69046),
        Ok("-Users-pmcfadin-projects-agentic-coding-monitor".to_string()),
        "underscores in the path map to hyphens in the namespace"
    );
    assert_eq!(
        namespace_of(264),
        Ok("-Users-pmcfadin-projects-WorkforceOS".to_string()),
        "and the recorded spelling is kept, capitals and all"
    );
}

#[test]
fn a_workspace_with_no_recorded_namespace_says_so_and_shows_what_it_looked_for() {
    let world = FakeWorld::with(
        vec![
            rec_in(69046, CLAUDE_EXE, "/Users/pmcfadin/projects/never_opened"),
            rec(88429, "/Users/pmcfadin/projects/acmon/target/debug/acmon"),
        ],
        88429,
    );

    let snapshot = collect(&world).expect("collection should succeed");
    let workspace = snapshot.sessions[0].workspace.as_ref().unwrap();

    assert_eq!(
        workspace.path, "/Users/pmcfadin/projects/never_opened",
        "the directory is still known even when its transcript is not"
    );
    assert_eq!(
        workspace.namespace,
        Err(NamespaceUnmatched::NotRecorded {
            mapped: "-Users-pmcfadin-projects-never-opened".to_string()
        }),
        "an unmatched namespace must show what was looked for, so it can be checked"
    );
}

#[test]
fn a_session_whose_working_directory_is_unreadable_shows_an_explicit_unknown() {
    // The session exists and is listed. Its workspace is not blank and not guessed.
    let world = FakeWorld::with(
        vec![
            rec_without_cwd(69046, CLAUDE_EXE),
            rec(88429, "/Users/pmcfadin/projects/acmon/target/debug/acmon"),
        ],
        88429,
    );

    let snapshot = collect(&world).expect("collection should succeed");

    assert_eq!(snapshot.sessions.len(), 1, "the session is still listed");
    assert_eq!(
        snapshot.sessions[0].workspace,
        Err(WorkspaceUnknown::PermissionDenied)
    );
}

#[test]
fn a_failure_to_list_recorded_namespaces_is_not_a_workspace_without_a_transcript() {
    // Two different facts: "this workspace has no transcript" and "we could not look".
    // Collapsing them would report the first when the second is true.
    let world = FakeWorld::with(
        vec![
            rec_in(
                69046,
                CLAUDE_EXE,
                "/Users/pmcfadin/projects/agentic_coding_monitor",
            ),
            rec(88429, "/Users/pmcfadin/projects/acmon/target/debug/acmon"),
        ],
        88429,
    )
    .without_namespace_listing("~/.claude/projects is not readable");

    let snapshot = collect(&world).expect("an unlistable transcript store is not fatal");
    let workspace = snapshot.sessions[0].workspace.as_ref().unwrap();

    assert!(
        matches!(
            workspace.namespace,
            Err(NamespaceUnmatched::ListingFailed(_))
        ),
        "got {:?}",
        workspace.namespace
    );
}
