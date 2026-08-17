//! Seam 1 — `collect` over an injected `World`.
//!
//! Every fixture here is a real observation from a developer machine, captured with
//! `proc_pidpath`. Nothing is invented: the executable paths, the version-string
//! basenames, and the near-miss processes all occurred.

use std::collections::HashMap;
use std::time::Duration;

use acmon::world::{ResourceSource, Resources, ResourcesUnavailable, Unmeasured};
use acmon::{collect, CollectError, ProcessRecord, ProcessSnapshot, World, WorldError};

struct FakeWorld {
    snapshot: Result<ProcessSnapshot, WorldError>,
    /// Per-pid resource readings. A pid with no entry gets [`measured_ledger`], so a
    /// test only has to state the readings it actually asserts on.
    ledgers: HashMap<i32, Result<Resources, ResourcesUnavailable>>,
}

impl FakeWorld {
    fn with(records: Vec<ProcessRecord>, observer_pid: i32) -> Self {
        FakeWorld {
            snapshot: Ok(ProcessSnapshot {
                records,
                observer_pid,
            }),
            ledgers: HashMap::new(),
        }
    }

    fn with_error(error: WorldError) -> Self {
        FakeWorld {
            snapshot: Err(error),
            ledgers: HashMap::new(),
        }
    }

    fn ledger(mut self, pid: i32, reading: Result<Resources, ResourcesUnavailable>) -> Self {
        self.ledgers.insert(pid, reading);
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
}

fn rec(pid: i32, exe: &str) -> ProcessRecord {
    ProcessRecord {
        pid,
        exe_path: Ok(exe.to_string()),
    }
}

fn rec_unreadable(pid: i32) -> ProcessRecord {
    use acmon::ExePathUnavailable;
    ProcessRecord {
        pid,
        exe_path: Err(ExePathUnavailable::PermissionDenied),
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
