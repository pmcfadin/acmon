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

use crate::collect::{Identity, Session, Snapshot, WorkspaceReport};
use crate::vcs::{Unreadable, WorkspaceState};
use crate::workspace::NamespaceResolution;
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

/// What the marker on a figure means.
///
/// Almost all of an agent's cost is in its children, and only the process that reaped them
/// can report that total — so once it exits, the figure exists nowhere on the machine except
/// in what an earlier run wrote down. Showing it is the point of remembering it. Showing it
/// without saying when it was taken would make a remembered total indistinguishable from a
/// live one, which is the same defect in the opposite direction.
const STALE_MARKER_CAVEAT: &str =
    "A figure marked * is the last reading taken before that session's process exited, not a \
     current one.";

/// How much room a CLI's id gets.
///
/// Sized to hold `cursor-agent`, the longest plausible fifth-CLI id on this machine, so the
/// common cases are never marked. Named rather than inlined because the row builder has to
/// shorten to exactly this width, and two numbers that must agree should be one number.
const CLI_WIDTH: u16 = 12;

/// The fixed columns, in order. Each width holds that column's widest value *or* the
/// longest reason for a value's absence, since a reason is printed in the value's place.
/// The byte columns are NINE rather than eight because `999.9 GB` is already eight and a
/// remembered figure carries a trailing `*`. Left at eight, ratatui would cut the marker off
/// the widest values — silently, and precisely on the rows where the figure is a memory
/// rather than a measurement.
const FIXED_COLUMNS: [(&str, u16); 8] = [
    ("PID", 6),
    // Twelve, not six. Six fitted `claude` and `codex` exactly, and those were the only two
    // ids possible until detectors became user-configurable (#12) — after which an id is
    // whatever the user typed. `cursor-agent` is twelve characters and rendered as `cursor`,
    // silently, which is a different CLI's name rather than a shorter version of this one.
    //
    // Twelve is not a guarantee, because no fixed width can be: an id longer than this is
    // shortened with a visible mark instead. See `shorten_with_a_mark`.
    ("CLI", CLI_WIDTH),
    // Eight, not seven: the longest state is seven characters and an inferred verdict
    // carries a trailing marker. Abbreviating STALLED to STALL would be a truncated
    // state, which is a wrong state rather than a shorter one.
    ("STATE", 8),
    ("OWN CPU", 9),
    ("CHILD CPU", 9),
    ("MEM", 9),
    ("PEAK", 9),
    ("WRITTEN", 9),
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
    // Session table: top border + header row + one row per session + bottom border
    let session_table_height = (snapshot.sessions.len() + 3) as u16;

    // At-risk panel: always present. Top border + title row + content rows + summary lines + bottom border.
    // The title row is a header inside the bordered block.
    let (_, panel_rows, panel_summary) = at_risk_panel_content(snapshot, width);
    let panel_height = 3 + panel_rows.len() as u16 + panel_summary.len() as u16;

    // Footer caveats
    let footer_height = footer_lines(snapshot, width).len() as u16;

    session_table_height + panel_height + footer_height
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

    // The remembered-figure caveat, and each remembered figure's age. The age is per row
    // rather than a single sentence because two sessions' last readings can be hours apart,
    // and "how old is this number" is the only question a reader can have about it.
    let stale: Vec<&Session> = snapshot
        .sessions
        .iter()
        .filter(|session| session.last_reading.is_some())
        .collect();
    if !stale.is_empty() {
        lines.extend(wrap_words(STALE_MARKER_CAVEAT, width));
        for session in stale {
            let reading = session
                .last_reading
                .as_ref()
                .expect("filtered to sessions that have one");
            let age = snapshot
                .taken_at
                .duration_since(reading.taken_at)
                .map(|age| format!("{} ago", format_age(age)))
                // A reading stamped in the future means the clock moved backwards between
                // runs. Say that rather than print a negative age or silently show "0s ago",
                // which would present the oldest possible figure as the freshest.
                .unwrap_or_else(|_| "at an unknown time — the clock moved backwards".to_string());
            lines.extend(wrap_words(
                &format!("  * {}: last read {}", identify(session), age),
                width,
            ));
        }
    }

    lines.extend(memory_lines(snapshot, width));
    lines
}

/// What a row is called when it has to be named in prose rather than pointed at.
///
/// The PID column reads `gone` for every transcript-derived session, so a pid alone does not
/// identify those rows. The transcript identity does, and it is the same string the row shows
/// in its workspace column.
fn identify(session: &Session) -> String {
    match &session.identity {
        Identity::Process { pid } => format!("{} {}", pid, session.cli),
        Identity::Transcript { recorded_as } => format!("{} {}", recorded_as, session.cli),
    }
}

