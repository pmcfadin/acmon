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
}

impl World for FakeWorld {
    fn process_snapshot(&self) -> Result<ProcessSnapshot, WorldError> {
        self.snapshot.clone()
    }
}

fn rec(pid: i32, exe: &str) -> ProcessRecord {
    ProcessRecord {
        pid,
        exe_path: Some(exe.to_string()),
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
        // --- the observing process itself, so the snapshot passes its own sentinel
        rec(99999, "/Users/pmcfadin/projects/acmon/target/debug/acmon"),
    ]
}

#[test]
fn lists_every_live_claude_session() {
    let world = FakeWorld::with(captured_process_table(), 99999);

    let snapshot = collect(&world).expect("collection should succeed");

    assert_eq!(
        snapshot.sessions.len(),
        9,
        "expected the nine captured Claude sessions, got {:?}",
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
        .filter(|r| r.pid != 99999)
        .collect();
    let world = FakeWorld::with(truncated, 99999);

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
            99999,
            "/Users/pmcfadin/projects/acmon/target/debug/acmon",
        )],
        99999,
    );

    let snapshot = collect(&world).expect("a trustworthy but empty snapshot is not an error");

    assert_eq!(snapshot.sessions.len(), 0);
}
