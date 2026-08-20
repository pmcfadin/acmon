//! The real [`World`] — the only code in the crate that touches the operating system.

use std::process::Command;
use std::time::{Duration, SystemTime};

use libproc::pid_rusage::{pidrusage, RUsageInfoV4};
use libproc::proc_pid;
use libproc::processes::{pids_by_type, ProcFilter};

use crate::deliver::{self, DeliveryReport};
use crate::isotime::unix_seconds_from_iso8601;
use crate::machtime::{MachTicks, MachTimebase};
use crate::world::{
    CodexSession, NotifyConfig, NotifyOutcome, PathUnavailable, ProcessRecord, ProcessSnapshot,
    ResourceSource, Resources, ResourcesUnavailable, StateRead, Unmeasured, World, WorldError,
};

/// How far below a sweep root the descent goes.
///
/// Four, because this project's own agent workflows put linked worktrees at
/// `<repo>/.claude/worktrees/<name>`, which is four levels below the directory the
/// repositories sit in.
const SWEEP_MAX_DEPTH: usize = 4;

/// The most directories one sweep may visit before giving up.
///
/// Public so a test can exhaust it by name rather than by hard-coding a number that would
/// silently stop exercising the bound the day the bound changed.
pub const SWEEP_BUDGET: usize = 4096;

/// The environment variable that relocates the state file.
///
/// Its main job is to let a test drive the real read-and-write path against a temporary
/// directory. A test that had to write to the developer's own `~/.acmon/state.json` would
/// either destroy real history or be skipped, and a skipped test of an atomic write is how
/// a non-atomic write ships.
pub const STATE_VARIABLE: &str = "ACMON_STATE";

/// The environment variable that relocates the notification config file.
///
/// Lets tests configure notifications without touching the developer's real config.
pub const NOTIFY_CONFIG_VARIABLE: &str = "ACMON_NOTIFY_CONFIG";

/// The environment variable that relocates the detector config file.
///
/// Lets tests configure detectors without touching the developer's real config.
pub const DETECTORS_VARIABLE: &str = "ACMON_DETECTORS";

/// Where the state carried between runs is kept.
///
/// `~/.acmon/state.json`, alongside the `~/.claude` and `~/.codex` directories the agents
/// themselves use — findable and deletable by hand, which matters for a file whose contents
/// change what the tool reports.
fn state_path() -> Result<std::path::PathBuf, String> {
    if let Ok(explicit) = std::env::var(STATE_VARIABLE) {
        if !explicit.trim().is_empty() {
            return Ok(std::path::PathBuf::from(explicit));
        }
    }
    let home = std::env::var("HOME").map_err(|e| {
        format!(
            "HOME is not readable, so {STATE_VARIABLE} \
             must name the state file explicitly: {e}"
        )
    })?;
    Ok(std::path::Path::new(&home)
        .join(".acmon")
        .join("state.json"))
}

/// Where the notification configuration is kept.
///
/// `~/.acmon/notify.toml`, alongside the state file. Resolved as a `Result` for the same
/// reason [`state_path`] is: without a home directory there is no answer, and a stand-in path
/// chosen because it is certain to fail would report "no alerting configured" — which is a
/// legitimate state, and therefore the worst possible disguise for a fault.
fn notify_config_path() -> Result<std::path::PathBuf, String> {
    if let Ok(explicit) = std::env::var(NOTIFY_CONFIG_VARIABLE) {
        if !explicit.trim().is_empty() {
            return Ok(std::path::PathBuf::from(explicit));
        }
    }
    let home = std::env::var("HOME").map_err(|e| {
        format!(
            "HOME is not readable, so {NOTIFY_CONFIG_VARIABLE} must name the notification \
             configuration explicitly: {e}"
        )
    })?;
    Ok(std::path::Path::new(&home)
        .join(".acmon")
        .join("notify.toml"))
}

/// Where the detector configuration is kept.
///
/// `~/.acmon/detectors.toml`, alongside the state file and notification config. Resolved as a
/// `Result` for the same reason [`state_path`] is: without a home directory there is no answer,
/// and a stand-in path chosen because it is certain to fail would report "no detector config" —
/// which is a legitimate state, and therefore the worst possible disguise for a fault.
fn detectors_path() -> Result<std::path::PathBuf, String> {
    if let Ok(explicit) = std::env::var(DETECTORS_VARIABLE) {
        if !explicit.trim().is_empty() {
            return Ok(std::path::PathBuf::from(explicit));
        }
    }
    let home = std::env::var("HOME").map_err(|e| {
        format!(
            "HOME is not readable, so {DETECTORS_VARIABLE} must name the detector \
             configuration explicitly: {e}"
        )
    })?;
    Ok(std::path::Path::new(&home)
        .join(".acmon")
        .join("detectors.toml"))
}

