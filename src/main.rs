use std::process::ExitCode;
use std::time::SystemTime;

use acmon::liveness::Thresholds;
use acmon::{collect, render, RealWorld, World};

fn main() -> ExitCode {
    let world = RealWorld::new();

    // Refuse to start on a threshold that cannot be read, rather than quietly using the
    // default. Someone who set one and got the default anyway would be reading verdicts
    // produced by a rule they believe they replaced.
    let thresholds = match Thresholds::from_environment() {
        Ok(thresholds) => thresholds,
        Err(error) => {
            eprintln!("acmon: {error}");
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
            eprintln!("acmon: {}", error);
            ExitCode::FAILURE
        }
    }
}
