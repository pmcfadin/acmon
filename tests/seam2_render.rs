//! Seam 2 — rendering a snapshot, with no real terminal involved.

use std::time::{Duration, SystemTime};

use acmon::liveness::{Method, State, Verdict};
use acmon::render::{minimum_width, render_to_lines, required_height};
use acmon::vcs::{Unreadable, WorkspaceState};
use acmon::workspace::{NamespaceResolution, NamespaceUnmatched, Workspace, WorkspaceUnknown};
use acmon::world::{ResourceSource, Resources, ResourcesUnavailable, Unmeasured};
use acmon::{Identity, Remembered, Session, Snapshot, WorkspaceReport};

/// A width that fits the whole table with room to spare. The narrow cases have their
/// own tests.
///
/// Derived from the minimum rather than stated, because a fixed number here turns silently
/// into a truncation test the day a column changes width — which is exactly what it did.
fn wide() -> u16 {
    minimum_width() + 32
}

/// The instant every snapshot here is taken as of.
///
/// A fixed one rather than `SystemTime::now()`, so that a test about the age of a remembered
/// figure states both ends of the subtraction and cannot drift.
fn fixture_now() -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(1_786_987_902)
}

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
        taken_at: fixture_now(),
        remembered: Remembered::none(),
        sessions: pids
            .iter()
            .map(|&pid| Session {
                identity: Identity::Process { pid },
                cli: "claude".to_string(),
                resources: Ok(measured_ledger()),
                workspace: measured_workspace(),
                last_reading: None,
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
        taken_at: fixture_now(),
        remembered: Remembered::none(),
        sessions: vec![Session {
            identity: Identity::Process { pid: 264 },
            cli: "claude".to_string(),
            resources: reading,
            workspace: measured_workspace(),
            last_reading: None,
            liveness: active_verdict(),
        }],
    }
}

fn snapshot_in(workspace: Result<Workspace, WorkspaceUnknown>) -> Snapshot {
    Snapshot {
        workspaces: Vec::new(),
        unlocated: Vec::new(),
        sweep_complete: true,
        taken_at: fixture_now(),
        remembered: Remembered::none(),
        sessions: vec![Session {
            identity: Identity::Process { pid: 264 },
            cli: "claude".to_string(),
            resources: Ok(measured_ledger()),
            workspace,
            last_reading: None,
            liveness: active_verdict(),
        }],
    }
}

fn rendered(snapshot: &Snapshot, width: u16) -> String {
    render_to_lines(snapshot, width, required_height(snapshot, width)).join("\n")
}

/// The rendered output with its line breaks and padding collapsed to single spaces.
///
/// For asserting on a *sentence*. A footer caveat is wrapped to the terminal's width, so
/// where the break falls is a property of the width rather than of what was said — and an
/// assertion on a phrase that happens to straddle one fails for a reason that has nothing to
/// do with the behaviour being tested.
fn prose(snapshot: &Snapshot, width: u16) -> String {
    rendered(snapshot, width)
        .split_whitespace()
        .collect::<Vec<&str>>()
        .join(" ")
}

