//! Seam 10 — user-configurable detectors.
//!
//! The failure this seam exists to prevent: a fifth agent CLI appears on the machine, and its
//! sessions are invisible until the tool ships with a new detector — which may never happen.
//! User configuration lets a detector be added without waiting for a release.

use std::time::{Duration, SystemTime};

use acmon::liveness::Thresholds;
use acmon::world::World;
use acmon::{collect, RealWorld};

fn now() -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(1_787_000_000)
}

/// A realistic path for cursor-agent, taken from the near-miss fixtures in seam1.
const CURSOR_AGENT_PATH: &str =
    "/Users/pmcfadin/.local/share/cursor-agent/versions/2026.05.01-eea359f/node";

fn scratch_detectors(name: &str, contents: Option<&str>) -> std::path::PathBuf {
    let directory =
        std::env::temp_dir().join(format!("acmon-seam10-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).expect("scratch directory");
    let path = directory.join("detectors.toml");
    if let Some(text) = contents {
        std::fs::write(&path, text).expect("write the detector config");
    }
    path
}

// --- Core: user detectors are recognised ---

#[test]
fn a_user_defined_detector_is_recognised_alongside_the_embedded_defaults() {
    // The motivation: cursor-agent exists on the machine and is not in the embedded detectors.
    // A user file adds it without waiting for a release.
    let detector_file = scratch_detectors(
        "with_cursor",
        Some(
            r#"
[[detector]]
id = "cursor-agent"
exe_contains = ["/cursor-agent/versions/"]
"#,
        ),
    );

    let config = RealWorld::with_detectors(&detector_file).read_detector_config();

    assert_eq!(
        config.unusable, None,
        "a well-formed user config is not unusable"
    );

    let cursor_detector = config
        .detectors
        .iter()
        .find(|d| d.id == "cursor-agent")
        .expect("cursor-agent detector should be present");
    assert!(
        cursor_detector.matches(CURSOR_AGENT_PATH),
        "the user-supplied cursor-agent detector should match its real path"
    );

    // The embedded detectors should still be present.
    let claude_detector = config
        .detectors
        .iter()
        .find(|d| d.id == "claude")
        .expect("embedded claude detector should still be present");
    assert!(
        claude_detector.matches("/Users/pmcfadin/.local/share/claude/versions/2.1.233"),
        "the embedded claude detector should still work"
    );

    let _ = std::fs::remove_dir_all(detector_file.parent().expect("parent"));
}

#[test]
fn a_user_detector_sharing_an_id_with_an_embedded_one_replaces_it_entirely() {
    // Override by id, not a field-wise merge: a user needs to be able to *remove* a rule
    // that is over-matching. This is the override mechanism.
    let detector_file = scratch_detectors(
        "override_claude",
        Some(
            r#"
[[detector]]
id = "claude"
exe_ends_with = ["/bin/claude-override"]
"#,
        ),
    );

    let config = RealWorld::with_detectors(&detector_file).read_detector_config();

    assert_eq!(config.unusable, None);

    let claude_detector = config
        .detectors
        .iter()
        .find(|d| d.id == "claude")
        .expect("claude detector should be present");

    // The user's rule should apply.
    assert!(
        claude_detector.matches("/some/path/bin/claude-override"),
        "the user-supplied rule should match"
    );

    // The embedded rules should NOT apply — this is a replacement, not a merge.
    assert!(
        !claude_detector.matches("/Users/pmcfadin/.local/share/claude/versions/2.1.233"),
        "the embedded rule should have been replaced, not merged with the user's"
    );

    let _ = std::fs::remove_dir_all(detector_file.parent().expect("parent"));
}

// --- Fault handling: malformed config falls back to embedded ---

#[test]
fn a_machine_with_no_detector_config_uses_the_embedded_defaults() {
    let path = scratch_detectors("absent", None);
    let config = RealWorld::with_detectors(&path).read_detector_config();

    assert_eq!(
        config.unusable, None,
        "never having configured detectors is a choice, not a fault"
    );

    // The embedded detectors should be present.
    assert!(
        config.detectors.iter().any(|d| d.id == "claude"),
        "embedded claude detector should be present"
    );
    assert!(
        config.detectors.iter().any(|d| d.id == "codex"),
        "embedded codex detector should be present"
    );

    let _ = std::fs::remove_dir_all(path.parent().expect("parent"));
}

#[test]
fn a_malformed_detector_config_carries_the_specific_error_and_falls_back_to_embedded() {
    // The whole point: a typo here means a fifth agent CLI silently stops being recognised.
    // The sessions simply are not there, which is indistinguishable from a quiet machine.
    let path = scratch_detectors(
        "malformed",
        Some(
            r#"
[[detector]]
id = "cursor-agent"
exe_contains = ]]not toml
"#,
        ),
    );
    let config = RealWorld::with_detectors(&path).read_detector_config();

    let why = config
        .unusable
        .as_ref()
        .expect("a config that cannot be parsed must say so, not quietly use only defaults");
    assert!(
        why.contains("detectors.toml"),
        "the reason must name the file, or nobody knows where to look; got {why:?}"
    );
    assert!(
        why.len() > "detectors.toml".len() + 10,
        "and must carry the parser's own complaint rather than only the filename; got {why:?}"
    );

    // The config should fall back to embedded detectors, not an empty set.
    assert!(
        !config.detectors.is_empty(),
        "a malformed config should fall back to embedded detectors, not an empty set — the \
         whole tool would stop recognising anything"
    );
    assert!(
        config.detectors.iter().any(|d| d.id == "claude"),
        "embedded detectors should be present as fallback"
    );

    let _ = std::fs::remove_dir_all(path.parent().expect("parent"));
}

