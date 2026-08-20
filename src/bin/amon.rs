//! `amon` — the monitor. Measures, records and notifies; it never draws.
//!
//! Today it is a verb surface and nothing more: every verb it advertises is still tracked
//! work. It exists ahead of its implementations on purpose, so that the two names are real
//! from the first commit of the split — but a verb that cannot do its job **fails**. A
//! LaunchAgent that ran `amon watch` and saw a zero would report a healthy monitor for as
//! long as the verb stayed unbuilt, which is the calm, plausible, wrong answer this project
//! exists to eliminate, arriving through an exit code.

use std::process::ExitCode;

use acmon::cli::{amon_usage, parse_amon, AmonRequest, VerbState};

fn main() -> ExitCode {
    match parse_amon(std::env::args().skip(1)) {
        Ok(AmonRequest::Help) => {
            print!("{}", amon_usage());
            ExitCode::SUCCESS
        }

        Ok(AmonRequest::Verb(verb)) => match verb.state() {
            VerbState::Planned { tracked_as } => {
                eprintln!(
                    "amon: `{}` is recognised but not built yet ({tracked_as}). \
                     Nothing was measured, recorded or notified.",
                    verb.name()
                );
                ExitCode::FAILURE
            }
            // Unreachable while every verb is planned. Left as an arm rather than a
            // catch-all so that adding an implementation is a compile-time prompt to wire
            // it up here.
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
