//! Seam 15 — alerts that fire with no terminal open, installed by `amon install`.
//!
//! The failure this seam exists to prevent is the one named in the PRD's risk table:
//! **`amon install` fails or the job never loads, and says nothing** — silent non-monitoring
//! that looks like installation success. It is the same defect class as every other one in this
//! project, aimed at the one command whose whole purpose is to make the monitor exist when
//! nobody is looking at a screen. An install that reported success on a job launchd never
//! registered would leave a machine unmonitored for as long as it took someone to notice that
//! no alert had ever arrived, which on a quiet week is never.
//!
//! So everything here is about what is *checked* rather than what is *assumed*: the plist is
//! parsed by the system's own parser rather than by eye, the load is verified by asking launchd
//! rather than by trusting `bootstrap`'s exit code, and a failure states what it left behind.
//!
//! **Nothing in this file may touch the real `~/Library/LaunchAgents`, and nothing in it may
//! register a job with the user's login session.** Every path here is relocated with
//! `ACMON_LAUNCH_AGENTS_DIR` and `ACMON_STATE_DIR` into a scratch directory, and launchd itself
//! is either a fake in this process or `/usr/bin/false` through `ACMON_LAUNCHCTL`. The one
//! command run against the real system is `/usr/bin/plutil`, which reads a file in the scratch
//! directory and writes nothing. What cannot be proven that way — that a plist this tool wrote,
//! bootstrapped into a live session, actually delivers a notification with no terminal open —
//! is stated as a human step in the ticket rather than faked here, because a fake launchd
//! proving a real launchd's behaviour is a test of itself.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use acmon::cli::{AmonVerb, VerbState};
use acmon::launchd::{
    bootout_arguments, bootstrap_arguments, install, parse_print, print_arguments,
    propagated_environment, service_target, status, uninstall, Install, Installed, Job, JobFacts,
    JobQuery, LastWrite, Launchctl, ProcessAnswer, Uninstalled, JOB_PATH, LABEL, LOG_FILE,
    THROTTLE_SECONDS,
};
use acmon::lock::Predecessor;
use acmon::starts::{self, History, LastStateWrite};
use acmon::state::{Paths, StateStore, TieredState, STATE_FILE};

// --- Scratch machinery ---

/// A directory tree that is this test's alone, removed on the way in.
///
/// Named per test so the suite's threads cannot collide, and rooted in `TMPDIR` so that no test
/// in this file can reach `~/Library/LaunchAgents` even by mistake.
fn scratch(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("acmon-seam15-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create the scratch root");
    root
}

/// A program path that exists and is executable, standing in for an installed `amon`.
///
/// The real `amon` under test, in fact: the plist has to point at something the kernel would
/// run, and a hand-made empty file would let a bug that requires an executable through.
fn amon_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_amon"))
}

/// The install request every test here works from: scratch LaunchAgents, scratch state.
fn request_in(root: &Path) -> Install {
    let launch_agents = root.join("LaunchAgents");
    let state = root.join("state");
    Install {
        launch_agents_dir: launch_agents,
        job: Job::monitor(&amon_binary(), &state, Vec::new()),
        launchctl_override: None,
    }
}

/// What a `launchctl print` of a running job looks like, from this machine.
///
/// Captured from `/bin/launchctl print gui/501/com.apple.syncdefaultsd` — a real, running
/// LaunchAgent — rather than invented, because the whole value of the parser is that it agrees
/// with launchd's actual output. Trimmed to the lines the parser reads; the field names,
/// spacing and phrasing are launchd's own.
const PRINT_RUNNING: &str = "gui/501/io.github.pmcfadin.acmon = {
\tactive count = 5
\tpath = /Users/someone/Library/LaunchAgents/io.github.pmcfadin.acmon.plist
\ttype = LaunchAgent
\tstate = running

\tprogram = /opt/homebrew/bin/amon
\truns = 3
\tpid = 1019
\tlast exit code = (never exited)
}";

/// The same, for a job launchd has registered and is not currently running.
///
/// Captured from `/bin/launchctl print gui/501/com.apple.mediacontinuityd`. This is the shape
/// that matters most: there is **no `pid =` line at all**, so a parser that reported a missing
/// pid as "not running" without reading `state` would be right here by luck and wrong on the
/// output above.
const PRINT_NOT_RUNNING: &str = "gui/501/io.github.pmcfadin.acmon = {
\tactive count = 0
\tpath = /Users/someone/Library/LaunchAgents/io.github.pmcfadin.acmon.plist
\ttype = LaunchAgent
\tstate = not running

\tprogram = /opt/homebrew/bin/amon
\truns = 12
\tlast exit code = 1
}";

/// A launchd that answers from a script, and records every question it was asked.
///
/// A fake rather than the real thing, and deliberately so: `bootstrap` registers a job with the
/// user's login session, which is a durable change to a developer's machine that no test may
/// make. What is under test here is the *decisions* — what is written, what is asked, what is
/// believed, and what is left behind — and those are the same decisions whichever launchd is
/// on the other side.
struct FakeLaunchd {
    asked: Mutex<Vec<String>>,
    answers: Mutex<Vec<JobQuery>>,
    #[allow(clippy::type_complexity)]
    on_bootstrap: Box<dyn Fn(&Path) -> Result<(), String> + Send + Sync>,
    #[allow(clippy::type_complexity)]
    on_bootout: Box<dyn Fn(&str) -> Result<(), String> + Send + Sync>,
}

impl FakeLaunchd {
    /// Answers the given queries in order; the last answer repeats.
    fn answering(answers: Vec<JobQuery>) -> FakeLaunchd {
        FakeLaunchd {
            asked: Mutex::new(Vec::new()),
            answers: Mutex::new(answers),
            on_bootstrap: Box::new(|_| Ok(())),
            on_bootout: Box::new(|_| Ok(())),
        }
    }

    /// A launchd that loads what it is given and reports it running, as pid 4242.
    fn cooperative() -> FakeLaunchd {
        FakeLaunchd::answering(vec![JobQuery::Loaded(JobFacts {
            running: Some(true),
            pid: Some(4242),
            runs: Some(1),
            last_exit: Some("(never exited)".to_string()),
        })])
    }

    fn with_bootstrap(
        mut self,
        behaviour: impl Fn(&Path) -> Result<(), String> + Send + Sync + 'static,
    ) -> FakeLaunchd {
        self.on_bootstrap = Box::new(behaviour);
        self
    }

