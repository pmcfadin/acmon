//! The LaunchAgent: alerts that fire with no terminal open.
//!
//! F24, G6, NF16, S11 and decisions 28 and 31. This is what makes "full time" true rather than
//! "whenever I remember to leave the pane open", and it is the reason the two-binary split
//! exists at all — a process that draws a screen cannot alert you when there is no screen.
//!
//! Three verbs live here, and one risk from the PRD governs all of them: **`amon install` fails
//! or the job never loads, and the failure looks like success.** Silent non-monitoring is the
//! worst outcome this product has, because nothing about it is visible: no error, no screen, and
//! no alert that anybody was expecting at a particular time. So:
//!
//! - the plist path is stated **before** the file is created, because a reader has to be able to
//!   go and look at the one file this tool writes outside its own directories;
//! - the load is **verified by asking launchd**, never inferred from `bootstrap` exiting zero;
//! - a load that could not be confirmed **undoes its own plist**, because a plist with no job is
//!   a machine that is unmonitored today and monitored after the next login, and nothing on disk
//!   says which;
//! - anything that could not be undone is **named**, so a half-install is a sentence a human can
//!   act on rather than a state they have to discover.
//!
//! ## What this writes, and where
//!
//! One file: `~/Library/LaunchAgents/io.github.pmcfadin.acmon.plist`. That is the **only** path
//! in the whole product written outside `~/.config/acmon/` and `~/.local/state/acmon/` (F24), and
//! `amon --help` says so where somebody installing would see it. `install` also creates the
//! state directory, which is inside the boundary and is required before the job is loaded:
//! launchd creates the file it is told to write the monitor's output to, but not the directory
//! above it, and a job whose `StandardErrorPath` cannot be opened does not start.
//!
//! ## No supervision but launchd's
//!
//! `KeepAlive` is the entire supervision story (decision 31, N7). There is deliberately no
//! watchdog for the watchdog: a second job watching the first can die exactly as quietly, and
//! then there are two silent failures instead of one. So exactly one job is installed, and gaps
//! are made *visible* instead — launchd's own run count and last exit code are reported by
//! `amon status` today, and the durable record that outlives them, with downtime and whether the
//! last exit was clean, is #28.
//!
//! Nothing here signals, restarts or kills anything. `KeepAlive` restarting `amon` is launchd
//! supervising the monitor; `bootout` during `uninstall` is a person asking for the job they
//! installed to be removed. Neither is this tool acting on an agent.
//!
//! ## No `sudo`, ever
//!
//! NF16. Everything targets `gui/<uid>` — the installing user's own GUI domain — which needs no
//! elevation. A tool that asks for root once has taught its user to give it, after which every
//! later mistake is a root mistake. `bootstrap`/`bootout` rather than the deprecated
//! `load -w`/`unload`, because the modern subcommands report failures instead of exiting zero
//! having done nothing, which is the same distinction this whole module is about.

use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

use crate::cli::{AmonVerb, VerbState};
use crate::state::{StateStore, STATE_FILE};

/// The label launchd knows the job by, and the stem of the plist's file name.
///
/// Reverse DNS of a domain the project actually controls (`pmcfadin.github.io`), rather than an
/// invented one. One label, because there is exactly one job — see N7 above.
pub const LABEL: &str = "io.github.pmcfadin.acmon";

/// Where the monitor's own stdout and stderr go: inside the state directory, so F24's claim
/// about the plist being the only outside write stays true of the files launchd opens too.
pub const LOG_FILE: &str = "amon.log";

/// The floor launchd puts under respawns, in seconds.
///
/// Stated rather than inherited even though it is launchd's default, because it is the *period
/// of a crash loop*: a monitor that cannot stay up produces one launch record every
/// `THROTTLE_SECONDS`, and #28 reads that cadence.
pub const THROTTLE_SECONDS: u32 = 10;

/// The `PATH` the job is given.
///
/// Measured, not assumed: `launchctl print` on a real LaunchAgent reports its default
/// environment as `PATH => /usr/bin:/bin:/usr/sbin:/sbin`. The collectors invoke `git` and
/// `curl` by name, so a monitor left with that default would fail to read every workspace whose
/// `git` is Homebrew's — a whole class of at-risk workspace missing from the panel, on a machine
/// where the same command works in a terminal.
pub const JOB_PATH: &str = "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin";

/// The environment variable that relocates the LaunchAgents directory.
///
/// A test seam of the same family as `ACMON_STATE_DIR`, and a hard requirement rather than a
/// convenience: no test in this crate may write to the developer's real
/// `~/Library/LaunchAgents`, because that is a durable change to a live machine which no
/// `git checkout` can undo.
pub const LAUNCH_AGENTS_VARIABLE: &str = "ACMON_LAUNCH_AGENTS_DIR";

/// The environment variable that replaces `launchctl` itself.
///
/// The only way to exercise `install` and `uninstall` end to end without registering a job with
/// somebody's real login session. Because a run that talked to something other than launchd must
/// never read as a real install, every verb that uses it **says so, loudly, in its output**.
pub const LAUNCHCTL_VARIABLE: &str = "ACMON_LAUNCHCTL";

/// launchd's own front end.
pub const LAUNCHCTL: &str = "/bin/launchctl";

/// The variables an installed job is given, when they are set where `install` ran.
///
/// Split-brain is the failure this prevents: `amon` writing one state directory while the
/// `agtop` in the user's terminal reads another, both of them healthy and disagreeing about the
/// machine. `HOME` is included because every path this tool resolves falls back to it and fails
/// loudly without it — launchd does set it for a LaunchAgent, and stating it costs nothing and
/// removes a dependency on that staying true.
///
/// A variable nobody set is **not** given a value here. Writing a default into the plist would
/// turn an absent setting into a decision the installer never made.
pub const PROPAGATED_VARIABLES: &[&str] = &[
    "HOME",
    crate::state::CONFIG_DIR_VARIABLE,
    crate::state::STATE_DIR_VARIABLE,
    crate::real_world::STATE_VARIABLE,
    crate::real_world::NOTIFY_CONFIG_VARIABLE,
    crate::real_world::DETECTORS_VARIABLE,
    crate::liveness::QUIET_THRESHOLD_VARIABLE,
    crate::liveness::STALL_THRESHOLD_VARIABLE,
    crate::memory::FORGET_VARIABLE,
];

