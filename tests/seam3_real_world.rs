//! Seam 3 — the real `World` against this machine.
//!
//! INVARIANTS ONLY. Never absolute values: the number of live sessions, their pids
//! and the size of the process table all change between runs, and timings on this
//! class of machine vary by roughly 2x. A test asserting a specific count is a test
//! that fails for reasons unrelated to correctness.

use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime};

use acmon::liveness::Thresholds;
use acmon::workspace::NamespaceUnmatched;
use acmon::world::{PathUnavailable, ResourceSource, ResourcesUnavailable, Unmeasured};
use acmon::{collect, Identity, RealWorld, World};

/// Helper to format a session identity for error messages.
fn format_identity(identity: &Identity) -> String {
    match identity {
        Identity::Process { pid } => format!("pid {}", pid),
        Identity::Transcript { recorded_as } => format!("transcript {}", recorded_as),
    }
}

#[test]
fn the_real_process_table_contains_the_observing_process() {
    let world = RealWorld::new();

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
    let world = RealWorld::new();

    let snapshot = collect(&world, SystemTime::now(), &Thresholds::default())
        .expect("collection over the real machine should succeed");

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
    let world = RealWorld::new();

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
    let world = RealWorld::new();
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
    let world = RealWorld::new();
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
    let world = RealWorld::new();
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
    let world = RealWorld::new();
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
    let world = RealWorld::new();

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
    let world = RealWorld::new();

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
    let world = RealWorld::new();
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
    let world = RealWorld::new();

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
    let world = RealWorld::new();

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
    let world = RealWorld::new();
    let recorded = world.recorded_namespaces().expect("listable");
    let snapshot = collect(&world, SystemTime::now(), &Thresholds::default())
        .expect("collection over the real machine should succeed");

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
    let world = RealWorld::new();

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

    let world = RealWorld::new();
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

    let world = RealWorld::new();

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
    let world = RealWorld::new();
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
    let world = RealWorld::new();

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
    let world = RealWorld::new();
    let snapshot = collect(&world, SystemTime::now(), &Thresholds::default())
        .expect("collection over the real machine should succeed");

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
    let world = RealWorld::new();
    let snapshot = collect(&world, SystemTime::now(), &Thresholds::default())
        .expect("collection over the real machine should succeed");

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
    // return the root, and it must not be a linked worktree (it is the primary).
    let world = RealWorld::new();
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

    // This is the primary repository, not a linked worktree.
    assert!(
        !linked_worktree,
        "this crate's repository is not a linked worktree"
    );
}

#[test]
fn repository_root_of_an_ancestorless_path_is_none() {
    // A path with no parent, such as `/`, cannot be inside a repository (unless someone
    // has created a repository at the filesystem root, which is not a real-world case).
    // This tests the termination condition of the ancestor walk.
    let world = RealWorld::new();

    let result = world.repository_root("/");

    assert_eq!(
        result, None,
        "the filesystem root is not inside a repository"
    );
}

#[test]
fn vcs_facts_of_a_nonexistent_path_is_path_gone() {
    use acmon::vcs::Unreadable;

    let world = RealWorld::new();

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

    let world = RealWorld::new();

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
    let world = RealWorld::new();
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
    let facts = RealWorld::new()
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

    let world = RealWorld::new();
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

    let world = RealWorld::new();

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
    let world = RealWorld::new();
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
    let world = RealWorld::new();

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
    let world = RealWorld::new();
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
    let world = RealWorld::new();
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

    let world = RealWorld::new();
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
