//! The real [`World`] — the only code in the crate that touches the operating system.

use libproc::proc_pid;
use libproc::processes::{pids_by_type, ProcFilter};

use crate::world::{ExePathUnavailable, ProcessRecord, ProcessSnapshot, World, WorldError};

pub struct RealWorld {
    observer_pid: i32,
}

impl RealWorld {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        RealWorld {
            observer_pid: std::process::id() as i32,
        }
    }
}

/// Whether a pid still refers to a live process.
///
/// Signal 0 performs the permission and existence checks without delivering
/// anything, so this observes without disturbing.
fn process_exists(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 || *libc::__error() == libc::EPERM }
}

impl World for RealWorld {
    fn process_snapshot(&self) -> Result<ProcessSnapshot, WorldError> {
        let pids = pids_by_type(ProcFilter::All).map_err(|e| {
            WorldError::ProcessEnumeration(format!("libproc::pids_by_type failed: {}", e))
        })?;

        let records = pids
            .into_iter()
            .map(|pid| {
                let pid = pid as i32;
                // There is no single call returning every path, so this is a list
                // followed by a read, and a pid can exit in between. Rather than
                // assume a cause, ask: if the process is gone the reason is exit, and
                // if it is still there the reason is that we may not read it. One
                // extra syscall, only on the failure path, in exchange for a reason
                // that is true rather than merely plausible.
                let exe_path = match proc_pid::pidpath(pid) {
                    Ok(path) if !path.is_empty() => Ok(path),
                    _ if process_exists(pid) => Err(ExePathUnavailable::PermissionDenied),
                    _ => Err(ExePathUnavailable::ProcessExited),
                };
                ProcessRecord { pid, exe_path }
            })
            .collect();

        Ok(ProcessSnapshot {
            records,
            observer_pid: self.observer_pid,
        })
    }

    fn output_width(&self) -> u16 {
        const FALLBACK: u16 = 80;
        let mut size: libc::winsize = unsafe { std::mem::zeroed() };
        let rc = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut size) };
        // Not a terminal (piped, redirected, or under a harness): fall back rather
        // than render a zero-width table.
        if rc == 0 && size.ws_col > 0 {
            size.ws_col
        } else {
            FALLBACK
        }
    }
}