/// The variables to carry into the job, read through the given lookup.
///
/// The lookup is a parameter so this is testable without mutating a process-wide environment
/// every other test in the same binary shares.
pub fn propagated_environment(lookup: &dyn Fn(&str) -> Option<String>) -> Vec<(String, String)> {
    PROPAGATED_VARIABLES
        .iter()
        .filter_map(|name| {
            lookup(name)
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
                .map(|value| ((*name).to_string(), value))
        })
        .collect()
}

// --- The job, and the plist that describes it ---

/// The one job this tool installs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Job {
    pub label: String,
    /// Absolute path to the `amon` binary launchd will run.
    pub program: PathBuf,
    /// The verbs and flags after it. `watch`, and nothing else.
    pub arguments: Vec<String>,
    /// Where launchd sends the monitor's stdout and stderr.
    pub log: PathBuf,
    /// Environment the job is given, over and above [`JOB_PATH`].
    pub environment: Vec<(String, String)>,
}

impl Job {
    /// The monitor under launchd: `amon watch`, logging into the state directory.
    ///
    /// `watch` rather than anything else because the monitor is the half that can alert with no
    /// terminal open. `agtop` under launchd would be a display drawing to nobody.
    pub fn monitor(program: &Path, state_dir: &Path, environment: Vec<(String, String)>) -> Job {
        Job {
            label: LABEL.to_string(),
            program: program.to_path_buf(),
            arguments: vec![AmonVerb::Watch.name().to_string()],
            log: state_dir.join(LOG_FILE),
            environment,
        }
    }

    /// What the job runs, as a reader would type it.
    pub fn command_line(&self) -> String {
        let mut line = self.program.display().to_string();
        for argument in &self.arguments {
            line.push(' ');
            line.push_str(argument);
        }
        line
    }

    /// The plist launchd reads.
    ///
    /// Hand-rendered rather than built with a plist library, because the whole file is nine keys
    /// and a dependency here would have to be justified against NF12's "no runtime
    /// dependencies". What is *not* hand-checked is whether the result parses: seam 15 runs the
    /// system's own `plutil` over it, since a plist that reads correctly to a human and fails to
    /// parse is exactly the install that looks fine and never loads.
    ///
    /// No `ProcessType`, deliberately. `Background` invites launchd to throttle the job, and this
    /// is a tool whose numbers are the product — a monitor being throttled would misreport both
    /// its own duty cycle and everything it sampled while suspended.
    pub fn plist(&self) -> String {
        let mut text = String::new();
        text.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
        text.push_str(
            "<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
             \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n",
        );
        text.push_str("<plist version=\"1.0\">\n<dict>\n");
        text.push_str(
            "\t<!-- Written by `amon install`. Do not hand-edit: `amon uninstall` removes it, \
             and `amon status` reports what launchd thinks of it. -->\n",
        );

        text.push_str(&format!(
            "\t<key>Label</key>\n\t<string>{}</string>\n",
            escape(&self.label)
        ));

        text.push_str("\t<key>ProgramArguments</key>\n\t<array>\n");
        text.push_str(&format!(
            "\t\t<string>{}</string>\n",
            escape(&self.program.display().to_string())
        ));
        for argument in &self.arguments {
            text.push_str(&format!("\t\t<string>{}</string>\n", escape(argument)));
        }
        text.push_str("\t</array>\n");

        // RunAtLoad: a monitor that waits to be asked is not resident. KeepAlive: without it, an
        // unclean exit is a permanent silence with nothing on screen to say so.
        text.push_str("\t<key>RunAtLoad</key>\n\t<true/>\n");
        text.push_str("\t<key>KeepAlive</key>\n\t<true/>\n");
        text.push_str(&format!(
            "\t<key>ThrottleInterval</key>\n\t<integer>{THROTTLE_SECONDS}</integer>\n"
        ));

        let log = escape(&self.log.display().to_string());
        text.push_str(&format!(
            "\t<key>StandardOutPath</key>\n\t<string>{log}</string>\n"
        ));
        text.push_str(&format!(
            "\t<key>StandardErrorPath</key>\n\t<string>{log}</string>\n"
        ));

        text.push_str("\t<key>EnvironmentVariables</key>\n\t<dict>\n");
        text.push_str(&format!(
            "\t\t<key>PATH</key>\n\t\t<string>{}</string>\n",
            escape(JOB_PATH)
        ));
        for (name, value) in self.environment.iter().filter(|(name, _)| name != "PATH") {
            text.push_str(&format!(
                "\t\t<key>{}</key>\n\t\t<string>{}</string>\n",
                escape(name),
                escape(value)
            ));
        }
        text.push_str("\t</dict>\n");

        text.push_str("</dict>\n</plist>\n");
        text
    }
}

/// XML-escape a value going into the plist.
///
/// One unescaped `&` in a home directory's name and launchd rejects the whole file — an install
/// that cannot work on a machine whose paths are legal.
fn escape(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&apos;"),
            other => escaped.push(other),
        }
    }
    escaped
}

// --- Asking launchd ---

/// What launchd says about the job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobQuery {
    /// launchd has the job registered, and reports these facts about it.
    Loaded(JobFacts),
    /// launchd does not have the job. A determinate answer, not a failure to get one.
    NotLoaded,
    /// launchd could not be asked, or answered something this build cannot read. Never
    /// collapsed into `NotLoaded`: "I could not tell" and "it is not installed" send a reader in
    /// opposite directions.
    Undetermined(String),
}

/// What `launchctl print` reports about a job it has.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct JobFacts {
    /// From `state = running` / `state = not running`. `None` when launchd printed neither,
    /// because a renamed field must not read as a monitor that had stopped.
    pub running: Option<bool>,
    /// launchd's pid for the job, absent when it is not currently running.
    pub pid: Option<u32>,
    /// How many times launchd has started it. The closest thing to a restart record that exists
    /// before #28: a monitor started thirty-seven times is cycling.
    pub runs: Option<u32>,
    /// launchd's `last exit code`, verbatim — it is `(never exited)` for a job still on its
    /// first run, which is a different fact from `0`.
    pub last_exit: Option<String>,
}

