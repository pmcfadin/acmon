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

/// The file the notification dedupe record lives in.
///
/// Named here, beside [`STATE_FILE`], for the same reason: the monitor that writes it and the
/// reader that consults it must not be able to disagree about which file they mean. Its own
/// artefact rather than a field of `state.json` because the two degrade independently — a
/// `state.json` this build cannot understand must not also cost the dedupe record, which would
/// turn one unreadable file into an alert storm.
pub const NOTIFIED_FILE: &str = "notified.json";

/// The file the workspaces and sessions carried between runs live in.
///
/// Not `state.json`: that name is taken, in this very directory, by the tiered file above. The
/// memory file was called `state.json` while it lived alone in `~/.acmon/`, and moving it here
/// under that name would have made one directory hold two different files with one name.
pub const MEMORY_FILE: &str = "memory.json";

/// The notification channel configuration, in the config directory.
pub const NOTIFY_CONFIG_FILE: &str = "notify.toml";

/// The user's detector overrides, in the config directory.
pub const DETECTORS_FILE: &str = "detectors.toml";

/// The one directory all of this lived in before #25 split config from state.
///
/// Named here rather than spelled out where it is consulted, because the whole point of naming
/// it is that no new code should ever join it to a path again.
pub const LEGACY_DIR: &str = ".acmon";

/// What [`MEMORY_FILE`] was called in [`LEGACY_DIR`].
pub const LEGACY_MEMORY_FILE: &str = "state.json";

/// The environment variable that relocates the state **directory**.
///
/// Distinct from `ACMON_STATE`, which names the memory *file* on its own. Its job is to let a
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
    /// The pre-split `~/.acmon`, and only when this run's directories are the machine's own.
    ///
    /// `None` the moment either directory is named explicitly, which is what makes
    /// `ACMON_STATE_DIR` and `ACMON_CONFIG_DIR` enough to isolate a run: an isolated run has no
    /// legacy directory to consult, so it cannot read one file out of the developer's home while
    /// writing every other one into a scratch tree. A relocated run that still reached into
    /// `~/.acmon` would be the worst of both — isolated enough to look safe in a test name, and
    /// not isolated at all.
    legacy: Option<PathBuf>,
}

impl Paths {
    /// Standard paths for the user's home directory.
    pub fn for_home(home: &Path) -> Self {
        Paths {
            config: home.join(".config").join("acmon"),
            state: home.join(".local").join("state").join("acmon"),
            legacy: Some(home.join(LEGACY_DIR)),
        }
    }

