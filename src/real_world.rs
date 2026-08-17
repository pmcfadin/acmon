//! The real [`World`] — the only code in the crate that touches the operating system.

use std::process::Command;
use std::time::Duration;

use libproc::pid_rusage::{pidrusage, RUsageInfoV4};
use libproc::proc_pid;
use libproc::processes::{pids_by_type, ProcFilter};

use crate::machtime::{MachTicks, MachTimebase};
use crate::world::{
    ExePathUnavailable, ProcessRecord, ProcessSnapshot, ResourceSource, Resources,
    ResourcesUnavailable, Unmeasured, World, WorldError,
};

pub struct RealWorld {
    observer_pid: i32,
    /// Read once from this machine, never assumed. Every duration in the kernel's
    /// ledger is a tick count that means nothing without it.
    timebase: MachTimebase,
}

impl RealWorld {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        RealWorld {
            observer_pid: std::process::id() as i32,
            timebase: read_timebase(),
        }
    }
}

/// `mach_timebase_info_data_t` from `<mach/mach_time.h>`.
///
/// Declared here rather than taken from `libc`, whose copy is deprecated in favour of
/// an additional dependency. Two `u32`s do not justify one.
#[repr(C)]
struct MachTimebaseInfo {
    numer: u32,
    denom: u32,
}

extern "C" {
    fn mach_timebase_info(info: *mut MachTimebaseInfo) -> libc::c_int;
}

/// Ask the kernel how long a mach tick is.
///
/// # Panics
///
/// If `mach_timebase_info()` fails. It reports a fixed per-machine constant and does
/// not fail in practice; if it ever did, every duration this tool prints would be wrong
/// by a factor it could not know, so stopping loudly is the only honest option.
fn read_timebase() -> MachTimebase {
    let mut info = MachTimebaseInfo { numer: 0, denom: 0 };
    let rc = unsafe { mach_timebase_info(&mut info) };
    assert!(
        rc == 0 && info.denom != 0,
        "mach_timebase_info() failed (rc {rc}, numer {}, denom {}); without it every \
         duration read from the kernel is a raw tick count of unknown length",
        info.numer,
        info.denom
    );
    MachTimebase::new(info.numer, info.denom)
}

/// Whether a pid still refers to a live process.
///
/// Signal 0 performs the permission and existence checks without delivering
/// anything, so this observes without disturbing.
fn process_exists(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 || *libc::__error() == libc::EPERM }
}

/// The fallback reader, for a live process the full ledger will not report on.
///
/// `ps(1)` reports cumulative own CPU and resident size without elevated privileges,
/// and nothing else — in particular it cannot see children at all, which is the whole
/// reason it is the fallback and not the primary. See
/// `docs/observability-mechanics.md` §2.7.
///
/// This costs one process execution, which on a machine with a synchronous
/// Endpoint Security stack is not free (§6). It is therefore only reached when the
/// cheap reader has already refused.
fn coarse_resources(pid: i32) -> Result<Resources, String> {
    let output = Command::new("/bin/ps")
        .args(["-o", "time=", "-o", "rss=", "-p", &pid.to_string()])
        .output()
        .map_err(|e| format!("could not run ps: {e}"))?;

    // Assert success before believing anything that came back. A failed run still
    // produces parseable-looking empty output.
    if !output.status.success() {
        return Err(format!("ps exited with {}", output.status));
    }

    let text = String::from_utf8_lossy(&output.stdout);
    let mut fields = text.split_whitespace();
    let cpu_field = fields.next().ok_or("ps reported no CPU time field")?;
    let rss_field = fields.next().ok_or("ps reported no resident size field")?;

    let own_cpu = parse_ps_cpu_time(cpu_field)?;
    let resident_bytes = rss_field
        .parse::<u64>()
        .map_err(|e| format!("ps resident size {rss_field:?} is not a number: {e}"))?
        * 1024;

    Ok(Resources {
        source: ResourceSource::Ps,
        own_cpu: Ok(own_cpu),
        children_cpu: Err(Unmeasured::NotReportedBy(ResourceSource::Ps)),
        current_memory: Ok(resident_bytes),
        peak_memory: Err(Unmeasured::NotReportedBy(ResourceSource::Ps)),
        bytes_written: Err(Unmeasured::NotReportedBy(ResourceSource::Ps)),
    })
}