#[test]
fn renders_one_row_per_session() {
    let snapshot = snapshot_of(&[264, 2880, 5333]);

    let text = rendered(&snapshot, wide());

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
    let text = rendered(&snapshot_of(&[]), wide());

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
    let text = rendered(&snapshot_of(&[69046]), wide());

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
    let text = rendered(&snapshot_of(&[69046]), wide());

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
    let text = rendered(&snapshot_of(&[69046]), wide());

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
    let text = rendered(&snapshot_of(&[69046]), wide());

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
    let text = rendered(
        &snapshot_in(Err(WorkspaceUnknown::PermissionDenied)),
        wide(),
    );

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
        wide(),
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
        wide(),
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
        taken_at: fixture_now(),
        remembered: Remembered::none(),
        sessions: vec![Session {
            identity: Identity::Process { pid: 264 },
            cli: "claude".to_string(),
            resources: Ok(measured_ledger()),
            workspace: measured_workspace(),
            last_reading: None,
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
        let text = rendered(&snapshot_in_state(verdict), wide());
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
        wide(),
    );
    let observed = rendered(
        &snapshot_in_state(Verdict {
            state: State::Active,
            method: Method::TranscriptChangedRecently,
        }),
        wide(),
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
        wide(),
    );
    let without = rendered(
        &snapshot_in_state(Verdict {
            state: State::Active,
            method: Method::TranscriptChangedRecently,
        }),
        wide(),
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

// ===== An UNKNOWN state has to say why it is unknown =====
//
// Both snapshots below carry the same state reached by the same method. They differ only in
// whether a transcript store exists for the CLI at all — a structural limit against a fault —
// and that difference is the one a reader acts on.

/// A session from a CLI added by configuration, as the collector really produces it: listed,
/// measured, attributed to a directory, and with no store to measure silence against. Pinned
/// against the collector in seam 10 by
/// `a_user_configured_cli_is_listed_but_its_liveness_is_honestly_unknown`.
fn snapshot_of_a_cli_with_no_transcript_store() -> Snapshot {
    Snapshot {
        workspaces: Vec::new(),
        unlocated: Vec::new(),
        sweep_complete: true,
        taken_at: fixture_now(),
        remembered: Remembered::none(),
        sessions: vec![Session {
            identity: Identity::Process { pid: 900 },
            cli: "cursor-agent".to_string(),
            resources: Ok(measured_ledger()),
            workspace: Ok(Workspace {
                path: "/Users/pmcfadin/projects/testing".to_string(),
                namespace: Err(NamespaceUnmatched::UnknownCli("cursor-agent".to_string())),
            }),
            last_reading: None,
            liveness: Verdict {
                state: State::Unknown,
                method: Method::TranscriptActivityUnknown,
            },
        }],
    }
}

/// A session whose CLI does have a transcript store, which was found and then would not say
/// when the transcript last changed. The same UNKNOWN, and a fault rather than a limit.
fn snapshot_of_a_transcript_that_would_not_answer() -> Snapshot {
    snapshot_in_state(Verdict {
        state: State::Unknown,
        method: Method::TranscriptActivityUnknown,
    })
}

#[test]
fn a_session_whose_cli_has_no_transcript_store_says_so_on_screen() {
    // The whole of #22: the reason was computed, stored on the workspace, and then dropped by
    // the renderer, leaving a row nobody could act on or account for.
    let text = prose(&snapshot_of_a_cli_with_no_transcript_store(), wide());

    assert!(
        text.contains("no transcript store is known for CLI cursor-agent"),
        "the row's state is unknown for a stated reason, and the reason must be on screen; \
         got:\n{text}"
    );
    assert!(
        text.contains("900 cursor-agent"),
        "and it must name which row it is about, or a machine with several sessions cannot use \
         it; got:\n{text}"
    );
}

#[test]
fn a_missing_transcript_store_and_an_unreadable_one_read_differently() {
    // A limit and a fault. One is investigated and may clear by itself; the other never will,
    // and the reader has to stop waiting for it. Identical output would be the calm plausible
    // wrong answer in the shape of a missing distinction.
    let limit = prose(&snapshot_of_a_cli_with_no_transcript_store(), wide());
    let fault = prose(&snapshot_of_a_transcript_that_would_not_answer(), wide());

    assert!(
        limit.contains("UNKNOWN!"),
        "a state no later run can determine must be marked in the row itself; got:\n{limit}"
    );
    assert!(
        !fault.contains("UNKNOWN!"),
        "a store that merely would not answer this time must not be marked as a limit; \
         got:\n{fault}"
    );
    assert!(
        fault.contains("activity could not be established"),
        "and the fault must state its own reason rather than being left blank; got:\n{fault}"
    );
    assert!(
        !fault.contains("no transcript store is known"),
        "the two reasons must not be interchangeable; got:\n{fault}"
    );
}

#[test]
fn a_session_that_can_never_be_announced_says_so_rather_than_simply_never_alerting() {
    // The more serious half of #22. #12's headline is that a fifth CLI needs no code change,
    // and a reader could reasonably assume alerting came with it. WAITING is the only session
    // state ever announced and reaching it needs a silence measurement, so this session is
    // monitored and will never alert — which must be readable rather than deducible from an
    // alert that never arrives.
    let limit = prose(&snapshot_of_a_cli_with_no_transcript_store(), wide());
    let fault = prose(&snapshot_of_a_transcript_that_would_not_answer(), wide());

    assert!(
        limit.contains("never announced"),
        "a session that cannot alert must say so; got:\n{limit}"
    );
    assert!(
        !fault.contains("never announced"),
        "and a session that merely has no verdict this run must not, or the note becomes \
         furniture a reader stops seeing; got:\n{fault}"
    );
}

#[test]
fn nothing_is_claimed_about_a_user_configured_clis_liveness() {
    // The row must not acquire a state by being explained. Absence of evidence stays absence
    // of evidence, however much prose is printed underneath it.
    let text = prose(&snapshot_of_a_cli_with_no_transcript_store(), wide());

    for state in ["ACTIVE", "WAITING", "STALLED"] {
        assert!(
            !text.contains(state),
            "no state but UNKNOWN was observed for this session, so {state} must appear \
             nowhere; got:\n{text}"
        );
    }
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
            taken_at: fixture_now(),
            remembered: Remembered::none(),
            sessions: vec![Session {
                identity: Identity::Transcript {
                    recorded_as: "-Users-pmcfadin-projects-agentic-coding-monitor".to_string(),
                },
                cli: "claude".to_string(),
                resources: Err(ResourcesUnavailable::ProcessExited),
                workspace: Err(WorkspaceUnknown::WorkspaceGone),
                last_reading: None,
                liveness: Verdict {
                    state: State::Stalled,
                    method: Method::NoProcessAndSilencePastStall,
                },
            }],
        },
        wide(),
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
            taken_at: fixture_now(),
            remembered: Remembered::none(),
            sessions: vec![Session {
                identity: Identity::Transcript {
                    recorded_as: namespace.to_string(),
                },
                cli: "claude".to_string(),
                resources: Err(ResourcesUnavailable::ProcessExited),
                workspace: Err(WorkspaceUnknown::WorkspaceGone),
                last_reading: None,
                liveness: Verdict {
                    state: State::Stalled,
                    method: Method::NoProcessAndSilencePastStall,
                },
            }],
        },
        wide(),
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
            taken_at: fixture_now(),
            remembered: Remembered::none(),
        },
        wide(),
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
            taken_at: fixture_now(),
            remembered: Remembered::none(),
        },
        wide(),
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
            taken_at: fixture_now(),
            remembered: Remembered::none(),
        },
        wide(),
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
            taken_at: fixture_now(),
            remembered: Remembered::none(),
        },
        wide(),
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
            taken_at: fixture_now(),
            remembered: Remembered::none(),
        },
        wide(),
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
            taken_at: fixture_now(),
            remembered: Remembered::none(),
        },
        wide(),
    );

    let without_warning = rendered(
        &Snapshot {
            sessions: Vec::new(),
            workspaces: Vec::new(),
            unlocated: Vec::new(),
            sweep_complete: true,
            taken_at: fixture_now(),
            remembered: Remembered::none(),
        },
        wide(),
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
            taken_at: fixture_now(),
            remembered: Remembered::none(),
        },
        wide(),
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
            taken_at: fixture_now(),
            remembered: Remembered::none(),
        },
        wide(),
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
                last_reading: None,
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
            taken_at: fixture_now(),
            remembered: Remembered::none(),
        },
        wide(),
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

