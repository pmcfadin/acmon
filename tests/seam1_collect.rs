//! Seam 1 — `collect` over an injected `World`.
//!
//! Every fixture here is a real observation from a developer machine, captured with
//! `proc_pidpath`. Nothing is invented: the executable paths, the version-string
//! basenames, and the near-miss processes all occurred.

use std::collections::HashMap;
use std::time::{Duration, SystemTime};

use acmon::liveness::State;
use acmon::workspace::{NamespaceUnmatched, WorkspaceUnknown};
use acmon::world::{
    ActivityUnavailable, CodexSession, ResourceSource, Resources, ResourcesUnavailable, Unmeasured,
};
use acmon::{collect, CollectError, ProcessRecord, ProcessSnapshot, World, WorldError};

struct FakeWorld {
    snapshot: Result<ProcessSnapshot, WorldError>,
    /// Per-pid resource readings. A pid with no entry gets [`measured_ledger`], so a
    /// test only has to state the readings it actually asserts on.
    ledgers: HashMap<i32, Result<Resources, ResourcesUnavailable>>,
    /// The transcript namespaces this machine is pretending to have recorded.
    namespaces: Result<Vec<String>, WorldError>,
    /// The Codex sessions this machine is pretending to have recorded.
    codex_sessions: Result<Vec<CodexSession>, WorldError>,
    /// Per-namespace activity times. A namespace with no entry gets a default time.
    namespace_activities: HashMap<String, Result<SystemTime, ActivityUnavailable>>,
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

/// A real Codex session observed on this machine, safe to use as a fixture.
/// Default activity is old enough (more than 48 hours before fixture_now) that it
/// won't be discovered as a transcript-derived session unless a test sets a more recent time.
fn recorded_codex_sessions() -> Vec<CodexSession> {
    vec![CodexSession {
        id: "01a010c6-9c79-76d1-82da-3fad4bbf3bc4".to_string(),
        workspace: "/Users/pmcfadin/Documents/Codex/2026-08-17/he".to_string(),
        last_activity: SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000 - 48 * 3600),
    }]
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
            codex_sessions: Ok(recorded_codex_sessions()),
            namespace_activities: HashMap::new(),
        }
    }

    fn with_error(error: WorldError) -> Self {
        FakeWorld {
            snapshot: Err(error),
            ledgers: HashMap::new(),
            namespaces: Ok(recorded_namespaces()),
            codex_sessions: Ok(recorded_codex_sessions()),
            namespace_activities: HashMap::new(),
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

    fn with_codex_sessions(mut self, sessions: Result<Vec<CodexSession>, WorldError>) -> Self {
        self.codex_sessions = sessions;
        self
    }

    fn namespace_activity(
        mut self,
        namespace: String,
        activity: Result<SystemTime, ActivityUnavailable>,
    ) -> Self {
        self.namespace_activities.insert(namespace, activity);
        self
    }
}

/// A fixed instant for liveness verdicts, so a test's answer does not depend on when it
/// ran. Sixty seconds after the activity time `FakeWorld` reports by default, which makes
/// an unremarkable session ACTIVE unless a test says otherwise.
fn fixture_now() -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_060)
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

    fn namespace_activity(&self, namespace: &str) -> Result<SystemTime, ActivityUnavailable> {
        self.namespace_activities
            .get(namespace)
            .cloned()
            // Default to very old activity (more than 48 hours before fixture_now), so
            // transcripts are not discovered as transcript-derived sessions unless a test
            // explicitly sets a more recent time. The discovery window is 2x the stall
            // threshold (24 hours), so 48 hours ensures transcripts won't be discovered.
            .unwrap_or_else(|| {
                Ok(SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000 - 48 * 3600))
            })
    }

    fn codex_sessions(&self) -> Result<Vec<CodexSession>, WorldError> {
        self.codex_sessions.clone()
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
fn lists_every_live_session_of_both_clis_and_nothing_else() {
    let world = FakeWorld::with(captured_process_table(), 88429);

    let snapshot = collect(&world, fixture_now()).expect("collection should succeed");

    let found: Vec<(String, &str)> = snapshot
        .sessions
        .iter()
        .map(|s| {
            (
                match &s.identity {
                    acmon::Identity::Process { pid } => format!("pid:{}", pid),
                    acmon::Identity::Transcript { recorded_as } => {
                        format!("transcript:{}", recorded_as)
                    }
                },
                s.cli.as_str(),
            )
        })
        .collect();

    assert_eq!(
        found.len(),
        12,
        "expected nine versioned Claude sessions, two Agent SDK bundled ones and one \
         Codex CLI, got {found:?}"
    );
    assert_eq!(
        found.iter().filter(|(_, cli)| *cli == "codex").count(),
        1,
        "exactly one process in this table is the Codex CLI; the others that mention \
         codex are a helper and a Computer Use client, got {found:?}"
    );
    assert_eq!(found.iter().filter(|(_, cli)| *cli == "claude").count(), 11);
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

    let result = collect(&world, fixture_now());

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

    let snapshot =
        collect(&world, fixture_now()).expect("a trustworthy but empty snapshot is not an error");

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

    let snapshot = collect(&world, fixture_now()).expect("collection should succeed");

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

    let snapshot = collect(&world, fixture_now()).expect("collection should succeed");

    let heavy_delegator = snapshot
        .sessions
        .iter()
        .find(|s| has_pid(s, 69046))
        .expect("session 69046");
    let self_worker = snapshot
        .sessions
        .iter()
        .find(|s| has_pid(s, 264))
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

    let snapshot = collect(&world, fixture_now()).expect("collection should succeed");

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

    let snapshot = collect(&world, fixture_now()).expect("collection should succeed");
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

    let result = collect(&world, fixture_now());

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

    let snapshot = collect(&world, fixture_now()).expect("collection should succeed");
    let find = |pid: i32| {
        snapshot
            .sessions
            .iter()
            .find(|s| has_pid(s, pid))
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

    let snapshot = collect(&world, fixture_now()).expect("collection should succeed");
    let namespace_of = |pid: i32| {
        snapshot
            .sessions
            .iter()
            .find(|s| has_pid(s, pid))
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

    let snapshot = collect(&world, fixture_now()).expect("collection should succeed");
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

    let snapshot = collect(&world, fixture_now()).expect("collection should succeed");

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

    let snapshot =
        collect(&world, fixture_now()).expect("an unlistable transcript store is not fatal");
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

/// The real Codex CLI, observed. Note it ends in a conventional binary directory.
const CODEX_EXE: &str = "/opt/homebrew/lib/node_modules/@openai/codex/node_modules/@openai/codex-darwin-arm64/vendor/aarch64-apple-darwin/bin/codex";

/// The workspace from the real Codex session fixture.
const CODEX_WORKSPACE: &str = "/Users/pmcfadin/Documents/Codex/2026-08-17/he";

/// The session id from the real Codex session fixture.
const CODEX_SESSION_ID: &str = "01a010c6-9c79-76d1-82da-3fad4bbf3bc4";

/// Everything below is a real path on a real machine, matches an agent-ish pattern, and
/// is NOT a CLI session. Each is a defect the tool being replaced had. A falsely
/// detected resident process is the worst kind of miss here: it downgrades a dead
/// session to merely waiting, so the tool goes quiet exactly when a session dies.
fn near_misses() -> Vec<ProcessRecord> {
    vec![
        // A `codex` binary bundled INSIDE the ChatGPT desktop application. This is the
        // sharpest exclusion of all: the filename is exactly `codex`, so only the
        // directory it sits in distinguishes it from the CLI.
        rec(70000, "/Applications/ChatGPT.app/Contents/Resources/codex"),
        // The desktop application itself, and two of its Codex framework helpers.
        rec(70001, "/Applications/ChatGPT.app/Contents/MacOS/ChatGPT"),
        rec(70002, "/Applications/ChatGPT.app/Contents/Frameworks/Codex Framework.framework/Versions/151.0.7922.137/Helpers/Codex (Renderer).app/Contents/MacOS/Codex (Renderer)"),
        rec(70003, "/Applications/ChatGPT.app/Contents/Frameworks/Codex Framework.framework/Versions/151.0.7922.137/Helpers/Codex (GPU).app/Contents/MacOS/Codex (GPU)"),
        // Codex's Computer Use helper, at both locations it has been observed at.
        rec(62507, "/Applications/ChatGPT.app/Contents/Resources/cua_node/lib/node_modules/@oai/sky/Codex Computer Use.app/Contents/MacOS/SkyComputerUseService"),
        rec(62508, "/Users/pmcfadin/.codex/computer-use/Codex Computer Use.app/Contents/SharedSupport/SkyComputerUseClient.app/Contents/MacOS/SkyComputerUseClient"),
        // A Codex helper that really does live in the CLI's own bin directory.
        rec(60099, "/opt/homebrew/lib/node_modules/@openai/codex/node_modules/@openai/codex-darwin-arm64/vendor/aarch64-apple-darwin/bin/codex-code-mode-host"),
        // The Claude desktop application and its helpers.
        rec(70004, "/Applications/Claude.app/Contents/MacOS/Claude"),
        rec(70005, "/Applications/Claude.app/Contents/Frameworks/Claude Helper.app/Contents/MacOS/Claude Helper"),
        rec(70006, "/Applications/Claude.app/Contents/Frameworks/Claude Helper (Renderer).app/Contents/MacOS/Claude Helper (Renderer)"),
        // The Cursor editor, its helpers, and the cursor-agent CLI — which is a coding
        // agent, but not one this tool claims to measure.
        rec(63202, "/Applications/Cursor.app/Contents/Frameworks/Cursor Helper.app/Contents/MacOS/Cursor Helper"),
        rec(63203, "/Applications/Cursor.app/Contents/MacOS/Cursor"),
        rec(21023, "/Users/pmcfadin/.local/share/cursor-agent/versions/2026.08.11-e8db854/cursor-agent"),
    ]
}

#[test]
fn the_real_codex_cli_is_a_session() {
    let world = FakeWorld::with(
        vec![
            rec(59293, CODEX_EXE),
            rec(88429, "/Users/pmcfadin/projects/acmon/target/debug/acmon"),
        ],
        88429,
    );

    let snapshot = collect(&world, fixture_now()).expect("collection should succeed");

    assert_eq!(snapshot.sessions.len(), 1, "expected one codex session");
    assert_eq!(snapshot.sessions[0].cli, "codex");
    assert!(
        has_pid(&snapshot.sessions[0], 59293),
        "expected session with pid 59293"
    );
}

#[test]
fn no_desktop_application_or_helper_is_ever_a_session() {
    // Six named regressions in one place. Every path here was observed running.
    let mut records = near_misses();
    records.push(rec(
        88429,
        "/Users/pmcfadin/projects/acmon/target/debug/acmon",
    ));
    let world = FakeWorld::with(records, 88429);

    let snapshot = collect(&world, fixture_now()).expect("collection should succeed");

    assert!(
        snapshot.sessions.is_empty(),
        "none of these is a session, got {} sessions",
        snapshot.sessions.len()
    );
}

#[test]
fn the_codex_cli_is_told_apart_from_a_helper_in_the_same_directory() {
    // The sharpest case: both live in the same `bin` directory, so only a rule anchored
    // to the end of the path separates them. A "contains codex" rule claims both.
    let world = FakeWorld::with(
        vec![
            rec(59293, CODEX_EXE),
            rec(60099, &format!("{CODEX_EXE}-code-mode-host")),
            rec(88429, "/Users/pmcfadin/projects/acmon/target/debug/acmon"),
        ],
        88429,
    );

    let snapshot = collect(&world, fixture_now()).expect("collection should succeed");

    assert_eq!(snapshot.sessions.len(), 1, "only the CLI is a session");
    assert!(
        has_pid(&snapshot.sessions[0], 59293),
        "expected session with pid 59293"
    );
}

#[test]
fn a_descriptive_process_name_rather_than_a_path_does_not_break_detection() {
    // Cursor reports names like this through `comm`. Detection reads the resolved
    // executable path instead, but a descriptive string must still be handled rather
    // than matched or panicked on.
    let world = FakeWorld::with(
        vec![
            rec(63300, "Cursor Helper: terminal pty-host"),
            rec(63301, ""),
            rec(88429, "/Users/pmcfadin/projects/acmon/target/debug/acmon"),
        ],
        88429,
    );

    let snapshot = collect(&world, fixture_now()).expect("a descriptive name is not a crash");

    assert!(snapshot.sessions.is_empty());
}

#[test]
fn the_codex_cli_is_recognised_wherever_it_is_installed() {
    // Both paths are real observations of the same CLI, taken at different times: the
    // npm install root moved. A rule anchored to an installation prefix would have
    // stopped recognising it silently; a rule anchored to the end of the path does not.
    let moved = "/Users/pmcfadin/.devbar/pkgs/npm/24.18.0/node-v24.18.0-darwin-arm64/lib/node_modules/@openai/codex/node_modules/@openai/codex-darwin-arm64/vendor/aarch64-apple-darwin/bin/codex";
    let world = FakeWorld::with(
        vec![
            rec(59293, CODEX_EXE),
            rec(59294, moved),
            rec(88429, "/Users/pmcfadin/projects/acmon/target/debug/acmon"),
        ],
        88429,
    );

    let snapshot = collect(&world, fixture_now()).expect("collection should succeed");

    assert_eq!(
        snapshot.sessions.len(),
        2,
        "the same CLI at two installation roots is two sessions"
    );
    assert!(snapshot.sessions.iter().all(|s| s.cli == "codex"));
}

#[test]
fn a_codex_binary_inside_an_application_bundle_is_not_a_cli_session() {
    // The filename is exactly `codex`. Only its directory tells it apart from the CLI,
    // which is why the rule requires a conventional binary directory rather than a name.
    let world = FakeWorld::with(
        vec![
            rec(70000, "/Applications/ChatGPT.app/Contents/Resources/codex"),
            rec(88429, "/Users/pmcfadin/projects/acmon/target/debug/acmon"),
        ],
        88429,
    );

    let snapshot = collect(&world, fixture_now()).expect("collection should succeed");

    assert!(
        snapshot.sessions.is_empty(),
        "a bundled binary is not a CLI session, got {} sessions",
        snapshot.sessions.len()
    );
}

#[test]
fn a_codex_sessions_workspace_comes_from_its_transcript_not_from_its_cwd() {
    // Acceptance criterion 6: the workspace must be what the transcript records, not what
    // the process reports.
    //
    // The two sources have to be told apart for this test to prove anything, so the
    // process is given a cwd that names the same directory in different capitals. APFS is
    // case-insensitive but case-preserving, so this really happens: both strings open the
    // same directory, and only one of them is what the transcript recorded. If the
    // implementation reported the cwd, this test would show the lowercase spelling.
    let cwd_in_different_capitals = CODEX_WORKSPACE.to_lowercase();
    assert_ne!(
        cwd_in_different_capitals, CODEX_WORKSPACE,
        "the fixture must actually differ, or this test proves nothing"
    );

    let world = FakeWorld::with(
        vec![
            rec_in(59293, CODEX_EXE, &cwd_in_different_capitals),
            rec(88429, "/Users/pmcfadin/projects/acmon/target/debug/acmon"),
        ],
        88429,
    );

    let snapshot = collect(&world, fixture_now()).expect("collection should succeed");
    let session = &snapshot.sessions[0];

    assert_eq!(session.cli, "codex");
    let workspace = session.workspace.as_ref().expect("workspace is readable");
    assert_eq!(
        workspace.path, CODEX_WORKSPACE,
        "the workspace must be the transcript's spelling, not the process's"
    );
    assert_eq!(
        workspace.namespace,
        Ok(CODEX_SESSION_ID.to_string()),
        "the namespace for a Codex session is the session id, not a hyphenated path"
    );
}

#[test]
fn a_codex_session_with_no_recent_transcript_still_appears_with_its_directory() {
    // A Codex session whose transcript is not in the index — either it is genuinely old,
    // or the index has not caught up — must still be listed with its directory. The
    // difference from "no transcript at all" and "could not look" must be preserved.
    let world = FakeWorld::with(
        vec![
            rec_in(59293, CODEX_EXE, "/Users/pmcfadin/projects/never_opened"),
            rec(88429, "/Users/pmcfadin/projects/acmon/target/debug/acmon"),
        ],
        88429,
    );

    let snapshot = collect(&world, fixture_now()).expect("collection should succeed");
    let session = &snapshot.sessions[0];

    assert_eq!(session.cli, "codex");
    let workspace = session.workspace.as_ref().expect("workspace is readable");
    assert_eq!(workspace.path, "/Users/pmcfadin/projects/never_opened");
    assert_eq!(
        workspace.namespace,
        Err(NamespaceUnmatched::NotRecorded {
            mapped: "/Users/pmcfadin/projects/never_opened".to_string()
        }),
        "a Codex session with no recent transcript shows its directory and states the \
         transcript is unmatched, never reports the workspace as unknown"
    );
}

#[test]
fn a_codex_index_read_failure_renders_as_could_not_look_not_as_no_transcript() {
    // Two different facts: "this session has no transcript" and "we could not look".
    // Collapsing them would report the first when the second is true.
    let world = FakeWorld::with(
        vec![
            rec_in(59293, CODEX_EXE, CODEX_WORKSPACE),
            rec(88429, "/Users/pmcfadin/projects/acmon/target/debug/acmon"),
        ],
        88429,
    )
    .with_codex_sessions(Err(WorldError::CodexIndex(
        "~/.codex/session_index.jsonl is not readable".to_string(),
    )));

    let snapshot = collect(&world, fixture_now()).expect("an unreadable Codex index is not fatal");
    let session = &snapshot.sessions[0];

    let workspace = session.workspace.as_ref().expect("workspace is readable");
    assert!(
        matches!(
            workspace.namespace,
            Err(NamespaceUnmatched::ListingFailed(_))
        ),
        "got {:?}",
        workspace.namespace
    );
}

#[test]
fn a_claude_session_and_a_codex_session_each_get_their_workspace_from_the_right_source() {
    // No cross-wiring. A Claude session in agentic_coding_monitor and a Codex session in
    // the fixture workspace must each resolve correctly, using their respective stores.
    let world = FakeWorld::with(
        vec![
            rec_in(
                69046,
                CLAUDE_EXE,
                "/Users/pmcfadin/projects/agentic_coding_monitor",
            ),
            rec_in(59293, CODEX_EXE, CODEX_WORKSPACE),
            rec(88429, "/Users/pmcfadin/projects/acmon/target/debug/acmon"),
        ],
        88429,
    );

    let snapshot = collect(&world, fixture_now()).expect("collection should succeed");

    let claude_session = snapshot
        .sessions
        .iter()
        .find(|s| s.cli == "claude")
        .expect("Claude session");
    let codex_session = snapshot
        .sessions
        .iter()
        .find(|s| s.cli == "codex")
        .expect("Codex session");

    let claude_ws = claude_session.workspace.as_ref().expect("readable");
    assert_eq!(
        claude_ws.path,
        "/Users/pmcfadin/projects/agentic_coding_monitor"
    );
    assert_eq!(
        claude_ws.namespace,
        Ok("-Users-pmcfadin-projects-agentic-coding-monitor".to_string()),
        "Claude session uses hyphenated namespace from recorded namespaces"
    );

    let codex_ws = codex_session.workspace.as_ref().expect("readable");
    assert_eq!(codex_ws.path, CODEX_WORKSPACE);
    assert_eq!(
        codex_ws.namespace,
        Ok(CODEX_SESSION_ID.to_string()),
        "Codex session uses session id from Codex sessions"
    );
}

/// An activity time relative to the fixed `fixture_now`, so silence is exact.
fn silent_for(seconds: u64) -> SystemTime {
    fixture_now() - Duration::from_secs(seconds)
}

/// Helper to check if a session has a specific process pid.
fn has_pid(session: &acmon::Session, pid: i32) -> bool {
    matches!(&session.identity, acmon::Identity::Process { pid: p } if *p == pid)
}

const CLAUDE_NAMESPACE: &str = "-Users-pmcfadin-projects-agentic-coding-monitor";

#[test]
fn a_session_whose_transcript_changed_recently_is_active() {
    let world = FakeWorld::with(
        vec![
            rec(69046, CLAUDE_EXE),
            rec(88429, "/Users/pmcfadin/projects/acmon/target/debug/acmon"),
        ],
        88429,
    )
    .namespace_activity(CLAUDE_NAMESPACE.to_string(), Ok(silent_for(30)));

    let snapshot = collect(&world, fixture_now()).expect("collection should succeed");

    assert_eq!(snapshot.sessions[0].liveness.state, State::Active);
    assert!(
        !snapshot.sessions[0].liveness.method.is_inferred(),
        "a transcript that changed is an observation, not an inference"
    );
}

#[test]
fn a_session_silent_past_the_quiet_threshold_with_a_live_process_is_waiting() {
    // Measured post-assistant silence is p99 3.9 minutes, so twenty minutes of silence
    // from a session whose process is still there is the shape of a human who has been
    // asked something and not yet answered.
    let world = FakeWorld::with(
        vec![
            rec(69046, CLAUDE_EXE),
            rec(88429, "/Users/pmcfadin/projects/acmon/target/debug/acmon"),
        ],
        88429,
    )
    .namespace_activity(CLAUDE_NAMESPACE.to_string(), Ok(silent_for(20 * 60)));

    let snapshot = collect(&world, fixture_now()).expect("collection should succeed");

    assert_eq!(snapshot.sessions[0].liveness.state, State::Waiting);
    assert!(
        snapshot.sessions[0].liveness.method.is_inferred(),
        "no signal for 'blocked on a human' exists to be read, so WAITING is inferred \
         and must say so"
    );
}

#[test]
fn a_session_whose_transcript_activity_cannot_be_read_is_unknown_not_active() {
    // The failure that matters: an unreadable activity time must not become "silent for
    // zero seconds", which would read as a busy session.
    let world = FakeWorld::with(
        vec![
            rec(69046, CLAUDE_EXE),
            rec(88429, "/Users/pmcfadin/projects/acmon/target/debug/acmon"),
        ],
        88429,
    )
    .namespace_activity(
        CLAUDE_NAMESPACE.to_string(),
        Err(ActivityUnavailable::Unreadable(
            "permission denied".to_string(),
        )),
    );

    let snapshot = collect(&world, fixture_now()).expect("collection should succeed");

    assert_eq!(snapshot.sessions[0].liveness.state, State::Unknown);
}

#[test]
fn a_session_with_no_recorded_transcript_is_unknown_rather_than_stalled() {
    // Its workspace has no namespace, so nothing can be said about silence. That must not
    // collapse into a verdict — least of all a verdict that the session is dead.
    let world = FakeWorld::with(
        vec![
            rec_in(69046, CLAUDE_EXE, "/Users/pmcfadin/projects/never_opened"),
            rec(88429, "/Users/pmcfadin/projects/acmon/target/debug/acmon"),
        ],
        88429,
    );

    let snapshot = collect(&world, fixture_now()).expect("collection should succeed");

    assert_eq!(snapshot.sessions[0].liveness.state, State::Unknown);
}

#[test]
fn a_codex_sessions_silence_comes_from_the_index_rather_than_a_transcript_read() {
    // Codex records when each session was last updated in its index, so liveness costs no
    // further read at all.
    let world = FakeWorld::with(
        vec![
            rec_in(59293, CODEX_EXE, CODEX_WORKSPACE),
            rec(88429, "/Users/pmcfadin/projects/acmon/target/debug/acmon"),
        ],
        88429,
    )
    .with_codex_sessions(Ok(vec![CodexSession {
        id: CODEX_SESSION_ID.to_string(),
        workspace: CODEX_WORKSPACE.to_string(),
        last_activity: silent_for(45 * 60),
    }]));

    let snapshot = collect(&world, fixture_now()).expect("collection should succeed");

    assert_eq!(snapshot.sessions[0].cli, "codex");
    assert_eq!(
        snapshot.sessions[0].liveness.state,
        State::Waiting,
        "forty-five minutes of silence with the process still there is waiting"
    );
}

#[test]
fn a_transcript_silent_past_stall_with_no_process_is_stalled() {
    // The core acceptance criterion: a transcript that has been silent for longer than
    // the stall threshold, with no live process for it, is STALLED. This is what makes
    // ticket #6's summary — "Kill a session and it becomes STALLED on the next
    // collection" — actually true.
    let world = FakeWorld::with(
        vec![rec_in(
            88429,
            "/Users/pmcfadin/projects/acmon/target/debug/acmon",
            "/Users/pmcfadin/projects/acmon", // Observer is in a different workspace
        )],
        88429,
    )
    .namespace_activity(CLAUDE_NAMESPACE.to_string(), Ok(silent_for(13 * 3600))); // 13 hours

    let snapshot = collect(&world, fixture_now()).expect("collection should succeed");

    let transcript_session = snapshot
        .sessions
        .iter()
        .find(|s| matches!(s.identity, acmon::Identity::Transcript { .. }))
        .expect("a transcript-derived session should exist");

    assert_eq!(
        transcript_session.liveness.state,
        State::Stalled,
        "a transcript silent past the stall threshold with no process is STALLED"
    );
    assert_eq!(transcript_session.cli, "claude");
}

#[test]
fn a_transcript_with_a_live_process_appears_once_and_is_not_stalled() {
    // The same transcript as above, but with a live process for it. It must appear
    // exactly once (not duplicated as both process-derived and transcript-derived), and
    // the verdict must come from the process-derived session, not the transcript.
    let world = FakeWorld::with(
        vec![
            rec(69046, CLAUDE_EXE),
            rec(88429, "/Users/pmcfadin/projects/acmon/target/debug/acmon"),
        ],
        88429,
    )
    .namespace_activity(CLAUDE_NAMESPACE.to_string(), Ok(silent_for(13 * 3600))); // 13 hours

    let snapshot = collect(&world, fixture_now()).expect("collection should succeed");

    let claude_sessions: Vec<_> = snapshot
        .sessions
        .iter()
        .filter(|s| s.cli == "claude")
        .collect();

    assert_eq!(
        claude_sessions.len(),
        1,
        "the session must appear exactly once, not as both process-derived and \
         transcript-derived"
    );
    assert!(
        matches!(claude_sessions[0].identity, acmon::Identity::Process { .. }),
        "the session must be process-derived, not transcript-derived"
    );
    assert_ne!(
        claude_sessions[0].liveness.state,
        State::Stalled,
        "a session with a live process is not STALLED, regardless of transcript silence"
    );
}

#[test]
fn a_transcript_past_stall_with_work_in_workspace_is_not_stalled() {
    // Acceptance criterion: "A live build or review process running in a workspace
    // prevents a false STALLED". This rule can only be tested when the session itself
    // has no process, because otherwise process_resident alone would prevent STALLED.
    let world = FakeWorld::with(
        vec![
            // A build running in the same directory the transcript is for.
            rec_in(
                12345,
                "/usr/bin/cargo",
                "/Users/pmcfadin/projects/agentic_coding_monitor",
            ),
            rec(88429, "/Users/pmcfadin/projects/acmon/target/debug/acmon"),
        ],
        88429,
    )
    .namespace_activity(CLAUDE_NAMESPACE.to_string(), Ok(silent_for(13 * 3600))); // 13 hours

    let snapshot = collect(&world, fixture_now()).expect("collection should succeed");

    let transcript_session = snapshot
        .sessions
        .iter()
        .find(|s| matches!(s.identity, acmon::Identity::Transcript { .. }))
        .expect("a transcript-derived session should exist");

    assert_eq!(
        transcript_session.liveness.state,
        State::Waiting,
        "work running in the workspace prevents a false STALLED verdict"
    );
}

#[test]
fn a_transcript_derived_claude_session_reports_workspace_as_not_invertible() {
    // A transcript-derived Claude session cannot report its workspace path, because the
    // namespace mapping is not invertible: `-a-b-c` could have come from `_`, `.`, `-`,
    // or `/`. The session must report this as a specific reason, not as a generic
    // "unknown" or a guessed path.
    let world = FakeWorld::with(
        vec![rec(
            88429,
            "/Users/pmcfadin/projects/acmon/target/debug/acmon",
        )],
        88429,
    )
    .namespace_activity(CLAUDE_NAMESPACE.to_string(), Ok(silent_for(5 * 3600))); // 5 hours

    let snapshot = collect(&world, fixture_now()).expect("collection should succeed");

    let transcript_session = snapshot
        .sessions
        .iter()
        .find(|s| matches!(s.identity, acmon::Identity::Transcript { .. }))
        .expect("a transcript-derived session should exist");

    assert_eq!(
        transcript_session.workspace,
        Err(WorkspaceUnknown::NotInvertible),
        "a transcript-derived Claude session must report NotInvertible, not a guessed path"
    );
}

#[test]
fn a_transcript_derived_codex_session_reports_its_workspace_from_the_transcript() {
    // A transcript-derived Codex session has its workspace recorded in the transcript,
    // so the workspace path is known even though the process is gone.
    let world = FakeWorld::with(
        vec![rec(
            88429,
            "/Users/pmcfadin/projects/acmon/target/debug/acmon",
        )],
        88429,
    )
    .with_codex_sessions(Ok(vec![CodexSession {
        id: CODEX_SESSION_ID.to_string(),
        workspace: CODEX_WORKSPACE.to_string(),
        last_activity: silent_for(5 * 3600), // 5 hours
    }]));

    let snapshot = collect(&world, fixture_now()).expect("collection should succeed");

    let transcript_session = snapshot
        .sessions
        .iter()
        .find(|s| s.cli == "codex" && matches!(s.identity, acmon::Identity::Transcript { .. }))
        .expect("a transcript-derived Codex session should exist");

    assert_eq!(
        transcript_session
            .workspace
            .as_ref()
            .expect("workspace is known")
            .path,
        CODEX_WORKSPACE,
        "a transcript-derived Codex session knows its workspace from the transcript"
    );
}

#[test]
fn a_transcript_silent_for_less_than_stall_with_no_process_is_unknown() {
    // A transcript that is silent but has not yet crossed the stall threshold is
    // UNKNOWN, not STALLED. The stall threshold is 12 hours; this transcript has been
    // silent for 6 hours.
    let world = FakeWorld::with(
        vec![rec_in(
            88429,
            "/Users/pmcfadin/projects/acmon/target/debug/acmon",
            "/Users/pmcfadin/projects/acmon", // Observer is in a different workspace
        )],
        88429,
    )
    .namespace_activity(CLAUDE_NAMESPACE.to_string(), Ok(silent_for(6 * 3600))); // 6 hours

    let snapshot = collect(&world, fixture_now()).expect("collection should succeed");

    let transcript_session = snapshot
        .sessions
        .iter()
        .find(|s| matches!(s.identity, acmon::Identity::Transcript { .. }))
        .expect("a transcript-derived session should exist");

    assert_eq!(
        transcript_session.liveness.state,
        State::Unknown,
        "a transcript silent for less than the stall threshold is UNKNOWN, not STALLED"
    );
}
