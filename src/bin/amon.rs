//! `amon` — the monitor. Measures, records and notifies; it never draws.
//!
//! Thin by design: `watch`'s lifecycle lives in the library (`acmon::watch`), so it can be
//! exercised without spawning a process, and so the monitor and the display can never disagree
//! about what it did.
//!
//! Every verb it advertises either does its job or **fails**. A LaunchAgent that ran a verb and
//! saw a zero would report a healthy monitor for as long as that verb stayed unbuilt, which is
//! the calm, plausible, wrong answer this project exists to eliminate, arriving through an exit
//! code. `watch` is built: it takes the single-writer lock (#26) and drives every tier from one
//! loop (#27), so it is the one verb that can exit zero — and it only does so having actually
//! monitored, reporting how many passes each tier completed and what the run cost.

use std::process::ExitCode;
use std::time::SystemTime;

use acmon::cli::{amon_usage, parse_amon, AmonRequest, AmonVerb, VerbState};
use acmon::launchd::{install, status, uninstall, Install, SystemLaunchctl, Uninstalled};
use acmon::state::StateStore;
use acmon::watch::{watch, WatchOptions, WatchStopped};

fn main() -> ExitCode {
    match parse_amon(std::env::args().skip(1)) {
        Ok(AmonRequest::Help) => {
            print!("{}", amon_usage());
            ExitCode::SUCCESS
        }

        Ok(AmonRequest::Verb {
            verb: AmonVerb::Watch,
            foreground,
        }) => run_watch(foreground),

        Ok(AmonRequest::Verb {
            verb: AmonVerb::Install,
            ..
        }) => run_install(),

        Ok(AmonRequest::Verb {
            verb: AmonVerb::Uninstall,
            ..
        }) => run_uninstall(),

        Ok(AmonRequest::Verb {
            verb: AmonVerb::Status,
            ..
        }) => run_status(),

        Ok(AmonRequest::Verb { verb, .. }) => match verb.state() {
            VerbState::Planned { tracked_as } => {
                eprintln!(
                    "amon: `{}` is recognised but not built yet ({tracked_as}). \
                     Nothing was measured, recorded or notified.",
                    verb.name()
                );
                ExitCode::FAILURE
            }
            // Both unreachable while `watch` is the only partial verb and no verb is
            // available. Left as arms rather than a catch-all so that building either one is a
            // compile-time prompt to wire it up here.
            VerbState::Partial { tracked_as, .. } => {
                eprintln!(
                    "amon: `{}` is partly built ({tracked_as}) but has no entry point wired up",
                    verb.name()
                );
                ExitCode::FAILURE
            }
            VerbState::Available => {
                eprintln!(
                    "amon: `{}` is marked available but has no implementation wired up",
                    verb.name()
                );
                ExitCode::FAILURE
            }
        },

        // Both "nothing asked for" and "asked for something unknown" end the same way: say
        // what happened, show what was available, and fail. Neither did any work.
        Err(error) => {
            eprintln!("amon: {error}");
            eprint!("{}", amon_usage());
            ExitCode::FAILURE
        }
    }
}

/// `amon watch`: hold the lock for the run's lifetime, and say what happened at every step.
///
/// Everything the run has to say goes to stderr, including the things that went right. stdout
/// belongs to output a caller might parse, and this verb produces none.
fn run_watch(foreground: bool) -> ExitCode {
    let options = match WatchOptions::from_environment(foreground) {
        Ok(options) => options,
        Err(reason) => {
            eprintln!("amon: watch cannot start: {reason}");
            return ExitCode::FAILURE;
        }
    };

    let mut notice = |line: &str| eprintln!("amon: watch: {line}");

    match watch(&options, &mut notice) {
        WatchStopped::LockRefused(refusal) => {
            eprintln!("amon: watch is not starting: {refusal}");
            ExitCode::FAILURE
        }
        WatchStopped::StateUnwritable(reason) => {
            eprintln!(
                "amon: watch holds the lock but cannot write state, so it is stopping: {reason}"
            );
            ExitCode::FAILURE
        }
        WatchStopped::LockNotReleased(reason) => {
            eprintln!(
                "amon: watch finished but could not release its lock, so the next start may be \
                 refused: {reason}"
            );
            ExitCode::FAILURE
        }
        // The one arm that exits zero, and it says what the run came to. A monitor that stopped
        // having collected nothing would exit zero here too if the count were not reported, so
        // the counts go on stderr where a LaunchAgent's log will keep them.
        WatchStopped::Finished(finished) => {
            let passes: Vec<String> = finished
                .passes
                .iter()
                .map(|(tier, count)| format!("{count} {tier}"))
                .collect();
            eprintln!(
                "amon: watch: ran {:.1}s and stopped because {}. Passes: {}.",
                finished.ran_for.as_secs_f64(),
                finished.because,
                passes.join(", ")
            );
            match (
                finished.monitor.duty_cycle.value,
                finished.monitor.duty_cycle.unavailable.as_deref(),
            ) {
                (Some(duty), _) => eprintln!(
                    "amon: watch: measured duty cycle {:.3}% of one core over the trailing {}s \
                     (budget {:.1}%).",
                    duty * 100.0,
                    finished.monitor.window_secs,
                    finished.monitor.budget * 100.0
                ),
                (None, Some(why)) => {
                    eprintln!("amon: watch: the duty cycle was not measurable: {why}")
                }
                (None, None) => eprintln!(
                    "amon: watch: the duty cycle is neither a figure nor a reason, which is a bug \
                     in the monitor's own metering"
                ),
            }
            ExitCode::SUCCESS
        }
    }
}