/// What has to be said about the state carried between runs.
///
/// Says nothing when the state was read and stored, which is the ordinary case. Every line
/// here reports something that changes how the rest of the output should be read: a lost
/// history makes the at-risk list shorter than it should be, and a failed store makes the
/// NEXT run's list shorter than it should be.
fn memory_lines(snapshot: &Snapshot, width: u16) -> Vec<String> {
    let mut lines = Vec::new();
    let remembered = &snapshot.remembered;

    if let Some(unusable) = &remembered.unusable {
        lines.extend(wrap_words(
            &format!(
                "WARNING: {unusable} — this run started with no history, so a workspace whose \
                 session has already exited may be missing from the list above.",
            ),
            width,
        ));
    }

    if let Err(why) = &remembered.persisted {
        lines.extend(wrap_words(
            &format!("WARNING: {why} — the next run will start with no history."),
            width,
        ));
    }

    if !remembered.forgotten.is_empty() {
        lines.extend(wrap_words(
            &format!(
                "Stopped watching {} workspace(s) that had been clean and quiet for over {}.",
                remembered.forgotten.len(),
                format_age(remembered.retention),
            ),
            width,
        ));
    }

    // Detector configuration health
    //
    // An unusable detector config is reported **unconditionally**, and first. A typo in the
    // user's detector file means a fifth agent CLI silently stops being recognised — the
    // sessions simply are not there, which is indistinguishable from a quiet machine and
    // exactly the failure this whole project exists to remove.
    if let Some(why) = &remembered.detector_config.unusable {
        lines.extend(wrap_words(
            &format!(
                "WARNING: detector configuration is unusable — sessions from user-configured \
                 CLIs will not be recognised ({why})."
            ),
            width,
        ));
    }

    // Notification channel health
    let health = &remembered.notify_health;

    // A configuration that could not be understood is reported **unconditionally**, and
    // before anything else about the channels. It delivers nothing, exactly like a machine
    // that was never set up to alert — and that second state is silent by design, so this one
    // would otherwise be silent by accident. This is the failure the ticket opens with: an
    // exhausted quota swallowed a full day of alerts because a dead channel and a calm machine
    // produced identical output.
    if let Some(why) = &health.config.unusable {
        lines.extend(wrap_words(
            &format!(
                "WARNING: notification configuration is unusable, so NOTHING WAS ANNOUNCED \
                 this run ({why})."
            ),
            width,
        ));
    }

    if !health.config.has_any() {
        // A monitor with no alerting wired must say so rather than silently never alerting —
        // but only when there was actually something it would have announced. Saying it on a
        // quiet machine every run trains a reader to ignore the line, and the line matters.
        // Suppressed when the configuration is unusable, because the warning above is the
        // truer account of the same silence.
        if health.config.unusable.is_none() && health.notable > 0 {
            lines.extend(wrap_words(
                "WARNING: No notification channels configured — notable states were observed \
                 but not announced.",
                width,
            ));
        }
    } else if health.has_failures() {
        // At least one channel is configured, and at least one delivery failed.
        let mut parts = Vec::new();
        if health.local_failed > 0 {
            parts.push(format!("local: {} failed", health.local_failed));
        }
        if health.remote_failed > 0 {
            parts.push(format!("remote: {} failed", health.remote_failed));
        }
        lines.extend(wrap_words(
            &format!(
                "WARNING: Notification delivery failures ({}) — these alerts will be \
                 re-announced on the next run.",
                parts.join(", ")
            ),
            width,
        ));
    }

    lines
}

/// A duration at the coarsest precision that still says something: days for the retention
/// period, hours and minutes for a reading's age, seconds only while it is still small.
fn format_age(age: Duration) -> String {
    let seconds = age.as_secs();
    match seconds {
        s if s >= 172_800 => format!("{} days", s / 86_400),
        s if s >= 86_400 => "1 day".to_string(),
        s if s >= 3_600 => format!("{}h{:02}m", s / 3_600, (s % 3_600) / 60),
        s if s >= 60 => format!("{}m{:02}s", s / 60, s % 60),
        s => format!("{s}s"),
    }
}

