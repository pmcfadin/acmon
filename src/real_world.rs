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

impl World for RealWorld {
    fn process_snapshot(&self) -> Result<ProcessSnapshot, WorldError> {
        let pids = pids_by_type(ProcFilter::All).map_err(|e| {
            WorldError::ProcessEnumeration(format!("libproc::pids_by_type failed: {}", e))
        })?;

        let records = pids
            .into_iter()
            .map(|pid| {
                let pid = pid as i32;
                // LIMITATION, stated rather than hidden. There is no single call that
                // returns every path, so this is a list followed by a read. A pid that
                // exits in between cannot be distinguished from one whose path we are
                // not permitted to read at the libproc API level, so both report
                // PermissionDenied. Neither becomes a session. That errs toward never
                // inventing a session, which is the safe direction. In practice every
                // same-user process is readable, and sessions are always same-user.
                let exe_path = match proc_pid::pidpath(pid) {
                    Ok(path) if !path.is_empty() => Ok(path),
                    _ => Err(ExePathUnavailable::PermissionDenied),
                };
                ProcessRecord { pid, exe_path }
            })
            .collect();

        Ok(ProcessSnapshot {
            records,
            observer_pid: self.observer_pid,
        })
    }
}