/// Read `launchctl print`'s output.
///
/// Field names and shapes taken from real output on this machine rather than from
/// documentation — `launchctl print gui/501/<label>` for both a running and a registered-but-
/// stopped agent. The load-bearing detail is that a job launchd is *not* running has no `pid =`
/// line at all, so the `state` line is what is read; inferring "not running" from a missing pid
/// would be right by luck there and wrong on the running case.
pub fn parse_print(output: &str) -> JobFacts {
    let mut facts = JobFacts::default();

    for line in output.lines() {
        let trimmed = line.trim();
        let Some((key, value)) = trimmed.split_once(" = ") else {
            continue;
        };
        let value = value.trim();
        match key.trim() {
            "state" if facts.running.is_none() => {
                facts.running = match value {
                    "running" => Some(true),
                    "not running" => Some(false),
                    // Anything else — including the nested `state = active` of a job's
                    // endpoints — leaves this undetermined rather than guessing.
                    _ => None,
                };
            }
            "pid" if facts.pid.is_none() => facts.pid = value.parse().ok(),
            "runs" if facts.runs.is_none() => facts.runs = value.parse().ok(),
            "last exit code" if facts.last_exit.is_none() => {
                facts.last_exit = Some(value.to_string())
            }
            _ => {}
        }
    }

    facts
}

/// The domain a per-user LaunchAgent belongs to. Never `system/`, which needs root (NF16).
pub fn domain_target(uid: u32) -> String {
    format!("gui/{uid}")
}

/// The job inside that domain.
pub fn service_target(uid: u32, label: &str) -> String {
    format!("{}/{label}", domain_target(uid))
}

pub fn bootstrap_arguments(uid: u32, plist: &Path) -> Vec<String> {
    vec![
        "bootstrap".to_string(),
        domain_target(uid),
        plist.display().to_string(),
    ]
}

pub fn bootout_arguments(uid: u32, label: &str) -> Vec<String> {
    vec!["bootout".to_string(), service_target(uid, label)]
}

pub fn print_arguments(uid: u32, label: &str) -> Vec<String> {
    vec!["print".to_string(), service_target(uid, label)]
}

/// Everything this tool asks of launchd.
///
/// A trait so that the decisions around a load — what is written, what is asked, what is
/// believed, and what is left behind — can be tested without registering a job with a real login
/// session. That is not a convenience: it is the difference between a suite that can run on a
/// developer's machine and one that changes it.
pub trait Launchctl {
    /// Register the job described by this plist with the user's own session.
    fn bootstrap(&self, plist: &Path) -> Result<(), String>;
    /// Unregister the job by label.
    fn bootout(&self, label: &str) -> Result<(), String>;
    /// What launchd currently says about the job.
    fn query(&self, label: &str) -> JobQuery;
    /// How launchd is being reached, for a notice. Never guessed at, and never omitted when it
    /// is not launchd.
    fn describes_itself_as(&self) -> String;
}

/// launchd itself, through `launchctl`.
#[derive(Debug, Clone)]
pub struct SystemLaunchctl {
    program: PathBuf,
    uid: u32,
    overridden: bool,
}

impl SystemLaunchctl {
    /// This machine's launchd, honouring [`LAUNCHCTL_VARIABLE`].
    pub fn from_environment() -> SystemLaunchctl {
        SystemLaunchctl::from_values(std::env::var(LAUNCHCTL_VARIABLE).ok().as_deref(), unsafe {
            libc::getuid()
        }
            as u32)
    }

    /// The same, with the environment passed in.
    pub fn from_values(program: Option<&str>, uid: u32) -> SystemLaunchctl {
        match program.map(str::trim).filter(|value| !value.is_empty()) {
            Some(explicit) => SystemLaunchctl {
                program: PathBuf::from(explicit),
                uid,
                overridden: true,
            },
            None => SystemLaunchctl {
                program: PathBuf::from(LAUNCHCTL),
                uid,
                overridden: false,
            },
        }
    }

    /// The stand-in in use, when one is. `None` means this really is launchd.
    pub fn override_in_use(&self) -> Option<&Path> {
        if self.overridden {
            Some(&self.program)
        } else {
            None
        }
    }

    fn run(&self, arguments: &[String]) -> Result<(bool, String, String), String> {
        let output = Command::new(&self.program)
            .args(arguments)
            .output()
            .map_err(|error| {
                format!(
                    "{} {} could not be run: {error}",
                    self.program.display(),
                    arguments.join(" ")
                )
            })?;
        Ok((
            output.status.success(),
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        ))
    }
}

impl Launchctl for SystemLaunchctl {
    fn bootstrap(&self, plist: &Path) -> Result<(), String> {
        let arguments = bootstrap_arguments(self.uid, plist);
        let (succeeded, stdout, stderr) = self.run(&arguments)?;
        if succeeded {
            return Ok(());
        }
        Err(format!(
            "{} {} failed: {}",
            self.program.display(),
            arguments.join(" "),
            one_line(&stderr, &stdout)
        ))
    }

    fn bootout(&self, label: &str) -> Result<(), String> {
        let arguments = bootout_arguments(self.uid, label);
        let (succeeded, stdout, stderr) = self.run(&arguments)?;
        if succeeded {
            return Ok(());
        }
        Err(format!(
            "{} {} failed: {}",
            self.program.display(),
            arguments.join(" "),
            one_line(&stderr, &stdout)
        ))
    }

    fn query(&self, label: &str) -> JobQuery {
        let arguments = print_arguments(self.uid, label);
        match self.run(&arguments) {
            Ok((true, stdout, _)) => JobQuery::Loaded(parse_print(&stdout)),
            Ok((false, stdout, stderr)) => {
                // launchd's own words for "no such job", verified on this machine: exit 113 and
                // `Could not find service "…" in domain for user gui: <uid>`. Anything else is a
                // failure to ask rather than an answer.
                if stderr.contains("Could not find service") || stdout.contains("Could not find") {
                    JobQuery::NotLoaded
                } else {
                    JobQuery::Undetermined(format!(
                        "{} {} did not answer: {}",
                        self.program.display(),
                        arguments.join(" "),
                        one_line(&stderr, &stdout)
                    ))
                }
            }
            Err(reason) => JobQuery::Undetermined(reason),
        }
    }

    fn describes_itself_as(&self) -> String {
        match self.override_in_use() {
            Some(path) => format!(
                "{} — NOT launchd, because {LAUNCHCTL_VARIABLE} is set. Nothing this run reports \
                 describes a real login session",
                path.display()
            ),
            None => format!("launchd, through {}", self.program.display()),
        }
    }
}

/// The most informative of two output streams, on one line.
fn one_line(stderr: &str, stdout: &str) -> String {
    for stream in [stderr, stdout] {
        let text = stream.trim();
        if !text.is_empty() {
            return text.replace('\n', "; ");
        }
    }
    "it said nothing at all".to_string()
}

// --- The request the three verbs work from ---