/// Build the at-risk workspace panel's content: rows and summary.
///
/// The SINGLE source of truth for both the height calculation and the drawing. If these
/// ever diverged, the panel would be silently clipped, and a clipped at-risk list is the
/// calm plausible wrong answer this project exists to remove.
///
/// The panel lists workspaces holding uncommitted work that no live session is driving,
/// and workspaces whose version-control state could not be read. This is the reason the
/// project exists: three sessions' work was lost in one day, and a workspace holding 27
/// uncommitted files was deleted minutes after sitting unflagged.
///
/// Returns (title, rows, summary_lines). The panel is ALWAYS present, even when the list
/// is empty — an absent panel reads as "did not check", while an empty panel with an
/// explicit summary reads as "checked and clear".
///
/// The panel's own column widths are below, and they are deliberately not the session
/// table's. The panel prints no CPU or memory figures, so it can give far more of each line
/// to the path — which is the only part of a row a human can act on. Borrowing the table's
/// `workspace_width` cut panel paths to 16 characters on an 88-column terminal while 46
/// columns sat unused.
fn at_risk_panel_content(snapshot: &Snapshot, width: u16) -> (String, Vec<String>, Vec<String>) {
    let path_column_width = panel_path_width(width);

    // Partition workspaces by state
    let mut dirty_stranded: Vec<&WorkspaceReport> = Vec::new();
    let mut unknown_at_risk: Vec<&WorkspaceReport> = Vec::new();
    let mut dirty_driven: Vec<&WorkspaceReport> = Vec::new();
    let mut clean_count = 0;
    let mut not_version_controlled_count = 0;
    let mut path_gone_count = 0;

    for workspace in &snapshot.workspaces {
        match &workspace.state {
            WorkspaceState::DirtyStranded => dirty_stranded.push(workspace),
            WorkspaceState::Unknown(Unreadable::QueryFailed(_))
            | WorkspaceState::Unknown(Unreadable::TimedOut) => unknown_at_risk.push(workspace),
            WorkspaceState::DirtyDriven => dirty_driven.push(workspace),
            WorkspaceState::Clean => clean_count += 1,
            WorkspaceState::Unknown(Unreadable::NotVersionControlled) => {
                not_version_controlled_count += 1
            }
            WorkspaceState::Unknown(Unreadable::PathGone) => path_gone_count += 1,
        }
    }

    // Sort stranded by uncommitted_entries descending (largest pile first)
    dirty_stranded.sort_by(|a, b| {
        b.uncommitted_entries
            .unwrap_or(0)
            .cmp(&a.uncommitted_entries.unwrap_or(0))
    });

    // Count at-risk
    let at_risk_count = dirty_stranded.len() + unknown_at_risk.len();
    let total_workspaces = snapshot.workspaces.len();

    let title = format!(
        " at risk — {} of {} workspaces ",
        at_risk_count, total_workspaces
    );

    // Build rows: stranded first, then unknown at-risk, then driven
    let mut rows = Vec::new();

    for workspace in dirty_stranded.iter().chain(unknown_at_risk.iter()) {
        rows.push(workspace_row(workspace, path_column_width));
    }

    for workspace in &dirty_driven {
        rows.push(workspace_row(workspace, path_column_width));
    }

    // Build summary
    let mut summary = Vec::new();

    // When nothing is at risk, say so explicitly
    if at_risk_count == 0 {
        if total_workspaces == 0 {
            // NOT reassurance. Nothing was checked — and on the machine behind
            // `docs/observability-mechanics.md` §4.6 there are 70 workspaces to check, so an
            // empty candidate set means discovery failed rather than that the machine is
            // clean. Ticket #7 requires an empty panel to read as "checked and clear"; this
            // case is the opposite of that and has to read the opposite way.
            summary.push(
                "No workspaces were located, so NOTHING WAS CHECKED — this is not a clear \
                 result."
                    .to_string(),
            );
        } else if !dirty_driven.is_empty() {
            summary.push(
                "No workspaces at risk — all dirty workspaces have active sessions.".to_string(),
            );
        } else {
            summary.push("No workspaces at risk — all checked workspaces are clean.".to_string());
        }
    }

    // Account for what was not listed
    let mut accounted = Vec::new();
    if clean_count > 0 {
        accounted.push(format!("{} clean", clean_count));
    }
    if !dirty_driven.is_empty() && at_risk_count > 0 {
        accounted.push(format!("{} dirty-driven", dirty_driven.len()));
    }
    if not_version_controlled_count > 0 {
        accounted.push(format!(
            "{} not version-controlled",
            not_version_controlled_count
        ));
    }
    if path_gone_count > 0 {
        accounted.push(format!("{} path gone", path_gone_count));
    }

    if !accounted.is_empty() {
        summary.push(format!("Also found: {}.", accounted.join(", ")));
    }

    // Unlocated namespaces
    let mut no_longer_exists_count = 0;
    let mut ambiguous_count = 0;
    let mut search_exhausted_count = 0;
    let mut misfiled_count = 0;

    for (_, resolution) in &snapshot.unlocated {
        match resolution {
            NamespaceResolution::NoLongerExists => no_longer_exists_count += 1,
            NamespaceResolution::Ambiguous(_) => ambiguous_count += 1,
            NamespaceResolution::SearchExhausted => search_exhausted_count += 1,
            // A resolved namespace has a path, so it belongs in `workspaces`, not here.
            // Counted rather than ignored: silently swallowing it would shrink the total the
            // panel claims to have checked, and a wrong denominator is how "0 at risk" stops
            // meaning anything.
            NamespaceResolution::Resolved(_) => misfiled_count += 1,
        }
    }

    let mut unlocated_parts = Vec::new();
    if no_longer_exists_count > 0 {
        unlocated_parts.push(format!("{} no longer exist", no_longer_exists_count));
    }
    if ambiguous_count > 0 {
        unlocated_parts.push(format!("{} ambiguous", ambiguous_count));
    }
    if search_exhausted_count > 0 {
        unlocated_parts.push(format!("{} search incomplete", search_exhausted_count));
    }

    if !unlocated_parts.is_empty() {
        summary.push(format!(
            "Recorded namespaces not located: {}.",
            unlocated_parts.join(", ")
        ));
    }
    if misfiled_count > 0 {
        summary.push(format!(
            "BUG: {misfiled_count} namespace(s) reported as unlocated already have a path."
        ));
    }

    // Partial coverage warning
    if !snapshot.sweep_complete {
        summary.push(
            "WARNING: Directory sweep incomplete — this list is partial, not exhaustive."
                .to_string(),
        );
    }

    (title, rows, summary)
}

