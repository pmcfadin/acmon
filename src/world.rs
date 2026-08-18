//! The only module permitted to describe the operating system.
//!
//! Everything else in the crate consumes [`World`], so the rest of the codebase is
//! pure and testable from captured fixtures.

use serde::{Deserialize, Serialize};

/// Why a path belonging to a process could not be read.
///
/// Each variant must be TRUE when reported. A reason that is merely plausible is
/// worse than no reason at all: it reads as a finding rather than as ignorance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathUnavailable {
    /// The process no longer exists. Confirmed after the failed read, not guessed.
    ProcessExited,
    /// The process still exists but its path could not be read.
    PermissionDenied,
}

/// One process, as observed in a single enumeration pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessRecord {
    pub pid: i32,
    /// The resolved executable path, or the reason it could not be obtained.
    ///
    /// An unmeasurable value is reported as absent with a stated reason — never an
    /// empty string. This is the overriding rule from AGENTS.md.
    pub exe_path: Result<String, PathUnavailable>,
    /// The process's current working directory, or the reason it could not be obtained.
    ///
    /// Read in the same pass as [`ProcessRecord::exe_path`], deliberately. Resolving it
    /// in a second pass reports processes that merely exited in between as unreadable —
    /// six such phantoms were observed while writing
    /// `docs/observability-mechanics.md` §4.1.
    pub cwd: Result<String, PathUnavailable>,
}

/// A whole-machine process enumeration.
///
/// `observer_pid` is the pid of the process that took the snapshot. It exists so a
/// snapshot can be checked against itself — see [`ProcessSnapshot::contains_observer`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessSnapshot {
    pub records: Vec<ProcessRecord>,
    pub observer_pid: i32,
}

impl ProcessSnapshot {
    /// Whether this snapshot contains the process that produced it.
    ///
    /// An all-process enumeration necessarily includes the observer. If it does not,
    /// the enumeration failed part-way and its emptiness carries no information —
    /// which is materially different from an idle machine.
    pub fn contains_observer(&self) -> bool {
        self.records.iter().any(|r| r.pid == self.observer_pid)
    }
}

/// Which reader produced a set of figures.
///
/// The source is part of the reading, not a detail behind it: the two readers do not
/// measure the same thing, and only one of them can see a process's children at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResourceSource {
    /// `proc_pid_rusage()` — the full ledger, own and children, for processes owned by
    /// the calling user.
    Rusage,
    /// `ps(1)` — cumulative own CPU and resident size, and nothing else. The fallback
    /// for processes owned by another user, where the full ledger is refused without
    /// elevated privileges.
    Ps,
}

impl std::fmt::Display for ResourceSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ResourceSource::Rusage => write!(f, "proc_pid_rusage"),
            ResourceSource::Ps => write!(f, "ps"),
        }
    }
}

/// Why one figure within a reading is missing.
///
/// Rendered in place of the number, so each variant has to be true and has to be short.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Unmeasured {
    /// The reader that answered does not report this figure at all.
    NotReportedBy(ResourceSource),
    /// The process exited before this figure could be read.
    ProcessExited,
    /// The process is alive, but this figure is not readable by this user.
    PermissionDenied,
}

impl std::fmt::Display for Unmeasured {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Unmeasured::NotReportedBy(ResourceSource::Ps) => write!(f, "ps-blind"),
            Unmeasured::NotReportedBy(ResourceSource::Rusage) => write!(f, "unlogged"),
            Unmeasured::ProcessExited => write!(f, "exited"),
            Unmeasured::PermissionDenied => write!(f, "no-perm"),
        }
    }
}