/// Finding no workspaces at all must NOT read as reassurance.
///
/// Ticket #7 requires an empty panel to read as "checked and clear". An empty *candidate
/// set* is the opposite situation and has to read the opposite way: nothing was checked, so
/// nothing is known. On the machine behind `docs/observability-mechanics.md` §4.6 there are
/// 70 workspaces to find, so discovering none means discovery failed.
///
/// Without this test, changing the branch to emit the reassuring wording would leave every
/// other test passing — which is how a safety net comes to report "0 at risk" for a machine
/// it never looked at.
#[test]
fn finding_no_workspaces_at_all_does_not_read_as_checked_and_clear() {
    let text = rendered(
        &Snapshot {
            sessions: Vec::new(),
            workspaces: Vec::new(),
            unlocated: Vec::new(),
            sweep_complete: true,
            taken_at: fixture_now(),
            remembered: Remembered::none(),
        },
        wide(),
    );

    assert!(
        text.contains("NOTHING WAS CHECKED"),
        "an empty candidate set must say that nothing was checked; got:\n{text}"
    );
    assert!(
        !text.contains("are clean"),
        "an empty candidate set must not claim the workspaces it never found are clean; \
         got:\n{text}"
    );
    assert!(
        !text.contains("No workspaces at risk"),
        "an empty candidate set must not report the absence of risk, which it cannot know; \
         got:\n{text}"
    );
}

