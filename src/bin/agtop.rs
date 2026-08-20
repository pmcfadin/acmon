//! `agtop` — the display. Draws; it never measures, records or notifies.
//!
//! Thin by design. Every decision it renders was made in the library, so that the monitor
//! and the display can never disagree about what a verdict means.

use std::process::ExitCode;
use std::time::SystemTime;

use acmon::cli::agtop_usage;
use acmon::liveness::Thresholds;
use acmon::{collect, render, RealWorld, World};

fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();

    match arguments.first().map(String::as_str) {
        None => draw(),
        Some("--help" | "-h") => {
            print!("{}", agtop_usage());
            ExitCode::SUCCESS
        }
        // Deliberately strict. `agtop watch` reading as a successful monitor start would
        // undo the whole point of there being two names.
        Some(unexpected) => {
            eprintln!("agtop: unexpected argument `{unexpected}`");
            eprint!("{}", agtop_usage());
            ExitCode::FAILURE
        }
    }
}

fn draw() -> ExitCode {
    let world = RealWorld::new();

    // Refuse to start on a threshold that cannot be read, rather than quietly using the
    // default. Someone who set one and got the default anyway would be reading verdicts
    // produced by a rule they believe they replaced.
    let thresholds = match Thresholds::from_environment() {
        Ok(thresholds) => thresholds,
        Err(error) => {
            eprintln!("agtop: {error}");
            return ExitCode::FAILURE;
        }
    };

    // The clock is read once, here, and injected. Everything downstream is deterministic
    // given that instant, which is what makes a liveness verdict testable.
    match collect(&world, SystemTime::now(), &thresholds) {
        Ok(snapshot) => {
            let width = world.output_width();
            let height = render::required_height(&snapshot, width);
            for line in render::render_to_lines(&snapshot, width, height) {
                println!("{line}");
            }
            ExitCode::SUCCESS
        }
        Err(error) => {
            // Say what went wrong. Never print an empty table on failure — that would
            // be indistinguishable from a machine with no agents running.
            eprintln!("agtop: {}", error);
            ExitCode::FAILURE
        }
    }
}
