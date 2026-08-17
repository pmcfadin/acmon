//! Turning a snapshot into terminal output.
//!
//! The drawing code is shared between production and tests. Tests drive it through
//! [`render_to_lines`], which renders into an in-memory buffer, so rendering is
//! verifiable without a terminal.
//!
//! NOTE: Production currently renders through `TestBackend`, which is architecturally
//! wrong but pragmatic for one-shot output. The live TUI (ticket #10) will use a real
//! terminal backend. Similarly, `crossterm` is intentionally absent from dependencies
//! until the TUI is needed.

use std::time::Duration;

use ratatui::backend::TestBackend;
use ratatui::layout::{Constraint, Layout};
use ratatui::widgets::{Block, Borders, Paragraph, Row, Table};
use ratatui::{Frame, Terminal};

use crate::collect::{Session, Snapshot};
use crate::world::Resources;

/// The caveat that travels with every child-CPU figure.
///
/// Work that detached or was orphaned before being reaped never enters the parent's
/// ledger — measured in `docs/observability-mechanics.md` §2.4, where 0.004 s of a
/// 0.6 s double-forked burner was all that was attributed. So the numbers below are
/// lower bounds, and saying so is part of reporting them honestly.
const FLOOR_CAVEAT: &str =
    "Child CPU omits detached and orphaned work, so these are floors, not totals.";

/// What the marker on a state means.
///
/// No signal for "blocked waiting on a human" exists to be read — it was looked for four
/// ways and never emitted — so WAITING is reached by inference from silence. A reader has
/// to be able to tell that apart from a state that was observed, which is why the marker
/// exists rather than the states simply being printed.
const INFERENCE_MARKER_CAVEAT: &str =
    "A state marked ? was inferred from silence, not observed directly.";

/// The fixed columns, in order. Each width holds that column's widest value *or* the
/// longest reason for a value's absence, since a reason is printed in the value's place.
const FIXED_COLUMNS: [(&str, u16); 8] = [
    ("PID", 6),
    ("CLI", 6),
    // Eight, not seven: the longest state is seven characters and an inferred verdict
    // carries a trailing marker. Abbreviating STALLED to STALL would be a truncated
    // state, which is a wrong state rather than a shorter one.
    ("STATE", 8),
    ("OWN CPU", 9),
    ("CHILD CPU", 9),
    ("MEM", 8),
    ("PEAK", 8),
    ("WRITTEN", 8),
];

/// The last column, which absorbs whatever width is left over.
const WORKSPACE_HEADER: &str = "WORKSPACE";

/// The narrowest the workspace column may be.
///
/// Sized to hold `unknown: no-perm`, the longest thing that can appear there other than
/// a path — and paths are shortened with a visible mark, while a reason must not be.
const WORKSPACE_MIN: u16 = 16;

/// One space between columns, matching `ratatui`'s default spacing.
const COLUMN_SPACING: u16 = 1;

/// Everything a row spends that is not a column's own content: borders and separators.
fn row_overhead() -> u16 {
    let columns = FIXED_COLUMNS.len() as u16 + 1;
    let borders = 2;
    COLUMN_SPACING * (columns - 1) + borders
}

fn fixed_content_width() -> u16 {
    FIXED_COLUMNS.iter().map(|(_, w)| w).sum()
}

/// The narrowest terminal that can hold the table without truncating a number.
pub fn minimum_width() -> u16 {
    fixed_content_width() + WORKSPACE_MIN + row_overhead()
}

/// How much room the workspace column gets at a given total width.
///
/// Derived rather than declared, and used both for the column constraint and for
/// shortening the path, so the two cannot disagree — if they did, `ratatui` would cut
/// the path silently and unmarked.
fn workspace_width(total: u16) -> u16 {
    total
        .saturating_sub(fixed_content_width() + row_overhead())
        .max(WORKSPACE_MIN)
}