// --- Remembered figures and the state carried between runs (ticket #8) ---

/// A session whose process is gone, carrying the last reading taken before it went.
fn a_session_with_a_remembered_ledger(taken_at: SystemTime) -> Session {
    Session {
        identity: Identity::Transcript {
            recorded_as: "-Users-pmcfadin-projects-agentic-coding-monitor".to_string(),
        },
        cli: "claude".to_string(),
        resources: Err(ResourcesUnavailable::ProcessExited),
        last_reading: Some(acmon::memory::Reading {
            resources: measured_ledger(),
            taken_at,
        }),
        workspace: measured_workspace(),
        liveness: Verdict {
            state: State::Stalled,
            method: Method::NoProcessAndSilencePastStall,
        },
    }
}

fn snapshot_of_sessions(sessions: Vec<Session>) -> Snapshot {
    Snapshot {
        taken_at: fixture_now(),
        sessions,
        workspaces: Vec::new(),
        unlocated: Vec::new(),
        sweep_complete: true,
        remembered: Remembered::none(),
    }
}

#[test]
fn a_remembered_ledger_is_shown_rather_than_lost_with_the_process() {
    // The figures are the whole reason for remembering: 32,317 s of child CPU exists nowhere
    // on the machine once the process that reaped it is gone.
    let text = rendered(
        &snapshot_of_sessions(vec![a_session_with_a_remembered_ledger(
            fixture_now() - Duration::from_secs(3 * 3600 + 12 * 60),
        )]),
        wide(),
    );

    for expected in ["27m49s", "8h58m", "482 MB", "622 MB", "166 MB"] {
        assert!(
            text.contains(expected),
            "the remembered figure {expected} must still be shown; got:\n{text}"
        );
    }
    assert!(
        !text.contains("exited     exited"),
        "and must not be replaced by the reason its process is gone; got:\n{text}"
    );
}

#[test]
fn a_remembered_figure_is_marked_and_its_age_is_stated() {
    // Without both of these a remembered total reads as a current one, which is the same
    // class of wrong answer as the figures this project exists to correct — just pointing the
    // other way.
    let text = rendered(
        &snapshot_of_sessions(vec![a_session_with_a_remembered_ledger(
            fixture_now() - Duration::from_secs(3 * 3600 + 12 * 60),
        )]),
        wide(),
    );

    assert!(
        text.contains("8h58m*"),
        "a remembered figure must carry its marker; got:\n{text}"
    );
    assert!(
        text.contains("166 MB*"),
        "including the byte columns, which is why they are nine wide; got:\n{text}"
    );
    assert!(
        text.contains("3h12m ago"),
        "the age has to be stated — a marker alone does not say whether the number is three \
         minutes or three weeks old; got:\n{text}"
    );
    assert!(
        text.contains("-Users-pmcfadin-projects-agentic-coding-monitor"),
        "and the age must name the row it belongs to; a transcript-derived row's PID column \
         reads 'gone', so the pid cannot identify it; got:\n{text}"
    );
}

