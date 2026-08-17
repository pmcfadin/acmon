//! The only module permitted to describe the operating system.
//!
//! Everything else in the crate consumes [`World`], so the rest of the codebase is
//! pure and testable from captured fixtures.

/// Why an executable path could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExePathUnavailable {
    /// The process exited between enumeration and detail read.
    ProcessExited,
    /// The path could not be read, likely due to permissions.
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
    /// Processes that exit during enumeration will have
    /// `exe_path = Err(ExePathUnavailable::PermissionDenied)` and are not
    /// distinguishable from processes whose paths cannot be read due to permissions.
    fn process_snapshot(&self) -> Result<ProcessSnapshot, WorldError>;
}
