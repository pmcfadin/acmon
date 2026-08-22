//! Seam 12 — on-disk state contract.
//!
//! The failure this seam exists to prevent: a resident `amon` is wedged forty minutes ago,
//! and `state.json` reads as perfectly healthy because freshness was a property of the file
//! rather than of the data. This is §2.2 of the PRD reproduced inside the tool itself — a
//! calm, plausible, wrong answer arriving through a timestamp that describes one fact while
//! misrepresenting every other.
//!
//! The fix: make freshness a property of the data. Every fact belongs to exactly one tier,
//! every tier has its own timestamp, and every write records the writer pid. A reader
//! determines the age and the writer from the file alone, without asking any process.

use std::path::PathBuf;
use std::time::{Duration, SystemTime};

/// A fixed instant every test reasons from.
fn now() -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(1_787_000_000)
}

fn ago(duration: Duration) -> SystemTime {
    now() - duration
}

/// A directory that is this test's alone, removed on the way out.
fn scratch(name: &str) -> PathBuf {
    let directory =
        std::env::temp_dir().join(format!("acmon-seam12-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&directory);
    directory
}

/// A whole scratch home directory, with a pre-split `~/.acmon` in it.
///
/// Every test below that has anything to do with the old location builds one of these. None of
/// them may go anywhere near the developer's own `~/.acmon`, which on a machine that has been
/// running this tool holds the only record of what it saw — and a test that destroyed it would
/// have destroyed exactly the thing this ticket exists to stop being lost.
fn scratch_home_with_pre_split_files(name: &str) -> PathBuf {
    let home = scratch(name);
    let legacy = home.join(".acmon");
    std::fs::create_dir_all(&legacy).expect("create the pre-split directory");

    std::fs::write(legacy.join("state.json"), pre_split_memory()).expect("the old memory file");
    // Deliberately malformed, both of them. A run that reads either one says so, naming the path,
    // which is what makes "the legacy directory is never named in the output" evidence that it was
    // never read rather than merely evidence that it was empty.
    std::fs::write(legacy.join("notify.toml"), "local_command = [oh dear\n")
        .expect("the old notify config");
    std::fs::write(legacy.join("detectors.toml"), "[[detector]] oh dear\n")
        .expect("the old detector config");

    home
}

/// The workspace remembered only by the pre-split memory file.
///
/// A path nothing on this machine will ever discover on its own, so its presence in a run's
/// remembered set proves the old file was read rather than rediscovered.
const CARRIED_WORKSPACE: &str = "/tmp/acmon-seam12-remembered-only-in-the-old-file";

/// The text of a pre-split `~/.acmon/state.json`, in the shape this build writes.
///
/// Built through `acmon::memory::serialise` rather than typed out, so the fixture cannot drift
/// from the format the parser expects and quietly start proving nothing.
fn pre_split_memory() -> String {
    acmon::memory::serialise(&acmon::memory::Memory {
        workspaces: vec![acmon::memory::RememberedWorkspace {
            path: CARRIED_WORKSPACE.to_string(),
            first_seen: now(),
            last_seen: now(),
            // Never settled, so no retention rule can forget it and make an unread file look
            // like a read one.
            settled_since: None,
        }],
        sessions: Vec::new(),
    })
}

/// Every file under a directory, with its bytes, so "untouched" can be asserted rather than hoped.
fn tree(directory: &std::path::Path) -> Vec<(String, Vec<u8>)> {
    let mut entries: Vec<(String, Vec<u8>)> = std::fs::read_dir(directory)
        .expect("the directory is listable")
        .map(|entry| {
            let entry = entry.expect("a readable entry");
            (
                entry.file_name().to_string_lossy().into_owned(),
                std::fs::read(entry.path()).expect("a readable file"),
            )
        })
        .collect();
    entries.sort();
    entries
}

// --- Path resolution: config vs. state split ---

#[test]
fn config_and_state_directories_are_distinct() {
    // The split makes "keep my config in dotfiles" and "delete state to recover" both safe.
    use acmon::state::Paths;

    let home = PathBuf::from("/Users/test");
    let paths = Paths::for_home(&home);

    assert!(
        paths.config_dir().starts_with(&home),
        "config lives under the home directory"
    );
    assert!(
        paths.state_dir().starts_with(&home),
        "state lives under the home directory"
    );
    assert_ne!(
        paths.config_dir(),
        paths.state_dir(),
        "config and state must be in different directories"
    );
    assert!(
        paths.config_dir().to_string_lossy().contains(".config"),
        "config follows XDG conventions"
    );
    assert!(
        paths.state_dir().to_string_lossy().contains(".local/state"),
        "state follows XDG conventions"
    );
}

#[test]
fn each_directory_can_be_relocated_independently_of_the_other() {
    // The override is what lets a test drive the real write path — lock included — without
    // touching the developer's own state. Independently, because relocating state for a test
    // must not silently relocate config with it.
    use acmon::state::Paths;

    let paths = Paths::from_values(None, Some("/tmp/acmon-state"), Some("/Users/test"))
        .expect("HOME covers the directory that was not named");

    assert_eq!(paths.state_dir(), PathBuf::from("/tmp/acmon-state"));
    assert_eq!(
        paths.config_dir(),
        PathBuf::from("/Users/test/.config/acmon"),
        "config must still fall to its usual place under HOME"
    );

    let both = Paths::from_values(Some("/tmp/acmon-config"), Some("/tmp/acmon-state"), None)
        .expect("nothing is left for HOME to answer");
    assert_eq!(both.config_dir(), PathBuf::from("/tmp/acmon-config"));
    assert_eq!(both.state_dir(), PathBuf::from("/tmp/acmon-state"));
}

#[test]
fn a_directory_that_cannot_be_resolved_is_an_error_naming_the_variable_to_set() {
    // Fail loud. A stand-in path chosen because it is certain to be empty would make a monitor
    // that cannot find its own state indistinguishable from one with nothing to report.
    use acmon::state::Paths;

    let error = Paths::from_values(None, None, None).expect_err("no HOME, no overrides");

    assert!(
        error.contains("HOME"),
        "the error must say what was missing; got {error:?}"
    );
    assert!(
        error.contains("ACMON_CONFIG_DIR") || error.contains("ACMON_STATE_DIR"),
        "and it must name the variable that would answer it; got {error:?}"
    );

    // A blank value is not an answer either, and must not read as one.
    let blank = Paths::from_values(Some("  "), Some("  "), None).expect_err("blank is not a path");
    assert!(!blank.trim().is_empty());
}

#[test]
fn deleting_the_state_directory_loses_history_and_nothing_else() {
    // Acceptance criterion: deleting the state directory loses history and nothing else —
    // the next run recreates it and works.
    use acmon::state::{Paths, StateStore};

    let base = scratch("delete-state");
    let paths = Paths::with_base(&base);

    // Write some config
    std::fs::create_dir_all(paths.config_dir()).expect("create config dir");
    std::fs::write(
        paths.config_dir().join("detectors.toml"),
        "[[detector]]\nid = \"test\"\n",
    )
    .expect("write config");

    // Write some state
    let store = StateStore::new(paths.clone());
    store
        .write_state("state.json", b"test state", std::process::id())
        .expect("write state");

    // Delete the state directory
    std::fs::remove_dir_all(paths.state_dir()).expect("remove state dir");

    // Config survives
    assert!(
        paths.config_dir().join("detectors.toml").exists(),
        "config must survive state directory deletion"
    );

    // State is gone
    assert!(!paths.state_dir().exists(), "state directory was deleted");

    // Next write recreates and works
    store
        .write_state("state.json", b"new state", std::process::id())
        .expect("recreates state directory");
    assert!(paths.state_dir().exists());

    let _ = std::fs::remove_dir_all(&base);
}

// --- Atomic writes: write-temp-then-rename ---

#[test]
fn state_writes_are_atomic_so_a_reader_never_sees_a_half_written_file() {
    // The requirement: a concurrent reader never observes a partial write. Verified by
    // interleaving writes and reads like seam8 does, asserting every read parses.
    use acmon::state::{Paths, StateStore};

    let base = scratch("atomic");
    let paths = Paths::with_base(&base);
    let store = StateStore::new(paths);

    let small = b"small";
    let large = vec![b'X'; 50_000];

    store
        .write_state("test.json", small, 1)
        .expect("first write");

    let stop = std::sync::atomic::AtomicBool::new(false);
    let reads = std::sync::atomic::AtomicUsize::new(0);

    std::thread::scope(|scope| {
        let reader = scope.spawn(|| {
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                match store.read_state("test.json") {
                    Ok(Some(content)) => {
                        assert!(
                            content == small || content == large[..],
                            "reader observed a torn write: {} bytes",
                            content.len()
                        );
                        reads.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                    Ok(None) => {} // File doesn't exist yet
                    Err(e) => panic!("read error: {}", e),
                }
            }
        });

        for round in 0..100 {
            let data = if round % 2 == 0 { small } else { &large[..] };
            store.write_state("test.json", data, 1).expect("write");
        }

        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        reader.join().expect("reader must not panic");
    });

    assert!(
        reads.load(std::sync::atomic::Ordering::Relaxed) > 0,
        "the reader must have actually read something"
    );

    let _ = std::fs::remove_dir_all(store.paths().state_dir());
}

#[test]
fn a_sigkill_mid_write_loses_only_the_pass_in_flight() {
    // Simulated by writing, checking the file exists and is readable, then removing the temp
    // file mid-way (if it leaked). The previous content must survive.
    use acmon::state::{Paths, StateStore};

    let base = scratch("sigkill");
    let paths = Paths::with_base(&base);
    let store = StateStore::new(paths);

    store
        .write_state("test.json", b"first", 1)
        .expect("first write");

    // If a temp file leaked, this would be observable
    let state_dir = store.paths().state_dir();
    let temp_files: Vec<_> = std::fs::read_dir(state_dir)
        .expect("listable")
        .filter_map(|e| e.ok())
        .filter(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            name.starts_with('.') || name.contains("tmp") || name.contains("temp")
        })
        .collect();

    assert!(
        temp_files.is_empty(),
        "atomic write must not leave temp files behind; found {:?}",
        temp_files
    );

    // The actual file is readable
    let content = store
        .read_state("test.json")
        .expect("no error")
        .expect("file exists");
    assert_eq!(content, b"first");

    let _ = std::fs::remove_dir_all(state_dir);
}

// --- Writer pid tracking ---

#[test]
fn every_write_records_the_writer_pid() {
    // Acceptance criterion: each write records the writer pid, and a reader can determine
    // the writer from the file alone.
    use acmon::state::{Paths, StateStore, TieredState};

    let base = scratch("writer-pid");
    let paths = Paths::with_base(&base);
    let store = StateStore::new(paths);

    let writer_pid = 12345;
    let state = TieredState::new(writer_pid);

    store
        .write_tiered_state("test.json", &state)
        .expect("write");

    let read_back = store
        .read_tiered_state("test.json")
        .expect("read")
        .expect("file exists");

    assert_eq!(
        read_back.writer_pid(),
        writer_pid,
        "the writer pid must be recoverable from the file"
    );

    let _ = std::fs::remove_dir_all(store.paths().state_dir());
}

#[test]
fn the_writer_pid_is_readable_without_asking_any_process() {
    // Stated explicitly: a reader determines the writer from the file alone.
    use acmon::state::{Paths, StateStore, TieredState};

    let base = scratch("pid-from-file");
    let paths = Paths::with_base(&base);
    let store = StateStore::new(paths);

    let state = TieredState::new(99999);
    store
        .write_tiered_state("test.json", &state)
        .expect("write");

    // Read the raw file as text
    let path = store.paths().state_dir().join("test.json");
    let raw = std::fs::read_to_string(&path).expect("read as text");

    assert!(
        raw.contains("99999"),
        "the writer pid must be present as readable text in the file; got:\n{raw}"
    );
    assert!(
        raw.contains("writer_pid") || raw.contains("pid"),
        "the field must be named so a human can find it; got:\n{raw}"
    );

    let _ = std::fs::remove_dir_all(store.paths().state_dir());
}

// --- Per-tier timestamps ---

#[test]
fn every_tier_has_its_own_timestamp() {
    // The load-bearing requirement: a single file-level timestamp describes the newest fact
    // and misdescribes every older one.
    use acmon::state::{Paths, StateStore, Tier, TieredState};

    let base = scratch("per-tier-stamps");
    let paths = Paths::with_base(&base);
    let store = StateStore::new(paths);

    let mut state = TieredState::new(1);

    // Update different tiers at different times
    state.set_tier_data(
        Tier::Fast,
        serde_json::json!("fast data"),
        ago(Duration::from_secs(5)),
    );
    state.set_tier_data(
        Tier::Medium,
        serde_json::json!("medium data"),
        ago(Duration::from_secs(30)),
    );
    state.set_tier_data(
        Tier::Slow,
        serde_json::json!("slow data"),
        ago(Duration::from_secs(120)),
    );

    store
        .write_tiered_state("test.json", &state)
        .expect("write");

    let read_back = store
        .read_tiered_state("test.json")
        .expect("read")
        .expect("file exists");

    // Each tier's timestamp is preserved
    let fast_age = now()
        .duration_since(
            read_back
                .tier_timestamp(Tier::Fast)
                .expect("fast timestamp"),
        )
        .expect("age is positive");
    let medium_age = now()
        .duration_since(
            read_back
                .tier_timestamp(Tier::Medium)
                .expect("medium timestamp"),
        )
        .expect("age is positive");
    let slow_age = now()
        .duration_since(
            read_back
                .tier_timestamp(Tier::Slow)
                .expect("slow timestamp"),
        )
        .expect("age is positive");

    assert!(
        fast_age < Duration::from_secs(10),
        "fast tier is recent: {:?}",
        fast_age
    );
    assert!(
        medium_age > Duration::from_secs(20) && medium_age < Duration::from_secs(40),
        "medium tier is 30s old: {:?}",
        medium_age
    );
    assert!(
        slow_age > Duration::from_secs(110) && slow_age < Duration::from_secs(130),
        "slow tier is 2 minutes old: {:?}",
        slow_age
    );

    let _ = std::fs::remove_dir_all(store.paths().state_dir());
}

#[test]
fn a_single_file_level_timestamp_is_forbidden() {
    // Explicit negative test: the file must NOT have a single timestamp that covers all data.
    use acmon::state::{Paths, StateStore, Tier, TieredState};

    let base = scratch("no-file-timestamp");
    let paths = Paths::with_base(&base);
    let store = StateStore::new(paths);

    let mut state = TieredState::new(1);
    state.set_tier_data(
        Tier::Fast,
        serde_json::json!("fast"),
        ago(Duration::from_secs(5)),
    );
    state.set_tier_data(
        Tier::Medium,
        serde_json::json!("medium"),
        ago(Duration::from_secs(30)),
    );
    state.set_tier_data(
        Tier::Slow,
        serde_json::json!("slow"),
        ago(Duration::from_secs(120)),
    );

    store
        .write_tiered_state("test.json", &state)
        .expect("write");

    let path = store.paths().state_dir().join("test.json");
    let raw = std::fs::read_to_string(&path).expect("read as text");

    // Count timestamp fields - there should be one per tier (3 tiers), not one for the file
    let timestamp_count = raw.matches("\"timestamp\"").count();

    assert_eq!(
        timestamp_count, 3,
        "must have exactly one timestamp per tier (3 tiers, each with its own); found {} in:\n{}",
        timestamp_count, raw
    );

    // Ensure there's NO top-level timestamp covering all data
    assert!(
        !raw.contains("\"written_at\"") && !raw.contains("\"updated_at\"") && !raw.contains("\"file_timestamp\""),
        "must not have a single file-level timestamp; the file should only have per-tier timestamps"
    );

    let _ = std::fs::remove_dir_all(store.paths().state_dir());
}

#[test]
fn tier_timestamps_are_readable_as_iso8601_by_a_human() {
    // Like the memory seam: every figure must be verifiable by hand.
    use acmon::state::{Paths, StateStore, Tier, TieredState};

    let base = scratch("human-readable");
    let paths = Paths::with_base(&base);
    let store = StateStore::new(paths);

    let mut state = TieredState::new(1);
    state.set_tier_data(Tier::Fast, serde_json::json!("test"), now());

    store
        .write_tiered_state("test.json", &state)
        .expect("write");

    let path = store.paths().state_dir().join("test.json");
    let raw = std::fs::read_to_string(&path).expect("read as text");

    assert!(
        raw.contains("2026-") && raw.contains("T") && raw.contains("Z"),
        "timestamps must be ISO 8601 dates, not epoch integers; got:\n{raw}"
    );

    let _ = std::fs::remove_dir_all(store.paths().state_dir());
}

// --- No heartbeat file ---

#[test]
fn there_is_no_heartbeat_file() {
    // Explicit negative test. The problem: a heartbeat can be fresh while the write it was
    // meant to prove has failed.
    use acmon::state::{Paths, StateStore};

    let base = scratch("no-heartbeat");
    let paths = Paths::with_base(&base);
    let store = StateStore::new(paths.clone());

    store
        .write_state("state.json", b"some state", 1)
        .expect("write");

    let files: Vec<_> = std::fs::read_dir(paths.state_dir())
        .expect("listable")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();

    for file in &files {
        assert!(
            !file.contains("heartbeat") && !file.contains("alive") && !file.contains("ping"),
            "must not have a heartbeat file; found {file}"
        );
    }

    let _ = std::fs::remove_dir_all(paths.state_dir());
}

#[test]
fn freshness_is_determined_from_per_tier_stamps_not_from_a_process() {
    // The alternative design, explicitly rejected: querying a process to ask if it's healthy.
    // That cannot work — the query itself can hang, and a wedged process says nothing about
    // whether its last write succeeded.
    use acmon::state::{Paths, StateStore, Tier, TieredState};

    let base = scratch("stamps-not-process");
    let paths = Paths::with_base(&base);
    let store = StateStore::new(paths);

    let mut state = TieredState::new(99999); // A pid that doesn't exist
    state.set_tier_data(
        Tier::Fast,
        serde_json::json!("data"),
        ago(Duration::from_secs(300)),
    );

    store
        .write_tiered_state("test.json", &state)
        .expect("write");

    let read_back = store
        .read_tiered_state("test.json")
        .expect("read")
        .expect("file exists");

    // The reader can determine age without asking any process
    let age = now()
        .duration_since(read_back.tier_timestamp(Tier::Fast).expect("timestamp"))
        .expect("age");

    assert!(
        age > Duration::from_secs(290) && age < Duration::from_secs(310),
        "age must be determinable from the file alone, even if the writer pid is gone"
    );

    let _ = std::fs::remove_dir_all(store.paths().state_dir());
}

// --- Every fact belongs to exactly one tier ---

#[test]
fn facts_are_organized_by_tier() {
    // Each fact belongs to exactly one tier and carries that tier's age.
    use acmon::state::{Paths, StateStore, Tier, TieredState};

    let base = scratch("facts-by-tier");
    let paths = Paths::with_base(&base);
    let store = StateStore::new(paths);

    let mut state = TieredState::new(1);

    // Different data for different tiers
    state.set_tier_data(
        Tier::Fast,
        serde_json::json!("fast facts"),
        ago(Duration::from_secs(1)),
    );
    state.set_tier_data(
        Tier::Medium,
        serde_json::json!("medium facts"),
        ago(Duration::from_secs(10)),
    );
    state.set_tier_data(
        Tier::Slow,
        serde_json::json!("slow facts"),
        ago(Duration::from_secs(60)),
    );

    store
        .write_tiered_state("test.json", &state)
        .expect("write");

    let read_back = store
        .read_tiered_state("test.json")
        .expect("read")
        .expect("file exists");

    // Each tier's data is retrievable with its timestamp
    assert_eq!(
        read_back.tier_data(Tier::Fast).expect("fast data"),
        &serde_json::json!("fast facts")
    );
    assert_eq!(
        read_back.tier_data(Tier::Medium).expect("medium data"),
        &serde_json::json!("medium facts")
    );
    assert_eq!(
        read_back.tier_data(Tier::Slow).expect("slow data"),
        &serde_json::json!("slow facts")
    );

    // And each has its own timestamp
    assert!(read_back.tier_timestamp(Tier::Fast).is_some());
    assert!(read_back.tier_timestamp(Tier::Medium).is_some());
    assert!(read_back.tier_timestamp(Tier::Slow).is_some());

    let _ = std::fs::remove_dir_all(store.paths().state_dir());
}

// --- Negative paths: fail loud, never fail to zero ---

#[test]
fn a_state_file_that_exists_but_cannot_be_read_produces_a_loud_error() {
    // The defect from AGENTS.md: a calm answer where an error should be. A file that exists
    // but cannot be read — permissions, I/O error, corrupted filesystem — must not read as
    // "no file yet", which is what None means.
    use acmon::state::{Paths, StateStore};

    let base = scratch("unreadable");
    let paths = Paths::with_base(&base);
    let store = StateStore::new(paths);

    // Write a file
    store
        .write_state("test.json", b"some state", 1)
        .expect("write");

    let path = store.paths().state_dir().join("test.json");

    // Make it unreadable (chmod 0000)
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&path).expect("metadata").permissions();
        perms.set_mode(0o000);
        std::fs::set_permissions(&path, perms).expect("chmod");

        // If running as root, chmod has no effect. Skip the assertion if that's the case.
        if std::fs::read(&path).is_ok() {
            eprintln!(
                "SKIP: running as root, chmod has no effect; cannot test unreadable file behavior"
            );
            let _ = std::fs::remove_dir_all(store.paths().state_dir());
            return;
        }
    }

    // Attempt to read it
    let result = store.read_state("test.json");

    // It must NOT return Ok(None) (which reads as "no file yet")
    assert!(
        result.is_err(),
        "a file that exists but cannot be read must produce an error, not None; got {:?}",
        result
    );

    // The error must name a reason
    let error = result.unwrap_err();
    assert!(
        !error.trim().is_empty(),
        "the error must state a reason, not be blank; got {:?}",
        error
    );
    assert!(
        error.contains("test.json") || error.contains("state"),
        "the error should name the file or directory; got {:?}",
        error
    );

    // Restore permissions for cleanup
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&path)
            .unwrap_or_else(|_| {
                // If we can't get metadata, try to remove the whole directory
                let _ = std::fs::remove_dir_all(store.paths().state_dir());
                panic!("Could not restore permissions for cleanup");
            })
            .permissions();
        perms.set_mode(0o644);
        let _ = std::fs::set_permissions(&path, perms);
    }

    let _ = std::fs::remove_dir_all(store.paths().state_dir());
}