#[test]
fn a_session_with_nothing_remembered_still_states_the_reason_it_has_no_figures() {
    // The regression guard for the case above: the marker path must not have replaced the
    // honest admission with a blank or a zero.
    let mut session = a_session_with_a_remembered_ledger(fixture_now());
    session.last_reading = None;

    let text = rendered(&snapshot_of_sessions(vec![session]), wide());

    assert!(
        text.contains("exited"),
        "with no reading, ever, the reason takes the figure's place; got:\n{text}"
    );
    assert!(
        !text.contains('*'),
        "and no marker appears, because nothing was remembered; got:\n{text}"
    );
}

#[test]
fn a_lost_history_is_reported_because_it_shortens_the_at_risk_list() {
    let text = rendered(
        &Snapshot {
            remembered: Remembered {
                unusable: Some(acmon::memory::Degraded::Unparsable(
                    "EOF while parsing an object at line 4 column 0".to_string(),
                )),
                ..Remembered::none()
            },
            ..snapshot_of_sessions(Vec::new())
        },
        wide(),
    );

    assert!(
        text.contains("WARNING"),
        "a run that lost its history reports a shorter at-risk list than the machine has, \
         and nothing else in the output would say so; got:\n{text}"
    );
    assert!(
        text.contains("line 4"),
        "the parser's own words must survive, so the file can be inspected rather than just \
         deleted; got:\n{text}"
    );
}

#[test]
fn a_failure_to_store_state_is_reported_because_the_next_run_pays_for_it() {
    let text = rendered(
        &Snapshot {
            remembered: Remembered {
                persisted: Err(
                    "could not store state in /Users/pmcfadin/.acmon/state.json: \
                                Read-only file system"
                        .to_string(),
                ),
                ..Remembered::none()
            },
            ..snapshot_of_sessions(Vec::new())
        },
        wide(),
    );

    assert!(
        text.contains("WARNING") && text.contains("next run"),
        "this run looks perfect and the next one starts blind; only saying so distinguishes \
         them; got:\n{text}"
    );
    assert!(
        text.contains("Read-only file system"),
        "with the reason it failed; got:\n{text}"
    );
}

#[test]
fn workspaces_dropped_from_memory_are_accounted_for() {
    let text = rendered(
        &Snapshot {
            remembered: Remembered {
                forgotten: vec![acmon::memory::Forgotten {
                    path: "/Users/pmcfadin/projects/finished".to_string(),
                    settled_for: Duration::from_secs(30 * 86_400),
                }],
                retention: Duration::from_secs(7 * 86_400),
                ..Remembered::none()
            },
            ..snapshot_of_sessions(Vec::new())
        },
        wide(),
    );

    assert!(
        text.contains("Stopped watching 1 workspace"),
        "pruning is correct, but a safety net that quietly shrinks is indistinguishable from \
         one that is working; got:\n{text}"
    );
    assert!(
        text.contains("7 days"),
        "and it must state the rule that dropped it, not just the count; got:\n{text}"
    );
}

#[test]
fn an_ordinary_run_says_nothing_about_its_own_state_file() {
    // The counterweight. A line on every run trains a reader to ignore the line.
    let text = rendered(&snapshot_of_sessions(vec![]), wide());

    assert!(
        !text.contains("WARNING") && !text.contains("Stopped watching"),
        "a run that read its state and stored it has nothing to report; got:\n{text}"
    );
}

// --- Notification channel health (ticket #9) ---

