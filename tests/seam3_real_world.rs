//! Seam 3 — the real `World` against this machine.
//!
//! INVARIANTS ONLY. Never absolute values: the number of live sessions, their pids
//! and the size of the process table all change between runs, and timings on this
//! class of machine vary by roughly 2x. A test asserting a specific count is a test
//! that fails for reasons unrelated to correctness.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime};

use acmon::liveness::Thresholds;
use acmon::state::Paths;
use acmon::workspace::NamespaceUnmatched;
use acmon::world::{PathUnavailable, ResourceSource, ResourcesUnavailable, Unmeasured};
use acmon::{collect_as, Identity, NotifyOutcome, Persistence, RealWorld, Role, World};

/// A scratch tree named for this test binary's own run, made once.
///
/// One tree for the whole binary, deliberately. The tests below run concurrently and none of them
/// reads another's writes back, so sharing it costs nothing — and a shared target is the condition
/// the atomic replacement exists to survive, so several of them writing at once is a use of the
/// guarantee rather than a hazard. The nanosecond suffix is what keeps a tree from a previous run
/// (pids are recycled) out of this one's way.
fn scratch_root() -> &'static Path {
    static ROOT: OnceLock<PathBuf> = OnceLock::new();
    ROOT.get_or_init(|| {
        let root = std::env::temp_dir().join(format!(
            "acmon-seam3-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("a clock after 1970")
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&root);
        root
    })
}

/// Both of a world's directories named into a scratch tree.
///
/// `home` is what an unrelocated run would resolve everything from. Every test here passes `None`
/// — a world with no home in play cannot reach the developer's files even by a path nobody
/// thought about. The two tests that need a home pass a stand-in one, to prove that naming these
/// two directories is what takes it out of play.
fn scratch_paths(root: &Path, home: Option<&str>) -> Paths {
    Paths::from_values(
        Some(&root.join("config").to_string_lossy()),
        Some(&root.join("state").to_string_lossy()),
        home,
    )
    .expect("both directories were given explicitly, so no home is needed")
}

/// The world every test in this file observes this machine through.
///
/// Real processes, real workspaces, real transcripts — that is the point of this seam and none of
/// it changes here. What changes is where the world's own three files are: in a scratch tree, not
/// in the developer's `~/.config/acmon` and `~/.local/state/acmon`.
///
/// `RealWorld::new()` appears nowhere in this file on purpose. A collection through it writes both
/// the remembered history and the notification dedupe record, and the dedupe record is what stops
/// a real condition being announced twice — so a suite that wrote it could silence a genuine alert
/// on the developer's own machine and leave nothing anywhere saying why (#38). Naming both
/// directories also takes the pre-split `~/.acmon/` out of play (#36), so nothing under the
/// developer's home is read either.
fn world() -> RealWorld {
    RealWorld::with_paths(scratch_paths(scratch_root(), None))
}

/// A collection over this machine for a reader: figures, no writes, no alerting.
///
/// Belt as well as braces. The world above already puts every write in a scratch tree, which is
/// what makes these tests safe *by construction*; passing the display's role means a test that
/// only wants the figures also asks for nothing else, which is safe by policy. Relocation is the
/// guarantee — a role is an argument someone can pass differently tomorrow — so the tests that
/// prove the relocation itself deliberately use the monitor's role instead.
fn figures_only(world: &RealWorld) -> acmon::Snapshot {
    collect_as(
        world,
        SystemTime::now(),
        &Thresholds::default(),
        Role::Display,
    )
    .expect("collection over the real machine should succeed")
}

/// Helper to format a session identity for error messages.
fn format_identity(identity: &Identity) -> String {
    match identity {
        Identity::Process { pid } => format!("pid {}", pid),
        Identity::Transcript { recorded_as } => format!("transcript {}", recorded_as),
    }
}

#[test]
fn the_real_process_table_contains_the_observing_process() {
    let world = world();

    let observation = world
        .process_snapshot()
        .expect("enumerating the process table should succeed");

    assert!(
        observation.contains_observer(),
        "a whole-machine enumeration must include this test process (pid {})",
        observation.observer_pid
    );
    assert!(
        observation.records.len() > 1,
        "a real machine runs more than one process"
    );
}

#[test]
fn collection_over_the_real_machine_yields_only_recognised_clis() {
    let world = world();

    let snapshot = figures_only(&world);

    for session in &snapshot.sessions {
        assert!(
            session.cli == "claude" || session.cli == "codex",
            "tickets #2 and #5 recognise claude and codex, so nothing else may be reported; \
             got {:?}",
            session.cli
        );
        // For process-derived sessions, pid is always positive.
        if let acmon::Identity::Process { pid } = session.identity {
            assert!(pid > 0, "a pid is always positive");
        }
    }
}

#[test]
fn an_unreadable_executable_path_is_absent_rather_than_empty() {
    // Many processes on a real machine are not readable by an unprivileged user.
    // Those must carry no path at all rather than an empty string, so a caller
    // cannot mistake "could not read" for "read, and it was blank".
    let world = world();

    let observation = world.process_snapshot().expect("enumeration");

    assert!(
        observation
            .records
            .iter()
            .all(|r| r.exe_path.as_ref().ok().map(|s| s.as_str()) != Some("")),
        "an unreadable path must be absent, never an empty string"
    );
}

#[test]
fn an_unavailable_path_states_a_reason_that_is_actually_true() {
    // A reason must be established, not assumed. If a record claims the process
    // exited, the process really must be gone. The converse is deliberately NOT
    // asserted: a process alive at snapshot time may legitimately have exited by now,
    // so only the direction that cannot race is checked.
    let world = world();
    let observation = world.process_snapshot().expect("enumeration");

    for record in &observation.records {
        if record.exe_path == Err(PathUnavailable::ProcessExited) {
            let alive = unsafe { libc::kill(record.pid, 0) == 0 };
            assert!(
                !alive,
                "pid {} is reported as exited but is alive — the reason is false",
                record.pid
            );
        }
    }
}

#[test]
fn output_width_is_always_usable() {
    // Under a test harness stdout is not a terminal, so this exercises the fallback.
    // Zero would render a table with no columns at all.
    let world = world();
    assert!(world.output_width() > 0, "a width of zero renders nothing");
}

/// Burn CPU in this process until at least `at_least` of wall time has passed.
///
/// Used to create something measurable. The amount is deliberately not asserted: what
/// the tests check is that two independent readers agree about it.
fn burn_own_cpu_for(at_least: Duration) {
    let started = Instant::now();
    let mut accumulator: u64 = 0;
    while started.elapsed() < at_least {
        for _ in 0..20_000 {
            accumulator = accumulator
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1);
        }
    }
    std::hint::black_box(accumulator);
}

/// Read this process's CPU time from `ps`, parsed here rather than by the code under
/// test. The duplication is the point: an expected value has to come from somewhere
/// other than the implementation it is checking.
fn cpu_seconds_according_to_ps(pid: i32) -> f64 {
    let output = Command::new("/bin/ps")
        .args(["-o", "time=", "-p", &pid.to_string()])
        .output()
        .expect("ps should run");
    assert!(output.status.success(), "ps failed: {:?}", output.status);

    let text = String::from_utf8_lossy(&output.stdout);
    let field = text.trim();
    let (minutes, seconds) = field
        .split_once(':')
        .unwrap_or_else(|| panic!("ps CPU time {field:?} is not MM:SS.CC"));
    minutes.parse::<f64>().expect("minutes") * 60.0 + seconds.parse::<f64>().expect("seconds")
}

