//! The real [`World`] — the only code in the crate that touches the operating system.

use std::process::Command;
use std::time::{Duration, SystemTime};

use libproc::pid_rusage::{pidrusage, RUsageInfoV4};
use libproc::proc_pid;
use libproc::processes::{pids_by_type, ProcFilter};

use crate::isotime::unix_seconds_from_iso8601;
use crate::machtime::{MachTicks, MachTimebase};
use crate::world::{
    CodexSession, PathUnavailable, ProcessRecord, ProcessSnapshot, ResourceSource, Resources,
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

/// `PROC_PIDVNODEPATHINFO` from `<sys/proc_info.h>` line 741.
const PROC_PIDVNODEPATHINFO: libc::c_int = 9;

/// `MAXPATHLEN`, the size of the `vip_path` array.
const MAXPATHLEN: usize = 1024;

/// `size_of(struct vnode_info)` from `<sys/proc_info.h>` line 309: a `vinfo_stat`
/// (136 bytes) followed by two `int`s and an `fsid_t`.
const VNODE_INFO_SIZE: usize = 152;

/// `struct vnode_info_path` — a `vnode_info` followed by the path.
///
/// The leading struct is carried as opaque bytes because none of its twenty-odd fields
/// is wanted; only its *size* matters, since it sets the offset the path is read from.
#[repr(C)]
struct VnodeInfoPath {
    _vnode_info: [u8; VNODE_INFO_SIZE],
    /// `char vip_path[MAXPATHLEN]`, NUL-terminated.
    path: [libc::c_char; MAXPATHLEN],
}

/// `struct proc_vnodepathinfo` — the working directory, then the root directory.
#[repr(C)]
struct ProcVnodePathInfo {
    current_directory: VnodeInfoPath,
    _root_directory: VnodeInfoPath,
}

/// `PROC_PIDVNODEPATHINFO_SIZE` is `sizeof(struct proc_vnodepathinfo)` = 2 × (152 + 1024).
///
/// This only proves the Rust declaration is self-consistent with the arithmetic above;
/// it cannot prove the arithmetic matches the kernel. What proves that is the seam-3
/// test comparing a cwd read this way against the same process's own view of it — a
/// wrong offset there yields a plausible-looking path, not an obvious error.
const _: () = assert!(std::mem::size_of::<ProcVnodePathInfo>() == 2352);

extern "C" {
    fn proc_pidinfo(
        pid: libc::c_int,
        flavor: libc::c_int,
        arg: u64,
        buffer: *mut libc::c_void,
        buffersize: libc::c_int,
    ) -> libc::c_int;
}

/// Read a process's current working directory.
///
/// `libproc::proc_pid::pidcwd` exists but is a stub on macOS that always returns an
/// error, so the underlying call is made directly.
fn read_cwd(pid: i32) -> Result<String, PathUnavailable> {
    let mut info: ProcVnodePathInfo = unsafe { std::mem::zeroed() };
    let capacity = std::mem::size_of::<ProcVnodePathInfo>() as libc::c_int;
    let written = unsafe {
        proc_pidinfo(
            pid,
            PROC_PIDVNODEPATHINFO,
            0,
            (&mut info as *mut ProcVnodePathInfo).cast(),
            capacity,
        )
    };

    // Insist on a completely filled buffer. A partial fill would leave the path field
    // holding whatever the kernel did not write, which is indistinguishable from a real
    // answer once it is a string.
    if written == capacity {
        let path = unsafe { std::ffi::CStr::from_ptr(info.current_directory.path.as_ptr()) }
            .to_string_lossy()
            .into_owned();
        if !path.is_empty() {
            return Ok(path);
        }
    }

    if process_exists(pid) {
        Err(PathUnavailable::PermissionDenied)
    } else {
        Err(PathUnavailable::ProcessExited)
    }
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

/// How recently a Codex session must have been updated to be worth opening.
///
/// This is what keeps the transcript store from being scanned. On the machine behind the
/// mechanics document the index holds 691 rows and exactly one falls inside this window.
/// Generous rather than tight: the cost of including a stale session is one line read,
/// while excluding a live one would lose its workspace.
const CODEX_RECENCY_WINDOW_SECONDS: i64 = 6 * 3_600;

/// The ids the Codex index reports as active within the recency window.
///
/// Only `id` and `updated_at` are deserialised. The index also carries a thread name,
/// which is user-supplied text and is deliberately never read into this program.
fn recently_active_codex_ids(index: &std::path::Path, now: i64) -> Result<Vec<String>, WorldError> {
    #[derive(serde::Deserialize)]
    struct IndexRow {
        id: String,
        updated_at: String,
    }

    let text = std::fs::read_to_string(index)
        .map_err(|e| WorldError::CodexIndex(format!("{}: {e}", index.display())))?;

    let mut ids = Vec::new();
    for (number, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let row: IndexRow = serde_json::from_str(line).map_err(|e| {
            WorldError::CodexIndex(format!("{} line {}: {e}", index.display(), number + 1))
        })?;
        // A timestamp that cannot be read is an error, never a silent skip: skipping it
        // would drop a live session from the table for a reason nobody would see.
        let updated = unix_seconds_from_iso8601(&row.updated_at).map_err(|e| {
            WorldError::CodexIndex(format!("{} line {}: {e}", index.display(), number + 1))
        })?;
        if now - updated <= CODEX_RECENCY_WINDOW_SECONDS {
            ids.push(row.id);
        }
    }
    Ok(ids)
}

/// Find the transcript file for each wanted id, reading directory entries only.
///
/// Descends newest date directory first and stops as soon as every wanted id is found,
/// so in practice this touches one or two directories. An id with no file is simply
/// absent from the result: the index outlives the transcripts it points at, and that is
/// not an error.
fn locate_codex_transcripts(
    sessions: &std::path::Path,
    wanted: &[String],
) -> Result<Vec<(String, std::path::PathBuf)>, WorldError> {
    let mut found: Vec<(String, std::path::PathBuf)> = Vec::new();
    let mut pending = vec![sessions.to_path_buf()];

    while let Some(directory) = pending.pop() {
        if found.len() == wanted.len() {
            break;
        }
        let entries = std::fs::read_dir(&directory)
            .map_err(|e| WorldError::CodexIndex(format!("{}: {e}", directory.display())))?;

        let mut subdirectories = Vec::new();
        for entry in entries {
            let entry = entry
                .map_err(|e| WorldError::CodexIndex(format!("{}: {e}", directory.display())))?;
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                subdirectories.push(entry.path());
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if let Some(id) = wanted.iter().find(|id| name.contains(id.as_str())) {
                if !found.iter().any(|(already, _)| already == id) {
                    found.push((id.clone(), entry.path()));
                }
            }
        }
        // Names are YYYY, MM, DD, so sorting ascending and popping from the end visits
        // the most recent first — where a recently updated session most likely lives.
        subdirectories.sort();
        pending.extend(subdirectories);
    }
    Ok(found)
}

/// Read a Codex session's workspace from the first record of its transcript.
///
/// Exactly one line is read, and exactly one field is taken from it. The record type is
/// checked first: if the first line is not the metadata record, this is an error rather
/// than an attempt to look further into a file that holds conversation content.
fn read_codex_workspace(path: &std::path::Path) -> Result<String, String> {
    use std::io::BufRead;

    #[derive(serde::Deserialize)]
    struct FirstRecord {
        r#type: String,
        payload: Payload,
    }
    #[derive(serde::Deserialize)]
    struct Payload {
        cwd: String,
    }

    let file = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut first_line = String::new();
    std::io::BufReader::new(file)
        .read_line(&mut first_line)
        .map_err(|e| format!("{}: {e}", path.display()))?;

    let record: FirstRecord = serde_json::from_str(&first_line)
        .map_err(|e| format!("{}: first record is not readable: {e}", path.display()))?;
    if record.r#type != "session_meta" {
        return Err(format!(
            "{}: first record is {:?}, not session_meta",
            path.display(),
            record.r#type
        ));
    }
    if record.payload.cwd.is_empty() {
        return Err(format!("{}: session_meta records no cwd", path.display()));
    }
    Ok(record.payload.cwd)
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
                    _ if process_exists(pid) => Err(PathUnavailable::PermissionDenied),
                    _ => Err(PathUnavailable::ProcessExited),
                };
                ProcessRecord {
                    pid,
                    exe_path,
                    // Same pass, same moment: see the field's documentation.
                    cwd: read_cwd(pid),
                }
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

    fn codex_sessions(&self) -> Result<Vec<CodexSession>, WorldError> {
        let home = std::env::var("HOME")
            .map_err(|e| WorldError::CodexIndex(format!("HOME is not readable: {e}")))?;
        let codex = std::path::Path::new(&home).join(".codex");

        let now = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|e| WorldError::CodexIndex(format!("the system clock is before 1970: {e}")))?
            .as_secs() as i64;

        let recent = recently_active_codex_ids(&codex.join("session_index.jsonl"), now)?;
        if recent.is_empty() {
            return Ok(Vec::new());
        }

        // Locating by id, not by date: a session created weeks ago and updated today
        // still lives in its creation date's directory (§4.4), so the directory cannot be
        // derived from `updated_at`. Only filenames are read to find it.
        let located = locate_codex_transcripts(&codex.join("sessions"), &recent)?;

        located
            .into_iter()
            .map(|(id, path)| {
                read_codex_workspace(&path)
                    .map(|workspace| CodexSession { id, workspace })
                    .map_err(WorldError::CodexIndex)
            })
            .collect()
    }

    fn recorded_namespaces(&self) -> Result<Vec<String>, WorldError> {
        let home = std::env::var("HOME")
            .map_err(|e| WorldError::NamespaceListing(format!("HOME is not readable: {e}")))?;
        let root = std::path::Path::new(&home).join(".claude").join("projects");

        let entries = std::fs::read_dir(&root)
            .map_err(|e| WorldError::NamespaceListing(format!("{}: {e}", root.display())))?;

        let mut names = Vec::new();
        for entry in entries {
            let entry = entry
                .map_err(|e| WorldError::NamespaceListing(format!("{}: {e}", root.display())))?;
            // Directory names only. The transcripts inside hold conversation content and
            // are never opened.
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                // Lossy rather than skipped: a name that is not valid UTF-8 still has to
                // appear, because a namespace missing from this list reads downstream as
                // a workspace with no transcript. A working directory read from the
                // kernel is converted the same way, so the two still meet.
                names.push(entry.file_name().to_string_lossy().into_owned());
            }
        }
        Ok(names)
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