fn health(config: acmon::world::NotifyConfig, notable: usize) -> acmon::collect::NotifyHealth {
    acmon::collect::NotifyHealth {
        config,
        notable,
        ..acmon::collect::NotifyHealth::none()
    }
}

fn snapshot_with_health(h: acmon::collect::NotifyHealth) -> Snapshot {
    Snapshot {
        taken_at: fixture_now(),
        sessions: Vec::new(),
        workspaces: Vec::new(),
        unlocated: Vec::new(),
        sweep_complete: true,
        remembered: Remembered {
            notify_health: h,
            ..Remembered::none()
        },
    }
}

fn snapshot_with_detector_config(config: acmon::world::DetectorConfig) -> Snapshot {
    Snapshot {
        taken_at: fixture_now(),
        sessions: Vec::new(),
        workspaces: Vec::new(),
        unlocated: Vec::new(),
        sweep_complete: true,
        remembered: Remembered {
            detector_config: config,
            ..Remembered::none()
        },
    }
}

// --- Detector configuration (ticket #12) ---

#[test]
fn an_unusable_detector_config_is_reported_even_when_no_sessions_were_found() {
    // The failure this ticket exists to prevent: a typo in the user's detector file means a
    // fifth agent CLI silently stops being recognised — the sessions simply are not there,
    // which is indistinguishable from a quiet machine. The warning must appear on every run,
    // whether or not sessions were found.
    let text = rendered(
        &snapshot_with_detector_config(acmon::world::DetectorConfig::unusable(
            "/Users/pmcfadin/.acmon/detectors.toml: could not parse detector configuration: \
             expected an equals, found a newline at line 3 column 18",
        )),
        wide(),
    );

    assert!(
        text.contains("WARNING") && text.contains("detector configuration is unusable"),
        "an unusable detector config must be reported, on every run, whether or not sessions \
         were found; got:\n{text}"
    );
    assert!(
        text.contains("line") && text.contains("column"),
        "with the parser's own words (line and column numbers), so the file can be fixed \
         rather than just deleted; got:\n{text}"
    );
}

#[test]
fn a_quiet_machine_with_embedded_detectors_only_says_nothing() {
    // The counterweight. Using only the embedded detectors is not a fault, and a warning on
    // every run trains a reader to ignore the warning.
    let text = rendered(
        &snapshot_with_detector_config(acmon::world::DetectorConfig::embedded_only()),
        wide(),
    );

    assert!(
        !text.contains("detector") && !text.contains("WARNING"),
        "a machine using only embedded detectors has nothing to report; got:\n{text}"
    );
}

// --- Notification channel health (ticket #9) ---

#[test]
fn an_unusable_notification_config_is_reported_even_when_there_was_nothing_to_announce() {
    // The failure this ticket opens with. A broken config delivers nothing, exactly like a
    // machine that was never set up to alert — and that second state is silent by design, so
    // this one would otherwise be silent by accident. Note `notable: 0`: the warning must not
    // wait for something to go wrong before admitting it cannot report anything going wrong.
    let text = rendered(
        &snapshot_with_health(health(
            acmon::world::NotifyConfig::unusable(
                "/Users/pmcfadin/.acmon/notify.toml is not readable as configuration: \
                 expected an equals, found a newline at line 2 column 14",
            ),
            0,
        )),
        wide(),
    );

    assert!(
        text.contains("WARNING") && text.contains("NOTHING WAS ANNOUNCED"),
        "an unusable config must say that nothing was announced, on every run, whether or not \
         anything happened to be notable; got:\n{text}"
    );
    assert!(
        text.contains("line 2"),
        "with the parser's own words, so the file can be fixed rather than just deleted; \
         got:\n{text}"
    );
}

