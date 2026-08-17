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

/// Column headers and widths, in order. The width has to hold the widest value *or*
/// the longest reason for a value's absence, since a reason is printed in its place.
const COLUMNS: [(&str, u16); 7] = [
    ("PID", 7),
    ("CLI", 8),
    ("OWN CPU", 10),
    ("CHILD CPU", 10),
    ("MEM", 10),
    ("PEAK", 10),
    ("WRITTEN", 10),
];

/// One space between columns, matching `ratatui`'s default spacing.
const COLUMN_SPACING: u16 = 1;

/// The narrowest terminal that can hold the table without truncating a number.
fn minimum_width() -> u16 {
    let content: u16 = COLUMNS.iter().map(|(_, w)| w).sum();
    let separators = COLUMN_SPACING * (COLUMNS.len() as u16 - 1);
    let borders = 2;
    content + separators + borders
}

/// Calculate the required height to render a snapshot without blank rows.
///
/// Needs the width because a terminal too narrow for the table gets a refusal instead
/// of the table, and the two have different heights.
pub fn required_height(snapshot: &Snapshot, width: u16) -> u16 {
    if width < minimum_width() {
        return wrap_words(&too_narrow_message(width), width).len() as u16;
    }
    // top border + header row + one row per session + bottom border + the caveat.
    (snapshot.sessions.len() + 3) as u16 + wrap_words(FLOOR_CAVEAT, width).len() as u16
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

    let caveat_height = wrap_words(FLOOR_CAVEAT, area.width).len() as u16;
    let [table_area, caveat_area] =
        Layout::vertical([Constraint::Min(3), Constraint::Length(caveat_height)]).areas(area);

    let title = format!(" acmon — {} agent session(s) ", snapshot.sessions.len());
    let rows: Vec<Row> = snapshot.sessions.iter().map(row_for).collect();
    let constraints: Vec<Constraint> = COLUMNS
        .iter()
        .map(|(_, width)| Constraint::Length(*width))
        .collect();

    let table = Table::new(rows, constraints)
        .header(Row::new(COLUMNS.map(|(header, _)| header)))
        .column_spacing(COLUMN_SPACING)
        .block(Block::default().borders(Borders::ALL).title(title));

    frame.render_widget(table, table_area);
    frame.render_widget(
        Paragraph::new(wrap_words(FLOOR_CAVEAT, area.width).join("\n")),
        caveat_area,
    );
}

/// One session's row: its identity, then its ledger or the reason it has none.
fn row_for(session: &Session) -> Row<'static> {
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

    let mut cells = vec![session.pid.to_string(), session.cli.clone()];
    cells.extend(figures);
    Row::new(cells)
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