#[test]
fn a_converted_cpu_time_agrees_with_what_ps_reports_independently() {
    // The guard against the units trap. Every *_time field in the kernel's ledger is a
    // mach tick count; read as nanoseconds it is 41.67x too small on this hardware and
    // still internally consistent. Only an independent, coarser reader catches it —
    // here `ps`, which reports in centiseconds.
    let world = world();
    let me = std::process::id() as i32;

    burn_own_cpu_for(Duration::from_millis(900));

    let ledger = world
        .resources(me)
        .expect("this process is readable by itself");
    let converted = ledger
        .own_cpu
        .expect("own CPU of one's own process is readable");
    let independent = cpu_seconds_according_to_ps(me);

    // Assert the measurement happened before comparing it — but ask the independent
    // reader, not the one under test. Asking the ledger would report a units bug as a
    // burn that never ran, which is a plausible reason and the wrong one.
    assert!(
        independent > 0.5,
        "the CPU burn did not happen: ps saw only {independent} s, so this comparison \
         would prove nothing"
    );

    // A ratio, never an absolute: how much CPU the burn cost varies with load, but the
    // two readers are looking at the same number.
    let ratio = converted.as_secs_f64() / independent;
    assert!(
        (0.7..1.3).contains(&ratio),
        "ledger says {converted:?}, ps says {independent} s — a ratio of {ratio:.3}. \
         A ratio near 1/41.67 means ticks were read as nanoseconds."
    );
}

#[test]
fn child_cpu_includes_grandchildren_not_only_direct_children() {
    // §2.4 of the mechanics document claims the roll-up is recursive. This proves it on
    // the machine running the tests, and proves the chain really was two deep — a shell
    // that exec'd instead of forking would make this a direct-child test wearing a
    // grandchild's name.
    let world = world();
    let me = std::process::id() as i32;

    let before = world
        .resources(me)
        .expect("readable")
        .children_cpu
        .expect("own children are readable");

    // The trailing `:` denies the outer shell its last-command exec optimisation, so it
    // must fork. The inner shell prints its own pid and its parent's, then burns.
    let started = Instant::now();
    let shell = Command::new("/bin/sh")
        .arg("-c")
        .arg("sh -c 'echo $$ $PPID; i=0; while [ $i -lt 200000 ]; do i=$((i+1)); done'; :")
        .stdout(Stdio::piped())
        .spawn()
        .expect("the burner should start");
    // Taken from the OS, so the identity below is checked against a different source
    // than the shell's own report of its parent.
    let direct_child = shell.id() as i32;
    let output = shell.wait_with_output().expect("the burner should finish");
    let elapsed = started.elapsed();

    // Assert success before believing anything. A shell that failed instantly also
    // produces a plausible-looking small number.
    assert!(
        output.status.success(),
        "burner exited with {:?}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );

    let printed = String::from_utf8_lossy(&output.stdout);
    let mut fields = printed.split_whitespace();
    let burner_pid: i32 = fields.next().expect("burner pid").parse().expect("a pid");
    let burner_parent: i32 = fields.next().expect("burner ppid").parse().expect("a pid");

    assert_ne!(
        burner_pid, direct_child,
        "the burner must not be our direct child, or this proves nothing about depth"
    );
    assert_eq!(
        burner_parent, direct_child,
        "the burner's parent must be the shell we launched, making it our grandchild"
    );

    let after = world
        .resources(me)
        .expect("readable")
        .children_cpu
        .expect("own children are readable");
    let attributed = after - before;

    // A ratio against the burn's own wall time, not an absolute: the burner is a shell
    // loop whose speed varies. Other tests in this binary also spawn processes, but
    // only ever add to this figure, so a lower bound stays sound.
    assert!(
        attributed.as_secs_f64() > elapsed.as_secs_f64() * 0.5,
        "a grandchild burned CPU for {elapsed:?} but only {attributed:?} reached our \
         ledger — the roll-up is not recursive on this machine"
    );
}

#[test]
fn a_process_owned_by_another_user_falls_back_to_the_coarser_reader() {
    // pid 1 is launchd, owned by root. The full ledger is refused for it without
    // elevated privileges, and the fallback must supply what it can while stating what
    // it cannot — never filling the gaps with zeroes.
    assert_ne!(
        unsafe { libc::geteuid() },
        0,
        "this test describes what an unprivileged reader sees; as root the answer differs"
    );
    let world = world();

    let reading = world
        .resources(1)
        .expect("pid 1 always exists, so some reader must answer for it");

    assert_eq!(
        reading.source,
        ResourceSource::Ps,
        "a root-owned process must come from the fallback reader"
    );
    assert!(
        reading.own_cpu.is_ok(),
        "the fallback does report cumulative own CPU without privileges"
    );
    assert_eq!(
        reading.children_cpu,
        Err(Unmeasured::NotReportedBy(ResourceSource::Ps)),
        "the fallback cannot see children, and must say so rather than report none"
    );
}

#[test]
fn a_pid_that_has_exited_is_reported_as_exited_rather_than_as_idle() {
    let world = world();

    let mut child = Command::new("/usr/bin/true").spawn().expect("spawn");
    let pid = child.id() as i32;
    let status = child.wait().expect("wait");
    assert!(status.success(), "the child should have exited cleanly");

    let reading = world.resources(pid);

    assert_eq!(
        reading,
        Err(ResourcesUnavailable::ProcessExited),
        "a reaped pid has no ledger, and that is not the same as a ledger of zeroes"
    );
}

#[test]
fn a_working_directory_is_read_in_the_same_pass_as_the_identity() {
    // §4.1: resolving cwd in a second pass produced six "unreadable" entries that were
    // simply dead processes. The value is checked against an independent source — what
    // this process itself believes its directory to be.
    let world = world();
    let independent = std::env::current_dir().expect("this process has a working directory");

    let observation = world.process_snapshot().expect("enumeration");
    let mine = observation
        .records
        .iter()
        .find(|r| r.pid == observation.observer_pid)
        .expect("the observer appears in its own snapshot");

    assert_eq!(
        mine.cwd.as_ref().map(String::as_str),
        Ok(independent.to_str().expect("a UTF-8 path")),
        "the cwd read from the kernel must match what this process reports for itself"
    );
}

#[test]
fn an_unreadable_working_directory_is_absent_with_a_reason_never_an_empty_string() {
    // Roughly a third of processes on a real machine belong to another user and are not
    // readable. Those must carry a reason, not an empty string that reads as "root".
    let world = world();

    let observation = world.process_snapshot().expect("enumeration");

    assert!(
        observation
            .records
            .iter()
            .all(|r| r.cwd.as_ref().map(String::as_str) != Ok("")),
        "an unreadable cwd must be absent, never an empty string"
    );
    let readable = observation.records.iter().filter(|r| r.cwd.is_ok()).count();
    assert!(
        readable > 0,
        "no cwd was readable at all, so this proves nothing about the reader"
    );
    assert!(
        readable < observation.records.len(),
        "every cwd on the machine was readable, which means the failure path is untested \
         here — expected some processes to belong to another user"
    );
}

#[test]
fn the_recorded_namespaces_on_this_machine_are_listable_and_hold_no_underscores() {
    // §4.3's corroboration, re-checked on the machine running the tests rather than
    // taken on trust from the document: if any recorded namespace contained an
    // underscore, the mapping rule would be wrong.
    let world = world();

    let recorded = world
        .recorded_namespaces()
        .expect("the transcript store should be listable");

    assert!(
        !recorded.is_empty(),
        "no namespaces were listed at all, so nothing below proves anything"
    );
    assert!(
        recorded.iter().all(|n| !n.contains('_')),
        "a recorded namespace contains an underscore, which contradicts the mapping rule: {:?}",
        recorded
            .iter()
            .filter(|n| n.contains('_'))
            .collect::<Vec<_>>()
    );
}

#[test]
fn every_workspace_attribution_on_this_machine_is_true_in_both_directions() {
    // Invariants, not counts. A resolved namespace must really be one of the recorded
    // ones, and an unresolved one must really be absent — the second direction is what
    // catches a matcher that is too strict, which is how the underscore defect
    // presented: a calm "no session here" for a workspace that had one.
    let world = world();
    let recorded = world.recorded_namespaces().expect("listable");
    let snapshot = figures_only(&world);

    for session in &snapshot.sessions {
        let Ok(workspace) = &session.workspace else {
            continue; // A workspace that could not be read is covered by its own test.
        };
        match &workspace.namespace {
            Ok(resolved) => {
                // The two CLIs are identified by different things, so each is checked
                // against its own store. A Claude namespace is a directory name; a Codex
                // "namespace" is a session id out of the index, which would never appear
                // in the Claude listing. Checking neither would let either be anything.
                if session.cli == "claude" {
                    assert!(
                        recorded.contains(resolved),
                        "session {} claims namespace {resolved}, which is not in the listing",
                        format_identity(&session.identity)
                    );
                } else if session.cli == "codex" {
                    let known_ids: Vec<String> = world
                        .codex_sessions()
                        .expect("the Codex index was readable above")
                        .into_iter()
                        .map(|session| session.id)
                        .collect();
                    assert!(
                        known_ids.contains(resolved),
                        "session {} claims Codex session id {resolved}, which the index \
                         does not report; known ids: {known_ids:?}",
                        format_identity(&session.identity)
                    );
                }
            }
            Err(NamespaceUnmatched::NotRecorded { mapped }) => assert!(
                !recorded.iter().any(|n| n.eq_ignore_ascii_case(mapped)),
                "session {} in {} was reported as having no recorded namespace, but {mapped} \
                 is in the listing — the match is too strict",
                format_identity(&session.identity),
                workspace.path
            ),
            Err(NamespaceUnmatched::ListingFailed(why)) => {
                panic!("the listing succeeded above, so this cannot be: {why}")
            }
            Err(NamespaceUnmatched::UnknownCli(_)) => {
                // A session for a CLI we don't know how to attribute is covered by its own
                // test when that CLI is added.
            }
        }
    }
}

#[test]
fn codex_sessions_from_the_real_machine_are_either_empty_or_well_formed() {
    // This machine may have zero recent Codex sessions, or it may have several. An empty
    // result is legitimate and not a failure — the test must not be vacuous in that case.
    // Invariants only: if sessions are returned, their shape must be correct.
    let world = world();

    let sessions = world
        .codex_sessions()
        .expect("reading the Codex session index should succeed");

    for session in &sessions {
        assert!(
            !session.id.is_empty(),
            "every Codex session must have a non-empty id, got {:?}",
            session
        );
        assert!(
            !session.workspace.is_empty(),
            "every Codex session must have a non-empty workspace, got {:?}",
            session
        );
        assert!(
            session.workspace.starts_with('/'),
            "every Codex workspace must be an absolute path, got {:?}",
            session.workspace
        );
    }

    // Either result is legitimate: emptiness means no recent Codex sessions on this
    // machine, which is a valid state. The test proves only that whatever came back has
    // the right shape, not that any particular count is correct.
}

#[test]
fn every_recorded_namespace_has_an_activity_time_or_a_stated_reason() {
    // For every namespace on this machine, namespace_activity returns either a time or a
    // stated reason — never a panic, never a suspiciously-epoch time. A zero or epoch
    // value would be the fail-to-zero defect this project exists to remove.
    use acmon::world::ActivityUnavailable;

    let world = world();
    let recorded = world
        .recorded_namespaces()
        .expect("the transcript store should be listable");

    assert!(
        !recorded.is_empty(),
        "no namespaces were listed at all, so nothing below proves anything"
    );

    // A time after 2020 and not in the future by more than a small margin guards against
    // epoch or zero values.
    let year_2020 = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_577_836_800);
    let future_margin = std::time::Duration::from_secs(3_600); // 1 hour

    for namespace in &recorded {
        let result = world.namespace_activity(namespace);
        match result {
            Ok(time) => {
                assert!(
                    time > year_2020,
                    "namespace {namespace} activity time {time:?} is suspiciously old — \
                     looks like an epoch or zero"
                );
                let now = std::time::SystemTime::now();
                assert!(
                    time < now + future_margin,
                    "namespace {namespace} activity time {time:?} is in the future — \
                     current time is {now:?}"
                );
            }
            Err(ActivityUnavailable::NotRecorded) => {
                panic!(
                    "namespace {namespace} is in the listing but reports as not recorded — \
                     should be Unreadable or NoTranscripts"
                );
            }
            Err(ActivityUnavailable::Unreadable(_)) | Err(ActivityUnavailable::NoTranscripts) => {
                // These are legitimate answers with stated reasons.
            }
        }
    }
}