/// Calculate the required height to render a snapshot without blank rows.
///
/// Needs the width because a terminal too narrow for the table gets a refusal instead
/// of the table, and the two have different heights.
pub fn required_height(snapshot: &Snapshot, width: u16) -> u16 {
    if width < minimum_width() {
        return wrap_words(&too_narrow_message(width), width).len() as u16;
    }
    // top border + header row + one row per session + bottom border + the caveats.
    (snapshot.sessions.len() + 3) as u16 + footer_lines(snapshot, width).len() as u16
}

/// The caveats printed under the table.
///
/// Produced by one function so the height calculation and the drawing cannot disagree; if
/// they did, a caveat would be silently clipped, and a clipped caveat is worse than none
/// because the numbers then look unqualified.
///
/// The inference caveat appears only when some verdict actually was inferred. Printing it
/// unconditionally would train a reader to ignore it.
fn footer_lines(snapshot: &Snapshot, width: u16) -> Vec<String> {
    let mut lines = wrap_words(FLOOR_CAVEAT, width);
    if snapshot
        .sessions
        .iter()
        .any(|session| session.liveness.method.is_inferred())
    {
        lines.extend(wrap_words(INFERENCE_MARKER_CAVEAT, width));
    }
    lines
}

fn too_narrow_message(width: u16) -> String {
    format!(
        "acmon needs {} columns to print these numbers without truncating them; \
         this terminal has {}. Widen it — a truncated CPU total is a plausible \
         wrong answer, so none is printed.",
        minimum_width(),
        width
    )
}

/// Draw a snapshot into a frame. The single source of truth for layout.
pub fn draw(frame: &mut Frame, snapshot: &Snapshot) {
    let area = frame.area();

    if area.width < minimum_width() {
        let message = wrap_words(&too_narrow_message(area.width), area.width).join("\n");
        frame.render_widget(Paragraph::new(message), area);
        return;
    }

    let caveats = footer_lines(snapshot, area.width);
    let [table_area, caveat_area] =
        Layout::vertical([Constraint::Min(3), Constraint::Length(caveats.len() as u16)])
            .areas(area);

    let title = format!(" acmon — {} agent session(s) ", snapshot.sessions.len());
    let workspace_width = workspace_width(area.width);
    let rows: Vec<Row> = snapshot
        .sessions
        .iter()
        .map(|session| row_for(session, workspace_width))
        .collect();

    let mut constraints: Vec<Constraint> = FIXED_COLUMNS
        .iter()
        .map(|(_, width)| Constraint::Length(*width))
        .collect();
    constraints.push(Constraint::Length(workspace_width));

    let mut headers: Vec<&str> = FIXED_COLUMNS.iter().map(|(header, _)| *header).collect();
    headers.push(WORKSPACE_HEADER);

    let table = Table::new(rows, constraints)
        .header(Row::new(headers))
        .column_spacing(COLUMN_SPACING)
        .block(Block::default().borders(Borders::ALL).title(title));

    frame.render_widget(table, table_area);
    frame.render_widget(Paragraph::new(caveats.join("\n")), caveat_area);
}

/// A state, with a marker when the verdict behind it was inferred rather than observed.
///
/// The marker is the whole reason the state column is eight wide. Without it, a WAITING
/// reached by guessing at a human's intent would be indistinguishable from an ACTIVE
/// established by a transcript that changed a second ago.
fn state_cell(verdict: &crate::liveness::Verdict) -> String {
    let state = match verdict.state {
        crate::liveness::State::Active => "ACTIVE",
        crate::liveness::State::Waiting => "WAITING",
        crate::liveness::State::Stalled => "STALLED",
        crate::liveness::State::Unknown => "UNKNOWN",
    };
    if verdict.method.is_inferred() {
        format!("{state}?")
    } else {
        state.to_string()
    }
}

