//! Seam 2 — rendering a snapshot, with no real terminal involved.

use acmon::render::render_to_lines;
use acmon::{Session, Snapshot};

fn snapshot_of(pids: &[i32]) -> Snapshot {
    Snapshot {
        sessions: pids
            .iter()
            .map(|&pid| Session {
                pid,
                cli: "claude".to_string(),
            })
            .collect(),
    }
}

#[test]
fn renders_one_row_per_session() {
    let snapshot = snapshot_of(&[264, 2880, 5333]);

    let lines = render_to_lines(&snapshot, 60, 10);
    let text = lines.join("\n");

    for pid in [264, 2880, 5333] {
        assert!(
            lines.iter().any(|l| l.contains(&pid.to_string())),
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
    let lines = render_to_lines(&snapshot_of(&[]), 60, 10);
    let text = lines.join("\n");

    assert!(
        text.contains('0'),
        "an empty snapshot must still say how many sessions were found; got:\n{text}"
    );
}