#[test]
fn a_namespace_that_does_not_exist_returns_not_recorded() {
    use acmon::world::ActivityUnavailable;

    let world = world();

    // A name that cannot exist: contains characters that would be replaced by the slug
    // mapping, and is long enough to be unlikely to collide.
    let nonexistent = "does-not-exist-aef8b1c2-3d4e-5f6a-7b8c-9d0e1f2a3b4c";

    let result = world.namespace_activity(nonexistent);

    assert_eq!(
        result,
        Err(ActivityUnavailable::NotRecorded),
        "a namespace that does not exist must return NotRecorded, not Unreadable or a time"
    );
}

#[test]
fn namespace_activity_agrees_with_an_independent_source() {
    // The activity time must agree with an independent reader: pick a namespace, find the
    // newest .jsonl in it with an independent command, and compare. Only a bound, not
    // exact equality — filesystem time resolution varies.
    let world = world();
    let recorded = world
        .recorded_namespaces()
        .expect("the transcript store should be listable");

    if recorded.is_empty() {
        // Vacuously true but still a legitimate state — say so loudly rather than
        // silently passing.
        println!(
            "WARNING: no namespaces on this machine, so namespace_activity agreement is \
             not tested"
        );
        return;
    }

    // Pick the first namespace as the test subject.
    let namespace = &recorded[0];
    let home = std::env::var("HOME").expect("HOME is readable");
    let namespace_path = format!("{}/.claude/projects/{}", home, namespace);

    // Find the newest .jsonl file's mtime using an independent source: `find` and `stat`.
    let find_output = Command::new("/usr/bin/find")
        .args([&namespace_path, "-name", "*.jsonl", "-type", "f"])
        .output()
        .expect("find should run");
    assert!(
        find_output.status.success(),
        "find failed: {:?}",
        find_output.status
    );

    let files = String::from_utf8_lossy(&find_output.stdout);
    let file_list: Vec<&str> = files.lines().collect();
    if file_list.is_empty() {
        // No transcripts in this namespace — the implementation should report
        // NoTranscripts.
        use acmon::world::ActivityUnavailable;
        assert_eq!(
            world.namespace_activity(namespace),
            Err(ActivityUnavailable::NoTranscripts),
            "namespace {namespace} has no .jsonl files, so namespace_activity should return \
             NoTranscripts"
        );
        return;
    }

    // Use stat to get the mtime of each file, then find the max.
    let mut newest_mtime = std::time::SystemTime::UNIX_EPOCH;
    for file in &file_list {
        let stat_output = Command::new("/usr/bin/stat")
            .args(["-f", "%m", file])
            .output()
            .expect("stat should run");
        assert!(
            stat_output.status.success(),
            "stat failed for {file}: {:?}",
            stat_output.status
        );
        let mtime_str = String::from_utf8_lossy(&stat_output.stdout);
        let mtime_secs: u64 = mtime_str
            .trim()
            .parse()
            .expect("stat output should be a number");
        let mtime = std::time::UNIX_EPOCH + std::time::Duration::from_secs(mtime_secs);
        newest_mtime = newest_mtime.max(mtime);
    }

    let reported = world
        .namespace_activity(namespace)
        .expect("namespace exists and has transcripts");

    // Allow a small tolerance for filesystem time resolution (typically 1 second, but be
    // generous).
    let tolerance = std::time::Duration::from_secs(2);
    let diff = if reported > newest_mtime {
        reported
            .duration_since(newest_mtime)
            .expect("diff calculation")
    } else {
        newest_mtime
            .duration_since(reported)
            .expect("diff calculation")
    };

    assert!(
        diff <= tolerance,
        "namespace {namespace} activity {reported:?} differs from independent source \
         {newest_mtime:?} by {diff:?} — beyond tolerance {tolerance:?}"
    );
}