/// Everything `install`, `uninstall` and `status` need to know about this machine.
#[derive(Debug, Clone)]
pub struct Install {
    /// Where the plist goes. Relocatable, so no test can reach the real one.
    pub launch_agents_dir: PathBuf,
    pub job: Job,
    /// Set when launchd is being reached through [`LAUNCHCTL_VARIABLE`], so every verb can say
    /// so rather than letting a fake read as the real thing.
    pub launchctl_override: Option<PathBuf>,
}

impl Install {
    /// The one file this product writes outside its own two directories.
    pub fn plist_path(&self) -> PathBuf {
        self.launch_agents_dir
            .join(format!("{}.plist", self.job.label))
    }

    /// Resolved from this machine's environment.
    ///
    /// Fails rather than guessing at any of it. A plist pointing at a program that is not there,
    /// or a LaunchAgents directory chosen because it was probably right, is a job launchd
    /// retries for ever.
    pub fn from_environment() -> Result<Install, String> {
        let program = std::env::current_exe().map_err(|error| {
            format!(
                "this binary's own path could not be resolved ({error}), and the plist has to \
                 name it absolutely"
            )
        })?;
        let paths = crate::state::Paths::from_environment()?;
        let launch_agents_dir = launch_agents_dir_from_values(
            std::env::var(LAUNCH_AGENTS_VARIABLE).ok().as_deref(),
            std::env::var("HOME").ok().as_deref(),
        )?;
        let environment = propagated_environment(&|name| std::env::var(name).ok());

        Ok(Install {
            launch_agents_dir,
            job: Job::monitor(&program, paths.state_dir(), environment),
            launchctl_override: std::env::var(LAUNCHCTL_VARIABLE).ok().map(PathBuf::from),
        })
    }
}

/// Where LaunchAgents live, honouring the override.
pub fn launch_agents_dir_from_values(
    explicit: Option<&str>,
    home: Option<&str>,
) -> Result<PathBuf, String> {
    if let Some(path) = explicit.map(str::trim).filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(path));
    }
    match home.map(str::trim).filter(|value| !value.is_empty()) {
        Some(home) => Ok(PathBuf::from(home).join("Library").join("LaunchAgents")),
        None => Err(format!(
            "HOME is not readable, so {LAUNCH_AGENTS_VARIABLE} must name the LaunchAgents \
             directory explicitly"
        )),
    }
}

// --- Installing ---

/// What a failed install left behind.
///
/// Its own type because "the install failed" and "the install failed and there is now a plist on
/// this machine that will load at the next login" are different facts, and only the second one
/// needs a human.
#[derive(Debug, Clone)]
pub struct Leftover {
    pub plist: PathBuf,
    /// `None` when the plist this run wrote was removed again; `Some(reason)` when it is still
    /// there and why the removal did not happen.
    pub plist_remains: Option<String>,
}

impl fmt::Display for Leftover {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.plist_remains {
            None => write!(
                formatter,
                "the plist this run wrote was removed again ({}), so nothing was left behind",
                self.plist.display()
            ),
            Some(reason) => write!(
                formatter,
                "the plist {} is still there and could not be removed ({reason}). launchd will \
                 read it at the next login, so remove it by hand or run `amon uninstall`",
                self.plist.display()
            ),
        }
    }
}

/// How an install ended.
#[derive(Debug)]
pub enum Installed {
    /// launchd was asked afterwards and has the job. The only success.
    Loaded {
        plist: PathBuf,
        facts: JobFacts,
    },
    /// A plist is already there. Not overwritten: it may be an older version of ours with a
    /// hand-edit in it, or somebody else's file, and destroying either as a side effect of
    /// `install` would be silent.
    AlreadyInstalled {
        plist: PathBuf,
        identical: bool,
    },
    /// The program the plist would name cannot be run. Checked before anything is written.
    ProgramUnusable {
        program: PathBuf,
        reason: String,
    },
    DirectoryUnusable {
        path: PathBuf,
        reason: String,
    },
    /// The state directory could not be made, so launchd could not open the monitor's log and
    /// the job would not start.
    StateDirectoryUnusable {
        path: PathBuf,
        reason: String,
    },
    PlistUnwritable {
        plist: PathBuf,
        reason: String,
    },
    /// `bootstrap` itself refused.
    LoadFailed {
        plist: PathBuf,
        reason: String,
        leftover: Leftover,
    },
    /// `bootstrap` returned, and launchd does not have the job — or would not say. This is the
    /// case the ticket exists for: reporting success here is silent non-monitoring.
    LoadUnverified {
        plist: PathBuf,
        reason: String,
        leftover: Leftover,
    },
}

impl Installed {
    /// Whether a job is now loaded. Nothing else counts as an install.
    pub fn is_installed(&self) -> bool {
        matches!(self, Installed::Loaded { .. })
    }

    /// The whole outcome as one paragraph, for stderr or a screen.
    pub fn message(&self) -> String {
        match self {
            Installed::Loaded { plist, facts } => {
                let mut text = format!(
                    "the LaunchAgent {} is installed and launchd confirms the job {LABEL} is \
                     loaded",
                    plist.display()
                );
                match facts.pid {
                    Some(pid) => text.push_str(&format!(", running as pid {pid}")),
                    None => text.push_str(
                        ", but launchd is not running it at this moment — see `amon status`",
                    ),
                }
                text
            }
            Installed::AlreadyInstalled { plist, identical } => format!(
                "{} already exists, so nothing was written and no job was loaded. Its contents \
                 {} what this build would write. Run `amon uninstall` first if you want it \
                 replaced; `amon status` says whether the job it describes is actually loaded.",
                plist.display(),
                if *identical { "match" } else { "differ from" }
            ),
            Installed::ProgramUnusable { program, reason } => format!(
                "nothing was written: the plist would point at {}, and {reason}. launchd would \
                 retry that path every {THROTTLE_SECONDS}s for ever without ever monitoring \
                 anything.",
                program.display()
            ),
            Installed::DirectoryUnusable { path, reason } => format!(
                "nothing was written: the LaunchAgents directory {} is unusable ({reason})",
                path.display()
            ),
            Installed::StateDirectoryUnusable { path, reason } => format!(
                "nothing was written: the state directory {} could not be created ({reason}), \
                 and launchd cannot start a job whose log it cannot open",
                path.display()
            ),
            Installed::PlistUnwritable { plist, reason } => format!(
                "no job was loaded: {} could not be written ({reason})",
                plist.display()
            ),
            Installed::LoadFailed {
                plist,
                reason,
                leftover,
            } => format!(
                "the job {LABEL} was not loaded: launchd refused the plist {} — {reason}. \
                 {leftover}",
                plist.display()
            ),
            Installed::LoadUnverified {
                plist,
                reason,
                leftover,
            } => format!(
                "the plist {} was written and bootstrapped, and launchd does not confirm the job \
                 {LABEL} is loaded: {reason}. Nothing is being monitored, so this is a failure \
                 rather than an install. {leftover}",
                plist.display()
            ),
        }
    }
}

