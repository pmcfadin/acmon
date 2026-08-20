//! On-disk state contract: config and state split, written atomically, stamped per tier.
//!
//! Implements the requirements of issue #25 and PRD decisions 21, 26, 29:
//!
//! - Config in `~/.config/acmon/`, mutable state in `~/.local/state/acmon/`, split by mutability
//! - Atomic writes (write-temp-then-rename), so a reader never sees a half-written file
//! - Every write records the **writer pid**
//! - Every write carries a timestamp **per tier**, never one timestamp for the file
//! - No heartbeat file
//! - A reader can determine the age and writer of any fact from the file alone
//!
//! This is the mechanism only. The tiered collection loop (#27), the lock (#26), launch
//! records (#28), dedupe (#29), and the display's freshness classification (#30) are
//! separate concerns.

use std::collections::HashMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

/// The three tiers, as defined in the PRD.
///
/// Every fact in the state file belongs to exactly one tier and carries that tier's age,
/// not the file's. The tiers are:
///
/// - **Fast**: near-free signals (libproc, ps) — collected frequently
/// - **Medium**: lsof-class operations — medium cadence
/// - **Slow**: git and Codex (2.7s for 34 workspaces) — slow cadence
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Tier {
    Fast,
    Medium,
    Slow,
}

/// The state file every reader looks for, and the only one `amon watch` publishes facts in.
///
/// Named here rather than spelled out at each call site so the monitor and the display cannot
/// disagree about which file they mean.
pub const STATE_FILE: &str = "state.json";

/// The environment variable that relocates the state **directory**.
///
/// Distinct from `ACMON_STATE`, which names the pre-split memory *file*. Its job is to let a
/// test drive the real acquire-write-release path — lock included — against a temporary
/// directory. A test that had to use the developer's own `~/.local/state/acmon/` would either
/// destroy real history or be skipped, and a skipped test of a lock is how two writers ship.
pub const STATE_DIR_VARIABLE: &str = "ACMON_STATE_DIR";

/// The environment variable that relocates the config directory.
pub const CONFIG_DIR_VARIABLE: &str = "ACMON_CONFIG_DIR";

/// Path resolution: config vs. state split.
///
/// Keeps config (`~/.config/acmon/`) and mutable state (`~/.local/state/acmon/`) separate,
/// so "keep my config in dotfiles" and "delete the state directory to recover" are both
/// safe operations.
#[derive(Debug, Clone)]
pub struct Paths {
    config: PathBuf,
    state: PathBuf,
}

impl Paths {
    /// Standard paths for the user's home directory.
    pub fn for_home(home: &Path) -> Self {
        Paths {
            config: home.join(".config").join("acmon"),
            state: home.join(".local").join("state").join("acmon"),
        }
    }

    /// Override base directory for testing.
    pub fn with_base(base: &Path) -> Self {
        Paths {
            config: base.join("config"),
            state: base.join("state"),
        }
    }

    /// Where this machine keeps them, honouring the two overrides.
    ///
    /// Fails rather than guessing. A stand-in path chosen because it is certain to be empty
    /// would make a monitor that cannot find its own state look like a monitor with nothing
    /// to report.
    pub fn from_environment() -> Result<Self, String> {
        Paths::from_values(
            std::env::var(CONFIG_DIR_VARIABLE).ok().as_deref(),
            std::env::var(STATE_DIR_VARIABLE).ok().as_deref(),
            std::env::var("HOME").ok().as_deref(),
        )
    }

    /// The resolution itself, with the environment passed in.
    ///
    /// Separated so it can be tested without mutating the process environment, which every
    /// other test in the same binary shares.
    pub fn from_values(
        config_dir: Option<&str>,
        state_dir: Option<&str>,
        home: Option<&str>,
    ) -> Result<Self, String> {
        let home = home.map(str::trim).filter(|value| !value.is_empty());

        let resolve = |explicit: Option<&str>, variable: &str, under_home: &[&str]| match explicit
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            Some(path) => Ok(PathBuf::from(path)),
            None => match home {
                Some(home) => Ok(under_home
                    .iter()
                    .fold(PathBuf::from(home), |path, segment| path.join(segment))),
                None => Err(format!(
                    "HOME is not readable, so {variable} must name the directory explicitly"
                )),
            },
        };