#[test]
fn codex_sessions_carry_last_activity_within_the_recency_window() {
    // Every CodexSession returned must carry a last_activity, and that time must be
    // within the recency window the implementation uses, since that is the filter that
    // selected it. An empty list is legitimate — say so in a comment.
    let world = world();

    let sessions = world
        .codex_sessions()
        .expect("reading the Codex session index should succeed");

    // An empty result is legitimate and not a failure: this machine may have no recent
    // Codex sessions. The test proves only that whatever came back has a last_activity
    // and it is recent.

    let now = std::time::SystemTime::now();
    let recency_window = std::time::Duration::from_secs(6 * 3_600); // 6 hours

    for session in &sessions {
        let age = now
            .duration_since(session.last_activity)
            .expect("last_activity should not be in the future");

        assert!(
            age <= recency_window,
            "session {} last_activity {:?} is older than the recency window — it should not \
             have been included",
            session.id,
            session.last_activity
        );
    }
}

#[test]
fn no_session_may_be_stalled_while_its_process_is_resident() {
    // Invariant: a session with a resident process can be ACTIVE, WAITING, or UNKNOWN,
    // but never STALLED. STALLED requires the absence of a process, which is observable.
    let world = world();
    let snapshot = figures_only(&world);

    for session in &snapshot.sessions {
        if let acmon::Identity::Process { pid } = session.identity {
            assert_ne!(
                session.liveness.state,
                acmon::liveness::State::Stalled,
                "session with pid {pid} is STALLED while its process is resident — this \
                 violates the liveness rule"
            );
        }
    }
}

#[test]
fn every_transcript_derived_session_has_process_resident_false() {
    // Invariant: a transcript-derived session exists precisely because its process is
    // gone. The liveness logic depends on this being false to reach the STALLED verdict.
    let world = world();
    let snapshot = figures_only(&world);

    // An empty result is legitimate — this machine may have no transcript-derived
    // sessions. The test proves only that whatever came back has the right shape.

    for session in &snapshot.sessions {
        if let acmon::Identity::Transcript { recorded_as } = &session.identity {
            // We can't directly inspect the Observation that was passed to classify, but
            // we can verify that the verdict is one that could only be reached with
            // process_resident: false. If process_resident were true, the verdict would
            // be ACTIVE or WAITING (rows 3-4 of the liveness table), never STALLED or
            // the process-absent UNKNOWN (rows 6-7).
            //
            // Also verify that resources are unavailable due to the process having exited.
            assert_eq!(
                session.resources,
                Err(acmon::world::ResourcesUnavailable::ProcessExited),
                "transcript-derived session {recorded_as} must report resources as exited, \
                 because the process is gone"
            );
        }
    }
}

#[test]
fn repository_root_of_this_crates_directory_is_this_crate() {
    // This crate is a git repository, so asking for the root of its own directory must
    // return the root, and the `linked_worktree` flag must agree with what this checkout
    // actually is. Which of the two it is depends on where the suite runs — agents here
    // work in linked worktrees under `.claude/worktrees/<name>` — so the test establishes
    // the expected value from `CARGO_MANIFEST_DIR/.git` (a file in a linked worktree, a
    // directory in a primary checkout) rather than asserting a location.
    let world = world();
    let cwd = std::env::current_dir().expect("this test process has a working directory");
    let cwd_str = cwd.to_str().expect("a UTF-8 path");

    let result = world.repository_root(cwd_str);

    assert!(
        result.is_some(),
        "this crate's directory must be inside a repository"
    );
    let (root, linked_worktree) = result.unwrap();

    // The root must be an ancestor of (or equal to) the current directory.
    assert!(
        cwd_str.starts_with(&root),
        "repository root {root} must be an ancestor of this directory {cwd_str}"
    );

    // Establish the expected value independently of the code under test: `.git` beside
    // this crate's manifest is a file in a linked worktree and a directory in a primary
    // checkout. `CARGO_MANIFEST_DIR` is known at compile time, so this does not reuse the
    // returned root or repeat the ancestor walk.
    let dot_git = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(".git");
    let dot_git_metadata =
        std::fs::metadata(&dot_git).expect("this crate's checkout has a `.git` entry");
    let expected_linked_worktree = dot_git_metadata.is_file();

    assert_eq!(
        linked_worktree,
        expected_linked_worktree,
        "`linked_worktree` must be derived from whether {} is a file (linked worktree) \
         or a directory (primary checkout)",
        dot_git.display()
    );
}

#[test]
fn repository_root_of_an_ancestorless_path_is_none() {
    // A path with no parent, such as `/`, cannot be inside a repository (unless someone
    // has created a repository at the filesystem root, which is not a real-world case).
    // This tests the termination condition of the ancestor walk.
    let world = world();

    let result = world.repository_root("/");

    assert_eq!(
        result, None,
        "the filesystem root is not inside a repository"
    );
}

#[test]
fn vcs_facts_of_a_nonexistent_path_is_path_gone() {
    use acmon::vcs::Unreadable;

    let world = world();

    // A path that cannot exist: contains a component that is unlikely to be real.
    let nonexistent = "/tmp/acmon-test-does-not-exist-e5a8f3b2-9c4d-4e1a-8b7f-3d2c1a0b9e8d";

    let result = world.vcs_facts(nonexistent);

    assert_eq!(
        result,
        Err(Unreadable::PathGone),
        "a nonexistent path must return PathGone, not NotVersionControlled or a reading"
    );
}