    fn questions(&self) -> Vec<String> {
        self.asked.lock().expect("not poisoned").clone()
    }
}

impl Launchctl for FakeLaunchd {
    fn bootstrap(&self, plist: &Path) -> Result<(), String> {
        self.asked
            .lock()
            .expect("not poisoned")
            .push(format!("bootstrap {}", plist.display()));
        (self.on_bootstrap)(plist)
    }

    fn bootout(&self, label: &str) -> Result<(), String> {
        self.asked
            .lock()
            .expect("not poisoned")
            .push(format!("bootout {label}"));
        (self.on_bootout)(label)
    }

    fn query(&self, label: &str) -> JobQuery {
        self.asked
            .lock()
            .expect("not poisoned")
            .push(format!("query {label}"));
        let mut answers = self.answers.lock().expect("not poisoned");
        if answers.len() > 1 {
            answers.remove(0)
        } else {
            answers
                .first()
                .cloned()
                .unwrap_or_else(|| JobQuery::Undetermined("the fake was given no answer".into()))
        }
    }

    fn describes_itself_as(&self) -> String {
        "a fake launchd inside the test process".to_string()
    }
}

/// Collect notices, recording for each one whether the plist existed at the moment it was said.
///
/// That pairing is the only way to assert "*before* creating it" as an ordering rather than as a
/// substring: a run that wrote the file first and mentioned it afterwards produces the same set
/// of lines.
fn install_watching_the_plist(
    request: &Install,
    launchctl: &dyn Launchctl,
) -> (Installed, Vec<(String, bool)>) {
    let plist = request.plist_path();
    let said: Mutex<Vec<(String, bool)>> = Mutex::new(Vec::new());
    let outcome = {
        let mut notice = |line: &str| {
            said.lock()
                .expect("not poisoned")
                .push((line.to_string(), plist.exists()));
        };
        install(request, launchctl, &mut notice)
    };
    (outcome, said.into_inner().expect("not poisoned"))
}

fn install_quietly(request: &Install, launchctl: &dyn Launchctl) -> Installed {
    install(request, launchctl, &mut |_| {})
}

/// Everything in a directory, sorted, so "exactly one file" is assertable.
fn entries(directory: &Path) -> Vec<String> {
    let mut names: Vec<String> = match std::fs::read_dir(directory) {
        Ok(reader) => reader
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect(),
        Err(_) => Vec::new(),
    };
    names.sort();
    names
}