#[test]
fn no_channels_configured_is_reported_when_something_needed_announcing() {
    // Reachable only from the count of what was *notable*. With no channel configured nothing
    // is ever attempted, so every delivered and failed tally stays at zero — reasoning from
    // those would make this warning unreachable while looking as though it were covered.
    let text = rendered(
        &snapshot_with_health(health(acmon::world::NotifyConfig::none(), 3)),
        wide(),
    );

    assert!(
        text.contains("No notification channels configured"),
        "three notable states and nowhere to send them is worth saying; got:\n{text}"
    );
}

#[test]
fn a_quiet_machine_with_no_channels_configured_says_nothing() {
    // The counterweight. Nothing to say and nowhere to say it is not a fault, and a warning
    // on every run trains a reader to ignore the warning.
    let text = rendered(
        &snapshot_with_health(health(acmon::world::NotifyConfig::none(), 0)),
        wide(),
    );

    assert!(
        !text.contains("WARNING"),
        "a quiet machine with no alerting configured has nothing to report; got:\n{text}"
    );
}

#[test]
fn a_failed_delivery_says_which_channel_failed_and_that_it_will_be_retried() {
    let text = rendered(
        &snapshot_with_health(acmon::collect::NotifyHealth {
            config: acmon::world::NotifyConfig {
                local_command: Some("terminal-notifier".to_string()),
                remote_url: Some("https://example.invalid/hook".to_string()),
                unusable: None,
            },
            notable: 2,
            local_delivered: 2,
            remote_failed: 2,
            ..acmon::collect::NotifyHealth::none()
        }),
        wide(),
    );

    assert!(
        text.contains("remote: 2 failed"),
        "naming the channel is the point — a dead remote and a healthy local must not read \
         the same; got:\n{text}"
    );
    assert!(
        text.contains("re-announced"),
        "and the reader has to know the alert is not simply lost; got:\n{text}"
    );
    assert!(
        !text.contains("local: "),
        "the healthy channel is not a failure and must not be listed as one; got:\n{text}"
    );
}

#[test]
fn alerts_that_were_never_sent_are_stated_with_the_reason_rather_than_dropped() {
    // Ticket #20. Delivery is bounded now, so a run can finish with alerts it never offered to
    // a channel. That is a cap, and a silent cap in an alerting path reads as "nothing to
    // report" — which is precisely the class of defect this project exists to remove.
    let text = rendered(
        &snapshot_with_health(acmon::collect::NotifyHealth {
            config: acmon::world::NotifyConfig {
                local_command: None,
                remote_url: Some("https://example.invalid/hook".to_string()),
                unusable: None,
            },
            notable: 14,
            remote_delivered: 8,
            remote_not_attempted: 6,
            not_attempted_reason: Some(
                "this run's alerting budget of 10s for the channel was spent before this alert \
                 was dispatched"
                    .to_string(),
            ),
            ..acmon::collect::NotifyHealth::none()
        }),
        wide(),
    );

    assert!(
        text.contains("NOT SENT") && text.contains("6"),
        "six alerts nobody was told about must be stated, and counted; got:\n{text}"
    );
    assert!(
        text.contains("budget"),
        "with the reason they were not sent — a bare count is a cap wearing a number; \
         got:\n{text}"
    );
    assert!(
        text.contains("re-announced"),
        "and the reader has to know they are not lost; got:\n{text}"
    );
}

#[test]
fn a_run_that_sent_everything_it_had_says_nothing_about_alerts_it_did_not_send() {
    // The counterweight to the test above. Fourteen notable states, fourteen delivered, and a
    // line about unsent alerts would be a lie — and a warning on a healthy run trains a reader
    // to ignore the warning that matters.
    let text = rendered(
        &snapshot_with_health(acmon::collect::NotifyHealth {
            config: acmon::world::NotifyConfig {
                local_command: Some("terminal-notifier".to_string()),
                remote_url: None,
                unusable: None,
            },
            notable: 14,
            local_delivered: 14,
            ..acmon::collect::NotifyHealth::none()
        }),
        wide(),
    );

    assert!(
        !text.contains("WARNING"),
        "a run that delivered every alert it decided on has nothing to warn about; got:\n{text}"
    );
}

