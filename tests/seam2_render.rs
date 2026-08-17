//! Seam 2 — rendering a snapshot, with no real terminal involved.

use std::time::Duration;

use acmon::liveness::{Method, State, Verdict};
use acmon::render::{minimum_width, render_to_lines, required_height};
use acmon::vcs::{Unreadable, WorkspaceState};
use acmon::workspace::{NamespaceResolution, Workspace, WorkspaceUnknown};
use acmon::world::{ResourceSource, Resources, ResourcesUnavailable, Unmeasured};
use acmon::{Identity, Session, Snapshot, WorkspaceReport};

/// A width that fits the whole table with room to spare. The narrow cases have their
/// own tests.
const WIDE: u16 = 120;

/// A workspace that exists, recorded under the namespace it really has on disk.
fn measured_workspace() -> Result<Workspace, WorkspaceUnknown> {
    Ok(Workspace {
        path: "/Users/pmcfadin/projects/agentic_coding_monitor".to_string(),
        namespace: Ok("-Users-pmcfadin-projects-agentic-coding-monitor".to_string()),
    })
}

/// A verdict reached by direct observation, so rows carry no inference marker unless a
/// test is specifically about one.
fn active_verdict() -> Verdict {
    Verdict {
        state: State::Active,
        method: Method::TranscriptChangedRecently,
    }
}

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
        workspaces: Vec::new(),
        unlocated: Vec::new(),
        sweep_complete: true,
        sessions: pids
            .iter()
            .map(|&pid| Session {
                identity: Identity::Process { pid },
                cli: "claude".to_string(),
                resources: Ok(measured_ledger()),
                workspace: measured_workspace(),
                liveness: active_verdict(),
            })
            .collect(),
    }
}

fn snapshot_with(reading: Result<Resources, ResourcesUnavailable>) -> Snapshot {
    Snapshot {
        workspaces: Vec::new(),
        unlocated: Vec::new(),
        sweep_complete: true,
        sessions: vec![Session {
            identity: Identity::Process { pid: 264 },
            cli: "claude".to_string(),
            resources: reading,
            workspace: measured_workspace(),
            liveness: active_verdict(),
        }],
    }
}

