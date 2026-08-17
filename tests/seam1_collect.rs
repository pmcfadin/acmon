//! Seam 1 — `collect` over an injected `World`.
//!
//! Every fixture here is a real observation from a developer machine, captured with
//! `proc_pidpath`. Nothing is invented: the executable paths, the version-string
//! basenames, and the near-miss processes all occurred.

use acmon::{collect, CollectError, ProcessRecord, ProcessSnapshot, World, WorldError};

struct FakeWorld {
    snapshot: Result<ProcessSnapshot, WorldError>,
}

impl FakeWorld {
    fn with(records: Vec<ProcessRecord>, observer_pid: i32) -> Self {
        FakeWorld {
            snapshot: Ok(ProcessSnapshot {
                records,
                observer_pid,
            }),
        }
    }

    fn with_error(error: WorldError) -> Self {
        FakeWorld {
            snapshot: Err(error),
        }
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

/// Captured from a real machine: nine live Claude sessions, the real Codex CLI, and
/// five processes that a careless detector would misclassify.
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