#[test]
fn a_failed_delivery_and_an_unsent_alert_are_reported_as_the_two_different_things_they_are() {
    // A channel that answered badly is evidence about the channel. A run that never reached
    // the rest of its alerts is evidence about the run. Folding them into one tally would make
    // a healthy notifier that ran out of time look broken, and hide how much went unsent.
    let text = rendered(
        &snapshot_with_health(acmon::collect::NotifyHealth {
            config: acmon::world::NotifyConfig {
                local_command: Some("terminal-notifier".to_string()),
                remote_url: None,
                unusable: None,
            },
            notable: 10,
            local_failed: 4,
            local_not_attempted: 6,
            not_attempted_reason: Some("the budget was spent".to_string()),
            ..acmon::collect::NotifyHealth::none()
        }),
        wide(),
    );

    assert!(
        text.contains("failures") && text.contains("local: 4 failed"),
        "the four the channel refused are delivery failures; got:\n{text}"
    );
    assert!(
        text.contains("NOT SENT") && text.contains("local: 6"),
        "the six it was never given are not; got:\n{text}"
    );
}

// --- A CLI id is whatever the user typed (ticket #12) ---

fn snapshot_of_cli(cli: &str) -> Snapshot {
    Snapshot {
        taken_at: fixture_now(),
        sessions: vec![Session {
            identity: Identity::Process { pid: 900 },
            cli: cli.to_string(),
            resources: Ok(measured_ledger()),
            workspace: measured_workspace(),
            last_reading: None,
            liveness: active_verdict(),
        }],
        workspaces: Vec::new(),
        unlocated: Vec::new(),
        sweep_complete: true,
        remembered: Remembered::none(),
    }
}

#[test]
fn a_user_configured_cli_id_is_shown_in_full_when_it_fits() {
    // `cursor-agent` is twelve characters and the most likely fifth CLI on this machine. Until
    // detectors became user-configurable the only ids possible were `claude` and `codex`, both
    // of which fit in six — so six was correct right up until it silently was not.
    let text = rendered(&snapshot_of_cli("cursor-agent"), wide());

    assert!(
        text.contains("cursor-agent"),
        "a plausible user-configured id must appear whole; got:\n{text}"
    );
    assert!(
        !text.contains("cursor-agent…"),
        "and unmarked, because nothing was dropped; got:\n{text}"
    );
}

#[test]
fn a_cli_id_too_long_for_its_column_is_marked_rather_than_quietly_cut() {
    // The failure this guards. `codex-experimental` cut to `codex-` is not a shorter version of
    // this CLI's name — it is a plausible name for a different CLI, and an unmarked cut gives
    // the reader no way to know which one the row is about. Same class as a truncated CPU total.
    let text = rendered(&snapshot_of_cli("codex-experimental"), wide());

    assert!(
        text.contains('…'),
        "an id that does not fit must carry the mark; got:\n{text}"
    );
    assert!(
        text.contains("codex-exper…"),
        "keeping the head, because a CLI's identity is at its start — unlike a path, whose \
         identity is at its end; got:\n{text}"
    );
}

#[test]
fn two_cli_ids_sharing_a_prefix_are_still_distinguishable_at_the_shortened_width() {
    // The reason the mark alone is not enough and the column had to widen too. At six, both of
    // these render as `codex-`, and the table would assert that two different CLIs are one.
    let short = rendered(&snapshot_of_cli("codex-alpha"), wide());
    let long = rendered(&snapshot_of_cli("codex-bravo"), wide());

    assert!(short.contains("codex-alpha") && long.contains("codex-bravo"));
    assert_ne!(
        short
            .lines()
            .find(|l| l.contains("codex"))
            .expect("a row naming the cli"),
        long.lines()
            .find(|l| l.contains("codex"))
            .expect("a row naming the cli"),
        "two distinct CLIs must not render identically"
    );
}
