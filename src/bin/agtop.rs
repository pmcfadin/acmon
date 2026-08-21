//! `agtop` — the display. Draws; it never measures, records or notifies.
//!
//! Thin by design. Every decision it renders was made in the library — the polling rules in
//! [`acmon::display`], the words in [`acmon::render`] — so that the monitor and the display can
//! never disagree about what a verdict means, and so that none of it needs a terminal to test.
//!
//! What lives here and nowhere else is the terminal itself: entering the alternate screen,
//! waiting for an event with a timeout, and leaving the terminal as it was found.

use std::io::IsTerminal;
use std::process::ExitCode;
use std::time::{Duration, Instant, SystemTime};

use acmon::cli::{agtop_usage, parse_agtop, AgtopRequest, ONCE_FLAG};
use acmon::display::{self, Poll, Poller, Screen, StateReading, POLL_INTERVAL};
use acmon::liveness::Thresholds;
use acmon::render;
use acmon::state::{Paths, StateStore, STATE_FILE};
use acmon::{RealWorld, World};

fn main() -> ExitCode {
    match parse_agtop(std::env::args().skip(1)) {
        Ok(AgtopRequest::Help) => {
            print!("{}", agtop_usage());
            ExitCode::SUCCESS
        }
        Ok(AgtopRequest::Once) => once(),
        Ok(AgtopRequest::Live) => live(),
        Err(error) => {
            eprintln!("agtop: {error}");
            eprint!("{}", agtop_usage());
            ExitCode::FAILURE
        }
    }
}

/// Everything the display needs before it can draw anything.
///
/// Resolved once, and refused rather than defaulted. Someone who set a threshold or a state
/// directory and got the built-in one anyway would be reading a screen produced by rules they
/// believe they replaced.
struct Standing {
    world: RealWorld,
    thresholds: Thresholds,
    store: StateStore,
}

fn standing() -> Result<Standing, String> {
    Ok(Standing {
        world: RealWorld::new(),
        thresholds: Thresholds::from_environment()?,
        store: StateStore::new(Paths::from_environment()?),
    })
}

/// Read the state file, then assemble the screen that describes what was found.
///
/// The display's own collection is made **once per run** and reused, which is what F28 asks for
/// — "its own single collection". A display that re-collected on every poll would spend the
/// 2.7 s git sweep once a second to watch a machine nobody is monitoring, becoming the tax it
/// exists to measure.
fn screen_for(
    standing: &Standing,
    reading: &StateReading,
    own: &mut Option<(Result<acmon::Snapshot, String>, Duration, SystemTime)>,
) -> Screen {
    let (facts, overhead, taken_at) = own.get_or_insert_with(|| {
        // The clock is read once, here, and injected. Everything downstream is deterministic
        // given that instant, which is what makes a liveness verdict testable.
        let now = SystemTime::now();
        let (facts, overhead) = display::own_collection(&standing.world, now, &standing.thresholds);
        (facts, overhead, now)
    });

    Screen::from_own_collection(reading, facts.clone(), *overhead, *taken_at)
}

/// One pass, as plain lines.
fn once() -> ExitCode {
    let standing = match standing() {
        Ok(standing) => standing,
        Err(error) => {
            eprintln!("agtop: {error}");
            return ExitCode::FAILURE;
        }
    };

    let reading = display::read_state_file(&standing.store);
    let mut own = None;
    let screen = screen_for(&standing, &reading, &mut own);

    let width = standing.world.output_width();
    let height = render::screen_height(&screen, width);
    let lines = render::screen_to_lines(&screen, width, height);

    // A screen with no figures on it is not a rendering; it is a stated failure, and it goes
    // to stderr with a non-zero exit so that a pipeline cannot read it as a quiet machine.
    if screen.facts.is_err() {
        for line in lines {
            eprintln!("{line}");
        }
        return ExitCode::FAILURE;
    }
    for line in lines {
        println!("{line}");
    }
    ExitCode::SUCCESS
}

/// Full screen, refreshing while open.
fn live() -> ExitCode {
    // Refused rather than drawn into a pipe. Escape sequences and an alternate screen written
    // to something that is not a terminal produce a file nobody can read, and `--once` is the
    // mode that exists for exactly this.
    if !std::io::stdout().is_terminal() {
        eprintln!(
            "agtop: stdout is not a terminal, so there is no screen to take. Use `agtop \
             {ONCE_FLAG}` for one pass as plain lines."
        );
        return ExitCode::FAILURE;
    }

    let standing = match standing() {
        Ok(standing) => standing,
        Err(error) => {
            eprintln!("agtop: {error}");
            return ExitCode::FAILURE;
        }
    };

    let mut terminal = match ratatui::try_init() {
        Ok(terminal) => terminal,
        Err(error) => {
            eprintln!("agtop: the terminal could not be prepared: {error}");
            return ExitCode::FAILURE;
        }
    };

    let outcome = draw_until_told_to_stop(&standing, &mut terminal);

    // Restored before anything is printed, always — including after a failure. A terminal left
    // in raw mode on the alternate screen is a broken shell, and the reason the run ended would
    // be invisible in it.
    if let Err(error) = ratatui::try_restore() {
        eprintln!("agtop: the terminal could not be restored: {error}");
        return ExitCode::FAILURE;
    }

    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("agtop: {error}");
            ExitCode::FAILURE
        }
    }
}

/// The loop: poll, re-read only what moved, draw, wait.
fn draw_until_told_to_stop(
    standing: &Standing,
    terminal: &mut ratatui::DefaultTerminal,
) -> Result<(), String> {
    let state_file = standing.store.paths().state_dir().join(STATE_FILE);
    let mut poller = Poller::new();
    let mut own = None;

    // The first poll can never be `Unchanged`, so the first turn always reads and draws.
    let mut reading = StateReading::Absent;
    let mut redraw = true;

    loop {
        if poller.observe(display::stat_state_file(&state_file)) != Poll::Unchanged {
            reading = display::read_state_file(&standing.store);
            redraw = true;
        }

        if redraw {
            let screen = screen_for(standing, &reading, &mut own);
            terminal
                .draw(|frame| render::draw_screen(frame, &screen))
                .map_err(|error| format!("the screen could not be drawn: {error}"))?;
            redraw = false;
        }

        match wait_for_a_command(POLL_INTERVAL)? {
            Some(display::Command::Quit) => return Ok(()),
            // `ratatui` re-measures the terminal on the next draw, so a resize needs nothing
            // but another pass. What a screen too short for everything should drop is #34.
            Some(display::Command::Redraw) => redraw = true,
            Some(display::Command::Ignore) | None => {}
        }
    }
}

/// Wait up to `budget` for something worth acting on.
///
/// Returns `None` when the budget expired with nothing to report, which is the ordinary case:
/// the display is a poller, and the wait is how it keeps to its interval without spinning.
fn wait_for_a_command(budget: Duration) -> Result<Option<display::Command>, String> {
    use ratatui::crossterm::event;

    let deadline = Instant::now() + budget;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(None);
        }
        if !event::poll(remaining).map_err(|error| format!("the terminal went quiet: {error}"))? {
            return Ok(None);
        }
        let event = event::read().map_err(|error| format!("the terminal went quiet: {error}"))?;
        match display::command_for(&event) {
            // An ignored event does not end the wait: a mouse move must not shorten the poll
            // interval, and a stream of them must not turn the interval into a busy loop.
            display::Command::Ignore => continue,
            command => return Ok(Some(command)),
        }
    }
}