/// The plist as the system's own property-list parser reads it.
///
/// `plutil` rather than a substring search, for the reason this project distrusts plausible
/// answers generally: a plist that reads correctly to a human and fails to parse is exactly the
/// install that looks fine and never loads. Reading a file in a scratch directory, writing
/// nothing.
fn as_the_system_parses_it(plist: &Path) -> serde_json::Value {
    let output = Command::new("/usr/bin/plutil")
        .args(["-convert", "json", "-o", "-"])
        .arg(plist)
        .output()
        .expect("plutil is present on macOS");
    assert!(
        output.status.success(),
        "plutil could not parse the plist this tool wrote ({:?}): {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("plutil emits JSON")
}

fn write_plist_of(job: &Job, root: &Path) -> PathBuf {
    let path = root.join(format!("{}.plist", job.label));
    std::fs::write(&path, job.plist()).expect("write the plist into the scratch directory");
    path
}

// --- The plist, as data ---

#[test]
fn the_plist_runs_amon_watch_and_names_exactly_one_job() {
    // Two things at once, both criteria. The job is `amon watch` — the monitor, not the
    // display, which cannot alert with no terminal open because there is no screen to draw
    // on. And there is exactly one job in the file: N7 forbids a second supervising process,
    // and the cheapest way to acquire one is a plist that quietly grew a second entry.
    let root = scratch("plist-program");
    let request = request_in(&root);
    let plist = write_plist_of(&request.job, &root);

    let parsed = as_the_system_parses_it(&plist);

    assert_eq!(
        parsed["Label"].as_str(),
        Some(LABEL),
        "the label launchd knows the job by must be the one this tool looks it up with"
    );
    let arguments: Vec<&str> = parsed["ProgramArguments"]
        .as_array()
        .expect("ProgramArguments is an array")
        .iter()
        .map(|value| value.as_str().expect("each argument is a string"))
        .collect();
    assert_eq!(
        arguments,
        vec![
            amon_binary().to_string_lossy().as_ref(),
            AmonVerb::Watch.name()
        ],
        "the job is the monitor, run by absolute path, with no verb but `watch`"
    );
    assert_eq!(
        request.job.plist().matches("<key>Label</key>").count(),
        1,
        "one job, one label: a second supervising job is exactly what N7 forbids"
    );
}

#[test]
fn the_plist_asks_launchd_to_start_the_monitor_and_to_keep_it_alive() {
    // The supervision story in its entirety (decision 31). `KeepAlive` is what makes the
    // monitor survive an unclean exit without a watchdog watching the watchdog, and
    // `RunAtLoad` is what makes it exist after a login nobody was present for.
    let root = scratch("plist-keepalive");
    let request = request_in(&root);
    let plist = write_plist_of(&request.job, &root);

    let parsed = as_the_system_parses_it(&plist);

    assert_eq!(
        parsed["KeepAlive"].as_bool(),
        Some(true),
        "without KeepAlive an unclean exit is a permanent silence"
    );
    assert_eq!(
        parsed["RunAtLoad"].as_bool(),
        Some(true),
        "a monitor that waits to be asked is not resident"
    );
    assert_eq!(
        parsed["ThrottleInterval"].as_u64(),
        Some(u64::from(THROTTLE_SECONDS)),
        "the restart floor is stated rather than inherited, because it is the period of a \
         crash loop and #28 reads that period"
    );
}

#[test]
fn the_plist_is_a_property_list_the_systems_own_parser_accepts() {
    // `plutil -lint`, read-only, on a file in a scratch directory. A plist launchd cannot
    // parse is an install that fails at `bootstrap` — or worse, one that loads today and not
    // after the next login.
    let root = scratch("plist-lint");
    let request = request_in(&root);
    let plist = write_plist_of(&request.job, &root);

    let output = Command::new("/usr/bin/plutil")
        .arg("-lint")
        .arg(&plist)
        .output()
        .expect("plutil is present on macOS");

    assert!(
        output.status.success(),
        "plutil -lint rejected the plist: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn a_path_carrying_xml_metacharacters_survives_into_the_plist_unchanged() {
    // The bug this catches is a corrupt plist on a machine whose home directory contains an
    // ampersand — one unescaped `&` and launchd rejects the whole file. Asserted through the
    // system parser, so what is checked is the path launchd would actually read, not the
    // bytes on either side of the escape.
    let root = scratch("plist-escaping");
    let awkward = root.join("state & \"logs\" <1>");
    std::fs::create_dir_all(&awkward).expect("create the awkward directory");

    let job = Job::monitor(&amon_binary(), &awkward, Vec::new());
    let plist = write_plist_of(&job, &root);

    let parsed = as_the_system_parses_it(&plist);
    assert_eq!(
        parsed["StandardErrorPath"].as_str(),
        Some(awkward.join(LOG_FILE).to_string_lossy().as_ref()),
        "the path launchd reads back must be the path it was given, character for character"
    );
}

#[test]
fn the_log_launchd_writes_lives_in_the_state_directory_and_nowhere_else() {
    // F24 counts every file this tool causes to exist, including the ones launchd opens on its
    // behalf. The monitor's own output is the only record of a job that will not stay up, so it
    // has to go somewhere — and the only place it may go is the state directory.
    let root = scratch("plist-log");
    let request = request_in(&root);
    let state = root.join("state");
    let plist = write_plist_of(&request.job, &root);

    let parsed = as_the_system_parses_it(&plist);

    for key in ["StandardOutPath", "StandardErrorPath"] {
        let path = PathBuf::from(
            parsed[key]
                .as_str()
                .unwrap_or_else(|| panic!("{key} is set")),
        );
        assert!(
            path.starts_with(&state),
            "{key} is {}, which is outside the state directory {}",
            path.display(),
            state.display()
        );
        assert_eq!(
            path.file_name().and_then(|name| name.to_str()),
            Some(LOG_FILE)
        );
    }
}

#[test]
fn the_plist_states_a_path_because_launchds_own_default_cannot_find_a_homebrew_git() {
    // Measured, not assumed: `launchctl print` on this machine reports a LaunchAgent's default
    // environment as `PATH => /usr/bin:/bin:/usr/sbin:/sbin`. The collectors invoke `git` and
    // `curl` by name, so a monitor inheriting that default would report every Homebrew-git
    // workspace as unreadable — a whole class of at-risk workspace silently missing from the
    // panel, on a machine where the same command works fine in a terminal.
    let root = scratch("plist-path");
    let request = request_in(&root);
    let plist = write_plist_of(&request.job, &root);

    let parsed = as_the_system_parses_it(&plist);
    let path = parsed["EnvironmentVariables"]["PATH"]
        .as_str()
        .expect("the job's PATH is stated");

    assert_eq!(path, JOB_PATH);
    for expected in ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin"] {
        assert!(
            path.contains(expected),
            "the job's PATH must include {expected}; it is {path}"
        );
    }
}

#[test]
fn the_installed_job_is_given_the_directories_this_install_was_run_with() {
    // Split-brain is the failure: `amon` writing one state directory while the `agtop` in the
    // user's terminal reads another, each of them healthy and disagreeing. If a variable is set
    // where `amon install` runs, the job carries it; if it is not set, the job is not given a
    // guess.
    let environment = propagated_environment(&|name| match name {
        "HOME" => Some("/Users/someone".to_string()),
        "ACMON_STATE_DIR" => Some("/Users/someone/scratch/state".to_string()),
        _ => None,
    });

    assert!(
        environment.contains(&("HOME".to_string(), "/Users/someone".to_string())),
        "the job needs HOME: every path this tool resolves falls back to it, and it fails \
         loudly without it; got {environment:?}"
    );
    assert!(
        environment.contains(&(
            "ACMON_STATE_DIR".to_string(),
            "/Users/someone/scratch/state".to_string()
        )),
        "a relocated state directory must reach the job, or the monitor and the display look \
         at different files; got {environment:?}"
    );
    assert!(
        !environment
            .iter()
            .any(|(name, _)| name == "ACMON_NOTIFY_CONFIG"),
        "a variable nobody set must not appear in the plist with an invented value; \
         got {environment:?}"
    );
}

// --- What is asked of launchd ---

#[test]
fn nothing_this_tool_asks_of_launchd_needs_sudo_or_the_system_domain() {
    // NF16. A per-user LaunchAgent needs no elevation, and a tool that asks for it once has
    // taught its user to give it — after which every later mistake is a root mistake.
    let plist = PathBuf::from("/Users/someone/Library/LaunchAgents/io.github.pmcfadin.acmon.plist");
    let invocations = [
        bootstrap_arguments(501, &plist),
        bootout_arguments(501, LABEL),
        print_arguments(501, LABEL),
    ];

    for arguments in &invocations {
        let line = arguments.join(" ");
        assert!(
            !line.contains("sudo"),
            "no invocation may ask for sudo: {line}"
        );
        assert!(
            !line.contains("system/"),
            "the system domain needs root; this job belongs to one user: {line}"
        );
        assert!(
            line.contains("gui/501"),
            "every invocation targets the installing user's own GUI domain: {line}"
        );
    }

    assert_eq!(service_target(501, LABEL), format!("gui/501/{LABEL}"));
}

#[test]
fn launchd_reporting_a_running_job_yields_its_pid_and_its_run_count() {
    // The run count is the closest thing to a restart record that exists before #28: a
    // monitor launchd has started twelve times is a monitor in a crash loop, and that is
    // readable from launchd's own output today.
    let facts = parse_print(PRINT_RUNNING);

    assert_eq!(facts.running, Some(true));
    assert_eq!(facts.pid, Some(1019));
    assert_eq!(facts.runs, Some(3));
    assert_eq!(facts.last_exit.as_deref(), Some("(never exited)"));
}

#[test]
fn a_loaded_job_that_is_not_running_is_reported_as_loaded_and_not_running() {
    // launchd prints no `pid =` line at all for a job it is not currently running, which is
    // why the state line is read rather than the pid's absence being interpreted. "Loaded but
    // not running" is the exact state of a monitor that keeps exiting, and collapsing it into
    // either neighbour hides a crash loop.
    let facts = parse_print(PRINT_NOT_RUNNING);

    assert_eq!(facts.running, Some(false));
    assert_eq!(facts.pid, None);
    assert_eq!(facts.runs, Some(12));
    assert_eq!(facts.last_exit.as_deref(), Some("1"));
}

#[test]
fn output_that_names_neither_a_state_nor_a_pid_leaves_running_undetermined() {
    // A future launchd that renames a field must not silently produce "not running" — that is
    // the calm, plausible, wrong answer, and it would read as a monitor that had stopped.
    let facts = parse_print("gui/501/io.github.pmcfadin.acmon = {\n\tactive count = 1\n}");

    assert_eq!(
        facts.running, None,
        "unknown must not collapse into false: got {facts:?}"
    );
    assert_eq!(facts.pid, None);
}

// --- Installing ---

#[test]
fn install_states_the_plist_it_will_create_before_the_file_exists() {
    // F24, in letter: it must say what file it is creating *before* creating it. Asserted as
    // an ordering against the filesystem, because a run that created the file and then
    // mentioned it would print exactly the same words.
    let root = scratch("install-announces");
    let request = request_in(&root);
    let launchd = FakeLaunchd::cooperative();

    let (outcome, said) = install_watching_the_plist(&request, &launchd);

    assert!(
        outcome.is_installed(),
        "this launchd loads what it is given: {outcome:?}"
    );
    let wanted = request.plist_path().display().to_string();
    let announced_before = said
        .iter()
        .any(|(line, plist_existed)| line.contains(&wanted) && !plist_existed);
    assert!(
        announced_before,
        "the path must be stated while the file does not yet exist; the run said {said:#?}"
    );
}

#[test]
fn a_successful_install_writes_the_plist_and_nothing_else_outside_its_own_directories() {
    // F24's central claim, and the one the documentation makes: the LaunchAgent plist is the
    // only file this product writes outside `~/.config/acmon/` and `~/.local/state/acmon/`.
    // Asserted by watching a whole scratch tree, so a stray dotfile or a leftover temporary
    // fails this test rather than joining the write set unnoticed.
    let root = scratch("install-one-file");
    let request = request_in(&root);
    let launchd = FakeLaunchd::cooperative();

    let outcome = install_quietly(&request, &launchd);
    assert!(outcome.is_installed(), "{outcome:?}");

    assert_eq!(
        entries(&request.launch_agents_dir),
        vec![format!("{LABEL}.plist")],
        "exactly one file, and it is the plist"
    );
    let mut outside = entries(&root);
    outside.retain(|name| name != "LaunchAgents" && name != "state");
    assert!(
        outside.is_empty(),
        "the install wrote {outside:?} outside the LaunchAgents, config and state directories"
    );
}

#[test]
fn install_creates_the_state_directory_so_launchd_can_open_the_monitors_log() {
    // A half-install that is easy to miss: launchd creates the file it is told to write stdout
    // to, but not the directory above it. Bootstrapped against a directory that does not exist
    // yet, the job fails to spawn — so `install` reports a loaded job that has never run, on a
    // machine where nothing is wrong but a missing `mkdir`.
    let root = scratch("install-state-dir");
    let request = request_in(&root);
    let launchd = FakeLaunchd::cooperative();

    let outcome = install_quietly(&request, &launchd);
    assert!(outcome.is_installed(), "{outcome:?}");

    let log = &request.job.log;
    assert!(
        log.parent().expect("the log has a directory").is_dir(),
        "the directory launchd will open {} in must exist before the job is loaded",
        log.display()
    );
}

#[test]
fn install_verifies_the_load_by_asking_launchd_rather_than_trusting_bootstrap() {
    // The criterion this ticket exists for. `bootstrap` exiting zero is not a loaded job:
    // reporting success on a job launchd does not have is silent non-monitoring wearing an
    // installation's clothes.
    let root = scratch("install-unverified");
    let request = request_in(&root);
    let launchd = FakeLaunchd::answering(vec![JobQuery::NotLoaded]);

    let outcome = install_quietly(&request, &launchd);

    assert!(
        !outcome.is_installed(),
        "launchd does not have the job, so the install did not happen: {outcome:?}"
    );
    let asked = launchd.questions();
    let bootstrapped = asked
        .iter()
        .position(|question| question.starts_with("bootstrap"))
        .expect("the plist was bootstrapped");
    let queried = asked
        .iter()
        .position(|question| question.starts_with("query"))
        .expect("launchd was asked whether the job is there");
    assert!(
        queried > bootstrapped,
        "the verification has to come after the load it verifies; asked {asked:?}"
    );
    assert!(
        outcome.message().contains(LABEL),
        "the failure must name the job launchd could not confirm: {}",
        outcome.message()
    );
}

#[test]
fn an_install_whose_job_never_loaded_removes_the_plist_it_wrote_and_says_so() {
    // The dangerous case is the half-install: a plist on disk and no job. It would load at the
    // next login and not before, so the machine is unmonitored for an unknowable period while
    // the file suggests otherwise. Rolled back, and the roll-back is stated — a silent
    // clean-up would leave the reader unable to tell what to try next.
    let root = scratch("install-rollback");
    let request = request_in(&root);
    let launchd =
        FakeLaunchd::answering(vec![JobQuery::NotLoaded]).with_bootstrap(|_| Err("EPERM".into()));

    let (outcome, said) = install_watching_the_plist(&request, &launchd);

    assert!(!outcome.is_installed(), "{outcome:?}");
    assert!(
        !request.plist_path().exists(),
        "the plist this run wrote must not survive a load that failed"
    );
    assert_eq!(
        entries(&request.launch_agents_dir),
        Vec::<String>::new(),
        "and nothing else may be left in the LaunchAgents directory either"
    );
    let message = outcome.message();
    assert!(
        message.contains("EPERM"),
        "the reason launchd gave has to reach the reader: {message}"
    );
    assert!(
        message.contains("removed") || said.iter().any(|(line, _)| line.contains("removed")),
        "what the failure left behind must be stated; message was {message}, run said {said:#?}"
    );
}

#[test]
fn an_install_that_cannot_remove_its_own_plist_says_the_plist_is_still_there() {
    // Roll-back can fail too, and then the half-install is real: a plist launchd will read at
    // the next login, with no job today. The one thing that must not happen is silence, so the
    // failure names the file and what to do about it.
    let root = scratch("install-rollback-fails");
    let request = request_in(&root);
    let directory = request.launch_agents_dir.clone();

    // The write of the plist succeeds; the directory then becomes read-only, so the unlink
    // that would undo it cannot. Sealing the directory from inside `bootstrap` is what puts
    // it between the write and the roll-back.
    let sealed = directory.clone();
    let launchd = FakeLaunchd::answering(vec![JobQuery::NotLoaded]).with_bootstrap(move |_| {
        let mut permissions = std::fs::metadata(&sealed)
            .expect("the directory exists by now")
            .permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o500);
        std::fs::set_permissions(&sealed, permissions).expect("seal the directory");
        Err("Operation not permitted".into())
    });

    let outcome = install_quietly(&request, &launchd);

    // Unsealed before any assertion can panic, so a failure here does not leave an
    // unreadable directory behind for the next run.
    let mut permissions = std::fs::metadata(&directory)
        .expect("metadata")
        .permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o700);
    std::fs::set_permissions(&directory, permissions).expect("unseal the directory");

    assert!(!outcome.is_installed(), "{outcome:?}");
    let message = outcome.message();
    assert!(
        message.contains(&request.plist_path().display().to_string()),
        "a plist that could not be removed must be named so a human can remove it: {message}"
    );
    assert!(
        message.contains("still"),
        "and the message must say it is still there rather than implying a clean failure: \
         {message}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn install_refuses_to_overwrite_a_plist_that_is_already_there() {
    // Someone else's file, or an older version of ours with a hand-edit in it. Overwriting is
    // destroying work this tool did not create, and doing it as a side effect of a verb called
    // `install` would be silent. The refusal names the file and the verb that removes it.
    let root = scratch("install-already");
    let request = request_in(&root);
    std::fs::create_dir_all(&request.launch_agents_dir).expect("create LaunchAgents");
    let existing = "<!-- somebody was here first -->";
    std::fs::write(request.plist_path(), existing).expect("write the incumbent");

    let launchd = FakeLaunchd::cooperative();
    let outcome = install_quietly(&request, &launchd);

    assert!(!outcome.is_installed(), "{outcome:?}");
    assert!(
        matches!(
            outcome,
            Installed::AlreadyInstalled {
                identical: false,
                ..
            }
        ),
        "an existing plist whose contents differ from ours is reported as such: {outcome:?}"
    );
    assert_eq!(
        std::fs::read_to_string(request.plist_path()).expect("still readable"),
        existing,
        "the incumbent file must be untouched"
    );
    assert!(
        outcome.message().contains("uninstall"),
        "the refusal has to name the way out: {}",
        outcome.message()
    );
    assert!(
        launchd
            .questions()
            .iter()
            .all(|q| !q.starts_with("bootstrap")),
        "nothing may be loaded on the strength of a file this run did not write: {:?}",
        launchd.questions()
    );
}