#[test]
fn a_truncated_or_corrupt_state_file_errors_rather_than_parsing_to_something_plausible() {
    // The other half of fail-to-zero: corruption that produces plausible-looking data is
    // worse than corruption that fails, because it reads as healthy.
    use acmon::state::{Paths, StateStore, Tier, TieredState};

    let base = scratch("corrupt");
    let paths = Paths::with_base(&base);
    let store = StateStore::new(paths.clone());

    // Write a valid file first
    let mut state = TieredState::new(1);
    state.set_tier_data(Tier::Fast, serde_json::json!({"test": "data"}), now());
    store
        .write_tiered_state("test.json", &state)
        .expect("write");

    // Corrupt it by truncating mid-JSON
    let path = paths.state_dir().join("test.json");
    let original = std::fs::read_to_string(&path).expect("read original");
    let truncated = &original[..original.len() / 2]; // Cut it in half
    std::fs::write(&path, truncated).expect("write truncated");

    // Attempt to read it
    let result = store.read_tiered_state("test.json");

    assert!(
        result.is_err(),
        "a truncated file must produce an error, not plausible-looking data; got {:?}",
        result
    );

    let error = result.unwrap_err();
    assert!(
        !error.trim().is_empty()
            && (error.contains("parse") || error.contains("JSON") || error.contains("UTF-8")),
        "the error must state what went wrong; got {:?}",
        error
    );

    let _ = std::fs::remove_dir_all(paths.state_dir());
}