/// One process's resource ledger, as far as it could be read.
///
/// Each figure is separately present-or-absent because the fallback reader supplies
/// only some of them. An absent figure carries a reason; none of them defaults to zero.
/// Serialised into the state file so that a session's lifetime totals outlive its process.
/// The reasons a figure is absent are serialised with it: a remembered reading that came
/// back as `ps-blind` must still say so next run rather than reappearing as a zero.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Resources {
    /// Which reader answered, and therefore what could have been seen at all.
    pub source: ResourceSource,
    /// CPU consumed by this process itself, user plus system.
    ///
    /// For an agent session this is reasoning and orchestration — not the builds, tests
    /// and hooks it launches, which land in [`Resources::children_cpu`].
    pub own_cpu: Result<std::time::Duration, Unmeasured>,
    /// CPU consumed by children this process has reaped, recursively — grandchildren
    /// included.
    ///
    /// **A floor, never a total.** Work that detached or was orphaned before being
    /// reaped never enters the ledger, so the true figure is this or more. See
    /// `docs/observability-mechanics.md` §2.4.
    pub children_cpu: Result<std::time::Duration, Unmeasured>,
    /// Current physical footprint, in bytes. A point sample, not a cumulative counter.
    pub current_memory: Result<u64, Unmeasured>,
    /// Largest physical footprint reached in this process's lifetime, in bytes.
    pub peak_memory: Result<u64, Unmeasured>,
    /// Bytes written to disk over this process's lifetime.
    pub bytes_written: Result<u64, Unmeasured>,
}

/// Why no figure at all could be read for a process.
///
/// Distinct from a [`Resources`] full of [`Unmeasured`] fields: that says the reading
/// happened and was partial, this says no reading happened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResourcesUnavailable {
    /// The process exited between being listed and being read. Confirmed, not assumed.
    ProcessExited,
    /// The process is alive, but every available reader refused or failed. Carries what
    /// the readers said, so the reason is reportable rather than merely categorised.
    AllReadersFailed(String),
}

impl std::fmt::Display for ResourcesUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ResourcesUnavailable::ProcessExited => write!(f, "exited"),
            ResourcesUnavailable::AllReadersFailed(_) => write!(f, "refused"),
        }
    }
}

/// Why a namespace's activity time could not be read.
///
/// Each variant must be TRUE when reported. A suspiciously epoch timestamp that is merely
/// plausible is worse than an absent value with a stated reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActivityUnavailable {
    /// No directory of that name exists. Distinct from unreadable: this is an answer.
    NotRecorded,
    /// The directory exists but its contents or times could not be read.
    Unreadable(String),
    /// The directory exists and holds no transcript, so it has no activity time.
    NoTranscripts,
}

impl std::fmt::Display for ActivityUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ActivityUnavailable::NotRecorded => write!(f, "not-recorded"),
            ActivityUnavailable::Unreadable(_) => write!(f, "unreadable"),
            ActivityUnavailable::NoTranscripts => write!(f, "no-transcripts"),
        }
    }
}

/// A Codex session recorded in the transcript store, recent enough to be worth reading.
///
/// Codex's transcript path encodes the date and the session id but **not** the working
/// directory, so unlike Claude Code there is no namespace to map a path onto. The
/// workspace can only come from inside the transcript — see
/// `docs/observability-mechanics.md` §4.4.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexSession {
    /// The session id. It appears verbatim in the transcript's filename, which is what
    /// makes locating the file by id possible without reading any of them.
    pub id: String,
    /// The workspace, taken from `payload.cwd` of the transcript's **first record only**.
    ///
    /// That record is metadata: cwd, versions, model provider. No conversation content is
    /// read, and nothing else from the record is retained.
    pub workspace: String,
    /// When this session last changed, taken from the `updated_at` field in the session
    /// index. This is the timestamp the index already parses for recency filtering, so
    /// it is available without reading anything extra.
    pub last_activity: std::time::SystemTime,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorldError {
    /// The process table could not be enumerated at all.
    ProcessEnumeration(String),
    /// The Codex session index could not be read, or a transcript it pointed at could
    /// not be understood.
    CodexIndex(String),
    /// The recorded transcript namespaces could not be listed.
    ///
    /// Not fatal to a collection: the sessions are still observable, only their
    /// workspaces cannot be attributed to a transcript.
    NamespaceListing(String),
}

impl std::fmt::Display for WorldError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WorldError::ProcessEnumeration(msg) => {
                write!(f, "process enumeration failed: {}", msg)
            }
            WorldError::NamespaceListing(msg) => {
                write!(f, "could not list recorded transcript namespaces: {}", msg)
            }
            WorldError::CodexIndex(msg) => {
                write!(f, "could not read the Codex session index: {}", msg)
            }
        }
    }
}