/// Write and load the LaunchAgent, and verify the load.
///
/// `notice` receives each step as it happens rather than at the end, because F24 requires the
/// path to be stated *before* the file is created — which is an ordering, not a sentence in a
/// summary.
pub fn install(
    request: &Install,
    launchctl: &dyn Launchctl,
    notice: &mut dyn FnMut(&str),
) -> Installed {
    let plist = request.plist_path();

    // First, before any write: what this is about to create, and the fact that it is the only
    // file outside this tool's own two directories.
    notice(&format!(
        "about to create {} — the LaunchAgent plist, and the only file this tool writes outside \
         its config and state directories",
        plist.display()
    ));
    notice(&format!(
        "the job it will run: {}",
        request.job.command_line()
    ));
    notice(&format!(
        "launchd will write the monitor's output to {}",
        request.job.log.display()
    ));
    notice(&format!(
        "launchd is being reached as: {}",
        launchctl.describes_itself_as()
    ));
    if let Some(overridden) = &request.launchctl_override {
        notice(&format!(
            "WARNING: {LAUNCHCTL_VARIABLE} is set to {}, so this run is not talking to launchd \
             and cannot install anything real",
            overridden.display()
        ));
    }
    // Keyed off the verb's own state, so this warning retires itself the moment `watch` can
    // monitor. A LaunchAgent running a verb that exits non-zero is a job launchd restarts every
    // ThrottleInterval for ever, and an install that did not say so would hand somebody a crash
    // loop and call it monitoring.
    match AmonVerb::Watch.state() {
        VerbState::Available => {}
        VerbState::Partial { built, tracked_as } => notice(&format!(
            "WARNING: `amon watch` has {built} but cannot monitor yet ({tracked_as}), so the job \
             will exit non-zero and launchd will restart it every {THROTTLE_SECONDS}s until that \
             lands. `amon status` shows the run count."
        )),
        VerbState::Planned { tracked_as } => notice(&format!(
            "WARNING: `amon watch` is not built yet ({tracked_as}), so the job will exit \
             non-zero and launchd will restart it every {THROTTLE_SECONDS}s until it is"
        )),
    }

    // The program, before anything exists on disk. A plist naming a path that is not there is a
    // job that fails for ever, and it is easier to refuse than to diagnose later.
    if let Err(reason) = usable_program(&request.job.program) {
        return Installed::ProgramUnusable {
            program: request.job.program.clone(),
            reason,
        };
    }

    match plist.try_exists() {
        Ok(true) => {
            let identical = std::fs::read_to_string(&plist)
                .map(|existing| existing == request.job.plist())
                .unwrap_or(false);
            return Installed::AlreadyInstalled { plist, identical };
        }
        Ok(false) => {}
        Err(error) => {
            return Installed::DirectoryUnusable {
                path: request.launch_agents_dir.clone(),
                reason: format!("{} could not be checked for: {error}", plist.display()),
            }
        }
    }

    if let Err(error) = std::fs::create_dir_all(&request.launch_agents_dir) {
        return Installed::DirectoryUnusable {
            path: request.launch_agents_dir.clone(),
            reason: format!("it could not be created: {error}"),
        };
    }

    // launchd creates the log file it is told to write, but not the directory above it, and a
    // job whose StandardErrorPath cannot be opened does not start. Inside the boundary F24
    // draws, so this costs nothing the documentation has to explain.
    if let Some(state_dir) = request.job.log.parent() {
        if let Err(error) = std::fs::create_dir_all(state_dir) {
            return Installed::StateDirectoryUnusable {
                path: state_dir.to_path_buf(),
                reason: error.to_string(),
            };
        }
    }

    // `create_new`, so the kernel — not a check followed by a window — is what refuses to
    // clobber a file somebody else put there.
    if let Err(error) = write_new(&plist, &request.job.plist()) {
        return Installed::PlistUnwritable {
            plist,
            reason: error,
        };
    }
    notice(&format!("created {}", plist.display()));

    if let Err(reason) = launchctl.bootstrap(&plist) {
        let leftover = undo(request, launchctl, &plist, notice);
        return Installed::LoadFailed {
            plist,
            reason,
            leftover,
        };
    }
    notice("launchd accepted the plist; asking it whether the job is actually loaded");

    // The criterion this whole module exists for. `bootstrap` exiting zero is not a loaded job.
    match launchctl.query(&request.job.label) {
        JobQuery::Loaded(facts) => {
            notice(&format!(
                "launchd has the job {LABEL}: {}",
                describe_facts(&facts)
            ));
            Installed::Loaded { plist, facts }
        }
        JobQuery::NotLoaded => {
            let leftover = undo(request, launchctl, &plist, notice);
            Installed::LoadUnverified {
                plist,
                reason: "launchd does not have a job by that label".to_string(),
                leftover,
            }
        }
        JobQuery::Undetermined(reason) => {
            let leftover = undo(request, launchctl, &plist, notice);
            Installed::LoadUnverified {
                plist,
                reason,
                leftover,
            }
        }
    }
}

/// Undo an install that did not finish.
///
/// A plist with no job is the dangerous half-state: unmonitored today, monitored after the next
/// login, and nothing on disk to say which. So the file this run wrote is removed again — and if
/// it cannot be, that is said rather than swallowed. `bootout` first, in case the job was
/// registered by a `bootstrap` that then failed, or by one whose result could not be read.
fn undo(
    request: &Install,
    launchctl: &dyn Launchctl,
    plist: &Path,
    notice: &mut dyn FnMut(&str),
) -> Leftover {
    if let Err(reason) = launchctl.bootout(&request.job.label) {
        // Expected in the common case — there is usually nothing to unload — so it is a note
        // rather than a failure. Reported all the same, because the one time it means something
        // is the time a job was registered and would not go away.
        notice(&format!(
            "unloading {LABEL} while undoing this install reported: {reason}"
        ));
    }

    match std::fs::remove_file(plist) {
        Ok(()) => {
            notice(&format!(
                "removed {} again, so this failed install left nothing behind",
                plist.display()
            ));
            Leftover {
                plist: plist.to_path_buf(),
                plist_remains: None,
            }
        }
        Err(error) => {
            notice(&format!(
                "WARNING: {} is still there and could not be removed: {error}",
                plist.display()
            ));
            Leftover {
                plist: plist.to_path_buf(),
                plist_remains: Some(error.to_string()),
            }
        }
    }
}

