//! Seam 3 — the real `World` against this machine.
//!
//! INVARIANTS ONLY. Never absolute values: the number of live sessions, their pids
//! and the size of the process table all change between runs, and timings on this
//! class of machine vary by roughly 2x. A test asserting a specific count is a test
//! that fails for reasons unrelated to correctness.

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use acmon::workspace::NamespaceUnmatched;
use acmon::world::{PathUnavailable, ResourceSource, ResourcesUnavailable, Unmeasured};
use acmon::{collect, RealWorld, World};

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

    let snapshot = collect(&world).expect("collection over the real machine should succeed");

    for session in &snapshot.sessions {
        assert_eq!(
            session.cli, "claude",
            "ticket #2 recognises only Claude, so nothing else may be reported"
        );
        assert!(session.pid > 0, "a pid is always positive");
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
    let snapshot = collect(&world).expect("collection over the real machine should succeed");

    for session in &snapshot.sessions {
        let Ok(workspace) = &session.workspace else {
            continue; // A workspace that could not be read is covered by its own test.
        };
        match &workspace.namespace {
            Ok(resolved) => assert!(
                recorded.contains(resolved),
                "session {} claims namespace {resolved}, which is not in the listing",
                session.pid
            ),
            Err(NamespaceUnmatched::NotRecorded { mapped }) => assert!(
                !recorded.iter().any(|n| n.eq_ignore_ascii_case(mapped)),
                "session {} in {} was reported as having no recorded namespace, but {mapped} \
                 is in the listing — the match is too strict",
                session.pid,
                workspace.path
            ),
            Err(NamespaceUnmatched::ListingFailed(why)) => {
                panic!("the listing succeeded above, so this cannot be: {why}")
            }
        }
    }
}
