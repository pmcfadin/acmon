//! One writer, enforced by the kernel.
//!
//! F18, F19 and decision 22: `amon watch` is the sole writer of every state artefact, and
//! that is a mechanism rather than a sentence in a README. Two monitors on one state
//! directory interleave their writes and duplicate their alerts, and neither symptom shows
//! from outside — the file still parses and the notifications still arrive, twice, describing
//! two different passes.
//!
//! So a second `amon watch` is refused, by `flock` on `<state dir>/watch.lock`, and told which
//! pid holds it. `flock` was chosen over a pid file with a liveness check because the kernel
//! releases it when the holder dies, however it dies: a `SIGKILL`ed monitor leaves nothing
//! that needs cleaning up, so the successor is never left arguing with a file about whether a
//! process still exists.
//!
//! The pid file is still there, but only as a *record*, not as the lock:
//!
//! - while a holder is live, the file names it, so a refusal can say who
//! - a clean release clears it, so a normal restart is not reported as a takeover
//! - a holder that died without releasing leaves its pid behind, and the successor reports
//!   taking the lock over from it. Never in silence: "a monitor died here" and "I started
//!   normally" are different facts about this machine
//!
//! The file is never unlinked. Unlinking on release is the classic form of this bug — a
//! process already waiting on the old inode locks something no longer reachable by name while
//! a third creates a new file, and both then believe they hold the lock.
//!
//! One consequence worth knowing before #27 starts running `git` and `lsof` under this lock: a
//! child forked from the holder inherits a copy of the descriptor, and a copy of the descriptor
//! keeps the lock alive. `std` opens files `CLOEXEC`, so the copy dies at the child's `exec`
//! and a collector subprocess cannot strand the lock — but that is a property of how the file
//! is opened, not an accident, and a future `Command` built with a pre-exec hook would break it.

use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

/// The lock file, in the state directory it protects.
pub const LOCK_FILE: &str = "watch.lock";

/// The holder that left the lock behind without releasing it.
///
/// Its liveness is reported alongside the pid because the two cases differ: a pid that is gone
/// is a monitor that died, while a pid still running that does not hold the lock is stranger
/// still — a recycled pid, or something that wrote the file without taking the lock — and the
/// reader needs to be able to tell them apart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Predecessor {
    pub pid: u32,
    pub still_running: bool,
}

/// Why the lock was not taken.
///
/// Never a bare "already running". The reader has to be able to look the holder up, and a
/// message that names no pid and no path leaves them with nothing to do next.
#[derive(Debug)]
pub enum LockRefusal {
    /// Held, and the holder is named.
    HeldBy { pid: u32, path: PathBuf },
    /// Held, but the lock file did not yield a pid — a holder mid-exit, or a file someone
    /// truncated. Reported as such, with the reason, rather than dressed up as either a named
    /// holder or a free lock.
    HeldByUnnamed { path: PathBuf, reason: String },
    /// The lock could not be attempted at all: the directory or the file was unusable.
    Unavailable { path: PathBuf, reason: String },
}

impl LockRefusal {
    /// The pid holding the lock, when it could be read.
    pub fn holder_pid(&self) -> Option<u32> {
        match self {
            LockRefusal::HeldBy { pid, .. } => Some(*pid),
            LockRefusal::HeldByUnnamed { .. } | LockRefusal::Unavailable { .. } => None,
        }
    }
}

impl fmt::Display for LockRefusal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LockRefusal::HeldBy { pid, path } => write!(
                formatter,
                "pid {pid} holds the state lock {}, so it is already the writer. \
                 Two writers interleave state and duplicate alerts, so this one is not \
                 starting. Use `amon status` and the log to watch the one that is running.",
                path.display()
            ),
            LockRefusal::HeldByUnnamed { path, reason } => write!(
                formatter,
                "something holds the state lock {}, but its pid could not be read from the \
                 file ({reason}), so this refusal cannot name it. The lock is held either \
                 way, so this instance is not starting.",
                path.display()
            ),
            LockRefusal::Unavailable { path, reason } => write!(
                formatter,
                "the state lock {} could not be taken: {reason}",
                path.display()
            ),
        }
    }
}

impl std::error::Error for LockRefusal {}

/// An exclusive hold on a state directory, released when this value is dropped.
///
/// Held for the writer's whole lifetime. There is deliberately no way to obtain one without
/// the kernel agreeing: `acquire` is the only constructor, so "the writer holds the lock" is a
/// thing a caller can require rather than remember.
#[derive(Debug)]
pub struct WatchLock {
    /// The open file description the lock belongs to. Closing it releases the lock, which is
    /// what makes a killed holder harmless.
    file: File,
    path: PathBuf,
    holder: u32,
    predecessor: Option<Predecessor>,
    unreadable_record: Option<String>,
}