/// Whether a path can be what a plist names as its program.
fn usable_program(program: &Path) -> Result<(), String> {
    if !program.is_absolute() {
        return Err(
            "it is not an absolute path — launchd's working directory is not the shell's, so a \
             relative program would resolve against something the installer never saw"
                .to_string(),
        );
    }
    match program.try_exists() {
        Ok(true) => {}
        Ok(false) => return Err("there is nothing at that path".to_string()),
        Err(error) => return Err(format!("it could not be checked for: {error}")),
    }
    if !program.is_file() {
        return Err("it is not a file".to_string());
    }
    Ok(())
}

fn write_new(path: &Path, contents: &str) -> Result<(), String> {
    use std::io::Write as _;

    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| error.to_string())?;
    file.write_all(contents.as_bytes())
        .map_err(|error| error.to_string())?;
    file.sync_all().map_err(|error| error.to_string())
}

// --- Uninstalling ---

/// How an uninstall ended.
#[derive(Debug)]
pub enum Uninstalled {
    /// The job is not loaded and the plist is gone. Both halves verified.
    Removed { plist: PathBuf, was_loaded: bool },
    /// There was nothing to remove — launchd was asked, and the plist's absence was read from
    /// the filesystem. A success, because the end state this verb exists to reach holds.
    NothingToRemove { plist: PathBuf },
    /// launchd still reports the job after being asked to unload it. The plist is deliberately
    /// left alone: removing it would leave a running job with nothing on disk to unload it by.
    StillLoaded { plist: PathBuf, reason: String },
    /// The job was unloaded and the plist could not be removed, which is the orphan the ticket
    /// names.
    PlistRemains { plist: PathBuf, reason: String },
    /// launchd could not be asked. Nothing was touched, because removing a plist on the strength
    /// of an answer nobody got could strand a running job for ever.
    Undetermined { plist: PathBuf, reason: String },
}

impl Uninstalled {
    pub fn succeeded(&self) -> bool {
        matches!(
            self,
            Uninstalled::Removed { .. } | Uninstalled::NothingToRemove { .. }
        )
    }

    pub fn message(&self) -> String {
        match self {
            Uninstalled::Removed { plist, was_loaded } => format!(
                "the job {LABEL} {} and {} is gone. launchd confirms it is no longer loaded.",
                if *was_loaded {
                    "was unloaded"
                } else {
                    "was not loaded"
                },
                plist.display()
            ),
            Uninstalled::NothingToRemove { plist } => format!(
                "there was nothing to remove: launchd has no job {LABEL} and there is no plist \
                 at {}.",
                plist.display()
            ),
            Uninstalled::StillLoaded { plist, reason } => format!(
                "launchd still reports the job {LABEL} after it was asked to unload it: \
                 {reason}. {} has been left in place — it is the only handle left on that job, \
                 and removing it would leave something running with nothing on disk to explain \
                 it.",
                plist.display()
            ),
            Uninstalled::PlistRemains { plist, reason } => format!(
                "the job {LABEL} was unloaded, and {} could not be removed ({reason}). That is \
                 an orphaned plist: launchd will load it again at the next login.",
                plist.display()
            ),
            Uninstalled::Undetermined { plist, reason } => format!(
                "launchd could not be asked whether {LABEL} is loaded ({reason}), so nothing was \
                 touched and {} is still there. Removing it without knowing could strand a \
                 running job with nothing to unload it by.",
                plist.display()
            ),
        }
    }
}

/// Unload the job and remove the plist, verifying both.
pub fn uninstall(
    request: &Install,
    launchctl: &dyn Launchctl,
    notice: &mut dyn FnMut(&str),
) -> Uninstalled {
    let plist = request.plist_path();
    notice(&format!(
        "launchd is being reached as: {}",
        launchctl.describes_itself_as()
    ));

    let was_loaded = match launchctl.query(&request.job.label) {
        JobQuery::Loaded(facts) => {
            notice(&format!(
                "launchd has the job {LABEL}: {}",
                describe_facts(&facts)
            ));
            true
        }
        JobQuery::NotLoaded => {
            notice(&format!("launchd has no job {LABEL}"));
            false
        }
        JobQuery::Undetermined(reason) => {
            return Uninstalled::Undetermined { plist, reason };
        }
    };

    if was_loaded {
        notice(&format!("asking launchd to unload {LABEL}"));
        if let Err(reason) = launchctl.bootout(&request.job.label) {
            notice(&format!("the unload reported: {reason}"));
        }
        // Verified, not assumed — the same rule as install's, in the other direction.
        match launchctl.query(&request.job.label) {
            JobQuery::NotLoaded => notice(&format!("launchd confirms {LABEL} is unloaded")),
            JobQuery::Loaded(facts) => {
                return Uninstalled::StillLoaded {
                    plist,
                    reason: format!("it is still there — {}", describe_facts(&facts)),
                }
            }
            JobQuery::Undetermined(reason) => {
                return Uninstalled::StillLoaded {
                    plist,
                    reason: format!("launchd would not say whether the unload worked: {reason}"),
                }
            }
        }
    }

    match plist.try_exists() {
        Ok(true) => match std::fs::remove_file(&plist) {
            Ok(()) => {
                notice(&format!("removed {}", plist.display()));
                Uninstalled::Removed { plist, was_loaded }
            }
            Err(error) => Uninstalled::PlistRemains {
                plist,
                reason: error.to_string(),
            },
        },
        Ok(false) if was_loaded => {
            notice(&format!(
                "there was no plist at {} — the job had been loaded from somewhere else",
                plist.display()
            ));
            Uninstalled::Removed { plist, was_loaded }
        }
        Ok(false) => Uninstalled::NothingToRemove { plist },
        Err(error) => Uninstalled::PlistRemains {
            plist,
            reason: format!("it could not be checked for: {error}"),
        },
    }
}

// --- Status ---

/// Where an answer about the monitor's process came from.
///
/// Reported alongside the answer because the two sources mean different things: launchd knows
/// about the job it supervises, and the state file knows about whoever is actually writing —
/// which under `amon watch --foreground` is a monitor launchd has never heard of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Evidence {
    Launchd,
    StateFileWriter,
}

impl fmt::Display for Evidence {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Evidence::Launchd => write!(formatter, "launchd"),
            Evidence::StateFileWriter => write!(formatter, "the writer pid in {STATE_FILE}"),
        }
    }
}