#[test]
fn install_refuses_when_the_binary_the_plist_would_point_at_is_not_there() {
    // A plist naming a path that does not exist is a job launchd will try to run and fail,
    // every ThrottleInterval, for ever. The check happens before anything is written, so a
    // refusal leaves the machine exactly as it was.
    let root = scratch("install-no-binary");
    let mut request = request_in(&root);
    let missing = root.join("bin").join("amon");
    request.job = Job::monitor(&missing, &root.join("state"), Vec::new());

    let launchd = FakeLaunchd::cooperative();
    let outcome = install_quietly(&request, &launchd);

    assert!(!outcome.is_installed(), "{outcome:?}");
    assert!(
        outcome.message().contains(&missing.display().to_string()),
        "the refusal must name the path it looked for: {}",
        outcome.message()
    );
    assert_eq!(
        entries(&request.launch_agents_dir),
        Vec::<String>::new(),
        "nothing may be written when the job could not work"
    );
    assert!(
        launchd.questions().is_empty(),
        "and launchd is not asked anything either: {:?}",
        launchd.questions()
    );
}

#[test]
fn install_will_not_point_a_plist_at_a_relative_path() {
    // launchd's working directory is not the shell's. A relative program path would resolve
    // against something the installer never saw, which is how a job runs the wrong binary or
    // none at all.
    let root = scratch("install-relative");
    let mut request = request_in(&root);
    request.job = Job::monitor(
        Path::new("target/debug/amon"),
        &root.join("state"),
        Vec::new(),
    );

    let outcome = install_quietly(&request, &FakeLaunchd::cooperative());

    assert!(!outcome.is_installed(), "{outcome:?}");
    assert!(
        outcome.message().contains("absolute"),
        "the refusal must say what is wrong with the path: {}",
        outcome.message()
    );
}