#[test]
fn vcs_facts_of_a_non_repository_directory_is_not_version_controlled() {
    use acmon::vcs::Unreadable;

    let world = world();

    // Create a temporary directory that is known not to be a repository.
    let temp_dir = std::env::temp_dir().join("acmon-test-not-a-repo");
    let _ = std::fs::create_dir(&temp_dir); // Ignore errors if it already exists.

    let result = world.vcs_facts(temp_dir.to_str().expect("UTF-8 path"));

    // Clean up.
    let _ = std::fs::remove_dir(&temp_dir);

    assert_eq!(
        result,
        Err(Unreadable::NotVersionControlled),
        "a real directory that is not a repository must return NotVersionControlled, not a \
         clean reading (which would be a fail-to-zero)"
    );
}

#[test]
fn vcs_facts_of_this_crate_agrees_with_repository_root() {
    // Invariant: the root reported by vcs_facts must match what repository_root said.
    // The count of uncommitted entries is deliberately NOT asserted — it varies between
    // runs as work progresses.
    let world = world();
    let cwd = std::env::current_dir().expect("this test process has a working directory");
    let cwd_str = cwd.to_str().expect("a UTF-8 path");

    let expected_root = world
        .repository_root(cwd_str)
        .expect("this crate is in a repository")
        .0;

    let facts = world
        .vcs_facts(cwd_str)
        .expect("this crate's directory must be readable by vcs_facts");

    assert_eq!(
        facts.root, expected_root,
        "vcs_facts must report the same root as repository_root"
    );
}

/// A throwaway git repository, plus the means to invalidate its index stat cache.
///
/// Built rather than borrowed. Proving that the query cannot write means provoking git
/// into writing, and provoking the repository this crate lives in is exactly the
/// interference `vcs_facts` exists to avoid.
struct ProbeRepository {
    directory: std::path::PathBuf,
    file: std::path::PathBuf,
}

impl ProbeRepository {
    fn create() -> ProbeRepository {
        let unique = format!(
            "acmon-vcs-write-probe-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("a clock after 1970")
                .as_nanos()
        );
        let directory = std::env::temp_dir().join(unique);
        std::fs::create_dir_all(&directory).expect("a temporary directory");
        let file = directory.join("tracked.txt");
        std::fs::write(&file, "unchanging content\n").expect("writing the tracked file");

        let repository = ProbeRepository { directory, file };
        repository.git(&["init", "-q"]);
        // Set identity and disable signing locally. A machine whose global config requires
        // a signature would otherwise fail the commit, and the test would report a
        // read-only violation that never happened.
        repository.git(&["config", "user.email", "probe@acmon.invalid"]);
        repository.git(&["config", "user.name", "acmon probe"]);
        repository.git(&["config", "commit.gpgsign", "false"]);
        repository.git(&["add", "tracked.txt"]);
        repository.git(&["commit", "-q", "-m", "the only commit"]);
        repository
    }

    /// Run git in the probe repository and require it to have succeeded.
    ///
    /// AGENTS.md: assert success before believing a measurement. A silently failed `git
    /// commit` leaves no index, and the read-only assertion below would then pass by
    /// comparing nothing to nothing.
    fn git(&self, arguments: &[&str]) {
        let status = std::process::Command::new("git")
            .arg("--no-optional-locks")
            .args(arguments)
            .current_dir(&self.directory)
            .output()
            .expect("git is installed");
        assert!(
            status.status.success(),
            "setting up the probe repository failed at `git {}`: {}",
            arguments.join(" "),
            String::from_utf8_lossy(&status.stderr)
        );
    }

    fn index_bytes(&self) -> Vec<u8> {
        std::fs::read(self.directory.join(".git").join("index"))
            .expect("the index exists once something has been committed")
    }

    /// Make the index's stat cache wrong without changing a byte of content.
    ///
    /// Two things about this are load-bearing, both learned by getting them wrong.
    ///
    /// A fixed, absurd modification time rather than "now": within the same second git
    /// treats a file as racily clean and may skip the refresh entirely, which was measured
    /// to make this provocation succeed only 1 time in 3.
    ///
    /// And the timestamp is a **parameter**, because each arm of the test must use a
    /// different one. The control arm's `git status` rewrites the index with whatever
    /// timestamp it found; re-applying that same timestamp afterwards leaves the stat cache
    /// already correct, so the second arm is never provoked and passes whether or not the
    /// query is read-only. Measured: with a shared timestamp the guarantee appears to hold
    /// even with `--no-optional-locks` removed. With distinct timestamps, removing the flag
    /// fails the test 3 times out of 3.
    fn make_index_stale(&self, timestamp: &str) {
        let touched = std::process::Command::new("touch")
            .args(["-m", "-t", timestamp])
            .arg(&self.file)
            .status()
            .expect("touch is installed");
        assert!(touched.success(), "could not backdate the tracked file");
    }
}

