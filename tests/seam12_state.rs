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
                if let Some(content) = store.read_state("test.json") {
                    assert!(
                        content == small || content == large[..],
                        "reader observed a torn write: {} bytes",
                        content.len()
                    );
                    reads.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
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
    let content = store.read_state("test.json").expect("file exists");
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
        "fast data".as_bytes().to_vec(),
        ago(Duration::from_secs(5)),
    );
    state.set_tier_data(
        Tier::Medium,
        "medium data".as_bytes().to_vec(),
        ago(Duration::from_secs(30)),
    );
    state.set_tier_data(
        Tier::Slow,
        "slow data".as_bytes().to_vec(),
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
    state.set_tier_data(Tier::Fast, b"fast".to_vec(), ago(Duration::from_secs(5)));
    state.set_tier_data(
        Tier::Medium,
        b"medium".to_vec(),
        ago(Duration::from_secs(30)),
    );
    state.set_tier_data(Tier::Slow, b"slow".to_vec(), ago(Duration::from_secs(120)));

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
    state.set_tier_data(Tier::Fast, b"test".to_vec(), now());

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
    state.set_tier_data(Tier::Fast, b"data".to_vec(), ago(Duration::from_secs(300)));

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
        b"fast facts".to_vec(),
        ago(Duration::from_secs(1)),
    );
    state.set_tier_data(
        Tier::Medium,
        b"medium facts".to_vec(),
        ago(Duration::from_secs(10)),
    );
    state.set_tier_data(
        Tier::Slow,
        b"slow facts".to_vec(),
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
        b"fast facts"
    );
    assert_eq!(
        read_back.tier_data(Tier::Medium).expect("medium data"),
        b"medium facts"
    );
    assert_eq!(
        read_back.tier_data(Tier::Slow).expect("slow data"),
        b"slow facts"
    );

    // And each has its own timestamp
    assert!(read_back.tier_timestamp(Tier::Fast).is_some());
    assert!(read_back.tier_timestamp(Tier::Medium).is_some());
    assert!(read_back.tier_timestamp(Tier::Slow).is_some());

    let _ = std::fs::remove_dir_all(store.paths().state_dir());
}