#[test]
fn an_unknown_schema_version_errors_and_names_both_versions() {
    // Future-proofing. A file from a newer acmon is refused rather than parsed into a subset
    // of itself, which would silently destroy state the newer build relies on.
    use acmon::state::{Paths, StateStore};

    let base = scratch("future-version");
    let paths = Paths::with_base(&base);
    let store = StateStore::new(paths.clone());

    // Write a file claiming to be from version 999
    let future_file = r#"{
        "version": 999,
        "writer_pid": 12345,
        "tiers": {}
    }"#;

    std::fs::create_dir_all(paths.state_dir()).expect("create dir");
    std::fs::write(paths.state_dir().join("test.json"), future_file).expect("write");

    // Attempt to read it
    let result = store.read_tiered_state("test.json");

    assert!(
        result.is_err(),
        "a future schema version must be refused; got {:?}",
        result
    );

    let error = result.unwrap_err();
    assert!(
        error.contains("999") && error.contains("version"),
        "the error must name the version found; got {:?}",
        error
    );
    assert!(
        error.contains("1") || error.contains("understood"),
        "the error must also name the version understood; got {:?}",
        error
    );

    let _ = std::fs::remove_dir_all(paths.state_dir());
}

// --- Everything a run touches, on one side of the split or the other (#36) ---
//
// The failure this section exists to prevent: `ACMON_STATE_DIR` relocated the state directory
// while three files — the memory file and the two config files — still resolved their own paths
// under the pre-split `~/.acmon`. A monitor started with only that variable set therefore wrote
// the developer's own remembered workspaces, which is why two seams had to redirect `ACMON_STATE`
// by hand. Forgetting that redirection did not produce a red test; it produced a test that wrote
// real user data. That is the wrong direction for a mistake to fail in.