/// Parse the CPU time `ps` prints: `[[DD-]HH:]MM:SS.CC`.
///
/// The minutes field is not capped at 60 on BSD `ps`, so `123:45.67` is a valid two
/// hours. Both shapes are accepted; anything else is an error rather than a zero.
fn parse_ps_cpu_time(field: &str) -> Result<Duration, String> {
    let malformed = || format!("ps CPU time {field:?} is not in [[DD-]HH:]MM:SS.CC form");

    let (days, clock) = match field.split_once('-') {
        Some((days, rest)) => (days.parse::<u64>().map_err(|_| malformed())?, rest),
        None => (0, field),
    };

    let parts: Vec<&str> = clock.split(':').collect();
    let (hours, minutes, seconds) = match parts.as_slice() {
        [h, m, s] => (*h, *m, *s),
        [m, s] => ("0", *m, *s),
        _ => return Err(malformed()),
    };

    let hours: u64 = hours.parse().map_err(|_| malformed())?;
    let minutes: u64 = minutes.parse().map_err(|_| malformed())?;
    let seconds: f64 = seconds.parse().map_err(|_| malformed())?;

    let whole = (days * 86_400) + (hours * 3_600) + (minutes * 60);
    Duration::try_from_secs_f64(seconds)
        .map(|fractional| Duration::from_secs(whole) + fractional)
        .map_err(|_| malformed())
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

    fn resources(&self, pid: i32) -> Result<Resources, ResourcesUnavailable> {
        match pidrusage::<RUsageInfoV4>(pid) {
            Ok(ledger) => Ok(Resources {
                source: ResourceSource::Rusage,
                // saturating: the sums are tick counts and cannot realistically
                // overflow, but a wrap would turn a huge total into a tiny one.
                own_cpu: Ok(self.timebase.duration(MachTicks(
                    ledger.ri_user_time.saturating_add(ledger.ri_system_time),
                ))),
                children_cpu: Ok(self.timebase.duration(MachTicks(
                    ledger
                        .ri_child_user_time
                        .saturating_add(ledger.ri_child_system_time),
                ))),
                current_memory: Ok(ledger.ri_phys_footprint),
                peak_memory: Ok(ledger.ri_lifetime_max_phys_footprint),
                bytes_written: Ok(ledger.ri_diskio_byteswritten),
            }),
            Err(ledger_error) => {
                // Establish which it is rather than guessing, exactly as for the
                // executable path: a pid can exit between being listed and being read,
                // and a process owned by another user is refused while very much alive.
                if !process_exists(pid) {
                    return Err(ResourcesUnavailable::ProcessExited);
                }
                coarse_resources(pid).map_err(|ps_error| {
                    ResourcesUnavailable::AllReadersFailed(format!(
                        "proc_pid_rusage: {ledger_error}; ps: {ps_error}"
                    ))
                })
            }
        }
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

/// Unit tests for the fallback reader's parsing, kept private.
///
/// The rest of the crate's tests live in `tests/` at agreed seams. This one is here
/// because the alternative — making a string parser public purely so an integration
/// test can reach it — would widen the API to serve the tests. An unparsed `ps` field
/// silently becoming the wrong number is exactly the class of defect this project
/// exists to eliminate, so it is tested where it lives.
#[cfg(test)]
mod tests {
    use super::parse_ps_cpu_time;

    /// Expected values are worked out by hand, not recomputed the way the parser does.
    /// The first two inputs are real `ps` output captured from this machine.
    #[test]
    fn parses_the_shapes_ps_actually_prints() {
        let cases = [
            ("0:00.01", 0.01),         // this shell, observed
            ("9:55.12", 595.12),       // launchd, observed
            ("48:41.60", 2_921.60),    // WindowServer, observed
            ("123:45.67", 7_425.67),   // minutes are not capped at 60 by BSD ps
            ("1:02:03.45", 3_723.45),  // the hours form
            ("2-03:04:05", 183_845.0), // the days form
        ];

        for (input, expected_secs) in cases {
            let parsed = parse_ps_cpu_time(input).expect(input);
            assert!(
                (parsed.as_secs_f64() - expected_secs).abs() < 0.001,
                "{input:?} should be {expected_secs} s, parsed as {parsed:?}"
            );
        }
    }

    #[test]
    fn refuses_anything_it_does_not_understand() {
        // Never a zero. A CPU time of zero for a busy process is the exact failure
        // mode this project was built to remove.
        for input in ["", "abc", "1:2:3:4", "-", "12", "1:xx"] {
            assert!(
                parse_ps_cpu_time(input).is_err(),
                "{input:?} must be an error, not a duration"
            );
        }
    }
}