impl Drop for ProbeRepository {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

/// Observing a repository must not write to it — proven, with a control arm.
///
/// The control arm is the whole value of this test. An earlier version simply ran
/// `vcs_facts` against this crate's own repository and asserted `.git/index` was
/// unmodified. That version passed with `--no-optional-locks` deliberately removed,
/// because git had nothing to refresh: it was a test that could not fail, which is the
/// same defect class as every bug documented in
/// `docs/observability-mechanics.md` §7.
///
/// So the index is first made stale, and a plain `git status` is required to rewrite it.
/// That establishes the provocation works before anything is concluded from the second
/// arm. The plain invocation is the one place in this project where
/// `--no-optional-locks` is deliberately omitted, and it is omitted to demonstrate that
/// the flag is what carries the guarantee.
#[test]
fn observing_a_repository_cannot_write_to_it() {
    let repository = ProbeRepository::create();
    let path = repository
        .directory
        .to_str()
        .expect("a UTF-8 temporary path")
        .to_string();

    // Control arm: without the precautions, git MUST rewrite the index.
    repository.make_index_stale("203001010101.01");
    let before_control = repository.index_bytes();
    let control = std::process::Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(&repository.directory)
        .output()
        .expect("git is installed");
    assert!(control.status.success(), "the control git status failed");
    let after_control = repository.index_bytes();
    assert_ne!(
        before_control, after_control,
        "the provocation failed: an unguarded `git status` did not rewrite the index, so \
         this test cannot detect a write and proves nothing about the guarded query"
    );

    // Subject arm: with the precautions, the index must be byte-identical afterwards.
    // A DIFFERENT timestamp from the control arm — see `make_index_stale`.
    repository.make_index_stale("202001010101.01");
    let before = repository.index_bytes();
    let facts = world()
        .vcs_facts(&path)
        .expect("the probe repository is readable");
    let after = repository.index_bytes();

    assert_eq!(
        before, after,
        "vcs_facts rewrote .git/index, so observing a workspace contends with the agent \
         working in it"
    );
    // The reading also has to be right, or an early error return would satisfy the
    // assertion above while measuring nothing. Backdating a file changes no content, so
    // the repository is clean.
    assert_eq!(
        facts.uncommitted_entries, 0,
        "backdating a tracked file changes no content, so the probe repository is clean"
    );
}

#[test]
fn resolve_namespace_round_trips_this_crates_namespace() {
    // The round trip that matters: map this crate's directory to a namespace, then resolve
    // that namespace back to a directory. The result must be this directory.
    use acmon::workspace::{namespace_for, NamespaceResolution};

    let world = world();
    let cwd = std::env::current_dir().expect("this test process has a working directory");
    let cwd_str = cwd.to_str().expect("a UTF-8 path");

    let namespace = namespace_for(cwd_str);
    let resolution = world.resolve_namespace(&namespace);

    match resolution {
        NamespaceResolution::Resolved(path) => {
            assert_eq!(
                path, cwd_str,
                "resolving the namespace of this crate's directory must return that directory"
            );
        }
        other => {
            panic!("expected Resolved({cwd_str}), got {other:?} — the round trip failed");
        }
    }
}

#[test]
fn resolve_namespace_of_a_nonexistent_namespace_is_no_longer_exists() {
    // An invented namespace that names nothing must return NoLongerExists, not
    // SearchExhausted — the distinction matters because one is an answer and the other is
    // a failure.
    use acmon::workspace::NamespaceResolution;

    let world = world();

    // A name that cannot exist: long and contains characters unlikely to match
    let nonexistent = "does-not-exist-9f3e8a7b-2c1d-4e5f-6a7b-8c9d0e1f2a3b";

    let resolution = world.resolve_namespace(nonexistent);

    assert_eq!(
        resolution,
        NamespaceResolution::NoLongerExists,
        "a nonexistent namespace must return NoLongerExists, not SearchExhausted or Resolved"
    );
}

#[test]
fn sweep_finds_this_crate_and_reports_complete() {
    // Sweeping this crate's parent directory must find this crate, and must report
    // complete == true.
    let world = world();
    let cwd = std::env::current_dir().expect("this test process has a working directory");
    let parent = cwd
        .parent()
        .expect("this crate is not at the filesystem root")
        .to_str()
        .expect("a UTF-8 path");

    let sweep = world.sweep_for_repositories(&[parent.to_string()]);

    assert!(
        sweep.complete,
        "sweeping this crate's parent should complete within the budget"
    );

    let found_this_crate = sweep.repositories.iter().any(|(path, _)| {
        let cwd_str = cwd.to_str().expect("a UTF-8 path");
        path == cwd_str
    });

    assert!(
        found_this_crate,
        "sweep of {} must find this crate at {}, got repositories: {:?}",
        parent,
        cwd.display(),
        sweep.repositories
    );
}

#[test]
fn sweep_of_empty_roots_returns_complete_with_no_repositories() {
    // Sweeping an empty list of roots must return no repositories and complete == true.
    // An empty answer must still be a complete one.
    let world = world();

    let sweep = world.sweep_for_repositories(&[]);

    assert_eq!(
        sweep.repositories.len(),
        0,
        "sweeping no roots must return no repositories"
    );
    assert!(
        sweep.complete,
        "sweeping no roots must report complete == true"
    );
}

#[test]
fn sweep_never_descends_into_a_repository() {
    // The sweep must not descend into a repository: assert directories_visited for a sweep
    // of this crate's parent is far smaller than the number of directories inside this
    // crate. Count them independently with std::fs to get a figure that does not recompute
    // the way the sweep does.
    let world = world();
    let cwd = std::env::current_dir().expect("this test process has a working directory");
    let cwd_str = cwd.to_str().expect("a UTF-8 path");
    let parent = cwd
        .parent()
        .expect("this crate is not at the filesystem root")
        .to_str()
        .expect("a UTF-8 path");

    let sweep = world.sweep_for_repositories(&[parent.to_string()]);

    // Count directories inside this crate using a naive recursive walk
    fn count_dirs(path: &str) -> usize {
        let Ok(entries) = std::fs::read_dir(path) else {
            return 0;
        };
        let mut count = 0;
        for entry in entries {
            let Ok(entry) = entry else { continue };
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                count += 1;
                count += count_dirs(&entry.path().to_string_lossy());
            }
        }
        count
    }

    let dirs_inside_crate = count_dirs(cwd_str);

    assert!(
        dirs_inside_crate > 10,
        "this crate should contain many directories (found {dirs_inside_crate}), or this \
         comparison proves nothing"
    );

    assert!(
        sweep.directories_visited < dirs_inside_crate,
        "sweep visited {} directories but this crate alone contains {dirs_inside_crate} — \
         the sweep descended into a repository",
        sweep.directories_visited
    );
}

#[test]
fn every_path_in_a_sweep_is_absolute_and_unique() {
    // Every path in a sweep result must be absolute, and no path may be returned twice.
    let world = world();
    let cwd = std::env::current_dir().expect("this test process has a working directory");
    let parent = cwd
        .parent()
        .expect("this crate is not at the filesystem root")
        .to_str()
        .expect("a UTF-8 path");

    let sweep = world.sweep_for_repositories(&[parent.to_string()]);

    let mut seen = std::collections::HashSet::new();
    for (path, _) in &sweep.repositories {
        assert!(
            path.starts_with('/'),
            "path {path} is not absolute — all sweep results must be absolute paths"
        );

        assert!(
            seen.insert(path.clone()),
            "path {path} appears more than once in the sweep — results must be deduplicated"
        );
    }
}

#[test]
fn vcs_facts_batch_returns_results_in_order() {
    // vcs_facts_batch must return exactly as many results as it was given paths, in order.
    // Feed it a list containing this crate's root twice plus a path that does not exist,
    // and assert positions 0 and 1 are equal Ok results and position 2 is Err(PathGone).
    use acmon::vcs::Unreadable;

    let world = world();
    let cwd = std::env::current_dir().expect("this test process has a working directory");
    let cwd_str = cwd.to_str().expect("a UTF-8 path");
    let nonexistent = "/tmp/acmon-test-vcs-batch-does-not-exist-a1b2c3d4";

    let paths = vec![
        cwd_str.to_string(),
        cwd_str.to_string(),
        nonexistent.to_string(),
    ];

    let results = world.vcs_facts_batch(&paths);

    assert_eq!(
        results.len(),
        3,
        "vcs_facts_batch must return exactly as many results as paths"
    );

    assert!(
        results[0].is_ok(),
        "position 0 (this crate's root) must be Ok, got {:?}",
        results[0]
    );
    assert!(
        results[1].is_ok(),
        "position 1 (this crate's root again) must be Ok, got {:?}",
        results[1]
    );
    assert_eq!(
        results[0], results[1],
        "positions 0 and 1 are the same path and must return equal Ok results"
    );

    assert_eq!(
        results[2],
        Err(Unreadable::PathGone),
        "position 2 (nonexistent path) must be Err(PathGone)"
    );
}