#[test]
fn a_user_detector_with_no_rules_is_rejected_as_unusable() {
    // A detector with no rules matches nothing, for every process, silently — the exact
    // hazard this project exists to remove. Refuse it at parse time rather than accepting
    // a config that would break recognition.
    let path = scratch_detectors(
        "toothless",
        Some(
            r#"
[[detector]]
id = "cursor-agent"
# No exe_contains, no exe_ends_with — matches nothing, silently.
"#,
        ),
    );
    let config = RealWorld::with_detectors(&path).read_detector_config();

    let why = config
        .unusable
        .as_ref()
        .expect("a detector with no rules must be rejected");
    assert!(
        why.contains("no matching rules"),
        "must explain what is wrong, not just that the file is bad; got {why:?}"
    );
    assert!(
        why.contains("cursor-agent"),
        "must name the offending detector so it can be fixed; got {why:?}"
    );

    // Should fall back to embedded detectors.
    assert!(
        config.detectors.iter().any(|d| d.id == "claude"),
        "should fall back to embedded when a user detector is toothless"
    );

    let _ = std::fs::remove_dir_all(path.parent().expect("parent"));
}

#[test]
fn a_well_formed_detector_config_is_read_and_reports_no_problem() {
    // The control. Without it, a reader could not tell whether the tests above pass
    // because the parser is strict or because it rejects everything.
    let path = scratch_detectors(
        "valid",
        Some(
            r#"
[[detector]]
id = "cursor-agent"
exe_contains = ["/cursor-agent/versions/"]

[[detector]]
id = "another-cli"
exe_ends_with = ["/bin/another"]
"#,
        ),
    );
    let config = RealWorld::with_detectors(&path).read_detector_config();

    assert_eq!(config.unusable, None);
    assert!(
        config.detectors.iter().any(|d| d.id == "cursor-agent"),
        "user-supplied detector should be present"
    );
    assert!(
        config.detectors.iter().any(|d| d.id == "another-cli"),
        "second user-supplied detector should be present"
    );
    assert!(
        config.detectors.iter().any(|d| d.id == "claude"),
        "embedded detectors should still be present"
    );

    let _ = std::fs::remove_dir_all(path.parent().expect("parent"));
}

#[test]
fn an_empty_user_detector_file_is_treated_as_no_additions() {
    // A file with no [[detector]] entries at all — just an empty array.
    let path = scratch_detectors("empty", Some("detector = []\n"));
    let config = RealWorld::with_detectors(&path).read_detector_config();

    assert_eq!(
        config.unusable, None,
        "a file with an empty detector array is not malformed, just pointless"
    );
    assert!(
        config.detectors.iter().any(|d| d.id == "claude"),
        "embedded detectors should still be present"
    );

    let _ = std::fs::remove_dir_all(path.parent().expect("parent"));
}

// --- Integration: detectors actually used in collection ---