const PANEL_STATE_WIDTH: u16 = 14; // "DIRTY-STRANDED", the longest state
const PANEL_COUNT_WIDTH: u16 = 15; // a count, or the reason printed in its place
const PANEL_KIND_WIDTH: u16 = 8; // "worktree" / "primary"

/// How much room the panel's path column gets at a given total width.
///
/// Derived rather than declared, and used both to shorten the path and to lay the row out,
/// so the two cannot disagree and cut the path a second time without marking it.
fn panel_path_width(total: u16) -> u16 {
    let prefix = PANEL_STATE_WIDTH + PANEL_COUNT_WIDTH + PANEL_KIND_WIDTH + 3;
    let borders = 2;
    total.saturating_sub(prefix + borders).max(WORKSPACE_MIN)
}

/// Format one workspace as a row for the at-risk panel.
///
/// Columns: state, count (or reason for Unknown), worktree/primary marker, path (shortened).
fn workspace_row(workspace: &WorkspaceReport, path_column_width: u16) -> String {
    let state = workspace.state.to_string();

    // For Unknown, show the reason in place of the count. Never "0", which would read as clean.
    let count_or_reason = match &workspace.state {
        WorkspaceState::Unknown(reason) => reason.to_string(),
        _ => workspace
            .uncommitted_entries
            .map(|n| n.to_string())
            .unwrap_or_else(|| "—".to_string()),
    };

    let worktree_marker = if workspace.linked_worktree {
        "worktree"
    } else {
        "primary"
    };

    let path = shorten_from_the_left(&workspace.path, path_column_width);

    format!(
        "{state:<state_width$} {count_or_reason:<count_width$} {worktree_marker:<kind_width$} {path}",
        state_width = PANEL_STATE_WIDTH as usize,
        count_width = PANEL_COUNT_WIDTH as usize,
        kind_width = PANEL_KIND_WIDTH as usize,
    )
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

    // Build the at-risk panel content
    let (panel_title, panel_rows, panel_summary) = at_risk_panel_content(snapshot, area.width);
    let panel_height = 3 + panel_rows.len() as u16 + panel_summary.len() as u16;

    let caveats = footer_lines(snapshot, area.width);
    let [table_area, panel_area, caveat_area] = Layout::vertical([
        Constraint::Min(3),
        Constraint::Length(panel_height),
        Constraint::Length(caveats.len() as u16),
    ])
    .areas(area);

    // Render session table
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

    // Render at-risk panel
    let mut panel_content = panel_rows;
    if !panel_summary.is_empty() {
        if !panel_content.is_empty() {
            panel_content.push(String::new()); // Blank line before summary
        }
        panel_content.extend(panel_summary);
    }

    let panel = Paragraph::new(panel_content.join("\n"))
        .block(Block::default().borders(Borders::ALL).title(panel_title));

    frame.render_widget(panel, panel_area);

    // Render footer caveats
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
    let figures = match (&session.resources, &session.last_reading) {
        (Ok(resources), _) => figures_of(resources),
        // Nothing was read now, but something was read before the process went. Show that,
        // marked: the total is the point of having remembered it, and almost all of an
        // agent's cost is a figure only its own process could ever have reported.
        (Err(_), Some(reading)) => remembered_figures_of(&reading.resources),
        // Nothing was read and nothing is remembered. Every figure carries the same reason
        // rather than the row being dropped or shown as idle — the session existed either way.
        (Err(reason), None) => [
            reason.to_string(),
            reason.to_string(),
            reason.to_string(),
            reason.to_string(),
            reason.to_string(),
        ],
    };

    // The workspace path when it is known, and otherwise the reason — carrying the recorded
    // namespace alongside it whenever there is one.
    //
    // The namespace matters because for a transcript-derived session it is the only thing
    // that identifies which workspace the row is about. A bare `gone` names nothing, and a
    // row that names nothing cannot be acted on. The program still must not claim to know
    // the path, so the reason comes first and the namespace after it.
    let workspace = match &session.workspace {
        Ok(workspace) => shorten_from_the_left(&workspace.path, workspace_width),
        Err(unknown) => match &session.identity {
            Identity::Transcript { recorded_as } => {
                shorten_from_the_left(&format!("{unknown}: {recorded_as}"), workspace_width)
            }
            Identity::Process { .. } => unknown.to_string(),
        },
    };

    let pid_cell = match &session.identity {
        Identity::Process { pid } => pid.to_string(),
        Identity::Transcript { .. } => "gone".to_string(),
    };

    let mut cells = vec![
        pid_cell,
        // Shortened here rather than left to `ratatui`, which would cut it without saying so.
        // A CLI id comes from user configuration since #12, so its length is not something
        // this code gets to assume.
        shorten_with_a_mark(&session.cli, CLI_WIDTH),
        state_cell(&session.liveness),
    ];
    cells.extend(figures);
    cells.push(workspace);
    Row::new(cells)
}

