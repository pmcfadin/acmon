use std::process::ExitCode;
use std::time::SystemTime;

use acmon::{collect, render, RealWorld, World};

fn main() -> ExitCode {
    let world = RealWorld::new();

    // The clock is read once, here, and injected. Everything downstream is deterministic
    // given that instant, which is what makes a liveness verdict testable.
    match collect(&world, SystemTime::now()) {
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