pub struct RealWorld {
    observer_pid: i32,
    /// Read once from this machine, never assumed. Every duration in the kernel's
    /// ledger is a tick count that means nothing without it.
    timebase: MachTimebase,
    /// Where the state carried between runs is kept, or why that could not be worked out.
    ///
    /// Resolved once, at construction, rather than each time it is used. A path that is a
    /// *field* is a path a test can point somewhere harmless — where one read from the
    /// environment at the point of use would have made every test that collects write to the
    /// developer's own `~/.acmon/state.json`, and a test suite that quietly overwrites the
    /// history the tool depends on is worse than one that skips the case.
    state_file: Result<std::path::PathBuf, String>,
    /// Where the notification configuration is kept, or why that could not be worked out.
    ///
    /// Resolved once at construction for the same reason as `state_file`.
    notify_config_file: Result<std::path::PathBuf, String>,
    /// Where the detector configuration is kept, or why that could not be worked out.
    ///
    /// Resolved once at construction for the same reason as `state_file`.
    detectors_file: Result<std::path::PathBuf, String>,
    /// How long one notification delivery gets, and — the same figure — how long a whole
    /// channel's deliveries get in one run.
    ///
    /// A field rather than a constant for the same reason the paths above are: the tests that
    /// prove a hanging channel is reported rather than waited on would otherwise each cost
    /// [`crate::deliver::REQUEST_BUDGET`], and a suite nobody runs proves nothing.
    notify_request_budget: Duration,
}

