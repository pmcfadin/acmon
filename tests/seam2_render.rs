//! Seam 2 — rendering a snapshot, with no real terminal involved.

use std::time::Duration;

use acmon::render::{render_to_lines, required_height};
use acmon::world::{ResourceSource, Resources, ResourcesUnavailable, Unmeasured};
use acmon::{Session, Snapshot};

/// A width that fits the whole table. The narrow case is its own test.
const WIDE: u16 = 100;

/// The ledger of session 69046, measured in `docs/observability-mechanics.md` §2.6.
fn measured_ledger() -> Resources {
    Resources {
        source: ResourceSource::Rusage,
        own_cpu: Ok(Duration::from_secs(1_669)),
        children_cpu: Ok(Duration::from_secs(32_317)),
        current_memory: Ok(482_000_000),
        peak_memory: Ok(622_000_000),
        bytes_written: Ok(166_000_000),
    }
}

fn snapshot_of(pids: &[i32]) -> Snapshot {
    Snapshot {
        sessions: pids
            .iter()
            .map(|&pid| Session {
                pid,
                cli: "claude".to_string(),
                resources: Ok(measured_ledger()),
            })
            .collect(),
    }
}

fn snapshot_with(reading: Result<Resources, ResourcesUnavailable>) -> Snapshot {
    Snapshot {
        sessions: vec![Session {
            pid: 264,
            cli: "claude".to_string(),
            resources: reading,
        }],
    }
}

fn rendered(snapshot: &Snapshot, width: u16) -> String {
    render_to_lines(snapshot, width, required_height(snapshot, width)).join("\n")
}

#[test]
fn renders_one_row_per_session() {
    let snapshot = snapshot_of(&[264, 2880, 5333]);

    let text = rendered(&snapshot, WIDE);

    for pid in [264, 2880, 5333] {
        assert!(
            text.contains(&pid.to_string()),
            "pid {pid} should appear as a row; got:\n{text}"
        );
    }
    assert!(
        text.contains("claude"),
        "each row should name the CLI; got:\n{text}"
    );
}

#[test]
fn states_the_session_count_so_zero_is_explicit() {
    // "No sessions" must read as a measured result, not as a blank screen that might
    // equally mean the tool is broken.
    let text = rendered(&snapshot_of(&[]), WIDE);

    assert!(
        text.contains('0'),
        "an empty snapshot must still say how many sessions were found; got:\n{text}"
    );
}

#[test]
fn a_row_separates_the_sessions_own_cpu_from_its_childrens() {
    // The central finding of this project: session 69046 spent 1,669 s in the agent
    // process and 32,317 s in the processes it launched. A monitor showing one number
    // reports about five per cent of the truth, so both must be present and distinct.
    // Expected renderings worked out by hand: 1,669 s = 27 m 49 s; 32,317 s = 8 h 58 m.
    let text = rendered(&snapshot_of(&[69046]), WIDE);

    assert!(
        text.contains("27m49s"),
        "the session's own CPU should read as 27m49s; got:\n{text}"
    );
    assert!(
        text.contains("8h58m"),
        "its children's CPU should read as 8h58m; got:\n{text}"
    );
}

#[test]
fn a_row_shows_memory_now_memory_at_peak_and_bytes_written() {
    let text = rendered(&snapshot_of(&[69046]), WIDE);

    for expected in ["482 MB", "622 MB", "166 MB"] {
        assert!(
            text.contains(expected),
            "expected {expected} in the row; got:\n{text}"
        );
    }
}

#[test]
fn states_that_child_totals_are_floors_because_detached_work_escapes() {
    // Verified in the mechanics document §2.4: a double-forked child's CPU never
    // reaches its parent's ledger. Every child total printed here is therefore a lower
    // bound, and output that does not say so invites being read as complete.
    let text = rendered(&snapshot_of(&[69046]), WIDE);

    assert!(
        text.contains("floor"),
        "output must state that child totals are floors; got:\n{text}"
    );
    assert!(
        text.contains("detached") || text.contains("orphan"),
        "output must say what is excluded, not merely that something is; got:\n{text}"
    );
}

#[test]
fn the_floor_caveat_survives_a_width_too_narrow_to_hold_it_on_one_line() {
    // The table fits from 73 columns; the caveat is 76 characters. Between those the
    // caveat has to wrap. Losing its tail to a clipped line would leave the numbers
    // looking complete, which is the failure the caveat exists to prevent.
    let text = rendered(&snapshot_of(&[69046]), 74);

    assert!(
        text.contains("floors,"),
        "the caveat's first line must survive; got:\n{text}"
    );
    assert!(
        text.contains("totals."),
        "and so must its last words; got:\n{text}"
    );
    assert!(
        text.contains('┘'),
        "the table must still be closed off, not pushed off the bottom; got:\n{text}"
    );
}

#[test]
fn a_figure_the_coarser_reader_cannot_see_shows_its_reason_not_a_zero() {
    // `ps` reports own CPU and resident size only. The figures it cannot see must say
    // so on the row: a zero here would report a session with busy children as idle.
    let text = rendered(
        &snapshot_with(Ok(Resources {
            source: ResourceSource::Ps,
            own_cpu: Ok(Duration::from_secs(1_669)),
            children_cpu: Err(Unmeasured::NotReportedBy(ResourceSource::Ps)),
            current_memory: Ok(482_000_000),
            peak_memory: Err(Unmeasured::NotReportedBy(ResourceSource::Ps)),
            bytes_written: Err(Unmeasured::NotReportedBy(ResourceSource::Ps)),
        })),
        WIDE,
    );

    assert!(
        text.contains("ps-blind"),
        "an unseeable figure must name its reason; got:\n{text}"
    );
    // "0.0s" is spelled out because it is what `format_cpu` produces for a zero
    // duration, and it does not contain the substring "0s".
    for zero in ["0.0s", "0m00s", "0 B", "0 MB", "0 kB"] {
        assert!(
            !text.contains(zero),
            "no absent figure may render as {zero}; got:\n{text}"
        );
    }
}

#[test]
fn a_session_whose_ledger_could_not_be_read_at_all_is_still_a_row() {
    // Detection already proved the session exists. Dropping the row would report a
    // running session as absent, which is the same failure as reporting zero.
    let text = rendered(
        &snapshot_with(Err(ResourcesUnavailable::ProcessExited)),
        WIDE,
    );

    assert!(
        text.contains("264"),
        "the session must still be listed; got:\n{text}"
    );
    assert!(
        text.contains("exited"),
        "and must carry the reason its figures are missing; got:\n{text}"
    );
}

#[test]
fn a_terminal_too_narrow_for_the_numbers_says_so_instead_of_truncating() {
    // The table truncates silently, and a truncated 32,317 s is a plausible wrong
    // number rather than an obvious error. Refuse instead.
    let snapshot = snapshot_of(&[69046]);
    let narrow = 40;

    let text = rendered(&snapshot, narrow);

    assert!(
        text.contains("40"),
        "the message should state the width available; got:\n{text}"
    );
    assert!(
        !text.contains("claude"),
        "no partial table may be drawn when it cannot be drawn correctly; got:\n{text}"
    );
}