/// Whether a monitor process is running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcessAnswer {
    Running {
        pid: u32,
        from: Evidence,
    },
    NotRunning {
        from: Evidence,
    },
    /// Nobody could say. Never reported as "not running", which reads as a monitor that stopped.
    Undetermined(String),
}

impl ProcessAnswer {
    /// Decide from what launchd said and what the state file says, in that order.
    ///
    /// The state file is consulted even when launchd has no job, because a monitor started by
    /// hand is still a monitor — and telling somebody there is none would invite them to start a
    /// second writer.
    pub fn determine(
        job: &JobQuery,
        writer: Option<u32>,
        alive: &dyn Fn(u32) -> bool,
    ) -> ProcessAnswer {
        if let JobQuery::Loaded(facts) = job {
            match (facts.pid, facts.running) {
                (Some(pid), _) => {
                    return ProcessAnswer::Running {
                        pid,
                        from: Evidence::Launchd,
                    }
                }
                (None, Some(false)) => {
                    return ProcessAnswer::NotRunning {
                        from: Evidence::Launchd,
                    }
                }
                (None, _) => {}
            }
        }

        match writer {
            Some(pid) if alive(pid) => ProcessAnswer::Running {
                pid,
                from: Evidence::StateFileWriter,
            },
            Some(_) => ProcessAnswer::NotRunning {
                from: Evidence::StateFileWriter,
            },
            None => match job {
                JobQuery::NotLoaded => ProcessAnswer::NotRunning {
                    from: Evidence::StateFileWriter,
                },
                JobQuery::Undetermined(reason) => ProcessAnswer::Undetermined(format!(
                    "launchd could not be asked ({reason}) and {STATE_FILE} names no writer"
                )),
                JobQuery::Loaded(_) => ProcessAnswer::Undetermined(format!(
                    "launchd reported neither a pid nor a state for the job, and {STATE_FILE} \
                     names no writer"
                )),
            },
        }
    }
}

/// The age of the monitor's last write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LastWrite {
    Age {
        path: PathBuf,
        age: Duration,
        writer_pid: Option<u32>,
    },
    /// Nothing has ever been written. An answer, not a failure to answer — and never an age of
    /// zero, which would read as a monitor that wrote a moment ago.
    Absent {
        path: PathBuf,
    },
    /// The file is stamped later than the clock this was asked about. Clock skew, or a restore
    /// from a backup; either way the age cannot be computed and a number here would read as a
    /// measurement.
    AheadOfTheClock {
        path: PathBuf,
        ahead: Duration,
    },
    Unreadable {
        path: PathBuf,
        reason: String,
    },
}

impl LastWrite {
    /// Read it from the state directory, against a given clock.
    ///
    /// The mtime of `state.json` rather than a tier's timestamp: this is the age of the *write*,
    /// which is what says whether a monitor is still working. The age of each *fact* is per-tier
    /// and belongs to the display's freshness classification (#30).
    pub fn of(store: &StateStore, now: SystemTime) -> LastWrite {
        let path = store.paths().state_dir().join(STATE_FILE);

        let modified = match std::fs::metadata(&path) {
            Ok(metadata) => match metadata.modified() {
                Ok(modified) => modified,
                Err(error) => {
                    return LastWrite::Unreadable {
                        path,
                        reason: format!("its modification time could not be read: {error}"),
                    }
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return LastWrite::Absent { path }
            }
            Err(error) => {
                return LastWrite::Unreadable {
                    path,
                    reason: error.to_string(),
                }
            }
        };

        let writer_pid = store
            .read_tiered_state(STATE_FILE)
            .ok()
            .flatten()
            .map(|state| state.writer_pid());

        match now.duration_since(modified) {
            Ok(age) => LastWrite::Age {
                path,
                age,
                writer_pid,
            },
            Err(error) => LastWrite::AheadOfTheClock {
                path,
                ahead: error.duration(),
            },
        }
    }

    /// The writer pid the state file names, when it could be read.
    pub fn writer_pid(&self) -> Option<u32> {
        match self {
            LastWrite::Age { writer_pid, .. } => *writer_pid,
            _ => None,
        }
    }

    /// Whether this is an answer rather than a failure to get one.
    pub fn determined(&self) -> bool {
        matches!(self, LastWrite::Age { .. } | LastWrite::Absent { .. })
    }
}

/// What `amon status` found.
#[derive(Debug)]
pub struct Status {
    pub plist: PathBuf,
    /// `Err` when the check itself failed, which is not the same as the file being absent.
    pub plist_present: Result<bool, String>,
    pub job: JobQuery,
    pub process: ProcessAnswer,
    pub last_write: LastWrite,
    /// The stand-in for launchd, when one is in use.
    pub launchctl_override: Option<PathBuf>,
}

impl Status {
    /// Whether all three questions were answered. Deliberately not "whether the monitor is
    /// healthy": a monitor that is switched off is a determinate answer, and conflating it with
    /// a question nobody could answer would destroy the distinction this verb exists to draw.
    pub fn complete(&self) -> bool {
        !matches!(self.job, JobQuery::Undetermined(_))
            && !matches!(self.process, ProcessAnswer::Undetermined(_))
            && self.last_write.determined()
            && self.plist_present.is_ok()
    }

    /// Which question went unanswered, when one did.
    pub fn unanswered(&self) -> Vec<String> {
        let mut missing = Vec::new();
        if let JobQuery::Undetermined(reason) = &self.job {
            missing.push(format!("whether the job {LABEL} is loaded: {reason}"));
        }
        if let ProcessAnswer::Undetermined(reason) = &self.process {
            missing.push(format!("whether a monitor process is running: {reason}"));
        }
        match &self.last_write {
            LastWrite::Unreadable { path, reason } => missing.push(format!(
                "the age of the last write: {} could not be read ({reason})",
                path.display()
            )),
            LastWrite::AheadOfTheClock { path, ahead } => missing.push(format!(
                "the age of the last write: {} is stamped {} ahead of this machine's clock",
                path.display(),
                describe_duration(*ahead)
            )),
            LastWrite::Age { .. } | LastWrite::Absent { .. } => {}
        }
        if let Err(reason) = &self.plist_present {
            missing.push(format!(
                "whether {} is there: {reason}",
                self.plist.display()
            ));
        }
        missing
    }

