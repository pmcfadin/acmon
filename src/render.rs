//! Turning a snapshot into terminal output.
//!
//! The drawing code is shared between production and tests. Both go through
//! [`draw_screen`]: the full-screen display draws it into a real `crossterm` terminal on the
//! alternate screen, and `agtop --once` and every test draw it into an in-memory buffer via
//! [`screen_to_lines`]. One drawing pass, so a rendering that has never been looked at by a
//! human is still the rendering that was asserted on.
//!
//! What is decided elsewhere: everything about *what* to draw, in [`crate::display`] — which
//! includes the order rows go in, because that is a statement about cost rather than about
//! typography. What is decided here is how it is said, and how much of it fits: [`fit`] settles
//! what a terminal too short for the whole screen gives up, and the drawing obeys it rather than
//! deciding again.

use std::time::Duration;

use ratatui::backend::TestBackend;
use ratatui::layout::{Constraint, Layout};
use ratatui::widgets::{Block, Borders, Paragraph, Row, Table};
use ratatui::{Frame, Terminal};

use crate::collect::{Identity, LivenessUnknown, Persistence, Session, Snapshot, WorkspaceReport};
use crate::display::{Meters, Screen};
use crate::notify::Rebuilt;
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

/// What the other marker on a state means.
///
/// `?` says a verdict was reached by inference. This says something worse: no verdict was
/// reached at all, and not for a reason that will clear on its own. Only WAITING is ever
/// announced (see [`notify`](crate::notify)) and reaching it needs a silence measurement, so a
/// session whose CLI has no transcript store is monitored and never alerts. Unsaid, that is
/// something a reader could only ever infer from an alert that did not arrive — which is how
/// a fifth CLI comes to be watched by a tool that will never speak about it.
const LIMIT_MARKER_CAVEAT: &str =
    "A state marked ! could not be determined at all, and will not be on any later run \
     either — such a session is monitored but is never announced.";

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
    let session_table_height = snapshot.sessions.len() as u16 + TABLE_CHROME;

    // At-risk panel: always present. Top border + title row + content rows + summary lines + bottom border.
    // The title row is a header inside the bordered block.
    let panel_height = panel_height(snapshot, width);

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

    lines.extend(order_lines(snapshot, width));
    lines.extend(unknown_state_lines(snapshot, width));
    lines.extend(memory_lines(snapshot, width));
    lines
}

/// What has to be said about the order the rows are in.
///
/// Only when a session has no child-CPU figure at all, because only then is the order not
/// self-evident from the column it is drawn by: those rows sit above every measured one, and a
/// reader seeing a reason where the largest total should be would otherwise reasonably conclude
/// the table is ordered by something else.
///
/// The position is the whole point (NF10). An absent cost sorted low would rank a session as
/// cheap on a figure nobody has — and the cheap end of the table is what a screen too short
/// drops.
fn order_lines(snapshot: &Snapshot, width: u16) -> Vec<String> {
    let unmeasured = crate::display::sessions_without_a_cost(&snapshot.sessions);
    if unmeasured == 0 {
        return Vec::new();
    }
    wrap_words(
        &format!(
            "Rows are ordered by child CPU, descending. {unmeasured} session(s) have no \
             child-CPU figure at all, so they are listed FIRST rather than last: an absent cost \
             is not a small one, and the bottom of this table is what a terminal too short drops."
        ),
        width,
    )
}