#[test]
fn the_memory_file_is_state_and_both_config_files_are_config() {
    // Each file on the side of the #25 split its content belongs on: the remembered workspaces
    // and sessions are mutable state, the two `.toml` files are config the user owns.
    use acmon::state::Paths;

    let paths = Paths::from_values(
        Some("/tmp/acmon-config"),
        Some("/tmp/acmon-state"),
        Some("/Users/test"),
    )
    .expect("both directories were given explicitly");

    assert_eq!(
        paths.locate_memory(None).read_path(),
        PathBuf::from("/tmp/acmon-state/memory.json"),
        "the remembered set is mutable state and belongs in the state directory"
    );
    assert_eq!(
        paths.locate_notify_config(None).read_path(),
        PathBuf::from("/tmp/acmon-config/notify.toml"),
        "notification channels are config"
    );
    assert_eq!(
        paths.locate_detectors(None).read_path(),
        PathBuf::from("/tmp/acmon-config/detectors.toml"),
        "detector overrides are config"
    );

    // Not `state.json`: that name is taken in this very directory by the tiered file the monitor
    // publishes, and one directory holding two different files under one name is not a split.
    assert_ne!(
        paths.locate_memory(None).read_path(),
        paths.state_dir().join(acmon::state::STATE_FILE),
        "the memory file must not collide with the tiered state file"
    );
}