impl std::error::Error for WorldError {}

/// What a sweep for repositories found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sweep {
    /// Each workspace found, with whether it is a linked worktree.
    pub repositories: Vec<(String, bool)>,
    /// False when the sweep hit its bound before finishing. A partial sweep presented as
    /// complete is a silent cap, and a silent cap in a safety net reads as "nothing to
    /// report".
    pub complete: bool,
    /// How many directories were visited, so both the cost and the bound are checkable.
    pub directories_visited: usize,
}

/// What was found where the state left by earlier runs is kept.
///
/// Three outcomes, and they are deliberately distinct. "No file yet" is an answer — the
/// first run on a machine has nothing to remember and that is not a fault. "A file that
/// could not be read" is not an answer, and collapsing it into the first would silently
/// discard every workspace this tool had been watching.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StateRead {
    /// The state file's contents, exactly as stored.
    Found(String),
    /// Nothing has been stored yet. A first run, not a failure.
    Absent,
    /// Something is stored and could not be read. Carries what the filesystem said, so the
    /// reason can be acted on rather than merely noted.
    Unreadable(String),
}

/// Everything the collector needs from outside itself.
pub trait World {
    /// Enumerate all processes with their executable paths.
    ///
    /// One logical observation, not one instant: pids are enumerated and each path is then
    /// read, because macOS offers no call that returns them together. A process that exits
    /// in between produces a record carrying [`PathUnavailable::ProcessExited`] — a reason
    /// established by asking, not assumed. Such a record is excluded when sessions are
    /// formed, so an exiting process is never reported as a session, nor as one with an
    /// unreadable field.
    ///
    /// Establishing the reason rather than guessing it is what makes that last clause hold.
    /// A process that is merely unreadable reports [`PathUnavailable::PermissionDenied`] and
    /// is a different thing entirely: it is alive, and it is a session we are failing to see.
    /// Implementations MUST determine which of the two is true, because a confidently wrong
    /// reason is worse than an admitted unknown — six phantom "unreadable" entries that were
    /// merely dead processes were observed while writing
    /// `docs/observability-mechanics.md` §4.1.
    fn process_snapshot(&self) -> Result<ProcessSnapshot, WorldError>;

    /// Read one process's resource ledger.
    ///
    /// Separate from [`World::process_snapshot`] because it is asked only about the
    /// processes that turned out to matter, and because a pid can exit in between —
    /// which is a reportable state, not an error to swallow.
    ///
    /// Implementations MUST prefer the reader that can see children, and fall back to a
    /// coarser one only when the full ledger is refused. A figure the answering reader
    /// cannot supply is [`Unmeasured::NotReportedBy`], never zero.
    fn resources(&self, pid: i32) -> Result<Resources, ResourcesUnavailable>;

    /// List the transcript namespaces recorded on this machine.
    ///
    /// Read once per collection rather than once per session: it is a single directory
    /// listing, and asking repeatedly would multiply the cost by the session count for
    /// an answer that cannot change in between.
    ///
    /// Only the directory *names* are read. No transcript is opened — they contain
    /// conversation content, which this tool never reads.
    fn recorded_namespaces(&self) -> Result<Vec<String>, WorldError>;

    /// When a Claude session's transcript namespace last changed.
    ///
    /// A namespace is a directory under `~/.claude/projects/<namespace>/` holding one or
    /// more `.jsonl` transcripts. Its last activity is the most recent modification time
    /// among those files — not the directory's own mtime, which does not update when a
    /// file inside it is appended to.
    ///
    /// Implementations use the directory entries' metadata only — no transcript is
    /// opened, because they contain conversation content.
    fn namespace_activity(
        &self,
        namespace: &str,
    ) -> Result<std::time::SystemTime, ActivityUnavailable>;