#[test]
fn install_says_the_job_cannot_monitor_yet_while_the_collection_loop_is_unbuilt() {
    // A LaunchAgent running a verb that exits non-zero is a job launchd restarts every
    // ThrottleInterval for ever. That is the true state of `amon watch` until #27 lands, and
    // an install that did not say so would hand someone a crash loop and call it monitoring.
    //
    // Keyed off the verb's own state, so this stops asserting a warning the moment the verb
    // stops needing one — a warning outliving its cause is the same defect in the other
    // direction.
    let root = scratch("install-watch-unbuilt");
    let request = request_in(&root);
    let (_, said) = install_watching_the_plist(&request, &FakeLaunchd::cooperative());
    let spoken = said
        .iter()
        .map(|(line, _)| line.as_str())
        .collect::<Vec<_>>()
        .join("\n");

    match AmonVerb::Watch.state() {
        VerbState::Available => assert!(
            !spoken.contains("#27"),
            "`watch` works now, so nothing should still be warning about #27:\n{spoken}"
        ),
        VerbState::Partial { tracked_as, .. } | VerbState::Planned { tracked_as } => assert!(
            spoken.contains(tracked_as),
            "`watch` cannot monitor yet, so the install must say so and name {tracked_as}:\n\
             {spoken}"
        ),
    }
}

// --- Uninstalling ---