    /// Override base directory for testing.
    pub fn with_base(base: &Path) -> Self {
        Paths {
            config: base.join("config"),
            state: base.join("state"),
            // Both directories are named, so there is no home this could be under. A test that
            // wants the pre-split directory in play builds a whole scratch home and resolves
            // through [`Paths::from_values`], the way a real run does.
            legacy: None,
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

        let named = |explicit: Option<&str>| {
            explicit
                .map(str::trim)
                .is_some_and(|value| !value.is_empty())
        };
        // The pre-split directory belongs to a run using this machine's own directories. Naming
        // either one is a statement that this run's files live somewhere else entirely, and it is
        // taken at its word.
        let legacy = match (named(config_dir) || named(state_dir), home) {
            (false, Some(home)) => Some(PathBuf::from(home).join(LEGACY_DIR)),
            _ => None,
        };

        Ok(Paths {
            config: resolve(config_dir, CONFIG_DIR_VARIABLE, &[".config", "acmon"])?,
            state: resolve(state_dir, STATE_DIR_VARIABLE, &[".local", "state", "acmon"])?,
            legacy,
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

    /// The pre-split `~/.acmon`, when this run is entitled to look at it at all.
    ///
    /// `None` for a run whose directories were named explicitly. See the field.
    pub fn legacy_dir(&self) -> Option<&Path> {
        self.legacy.as_deref()
    }

    /// Where this run reads and writes the workspaces and sessions carried between runs.
    ///
    /// `explicit` is `ACMON_STATE`, which still names the file outright when it is set.
    pub fn locate_memory(&self, explicit: Option<&str>) -> Located {
        self.locate(
            "the remembered history",
            "and everything written from here on is written there, so the history carries itself \
             across without anything being moved",
            self.state.join(MEMORY_FILE),
            LEGACY_MEMORY_FILE,
            explicit,
            crate::real_world::STATE_VARIABLE,
        )
    }

    /// Where this run reads its notification channel configuration.
    ///
    /// `explicit` is `ACMON_NOTIFY_CONFIG`.
    pub fn locate_notify_config(&self, explicit: Option<&str>) -> Located {
        self.locate(
            "the notification configuration",
            "and nothing writes it, so move it there yourself when you want it read from the \
             split's own location",
            self.config.join(NOTIFY_CONFIG_FILE),
            NOTIFY_CONFIG_FILE,
            explicit,
            crate::real_world::NOTIFY_CONFIG_VARIABLE,
        )
    }

    /// Where this run reads its detector overrides.
    ///
    /// `explicit` is `ACMON_DETECTORS`.
    pub fn locate_detectors(&self, explicit: Option<&str>) -> Located {
        self.locate(
            "the detector configuration",
            "and nothing writes it, so move it there yourself when you want it read from the \
             split's own location",
            self.config.join(DETECTORS_FILE),
            DETECTORS_FILE,
            explicit,
            crate::real_world::DETECTORS_VARIABLE,
        )
    }

    /// The one rule all three follow, so none of them can drift from the others.
    ///
    /// An explicit path wins outright. Otherwise the split's own location is used — unless it
    /// holds no such file and the pre-split directory does, in which case the pre-split one is
    /// **read** and said out loud. That last clause is the whole of this ticket: starting from an
    /// empty memory because a file moved would report a machine with no remembered sessions,
    /// which is a calm, plausible, wrong answer and is exactly what `AGENTS.md` forbids. Nothing
    /// is moved or deleted to achieve it — a write always goes to `canonical`, so the first run
    /// that writes carries the history across on its own and leaves the old file untouched behind
    /// it, recoverable by anyone who wants it back.
    fn locate(
        &self,
        what: &'static str,
        then: &'static str,
        canonical: PathBuf,
        legacy_name: &str,
        explicit: Option<&str>,
        variable: &'static str,
    ) -> Located {
        if let Some(path) = explicit.map(str::trim).filter(|value| !value.is_empty()) {
            return Located {
                what,
                then,
                read: PathBuf::from(path),
                canonical: PathBuf::from(path),
                how: Found::Named(variable),
            };
        }

        // Checked in this order deliberately: the split's location is authoritative the instant it
        // exists, so a machine that has been carried forward once never looks back again.
        if !canonical.exists() {
            if let Some(legacy) = self.legacy.as_ref().map(|dir| dir.join(legacy_name)) {
                if legacy.exists() {
                    return Located {
                        what,
                        then,
                        read: legacy,
                        canonical,
                        how: Found::CarriedForward,
                    };
                }
            }
        }

        Located {
            what,
            then,
            read: canonical.clone(),
            canonical,
            how: Found::InPlace,
        }
    }
}

/// Which file a run actually reads, and where a write of it would go.
///
/// Two paths rather than one, because they differ for exactly one run per machine: the one that
/// finds its history in the pre-split directory, reads it from there, and writes it to the
/// split's own location.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Located {
    what: &'static str,
    /// What follows "nothing was moved or deleted" in the sentence a carried-forward file says.
    ///
    /// Per file, because the two halves of the split differ in the one way that matters to whoever
    /// reads the line: the state directory's file is written, so it carries itself across on the
    /// next write, and the config directory's files are only ever read, so they stay where they are
    /// until their owner moves them.
    then: &'static str,
    read: PathBuf,
    canonical: PathBuf,
    how: Found,
}

/// How a [`Located`] path was arrived at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Found {
    /// In the directory the #25 split puts it in. The ordinary case, and silent.
    InPlace,
    /// At a path named outright by the given environment variable.
    Named(&'static str),
    /// In the pre-split `~/.acmon`, because the split's own location holds no such file.
    CarriedForward,
}

impl Located {
    /// The file this run reads.
    pub fn read_path(&self) -> &Path {
        &self.read
    }

    /// The file this run writes, and where the next run will look first.
    pub fn write_path(&self) -> &Path {
        &self.canonical
    }

    /// How this path was arrived at.
    pub fn how(&self) -> Found {
        self.how
    }

    /// What has to be said about where this file was found, or `None` when nothing does.
    ///
    /// Silent for the ordinary case and only for the ordinary case. A run that read a pre-split
    /// file says so, because "read the old one" and "started from nothing" produce the same
    /// screen otherwise, and one of them is a lie about the machine.
    pub fn worth_stating(&self) -> Option<String> {
        match self.how {
            Found::InPlace => None,
            Found::Named(variable) => Some(format!(
                "{} is {}, named by {variable} rather than found in its usual directory",
                self.what,
                self.read.display()
            )),
            Found::CarriedForward => Some(format!(
                "{} was read from the pre-split {}, because {} does not exist. Nothing was moved \
                 or deleted: {} is where this tool looks first, {}",
                self.what,
                self.read.display(),
                self.canonical.display(),
                self.canonical.display(),
                self.then
            )),
        }
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

    /// How many tiers this state carries.
    ///
    /// A reader needs this to tell a monitor that has published facts from one that holds the
    /// writer role and has collected nothing — which are the same file with the same pid in it,
    /// and which mean opposite things on a screen.
    pub fn tier_count(&self) -> usize {
        self.tiers.len()
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

    /// Read a state artefact as text, in the three-outcome shape a reader has to act on.
    ///
    /// The same three outcomes [`crate::world::StateRead`] exists for, because collapsing any
    /// two of them is how a state directory that cannot be read starts reading as a state
    /// directory with nothing in it. Offered here so that the monitor and a test drive the same
    /// code: a fake that reimplemented this read could pass while the real one was broken.
    pub fn read_text(&self, name: &str) -> crate::world::StateRead {
        match self.read_state(name) {
            Ok(Some(bytes)) => match String::from_utf8(bytes) {
                Ok(text) => crate::world::StateRead::Found(text),
                Err(error) => crate::world::StateRead::Unreadable(format!(
                    "{} is not valid UTF-8: {error}",
                    self.paths.state_dir().join(name).display()
                )),
            },
            Ok(None) => crate::world::StateRead::Absent,
            Err(why) => crate::world::StateRead::Unreadable(why),
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