        Ok(Paths {
            config: resolve(config_dir, CONFIG_DIR_VARIABLE, &[".config", "acmon"])?,
            state: resolve(state_dir, STATE_DIR_VARIABLE, &[".local", "state", "acmon"])?,
        })
    }

    /// Where config files live: `detectors.toml`, `notify.toml`.
    ///
    /// An override layer over the embedded default catalog, surviving `brew upgrade`.
    pub fn config_dir(&self) -> &Path {
        &self.config
    }

    /// Where mutable state lives: `state.json`, `registry.json`, `events.jsonl`,
    /// `notified.json`, `starts.jsonl`.
    ///
    /// Split from config so that deleting this directory loses history and nothing else.
    pub fn state_dir(&self) -> &Path {
        &self.state
    }
}

/// State stored per tier with its own timestamp.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct TierEntry {
    /// When this tier's data was written.
    #[serde(with = "iso")]
    timestamp: SystemTime,
    /// The tier's data as JSON.
    ///
    /// The tiered collection loop (#27) owns the schema. Stored as `serde_json::Value`
    /// rather than opaque bytes because the collection loop will write JSON, and storing
    /// it as JSON keeps the file human-readable without base64 encoding.
    data: serde_json::Value,
}

/// The on-disk format: writer pid + per-tier data and timestamps.
///
/// No single file-level timestamp — it would describe the newest fact and misdescribe every
/// older one. No heartbeat file — a heartbeat can be fresh while the write it was meant to
/// prove has failed.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct StateFile {
    /// Schema version for forward/backward compatibility.
    version: u32,
    /// The pid of the process that wrote this file.
    ///
    /// A reader can determine whether the writer is still alive without asking it anything,
    /// which is how a wedged monitor becomes detectable rather than reading as healthy.
    writer_pid: u32,
    /// Per-tier entries, each with its own timestamp and data.
    tiers: HashMap<Tier, TierEntry>,
}

const STATE_VERSION: u32 = 1;

/// Tiered state: data organized by tier, each with its own timestamp.
///
/// This is the in-memory representation that gets serialized to disk.
#[derive(Debug, Clone)]
pub struct TieredState {
    writer_pid: u32,
    tiers: HashMap<Tier, TierEntry>,
}

impl TieredState {
    /// Create a new empty tiered state for the given writer pid.
    pub fn new(writer_pid: u32) -> Self {
        TieredState {
            writer_pid,
            tiers: HashMap::new(),
        }
    }

    /// Set data for a tier with its timestamp.
    pub fn set_tier_data(&mut self, tier: Tier, data: serde_json::Value, timestamp: SystemTime) {
        self.tiers.insert(tier, TierEntry { timestamp, data });
    }

    /// Get data for a tier.
    pub fn tier_data(&self, tier: Tier) -> Option<&serde_json::Value> {
        self.tiers.get(&tier).map(|entry| &entry.data)
    }

    /// Get the timestamp for a tier.
    pub fn tier_timestamp(&self, tier: Tier) -> Option<SystemTime> {
        self.tiers.get(&tier).map(|entry| entry.timestamp)
    }

    /// Get the writer pid.
    pub fn writer_pid(&self) -> u32 {
        self.writer_pid
    }

    /// Serialize to the on-disk format.
    fn to_state_file(&self) -> StateFile {
        StateFile {
            version: STATE_VERSION,
            writer_pid: self.writer_pid,
            tiers: self.tiers.clone(),
        }
    }

    /// Deserialize from the on-disk format.
    fn from_state_file(file: StateFile) -> Result<Self, String> {
        if file.version != STATE_VERSION {
            return Err(format!(
                "unknown state version {} (understood: {})",
                file.version, STATE_VERSION
            ));
        }
        Ok(TieredState {
            writer_pid: file.writer_pid,
            tiers: file.tiers,
        })
    }
}

/// The state store: atomic writes, path management.
///
/// Handles the mechanics of writing state to disk atomically (write-temp-then-rename) and
/// reading it back. Does not interpret the data — that's for the collection loop.
pub struct StateStore {
    paths: Paths,
}

impl StateStore {
    /// Create a state store for the given paths.
    pub fn new(paths: Paths) -> Self {
        StateStore { paths }
    }

    /// Get the paths this store uses.
    pub fn paths(&self) -> &Paths {
        &self.paths
    }

