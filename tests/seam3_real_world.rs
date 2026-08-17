//! Seam 3 — the real `World` against this machine.
//!
//! INVARIANTS ONLY. Never absolute values: the number of live sessions, their pids
//! and the size of the process table all change between runs, and timings on this
//! class of machine vary by roughly 2x. A test asserting a specific count is a test
//! that fails for reasons unrelated to correctness.

use acmon::world::ExePathUnavailable;
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
        if record.exe_path == Err(ExePathUnavailable::ProcessExited) {
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
