//! Seam 7 — pure liveness classification for agent sessions.
//!
//! A seventh seam, and a purely logical one. It exists because every defect in the tool
//! this project replaces was a calm, plausible, wrong answer rather than an error, and
//! each stemmed from reasoning about absence (a process is not resident, a transcript has
//! not changed) without checking whether the observation was trustworthy. The only defence
//! is a decision table implemented exactly as specified, with tests that prove every row
//! matters and that the order is load-bearing.

use acmon::liveness::{classify, Method, Observation, State, Thresholds};
use std::time::Duration;

/// Helper to build an Observation with all fields explicitly set. Reduces noise in tests
/// by making the common case (trustworthy snapshot, no workspace work) the default, and
/// the varying field the only one that changes between tests.
fn observed(
    silence: Option<Duration>,
    process_resident: bool,
    work_running_in_workspace: bool,
    snapshot_trustworthy: bool,
) -> Observation {
    Observation {
        silence,
        process_resident,
        work_running_in_workspace,
        snapshot_trustworthy,
    }
}

#[test]
fn row_1_transcript_activity_unknown_yields_unknown() {
    // When the transcript's last-modified time could not be read at all, nothing can be
    // said about the session's liveness. This row exists because the replacement project
    // must fail loud rather than defaulting to a plausible-looking zero.
    let thresholds = Thresholds::default();

    let verdict = classify(&observed(None, true, false, true), &thresholds);

    assert_eq!(verdict.state, State::Unknown);
    assert_eq!(verdict.method, Method::TranscriptActivityUnknown);
}

#[test]
fn row_2_untrustworthy_snapshot_with_absent_process_yields_unknown() {
    // When the process enumeration failed or was incomplete, the absence of a process in
    // the listing is not evidence that the process is gone — it might have been missed.
    // This row must come before any row that reasons from `!process_resident`, or an
    // enumeration failure could produce a false STALLED verdict.
    let thresholds = Thresholds::default();

    let verdict = classify(
        &observed(
            Some(Duration::from_secs(24 * 3600)), // 24 hours, well past stall
            false,                                // process not seen
            false,
            false, // snapshot untrustworthy
        ),
        &thresholds,
    );

    assert_eq!(verdict.state, State::Unknown);
    assert_eq!(verdict.method, Method::SnapshotCannotEstablishAbsence);
}

#[test]
fn row_3_silence_within_quiet_threshold_yields_active() {
    // The transcript changed recently enough to assert that the session is working. This
    // is a direct observation, not an inference.
    let thresholds = Thresholds::default();

    let verdict = classify(
        &observed(
            Some(Duration::from_secs(60)), // 1 minute
            false,
            false,
            true,
        ),
        &thresholds,
    );

    assert_eq!(verdict.state, State::Active);
    assert_eq!(verdict.method, Method::TranscriptChangedRecently);
}

#[test]
fn row_4_process_resident_but_silent_yields_waiting() {
    // The transcript is stale past the quiet threshold, but the process is still there.
    // Inferred: the session is probably waiting for human input, though the human might
    // have abandoned it while the process continues idling.
    let thresholds = Thresholds::default();

    let verdict = classify(
        &observed(
            Some(Duration::from_secs(20 * 60)), // 20 minutes, past quiet
            true,                               // process resident
            false,
            true,
        ),
        &thresholds,
    );

    assert_eq!(verdict.state, State::Waiting);
    assert_eq!(verdict.method, Method::ProcessResidentButSilent);
}

#[test]
fn row_5_work_running_in_workspace_yields_waiting() {
    // Work is running in the workspace — a build, a test run, a linter. This is
    // legitimate silence; the session is likely waiting for that work to complete. This
    // row must come before row 6 (the STALLED check), or live workspace activity could be
    // misclassified as a dead session.
    let thresholds = Thresholds::default();

    let verdict = classify(
        &observed(
            Some(Duration::from_secs(24 * 3600)), // 24 hours, well past stall
            false,                                // process absent
            true,                                 // but work is running
            true,
        ),
        &thresholds,
    );

    assert_eq!(verdict.state, State::Waiting);
    assert_eq!(verdict.method, Method::WorkRunningInWorkspace);
}

#[test]
fn row_6_no_process_and_silence_past_stall_yields_stalled() {
    // The process is absent and silence exceeds the stall threshold. This is the strongest
    // evidence that the session is dead, though even this cannot be certain — the
    // threshold is bounded by the sampling window that produced it.
    let thresholds = Thresholds::default();

    let verdict = classify(
        &observed(
            Some(Duration::from_secs(24 * 3600)), // 24 hours, past stall
            false,                                // process absent
            false,
            true,
        ),
        &thresholds,
    );

    assert_eq!(verdict.state, State::Stalled);
    assert_eq!(verdict.method, Method::NoProcessAndSilencePastStall);
}

#[test]
fn row_7_process_absent_before_stall_threshold_yields_unknown() {
    // The process is absent, but the stall threshold has not yet passed. Not enough
    // evidence to call it stalled; not enough to call it active. This row exists because
    // the acceptance criteria require *both* silence past the stall threshold *and* an
    // absent process before asserting STALLED.
    let thresholds = Thresholds::default();

    let verdict = classify(
        &observed(
            Some(Duration::from_secs(60 * 60)), // 1 hour, past quiet but before stall
            false,                              // process absent
            false,
            true,
        ),
        &thresholds,
    );

    assert_eq!(verdict.state, State::Unknown);
    assert_eq!(verdict.method, Method::ProcessAbsentBeforeStallThreshold);
}

