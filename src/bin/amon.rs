//! `amon` — the monitor. Measures, records and notifies; it never draws.
//!
//! Thin by design: `watch`'s lifecycle lives in the library (`acmon::watch`), so it can be
//! exercised without spawning a process, and so the monitor and the display can never disagree
//! about what it did.
//!
//! Every verb it advertises either does its job or **fails**. A LaunchAgent that ran
//! `amon watch` and saw a zero would report a healthy monitor for as long as the collection
//! loop stayed unbuilt, which is the calm, plausible, wrong answer this project exists to
//! eliminate, arriving through an exit code. `watch` today takes the single-writer lock (#26)
//! and publishes which pid holds it; the loop that would give that lock something to protect is
//! #27, so the run still ends non-zero.

use std::process::ExitCode;

use acmon::cli::{amon_usage, parse_amon, AmonRequest, AmonVerb, VerbState};
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
        WatchStopped::LoopNotBuilt { tracked_as } => {
            eprintln!(
                "amon: `watch` took the single-writer lock and released it cleanly, but the \
                 tiered collection loop is not built yet ({tracked_as}). No tier was collected \
                 and nothing was notified, so this is a failure rather than a monitor."
            );
            ExitCode::FAILURE
        }
    }
}