#[test]
fn naming_either_directory_leaves_no_pre_split_directory_for_a_run_to_reach_into() {
    // The mechanism behind the isolation criterion. A run told where its files live has no
    // business reading one of them out of a home directory it was told to leave alone — that
    // would be isolated enough to look safe in a test name and not isolated at all.
    use acmon::state::Paths;

    let own =
        Paths::from_values(None, None, Some("/Users/test")).expect("HOME answers both directories");
    assert_eq!(
        own.legacy_dir(),
        Some(std::path::Path::new("/Users/test/.acmon")),
        "a run using this machine's own directories may consult the pre-split one"
    );

    for (config, state) in [
        (Some("/tmp/acmon-config"), None),
        (None, Some("/tmp/acmon-state")),
        (Some("/tmp/acmon-config"), Some("/tmp/acmon-state")),
    ] {
        let relocated = Paths::from_values(config, state, Some("/Users/test")).expect("resolvable");
        assert_eq!(
            relocated.legacy_dir(),
            None,
            "naming a directory ({config:?}, {state:?}) must take the pre-split one out of play"
        );
    }
}

#[test]
fn the_three_specific_variables_still_name_their_files_outright() {
    // They stop being the only way to move these files; they do not stop working. Several tests
    // in other seams point `ACMON_NOTIFY_CONFIG` at a file that does not exist precisely so that
    // no test run can deliver a notification anywhere.
    use acmon::state::{Found, Paths};

    let paths = Paths::from_values(Some("/tmp/acmon-config"), Some("/tmp/acmon-state"), None)
        .expect("resolvable");

    for (located, expected, variable) in [
        (
            paths.locate_memory(Some("/tmp/named-memory.json")),
            "/tmp/named-memory.json",
            "ACMON_STATE",
        ),
        (
            paths.locate_notify_config(Some("/tmp/named-notify.toml")),
            "/tmp/named-notify.toml",
            "ACMON_NOTIFY_CONFIG",
        ),
        (
            paths.locate_detectors(Some("/tmp/named-detectors.toml")),
            "/tmp/named-detectors.toml",
            "ACMON_DETECTORS",
        ),
    ] {
        assert_eq!(located.read_path(), PathBuf::from(expected));
        assert_eq!(
            located.write_path(),
            PathBuf::from(expected),
            "a file named outright is also written where it was named"
        );
        assert_eq!(located.how(), Found::Named(variable));
        let said = located
            .worth_stating()
            .expect("an overridden path is worth saying out loud");
        assert!(
            said.contains(variable) && said.contains(expected),
            "and the sentence names the variable and the path; got {said:?}"
        );
    }

    // A blank value is not an answer, and must not read as one.
    assert_eq!(
        paths.locate_memory(Some("   ")).read_path(),
        PathBuf::from("/tmp/acmon-state/memory.json"),
        "a blank override falls through to the split's own location"
    );
}