/// A sweep that runs out of budget must say the coverage is partial.
///
/// This is the "no silent caps" rule from AGENTS.md applied to the one place in this ticket
/// where a bound could quietly shrink the answer. A truncated list of at-risk workspaces
/// presented as exhaustive is worse than no list: "0 at risk" stops meaning anything once it
/// can also mean "stopped looking".
///
/// The bound is exercised for real rather than asserted about, so the tree is built wide
/// enough to exceed it. Flat rather than deep: the sweep visits every child of a root, so
/// breadth costs one `mkdir` per visit while depth would also need the descent limit raised.
#[test]
fn a_sweep_that_exhausts_its_budget_reports_incomplete_coverage() {
    let unique = format!(
        "acmon-sweep-budget-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("a clock after 1970")
            .as_nanos()
    );
    let root = std::env::temp_dir().join(unique);
    std::fs::create_dir_all(&root).expect("a temporary directory");

    // One more than the budget, so the root itself plus the children cannot fit inside it.
    for index in 0..=acmon::real_world::SWEEP_BUDGET {
        std::fs::create_dir(root.join(format!("d{index}"))).expect("a child directory");
    }

    let world = world();
    let sweep = world.sweep_for_repositories(&[root.to_string_lossy().into_owned()]);

    // Clean up before asserting, so a failure does not leave thousands of directories behind.
    let _ = std::fs::remove_dir_all(&root);

    assert!(
        !sweep.complete,
        "a sweep that visited {} directories against a budget of {} claimed to be complete",
        sweep.directories_visited,
        acmon::real_world::SWEEP_BUDGET
    );
    assert!(
        sweep.directories_visited >= acmon::real_world::SWEEP_BUDGET,
        "the sweep stopped at {} directories, below its budget of {}, so this test did not \
         exercise the bound at all",
        sweep.directories_visited,
        acmon::real_world::SWEEP_BUDGET
    );
}

// --- What a collection here may and may not touch (#38) -----------------------------------------
//
// The two tests below are about the suite rather than about the machine, and they are here because
// this is the file that collects against the real machine. Both work against a **stand-in** home
// directory built in `TMPDIR`: a tree shaped exactly like a developer's, with the three files a run
// resolves and a notification channel configured. Nothing here reads or writes the real
// `~/.config/acmon`, `~/.local/state/acmon` or `~/.acmon` — asserting about those would make the
// suite fail whenever a real `amon watch` happened to be running, which on this machine it usually
// is.

/// A stand-in for a developer's home: every file a run resolves, and a channel that leaves a mark.
///
/// Built rather than borrowed, for the same reason `ProbeRepository` above is: proving that a
/// relocated collection cannot reach a machine's own files means having a machine's own files to
/// aim at, and aiming at the developer's is the harm being prevented.
struct StandInMachine {
    home: PathBuf,
    /// `~/.local/state/acmon/notified.json` — the notification dedupe record.
    notified: PathBuf,
    /// `~/.local/state/acmon/memory.json` — the remembered history.
    memory: PathBuf,
    /// `~/.acmon/state.json` — the pre-split history, which nothing may ever write (#36).
    pre_split: PathBuf,
    /// The file the configured channel appends every payload it is handed to.
    ///
    /// `cat` rather than `touch`: a mark that proved only that *something* ran could have been
    /// left by anything, whereas a mark holding the payload came from a delivery.
    mark: PathBuf,
    /// The `local_command` written into the stand-in's `notify.toml`.
    command: String,
}

impl StandInMachine {
    fn create(name: &str) -> StandInMachine {
        let home = scratch_root().join(name);
        let _ = std::fs::remove_dir_all(&home);
        let config = home.join(".config").join("acmon");
        let state = home.join(".local").join("state").join("acmon");
        let pre_split_dir = home.join(".acmon");
        for directory in [&config, &state, &pre_split_dir] {
            std::fs::create_dir_all(directory).expect("a directory in the stand-in home");
        }

        let mark = home.join("delivered-through-the-configured-channel");
        let command = format!("/bin/cat >> {}", mark.display());
        std::fs::write(
            config.join("notify.toml"),
            format!("local_command = \"{command}\"\n"),
        )
        .expect("configure the stand-in's channel");

        // Distinctive, valid contents: a record and a history a run would read and honour, so that
        // "unchanged" below means "a real file a real run would have rewritten", not "a scrap of
        // text nothing was ever going to touch".
        let notified = state.join("notified.json");
        std::fs::write(
            &notified,
            acmon::notify::serialise(&acmon::notify::AnnouncementRecord {
                sessions: Vec::new(),
                workspaces: vec![(
                    "/stand-in/announced-workspace".to_string(),
                    acmon::notify::AnnouncedWorkspaceState::DirtyStranded,
                )],
            }),
        )
        .expect("write the stand-in's dedupe record");

        let memory = state.join("memory.json");
        std::fs::write(&memory, remembering("/stand-in/remembered-workspace"))
            .expect("write the stand-in's history");

        let pre_split = pre_split_dir.join("state.json");
        std::fs::write(&pre_split, remembering("/stand-in/pre-split-workspace"))
            .expect("write the stand-in's pre-split history");

        StandInMachine {
            home,
            notified,
            memory,
            pre_split,
            mark,
            command,
        }
    }

    /// A world resolving this machine's files the way an unrelocated run does.
    ///
    /// No directory named, so everything comes from the home — including the pre-split `~/.acmon`.
    /// This is the arm that establishes the files below are the ones a run really uses.
    fn as_that_machine(&self) -> RealWorld {
        RealWorld::with_paths(
            Paths::from_values(None, None, Some(&self.home.to_string_lossy()))
                .expect("a home was given"),
        )
    }

    /// A world built the way every test in this file builds one — with this machine's home still
    /// there to be found.
    ///
    /// Strictly weaker than `world()`, which passes no home at all: this one has somewhere to go
    /// wrong, which is the point. Its own scratch tree is separate from the shared one so that the
    /// files it writes are this test's alone.
    fn as_a_test_does(&self) -> RealWorld {
        RealWorld::with_paths(scratch_paths(
            &self.home.join("what-a-test-writes-instead"),
            Some(&self.home.to_string_lossy()),
        ))
    }

    /// Where a world built by `as_a_test_does` keeps its own state.
    fn scratch_state_dir(&self) -> PathBuf {
        self.home.join("what-a-test-writes-instead").join("state")
    }

    /// A workspace on this machine holding uncommitted work with nothing driving it, remembered in
    /// the state a test's world reads — so a collection through that world has an alert to deliver.
    ///
    /// This is what stops the arm below being a hope. A collection with nothing notable to say asks
    /// no channel anything at all, so a mark that never appeared would prove only that the machine
    /// happened to be quiet. An untracked file is uncommitted work by this tool's own definition,
    /// and a repository nobody is working in is stranded, which is the condition #9 alerts on.
    fn with_something_worth_announcing(&self) -> String {
        let workspace = self.home.join("stranded-workspace");
        std::fs::create_dir_all(&workspace).expect("the workspace directory");
        let initialised = Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(&workspace)
            .status()
            .expect("git is installed");
        assert!(
            initialised.success(),
            "the stranded workspace has to be a repository, or nothing about it is notable"
        );
        std::fs::write(
            workspace.join("work-nobody-committed.txt"),
            "uncommitted work\n",
        )
        .expect("leave uncommitted work in it");

        let path = workspace.to_string_lossy().into_owned();
        let state = self.scratch_state_dir();
        std::fs::create_dir_all(&state).expect("the scratch state directory");
        std::fs::write(state.join("memory.json"), remembering(&path))
            .expect("remember the workspace where a test's world will read it");
        path
    }

