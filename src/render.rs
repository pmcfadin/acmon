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

use ratatui::backend::TestBackend;
use ratatui::layout::Constraint;
use ratatui::widgets::{Block, Borders, Row, Table};
use ratatui::{Frame, Terminal};

use crate::collect::Snapshot;

/// Calculate the required height to render a snapshot without blank rows.
///
/// Height = top border + header row + N session rows + bottom border.
pub fn required_height(snapshot: &Snapshot) -> u16 {
    (snapshot.sessions.len() + 3) as u16
}

/// Draw a snapshot into a frame. The single source of truth for layout.
pub fn draw(frame: &mut Frame, snapshot: &Snapshot) {
    let title = format!(" acmon — {} agent session(s) ", snapshot.sessions.len());

    let rows: Vec<Row> = snapshot
        .sessions
        .iter()
        .map(|s| Row::new(vec![s.pid.to_string(), s.cli.clone()]))
        .collect();

    let table = Table::new(rows, [Constraint::Length(9), Constraint::Min(10)])
        .header(Row::new(vec!["PID", "CLI"]))
        .block(Block::default().borders(Borders::ALL).title(title));

    frame.render_widget(table, frame.area());
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
