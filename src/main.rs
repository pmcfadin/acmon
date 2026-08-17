use std::process::ExitCode;

use acmon::{collect, render, RealWorld, World};

fn main() -> ExitCode {
    let world = RealWorld::new();

    match collect(&world) {
        Ok(snapshot) => {
            let height = render::required_height(&snapshot);
            for line in render::render_to_lines(&snapshot, world.output_width(), height) {
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
