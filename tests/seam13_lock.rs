//! Seam 13 — one writer, enforced by a lock rather than by documentation.
//!
//! The failure this seam exists to prevent: two `amon watch` processes on one state
//! directory. Their writes interleave and their alerts duplicate, and neither symptom is
//! visible from outside — the file still parses, the notifications still arrive, just twice
//! and describing two different passes. A note in the README saying "only run one" is not a
//! mechanism, and the second writer is exactly the kind of fault this project exists to make
//! impossible rather than merely discouraged.
//!
//! So the refusal comes from the kernel, and it names the pid that holds the lock. "Already
//! running" on its own is unactionable: the reader cannot tell a LaunchAgent from a terminal
//! they forgot, and cannot look up either.
//!
//! The other half is the dead holder. `flock` is released with the file description, so a
//! monitor that was `SIGKILL`ed leaves a lock nobody holds — and a successor must take it.
//! But it must take it *loudly*: "I took over from a monitor that died" and "I started
//! normally" are different facts about this machine, and silently overwriting the record
//! turns the first into the second.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use acmon::lock::WatchLock;
use acmon::state::{Paths, StateStore, STATE_FILE};

/// A state directory that is this test's alone, removed on the way in.
///
/// No test may use the developer's own `~/.local/state/acmon`: it would either destroy real
/// history or have to be skipped, and a skipped test of a lock is how two writers ship.
fn scratch(name: &str) -> PathBuf {
    let directory =
        std::env::temp_dir().join(format!("acmon-seam13-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&directory);
    directory
}

/// A store reading the same files `amon watch` writes, through the same API.
fn store_for(state_dir: &Path) -> StateStore {
    let config = state_dir.join("config");
    let paths = Paths::from_values(
        Some(&config.to_string_lossy()),
        Some(&state_dir.to_string_lossy()),
        None,
    )
    .expect("both directories were given explicitly");
    StateStore::new(paths)
}

/// Spawn `amon` with its state directory relocated and, optionally, a lock-holding window.
///
/// The hold window is the only way to have two real processes contend: with the collection
/// loop still unbuilt (#27) an unheld `amon watch` acquires and releases in microseconds, and
/// a stub that pretends to hold a lock proves nothing about `flock`.
fn amon(state_dir: &Path, arguments: &[&str], hold_ms: Option<&str>) -> Child {
    let mut command = Command::new(env!("CARGO_BIN_EXE_amon"));
    command
        .args(arguments)
        .env(acmon::state::STATE_DIR_VARIABLE, state_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(hold) = hold_ms {
        command.env(acmon::watch::HOLD_VARIABLE, hold);
    }
    command.spawn().expect("amon is built and runnable")
}

/// Run `amon` to completion: (succeeded, stdout, stderr).
fn amon_run(state_dir: &Path, arguments: &[&str]) -> (bool, String, String) {
    let child = amon(state_dir, arguments, None);
    let output = child.wait_with_output().expect("amon terminates");
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// Wait until the running `amon watch` has published a state file, and return its writer pid.
///
/// Bounded, and it checks the child is still alive on every turn: a measurement believed from
/// a process that already exited is the mistake `AGENTS.md` records twice.
fn writer_pid_once_published(state_dir: &Path, child: &mut Child) -> u32 {
    let store = store_for(state_dir);

    for _ in 0..200 {
        if let Some(status) = child.try_wait().expect("child status is readable") {
            panic!("`amon watch` exited ({status}) before publishing {STATE_FILE}");
        }
        if let Some(state) = store
            .read_tiered_state(STATE_FILE)
            .expect("readable if present")
        {
            return state.writer_pid();
        }
        std::thread::sleep(Duration::from_millis(25));
    }

    panic!("`amon watch` never published {STATE_FILE} while holding the lock");
}

/// Take a lock that a previous holder in *this* process has finished with.
///
/// Bounded retries, because of a race that belongs to this test binary rather than to the
/// lock: another test thread spawning `amon` copies this process's file descriptors into the
/// child, and the copy keeps the lock alive until the child execs and `CLOEXEC` closes it.
/// Observed once, for microseconds. The bound is what keeps this honest — a lock that never
/// frees still fails the test, and it fails naming the holder.
fn acquire_once_free(state_dir: &Path) -> WatchLock {
    let mut last = None;
    for _ in 0..100 {
        match WatchLock::acquire(state_dir) {
            Ok(lock) => return lock,
            Err(refusal) => {
                last = Some(refusal);
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
    panic!(
        "the lock never became available after its holder finished with it: {}",
        last.expect("a hundred refusals leave a reason")
    );
}

/// Everything in the state directory, sorted, so "wrote nothing" is assertable.
fn entries(state_dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(state_dir)
        .expect("the state directory exists")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

// --- The lock itself ---

#[test]
fn the_lock_lives_in_the_state_directory_and_records_the_holders_pid() {
    // The pid has to be on disk, in decimal a human can read, or the refusal below has
    // nothing to name.
    let state_dir = scratch("acquire");

    let lock = WatchLock::acquire(&state_dir).expect("an empty directory has no holder");

    assert_eq!(
        lock.path().parent(),
        Some(state_dir.as_path()),
        "the lock belongs in the state directory it protects, not somewhere else"
    );
    assert_eq!(lock.holder_pid(), std::process::id());

    let recorded = std::fs::read_to_string(lock.path()).expect("the lock file is readable");
    assert!(
        recorded.contains(&std::process::id().to_string()),
        "the holder's pid must be readable from the file as decimal text; got {recorded:?}"
    );

    lock.release().expect("release");
    let _ = std::fs::remove_dir_all(&state_dir);
}

#[test]
fn a_second_acquisition_is_refused_and_names_the_pid_that_holds_the_lock() {
    // `flock` conflicts per open file description, so a second descriptor is refused even
    // inside one process. That is what makes this assertable without a second binary — and
    // the cross-process case is proven below with two real `amon watch` processes.
    let state_dir = scratch("refused");

    let held = WatchLock::acquire(&state_dir).expect("first acquisition");
    let refusal = WatchLock::acquire(&state_dir).expect_err("the lock is held");

    let reported = refusal.holder_pid();
    assert_eq!(
        reported,
        Some(std::process::id()),
        "a refusal must name the pid holding the lock; got {refusal}"
    );
    assert!(
        refusal
            .to_string()
            .contains(&std::process::id().to_string()),
        "the message a human reads must carry the pid too; got {refusal}"
    );

    held.release().expect("release");
    let _ = std::fs::remove_dir_all(&state_dir);
}

#[test]
fn a_refusal_that_cannot_read_the_holders_pid_says_so_instead_of_just_already_running() {
    // The lock is held but carries no pid — a holder mid-exit, or a file someone truncated.
    // "Already running" would be a dead end for the reader; the refusal has to say the pid
    // was unavailable and why, so they can tell this apart from a named holder.
    let state_dir = scratch("unnamed-holder");
    std::fs::create_dir_all(&state_dir).expect("create state directory");

    let path = state_dir.join("watch.lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(true)
        .open(&path)
        .expect("create the lock file");
    let taken = unsafe {
        libc::flock(
            std::os::unix::io::AsRawFd::as_raw_fd(&file),
            libc::LOCK_EX | libc::LOCK_NB,
        )
    };
    assert_eq!(
        taken, 0,
        "the test must actually hold the lock it claims to"
    );

    let refusal = WatchLock::acquire(&state_dir).expect_err("the lock is held");

    assert_eq!(
        refusal.holder_pid(),
        None,
        "an empty lock file names nobody, and must not be reported as if it did"
    );
    let message = refusal.to_string();
    assert!(
        message.contains("pid"),
        "the refusal must say the pid was unavailable; got {message}"
    );
    assert!(
        message.contains(&path.display().to_string()),
        "and it must name the lock file, which is the one thing the reader can inspect; \
         got {message}"
    );

    drop(file);
    let _ = std::fs::remove_dir_all(&state_dir);
}

#[test]
fn a_lock_left_by_a_process_that_died_is_taken_over_and_the_dead_pid_is_reported() {
    // A `SIGKILL`ed monitor leaves a lock nobody holds. Refusing to start would leave the
    // machine unmonitored on the strength of a file; overwriting in silence would erase the
    // fact that a monitor died. So: take it, and say whose it was.
    let state_dir = scratch("stale");
    std::fs::create_dir_all(&state_dir).expect("create state directory");

    // A genuinely dead pid, not an invented one: a real child, reaped, so the kernel agrees
    // it is gone.
    let mut corpse = Command::new("/usr/bin/true").spawn().expect("spawn");
    let dead_pid = corpse.id();
    let status = corpse.wait().expect("reap");
    assert!(status.success(), "the corpse must have actually run");

    std::fs::write(state_dir.join("watch.lock"), format!("{dead_pid}\n")).expect("write lock file");

    let lock = WatchLock::acquire(&state_dir).expect("a lock nobody holds is not held");

    let predecessor = lock
        .took_over_from()
        .expect("taking over a dead holder's lock is a reportable event, not a silent one");
    assert_eq!(predecessor.pid, dead_pid, "the dead pid must be named");
    assert!(
        !predecessor.still_running,
        "the pid was reaped before this assertion, so it must be reported as gone"
    );
    assert_eq!(lock.holder_pid(), std::process::id());

    lock.release().expect("release");
    let _ = std::fs::remove_dir_all(&state_dir);
}

#[test]
fn a_clean_release_leaves_no_predecessor_so_a_restart_is_not_blocked_by_itself() {
    // The restart case. If a clean exit left its pid behind, every normal restart would
    // report taking over from a dead monitor — a crash that never happened, which is the same
    // class of wrong answer as missing one that did.
    let state_dir = scratch("release");

    let first = WatchLock::acquire(&state_dir).expect("first acquisition");
    let first_pid = first.holder_pid();
    first.release().expect("clean release");

    let second = acquire_once_free(&state_dir);
    assert!(
        second.took_over_from().is_none(),
        "a clean release is not a death, so pid {first_pid} must not be reported as a \
         predecessor"
    );

    second.release().expect("release");
    let _ = std::fs::remove_dir_all(&state_dir);
}

#[test]
fn a_lock_dropped_without_release_reads_as_an_unclean_exit() {
    // The complement of the test above, and the reason a clean release clears the pid: a
    // holder that vanished without releasing is exactly what a crash looks like, and it must
    // be reported that way.
    let state_dir = scratch("dropped");

    let holder_pid = {
        let lock = WatchLock::acquire(&state_dir).expect("first acquisition");
        lock.holder_pid()
        // Dropped, never released.
    };

    let next = acquire_once_free(&state_dir);
    let predecessor = next
        .took_over_from()
        .expect("a holder that never released must leave a record of itself");
    assert_eq!(predecessor.pid, holder_pid);
    assert!(
        predecessor.still_running,
        "this process is the one that dropped it, and it is plainly alive"
    );

    next.release().expect("release");
    let _ = std::fs::remove_dir_all(&state_dir);
}

#[test]
fn the_lock_file_is_never_unlinked_so_no_successor_can_lock_a_deleted_inode() {
    // Removing the file on release is the classic form of this bug: a process already
    // blocked on the old inode locks something no longer reachable by name, a third process
    // creates a new file, and both hold "the" lock. So the file stays, and only its contents
    // change.
    let state_dir = scratch("no-unlink");

    let lock = WatchLock::acquire(&state_dir).expect("acquire");
    let path = lock.path().to_path_buf();
    lock.release().expect("release");

    assert!(
        path.exists(),
        "the lock file must survive its holder's release"
    );

    let _ = std::fs::remove_dir_all(&state_dir);
}

// --- Two real `amon watch` processes ---
//
// These spawn the built binary, which is the only honest way to test contention: the
// behaviour under test is one kernel refusing one process, and a stub standing in for either
// side would be testing this test.

#[test]
fn a_second_amon_watch_refuses_to_start_naming_the_pid_that_holds_the_lock() {
    // S10, end to end, plus the two criteria that hang off it: the lock is taken before the
    // first write, and `state.json` shows one writer pid throughout a run in which a second
    // instance was attempted.
    let state_dir = scratch("two-watches");

    let mut first = amon(&state_dir, &["watch"], Some("8000"));
    let first_pid = writer_pid_once_published(&state_dir, &mut first);
    assert_eq!(
        first_pid,
        first.id(),
        "the writer pid in {STATE_FILE} must be the process that holds the lock"
    );

    let before = entries(&state_dir);

    let second = amon(&state_dir, &["watch"], None);
    let second_pid = second.id();
    let output = second
        .wait_with_output()
        .expect("the second instance exits");
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    assert!(
        !output.status.success(),
        "a second `amon watch` must exit non-zero; it did not start monitoring"
    );
    assert!(
        stderr.contains(&first_pid.to_string()),
        "the refusal must name the pid holding the lock ({first_pid}); stderr was:\n{stderr}"
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).trim().is_empty(),
        "a refusal belongs on stderr"
    );

    // The lock precedes the first write: a refused instance leaves the directory exactly as
    // it found it, because it never got as far as writing anything.
    assert_eq!(
        entries(&state_dir),
        before,
        "the refused instance must not have written into the state directory"
    );

    let store = store_for(&state_dir);
    let still = store
        .read_tiered_state(STATE_FILE)
        .expect("readable")
        .expect("present");
    assert_eq!(
        still.writer_pid(),
        first_pid,
        "the state file must still name the lock holder as its writer"
    );
    let raw = std::fs::read_to_string(state_dir.join(STATE_FILE)).expect("read state file");
    assert!(
        !raw.contains(&second_pid.to_string()),
        "the refused instance's pid ({second_pid}) must appear nowhere in the state it never \
         wrote:\n{raw}"
    );

    // The holder ends its own run: it exits non-zero because the collection loop is not
    // built, having held the lock for its whole lifetime.
    let first_output = first.wait_with_output().expect("the holder exits");
    let first_stderr = String::from_utf8_lossy(&first_output.stderr).into_owned();
    assert!(
        !first_output.status.success(),
        "`amon watch` cannot monitor yet, so it must not report success; stderr was:\n\
         {first_stderr}"
    );

    // And a restart is not blocked by its own predecessor, which is what a clean release is
    // for.
    let (_, _, restart_stderr) = amon_run(&state_dir, &["watch"]);
    assert!(
        !restart_stderr.contains("holds the state lock"),
        "a restart after a clean exit must not be refused; stderr was:\n{restart_stderr}"
    );
    assert!(
        !restart_stderr.contains("took over"),
        "and it must not report taking over from its own cleanly exited predecessor \
         (pid {first_pid}); stderr was:\n{restart_stderr}"
    );

    let _ = std::fs::remove_dir_all(&state_dir);
}

#[test]
fn amon_watch_in_the_foreground_is_subject_to_the_same_lock() {
    // `--foreground` is for debugging, and two writers is two writers regardless of intent.
    // The holder here is this test process, holding the real lock through the same code path
    // `amon watch` uses.
    let state_dir = scratch("foreground");
    let held = WatchLock::acquire(&state_dir).expect("this test takes the lock first");

    let (success, stdout, stderr) = amon_run(&state_dir, &["watch", "--foreground"]);

    assert!(
        !success,
        "`amon watch --foreground` must be refused while the lock is held"
    );
    assert!(
        stderr.contains(&held.holder_pid().to_string()),
        "the refusal must name the pid holding the lock; stderr was:\n{stderr}"
    );
    assert!(stdout.trim().is_empty(), "a refusal belongs on stderr");

    assert!(
        !state_dir.join(STATE_FILE).exists(),
        "the refused foreground instance must not have written state either"
    );

    held.release().expect("release");
    let _ = std::fs::remove_dir_all(&state_dir);
}

#[test]
fn a_monitor_that_was_killed_is_taken_over_by_name_rather_than_in_silence() {
    // The stale case with a real corpse: `SIGKILL` the holder, reap it, and start again. The
    // kernel has released the lock, so the successor must start — and must say whose lock it
    // took, because a monitor died and that is worth knowing.
    let state_dir = scratch("killed");

    let mut victim = amon(&state_dir, &["watch"], Some("8000"));
    let victim_pid = writer_pid_once_published(&state_dir, &mut victim);

    // The test kills its own child. Nothing in the product signals anything — see the rule
    // in AGENTS.md — but a monitor dying mid-run is the case this criterion is about, and
    // simulating it with a hand-written file would not prove the kernel releases the lock.
    assert_eq!(
        unsafe { libc::kill(victim_pid as libc::pid_t, libc::SIGKILL) },
        0,
        "the SIGKILL must actually have been delivered"
    );
    let status = victim.wait().expect("reap the victim");
    assert!(
        !status.success(),
        "a killed process did not exit cleanly, and the exit status must show it"
    );

    let (success, _, stderr) = amon_run(&state_dir, &["watch"]);

    assert!(
        stderr.contains(&victim_pid.to_string()),
        "the successor must name the dead holder's pid ({victim_pid}); stderr was:\n{stderr}"
    );
    assert!(
        !stderr.contains("holds the state lock"),
        "a lock left by a dead process is not held, so the successor must not be refused; \
         stderr was:\n{stderr}"
    );
    assert!(
        !success,
        "the successor still cannot monitor — the loop is #27 — so it still exits non-zero"
    );

    let store = store_for(&state_dir);
    let state = store
        .read_tiered_state(STATE_FILE)
        .expect("readable")
        .expect("present");
    assert_ne!(
        state.writer_pid(),
        victim_pid,
        "the successor is the writer now, and the state file must say so"
    );

    let _ = std::fs::remove_dir_all(&state_dir);
}

// --- Nothing but the monitor writes state ---

#[test]
fn agtop_takes_no_lock_and_leaves_the_state_directory_exactly_as_it_found_it() {
    // F18 and F26 from the other side: the display is read-only. A second writer would undo
    // the lock, and a notification from a foreground UI is redundant with looking at it.
    //
    // Asserted as an empty directory rather than as a list of files that must be absent. When
    // this seam landed, the display still persisted the pre-split memory file and could still
    // deliver through `collect`, so the assertion had to name the two files it did cover — and
    // a named list is a list that a new state artefact is not on. That is not hypothetical: #29
    // put `notified.json` in this directory, `agtop` reached it through `collect`, and this test
    // stayed green while its own name said the display writes nothing. #10 made the display a
    // reader, so the guarantee is now the strong one: nothing at all.
    //
    // Run through `--once`, deliberately. A bare `agtop` takes the screen and refuses when
    // there is no terminal, which is every test harness — it would never reach a collection,
    // and this test would pass by never having done anything.
    let state_dir = scratch("agtop-readonly");
    std::fs::create_dir_all(&state_dir).expect("create state directory");

    let legacy = state_dir.join("legacy-memory.json");
    let absent_notify = state_dir.join("no-such-notify.toml");

    let output = Command::new(env!("CARGO_BIN_EXE_agtop"))
        .arg("--once")
        .env(acmon::state::STATE_DIR_VARIABLE, &state_dir)
        .env("ACMON_STATE", &legacy)
        .env("ACMON_NOTIFY_CONFIG", &absent_notify)
        .stdin(Stdio::null())
        .output()
        .expect("agtop is built and runnable");

    let left_behind: Vec<String> = std::fs::read_dir(&state_dir)
        .expect("the state directory is still there")
        .map(|entry| {
            entry
                .expect("a readable entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();

    // Whatever agtop decided, it decided it without becoming a writer of this directory.
    assert!(
        left_behind.is_empty(),
        "the display left {left_behind:?} in the monitor's state directory; it is read-only \
         (exit was {:?})",
        output.status
    );
    assert!(
        !legacy.exists(),
        "the display must not write the pre-split memory file either — a reader that writes \
         anywhere is a second writer"
    );

    // Everything else it left behind has to be on the list, by name. A new artefact appearing
    // here fails this test rather than quietly joining the display's write set.
    let not_yet_retired = [
        acmon::state::NOTIFIED_FILE, // #29's dedupe record, reached through `collect` — #10
        "legacy-memory.json",        // the pre-split memory file, redirected in above — #10
    ];
    let left_behind: Vec<String> = entries(&state_dir)
        .into_iter()
        .filter(|name| !not_yet_retired.contains(&name.as_str()))
        .collect();

    assert!(
        left_behind.is_empty(),
        "the display wrote {left_behind:?} into the monitor's state directory; either it \
         should not, or the list this test keeps has to say why not yet, naming the ticket \
         that retires it"
    );

    let _ = std::fs::remove_dir_all(&state_dir);
}