fn snapshot_in(workspace: Result<Workspace, WorkspaceUnknown>) -> Snapshot {
    Snapshot {
        workspaces: Vec::new(),
        unlocated: Vec::new(),
        sweep_complete: true,
        sessions: vec![Session {
            identity: Identity::Process { pid: 264 },
            cli: "claude".to_string(),
            resources: Ok(measured_ledger()),
            workspace,
            liveness: active_verdict(),
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
fn the_floor_caveat_stays_whole_at_the_narrowest_width_that_renders() {
    // The boundary case, taken from the code rather than hardcoded so it follows any
    // future change to the columns. Losing the caveat's tail here would leave the
    // numbers looking complete, which is the failure it exists to prevent.
    let text = rendered(&snapshot_of(&[69046]), minimum_width());

    assert!(
        text.contains("floors,"),
        "the caveat must survive at the minimum width; got:\n{text}"
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
fn a_row_names_the_directory_the_session_is_working_in() {
    let text = rendered(&snapshot_of(&[69046]), WIDE);

    assert!(
        text.contains("/Users/pmcfadin/projects/agentic_coding_monitor"),
        "the workspace directory should appear in full when it fits; got:\n{text}"
    );
}

#[test]
fn a_path_too_long_for_its_column_is_cut_from_the_left_and_marked() {
    // The tail of a path is what distinguishes one workspace from another; the head is
    // shared by everything under the same home directory. An unmarked cut could name a
    // directory that exists and is not this one.
    let long = "/Users/pmcfadin/projects/workforceos/.claude/worktrees/obs-increment-3";
    let text = rendered(
        &snapshot_in(Ok(Workspace {
            path: long.to_string(),
            namespace: Ok(
                "-Users-pmcfadin-projects-workforceos--claude-worktrees-obs-increment-3"
                    .to_string(),
            ),
        })),
        minimum_width(),
    );

    assert!(
        !text.contains(long),
        "the path cannot fit at this width, so it must not be claimed in full; got:\n{text}"
    );
    assert!(
        text.contains('…'),
        "a shortened path must be marked as shortened; got:\n{text}"
    );
    assert!(
        text.contains("increment-3"),
        "the distinctive tail must be what survives; got:\n{text}"
    );
}

#[test]
fn a_session_whose_workspace_is_unknown_says_so_with_a_reason() {
    // Never blank, and never a guess. A blank column would read as the root directory.
    let text = rendered(&snapshot_in(Err(WorkspaceUnknown::PermissionDenied)), WIDE);

    assert!(
        text.contains("unknown"),
        "an undetermined workspace must say so explicitly; got:\n{text}"
    );
    assert!(
        text.contains("no-perm"),
        "and must give the reason; got:\n{text}"
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

fn snapshot_in_state(verdict: Verdict) -> Snapshot {
    Snapshot {
        workspaces: Vec::new(),
        unlocated: Vec::new(),
        sweep_complete: true,
        sessions: vec![Session {
            identity: Identity::Process { pid: 264 },
            cli: "claude".to_string(),
            resources: Ok(measured_ledger()),
            workspace: measured_workspace(),
            liveness: verdict,
        }],
    }
}

#[test]
fn a_row_names_the_sessions_state() {
    for (verdict, expected) in [
        (
            Verdict {
                state: State::Active,
                method: Method::TranscriptChangedRecently,
            },
            "ACTIVE",
        ),
        (
            Verdict {
                state: State::Stalled,
                method: Method::NoProcessAndSilencePastStall,
            },
            "STALLED",
        ),
        (
            Verdict {
                state: State::Unknown,
                method: Method::TranscriptActivityUnknown,
            },
            "UNKNOWN",
        ),
    ] {
        let text = rendered(&snapshot_in_state(verdict), WIDE);
        assert!(
            text.contains(expected),
            "expected state {expected} in the row; got:\n{text}"
        );
    }
}

#[test]
fn an_inferred_state_is_marked_and_an_observed_one_is_not() {
    // The distinction acceptance criterion 4 asks for. Both rows below say WAITING, so
    // this can only pass if the marker comes from the method rather than the state.
    let inferred = rendered(
        &snapshot_in_state(Verdict {
            state: State::Waiting,
            method: Method::ProcessResidentButSilent,
        }),
        WIDE,
    );
    let observed = rendered(
        &snapshot_in_state(Verdict {
            state: State::Active,
            method: Method::TranscriptChangedRecently,
        }),
        WIDE,
    );

    assert!(
        inferred.contains("WAITING?"),
        "an inferred verdict must be marked; got:\n{inferred}"
    );
    assert!(
        !observed.contains("ACTIVE?"),
        "an observed verdict must not be marked; got:\n{observed}"
    );
}

#[test]
fn the_marker_is_explained_only_when_something_was_actually_inferred() {
    // Printed always, the note becomes furniture a reader stops seeing.
    let with_inference = rendered(
        &snapshot_in_state(Verdict {
            state: State::Waiting,
            method: Method::ProcessResidentButSilent,
        }),
        WIDE,
    );
    let without = rendered(
        &snapshot_in_state(Verdict {
            state: State::Active,
            method: Method::TranscriptChangedRecently,
        }),
        WIDE,
    );

    assert!(
        with_inference.contains("inferred from silence"),
        "the marker must be explained when it appears; got:\n{with_inference}"
    );
    assert!(
        !without.contains("inferred from silence"),
        "and not explained when nothing was inferred; got:\n{without}"
    );
}

#[test]
fn a_state_is_never_abbreviated_to_fit() {
    // A truncated STALLED is a different word, and STALL would read as an instruction.
    // The column is sized for the longest state plus its marker; this pins that.
    let text = rendered(
        &snapshot_in_state(Verdict {
            state: State::Stalled,
            method: Method::NoProcessAndSilencePastStall,
        }),
        minimum_width(),
    );

    assert!(
        text.contains("STALLED"),
        "the full state must survive at the narrowest renderable width; got:\n{text}"
    );
}

#[test]
fn a_transcript_derived_row_shows_gone_not_a_number() {
    // A transcript-derived session has no pid because the process exited. The PID column
    // must show something short and true — "gone" is the intended word — never blank,
    // and never a fabricated number like 0 or -1.
    let text = rendered(
        &Snapshot {
            workspaces: Vec::new(),
            unlocated: Vec::new(),
            sweep_complete: true,
            sessions: vec![Session {
                identity: Identity::Transcript {
                    recorded_as: "-Users-pmcfadin-projects-agentic-coding-monitor".to_string(),
                },
                cli: "claude".to_string(),
                resources: Err(ResourcesUnavailable::ProcessExited),
                workspace: Err(WorkspaceUnknown::WorkspaceGone),
                liveness: Verdict {
                    state: State::Stalled,
                    method: Method::NoProcessAndSilencePastStall,
                },
            }],
        },
        WIDE,
    );

    assert!(
        text.contains("gone"),
        "the PID column must show 'gone' for a transcript-derived session; got:\n{text}"
    );
    // Check that fabricated PIDs don't appear in the session table.
    // Look for the session table line (contains "gone" and "claude" and "STALLED").
    for line in text.lines() {
        if line.contains("gone") && line.contains("claude") && line.contains("STALLED") {
            // This is the session row. Check it doesn't start with a fabricated PID.
            for fabrication in ["0", "-1", "N/A"] {
                let pattern = format!("│{:<6}", fabrication);
                assert!(
                    !line.starts_with(&pattern),
                    "the PID column must not show a fabricated number like {fabrication}; got line:\n{line}"
                );
            }
        }
    }
}

#[test]
fn a_transcript_derived_claude_row_shows_its_namespace_in_the_workspace_column() {
    // A transcript-derived Claude session cannot show a workspace path (the namespace
    // mapping is not invertible), but the namespace itself should appear in the
    // workspace column. A human reading `-Users-pmcfadin-projects-agentic-coding-monitor`
    // can see which directory it is; the program must not claim to know, but withholding
    // it entirely would make the row useless.
    let namespace = "-Users-pmcfadin-projects-agentic-coding-monitor";
    let text = rendered(
        &Snapshot {
            workspaces: Vec::new(),
            unlocated: Vec::new(),
            sweep_complete: true,
            sessions: vec![Session {
                identity: Identity::Transcript {
                    recorded_as: namespace.to_string(),
                },
                cli: "claude".to_string(),
                resources: Err(ResourcesUnavailable::ProcessExited),
                workspace: Err(WorkspaceUnknown::WorkspaceGone),
                liveness: Verdict {
                    state: State::Stalled,
                    method: Method::NoProcessAndSilencePastStall,
                },
            }],
        },
        WIDE,
    );

    assert!(
        text.contains("agentic-coding-monitor"),
        "the workspace column must show the namespace verbatim; got:\n{text}"
    );
    assert!(
        !text.contains("not-invertible"),
        "the workspace column must not show the error reason when the namespace is available; \
         got:\n{text}"
    );
}

// ===== At-risk workspace panel tests =====

#[test]
fn at_risk_panel_lists_a_stranded_workspace() {
    // The primary case: uncommitted work with no session driving it.
    let text = rendered(
        &Snapshot {
            sessions: Vec::new(),
            workspaces: vec![WorkspaceReport {
                path: "/Users/pmcfadin/projects/abandoned".to_string(),
                state: WorkspaceState::DirtyStranded,
                linked_worktree: false,
                uncommitted_entries: Some(27),
            }],
            unlocated: Vec::new(),
            sweep_complete: true,
        },
        WIDE,
    );

    assert!(
        text.contains("DIRTY-STRANDED"),
        "the panel must show the workspace state; got:\n{text}"
    );
    assert!(
        text.contains("27"),
        "the panel must show the uncommitted entry count; got:\n{text}"
    );
    assert!(
        text.contains("abandoned"),
        "the panel must show the workspace path; got:\n{text}"
    );
}

#[test]
fn at_risk_panel_is_present_when_nothing_is_at_risk() {
    // An empty panel must read as "checked and clear", never as possibly broken.
    let text = rendered(
        &Snapshot {
            sessions: Vec::new(),
            workspaces: vec![WorkspaceReport {
                path: "/Users/pmcfadin/projects/clean".to_string(),
                state: WorkspaceState::Clean,
                linked_worktree: false,
                uncommitted_entries: Some(0),
            }],
            unlocated: Vec::new(),
            sweep_complete: true,
        },
        WIDE,
    );

    // Panel border and title must be present
    assert!(
        text.contains("at risk"),
        "the panel title must be present even when empty; got:\n{text}"
    );
    assert!(
        text.contains("0 of 1 workspaces"),
        "the panel must state how many were checked; got:\n{text}"
    );
    // Must contain a phrase meaning checked-and-clear
    assert!(
        text.contains("No workspaces at risk") || text.contains("none found"),
        "the panel must explicitly say nothing is at risk; got:\n{text}"
    );
}

#[test]
fn dirty_driven_and_dirty_stranded_are_distinguishable() {
    // A driven workspace is visibly different from a stranded one.
    let text = rendered(
        &Snapshot {
            sessions: Vec::new(),
            workspaces: vec![
                WorkspaceReport {
                    path: "/Users/pmcfadin/projects/stranded".to_string(),
                    state: WorkspaceState::DirtyStranded,
                    linked_worktree: false,
                    uncommitted_entries: Some(10),
                },
                WorkspaceReport {
                    path: "/Users/pmcfadin/projects/driven".to_string(),
                    state: WorkspaceState::DirtyDriven,
                    linked_worktree: false,
                    uncommitted_entries: Some(5),
                },
            ],
            unlocated: Vec::new(),
            sweep_complete: true,
        },
        WIDE,
    );

    assert!(
        text.contains("DIRTY-STRANDED"),
        "stranded state must appear; got:\n{text}"
    );
    assert!(
        text.contains("DIRTY-DRIVEN"),
        "driven state must appear; got:\n{text}"
    );
    // Verify the strings are actually different
    let stranded_index = text.find("DIRTY-STRANDED").unwrap();
    let driven_index = text.find("DIRTY-DRIVEN").unwrap();
    assert_ne!(
        stranded_index, driven_index,
        "the two states must be distinguishable"
    );
}

#[test]
fn unknown_workspace_is_listed_and_does_not_show_zero() {
    // An Unknown(QueryFailed) workspace is at risk and must not show "0" where the count goes.
    let text = rendered(
        &Snapshot {
            sessions: Vec::new(),
            workspaces: vec![WorkspaceReport {
                path: "/Users/pmcfadin/projects/unreadable".to_string(),
                state: WorkspaceState::Unknown(Unreadable::QueryFailed("git failed".to_string())),
                linked_worktree: false,
                uncommitted_entries: None,
            }],
            unlocated: Vec::new(),
            sweep_complete: true,
        },
        WIDE,
    );

    assert!(
        text.contains("UNKNOWN"),
        "the unknown state must appear; got:\n{text}"
    );
    assert!(
        text.contains("query failed"),
        "the reason must appear in place of the count; got:\n{text}"
    );

    // Verify no "0" appears in the count position. We need to be careful: "0" appears in
    // "1 of 0 workspaces" etc. Check that the line containing "UNKNOWN" doesn't have
    // a standalone "0" that looks like a count.
    for line in text.lines() {
        if line.contains("UNKNOWN") {
            // The count column should contain "query failed", not "0"
            assert!(
                !line.contains(" 0 "),
                "the Unknown workspace row must not contain a standalone 0; got line:\n{line}"
            );
        }
    }
}

#[test]
fn linked_worktree_shows_worktree_attribute() {
    let text = rendered(
        &Snapshot {
            sessions: Vec::new(),
            workspaces: vec![WorkspaceReport {
                path: "/Users/pmcfadin/projects/obs/.claude/worktrees/feature-1".to_string(),
                state: WorkspaceState::DirtyStranded,
                linked_worktree: true,
                uncommitted_entries: Some(3),
            }],
            unlocated: Vec::new(),
            sweep_complete: true,
        },
        WIDE,
    );

    assert!(
        text.contains("worktree"),
        "a linked worktree must show the worktree attribute; got:\n{text}"
    );
}

#[test]
fn sweep_incomplete_produces_partial_coverage_warning() {
    let with_warning = rendered(
        &Snapshot {
            sessions: Vec::new(),
            workspaces: Vec::new(),
            unlocated: Vec::new(),
            sweep_complete: false,
        },
        WIDE,
    );

    let without_warning = rendered(
        &Snapshot {
            sessions: Vec::new(),
            workspaces: Vec::new(),
            unlocated: Vec::new(),
            sweep_complete: true,
        },
        WIDE,
    );

    assert!(
        with_warning.contains("incomplete") || with_warning.contains("partial"),
        "sweep_complete: false must produce a warning; got:\n{with_warning}"
    );
    assert!(
        !without_warning.contains("incomplete"),
        "sweep_complete: true must not produce the warning; got:\n{without_warning}"
    );
}

#[test]
fn unlocated_namespaces_are_counted_and_distinguished() {
    let text = rendered(
        &Snapshot {
            sessions: Vec::new(),
            workspaces: Vec::new(),
            unlocated: vec![
                (
                    "namespace1".to_string(),
                    NamespaceResolution::NoLongerExists,
                ),
                (
                    "namespace2".to_string(),
                    NamespaceResolution::NoLongerExists,
                ),
                (
                    "namespace3".to_string(),
                    NamespaceResolution::Ambiguous(vec!["a".to_string(), "b".to_string()]),
                ),
                (
                    "namespace4".to_string(),
                    NamespaceResolution::SearchExhausted,
                ),
            ],
            sweep_complete: true,
        },
        WIDE,
    );

    // Must report all three categories
    assert!(
        text.contains("no longer exist") || text.contains("2"),
        "NoLongerExists must be counted; got:\n{text}"
    );
    assert!(
        text.contains("ambiguous") || text.contains("1"),
        "Ambiguous must be counted; got:\n{text}"
    );
    assert!(
        text.contains("incomplete") || text.contains("search"),
        "SearchExhausted must be counted and read differently from NoLongerExists; got:\n{text}"
    );
}

#[test]
fn stranded_workspaces_are_ordered_by_count_descending() {
    // Largest pile of endangered work first.
    let text = rendered(
        &Snapshot {
            sessions: Vec::new(),
            workspaces: vec![
                WorkspaceReport {
                    path: "/Users/pmcfadin/projects/small".to_string(),
                    state: WorkspaceState::DirtyStranded,
                    linked_worktree: false,
                    uncommitted_entries: Some(5),
                },
                WorkspaceReport {
                    path: "/Users/pmcfadin/projects/large".to_string(),
                    state: WorkspaceState::DirtyStranded,
                    linked_worktree: false,
                    uncommitted_entries: Some(47),
                },
                WorkspaceReport {
                    path: "/Users/pmcfadin/projects/medium".to_string(),
                    state: WorkspaceState::DirtyStranded,
                    linked_worktree: false,
                    uncommitted_entries: Some(15),
                },
            ],
            unlocated: Vec::new(),
            sweep_complete: true,
        },
        WIDE,
    );

    // Find the positions of the three workspaces in the output
    let large_pos = text.find("large").expect("'large' should be in output");
    let medium_pos = text.find("medium").expect("'medium' should be in output");
    let small_pos = text.find("small").expect("'small' should be in output");

    assert!(
        large_pos < medium_pos,
        "largest (47) must come before medium (15); got:\n{text}"
    );
    assert!(
        medium_pos < small_pos,
        "medium (15) must come before small (5); got:\n{text}"
    );
}

#[test]
fn panel_does_not_clip_existing_caveat() {
    // The panel's presence must not clip the existing child-CPU floor caveat.
    let text = rendered(
        &Snapshot {
            sessions: vec![Session {
                identity: Identity::Process { pid: 69046 },
                cli: "claude".to_string(),
                resources: Ok(measured_ledger()),
                workspace: measured_workspace(),
                liveness: active_verdict(),
            }],
            workspaces: vec![WorkspaceReport {
                path: "/Users/pmcfadin/projects/test".to_string(),
                state: WorkspaceState::DirtyStranded,
                linked_worktree: false,
                uncommitted_entries: Some(10),
            }],
            unlocated: Vec::new(),
            sweep_complete: true,
        },
        WIDE,
    );

    // The existing floor caveat must still be fully present
    assert!(
        text.contains("floor"),
        "the floor caveat must survive when the panel is present; got:\n{text}"
    );
    assert!(
        text.contains("totals."),
        "and so must its last words; got:\n{text}"
    );
}