impl RealWorld {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        RealWorld {
            observer_pid: std::process::id() as i32,
            timebase: read_timebase(),
            state_file: state_path(),
            notify_config_file: notify_config_path(),
            detectors_file: detectors_path(),
            notify_request_budget: crate::deliver::REQUEST_BUDGET,
        }
    }

    /// The same world, keeping its state in a named file instead of the usual one.
    pub fn with_state_file(path: impl Into<std::path::PathBuf>) -> Self {
        RealWorld {
            state_file: Ok(path.into()),
            ..RealWorld::new()
        }
    }

    /// The same world, reading notification config from a named file.
    pub fn with_notify_config(path: impl Into<std::path::PathBuf>) -> Self {
        RealWorld {
            notify_config_file: Ok(path.into()),
            ..RealWorld::new()
        }
    }

    /// The same world, reading detector config from a named file.
    pub fn with_detectors(path: impl Into<std::path::PathBuf>) -> Self {
        RealWorld {
            detectors_file: Ok(path.into()),
            ..RealWorld::new()
        }
    }

    /// The same world, giving each notification — and each run's alerting step — less time.
    ///
    /// For tests about what happens when a channel will not answer. At the real budget of
    /// [`crate::deliver::REQUEST_BUDGET`] every such test would sit for ten seconds a case.
    pub fn with_notify_request_budget(budget: Duration) -> Self {
        RealWorld {
            notify_request_budget: budget,
            ..RealWorld::new()
        }
    }

    /// What one run's deliveries to one channel are allowed to spend.
    ///
    /// [`deliver::CONCURRENCY`] in flight, and the whole batch held to a single request's
    /// budget — so a dead endpoint costs one timeout per run instead of one per alert, and the
    /// alerts the budget did not reach are reported as not attempted rather than dropped.
    fn notify_bounds(&self) -> deliver::Bounds {
        deliver::Bounds {
            workers: deliver::CONCURRENCY,
            budget: self.notify_request_budget,
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
///
/// Public because the single-writer lock needs the same question answered about the pid a dead
/// monitor left in its lock file, and two copies of a `kill(pid, 0)` would be two places for
/// the "the tool observes, it never acts" rule to be got wrong.
pub fn process_exists(pid: i32) -> bool {
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

/// The ids the Codex index reports as active within the recency window, with their
/// last-activity timestamps.
///
/// Only `id` and `updated_at` are deserialised. The index also carries a thread name,
/// which is user-supplied text and is deliberately never read into this program.
fn recently_active_codex_ids(
    index: &std::path::Path,
    now: i64,
) -> Result<Vec<(String, SystemTime)>, WorldError> {
    #[derive(serde::Deserialize)]
    struct IndexRow {
        id: String,
        updated_at: String,
    }

    let text = std::fs::read_to_string(index)
        .map_err(|e| WorldError::CodexIndex(format!("{}: {e}", index.display())))?;

    let mut sessions = Vec::new();
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
            let last_activity = std::time::UNIX_EPOCH
                + Duration::from_secs(updated.try_into().map_err(|_| {
                    WorldError::CodexIndex(format!(
                        "{} line {}: timestamp {} is negative",
                        index.display(),
                        number + 1,
                        updated
                    ))
                })?);
            sessions.push((row.id, last_activity));
        }
    }
    Ok(sessions)
}

/// Find the transcript file for each wanted session, reading directory entries only.
///
/// No transcript is opened here, so the store's contents are never scanned — but be
/// precise about what bounds the walk, because it is not the recency window. The walk
/// stops when every wanted id has been found, and descends the newest date directory
/// first, so a session updated recently is normally found in the first directory or two.
/// The pathological case is a session created long ago and updated today: its file lives
/// in its *creation* date's directory (§4.4), so reaching it means descending past the
/// newer ones. An id with no file at all is the worst case, since nothing can satisfy it
/// and the remaining directories are all visited; that is bounded by the number of date
/// directories, and such an id is simply absent from the result rather than an error,
/// because the index outlives the transcripts it points at.
fn locate_codex_transcripts(
    sessions: &std::path::Path,
    wanted: &[(String, SystemTime)],
) -> Result<Vec<(String, std::path::PathBuf, SystemTime)>, WorldError> {
    let mut found: Vec<(String, std::path::PathBuf, SystemTime)> = Vec::new();
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
            if let Some((id, last_activity)) =
                wanted.iter().find(|(id, _)| name.contains(id.as_str()))
            {
                if !found.iter().any(|(already, _, _)| already == id) {
                    found.push((id.clone(), entry.path(), *last_activity));
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
/// Exactly one line is read from the file, and [`workspace_from_first_record`] takes
/// exactly one field from it.
fn read_codex_workspace(path: &std::path::Path) -> Result<String, String> {
    use std::io::BufRead;

    let file = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut first_line = String::new();
    std::io::BufReader::new(file)
        .read_line(&mut first_line)
        .map_err(|e| format!("{}: {e}", path.display()))?;

    workspace_from_first_record(&first_line).map_err(|e| format!("{}: {e}", path.display()))
}

/// Take the workspace out of a Codex transcript's first record.
///
/// The record type is checked first: if this is not the metadata record then something is
/// wrong with the file, and the answer is an error rather than looking further into a file
/// whose later records hold conversation.
///
/// **On what is unavoidably read.** The first record is a single JSON object, and one of
/// its sibling fields is `base_instructions` — the model's system prompt. Reaching `cwd`
/// means streaming past that field, because JSON has no index and, as observed, the
/// fields are ordered alphabetically so `base_instructions` comes first. What *is*
/// controlled: only `cwd` and `type` are deserialised, so nothing else is ever
/// materialised as a value; the record is dropped at the end of this function; and no
/// error path can quote a field this does not deserialise, which is asserted by a test
/// rather than merely intended. Conversation records — the turns themselves — are never
/// read at all, since only the first line is ever fetched.
fn workspace_from_first_record(line: &str) -> Result<String, String> {
    #[derive(serde::Deserialize)]
    struct FirstRecord {
        r#type: String,
        payload: Payload,
    }
    #[derive(serde::Deserialize)]
    struct Payload {
        cwd: String,
    }

    let record: FirstRecord =
        serde_json::from_str(line).map_err(|e| format!("first record is not readable: {e}"))?;
    if record.r#type != "session_meta" {
        return Err(format!(
            "first record is {:?}, not session_meta",
            record.r#type
        ));
    }
    if record.payload.cwd.is_empty() {
        return Err("session_meta records no cwd".to_string());
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
            .map(|(id, path, last_activity)| {
                read_codex_workspace(&path)
                    .map(|workspace| CodexSession {
                        id,
                        workspace,
                        last_activity,
                    })
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

    fn namespace_activity(
        &self,
        namespace: &str,
    ) -> Result<SystemTime, crate::world::ActivityUnavailable> {
        use crate::world::ActivityUnavailable;

        let home = std::env::var("HOME")
            .map_err(|e| ActivityUnavailable::Unreadable(format!("HOME is not readable: {e}")))?;
        let namespace_dir = std::path::Path::new(&home)
            .join(".claude")
            .join("projects")
            .join(namespace);

        // Check if the directory exists at all. Distinct from unreadable: this is an
        // answer.
        if !namespace_dir.exists() {
            return Err(ActivityUnavailable::NotRecorded);
        }

        let entries = std::fs::read_dir(&namespace_dir).map_err(|e| {
            ActivityUnavailable::Unreadable(format!("{}: {e}", namespace_dir.display()))
        })?;

        // Find the most recent modification time among all .jsonl files. The directory's
        // own mtime is not used: appending to a file inside a directory does not update
        // the directory's mtime — only creating, renaming or deleting an entry does. A
        // session that has been appending to one transcript for hours would look
        // untouched if we used the directory mtime.
        let mut most_recent: Option<SystemTime> = None;
        for entry in entries {
            let entry = entry.map_err(|e| {
                ActivityUnavailable::Unreadable(format!("{}: {e}", namespace_dir.display()))
            })?;
            let name = entry.file_name();
            if name.to_string_lossy().ends_with(".jsonl") {
                let metadata = entry.metadata().map_err(|e| {
                    ActivityUnavailable::Unreadable(format!(
                        "{}/{}: {e}",
                        namespace_dir.display(),
                        name.to_string_lossy()
                    ))
                })?;
                let modified = metadata.modified().map_err(|e| {
                    ActivityUnavailable::Unreadable(format!(
                        "{}/{}: modification time unavailable: {e}",
                        namespace_dir.display(),
                        name.to_string_lossy()
                    ))
                })?;
                most_recent = Some(match most_recent {
                    None => modified,
                    Some(prev) => prev.max(modified),
                });
            }
        }

        most_recent.ok_or(ActivityUnavailable::NoTranscripts)
    }

    fn output_width(&self) -> u16 {
        // The width used when stdout is not a terminal.
        //
        // Deliberately NOT 80. A pipe, a file, or a test harness imposes no width at all, so
        // an unmeasurable width is not evidence of a narrow one. This previously fell back to
        // 80, and because the table needs 88, `acmon | less` and `acmon > file` printed a
        // refusal to widen a terminal that was not there — an unmeasured value standing in
        // for a measured constraint, which is the failure mode AGENTS.md forbids.
        const NOT_A_TERMINAL: u16 = 120;

        let mut size: libc::winsize = unsafe { std::mem::zeroed() };
        let rc = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut size) };
        if rc == 0 && size.ws_col > 0 {
            // A real terminal answered. Its width is a genuine constraint, and if it is too
            // narrow the caller gets a refusal rather than a truncated number.
            return size.ws_col;
        }

        // No terminal. Honour `COLUMNS` if the caller set it, since that is an explicit
        // statement of intent, and otherwise use a width the output actually fits in.
        std::env::var("COLUMNS")
            .ok()
            .and_then(|value| value.trim().parse::<u16>().ok())
            .filter(|width| *width > 0)
            .unwrap_or(NOT_A_TERMINAL)
    }

    fn repository_root(&self, path: &str) -> Option<(String, bool)> {
        use std::path::Path;

        // Walk up the directory tree looking for a `.git` entry. Each step is a stat,
        // not a process launch — which matters because this repo pays a measured
        // re-authorisation tax on every exec, and this will be called once per observed
        // process working directory (hundreds).
        let mut current = Path::new(path);
        loop {
            let git_path = current.join(".git");

            // Check if .git exists and whether it is a file or directory. A file means
            // this is a linked worktree; a directory means it is a primary repository.
            // The distinction is observable with the same stat that checks existence, so
            // it costs nothing extra.
            if let Ok(metadata) = std::fs::metadata(&git_path) {
                let linked_worktree = metadata.is_file();
                return Some((current.to_string_lossy().into_owned(), linked_worktree));
            }

            // Move to the parent. If there is no parent, we have walked to the root
            // without finding a repository.
            current = current.parent()?;
        }
    }

    fn vcs_facts(&self, path: &str) -> Result<crate::vcs::VcsFacts, crate::vcs::Unreadable> {
        use crate::vcs::{Unreadable, VcsFacts};

        // Check if the path exists before attempting anything else.
        if !std::path::Path::new(path).exists() {
            return Err(Unreadable::PathGone);
        }

        // Find the repository root. If there is none, this is not a versioned directory.
        let (root, linked_worktree) = self
            .repository_root(path)
            .ok_or(Unreadable::NotVersionControlled)?;

        // Query git for the status. Every flag is load-bearing — see the inline comments.
        // The query is run against the ROOT, not the path itself, because a process
        // working in a subdirectory is still working in the same repository.
        let mut child = Command::new("git")
            // --no-optional-locks: Stops git refreshing and rewriting the index. Without
            // this flag, a status query can take a lock the agent working in this
            // repository needs, making the observer a participant. This is the mechanical
            // enforcement of the "MUST NOT mutate" contract in the trait doc.
            .arg("--no-optional-locks")
            // -c core.fsmonitor=false: Stops git STARTING a filesystem-monitor daemon.
            // Launching a daemon into the observed repository would make this tool act
            // rather than observe, which violates the "the tool observes; it never acts"
            // rule from AGENTS.md.
            .arg("-c")
            .arg("core.fsmonitor=false")
            // -c gc.auto=0: Belt and braces against any housekeeping write. Auto-gc can
            // trigger on certain operations; disabling it ensures the query is
            // read-only.
            .arg("-c")
            .arg("gc.auto=0")
            .arg("-C")
            .arg(&root)
            .arg("status")
            .arg("--porcelain")
            // --untracked-files=normal: Untracked files ARE uncommitted work. The
            // workspace whose loss motivated this project held files git had never seen,
            // so "uncommitted" explicitly includes them.
            .arg("--untracked-files=normal")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| Unreadable::QueryFailed(format!("could not spawn git: {e}")))?;

        // Enforce a timeout: poll try_wait() with short sleeps, and kill the child if
        // it exceeds the budget. Killing our own git child is not a breach of "the tool
        // observes; it never acts" — that rule protects agent sessions, and an unbounded
        // query would hang the live display that ticket #10 builds.
        const VCS_QUERY_BUDGET: Duration = Duration::from_secs(5);
        let started = std::time::Instant::now();
        let poll_interval = Duration::from_millis(50);

        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    // Process exited. Check the exit status BEFORE believing the output —
                    // AGENTS.md rule: "Assert success before believing a measurement."
                    // A failed git still produces parseable-looking empty output, and
                    // reading that as "clean" is exactly the fail-to-zero this project
                    // exists to eliminate.
                    if !status.success() {
                        let mut stderr = Vec::new();
                        if let Some(mut pipe) = child.stderr.take() {
                            let _ = std::io::Read::read_to_end(&mut pipe, &mut stderr);
                        }
                        let error = String::from_utf8_lossy(&stderr).trim().to_string();
                        return Err(Unreadable::QueryFailed(error));
                    }

                    // Status succeeded. Count the non-blank lines in stdout — each one is
                    // an uncommitted entry.
                    let mut stdout = Vec::new();
                    if let Some(mut pipe) = child.stdout.take() {
                        let _ = std::io::Read::read_to_end(&mut pipe, &mut stdout);
                    }
                    let output = String::from_utf8_lossy(&stdout);
                    let uncommitted_entries = output
                        .lines()
                        .filter(|line| !line.trim().is_empty())
                        .count();

                    return Ok(VcsFacts {
                        root,
                        uncommitted_entries,
                        linked_worktree,
                    });
                }
                Ok(None) => {
                    // Still running. Check if we have exceeded the budget.
                    if started.elapsed() > VCS_QUERY_BUDGET {
                        // Timeout. Kill the child and wait for it to exit, so we do not
                        // leave it running.
                        let _ = child.kill();
                        let _ = child.wait();
                        return Err(Unreadable::TimedOut);
                    }
                    // Still within budget. Sleep briefly and poll again.
                    std::thread::sleep(poll_interval);
                }
                Err(e) => {
                    // try_wait failed, which is unusual. Kill and report.
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(Unreadable::QueryFailed(format!("try_wait failed: {e}")));
                }
            }
        }
    }

    fn resolve_namespace(&self, namespace: &str) -> crate::workspace::NamespaceResolution {
        use crate::workspace;

        // Supply a real directory lister to the pure resolution function. The lister
        // returns only sub-directory names and skips symlinks.
        //
        // Why skip symlinks: `/tmp` is a symlink to `/private/tmp` on macOS. Following
        // links can produce cycles, and the kernel reports resolved paths anyway, so a
        // symlinked route is never the path a transcript recorded. Calling `is_dir()` on
        // `DirEntry::file_type()` does not follow links on Unix, so testing `is_dir()`
        // naturally excludes them.
        let lister = |path: &str| -> Option<Vec<String>> {
            let entries = std::fs::read_dir(path).ok()?;
            let mut directories = Vec::new();
            for entry in entries {
                let Ok(entry) = entry else { continue };
                if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    directories.push(entry.file_name().to_string_lossy().into_owned());
                }
            }
            Some(directories)
        };

        workspace::resolve_namespace(namespace, &lister)
    }

    fn sweep_for_repositories(&self, roots: &[String]) -> crate::world::Sweep {
        use crate::world::Sweep;
        use std::collections::HashSet;

        let mut repositories: Vec<(String, bool)> = Vec::new();
        let mut directories_visited = 0;
        let mut complete = true;

        // Phase 1 — bounded descent.
        //
        // Walk down from each root to SWEEP_MAX_DEPTH. A directory containing a `.git`
        // entry is a workspace: record it and do not descend into it. Skip symlinks for
        // the same reason `resolve_namespace` does — following them can produce cycles.
        // Count every directory visited; on exceeding SWEEP_BUDGET, stop.
        //
        // Measured on the target machine, sweeping `~/projects`: 68 workspaces from 122
        // directories visited, in under 10 ms. The pruning is what makes it cheap — the
        // same sweep without pruning visits 18,146 directories and takes 809 ms to find
        // only 4 more.
        fn descend(
            path: &str,
            depth: usize,
            max_depth: usize,
            repositories: &mut Vec<(String, bool)>,
            directories_visited: &mut usize,
            budget: usize,
        ) -> bool {
            if *directories_visited >= budget {
                return false; // Budget exhausted
            }
            if depth > max_depth {
                return true; // Max depth reached, but budget not exhausted
            }

            *directories_visited += 1;

            // Check for `.git` entry
            let git_path = std::path::Path::new(path).join(".git");
            if let Ok(metadata) = std::fs::metadata(&git_path) {
                let linked_worktree = metadata.is_file();
                repositories.push((path.to_string(), linked_worktree));
                return true; // Do not descend into a repository
            }

            // List children and descend
            let Ok(entries) = std::fs::read_dir(path) else {
                return true; // Unreadable directory kills only this branch
            };

            for entry in entries {
                let Ok(entry) = entry else { continue };
                // Skip symlinks — `file_type()` does not follow links on Unix
                if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    continue;
                }
                let child_path = entry.path().to_string_lossy().into_owned();
                if !descend(
                    &child_path,
                    depth + 1,
                    max_depth,
                    repositories,
                    directories_visited,
                    budget,
                ) {
                    return false; // Budget exhausted
                }
            }

            true
        }

        for root in roots {
            if !descend(
                root,
                0,
                SWEEP_MAX_DEPTH,
                &mut repositories,
                &mut directories_visited,
                SWEEP_BUDGET,
            ) {
                complete = false;
                break;
            }
        }

        // Phase 2 — read git's own worktree registry.
        //
        // Pruning at `.git` has one hole: a linked worktree can live inside another
        // repository's tree, and this project's agent workflows put them at
        // `<repo>/.claude/worktrees/<name>`, where phase 1 will never look. Closing that
        // hole needs no deep sweep and no subprocess, because git already keeps a registry:
        // for a primary repository, `<repo>/.git/worktrees/<name>/gitdir` is a file whose
        // contents are the path of that worktree's own `.git` file, so the worktree
        // directory is that path's parent.
        //
        // Measured: this recovers 2 worktrees phase 1 missed, and the whole two-phase
        // discovery still costs 9 ms. One of the two recovered paths does not exist — a
        // stale registration git never cleaned up — so a registered worktree must be
        // reported like any other candidate and left to `vcs_facts` to call `PathGone`. Do
        // not filter it out silently and do not error.
        let mut linked_worktrees = Vec::new();
        for (repo_root, is_linked) in &repositories {
            // Only check primary repositories (not linked worktrees)
            if *is_linked {
                continue;
            }

            let worktrees_dir = std::path::Path::new(repo_root)
                .join(".git")
                .join("worktrees");
            let Ok(entries) = std::fs::read_dir(&worktrees_dir) else {
                continue; // No worktrees directory, or unreadable
            };

            for entry in entries {
                let Ok(entry) = entry else { continue };
                if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    continue;
                }

                let gitdir_file = entry.path().join("gitdir");
                let Ok(gitdir_contents) = std::fs::read_to_string(&gitdir_file) else {
                    continue;
                };

                // The gitdir file contains the path to the worktree's `.git` file; the
                // worktree directory is its parent
                let gitdir_path = gitdir_contents.trim();
                if let Some(parent) = std::path::Path::new(gitdir_path).parent() {
                    let worktree_path = parent.to_string_lossy().into_owned();
                    linked_worktrees.push((worktree_path, true));
                }
            }
        }
        repositories.extend(linked_worktrees);

        // Deduplicate the combined result
        let mut seen = HashSet::new();
        repositories.retain(|(path, _)| seen.insert(path.clone()));

        Sweep {
            repositories,
            complete,
            directories_visited,
        }
    }

    fn vcs_facts_batch(
        &self,
        paths: &[String],
    ) -> Vec<Result<crate::vcs::VcsFacts, crate::vcs::Unreadable>> {
        use std::sync::Arc;

        // For 0 or 1 paths, just use the single-threaded implementation
        if paths.len() <= 1 {
            return paths.iter().map(|p| self.vcs_facts(p)).collect();
        }

        // Split paths into chunks, one thread per chunk, at most min(8, paths.len()) threads.
        //
        // Why concurrent: `git status` costs min 21 ms · median 83 ms · max 149 ms per
        // repository, and 70 workspaces sequentially is 5.0 seconds, which alone blows the
        // project's one-second fast-tier budget.
        let num_threads = std::cmp::min(8, paths.len());
        let chunk_size = paths.len().div_ceil(num_threads);

        // Use Arc to share self across threads (RealWorld must be Sync)
        let self_arc = Arc::new(self);
        let paths_arc = Arc::new(paths.to_vec());

        let mut results = vec![Err(crate::vcs::Unreadable::NotVersionControlled); paths.len()];

        std::thread::scope(|scope| {
            let mut handles = Vec::new();

            for chunk_idx in 0..num_threads {
                let start = chunk_idx * chunk_size;
                if start >= paths.len() {
                    break;
                }
                let end = std::cmp::min(start + chunk_size, paths.len());

                let self_clone = Arc::clone(&self_arc);
                let paths_clone = Arc::clone(&paths_arc);

                let handle = scope.spawn(move || {
                    let mut chunk_results = Vec::new();
                    for idx in start..end {
                        chunk_results.push(self_clone.vcs_facts(&paths_clone[idx]));
                    }
                    (start, chunk_results)
                });

                handles.push(handle);
            }

            // Collect results from threads in order
            for handle in handles {
                let (start, chunk_results) = handle.join().expect("thread should not panic");
                for (i, result) in chunk_results.into_iter().enumerate() {
                    results[start + i] = result;
                }
            }
        });

        results
    }

    fn read_state(&self) -> StateRead {
        let path = match &self.state_file {
            Ok(path) => path,
            Err(why) => return StateRead::Unreadable(why.clone()),
        };

        match std::fs::read_to_string(path) {
            Ok(contents) => StateRead::Found(contents),
            // Both of these mean nothing has been stored yet: no file, and no `~/.acmon`
            // for one to be in. Neither is a failure, and reporting them as one would put a
            // warning on every first run.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => StateRead::Absent,
            Err(error) => StateRead::Unreadable(format!("{}: {error}", path.display())),
        }
    }

    fn write_state(&self, contents: &str) -> Result<(), String> {
        use std::io::Write;

        let path = self.state_file.as_ref().map_err(String::clone)?;
        let directory = path
            .parent()
            .ok_or_else(|| format!("{} has no parent directory", path.display()))?;
        std::fs::create_dir_all(directory)
            .map_err(|e| format!("could not create {}: {e}", directory.display()))?;

        // Write beside the target, then rename over it. `rename(2)` within one directory is
        // atomic, so a concurrently reading acmon — and leaving one open while working is the
        // whole point of the tool — sees either the previous state entire or this one entire.
        // Writing in place would let it read a truncated file, and a truncated state file
        // does not fail to parse: it parses as FEWER remembered workspaces, which is a
        // shorter at-risk list that reads as a safer machine.
        //
        // The temporary name carries this process's pid so that two acmon runs writing at the
        // same moment do not each half-fill one temporary file and then rename the result.
        let temporary = path.with_extension(format!("new.{}", std::process::id()));

        let write = || -> Result<(), std::io::Error> {
            let mut file = std::fs::File::create(&temporary)?;
            file.write_all(contents.as_bytes())?;
            // Before the rename, not after: the rename is what publishes the file, and
            // publishing a name that points at unflushed data is the failure this ordering
            // exists to prevent.
            file.sync_all()?;
            std::fs::rename(&temporary, path)
        };

        write().map_err(|error| {
            // A failed attempt must not leave the temporary behind to accumulate one file
            // per run. Its own failure is not reported: the write error is the one that
            // matters, and a cleanup error stacked on top of it would bury the cause.
            let _ = std::fs::remove_file(&temporary);
            format!("could not store state in {}: {error}", path.display())
        })
    }

    fn read_notify_config(&self) -> NotifyConfig {
        let path = match &self.notify_config_file {
            Ok(path) => path,
            Err(why) => return NotifyConfig::unusable(why.clone()),
        };

        let contents = match std::fs::read_to_string(path) {
            Ok(text) => text,
            // No file is an answer: this machine has no alerting configured, which is
            // allowed. A file that exists and cannot be read is NOT the same thing, and
            // saying so is what stops a permissions mistake from reading as "no alerts
            // wanted".
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return NotifyConfig::none()
            }
            Err(error) => {
                return NotifyConfig::unusable(format!(
                    "{} could not be read: {error}",
                    path.display()
                ))
            }
        };

        #[derive(serde::Deserialize)]
        struct ConfigFile {
            local_command: Option<String>,
            remote_url: Option<String>,
        }

        // A malformed config delivers nothing, so it MUST carry its reason. Discarding the
        // parser's complaint here would leave a typo in `notify.toml` indistinguishable from
        // a machine that was never set up to alert — and the second of those is silent by
        // design, so the first would be silent by accident.
        let parsed: ConfigFile = match toml::from_str(&contents) {
            Ok(config) => config,
            Err(error) => {
                return NotifyConfig::unusable(format!(
                    "{} is not readable as configuration: {error}",
                    path.display()
                ))
            }
        };

        NotifyConfig {
            local_command: parsed.local_command.filter(|s| !s.trim().is_empty()),
            remote_url: parsed.remote_url.filter(|s| !s.trim().is_empty()),
            unusable: None,
        }
    }

    fn read_detector_config(&self) -> crate::world::DetectorConfig {
        use crate::world::DetectorConfig;

        let path = match &self.detectors_file {
            Ok(path) => path,
            Err(why) => return DetectorConfig::unusable(why.clone()),
        };

        let contents = match std::fs::read_to_string(path) {
            Ok(text) => text,
            // No file is an answer: this machine uses only the embedded detectors, which is
            // allowed and expected. A file that exists and cannot be read is NOT the same
            // thing, and saying so is what stops a permissions mistake from reading as "uses
            // defaults".
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return DetectorConfig::embedded_only()
            }
            Err(error) => {
                return DetectorConfig::unusable(format!(
                    "{} could not be read: {error}",
                    path.display()
                ))
            }
        };

        // Parse the user detectors. A malformed file or one with a toothless detector must
        // report the specific error and fall back to embedded defaults.
        let user_detectors = match crate::detect::parse_user_detectors(&contents) {
            Ok(detectors) => detectors,
            Err(why) => return DetectorConfig::unusable(format!("{}: {why}", path.display())),
        };

        // Layer the user detectors over the embedded ones.
        let embedded = crate::detect::embedded_detectors();
        let merged = crate::detect::merge_detectors(embedded, user_detectors);

        DetectorConfig {
            detectors: merged,
            unusable: None,
        }
    }

    fn notify_local(&self, command: &str, payload: &str) -> NotifyOutcome {
        // Run the command synchronously with the payload on stdin.
        let mut child = match Command::new("sh")
            .arg("-c")
            .arg(command)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            Ok(child) => child,
            Err(e) => return NotifyOutcome::Failed(format!("could not spawn: {e}")),
        };

        if let Some(mut stdin) = child.stdin.take() {
            use std::io::Write;
            let _ = stdin.write_all(payload.as_bytes());
        }

        // Bounded, the same way a version-control query is. `child.wait()` on its own is
        // unbounded, and a notifier that never exits — a GUI helper waiting on a dialog, a
        // command left in the config with a typo that makes it read stdin forever — would stop
        // the collection returning at all. Killing our own notifier child is not a breach of
        // "the tool observes; it never acts": that rule protects agent sessions.
        let started = std::time::Instant::now();
        let poll_interval = Duration::from_millis(20);

        loop {
            match child.try_wait() {
                Ok(Some(status)) if status.success() => return NotifyOutcome::Delivered,
                Ok(Some(status)) => return NotifyOutcome::Failed(format!("exited {}", status)),
                Ok(None) => {
                    if started.elapsed() >= self.notify_request_budget {
                        let _ = child.kill();
                        let _ = child.wait();
                        return NotifyOutcome::Failed(format!(
                            "the local command did not exit within {:?}, so nothing can be said \
                             to have been delivered",
                            self.notify_request_budget
                        ));
                    }
                    std::thread::sleep(poll_interval);
                }
                Err(e) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return NotifyOutcome::Failed(format!("wait failed: {e}"));
                }
            }
        }
    }

    fn notify_remote(&self, url: &str, payload: &str) -> NotifyOutcome {
        // Use curl synchronously. Check both process exit status and HTTP status code.
        //
        // `--max-time` carries the same budget the local channel polls against, expressed with
        // millisecond precision because a test budget under a second would otherwise round to
        // `0`, which curl reads as no limit at all — the one value that must never reach it.
        let max_time = format!("{:.3}", self.notify_request_budget.as_secs_f64().max(0.01));
        let output = match Command::new("curl")
            .arg("--fail") // Exit non-zero on HTTP 4xx/5xx
            .arg("--silent") // No progress meter
            .arg("--show-error") // But do show errors
            .arg("--max-time")
            .arg(&max_time)
            .arg("-X")
            .arg("POST")
            .arg("-H")
            .arg("Content-Type: application/json")
            .arg("--data")
            .arg(payload)
            .arg(url)
            .output()
        {
            Ok(output) => output,
            Err(e) => return NotifyOutcome::Failed(format!("could not spawn curl: {e}")),
        };

        if output.status.success() {
            NotifyOutcome::Delivered
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            NotifyOutcome::Failed(format!("curl exited {}: {}", output.status, stderr.trim()))
        }
    }

    fn notify_local_batch(&self, command: &str, payloads: &[String]) -> DeliveryReport {
        deliver::in_parallel(payloads, self.notify_bounds(), |payload| {
            self.notify_local(command, payload)
        })
    }

    fn notify_remote_batch(&self, url: &str, payloads: &[String]) -> DeliveryReport {
        deliver::in_parallel(payloads, self.notify_bounds(), |payload| {
            self.notify_remote(url, payload)
        })
    }
}