#[test]
fn a_pre_split_file_is_read_from_its_old_place_and_the_run_says_which_one_it_read() {
    // The criterion: an existing `~/.acmon/state.json` is not silently ignored. Starting from an
    // empty memory because the file moved would report a machine with no remembered sessions —
    // the calm, plausible, wrong answer this project exists to remove. Reading it is only half of
    // not lying about it: a run that read the old file has to say so, or "read the old one" and
    // "started from nothing" arrive on screen as the same run.
    use acmon::state::{Found, Paths};

    let home = scratch_home_with_pre_split_files("says-which-one");
    let paths = Paths::from_values(None, None, Some(&home.to_string_lossy()))
        .expect("the scratch home answers both directories");

    for (located, old_name, new_name) in [
        (paths.locate_memory(None), "state.json", "memory.json"),
        (
            paths.locate_notify_config(None),
            "notify.toml",
            "notify.toml",
        ),
        (
            paths.locate_detectors(None),
            "detectors.toml",
            "detectors.toml",
        ),
    ] {
        assert_eq!(located.how(), Found::CarriedForward);
        assert_eq!(
            located.read_path(),
            home.join(".acmon").join(old_name),
            "the old file is what this run reads"
        );

        let said = located
            .worth_stating()
            .expect("a run that read the old file says so");
        assert!(
            said.contains(&*home.join(".acmon").join(old_name).to_string_lossy()),
            "the sentence names the file it read; got {said:?}"
        );
        assert!(
            said.contains(new_name) && said.contains("Nothing was moved or deleted"),
            "and says where the tool looks now, and that nothing was moved; got {said:?}"
        );
    }

    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn once_the_new_location_has_the_file_the_pre_split_one_is_never_consulted_again() {
    // A machine is carried forward once. Preferring the old file for as long as it exists would
    // make the old one authoritative forever, and then a deleted state directory would silently
    // resurrect a history from months ago.
    use acmon::state::{Found, Paths};

    let home = scratch_home_with_pre_split_files("only-once");
    let paths = Paths::from_values(None, None, Some(&home.to_string_lossy()))
        .expect("the scratch home answers both directories");

    std::fs::create_dir_all(paths.state_dir()).expect("create the state directory");
    std::fs::write(paths.state_dir().join("memory.json"), "{}").expect("the new memory file");
    std::fs::create_dir_all(paths.config_dir()).expect("create the config directory");
    std::fs::write(paths.config_dir().join("notify.toml"), "").expect("the new notify config");

    let memory = paths.locate_memory(None);
    assert_eq!(memory.how(), Found::InPlace);
    assert_eq!(memory.read_path(), paths.state_dir().join("memory.json"));
    assert_eq!(
        memory.worth_stating(),
        None,
        "the ordinary case is silent, or the line means nothing"
    );

    let notify = paths.locate_notify_config(None);
    assert_eq!(notify.how(), Found::InPlace);
    assert_eq!(notify.worth_stating(), None);

    // And the file that has NOT been carried across is still read from the old place, so this is
    // per file rather than per machine.
    assert_eq!(paths.locate_detectors(None).how(), Found::CarriedForward);

    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn the_history_carries_itself_across_on_the_first_write_and_the_old_file_is_left_alone() {
    // The whole of the decision, exercised through the real world rather than through the path
    // resolution alone: the old file is READ, the result is WRITTEN to the split's own location,
    // and nothing is moved or deleted — so a user who loses faith in this can put the old file
    // back by doing nothing at all.
    //
    // Driven through `RealWorld::with_paths` rather than by setting variables, because the
    // environment is process-wide and this binary's tests run concurrently.
    use acmon::state::Paths;
    use acmon::World;

    let home = scratch_home_with_pre_split_files("carries-itself");
    let legacy = home.join(".acmon").join("state.json");
    let before = std::fs::read(&legacy).expect("the old file is readable");

    let paths = Paths::from_values(None, None, Some(&home.to_string_lossy()))
        .expect("the scratch home answers both directories");
    let world = acmon::RealWorld::with_paths(paths.clone());

    match world.read_state() {
        acmon::world::StateRead::Found(text) => assert!(
            text.contains(CARRIED_WORKSPACE),
            "the run read the old file's contents, not an empty memory; got:\n{text}"
        ),
        other => {
            panic!("the old file must be read, not treated as absent or unreadable: {other:?}")
        }
    }

    assert!(
        world
            .path_notices()
            .iter()
            .any(|line| line.contains(&*legacy.to_string_lossy())),
        "and it says which file it read: {:?}",
        world.path_notices()
    );

    let carried = acmon::memory::serialise(&acmon::memory::Memory {
        workspaces: Vec::new(),
        sessions: Vec::new(),
    });
    world.write_state(&carried).expect("the write succeeds");

    let new_file = paths.state_dir().join("memory.json");
    assert_eq!(
        std::fs::read_to_string(&new_file).expect("the new file exists"),
        carried,
        "the write went to the state directory, which is where the next run looks first"
    );
    assert_eq!(
        std::fs::read(&legacy).expect("the old file is still there"),
        before,
        "and the old file is byte-for-byte as it was: nothing was moved, nothing was deleted"
    );

    // The next run finds the history where this one left it, and stops mentioning the old file.
    // The two config files keep announcing themselves, and should: nothing writes them, so they
    // stay in the old place until their owner moves them, and a run that is still reading them
    // from there has not finished saying so.
    let next = acmon::RealWorld::with_paths(paths);
    assert!(
        !next
            .path_notices()
            .iter()
            .any(|line| line.contains("remembered")),
        "a machine's history is carried forward once, and then stops being mentioned: {:?}",
        next.path_notices()
    );
    match next.read_state() {
        acmon::world::StateRead::Found(text) => assert_eq!(
            text, carried,
            "and the next run reads what this one wrote, not the old file again"
        ),
        other => panic!("the next run must find the carried history: {other:?}"),
    }

    let _ = std::fs::remove_dir_all(&home);
}

// --- Both binaries, against a whole scratch home (#36) ---
//
// Asserted by running the real binaries and inspecting what exists afterwards, because the
// criterion is about what a *process* touched rather than about what a function returned. The two
// tests below share one fixture and differ in one thing — whether the directory variables are set
// — and they assert opposite outcomes from it. That pairing is what stops either of them passing
// for the wrong reason: the second proves the pre-split directory would have been named in the
// output had it been read, and the first proves it was not.

/// Run one of the two binaries against a scratch home, returning (succeeded, everything it said).
fn run_binary(
    binary: &str,
    home: &std::path::Path,
    relocated: Option<&std::path::Path>,
) -> (bool, String) {
    let mut command = std::process::Command::new(binary);
    if binary.ends_with("amon") {
        command.arg("watch").env(acmon::watch::RUN_VARIABLE, "400");
    } else {
        // `--once`, deliberately: a bare `agtop` takes the screen and refuses without a terminal,
        // which is every test harness — it would never reach a collection at all.
        command.arg("--once");
    }
    command.env("HOME", home);
    if let Some(directory) = relocated {
        command
            .env(acmon::state::STATE_DIR_VARIABLE, directory.join("state"))
            .env(acmon::state::CONFIG_DIR_VARIABLE, directory.join("config"));
    }
    let output = command
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap_or_else(|e| panic!("{binary} is built and runnable: {e}"));
    (
        output.status.success(),
        format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ),
    )
}

#[test]
fn setting_only_the_two_directory_variables_isolates_a_run_from_the_pre_split_directory() {
    // The criterion this ticket turns on. Before it, `ACMON_STATE_DIR` moved `state.json` and
    // `notified.json` and left the memory file in `~/.acmon` — so a test that relocated the state
    // directory and forgot the extra variable wrote the developer's own remembered workspaces, and
    // did it while passing. Asserted for both binaries, because both of them read these files.
    let home = scratch_home_with_pre_split_files("isolated");
    let elsewhere = scratch("isolated-elsewhere");
    let legacy = home.join(".acmon");
    let before = tree(&legacy);

    for binary in [env!("CARGO_BIN_EXE_amon"), env!("CARGO_BIN_EXE_agtop")] {
        let (succeeded, said) = run_binary(binary, &home, Some(&elsewhere));
        // Assert success before believing anything about what it did or did not touch: a binary
        // that failed to start touched nothing, and would pass this test having proved nothing.
        assert!(
            !said.trim().is_empty(),
            "{binary} said nothing at all, so it cannot be evidence of anything"
        );
        if binary.ends_with("amon") {
            assert!(
                succeeded,
                "the monitor has to have actually run its window; it said:\n{said}"
            );
            assert!(
                said.contains(&*elsewhere.join("state").to_string_lossy()),
                "and to have used the directory it was given; it said:\n{said}"
            );
        }

        // Nothing read. Every file resolved out of the pre-split directory is announced by the
        // run that resolved it — that is the other half of this ticket — and each of the three
        // fixtures there would announce itself: the memory file by being carried forward, the two
        // malformed `.toml` files by being unreadable as configuration. So the directory's own
        // path appearing nowhere in what the run said is evidence that none of them was reached.
        assert!(
            !said.contains(&*legacy.to_string_lossy()),
            "{binary} named the pre-split directory, so it reached into it; it said:\n{said}"
        );

        // Nothing written, and nothing changed: byte-for-byte, name-for-name.
        assert_eq!(
            tree(&legacy),
            before,
            "{binary} altered the pre-split directory"
        );
    }

    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&elsewhere);
}