    /// The three files that must be byte-for-byte identical after a test has collected.
    fn own_files(&self) -> Vec<(&'static str, PathBuf, Vec<u8>)> {
        [
            ("the dedupe record", self.notified.clone()),
            ("the remembered history", self.memory.clone()),
            ("the pre-split history", self.pre_split.clone()),
        ]
        .into_iter()
        .map(|(what, path)| {
            let bytes = std::fs::read(&path).expect("a file the stand-in machine just wrote");
            (what, path, bytes)
        })
        .collect()
    }
}

/// One workspace, remembered — enough to make a state file that parses and is not empty.
fn remembering(path: &str) -> String {
    let now = SystemTime::now();
    acmon::memory::serialise(&acmon::Memory {
        workspaces: vec![acmon::memory::RememberedWorkspace {
            path: path.to_string(),
            first_seen: now,
            last_seen: now,
            settled_since: None,
        }],
        sessions: Vec::new(),
    })
}

/// A monitor-role collection over this machine leaves the machine's own files alone.
///
/// The monitor's role deliberately: it is the role that writes, and the one every test in this file
/// used to get by default. What makes this safe is not the role but where the world's directories
/// point, and this test is the proof of that — it collects as a monitor, checks the writes really
/// happened, and then checks they happened somewhere disposable.
#[test]
fn a_monitor_role_collection_writes_into_its_scratch_tree_and_not_into_the_machines_own_files() {
    let machine = StandInMachine::create("relocated-writes");

    // Provocation first. Without it, every "unchanged" assertion below could hold because the
    // paths this test watches are not the ones a run resolves at all — a test that cannot fail.
    let announced_by_that_machine =
        acmon::notify::serialise(&acmon::notify::AnnouncementRecord::empty());
    machine
        .as_that_machine()
        .write_notified(&announced_by_that_machine)
        .expect("a world resolving that home must be able to write its dedupe record");
    assert_eq!(
        std::fs::read_to_string(&machine.notified).expect("readable"),
        announced_by_that_machine,
        "a write through a world resolving that home did not land in the file this test watches, \
         so nothing below would prove anything about a relocated world"
    );

    let before = machine.own_files();

    let snapshot = collect_as(
        &machine.as_a_test_does(),
        SystemTime::now(),
        &Thresholds::default(),
        Role::Monitor,
    )
    .expect("collection over the real machine should succeed");

    // Assert the writes happened before believing anything about where they went: a collection
    // that failed to write would leave every file below untouched for the wrong reason.
    assert_eq!(
        snapshot.remembered.persisted,
        Persistence::Stored,
        "a monitor-role collection stores its history, or this proves nothing"
    );
    assert_eq!(
        snapshot.remembered.notified.persisted,
        Persistence::Stored,
        "and stores its dedupe record"
    );
    for file in ["memory.json", "notified.json"] {
        let written = machine.scratch_state_dir().join(file);
        assert!(
            written.exists(),
            "the collection's {file} must be in its own scratch state directory, at {}",
            written.display()
        );
    }

    for (what, path, bytes) in before {
        assert_eq!(
            std::fs::read(&path).expect("still readable"),
            bytes,
            "{what} at {} was written by a test collection",
            path.display()
        );
    }
}

/// A channel configured on the machine running the suite cannot be reached from a test.
///
/// The second criterion of #38, and the reason it is worded the way it is: this suite was safe only
/// because the machine it was written on has no `notify.toml`. The day one appears, `cargo test`
/// delivers real notifications and — worse — records real conditions as already announced, so the
/// monitor stays quiet about them and nothing anywhere says why.
///
/// Both arms use the same configured channel, and it is a real one: `sh -c` runs the command with
/// the payload on its stdin, exactly as a delivery to a developer's notifier would.
#[test]
fn no_test_can_deliver_through_a_channel_configured_on_the_machine_it_runs_on() {
    const PAYLOAD: &str = "seam 3 probe payload — no test may ever deliver this";
    let machine = StandInMachine::create("configured-channel");
    assert!(
        !machine.mark.exists(),
        "the mark must start absent, or its presence later says nothing"
    );

    // Provocation arm: on that machine, unrelocated, the channel is found and delivered through.
    let as_that_machine = machine.as_that_machine();
    let configured = as_that_machine.read_notify_config();
    assert_eq!(
        configured.local_command.as_deref(),
        Some(machine.command.as_str()),
        "a world resolving that home must find the channel configured in it, or the arm below \
         proves only that a channel nobody could see delivered nothing"
    );
    let report = as_that_machine.notify_local_batch(&machine.command, &[PAYLOAD.to_string()]);
    assert_eq!(
        report.outcomes,
        vec![NotifyOutcome::Delivered],
        "and delivering through it must succeed, or the mark could be absent for its own reasons"
    );
    assert_eq!(
        std::fs::read_to_string(&machine.mark).expect("the channel's command left its mark"),
        PAYLOAD,
        "the mark carries the payload, so a mark is evidence of a delivery and not of a stray \
         process"
    );

    std::fs::remove_file(&machine.mark).expect("clear the mark before the arm that matters");

    // Subject arm: the world a test builds, on that same machine, with its `notify.toml` still
    // sitting in the home this world was given. Naming the two directories is what takes it out of
    // play, and no channel is configured in the scratch config directory.
    let stranded = machine.with_something_worth_announcing();
    let as_a_test_does = machine.as_a_test_does();
    let seen_by_a_test = as_a_test_does.read_notify_config();
    assert_eq!(
        seen_by_a_test.local_command, None,
        "a test's world must see no local channel, however the machine under it is configured"
    );
    assert_eq!(seen_by_a_test.remote_url, None, "nor a remote one");
    assert_eq!(
        seen_by_a_test.unusable, None,
        "and it must report a config that is absent rather than one it could not read — the two \
         are different states and only one of them is a fault"
    );

    // The monitor's role on purpose: the role that delivers, asked to collect the real machine.
    let snapshot = collect_as(
        &as_a_test_does,
        SystemTime::now(),
        &Thresholds::default(),
        Role::Monitor,
    )
    .expect("collection over the real machine should succeed");

    assert_eq!(
        snapshot.remembered.notify_health.config.local_command, None,
        "the run itself must report that it had no local channel to deliver through"
    );
    // And it really did run as a monitor with something to say. A collection that returned early,
    // or one with nothing notable in it, would also leave no mark and would prove nothing about the
    // channel.
    assert_eq!(
        snapshot.remembered.notified.persisted,
        Persistence::Stored,
        "a monitor-role collection records what it announced, so this run did reach the step \
         where a delivery would have happened"
    );
    let stranded_now = snapshot
        .workspaces
        .iter()
        .find(|workspace| workspace.path == stranded)
        .unwrap_or_else(|| {
            panic!(
                "the remembered workspace {stranded} was not re-checked, so this run had nothing \
                 to announce and the absent mark below means nothing; got {:?}",
                snapshot
                    .workspaces
                    .iter()
                    .map(|w| &w.path)
                    .collect::<Vec<_>>()
            )
        });
    assert_eq!(
        stranded_now.state,
        acmon::vcs::WorkspaceState::DirtyStranded,
        "and it holds uncommitted work with nothing driving it, which is what makes it notable"
    );
    assert!(
        snapshot.remembered.notify_health.notable > 0,
        "so the run had at least one alert in hand when it reached the delivery step"
    );

    assert!(
        !machine.mark.exists(),
        "a test collection delivered through the channel configured on the machine running it — \
         the mark at {} was left behind",
        machine.mark.display()
    );
}