#[test]
fn uninstall_unloads_the_job_and_removes_the_plist_leaving_nothing_behind() {
    // "Leaving no orphaned plist" is the criterion, and both halves are load-bearing: a plist
    // with no job loads at the next login, and a job with no plist keeps running with nothing
    // on disk to explain it.
    let root = scratch("uninstall-clean");
    let request = request_in(&root);
    let launchd = FakeLaunchd::answering(vec![
        JobQuery::Loaded(JobFacts {
            running: Some(true),
            pid: Some(4242),
            runs: Some(1),
            last_exit: None,
        }),
        JobQuery::NotLoaded,
    ]);
    assert!(
        install_quietly(&request, &FakeLaunchd::cooperative()).is_installed(),
        "the fixture installs first"
    );

    let outcome = uninstall(&request, &launchd, &mut |_| {});

    assert!(
        matches!(
            outcome,
            Uninstalled::Removed {
                was_loaded: true,
                ..
            }
        ),
        "{outcome:?}"
    );
    assert_eq!(
        entries(&request.launch_agents_dir),
        Vec::<String>::new(),
        "no orphaned plist"
    );
    let asked = launchd.questions();
    assert!(
        asked
            .iter()
            .any(|question| question == &format!("bootout {LABEL}")),
        "the job has to be unloaded by name: {asked:?}"
    );
    assert!(
        asked.iter().filter(|q| q.starts_with("query")).count() >= 2,
        "and the unload has to be verified, not assumed: {asked:?}"
    );
}

#[test]
fn uninstall_with_nothing_installed_says_there_was_nothing_to_remove() {
    // Not a failure: the end state this verb exists to reach already holds, and it was
    // *checked* rather than assumed — launchd was asked, and the plist's absence was read from
    // the filesystem. Claiming to have removed something would be the invention; refusing
    // would make `brew uninstall` fail on a machine that never installed the job.
    let root = scratch("uninstall-nothing");
    let request = request_in(&root);
    let launchd = FakeLaunchd::answering(vec![JobQuery::NotLoaded]);

    let outcome = uninstall(&request, &launchd, &mut |_| {});

    assert!(
        matches!(outcome, Uninstalled::NothingToRemove { .. }),
        "{outcome:?}"
    );
    assert!(outcome.succeeded(), "the end state holds: {outcome:?}");
}

#[test]
fn uninstall_keeps_the_plist_when_launchd_still_reports_the_job_loaded() {
    // Removing the file would leave a loaded job with nothing on disk to unload it by, and no
    // trace of where it came from. So the plist stays, and the failure is loud: a job that
    // would not unload is a thing a human has to look at.
    let root = scratch("uninstall-still-loaded");
    let request = request_in(&root);
    assert!(install_quietly(&request, &FakeLaunchd::cooperative()).is_installed());

    let stubborn = FakeLaunchd::answering(vec![JobQuery::Loaded(JobFacts {
        running: Some(true),
        pid: Some(4242),
        runs: Some(1),
        last_exit: None,
    })]);
    let outcome = uninstall(&request, &stubborn, &mut |_| {});

    assert!(!outcome.succeeded(), "{outcome:?}");
    assert!(
        request.plist_path().exists(),
        "the plist must survive: it is the only handle left on a job that would not unload"
    );
    assert!(
        outcome.message().contains(LABEL),
        "the failure must name the job still loaded: {}",
        outcome.message()
    );
}

#[test]
fn uninstall_refuses_to_remove_a_plist_when_launchd_cannot_be_asked() {
    // The one answer that is not an answer. Removing the file here might strand a running job
    // for ever, and leaving it alone costs nothing but a second attempt — so the tool does the
    // recoverable thing and says why.
    let root = scratch("uninstall-undetermined");
    let request = request_in(&root);
    assert!(install_quietly(&request, &FakeLaunchd::cooperative()).is_installed());

    let mute = FakeLaunchd::answering(vec![JobQuery::Undetermined(
        "launchctl could not be run".to_string(),
    )]);
    let outcome = uninstall(&request, &mute, &mut |_| {});

    assert!(!outcome.succeeded(), "{outcome:?}");
    assert!(
        request.plist_path().exists(),
        "a plist must not be removed on the strength of an answer nobody got"
    );
    assert!(
        outcome.message().contains("launchctl could not be run"),
        "the reason has to reach the reader: {}",
        outcome.message()
    );
    assert!(
        mute.questions()
            .iter()
            .all(|question| !question.starts_with("bootout")),
        "nothing is unloaded either, because nothing is known: {:?}",
        mute.questions()
    );
}

// --- Status ---

/// A store over a scratch state directory, reading the same files the monitor writes.
fn store_in(root: &Path) -> StateStore {
    let state = root.join("state");
    let paths = Paths::from_values(
        Some(&root.join("config").to_string_lossy()),
        Some(&state.to_string_lossy()),
        None,
    )
    .expect("both directories given explicitly");
    StateStore::new(paths)
}

#[test]
fn status_reports_the_job_the_process_and_the_age_of_the_last_write() {
    // The three questions the verb exists to answer, all three of them at once, because the
    // combination is what tells a reader whether the machine is being watched.
    let root = scratch("status-healthy");
    let request = request_in(&root);
    let store = store_in(&root);
    store
        .write_tiered_state(STATE_FILE, &TieredState::new(std::process::id()))
        .expect("publish a state file");

    let launchd = FakeLaunchd::answering(vec![JobQuery::Loaded(JobFacts {
        running: Some(true),
        pid: Some(std::process::id()),
        runs: Some(1),
        last_exit: Some("(never exited)".to_string()),
    })]);
    let report = status(&request, &launchd, &store, SystemTime::now(), &|pid| {
        pid == std::process::id()
    });

    assert!(
        report.complete(),
        "every question was answerable: {:#?}",
        report.lines()
    );
    assert!(
        matches!(report.job, JobQuery::Loaded(_)),
        "{:?}",
        report.job
    );
    assert!(
        matches!(report.process, ProcessAnswer::Running { .. }),
        "{:?}",
        report.process
    );
    assert!(
        matches!(report.last_write, LastWrite::Age { .. }),
        "{:?}",
        report.last_write
    );
    let lines = report.lines().join("\n");
    for expected in ["loaded", "running", "last write"] {
        assert!(
            lines.to_lowercase().contains(expected),
            "the report must state {expected}:\n{lines}"
        );
    }
}

#[test]
fn status_never_reports_a_state_file_that_was_never_written_as_a_fresh_one() {
    // The fail-to-zero this verb would be most likely to commit: no state file, an age of
    // zero, and a screen that reads as a monitor which wrote something a moment ago.
    let root = scratch("status-no-state");
    let request = request_in(&root);
    let store = store_in(&root);
    let launchd = FakeLaunchd::answering(vec![JobQuery::NotLoaded]);

    let report = status(&request, &launchd, &store, SystemTime::now(), &|_| false);

    assert!(
        matches!(report.last_write, LastWrite::Absent { .. }),
        "{:?}",
        report.last_write
    );
    let lines = report.lines().join("\n");
    assert!(
        lines.contains(STATE_FILE),
        "the report names the file it looked for:\n{lines}"
    );
    assert!(
        !lines.contains(" 0s") && !lines.contains("0 seconds"),
        "an absent write has no age, and must not be given one:\n{lines}"
    );
    assert!(
        report.complete(),
        "\"nothing has ever been written\" is an answer, not a failure to answer:\n{lines}"
    );
}