/// What the three LaunchAgent verbs need: where the plist goes, and how launchd is reached.
///
/// Resolved once, and reported as a failure rather than guessed at. A LaunchAgents directory
/// chosen because it was probably right is a job nobody can find again.
fn launchd_request(verb: &str) -> Result<(Install, SystemLaunchctl), ExitCode> {
    match Install::from_environment() {
        Ok(request) => Ok((request, SystemLaunchctl::from_environment())),
        Err(reason) => {
            eprintln!("amon: {verb} cannot run: {reason}");
            Err(ExitCode::FAILURE)
        }
    }
}

/// `amon install`: write the LaunchAgent, load it, and verify the load with launchd.
///
/// Exits non-zero unless launchd confirms the job. Reporting success on a job that never loaded
/// is the exact failure this verb exists to prevent — the machine would be unmonitored and
/// nothing on it would say so.
fn run_install() -> ExitCode {
    let (request, launchctl) = match launchd_request("install") {
        Ok(resolved) => resolved,
        Err(code) => return code,
    };

    let mut notice = |line: &str| eprintln!("amon: install: {line}");
    let outcome = install(&request, &launchctl, &mut notice);

    if outcome.is_installed() {
        eprintln!("amon: install: {}", outcome.message());
        ExitCode::SUCCESS
    } else {
        eprintln!(
            "amon: install did not install anything: {}",
            outcome.message()
        );
        ExitCode::FAILURE
    }
}

/// `amon uninstall`: unload the job and remove the plist, verifying both.
fn run_uninstall() -> ExitCode {
    let (request, launchctl) = match launchd_request("uninstall") {
        Ok(resolved) => resolved,
        Err(code) => return code,
    };

    let mut notice = |line: &str| eprintln!("amon: uninstall: {line}");
    let outcome = uninstall(&request, &launchctl, &mut notice);

    if outcome.succeeded() {
        eprintln!("amon: uninstall: {}", outcome.message());
        // "There was nothing to remove" is a success, because the state this verb exists to
        // reach holds and was checked rather than assumed. It is still said out loud, so nobody
        // reads a zero as "the job you thought was installed has been removed".
        if matches!(outcome, Uninstalled::NothingToRemove { .. }) {
            eprintln!(
                "amon: uninstall: nothing was installed here, so nothing changed on this machine"
            );
        }
        ExitCode::SUCCESS
    } else {
        eprintln!("amon: uninstall did not finish: {}", outcome.message());
        ExitCode::FAILURE
    }
}

/// `amon status`: whether the job is loaded, whether a process is running, and the age of the
/// last write.
///
/// The report goes to stdout, because a reader wants whatever could be determined. The exit code
/// says whether all three questions were *answered* — not whether the answers were good news. A
/// monitor that is switched off is a determinate answer, and conflating it with a question
/// nobody could answer would destroy the distinction this verb exists to draw.
fn run_status() -> ExitCode {
    let (request, launchctl) = match launchd_request("status") {
        Ok(resolved) => resolved,
        Err(code) => return code,
    };

    let paths = match acmon::state::Paths::from_environment() {
        Ok(paths) => paths,
        Err(reason) => {
            eprintln!("amon: status cannot run: {reason}");
            return ExitCode::FAILURE;
        }
    };
    let store = StateStore::new(paths);

    let report = status(&request, &launchctl, &store, SystemTime::now(), &|pid| {
        acmon::real_world::process_exists(pid as libc::pid_t)
    });

    for line in report.lines() {
        println!("{line}");
    }

    if report.complete() {
        ExitCode::SUCCESS
    } else {
        for missing in report.unanswered() {
            eprintln!("amon: status could not determine {missing}");
        }
        eprintln!(
            "amon: status is incomplete, so it is failing rather than letting an unanswered \
             question read as a negative answer"
        );
        ExitCode::FAILURE
    }
}