    /// Write state atomically: write to a temp file, then rename.
    ///
    /// The rename is atomic on all POSIX filesystems, so a concurrent reader never observes
    /// a half-written file. A `SIGKILL` mid-write loses only the pass in flight.
    ///
    /// Creates the state directory if it doesn't exist.
    pub fn write_state(&self, name: &str, data: &[u8], writer_pid: u32) -> Result<(), String> {
        let state_dir = self.paths.state_dir();
        std::fs::create_dir_all(state_dir).map_err(|e| {
            format!(
                "could not create state directory {}: {}",
                state_dir.display(),
                e
            )
        })?;

        let final_path = state_dir.join(name);

        // Write to a temp file in the same directory (required for atomic rename)
        let temp_path = state_dir.join(format!(".{}.tmp.{}", name, writer_pid));

        let mut file = std::fs::File::create(&temp_path)
            .map_err(|e| format!("could not create temp file {}: {}", temp_path.display(), e))?;

        file.write_all(data)
            .map_err(|e| format!("could not write to {}: {}", temp_path.display(), e))?;

        file.sync_all()
            .map_err(|e| format!("could not sync {}: {}", temp_path.display(), e))?;

        // Atomic rename
        std::fs::rename(&temp_path, &final_path).map_err(|e| {
            format!(
                "could not rename {} to {}: {}",
                temp_path.display(),
                final_path.display(),
                e
            )
        })?;

        Ok(())
    }

    /// Read state if it exists.
    ///
    /// Returns `Ok(None)` if the file doesn't exist (a first run), `Ok(Some(data))` on
    /// success, or `Err` if the file exists but cannot be read.
    ///
    /// Distinguishes three outcomes to avoid "fail to zero": a file that exists but cannot
    /// be read — permissions, I/O error, corrupted filesystem — must not be indistinguishable
    /// from "no file yet", which is what `None` means.
    pub fn read_state(&self, name: &str) -> Result<Option<Vec<u8>>, String> {
        let path = self.paths.state_dir().join(name);
        match std::fs::read(&path) {
            Ok(data) => Ok(Some(data)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!(
                "could not read state file {}: {}",
                path.display(),
                e
            )),
        }
    }

    /// Write tiered state atomically.
    ///
    /// Serializes the state to JSON and writes it atomically. The JSON is pretty-printed
    /// with ISO 8601 timestamps, so a human can read and verify it.
    pub fn write_tiered_state(&self, name: &str, state: &TieredState) -> Result<(), String> {
        let file = state.to_state_file();
        let json =
            serde_json::to_string_pretty(&file).expect("tiered state is always serializable");
        self.write_state(name, json.as_bytes(), state.writer_pid)
    }

    /// Read tiered state if it exists.
    ///
    /// Returns `Ok(None)` if the file doesn't exist (a first run), `Ok(Some(state))` on
    /// success, or `Err` if the file exists but cannot be read or parsed.
    pub fn read_tiered_state(&self, name: &str) -> Result<Option<TieredState>, String> {
        let data = match self.read_state(name)? {
            Some(data) => data,
            None => return Ok(None),
        };

        let text =
            String::from_utf8(data).map_err(|e| format!("state file is not valid UTF-8: {}", e))?;

        let file: StateFile = serde_json::from_str(&text)
            .map_err(|e| format!("could not parse state file: {}", e))?;

        TieredState::from_state_file(file).map(Some)
    }
}

/// Timestamps as ISO 8601, so the state file can be read without a converter.
///
/// Shares the time conversion logic from isotime.rs (extracted to avoid duplication with
/// memory.rs).
mod iso {
    use std::time::SystemTime;

    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(time: &SystemTime, serializer: S) -> Result<S::Ok, S::Error> {
        crate::isotime::iso8601_from_unix_seconds(crate::isotime::unix_seconds(*time))
            .serialize(serializer)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<SystemTime, D::Error> {
        let text = String::deserialize(deserializer)?;
        crate::isotime::unix_seconds_from_iso8601(&text)
            .map(crate::isotime::time_from_unix_seconds)
            .map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tier_entry_round_trips_through_json() {
        let entry = TierEntry {
            timestamp: SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_787_000_000),
            data: serde_json::json!({"test": "data"}),
        };

        let json = serde_json::to_string(&entry).expect("serialize");
        let parsed: TierEntry = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(parsed.data, entry.data);
        assert_eq!(parsed.timestamp, entry.timestamp);
    }

    #[test]
    fn state_file_round_trips() {
        let mut tiers = HashMap::new();
        tiers.insert(
            Tier::Fast,
            TierEntry {
                timestamp: SystemTime::UNIX_EPOCH,
                data: serde_json::json!("fast"),
            },
        );

        let file = StateFile {
            version: STATE_VERSION,
            writer_pid: 12345,
            tiers,
        };

        let json = serde_json::to_string_pretty(&file).expect("serialize");
        let parsed: StateFile = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(parsed.version, file.version);
        assert_eq!(parsed.writer_pid, file.writer_pid);
        assert_eq!(parsed.tiers.len(), 1);
    }
}
