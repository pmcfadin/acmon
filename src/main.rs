use std::process::ExitCode;

use acmon::{collect, render, RealWorld};

fn main() -> ExitCode {
    let world = RealWorld::new();

    match collect(&world) {
        Ok(snapshot) => {
            // Height: a bordered block, a header row, and one row per session.
            let height = (snapshot.sessions.len() + 4) as u16;
            for line in render::render_to_lines(&snapshot, 78, height) {
                println!("{line}");
            }
            ExitCode::SUCCESS
        }
        Err(error) => {
            // Say what went wrong. Never print an empty table on failure — that would
            // be indistinguishable from a machine with no agents running.
            eprintln!("acmon: could not collect a trustworthy snapshot: {error:?}");
            ExitCode::FAILURE
        }
    }
}