#[test]
fn status_finds_a_monitor_running_outside_launchd_from_the_state_files_writer() {
    // `amon watch --foreground` in a terminal is a monitor with no job loaded. Reporting "not
    // running" because launchd has never heard of it would be wrong in the direction that
    // matters least — but it would also tell someone to install a second writer.
    let root = scratch("status-foreground");
    let request = request_in(&root);
    let store = store_in(&root);
    store
        .write_tiered_state(STATE_FILE, &TieredState::new(std::process::id()))
        .expect("publish a state file");

    let launchd = FakeLaunchd::answering(vec![JobQuery::NotLoaded]);
    let report = status(&request, &launchd, &store, SystemTime::now(), &|pid| {
        pid == std::process::id()
    });

    match report.process {
        ProcessAnswer::Running { pid, .. } => assert_eq!(pid, std::process::id()),
        other => panic!("a live writer pid is a running monitor: {other:?}"),
    }
    let lines = report.lines().join("\n");
    assert!(
        lines.contains(STATE_FILE) && lines.contains(&std::process::id().to_string()),
        "and the report says where that answer came from:\n{lines}"
    );
}

#[test]
fn status_that_could_not_ask_launchd_says_so_instead_of_reporting_the_job_absent() {
    // "I could not tell" and "it is not installed" are different facts, and collapsing them
    // is how a broken `launchctl` reads as a machine nobody ever installed the monitor on.
    let root = scratch("status-undetermined");
    let request = request_in(&root);
    let store = store_in(&root);
    let launchd = FakeLaunchd::answering(vec![JobQuery::Undetermined(
        "launchctl exited 1 with no output".to_string(),
    )]);

    let report = status(&request, &launchd, &store, SystemTime::now(), &|_| false);

    assert!(!report.complete(), "{:#?}", report.lines());
    let lines = report.lines().join("\n");
    assert!(
        lines.contains("launchctl exited 1 with no output"),
        "the reason has to reach the reader:\n{lines}"
    );
}

#[test]
fn status_reports_the_durable_launch_record_rather_than_launchds_own_run_count() {
    // A `KeepAlive` restart must not be silent, and the answer is the record seam 17 appends, not
    // launchd's own count: that count resets whenever the job is reloaded, knows nothing about how
    // long the machine went unmonitored, and cannot separate a clean stop from a `SIGKILL`. Where
    // the record answers, launchd's count is left out — two counts of the same thing would leave
    // the reader reconciling them.
    let root = scratch("status-restarts");
    let request = request_in(&root);
    let store = store_in(&root);

    // Two launches on record, the second following a monitor that died holding the lock.
    let first = starts::decide(
        SystemTime::now() - Duration::from_secs(600),
        4242,
        &LastStateWrite::Never,
        None,
        None,
        &History::NothingRecorded,
    );
    starts::append(&store, &first).expect("append the first launch");
    let second = starts::decide(
        SystemTime::now() - Duration::from_secs(60),
        4243,
        &LastStateWrite::At(SystemTime::now() - Duration::from_secs(90)),
        Some(&Predecessor {
            pid: 4242,
            still_running: false,
        }),
        None,
        &starts::history(&store),
    );
    starts::append(&store, &second).expect("append the second launch");

    let launchd = FakeLaunchd::answering(vec![JobQuery::Loaded(JobFacts {
        running: Some(false),
        pid: None,
        runs: Some(37),
        last_exit: Some("1".to_string()),
    })]);

    let report = status(&request, &launchd, &store, SystemTime::now(), &|_| false);
    let lines = report.lines().join("\n");

    assert!(
        lines.contains("launches: 2"),
        "the durable count is the answer to how many times the monitor has started:\n{lines}"
    );
    assert!(
        lines.contains("30s of downtime"),
        "and it carries the downtime, which launchd's count cannot:\n{lines}"
    );
    assert!(
        lines.contains("did not exit cleanly") && lines.contains("4242"),
        "and it names the monitor that died, which launchd's exit code cannot:\n{lines}"
    );
    // Asserted against the sentence launchd's count is reported in, not against the number.
    // `!lines.contains("37")` was the same claim and was flaky about once in three runs: the
    // report prints an ISO 8601 timestamp, so any launch beginning at 37 minutes or 37 seconds
    // past put "37" on screen with nothing to do with a run count. A substring that short is a
    // coincidence waiting for a clock, and a test that fails by the hour teaches a reader to
    // re-run rather than to look.
    assert!(
        !lines.contains("by launchd since this job was loaded"),
        "launchd's own run count must give way to the record rather than sit beside it as a \
         second answer:\n{lines}"
    );
    assert!(
        matches!(report.process, ProcessAnswer::NotRunning { .. }),
        "loaded and not running is a determinate answer: {:?}",
        report.process
    );
    assert!(
        report.complete(),
        "every question was answered, including the launch record's:\n{:#?}",
        report.unanswered()
    );
}

#[test]
fn status_falls_back_to_launchds_own_run_count_only_where_no_launch_record_exists() {
    // The complement, and the reason the fallback stays: a state directory with no record at all —
    // a machine whose monitor has never run under a build that keeps one — still has launchd's
    // count, and reporting nothing would be worse than reporting the weaker fact. It has to say
    // which fact it is.
    let root = scratch("status-restarts-fallback");
    let request = request_in(&root);
    let store = store_in(&root);
    let launchd = FakeLaunchd::answering(vec![JobQuery::Loaded(JobFacts {
        running: Some(false),
        pid: None,
        runs: Some(37),
        last_exit: Some("1".to_string()),
    })]);

    let report = status(&request, &launchd, &store, SystemTime::now(), &|_| false);
    let lines = report.lines().join("\n");

    assert!(
        lines.contains("started 37 times by launchd")
            && lines.contains("all launchd itself can say"),
        "with no record, launchd's count is the only evidence and is labelled as such:\n{lines}"
    );
    assert!(
        lines.contains("launches: none recorded"),
        "and the absence of the durable record is stated rather than left to be inferred from a \
         missing line:\n{lines}"
    );
    assert!(
        report.complete(),
        "\"nothing has ever launched here\" is an answer, not a failure to get one:\n{:#?}",
        report.unanswered()
    );
}

