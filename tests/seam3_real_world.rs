//! Seam 3 — the real `World` against this machine.
//!
//! INVARIANTS ONLY. Never absolute values: the number of live sessions, their pids
//! and the size of the process table all change between runs, and timings on this
//! class of machine vary by roughly 2x. A test asserting a specific count is a test
//! that fails for reasons unrelated to correctness.

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
            .all(|r| r.exe_path.as_deref() != Some("")),
        "an unreadable path must be absent, never an empty string"
    );
}