#[test]
fn silence_exactly_equal_to_quiet_is_active() {
    // Boundary test. The decision table specifies `silence <= quiet` for ACTIVE, so
    // silence exactly equal to the threshold must still be ACTIVE, not WAITING. A
    // comparison error here (`<` instead of `<=`) would produce a plausible wrong verdict.
    let thresholds = Thresholds::default();

    let verdict = classify(
        &observed(Some(thresholds.quiet), false, false, true),
        &thresholds,
    );

    assert_eq!(verdict.state, State::Active);
    assert_eq!(verdict.method, Method::TranscriptChangedRecently);
}

#[test]
fn silence_exactly_equal_to_stall_is_unknown_not_stalled() {
    // Boundary test. The decision table specifies `silence > stall` for STALLED, so
    // silence exactly equal to the threshold is not yet stalled. A comparison error here
    // (`>=` instead of `>`) would produce a false STALLED verdict one threshold-length
    // early.
    let thresholds = Thresholds::default();

    let verdict = classify(
        &observed(
            Some(thresholds.stall),
            false, // process absent
            false,
            true,
        ),
        &thresholds,
    );

    // At exactly the stall threshold, with no process, the state is still UNKNOWN because
    // the threshold has not been *exceeded*. This is row 7, not row 6.
    assert_eq!(verdict.state, State::Unknown);
    assert_eq!(verdict.method, Method::ProcessAbsentBeforeStallThreshold);
}

#[test]
fn inferred_and_asserted_verdicts_are_distinguishable() {
    // Acceptance criterion 4 requires that an inferred verdict and an asserted verdict are
    // distinguishable. Pick a case where the *state* is identical, so the test proves the
    // distinction comes from the method, not the state.
    let thresholds = Thresholds::default();

    let inferred = classify(
        &observed(
            Some(Duration::from_secs(20 * 60)), // past quiet
            true,                               // process resident
            false,
            true,
        ),
        &thresholds,
    );

    let asserted = classify(
        &observed(
            Some(Duration::from_secs(60)), // within quiet
            false,
            false,
            true,
        ),
        &thresholds,
    );

    assert_eq!(inferred.state, State::Waiting);
    assert_eq!(asserted.state, State::Active);

    assert!(inferred.method.is_inferred(), "row 4 must be inferred");
    assert!(!asserted.method.is_inferred(), "row 3 must be asserted");
}

#[test]
fn untrustworthy_snapshot_never_yields_stalled_even_with_long_silence() {
    // Acceptance criterion 5 requires that STALLED is never asserted when the process
    // snapshot is untrustworthy. This test uses inputs that would otherwise produce
    // STALLED (long silence, no process), and confirms the verdict is UNKNOWN instead.
    // This test must fail if row 2 is removed or reordered after any row that reasons from
    // `!process_resident`.
    let thresholds = Thresholds::default();

    let verdict = classify(
        &observed(
            Some(Duration::from_secs(48 * 3600)), // 48 hours, far past stall
            false,                                // process absent
            false,
            false, // snapshot untrustworthy
        ),
        &thresholds,
    );

    assert_eq!(verdict.state, State::Unknown);
    assert_ne!(
        verdict.state,
        State::Stalled,
        "an untrustworthy snapshot must never yield STALLED"
    );
}

#[test]
fn work_running_in_workspace_never_yields_stalled() {
    // Acceptance criterion 6 requires that a live build prevents a false STALLED verdict.
    // This test uses inputs that would otherwise produce STALLED (long silence, no
    // process), and confirms the verdict is WAITING instead. This test must fail if row 5
    // is removed or reordered after row 6.
    let thresholds = Thresholds::default();

    let verdict = classify(
        &observed(
            Some(Duration::from_secs(48 * 3600)), // 48 hours, far past stall
            false,                                // process absent
            true,                                 // but work is running
            true,
        ),
        &thresholds,
    );

    assert_eq!(verdict.state, State::Waiting);
    assert_ne!(
        verdict.state,
        State::Stalled,
        "workspace activity must prevent STALLED"
    );
}

#[test]
fn default_quiet_threshold_exceeds_measured_post_assistant_p99() {
    // The default quiet threshold must sit above normal interaction cadence. Measured
    // post-assistant silence (i.e. a human has been asked something and has not replied)
    // is p99 3.9 minutes, from `docs/observability-mechanics.md` §3.3. The default must
    // exceed that, or WAITING verdicts become noise. This is not a tautology — the test
    // uses the measured value, not the default itself.
    let measured_p99 = Duration::from_secs_f64(3.9 * 60.0); // 3.9 minutes
    let thresholds = Thresholds::default();

    assert!(
        thresholds.quiet > measured_p99,
        "default quiet threshold must exceed measured post-assistant p99 of 3.9 minutes, \
         got {:?}",
        thresholds.quiet
    );
}

#[test]
fn default_stall_threshold_exceeds_measured_maximum_silence_followed_by_resumption() {
    // The default stall threshold must sit above measured legitimate silence. The longest
    // observed silence followed by resumption was 9.3 hours, from
    // `docs/observability-mechanics.md` §3.3. The default must exceed that, or sessions
    // that would resume are prematurely classified as STALLED. This is not a tautology —
    // the test uses the measured value, not the default itself.
    let measured_max = Duration::from_secs_f64(9.3 * 3600.0); // 9.3 hours
    let thresholds = Thresholds::default();

    assert!(
        thresholds.stall > measured_max,
        "default stall threshold must exceed measured maximum resumption gap of 9.3 hours, \
         got {:?}",
        thresholds.stall
    );
}