impl WatchLock {
    /// Take the lock, or say who has it.
    ///
    /// Creates the state directory if it is not there — a first run, or a run after someone
    /// deleted the directory to recover, both of which are supported.
    pub fn acquire(state_dir: &Path) -> Result<WatchLock, LockRefusal> {
        let path = state_dir.join(LOCK_FILE);

        std::fs::create_dir_all(state_dir).map_err(|error| LockRefusal::Unavailable {
            path: path.clone(),
            reason: format!(
                "the state directory {} could not be created: {error}",
                state_dir.display()
            ),
        })?;

        // Never `truncate`: the pid already in the file is the record a refusal or a takeover
        // report is built from, and destroying it before knowing whether we hold the lock
        // would erase the name of a monitor that died.
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|error| LockRefusal::Unavailable {
                path: path.clone(),
                reason: format!("the lock file could not be opened: {error}"),
            })?;

        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::EWOULDBLOCK) {
                return Err(LockRefusal::Unavailable {
                    path,
                    reason: format!("flock failed: {error}"),
                });
            }
            // Held. Say by whom, and if the file will not say, say that instead of guessing.
            return Err(match recorded_pid(&mut file) {
                Ok(Some(pid)) => LockRefusal::HeldBy { pid, path },
                Ok(None) => LockRefusal::HeldByUnnamed {
                    path,
                    reason: "the lock file is empty; the holder may be exiting".to_string(),
                },
                Err(reason) => LockRefusal::HeldByUnnamed { path, reason },
            });
        }

        // We hold it. Whatever the file says was left by someone who did not release.
        let (predecessor, unreadable_record) = match recorded_pid(&mut file) {
            Ok(Some(pid)) => (
                Some(Predecessor {
                    pid,
                    still_running: crate::real_world::process_exists(pid as libc::pid_t),
                }),
                None,
            ),
            Ok(None) => (None, None),
            Err(reason) => (None, Some(reason)),
        };

        let holder = std::process::id();
        record_pid(&mut file, holder).map_err(|reason| LockRefusal::Unavailable {
            path: path.clone(),
            reason,
        })?;

        Ok(WatchLock {
            file,
            path,
            holder,
            predecessor,
            unreadable_record,
        })
    }

    /// The pid recorded in the lock file: this process.
    pub fn holder_pid(&self) -> u32 {
        self.holder
    }

    /// The lock file itself, so a caller can name it in a message.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The holder that left this lock behind without releasing it, if there was one.
    ///
    /// `None` after a predecessor's clean exit, which clears the record. That distinction is
    /// the point: were a normal restart to report a takeover, it would describe a crash that
    /// never happened.
    pub fn took_over_from(&self) -> Option<&Predecessor> {
        self.predecessor.as_ref()
    }

    /// Why the previous record could not be read, when there was one and it made no sense.
    ///
    /// Kept separate from [`WatchLock::took_over_from`] because there is no pid to report, and
    /// silently treating an unreadable record as an absent one would be the fail-to-zero this
    /// project exists to eliminate.
    pub fn unreadable_record(&self) -> Option<&str> {
        self.unreadable_record.as_deref()
    }

    /// Release the lock on a clean exit, clearing the record of this holder.
    ///
    /// Clearing is what makes the next start a start rather than a takeover. A holder that
    /// vanishes without calling this leaves its pid behind on purpose — that is what a crash
    /// looks like, and the successor reports it.
    pub fn release(mut self) -> Result<(), String> {
        clear_record(&mut self.file)?;

        if unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) } != 0 {
            return Err(format!(
                "the state lock {} could not be released: {}",
                self.path.display(),
                std::io::Error::last_os_error()
            ));
        }

        Ok(())
    }
}

/// The pid the lock file records, if any.
///
/// `Ok(None)` for an empty file — no holder has recorded itself, or the last one cleared it on
/// the way out. `Err` for contents that are not a pid, which is a different thing and must not
/// read as absence.
fn recorded_pid(file: &mut File) -> Result<Option<u32>, String> {
    file.rewind()
        .map_err(|error| format!("the lock file could not be rewound: {error}"))?;

    let mut contents = String::new();
    file.read_to_string(&mut contents)
        .map_err(|error| format!("the lock file could not be read: {error}"))?;

    let trimmed = contents.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }

    trimmed.parse::<u32>().map(Some).map_err(|_| {
        format!("the lock file contains {trimmed:?}, which is not a pid this tool wrote")
    })
}

fn record_pid(file: &mut File, pid: u32) -> Result<(), String> {
    truncate(file)?;
    // Trailing newline so `cat watch.lock` reads cleanly: a human checking who holds the lock
    // is the whole reason the pid is on disk in decimal.
    writeln!(file, "{pid}").map_err(|error| format!("the pid could not be recorded: {error}"))?;
    file.flush()
        .map_err(|error| format!("the recorded pid could not be flushed: {error}"))
}

fn clear_record(file: &mut File) -> Result<(), String> {
    truncate(file)?;
    file.flush()
        .map_err(|error| format!("the cleared lock file could not be flushed: {error}"))
}

fn truncate(file: &mut File) -> Result<(), String> {
    file.set_len(0)
        .map_err(|error| format!("the lock file could not be truncated: {error}"))?;
    file.seek(SeekFrom::Start(0))
        .map(|_| ())
        .map_err(|error| format!("the lock file could not be rewound: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_refusal_naming_a_pid_carries_it_in_the_message() {
        let refusal = LockRefusal::HeldBy {
            pid: 4242,
            path: PathBuf::from("/tmp/acmon/watch.lock"),
        };

        let message = refusal.to_string();
        assert!(message.contains("4242"), "{message}");
        assert!(message.contains("/tmp/acmon/watch.lock"), "{message}");
        assert_eq!(refusal.holder_pid(), Some(4242));
    }

    #[test]
    fn contents_that_are_not_a_pid_are_an_error_rather_than_an_absent_record() {
        let path = std::env::temp_dir().join(format!("acmon-lock-unit-{}", std::process::id()));
        std::fs::write(&path, "the monitor was here").expect("write");

        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .expect("open");

        let reason = recorded_pid(&mut file).expect_err("not a pid");
        assert!(
            reason.contains("not a pid"),
            "the reason must say what was wrong; got {reason}"
        );

        let _ = std::fs::remove_file(&path);
    }
}
