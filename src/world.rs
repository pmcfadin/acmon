//! The only module permitted to describe the operating system.
//!
//! Everything else in the crate consumes [`World`], so the rest of the codebase is
//! pure and testable from captured fixtures.

/// Why an executable path could not be read.
///
/// Each variant must be TRUE when reported. A reason that is merely plausible is
/// worse than no reason at all: it reads as a finding rather than as ignorance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExePathUnavailable {
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
    pub exe_path: Result<String, ExePathUnavailable>,
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
#[derive(Debug, Clone, PartialEq, Eq)]
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
            Unmeasured::NotReportedBy(ResourceSource::Rusage) => write!(f, "unreported"),
            Unmeasured::ProcessExited => write!(f, "exited"),
            Unmeasured::PermissionDenied => write!(f, "no-perm"),
        }
    }
}

/// One process's resource ledger, as far as it could be read.
///
/// Each figure is separately present-or-absent because the fallback reader supplies
/// only some of them. An absent figure carries a reason; none of them defaults to zero.
#[derive(Debug, Clone, PartialEq, Eq)]
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
            ResourcesUnavailable::AllReadersFailed(_) => write!(f, "unreadable"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorldError {
    /// The process table could not be enumerated at all.
    ProcessEnumeration(String),
}

impl std::fmt::Display for WorldError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WorldError::ProcessEnumeration(msg) => {
                write!(f, "process enumeration failed: {}", msg)
            }
        }
    }
}

impl std::error::Error for WorldError {}

/// Everything the collector needs from outside itself.
pub trait World {
    /// Enumerate all processes with their executable paths.
    ///
    /// Returns a snapshot of the process table at a single point in time. The
    /// implementation may perform multiple system calls to gather process details;
    /// the contract is that the result represents one logical observation, not that
    /// it's gathered in a single syscall.
    ///
    /// A process that exits mid-enumeration reports
    /// [`ExePathUnavailable::ProcessExited`] and one that is merely unreadable reports
    /// [`ExePathUnavailable::PermissionDenied`]. Implementations MUST establish which
    /// is true rather than assuming, because a confidently wrong reason is worse than
    /// an admitted unknown.
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

    /// The width available for output, in columns.
    ///
    /// Lives here because `world` is the only module permitted to touch the operating
    /// system, and because a fake can then pin it for deterministic render tests.
    fn output_width(&self) -> u16;
}