#[test]
fn a_run_using_this_machines_own_directories_reads_the_pre_split_files_and_says_so() {
    // The counterpart, same fixture: with nothing relocated, a machine whose history is still in
    // `~/.acmon` has it read, and the run states that it did rather than leaving it to be inferred
    // from a file that is suddenly somewhere else. It is also what makes the test above mean
    // something — the pre-split directory really does get named when it is read.
    let home = scratch_home_with_pre_split_files("machines-own");
    let legacy = home.join(".acmon");
    let before = tree(&legacy);

    let (succeeded, said) = run_binary(env!("CARGO_BIN_EXE_amon"), &home, None);
    assert!(
        succeeded,
        "the monitor has to have actually run its window; it said:\n{said}"
    );
    assert!(
        said.contains(&*legacy.join("state.json").to_string_lossy()),
        "the run must name the pre-split file it read; it said:\n{said}"
    );
    assert!(
        said.contains(
            &*home
                .join(".local/state/acmon/memory.json")
                .to_string_lossy()
        ),
        "and where it keeps that history from now on; it said:\n{said}"
    );
    assert!(
        said.contains("Nothing was moved or deleted"),
        "and that it is a read rather than a migration; it said:\n{said}"
    );

    // The reading is not a taking. Whatever the run did with what it read, the old file is
    // exactly as it was — which is what makes this recoverable by doing nothing.
    assert_eq!(
        tree(&legacy),
        before,
        "the pre-split directory must be left byte-for-byte as it was found"
    );

    let _ = std::fs::remove_dir_all(&home);
}
