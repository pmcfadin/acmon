//! Seam 6 — pure VCS classification logic.
//!
//! Pure, like seam 4 and seam 5: the logic is deterministic and depends only on its
//! arguments. No I/O, no clock, no world.

use acmon::vcs::{classify, Unreadable, VcsFacts, WorkspaceState};

fn clean_facts() -> VcsFacts {
    VcsFacts {
        root: "/Users/test/repo".to_string(),
        uncommitted_entries: 0,
        linked_worktree: false,
    }
}

fn dirty_facts() -> VcsFacts {
    VcsFacts {
        root: "/Users/test/repo".to_string(),
        uncommitted_entries: 3,
        linked_worktree: false,
    }
}

#[test]
fn clean_workspace_is_clean_regardless_of_session() {
    assert_eq!(
        classify(&Ok(clean_facts()), true),
        WorkspaceState::Clean,
        "a clean workspace with a session driving is CLEAN"
    );
    assert_eq!(
        classify(&Ok(clean_facts()), false),
        WorkspaceState::Clean,
        "a clean workspace with no session driving is CLEAN"
    );
}

#[test]
fn dirty_workspace_with_session_is_dirty_driven() {
    assert_eq!(
        classify(&Ok(dirty_facts()), true),
        WorkspaceState::DirtyDriven,
        "a dirty workspace with a session driving is DIRTY-DRIVEN"
    );
}

#[test]
fn dirty_workspace_without_session_is_dirty_stranded() {
    assert_eq!(
        classify(&Ok(dirty_facts()), false),
        WorkspaceState::DirtyStranded,
        "a dirty workspace with no session driving is DIRTY-STRANDED"
    );
}

#[test]
fn unreadable_becomes_unknown_with_reason_carried() {
    assert_eq!(
        classify(&Err(Unreadable::NotVersionControlled), true),
        WorkspaceState::Unknown(Unreadable::NotVersionControlled)
    );
    assert_eq!(
        classify(&Err(Unreadable::PathGone), false),
        WorkspaceState::Unknown(Unreadable::PathGone)
    );
    assert_eq!(
        classify(
            &Err(Unreadable::QueryFailed("git failed".to_string())),
            true
        ),
        WorkspaceState::Unknown(Unreadable::QueryFailed("git failed".to_string()))
    );
    assert_eq!(
        classify(&Err(Unreadable::TimedOut), false),
        WorkspaceState::Unknown(Unreadable::TimedOut)
    );
}

#[test]
fn at_risk_is_true_for_dirty_stranded() {
    let state = WorkspaceState::DirtyStranded;
    assert!(
        state.at_risk(),
        "DIRTY-STRANDED is at-risk — this is the primary detection target"
    );
}

#[test]
fn at_risk_is_true_for_query_failed_and_timed_out() {
    // These are genuine read failures where the state is unknown because version control
    // exists but would not answer. The precautionary principle applies.
    let query_failed = WorkspaceState::Unknown(Unreadable::QueryFailed("error".to_string()));
    let timed_out = WorkspaceState::Unknown(Unreadable::TimedOut);

    assert!(
        query_failed.at_risk(),
        "UNKNOWN(QueryFailed) is at-risk — we cannot tell if uncommitted work exists"
    );
    assert!(
        timed_out.at_risk(),
        "UNKNOWN(TimedOut) is at-risk — we cannot tell if uncommitted work exists"
    );
}

#[test]
fn at_risk_is_false_for_answers_not_read_failures() {
    // NotVersionControlled and PathGone are ANSWERS, not read failures. They say "there
    // is no version control" and "the path is gone", which are states of the world rather
    // than states of our knowledge. A workspace with no version control cannot hold
    // uncommitted versioned changes, and a gone path is already lost.
    let not_vc = WorkspaceState::Unknown(Unreadable::NotVersionControlled);
    let path_gone = WorkspaceState::Unknown(Unreadable::PathGone);

    assert!(
        !not_vc.at_risk(),
        "UNKNOWN(NotVersionControlled) is NOT at-risk — there is no version control, so \
         there is nothing to lose"
    );
    assert!(
        !path_gone.at_risk(),
        "UNKNOWN(PathGone) is NOT at-risk — the workspace is already gone, so there is \
         nothing left to protect"
    );
}

#[test]
fn at_risk_is_false_for_clean_and_dirty_driven() {
    assert!(
        !WorkspaceState::Clean.at_risk(),
        "CLEAN is not at-risk — no uncommitted work"
    );
    assert!(
        !WorkspaceState::DirtyDriven.at_risk(),
        "DIRTY-DRIVEN is not at-risk — a live session is driving the workspace"
    );
}

#[test]
fn workspace_state_display_strings_are_uppercase_and_hyphenated() {
    assert_eq!(WorkspaceState::Clean.to_string(), "CLEAN");
    assert_eq!(WorkspaceState::DirtyDriven.to_string(), "DIRTY-DRIVEN");
    assert_eq!(WorkspaceState::DirtyStranded.to_string(), "DIRTY-STRANDED");
    assert_eq!(
        WorkspaceState::Unknown(Unreadable::NotVersionControlled).to_string(),
        "UNKNOWN"
    );
}

#[test]
fn unreadable_display_strings_are_short_and_true() {
    // Short: they render in a table column.
    // True: each says what actually happened, not merely something plausible.
    assert_eq!(
        Unreadable::NotVersionControlled.to_string(),
        "no repository"
    );
    assert_eq!(Unreadable::PathGone.to_string(), "path gone");
    assert_eq!(
        Unreadable::QueryFailed("any message".to_string()).to_string(),
        "query failed"
    );
    assert_eq!(Unreadable::TimedOut.to_string(), "timed out");
}

#[test]
fn display_does_not_leak_the_query_failed_message() {
    // The QueryFailed variant carries what git said, but the Display impl must not echo
    // it — the message could be long, and the table column is narrow. A separate accessor
    // can retrieve it when debugging, but the short label is what renders.
    let state = WorkspaceState::Unknown(Unreadable::QueryFailed(
        "fatal: very long git error message that should not appear in the table".to_string(),
    ));
    assert_eq!(state.to_string(), "UNKNOWN");

    let reason = match state {
        WorkspaceState::Unknown(r) => r,
        _ => panic!("expected Unknown"),
    };
    assert_eq!(reason.to_string(), "query failed");
}