    /// The report, one fact per line.
    pub fn lines(&self) -> Vec<String> {
        let mut lines = Vec::new();

        if let Some(overridden) = &self.launchctl_override {
            lines.push(format!(
                "launchd: NOT ASKED — {LAUNCHCTL_VARIABLE} points at {}, so nothing below \
                 describes a real login session",
                overridden.display()
            ));
        }

        lines.push(match &self.plist_present {
            Ok(true) => format!("plist: present at {}", self.plist.display()),
            Ok(false) => format!("plist: absent — nothing at {}", self.plist.display()),
            Err(reason) => format!(
                "plist: UNDETERMINED — {} could not be checked for: {reason}",
                self.plist.display()
            ),
        });

        lines.push(match &self.job {
            JobQuery::Loaded(facts) => {
                format!("job {LABEL}: loaded — {}", describe_facts(facts))
            }
            JobQuery::NotLoaded => format!(
                "job {LABEL}: NOT loaded — launchd has no such job, so nothing is being \
                 monitored from a login session. `amon install` loads it."
            ),
            JobQuery::Undetermined(reason) => format!(
                "job {LABEL}: UNDETERMINED — launchd could not be asked: {reason}. This is not \
                 the same as the job being absent."
            ),
        });

        lines.push(match &self.process {
            ProcessAnswer::Running { pid, from } => {
                format!("process: running as pid {pid}, according to {from}")
            }
            ProcessAnswer::NotRunning { from } => format!(
                "process: NOT running, according to {from} — no monitor is writing state or \
                 sending alerts right now"
            ),
            ProcessAnswer::Undetermined(reason) => {
                format!("process: UNDETERMINED — {reason}")
            }
        });

        lines.push(match &self.last_write {
            LastWrite::Age {
                path,
                age,
                writer_pid,
            } => format!(
                "last write: {} ago, to {}{}",
                describe_duration(*age),
                path.display(),
                match writer_pid {
                    Some(pid) => format!(", by pid {pid}"),
                    None => String::new(),
                }
            ),
            LastWrite::Absent { path } => format!(
                "last write: none — {} has never been written, so no monitor has ever completed \
                 a pass here",
                path.display()
            ),
            LastWrite::AheadOfTheClock { path, ahead } => format!(
                "last write: UNDETERMINED — {} is stamped {} ahead of this machine's clock, so \
                 its age cannot be computed",
                path.display(),
                describe_duration(*ahead)
            ),
            LastWrite::Unreadable { path, reason } => format!(
                "last write: UNDETERMINED — {} could not be read: {reason}",
                path.display()
            ),
        });

        lines
    }
}

/// Report on the job, the process, and the age of the last write.
pub fn status(
    request: &Install,
    launchctl: &dyn Launchctl,
    store: &StateStore,
    now: SystemTime,
    alive: &dyn Fn(u32) -> bool,
) -> Status {
    let plist = request.plist_path();
    let job = launchctl.query(&request.job.label);
    let last_write = LastWrite::of(store, now);
    let process = ProcessAnswer::determine(&job, last_write.writer_pid(), alive);

    Status {
        plist_present: plist.try_exists().map_err(|error| error.to_string()),
        plist,
        job,
        process,
        last_write,
        launchctl_override: request.launchctl_override.clone(),
    }
}

/// launchd's facts about a job as one clause.
///
/// The run count and last exit code are in here on purpose: a `KeepAlive` restart must not be
/// silent, and until #28's launch record lands these are the only evidence that a monitor has
/// been cycling rather than sitting still.
fn describe_facts(facts: &JobFacts) -> String {
    let mut parts = Vec::new();
    parts.push(match (facts.running, facts.pid) {
        (Some(true), Some(pid)) => format!("running as pid {pid}"),
        (Some(true), None) => "running, and launchd reported no pid".to_string(),
        (Some(false), _) => "not running at this moment".to_string(),
        (None, Some(pid)) => format!("pid {pid}, and launchd reported no state"),
        (None, None) => "launchd reported neither a pid nor a state".to_string(),
    });
    if let Some(runs) = facts.runs {
        parts.push(format!(
            "started {runs} time{} by launchd (a durable launch record, with downtime and \
             whether each exit was clean, is #28)",
            if runs == 1 { "" } else { "s" }
        ));
    }
    if let Some(last_exit) = &facts.last_exit {
        parts.push(format!("last exit code {last_exit}"));
    }
    parts.join("; ")
}

/// A duration a human reads, never rounded down to nothing.
fn describe_duration(duration: Duration) -> String {
    let seconds = duration.as_secs();
    if seconds < 60 {
        return format!("{seconds}s");
    }
    if seconds < 3600 {
        return format!("{}m {}s", seconds / 60, seconds % 60);
    }
    if seconds < 86_400 {
        return format!("{}h {}m", seconds / 3600, (seconds % 3600) / 60);
    }
    format!("{}d {}h", seconds / 86_400, (seconds % 86_400) / 3600)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_launchctl_override_never_describes_itself_as_launchd() {
        let real = SystemLaunchctl::from_values(None, 501);
        assert!(real.override_in_use().is_none());
        assert!(real.describes_itself_as().contains(LAUNCHCTL));

        let fake = SystemLaunchctl::from_values(Some("/usr/bin/false"), 501);
        assert_eq!(fake.override_in_use(), Some(Path::new("/usr/bin/false")));
        let described = fake.describes_itself_as();
        assert!(described.contains("NOT launchd"), "{described}");
        assert!(described.contains("/usr/bin/false"), "{described}");
    }

    #[test]
    fn the_launch_agents_directory_falls_back_to_home_and_fails_without_one() {
        assert_eq!(
            launch_agents_dir_from_values(None, Some("/Users/someone")).expect("home is enough"),
            PathBuf::from("/Users/someone/Library/LaunchAgents")
        );
        assert_eq!(
            launch_agents_dir_from_values(Some("/tmp/agents"), Some("/Users/someone"))
                .expect("explicit wins"),
            PathBuf::from("/tmp/agents")
        );
        let reason = launch_agents_dir_from_values(None, None).expect_err("nothing to go on");
        assert!(reason.contains(LAUNCH_AGENTS_VARIABLE), "{reason}");
    }

    #[test]
    fn a_relative_program_is_refused_before_anything_is_written() {
        let reason = usable_program(Path::new("target/debug/amon")).expect_err("relative");
        assert!(reason.contains("absolute"), "{reason}");
    }

    #[test]
    fn an_age_is_never_rounded_down_to_nothing() {
        assert_eq!(describe_duration(Duration::from_millis(1500)), "1s");
        assert_eq!(describe_duration(Duration::from_secs(61)), "1m 1s");
        assert_eq!(describe_duration(Duration::from_secs(3700)), "1h 1m");
        assert_eq!(describe_duration(Duration::from_secs(100_000)), "1d 3h");
    }
}