#[test]
fn status_will_not_report_a_launch_record_it_cannot_read_as_a_machine_nothing_has_run_on() {
    // The fail-loud rule on the one file that says whether the monitor has been cycling. A record
    // that cannot be parsed reported as "none recorded" would say a crash-looping monitor has never
    // started, which is the calm, plausible, wrong answer.
    let root = scratch("status-restarts-unreadable");
    let request = request_in(&root);
    let store = store_in(&root);
    std::fs::create_dir_all(store.paths().state_dir()).expect("create the state directory");
    std::fs::write(starts::path(&store), "the monitor was here\n").expect("write a bad record");

    let report = status(
        &request,
        &launchd_that_answers_nothing(),
        &store,
        SystemTime::now(),
        &|_| false,
    );
    let lines = report.lines().join("\n");

    assert!(
        lines.contains("launches: UNDETERMINED"),
        "an unreadable record is undetermined, never none:\n{lines}"
    );
    assert!(
        !report.complete(),
        "and status fails rather than letting it read as a negative answer:\n{lines}"
    );
    assert!(
        report
            .unanswered()
            .iter()
            .any(|missing| missing.contains("how many times the monitor has launched")),
        "and it says which question went unanswered: {:#?}",
        report.unanswered()
    );
}

#[test]
fn a_state_file_stamped_ahead_of_the_clock_is_not_reported_as_freshly_written() {
    // Clock skew, or a file restored from a backup. An age computed by subtraction would
    // underflow into something enormous or be clamped to zero; either way it would be a
    // number, and a number here reads as a measurement.
    let root = scratch("status-future");
    let request = request_in(&root);
    let store = store_in(&root);
    store
        .write_tiered_state(STATE_FILE, &TieredState::new(std::process::id()))
        .expect("publish a state file");

    // The clock is a parameter, so "the file is in the future" needs no `utimes`: ask about a
    // moment before the write happened.
    let an_hour_ago = SystemTime::now() - Duration::from_secs(3600);
    let report = status(
        &request,
        &launchd_that_answers_nothing(),
        &store,
        an_hour_ago,
        &|_| true,
    );

    assert!(
        matches!(report.last_write, LastWrite::AheadOfTheClock { .. }),
        "{:?}",
        report.last_write
    );
    assert!(
        !report.complete(),
        "an age that cannot be computed is not an age:\n{:#?}",
        report.lines()
    );
}

fn launchd_that_answers_nothing() -> FakeLaunchd {
    FakeLaunchd::answering(vec![JobQuery::Undetermined("not asked about".to_string())])
}

// --- The binary itself ---
//
// Two cases only, both of them failures, and neither of them reaching real launchd: what an
// exit code does is the behaviour under test, and that cannot be observed from inside the
// library. `ACMON_LAUNCHCTL` points at `/usr/bin/false` — a launchd that refuses everything —
// so no job is registered with anybody's session.

fn amon(root: &Path, arguments: &[&str]) -> (bool, String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_amon"))
        .args(arguments)
        .env(acmon::state::STATE_DIR_VARIABLE, root.join("state"))
        .env(acmon::state::CONFIG_DIR_VARIABLE, root.join("config"))
        .env(
            acmon::launchd::LAUNCH_AGENTS_VARIABLE,
            root.join("LaunchAgents"),
        )
        .env(acmon::launchd::LAUNCHCTL_VARIABLE, "/usr/bin/false")
        .stdin(Stdio::null())
        .output()
        .expect("amon is built and runnable");
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[test]
fn amon_install_exits_non_zero_when_the_job_did_not_load() {
    // The exit code is the whole point. A LaunchAgent, a Homebrew caveat, or a person reading
    // a zero here concludes the machine is being monitored, and this is the one command whose
    // success nobody re-checks.
    let root = scratch("binary-install-fails");

    let (success, stdout, stderr) = amon(&root, &["install"]);

    assert!(
        !success,
        "no job was loaded, so `amon install` must not exit zero:\n{stderr}"
    );
    assert!(
        stderr.contains(&format!("{LABEL}.plist")),
        "the failure must name the plist it was working on:\n{stderr}"
    );
    assert!(
        stderr.contains("/usr/bin/false"),
        "and it must say that launchd was reached through an override, because a run that \
         talked to something other than launchd must never read as a real install:\n{stderr}"
    );
    assert_eq!(
        entries(&root.join("LaunchAgents")),
        Vec::<String>::new(),
        "the failed install left a plist behind:\n{stdout}\n{stderr}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn amon_uninstall_exits_non_zero_rather_than_removing_a_plist_it_could_not_ask_about() {
    // `/usr/bin/false` is a launchd that will not say whether the job is loaded. Refusing is
    // the recoverable half of the choice, and the exit code has to carry it: a zero here would
    // tell a `brew uninstall` script that the job is gone when it may still be running.
    let root = scratch("binary-uninstall-refuses");
    let plist = root.join("LaunchAgents").join(format!("{LABEL}.plist"));
    std::fs::create_dir_all(plist.parent().expect("parent")).expect("create LaunchAgents");
    std::fs::write(&plist, "<!-- pretend -->").expect("write a plist to leave alone");

    let (success, _, stderr) = amon(&root, &["uninstall"]);

    assert!(
        !success,
        "an uninstall that knows nothing must fail:\n{stderr}"
    );
    assert!(
        plist.exists(),
        "and it must leave the plist alone:\n{stderr}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn amon_status_prints_its_report_and_fails_when_a_question_went_unanswered() {
    // Fail loud, never fail to zero, applied to a reporting verb: the report still goes to
    // stdout, because a reader needs whatever could be determined, and the exit code says that
    // one of the three answers is missing rather than negative.
    let root = scratch("binary-status");

    let (success, stdout, stderr) = amon(&root, &["status"]);

    assert!(
        !stdout.trim().is_empty(),
        "status must report what it could determine on stdout:\n{stderr}"
    );
    assert!(
        !success,
        "launchd could not be asked, so the report is incomplete:\n{stdout}\n{stderr}"
    );
    assert!(
        !stderr.trim().is_empty(),
        "and it must say which question went unanswered"
    );

    let _ = std::fs::remove_dir_all(&root);
}