    /// The Codex sessions the index reports as recently active, with their workspaces.
    ///
    /// Bounded by recency on purpose. The index on the machine behind the mechanics
    /// document holds 691 rows against a 7 GB transcript store, of which one was active
    /// in the last six hours. Only sessions inside that window are opened, so the store
    /// is never scanned.
    ///
    /// Implementations read the **first record only** of each transcript, which is
    /// metadata. Conversation content is never read, stored, or displayed.
    fn codex_sessions(&self) -> Result<Vec<CodexSession>, WorldError>;

    /// The width available for output, in columns.
    ///
    /// Lives here because `world` is the only module permitted to touch the operating
    /// system, and because a fake can then pin it for deterministic render tests.
    fn output_width(&self) -> u16;

    /// The repository root containing a path, and whether that root is a linked worktree —
    /// established WITHOUT running a subprocess.
    ///
    /// Returns `(root, linked_worktree)` where `linked_worktree` is true iff the
    /// repository's `.git` is a file rather than a directory. Walking ancestors for a
    /// `.git` entry is a handful of `stat` calls, where `git rev-parse` is a process
    /// launch. This repo pays a measured re-authorisation tax on every exec, and the
    /// candidate set is one entry per observed process working directory — hundreds.
    ///
    /// `.git` being a FILE rather than a DIRECTORY is exactly what distinguishes a linked
    /// worktree, so the attribute falls out of the same stat that finds the root. No
    /// extra syscall is needed.
    ///
    /// Returns `None` when no ancestor has a `.git` entry. That is an answer ("not in a
    /// repository"), never an error.
    fn repository_root(&self, path: &str) -> Option<(String, bool)>;

    /// What version control says about a workspace, read without writing to it.
    ///
    /// This MUST NOT be able to mutate the repository being observed, because the
    /// repository may have a live agent working in it, and contending for its index
    /// would make the observer a participant. The implementation therefore uses
    /// `git --no-optional-locks` and disables filesystem monitors and automatic
    /// housekeeping — see the implementation for the full set of precautions.
    ///
    /// Returns `Err` when the state genuinely could not be determined — the path does not
    /// exist, there is no repository, or git refused to answer. `Ok` with
    /// `uncommitted_entries == 0` is clean; anything else is dirty.
    fn vcs_facts(&self, path: &str) -> Result<crate::vcs::VcsFacts, crate::vcs::Unreadable>;

    /// Which existing directory a recorded transcript namespace names.
    fn resolve_namespace(&self, namespace: &str) -> crate::workspace::NamespaceResolution;

    /// Find every git workspace at or below the given roots.
    fn sweep_for_repositories(&self, roots: &[String]) -> Sweep;

    /// Read version-control facts for many workspaces.
    ///
    /// Order matches `paths`. A **default implementation** loops over `vcs_facts`, so a fake
    /// World needs nothing; `RealWorld` overrides it to run the queries concurrently.
    fn vcs_facts_batch(
        &self,
        paths: &[String],
    ) -> Vec<Result<crate::vcs::VcsFacts, crate::vcs::Unreadable>> {
        paths.iter().map(|p| self.vcs_facts(p)).collect()
    }

    /// Read the state earlier runs left behind.
    ///
    /// The **default is a World with no memory at all**, which is what a fixture-driven fake
    /// is. It answers [`StateRead::Absent`] because that is true of it.
    fn read_state(&self) -> StateRead {
        StateRead::Absent
    }

    /// Replace the stored state with `contents`.
    ///
    /// Implementations MUST make the replacement atomic from a reader's point of view: a
    /// concurrent `acmon` — and running two is expected, since the point of the tool is to
    /// leave one open while working — must see either the whole previous state or the whole
    /// new one, never a half-written file. Writing in place would let a reader observe a
    /// truncated file, and a truncated state file parses as fewer remembered workspaces
    /// rather than as an error.
    ///
    /// The **default refuses**, rather than reporting a write that did not happen. A World
    /// with no state store is a legitimate thing; one that accepts state and silently drops
    /// it is the exact defect this project exists to remove, and the caller has to be able
    /// to tell a reader that the next run will start blind.
    fn write_state(&self, _contents: &str) -> Result<(), String> {
        Err("this World has no state store, so nothing was carried to the next run".to_string())
    }
}
