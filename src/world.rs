//! The only module permitted to describe the operating system.
//!
//! Everything else in the crate consumes [`World`], so the rest of the codebase is
//! pure and testable from captured fixtures.

/// One process, as observed in a single enumeration pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessRecord {
    pub pid: i32,
    /// `None` when the path could not be read.
    ///
    /// Never an empty string: an unreadable path must not be representable in the
    /// same way as a real one.
    pub exe_path: Option<String>,
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

/// Everything the collector needs from outside itself.
pub trait World {
    /// Enumerate all processes in a single pass.
    ///
    /// Identity and details must be gathered together. Enumerating first and
    /// enriching afterwards reports processes that merely exited in between as
    /// having unreadable fields, which is indistinguishable from a genuine failure
    /// to read a live process.
    fn process_snapshot(&self) -> Result<ProcessSnapshot, WorldError>;
}