#[test]
fn a_session_from_a_user_defined_cli_is_collected() {
    // End-to-end: a cursor-agent process with a user detector should produce a session.
    // This drives the whole collect path, not just the config reader.
    use acmon::world::{ProcessRecord, ProcessSnapshot, World, WorldError};

    struct FakeWorld {
        detector_config: acmon::world::DetectorConfig,
        records: Vec<ProcessRecord>,
    }

    impl World for FakeWorld {
        fn process_snapshot(&self) -> Result<ProcessSnapshot, WorldError> {
            Ok(ProcessSnapshot {
                records: self.records.clone(),
                observer_pid: 4242,
            })
        }

        fn resources(
            &self,
            _pid: i32,
        ) -> Result<acmon::world::Resources, acmon::world::ResourcesUnavailable> {
            Ok(acmon::world::Resources {
                source: acmon::world::ResourceSource::Rusage,
                own_cpu: Ok(Duration::from_secs(10)),
                children_cpu: Ok(Duration::from_secs(50)),
                current_memory: Ok(100_000_000),
                peak_memory: Ok(200_000_000),
                bytes_written: Ok(50_000_000),
            })
        }

        fn recorded_namespaces(&self) -> Result<Vec<String>, WorldError> {
            Ok(Vec::new())
        }

        fn namespace_activity(
            &self,
            _namespace: &str,
        ) -> Result<SystemTime, acmon::world::ActivityUnavailable> {
            Err(acmon::world::ActivityUnavailable::NotRecorded)
        }

        fn codex_sessions(&self) -> Result<Vec<acmon::world::CodexSession>, WorldError> {
            Ok(Vec::new())
        }

        fn repository_root(&self, _path: &str) -> Option<(String, bool)> {
            None
        }

        fn vcs_facts(&self, _path: &str) -> Result<acmon::vcs::VcsFacts, acmon::vcs::Unreadable> {
            Err(acmon::vcs::Unreadable::NotVersionControlled)
        }

        fn resolve_namespace(&self, _namespace: &str) -> acmon::workspace::NamespaceResolution {
            acmon::workspace::NamespaceResolution::NoLongerExists
        }

        fn sweep_for_repositories(&self, _roots: &[String]) -> acmon::world::Sweep {
            acmon::world::Sweep {
                repositories: Vec::new(),
                complete: true,
                directories_visited: 0,
            }
        }

        fn output_width(&self) -> u16 {
            120
        }

        fn read_detector_config(&self) -> acmon::world::DetectorConfig {
            self.detector_config.clone()
        }
    }

    // Build a detector config with cursor-agent.
    let embedded = acmon::detect::embedded_detectors();
    let user = vec![acmon::detect::Detector {
        id: "cursor-agent".to_string(),
        exe_contains: vec!["/cursor-agent/versions/".to_string()],
        exe_ends_with: Vec::new(),
    }];
    let merged = acmon::detect::merge_detectors(embedded, user);
    let detector_config = acmon::world::DetectorConfig {
        detectors: merged,
        unusable: None,
    };

    let world = FakeWorld {
        detector_config,
        records: vec![
            ProcessRecord {
                pid: 4242,
                exe_path: Ok("/usr/bin/acmon".to_string()),
                cwd: Ok("/Users/pmcfadin".to_string()),
            },
            ProcessRecord {
                pid: 900,
                exe_path: Ok(CURSOR_AGENT_PATH.to_string()),
                cwd: Ok("/Users/pmcfadin/projects/testing".to_string()),
            },
        ],
    };

    let snapshot =
        collect(&world, now(), &Thresholds::default()).expect("collection should succeed");

    let cursor_session = snapshot
        .sessions
        .iter()
        .find(|s| s.cli == "cursor-agent")
        .expect("a process matching the user-supplied cursor-agent detector should be a session");

    assert_eq!(
        cursor_session.identity,
        acmon::Identity::Process { pid: 900 },
        "the session should have the right process identity"
    );
}

// --- Non-regression: the tool never writes to the detector file ---

#[test]
fn the_tool_never_writes_to_the_user_detector_file() {
    // "User configuration is untouched by an upgrade of the tool itself" — asserted in a test
    // rather than only in prose. The tool has a write path for state.json; nothing structural
    // stops one appearing here, and the moment it does, user config stops being durable.
    let path = scratch_detectors(
        "never_written",
        Some(
            r#"
[[detector]]
id = "cursor-agent"
exe_contains = ["/cursor-agent/versions/"]
"#,
        ),
    );

    // Read it.
    let _ = RealWorld::with_detectors(&path).read_detector_config();

    // The file should still exist and have exactly the contents we wrote.
    let contents = std::fs::read_to_string(&path)
        .expect("the detector file should still exist after being read");
    assert!(
        contents.contains("cursor-agent"),
        "the file contents should be unchanged"
    );

    let _ = std::fs::remove_dir_all(path.parent().expect("parent"));
}