// Compile-time assertion that RealWorld is Sync, so the concurrent vcs_facts_batch works.
// If RealWorld ever gains a field with interior mutability that is not Sync, this will fail
// the build rather than silently serializing the batch.
const _: fn() = || {
    fn assert_sync<T: Sync>() {}
    assert_sync::<RealWorld>();
};

/// Unit tests for the fallback reader's parsing, kept private.
///
/// The rest of the crate's tests live in `tests/` at agreed seams. This one is here
/// because the alternative — making a string parser public purely so an integration
/// test can reach it — would widen the API to serve the tests. An unparsed `ps` field
/// silently becoming the wrong number is exactly the class of defect this project
/// exists to eliminate, so it is tested where it lives.
#[cfg(test)]
mod tests {
    use super::{parse_ps_cpu_time, workspace_from_first_record};

    /// A first record shaped like the real thing, including the sibling field that holds
    /// the model's system prompt. The marker text stands in for it.
    fn first_record_with(cwd: &str, secret: &str) -> String {
        format!(
            r#"{{"type":"session_meta","payload":{{"base_instructions":"{secret}",
               "cli_version":"1.2.3","cwd":{cwd},"originator":"cli"}},
               "timestamp":"2026-08-17T10:30:46Z"}}"#
        )
    }

    #[test]
    fn takes_the_workspace_and_leaves_everything_else() {
        let record = first_record_with(
            "\"/Users/pmcfadin/Documents/Codex/2026-08-17/he\"",
            "SENSITIVE-SYSTEM-PROMPT",
        );

        assert_eq!(
            workspace_from_first_record(&record),
            Ok("/Users/pmcfadin/Documents/Codex/2026-08-17/he".to_string())
        );
    }

    #[test]
    fn no_error_message_can_quote_a_field_this_does_not_deserialise() {
        // The sharp end of "no conversation content is read, stored, or displayed".
        // Reaching cwd means streaming past base_instructions, so the guarantee that
        // matters is that no failure path can ever echo it outward. serde's type errors
        // do quote offending values, so this is asserted rather than assumed.
        const SECRET: &str = "SENSITIVE-SYSTEM-PROMPT";
        let malformed = [
            // cwd of the wrong type: the error quotes the value it found.
            first_record_with("12345", SECRET),
            // cwd absent entirely.
            format!(r#"{{"type":"session_meta","payload":{{"base_instructions":"{SECRET}"}}}}"#),
            // not the metadata record at all.
            format!(r#"{{"type":"response_item","payload":{{"base_instructions":"{SECRET}"}}}}"#),
            // truncated JSON, as a half-written line would be.
            format!(r#"{{"type":"session_meta","payload":{{"base_instructions":"{SECRET}"#),
        ];

        for record in malformed {
            let error = workspace_from_first_record(&record)
                .expect_err("each of these records is unusable");
            assert!(
                !error.contains(SECRET),
                "an error message leaked a field that is never deserialised: {error}"
            );
        }
    }

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