/// Keep the start of a name, marking that the end was dropped.
///
/// The opposite direction to [`shorten_from_the_left`], and for the opposite reason: a
/// workspace's identity is in its tail, while a CLI's is in its head — `codex` and
/// `codex-experimental` differ at the end, and nothing shares a prefix by accident the way
/// every path under one home directory does.
///
/// Marked, always. `cursor-agent` cut to `cursor` is not a shorter version of this CLI's name;
/// it is a plausible name for a different one, and the reader has no way to tell.
fn shorten_with_a_mark(text: &str, width: u16) -> String {
    let width = width as usize;
    let characters: Vec<char> = text.chars().collect();
    if characters.len() <= width || width == 0 {
        return text.to_string();
    }
    let kept: String = characters[..width - 1].iter().collect();
    format!("{kept}…")
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

/// The same figures, marked as remembered rather than current.
fn remembered_figures_of(resources: &Resources) -> [String; 5] {
    [
        mark(resources.own_cpu.as_ref().map(format_cpu)),
        mark(resources.children_cpu.as_ref().map(format_cpu)),
        mark(resources.current_memory.as_ref().map(format_bytes)),
        mark(resources.peak_memory.as_ref().map(format_bytes)),
        mark(resources.bytes_written.as_ref().map(format_bytes)),
    ]
}

/// A figure, or the reason it is missing. Never a zero standing in for either.
fn show<E: std::fmt::Display>(figure: Result<String, E>) -> String {
    match figure {
        Ok(text) => text,
        Err(reason) => reason.to_string(),
    }
}

/// A remembered figure, marked, or the reason it is missing — unmarked.
///
/// Only a value gets the marker. A reason the reading could not supply in the first place —
/// `ps-blind`, say — did not become stale by being remembered: it was that reason when it was
/// read and it is that reason still, and marking it would imply a number had gone off.
fn mark<E: std::fmt::Display>(figure: Result<String, E>) -> String {
    match figure {
        Ok(text) => format!("{text}*"),
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