/// One session's row: its identity, its ledger or the reason it has none, then where it
/// is working or the reason that is unknown.
fn row_for(session: &Session, workspace_width: u16) -> Row<'static> {
    let figures = match &session.resources {
        Ok(resources) => figures_of(resources),
        // Nothing was read. Every figure carries the same reason rather than the row
        // being dropped or shown as idle — the session is running either way.
        Err(reason) => [
            reason.to_string(),
            reason.to_string(),
            reason.to_string(),
            reason.to_string(),
            reason.to_string(),
        ],
    };

    let workspace = match &session.workspace {
        Ok(workspace) => shorten_from_the_left(&workspace.path, workspace_width),
        Err(unknown) => unknown.to_string(),
    };

    let mut cells = vec![
        session.pid.to_string(),
        session.cli.clone(),
        state_cell(&session.liveness),
    ];
    cells.extend(figures);
    cells.push(workspace);
    Row::new(cells)
}

/// Keep the end of a path, marking that the beginning was dropped.
///
/// The tail carries a workspace's identity; the head is shared by everything under the
/// same home directory. An unmarked cut could name a directory that exists and is not
/// this one, which is the same class of defect as a truncated CPU total.
fn shorten_from_the_left(text: &str, width: u16) -> String {
    let width = width as usize;
    let characters: Vec<char> = text.chars().collect();
    if characters.len() <= width || width == 0 {
        return text.to_string();
    }
    let kept: String = characters[characters.len() - (width - 1)..]
        .iter()
        .collect();
    format!("…{kept}")
}

fn figures_of(resources: &Resources) -> [String; 5] {
    [
        show(resources.own_cpu.as_ref().map(format_cpu)),
        show(resources.children_cpu.as_ref().map(format_cpu)),
        show(resources.current_memory.as_ref().map(format_bytes)),
        show(resources.peak_memory.as_ref().map(format_bytes)),
        show(resources.bytes_written.as_ref().map(format_bytes)),
    ]
}

/// A figure, or the reason it is missing. Never a zero standing in for either.
fn show<E: std::fmt::Display>(figure: Result<String, E>) -> String {
    match figure {
        Ok(text) => text,
        Err(reason) => reason.to_string(),
    }
}

/// CPU time at the precision a reader can act on: hours and minutes for the long
/// totals, seconds only while they are still small.
fn format_cpu(duration: &Duration) -> String {
    let secs = duration.as_secs();
    if secs >= 3_600 {
        format!("{}h{:02}m", secs / 3_600, (secs % 3_600) / 60)
    } else if secs >= 60 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{:.1}s", duration.as_secs_f64())
    }
}

/// Byte counts in decimal units, matching how the mechanics document reports them.
fn format_bytes(bytes: &u64) -> String {
    match *bytes {
        b if b >= 1_000_000_000 => format!("{:.1} GB", b as f64 / 1_000_000_000.0),
        b if b >= 1_000_000 => format!("{} MB", b / 1_000_000),
        b if b >= 1_000 => format!("{} kB", b / 1_000),
        b => format!("{} B", b),
    }
}

/// Wrap on word boundaries.
///
/// Hand-rolled rather than delegated to `Paragraph::wrap` so that the line count used
/// for layout is computed by the same code that produces the lines. Two different
/// answers there would show up as a clipped or padded caveat.
fn wrap_words(text: &str, width: u16) -> Vec<String> {
    let width = width.max(1) as usize;
    let mut lines: Vec<String> = Vec::new();
    let mut line = String::new();

    for word in text.split_whitespace() {
        let needed = if line.is_empty() {
            word.chars().count()
        } else {
            line.chars().count() + 1 + word.chars().count()
        };
        if needed > width && !line.is_empty() {
            lines.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}

/// Render a snapshot into an in-memory buffer and return it as text lines.
///
/// Used by tests, and by anything that wants the output without a terminal.
pub fn render_to_lines(snapshot: &Snapshot, width: u16, height: u16) -> Vec<String> {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("in-memory terminal");
    terminal
        .draw(|frame| draw(frame, snapshot))
        .expect("drawing into an in-memory buffer cannot fail");

    let buffer = terminal.backend().buffer().clone();
    (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect()
}
