use std::process::ExitCode;

use acmon::{collect, render, RealWorld};

fn main() -> ExitCode {
    let world = RealWorld::new();

    match collect(&world) {
        Ok(snapshot) => {
            let height = render::required_height(&snapshot);
            // Standard terminal width; no crossterm dependency needed for one-shot output
            let width = 80;
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