/// What has to be said about a state that could not be determined, one line per row.
///
/// UNKNOWN with nothing beside it is the one verdict a reader can neither act on nor account
/// for: a transcript store that broke and a store that was never there for this CLI look
/// identical in the state column, and they need opposite responses — investigate the first,
/// live with the second. So each undetermined state names its own reason here, and a reason
/// that is a structural limit rather than a fault is marked as one.
fn unknown_state_lines(snapshot: &Snapshot, width: u16) -> Vec<String> {
    let undetermined: Vec<(&Session, LivenessUnknown)> = snapshot
        .sessions
        .iter()
        .filter_map(|session| session.liveness_unknown().map(|why| (session, why)))
        .collect();
    if undetermined.is_empty() {
        return Vec::new();
    }

    let mut lines = Vec::new();
    // Explained only where the marker actually appears, for the same reason the inference
    // caveat is: a note printed on every run becomes furniture a reader stops seeing.
    if undetermined.iter().any(|(_, why)| why.is_structural()) {
        lines.extend(wrap_words(LIMIT_MARKER_CAVEAT, width));
    }
    for (session, why) in &undetermined {
        let marker = if why.is_structural() { "! " } else { "" };
        lines.extend(wrap_words(
            &format!("  {marker}{}: {why}.", identify(session)),
            width,
        ));
    }
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

    match &remembered.persisted {
        Persistence::Stored => {}
        Persistence::Failed(why) => lines.extend(wrap_words(
            &format!("WARNING: {why} — the next run will start with no history."),
            width,
        )),
        // Not a warning. A run that was never going to write did not fail to write, and
        // dressing it as a failure would train a reader to ignore the line that means one.
        Persistence::NotAttempted { because } => lines.extend(wrap_words(
            &format!("Nothing was written: {because}."),
            width,
        )),
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
    let notified = &remembered.notified;

    // Why this run announced conditions that have not changed.
    //
    // An absent record is an answer — a first run, or a state directory someone deleted on purpose.
    // A record that is there and could not be used is a fault. The two have the same consequence
    // and must not read alike, because only one of them is worth going and looking at the file for.
    match &notified.rebuilt {
        // Unconditional, like an unusable notification config and for the same reason: a monitor
        // that has lost the ability to dedupe at all has lost it on a quiet machine too, and the
        // run where that first matters is the run that will not say so.
        Some(
            rebuilt @ (Rebuilt::Unreadable(_)
            | Rebuilt::Unparsable(_)
            | Rebuilt::UnknownVersion { .. }),
        ) => {
            lines.extend(wrap_words(
                &format!(
                    "WARNING: {rebuilt} — so NOTHING was deduped this run: no condition was \
                     suppressed as already announced."
                ),
                width,
            ));
        }
        // Said only when the missing record actually cost a re-announcement, measured by what
        // reached a channel rather than by what was notable: alerts that never arrived are a
        // different and louder problem, reported below. A line on every quiet first run would
        // train a reader to skip the line, and this line matters exactly once.
        Some(rebuilt @ Rebuilt::NothingRecorded) => {
            let re_announced = notified.record.sessions.len() + notified.record.workspaces.len();
            if re_announced > 0 {
                lines.extend(wrap_words(
                    &format!(
                        "{rebuilt} — so {re_announced} condition(s) still true were announced \
                         again rather than deduped. Nothing changed on the machine to cause that."
                    ),
                    width,
                ));
            }
        }
        None => {}
    }

    // A dedupe record that could not be stored.
    //
    // Reported whenever there was anything to lose. The failure is in the safe direction — an
    // unrecorded alert is announced again rather than dropped — but a monitor that re-announces
    // the same stranding every run with nothing to explain why is how a reader learns that its
    // alerts mean nothing.
    if let Persistence::Failed(why) = &notified.persisted {
        if health.notable > 0 || !notified.record.is_empty() {
            lines.extend(wrap_words(
                &format!(
                    "WARNING: {why} — the next run will announce every condition still true, \
                     including the ones announced this run."
                ),
                width,
            ));
        }
    }

    // A run that never notifies says so once, and nothing below applies to it. Every warning
    // after this point is about a channel that was asked something, and a reader given
    // "no channels configured" by a display would go and configure one that was already there.
    if let Some(because) = health.read_only {
        lines.extend(wrap_words(
            &format!(
                "Nothing was announced: {because}. {} condition(s) on this screen would be \
                 announced by a running monitor.",
                health.notable
            ),
            width,
        ));
        return lines;
    }

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

    // Alerts that were never offered to a channel at all.
    //
    // Reported separately from the failures above, and never merely counted: an alerting step
    // that ran out of its budget with six strandings left to announce, and said nothing about
    // them, is a silent cap in the one path where silence is read as "nothing is wrong". Both
    // warnings can appear in the same run — a channel that answered badly for the alerts it
    // was given and a run that did not reach the rest are two different facts.
    if health.has_unattempted() {
        let mut parts = Vec::new();
        if health.local_not_attempted > 0 {
            parts.push(format!("local: {}", health.local_not_attempted));
        }
        if health.remote_not_attempted > 0 {
            parts.push(format!("remote: {}", health.remote_not_attempted));
        }
        let why = health
            .not_attempted_reason
            .as_deref()
            .unwrap_or("and no reason was reported for it, which is itself a fault in this tool");
        lines.extend(wrap_words(
            &format!(
                "WARNING: {} alert(s) were NOT SENT at all ({}) — {}. They are not recorded as \
                 announced, and will be re-announced on the next run.",
                health.not_attempted(),
                parts.join(", "),
                why
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

/// Draw a snapshot into a frame, assuming it has [`required_height`] rows to draw into.
///
/// Every session it holds, so the caller has to have asked how tall that is. What a terminal
/// shorter than that gives up is [`draw_screen`]'s, via [`fit`] — the display never reaches this
/// entry point, and a second place deciding what fits is exactly what this ticket removed.
pub fn draw(frame: &mut Frame, snapshot: &Snapshot) {
    draw_in(frame, snapshot, frame.area());
}

/// Draw a snapshot into part of a frame, with every session in it.
///
/// Separated from [`draw`] so the whole screen — meters, notices, then this — is one drawing
/// pass rather than two that could disagree about how much room they have.
pub fn draw_in(frame: &mut Frame, snapshot: &Snapshot, area: ratatui::layout::Rect) {
    draw_body(
        frame,
        snapshot,
        &Fit::everything(snapshot.sessions.len()),
        area,
    );
}

/// Draw the figures: the session table cut to what [`fit`] allowed, then the panel, then the
/// caveats.
fn draw_body(frame: &mut Frame, snapshot: &Snapshot, fit: &Fit, area: ratatui::layout::Rect) {
    if area.width < minimum_width() {
        let message = wrap_words(&too_narrow_message(area.width), area.width).join("\n");
        frame.render_widget(Paragraph::new(message), area);
        return;
    }

    // Build the at-risk panel content
    let (panel_title, panel_rows, panel_summary) = at_risk_panel_content(snapshot, area.width);
    let panel_height = PANEL_CHROME + panel_rows.len() as u16 + panel_summary.len() as u16;

    let caveats = footer_lines(snapshot, area.width);
    // Every region is exactly the height its content needs, and the slack sits at the bottom.
    // A region that absorbed the slack instead would grow or shrink with the terminal, and what
    // fits would then be decided here as well as in `fit`.
    let table_height = if fit.table {
        TABLE_CHROME + fit.shown as u16
    } else {
        0
    };
    let [table_area, panel_area, caveat_area, _slack] = Layout::vertical([
        Constraint::Length(table_height),
        Constraint::Length(panel_height),
        Constraint::Length(caveats.len() as u16),
        Constraint::Min(0),
    ])
    .areas(area);

    // Render session table, costliest session first (F55).
    if fit.table {
        let title = if fit.hidden > 0 {
            format!(
                " acmon — {} agent session(s), most child CPU first — {} shown ",
                snapshot.sessions.len(),
                fit.shown
            )
        } else {
            format!(
                " acmon — {} agent session(s), most child CPU first ",
                snapshot.sessions.len()
            )
        };
        let workspace_width = workspace_width(area.width);
        let rows: Vec<Row> = crate::display::in_cost_order(&snapshot.sessions)
            .into_iter()
            .take(fit.shown)
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
    }

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

/// A state, with a marker when the verdict behind it was inferred rather than observed, or
/// when there is no verdict to be had at all.
///
/// The marker is the whole reason the state column is eight wide. Without it, a WAITING
/// reached by guessing at a human's intent would be indistinguishable from an ACTIVE
/// established by a transcript that changed a second ago — and an UNKNOWN that no later run
/// can resolve would be indistinguishable from one that the next run may well answer.
///
/// At most one marker is ever appended, which is what keeps the cell inside its eight
/// columns. The two cannot co-occur: an inferred verdict is always WAITING, and a structural
/// limit is always UNKNOWN.
fn state_cell(session: &Session) -> String {
    let state = match session.liveness.state {
        crate::liveness::State::Active => "ACTIVE",
        crate::liveness::State::Waiting => "WAITING",
        crate::liveness::State::Stalled => "STALLED",
        crate::liveness::State::Unknown => "UNKNOWN",
    };
    if session.liveness.method.is_inferred() {
        format!("{state}?")
    } else if session
        .liveness_unknown()
        .is_some_and(|why| why.is_structural())
    {
        format!("{state}!")
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
        state_cell(session),
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

// --- The whole screen: what it cost, what is wrong with it, and then the figures ---

/// How much room a gauge's label gets. The width of the longest one, so the bars line up.
const GAUGE_LABEL_WIDTH: usize = 19; // "collection overhead"
/// How many cells a gauge's bar is drawn in.
const GAUGE_BAR_WIDTH: usize = 10;
/// The narrowest a gauge's figure may be printed in. A longer figure widens its own cell rather
/// than being cut: no width may cost a digit.
const GAUGE_FIGURE_WIDTH: usize = 8;
/// A filled cell of a bar.
const GAUGE_FILL: char = '|';
/// What a bar is drawn with when there is no figure at all.
///
/// Not spaces. An empty bar is what a duty cycle of zero looks like — a monitor that is running
/// and idle — and that is the one thing an absent figure never means.
const GAUGE_ABSENT_FILL: char = '?';
/// The last cell of a bar whose figure is past the end of its scale.
///
/// A bar silently pegged at full would report a collection that took two and a half times the
/// refresh interval as one that exactly filled it.
const GAUGE_PAST_SCALE: char = '>';
/// What is printed where a gauge's figure would be when there is none.
const GAUGE_ABSENT_FIGURE: &str = "absent";
/// The gap between two gauges in the row.
const GAUGE_SPACING: &str = "  ";

/// What every bar in the row means, said once.
///
/// A gauge is a picture of a ratio, and a picture of a ratio whose denominator is unstated is
/// not a measurement. Said on every screen rather than only when something is odd, because
/// unlike a warning this is how to read the row at all.
fn gauge_legend() -> String {
    format!(
        "Gauges: the overhead bar is a fraction of the {} refresh interval, the duty-cycle bar \
         a fraction of wall time. A bar of {GAUGE_ABSENT_FILL} is no figure at all rather than \
         a zero, and one ending {GAUGE_PAST_SCALE} is past the end of its scale.",
        format_age(crate::display::POLL_INTERVAL),
    )
}

/// One meter in the row: what it is, and either its figure or why there is none.
struct Gauge {
    label: &'static str,
    /// The figure as text and as a fraction of this gauge's scale, or the reason there is no
    /// figure. Never a fraction standing in for an absent one.
    reading: Result<(String, f64), String>,
}

/// The meters as gauges, in the order they are drawn.
///
/// A row rather than a sentence (decision 37): v2's machine-tax attribution — XProtect, Jamf,
/// Gatekeeper, Zscaler — is a set of figures of exactly this shape, and it moves into this row
/// rather than forcing a redesign of the top of the screen.
fn gauges(meters: &Meters) -> Vec<Gauge> {
    vec![
        Gauge {
            label: "collection overhead",
            reading: match &meters.overhead {
                Ok(cost) => Ok((
                    format_cpu(cost),
                    cost.as_secs_f64() / crate::display::POLL_INTERVAL.as_secs_f64(),
                )),
                Err(why) => Err(why.to_string()),
            },
        },
        Gauge {
            label: "amon duty cycle",
            reading: match &meters.duty_cycle {
                Ok(fraction) => Ok((format!("{:.1}%", fraction * 100.0), *fraction)),
                Err(why) => Err(why.to_string()),
            },
        },
    ]
}

/// A bar, at a fraction of its scale.
fn bar_of(fraction: f64) -> String {
    // A fraction that is not a number is not a fraction. Drawn as absent rather than as empty,
    // for the same reason an absent figure is.
    if !fraction.is_finite() {
        return GAUGE_ABSENT_FILL.to_string().repeat(GAUGE_BAR_WIDTH);
    }
    if fraction > 1.0 {
        let mut bar = GAUGE_FILL.to_string().repeat(GAUGE_BAR_WIDTH - 1);
        bar.push(GAUGE_PAST_SCALE);
        return bar;
    }
    let filled = (fraction.max(0.0) * GAUGE_BAR_WIDTH as f64).round() as usize;
    // Anything above zero gets a cell. Rounding a small positive figure down to an empty bar
    // would draw a measurement as the absence of one.
    let filled = if fraction > 0.0 {
        filled.clamp(1, GAUGE_BAR_WIDTH)
    } else {
        filled.min(GAUGE_BAR_WIDTH)
    };
    format!(
        "{}{}",
        GAUGE_FILL.to_string().repeat(filled),
        " ".repeat(GAUGE_BAR_WIDTH - filled)
    )
}

/// One gauge, as it appears in the row.
fn gauge_cell(gauge: &Gauge) -> String {
    let (bar, figure) = match &gauge.reading {
        Ok((figure, fraction)) => (bar_of(*fraction), figure.clone()),
        Err(_) => (
            GAUGE_ABSENT_FILL.to_string().repeat(GAUGE_BAR_WIDTH),
            GAUGE_ABSENT_FIGURE.to_string(),
        ),
    };
    let figure_width = GAUGE_FIGURE_WIDTH.max(figure.chars().count());
    format!(
        "{:<label_width$}[{bar}{figure:>figure_width$}]",
        gauge.label,
        label_width = GAUGE_LABEL_WIDTH,
    )
}

/// Pack cells onto as few lines as the width allows, in order.
fn pack(cells: &[String], width: u16) -> Vec<String> {
    let width = width.max(1) as usize;
    let mut lines: Vec<String> = Vec::new();
    let mut line = String::new();
    for cell in cells {
        let needed = if line.is_empty() {
            cell.chars().count()
        } else {
            line.chars().count() + GAUGE_SPACING.chars().count() + cell.chars().count()
        };
        if needed > width && !line.is_empty() {
            lines.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push_str(GAUGE_SPACING);
        }
        line.push_str(cell);
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}

/// The meter row: what this tool costs, above the figures it produced.
///
/// First-class figures rather than a debug line (F33, G7): a resident process that cannot state
/// its own duty cycle is exactly what this tool would flag on someone else's machine.
///
/// Plain lines, always — the same content full-screen and under `agtop --once` (F34), because
/// the one-shot output is what makes the row assertable without a terminal.
///
/// Three things the row says besides the figures: the instant they were taken as of, so a gauge
/// is not read as live; the scale each bar is drawn against; and, for a figure that is missing,
/// the reason it is missing, in full and on its own line. A reason is a sentence and a cell is
/// eight columns wide, so squeezing one into the other would truncate it — and the truncated
/// reasons here all begin "no monitor".
pub fn meter_row(meters: &Meters, width: u16) -> Vec<String> {
    let gauges = gauges(meters);

    // Stated as the instant it was read at, not as an age. Turning an instant into an age, and
    // an age into a verdict about the monitor, is #30 — and half of that rule here would put
    // two different freshness stories on one screen.
    let mut lines = wrap_words(
        &format!(
            "METERS  as of {}",
            crate::isotime::iso8601_from_unix_seconds(crate::isotime::unix_seconds(
                meters.taken_at
            ))
        ),
        width,
    );

    let cells: Vec<String> = gauges.iter().map(gauge_cell).collect();
    if cells
        .iter()
        .all(|cell| cell.chars().count() <= width as usize)
    {
        lines.extend(pack(&cells, width));
    } else {
        // Narrower than one gauge. The figures are what matter, so they are printed as prose
        // rather than as a bar that would run off the side of the terminal.
        for gauge in &gauges {
            let figure = match &gauge.reading {
                Ok((figure, _)) => figure.clone(),
                Err(_) => GAUGE_ABSENT_FIGURE.to_string(),
            };
            lines.extend(wrap_words(&format!("{}: {figure}", gauge.label), width));
        }
    }

    for gauge in &gauges {
        if let Err(why) = &gauge.reading {
            lines.extend(wrap_words(&format!("  {}: {why}", gauge.label), width));
        }
    }
    lines.extend(wrap_words(&gauge_legend(), width));
    lines
}

/// Everything above the figures: the meter row, then whatever has to be said about them.
fn screen_header(screen: &Screen, width: u16) -> Vec<String> {
    let mut lines = meter_row(&screen.meters, width);
    for notice in &screen.notices {
        lines.extend(wrap_words(notice, width));
    }
    lines
}

/// What is printed in place of the figures when there are none.
fn no_facts_message(reason: &str) -> String {
    format!(
        "NO FIGURES: {reason}. Nothing is drawn in their place — an empty table is \
         indistinguishable from a machine with no agents running on it, and that is the \
         plausible wrong answer this tool exists to remove."
    )
}

/// How tall the whole screen needs to be to hold everything it has to say.
pub fn screen_height(screen: &Screen, width: u16) -> u16 {
    let header = screen_header(screen, width).len() as u16;
    let body = match &screen.facts {
        Ok(snapshot) => required_height(snapshot, width),
        Err(reason) => wrap_words(&no_facts_message(reason), width).len() as u16,
    };
    header + body
}

// --- A terminal too short: what goes, and what says so ---------------------------------------

/// What the height allowed: which session rows are drawn, and what has to be said about the
/// rest.
///
/// Decided in one place, by [`fit`], and obeyed by the drawing below — the same rule that makes
/// the footer caveats safe to draw. A height calculation and a drawing pass that each decided
/// for themselves would clip a line without saying so, and a clipped warning is worse than no
/// warning because the numbers above it then look unqualified.
struct Fit {
    /// Whether the session table is drawn at all.
    table: bool,
    /// How many session rows are drawn, costliest first.
    shown: usize,
    /// How many rows were dropped, cheapest first.
    hidden: usize,
    /// The lines that say so, wrapped. Empty when nothing was dropped.
    notice: Vec<String>,
}

impl Fit {
    /// Every row, and nothing to say.
    fn everything(sessions: usize) -> Fit {
        Fit {
            table: true,
            shown: sessions,
            hidden: 0,
            notice: Vec::new(),
        }
    }
}

/// What the session table costs before a single row is in it: two borders and the column
/// headings.
const TABLE_CHROME: u16 = 3;

/// What the at-risk panel costs besides its rows: two borders and the blank line above its
/// summary.
const PANEL_CHROME: u16 = 3;

fn panel_height(snapshot: &Snapshot, width: u16) -> u16 {
    let (_, rows, summary) = at_risk_panel_content(snapshot, width);
    PANEL_CHROME + rows.len() as u16 + summary.len() as u16
}

/// What has to be said when the terminal is shorter than the screen.
///
/// Never silent, and never at the bottom: the top of the screen is the one place a clip cannot
/// reach, so the statement that something was dropped goes above everything it is about.
fn shortfall_lines(
    hidden: usize,
    table_kept: bool,
    bottom_cut: bool,
    needs: u16,
    has: u16,
    width: u16,
) -> Vec<String> {
    let mut text = String::new();

    // The worst news first, because on a screen this short the end of this very notice is what
    // gets cut. A reader who sees one line of it has to see the line that matters.
    if bottom_cut {
        text.push_str(&format!(
            "THIS TERMINAL IS TOO SHORT AND ITS BOTTOM IS CUT: the whole screen needs {needs} \
             rows and this one has {has}, so the at-risk panel and the warnings under it are \
             below the fold. Lengthen the terminal. "
        ));
    } else {
        if hidden > 0 {
            text.push_str(&format!("+{hidden} sessions not shown. "));
        }
        text.push_str(&format!(
            "THIS TERMINAL IS TOO SHORT: the whole screen needs {needs} rows and this one has \
             {has}. "
        ));
    }

    if table_kept {
        text.push_str(
            "The session rows with the least child CPU were dropped, so the ones on screen are \
             the costliest. ",
        );
    } else if hidden > 0 {
        // Stated again here, because in the cut case above it is the sentence a clipped notice
        // is most likely to lose, and a count nobody read is a silent cut.
        text.push_str(&format!(
            "+{hidden} sessions not shown: not one row fits, so the table is not drawn at all — \
             column headings with nothing under them read as a machine with no agents on it. "
        ));
    } else {
        text.push_str(
            "There are no sessions to show, and the table's own headings do not fit either. ",
        );
    }

    if !bottom_cut {
        text.push_str(
            "Nothing else was dropped: the at-risk panel is whole and every warning under it is \
             still on screen.",
        );
    }
    wrap_words(&text, width)
}

/// Decide what fits, once.
///
/// The session rows are the only elastic part of this screen, and they are dropped from the
/// cheap end (F54). The at-risk panel is not a candidate — it is the highest-stakes thing on
/// screen (F32) — and neither is the footer, which carries every warning about what was not
/// announced and what could not be determined. A screen that quietly dropped one of those is
/// the failure this project exists to remove.
fn fit(screen: &Screen, snapshot: &Snapshot, width: u16, height: u16) -> Fit {
    let total = snapshot.sessions.len();
    let needs = screen_height(screen, width);
    // A terminal too narrow for the numbers gets a refusal instead of a table, and there are no
    // rows in a refusal to drop.
    if height >= needs || width < minimum_width() {
        return Fit::everything(total);
    }

    let fixed = screen_header(screen, width).len() as u16
        + panel_height(snapshot, width)
        + footer_lines(snapshot, width).len() as u16;

    // The most rows that fit, costliest first.
    for shown in (1..=total).rev() {
        let hidden = total - shown;
        let notice = shortfall_lines(hidden, true, false, needs, height, width);
        if fixed + TABLE_CHROME + shown as u16 + notice.len() as u16 <= height {
            return Fit {
                table: true,
                shown,
                hidden,
                notice,
            };
        }
    }

    // Not one row fits. The table goes with them, chrome included.
    let notice = shortfall_lines(total, false, false, needs, height, width);
    let notice = if fixed + notice.len() as u16 > height {
        // Said, not discovered by the reader. The longer wording can only be reached from a
        // screen that was already cut, so stating it cannot make the statement wrong.
        shortfall_lines(total, false, true, needs, height, width)
    } else {
        notice
    };
    Fit {
        table: false,
        shown: 0,
        hidden: total,
        notice,
    }
}

/// Draw the whole screen: what had to be dropped to fit it, the meters, the notices, and then
/// the figures or the reason there are none.
///
/// The one drawing entry point `agtop` uses, full-screen and one-shot alike. Two of them would
/// be two screens that drift apart, and the one-shot output exists precisely so that what the
/// full screen shows can be asserted on without a terminal.
pub fn draw_screen(frame: &mut Frame, screen: &Screen) {
    let area = frame.area();
    let header = screen_header(screen, area.width);
    // A screen with no figures has nothing elastic on it: its whole body is a stated refusal a
    // few lines long, and there is no row in it to drop.
    let fit = match &screen.facts {
        Ok(snapshot) => fit(screen, snapshot, area.width, area.height),
        Err(_) => Fit::everything(0),
    };

    let [notice_area, header_area, body_area] = Layout::vertical([
        Constraint::Length(fit.notice.len() as u16),
        Constraint::Length(header.len() as u16),
        Constraint::Min(0),
    ])
    .areas(area);

    if !fit.notice.is_empty() {
        frame.render_widget(Paragraph::new(fit.notice.join("\n")), notice_area);
    }
    frame.render_widget(Paragraph::new(header.join("\n")), header_area);

    match &screen.facts {
        Ok(snapshot) => draw_body(frame, snapshot, &fit, body_area),
        Err(reason) => frame.render_widget(
            Paragraph::new(wrap_words(&no_facts_message(reason), body_area.width).join("\n")),
            body_area,
        ),
    }
}

/// Render a whole screen into an in-memory buffer and return it as text lines.
///
/// What `agtop --once` prints, and what the tests assert on. The one-shot mode is not a
/// fallback: it is what keeps the renderer testable against a fixed buffer instead of a live
/// terminal, and what keeps the output pipeable.
pub fn screen_to_lines(screen: &Screen, width: u16, height: u16) -> Vec<String> {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("in-memory terminal");
    terminal
        .draw(|frame| draw_screen(frame, screen))
        .expect("drawing into an in-memory buffer cannot fail");
    lines_of(terminal.backend().buffer().clone())
}

/// Render a snapshot into an in-memory buffer and return it as text lines.
///
/// Used by tests, and by anything that wants the output without a terminal.
pub fn render_to_lines(snapshot: &Snapshot, width: u16, height: u16) -> Vec<String> {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("in-memory terminal");
    terminal
        .draw(|frame| draw(frame, snapshot))
        .expect("drawing into an in-memory buffer cannot fail");
    lines_of(terminal.backend().buffer().clone())
}

/// One buffer's worth of cells, as trimmed lines.
fn lines_of(buffer: ratatui::buffer::Buffer) -> Vec<String> {
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
