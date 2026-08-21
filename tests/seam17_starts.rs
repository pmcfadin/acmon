//! Seam 17 — every launch records its downtime and whether the last exit was clean.
//!
//! The failure this seam exists to prevent: `amon` dies at 02:00, launchd `KeepAlive` restarts it,
//! and nothing anywhere says either thing happened. `state.json` after a night of dying and
//! restarting every ten seconds is the same file, with the same shape and the same plausible
//! figures, as `state.json` after a night of working. G1 and G2 stopped holding hours ago and the
//! machine reads as healthy.
//!
//! It is deliberately **not** prevented with a second process watching the first (N7, decision 31).
//! A watchdog for the watchdog dies just as quietly, and then there are two silent failures. So the
//! gap is made *visible* instead: one appended line per launch, saying when the monitor started,
//! how long nothing was being recorded, and whether the run before it ended on purpose.
//!
//! Two rules from `AGENTS.md` shape every test below.
//!
//! **A downtime is a subtraction over two stamps, so nothing here sleeps to make a gap.** The
//! launch instant, the last state write, and the previous record are all parameters of
//! `starts::decide`, which reads no clock and touches no file — so the arithmetic and its failure
//! modes are assertable in microseconds, and nothing is asserted about how long anything took.
//!
//! **None of the failure modes is zero.** A first launch, a state write stamped after the launch, a
//! record that will not parse and a lock record that will not read are four different sentences, and
//! all four are `null` plus a reason rather than a comfortable number. A downtime of `0` would read
//! as a monitor that restarted instantly; a previous exit of "clean" on a first launch would assert
//! a shutdown that never happened.
//!
//! The unclean case is proven with two real processes, because it can only be proven that way: the
//! evidence that a monitor died is a pid the kernel left in a lock file by releasing an `flock`
//! without the holder getting to tidy up, and no fake produces that. The `SIGKILL` goes to a child
//! this file started itself — the observe-never-act rule protects agent sessions, not test fixtures.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, SystemTime};

use acmon::lock::Predecessor;
use acmon::starts::{
    self, History, LastStateWrite, PreviousExit, StartRecord, CYCLING_THRESHOLD, RECENT_LAUNCHES,
    SHORT_RUN, STARTS_FILE,
};
use acmon::state::{Paths, StateStore, STATE_FILE};
use acmon::tiers::{self, Published};

// --- The arithmetic, and everything it refuses to guess ----------------------------------------

#[test]
fn a_first_launch_ever_reports_the_absence_of_a_previous_exit_rather_than_a_clean_one() {
    // The criterion this seam is most likely to get wrong by accident. An empty lock record is what
    // a clean release leaves *and* what a machine nobody has ever monitored leaves, so a monitor
    // reading the lock alone would report its very first start as following an orderly shutdown —
    // asserting that a monitor stopped on purpose when none has ever run.
    let record = starts::decide(
        an_instant(),
        4242,
        &LastStateWrite::Never,
        None,
        None,
        &History::NothingRecorded,
    );

    assert_eq!(record.previous_exit, PreviousExit::ABSENT);
    assert_ne!(
        record.previous_exit,
        PreviousExit::CLEAN,
        "a first launch has no exit to have been clean"
    );
    assert_eq!(record.launches.value, Some(1), "it is the first launch");
    assert_eq!(record.previous_pid, None);
    assert_eq!(
        record.downtime_secs.value, None,
        "and there is nothing to have been down from, so the downtime is not a figure"
    );
    assert!(
        record
            .downtime_secs
            .unavailable
            .as_deref()
            .is_some_and(|why| why.contains("first launch")),
        "the reason has to say which of the four this is: {:?}",
        record.downtime_secs.unavailable
    );
    assert_eq!(
        record.previous_uptime_secs.value, None,
        "and no previous run to have been short"
    );
    assert_eq!(record.previous_run_was_short, None);
    assert_eq!(record.cycling, None, "one launch is not a pattern");
}

#[test]
fn a_launch_after_a_clean_stop_says_it_was_clean_and_states_the_gap_it_measured() {
    // The other half of the same distinction. A clean release clears the pid record, so an empty
    // record *plus* a launch already on file is a monitor that stopped on purpose — and the gap
    // since its last write is a real measurement, not an absence.
    let previous = an_instant();
    let stopped_writing = previous + Duration::from_secs(3600);
    let relaunched = stopped_writing + Duration::from_secs(42);

    let record = starts::decide(
        relaunched,
        4243,
        &LastStateWrite::At(stopped_writing),
        None,
        None,
        &recorded(vec![launch_at(previous, 4242)]),
    );

    assert_eq!(record.previous_exit, PreviousExit::CLEAN);
    assert_eq!(
        record.downtime_secs.value,
        Some(42.0),
        "the downtime is the gap between the last state write and this launch"
    );
    assert_eq!(
        record.previous_uptime_secs.value,
        Some(3600.0),
        "and the previous run's length is its own launch to its own last write"
    );
    assert_eq!(
        record.previous_run_was_short,
        Some(false),
        "an hour is not a monitor that died on the way up"
    );
    assert_eq!(record.launches.value, Some(2));
    assert_eq!(record.cycling, None);
}

#[test]
fn a_launch_after_a_monitor_that_died_says_unclean_and_names_the_pid_it_took_over_from() {
    // The lock record is the evidence, and the two facts a boolean cannot hold are kept apart: a
    // dead predecessor and a predecessor still running without the lock are both unclean exits, and
    // both name the pid, because the reader has to be able to go and look.
    let previous = an_instant();
    let died_at = previous + Duration::from_secs(9);
    let relaunched = died_at + Duration::from_secs(10);

    let record = starts::decide(
        relaunched,
        4243,
        &LastStateWrite::At(died_at),
        Some(&Predecessor {
            pid: 4242,
            still_running: false,
        }),
        None,
        &recorded(vec![launch_at(previous, 4242)]),
    );

    assert_eq!(record.previous_exit, PreviousExit::UNCLEAN);
    assert_eq!(record.previous_pid, Some(4242));
    assert!(
        record.previous_exit_why.contains("4242")
            && record.previous_exit_why.contains("no longer running"),
        "the sentence on the line has to stand on its own: {}",
        record.previous_exit_why
    );
    assert_eq!(record.downtime_secs.value, Some(10.0));
    assert_eq!(
        record.previous_uptime_secs.value,
        Some(9.0),
        "nine seconds of uptime, which is what a crash loop looks like one launch at a time"
    );
    assert_eq!(record.previous_run_was_short, Some(true));
    assert_eq!(record.unclean_exits, 1);
}

#[test]
fn the_downtime_is_measured_from_the_last_state_write_and_needs_no_shutdown_record() {
    // The criterion, stated as the thing it rules out. A shutdown record subtracted from the next
    // launch is unmeasurable in exactly the case it exists to measure: a `SIGKILL`ed monitor never
    // writes one. So the gap is measured from the write the monitor was doing anyway — and the
    // unclean case below has a downtime for that reason, with nothing on disk but a state file.
    let died_at = an_instant();
    let record = starts::decide(
        died_at + Duration::from_secs(11),
        4243,
        &LastStateWrite::At(died_at),
        Some(&Predecessor {
            pid: 4242,
            still_running: false,
        }),
        None,
        &recorded(vec![launch_at(died_at - Duration::from_secs(5), 4242)]),
    );

    assert_eq!(
        record.downtime_secs.value,
        Some(11.0),
        "an unclean exit still has a downtime, because it is not the exit that is being measured"
    );
    assert_eq!(record.previous_exit, PreviousExit::UNCLEAN);
}

#[test]
fn a_state_write_stamped_after_the_launch_is_not_reported_as_no_downtime_at_all() {
    // Clock skew, a machine whose clock stepped backwards, or a state directory restored from a
    // backup. Subtraction would underflow, and either arm of the usual fix — clamping to zero or
    // reporting the wrapped figure — produces a number, and a number here reads as a measurement.
    let launched = an_instant();
    let record = starts::decide(
        launched,
        4243,
        &LastStateWrite::At(launched + Duration::from_secs(300)),
        None,
        None,
        &recorded(vec![launch_at(launched - Duration::from_secs(600), 4242)]),
    );

    assert_eq!(record.downtime_secs.value, None);
    let why = record.downtime_secs.unavailable.expect("a reason");
    assert!(
        why.contains("clock") && why.contains("300"),
        "the reason must say what it saw and what it concluded: {why}"
    );
    assert_eq!(
        record.previous_exit,
        PreviousExit::CLEAN,
        "an unusable clock does not change what the lock record said about the previous exit"
    );
}

#[test]
fn a_previous_launch_stamped_after_the_last_state_write_leaves_its_run_length_unknown() {
    // The same backwards clock seen from the other subtraction. The previous run cannot have ended
    // before it began, so its length is unknown — and unknown must not become `0`, which would be
    // the shortest run imaginable and would make every restart read as a crash loop.
    let last_write = an_instant();
    let record = starts::decide(
        last_write + Duration::from_secs(30),
        4243,
        &LastStateWrite::At(last_write),
        None,
        None,
        &recorded(vec![launch_at(last_write + Duration::from_secs(5), 4242)]),
    );

    assert_eq!(record.previous_uptime_secs.value, None);
    assert!(
        record
            .previous_uptime_secs
            .unavailable
            .as_deref()
            .is_some_and(|why| why.contains("backwards")),
        "{:?}",
        record.previous_uptime_secs.unavailable
    );
    assert_eq!(
        record.previous_run_was_short, None,
        "an unknown run length is not a short one; that is how a crash loop would hide behind a \
         fault in its own record"
    );
}

#[test]
fn a_last_state_write_that_cannot_be_read_leaves_the_downtime_unknown_rather_than_zero() {
    let record = starts::decide(
        an_instant(),
        4243,
        &LastStateWrite::Unreadable("the volume is not mounted".to_string()),
        None,
        None,
        &recorded(vec![launch_at(an_instant(), 4242)]),
    );

    assert_eq!(record.downtime_secs.value, None);
    assert!(
        record
            .downtime_secs
            .unavailable
            .as_deref()
            .is_some_and(|why| why.contains("the volume is not mounted")),
        "the underlying reason has to reach the reader: {:?}",
        record.downtime_secs.unavailable
    );
    assert_eq!(record.previous_uptime_secs.value, None);
}

#[test]
fn a_launch_record_that_cannot_be_read_is_never_a_directory_nothing_has_ever_run_in() {
    // "This file will not parse" and "no monitor has ever launched here" are opposite facts about a
    // machine, and the second is the reassuring one. So an unreadable history leaves the launch
    // count, the previous exit and the previous run length all unknown — with the reason attached to
    // each — rather than restarting the count from one.
    let record = starts::decide(
        an_instant(),
        4243,
        &LastStateWrite::At(an_instant()),
        None,
        None,
        &History::Unreadable("line 12 is not a launch record this tool wrote".to_string()),
    );

    assert_eq!(record.previous_exit, PreviousExit::UNKNOWN);
    assert_ne!(record.previous_exit, PreviousExit::CLEAN);
    assert_ne!(record.previous_exit, PreviousExit::ABSENT);
    assert_eq!(
        record.launches.value, None,
        "this launch cannot be numbered against a count nobody could take"
    );
    assert!(
        record
            .launches
            .unavailable
            .as_deref()
            .is_some_and(|why| why.contains("line 12")),
        "{:?}",
        record.launches.unavailable
    );
    assert_eq!(record.previous_uptime_secs.value, None);
}

#[test]
fn a_lock_record_that_cannot_be_read_leaves_the_previous_exit_unknown_rather_than_clean() {
    let record = starts::decide(
        an_instant(),
        4243,
        &LastStateWrite::At(an_instant()),
        None,
        Some("the lock file contains \"who knows\", which is not a pid this tool wrote"),
        &recorded(vec![launch_at(an_instant(), 4242)]),
    );

    assert_eq!(record.previous_exit, PreviousExit::UNKNOWN);
    assert!(
        record.previous_exit_why.contains("who knows"),
        "{}",
        record.previous_exit_why
    );
}

#[test]
fn state_written_here_with_no_launch_on_record_is_not_reported_as_a_clean_exit_either() {
    // The remaining ambiguity, and the reason this module consults more than the lock. A state
    // directory written by a build that kept no launch record has a cleared lock and no history, and
    // that is neither a first launch nor an orderly shutdown — it is a previous run this record
    // cannot vouch for, and it says so.
    let record = starts::decide(
        an_instant(),
        4243,
        &LastStateWrite::At(an_instant() - Duration::from_secs(60)),
        None,
        None,
        &History::NothingRecorded,
    );

    assert_eq!(record.previous_exit, PreviousExit::UNKNOWN);
    assert!(
        record.previous_exit_why.contains(STATE_FILE),
        "the reason names the evidence that a monitor ran here: {}",
        record.previous_exit_why
    );
    assert_eq!(
        record.downtime_secs.value,
        Some(60.0),
        "the downtime is still measurable: it comes from the state write, not from the history"
    );
}

// --- A crash loop as a pattern ------------------------------------------------------------------

#[test]
fn repeated_launches_after_short_runs_read_as_a_crash_loop_on_the_line_itself() {
    // The criterion about reconstruction. A reader must not have to diff consecutive lines to see
    // the shape: each line carries the run length of the launch before it, and the newest line
    // carries the verdict over the window. `tail -1 starts.jsonl` answers "is this monitor
    // cycling?".
    //
    // Driven through the real file rather than a hand-built history, because the window is fixed and
    // only the reader knows how far back it looks: a test that assembled its own history would be
    // asserting against its own idea of the window rather than the one a reader uses.
    let directory = scratch("cycling");
    let store = store_in(&directory);

    // Launches that die four seconds in, one every ten seconds — the shape launchd's
    // `ThrottleInterval` gives a monitor that cannot stay up.
    let mut at = an_instant();
    let mut records: Vec<StartRecord> = Vec::new();
    for launch in 0..RECENT_LAUNCHES as u32 + 1 {
        let record = starts::decide(
            at,
            4000 + launch,
            // The predecessor died four seconds into its own run, six seconds before this launch.
            &LastStateWrite::At(if launch == 0 {
                at
            } else {
                at - Duration::from_secs(6)
            }),
            (launch > 0).then_some(&Predecessor {
                pid: 3999 + launch,
                still_running: false,
            }),
            None,
            &starts::history(&store),
        );
        starts::append(&store, &record).expect("append the launch");
        records.push(record);
        at += Duration::from_secs(10);
    }

    // The first launch contributes no short run — it had no predecessor to have run at all — so the
    // window fills from the second, and the threshold is reached one launch later than its number.
    assert_eq!(
        records[0].cycling, None,
        "a first launch has no pattern to be part of"
    );
    assert_eq!(
        records[CYCLING_THRESHOLD - 1].cycling,
        None,
        "and {} short runs is still under the threshold of {CYCLING_THRESHOLD}: {:#?}",
        CYCLING_THRESHOLD - 1,
        records[CYCLING_THRESHOLD - 1]
    );

    let verdict = records[CYCLING_THRESHOLD]
        .cycling
        .as_deref()
        .unwrap_or_else(|| {
            panic!(
                "{CYCLING_THRESHOLD} short runs in the window is a monitor being restarted: {:#?}",
                records[CYCLING_THRESHOLD]
            )
        });
    assert!(
        verdict.contains(&SHORT_RUN.as_secs().to_string()),
        "the verdict says what counted as short, so it can be argued with: {verdict}"
    );

    let last = records.last().expect("every launch produced a record");
    assert!(last.is_cycling(), "{last:#?}");
    assert_eq!(
        last.launches_considered, RECENT_LAUNCHES,
        "the window is fixed, so the figure is a rate rather than a total that only grows"
    );
    assert_eq!(
        last.short_runs, RECENT_LAUNCHES,
        "every launch in the window followed a run of four seconds"
    );
    assert_eq!(
        last.unclean_exits, RECENT_LAUNCHES,
        "and every one of them followed a monitor that died rather than stopped"
    );
    assert_eq!(
        last.previous_run_was_short,
        Some(true),
        "the run length is on the line, so no reader has to reconstruct it from two of them"
    );
    assert_eq!(last.launches.value, Some(RECENT_LAUNCHES as u64 + 1));

    let _ = std::fs::remove_dir_all(&directory);
}

#[test]
fn a_monitor_that_ran_for_hours_before_stopping_is_not_reported_as_cycling() {
    // The false positive that would make the verdict worthless. A machine restarted once a day by a
    // reboot is not a crash loop, and a tool that called it one would teach its reader to ignore the
    // line.
    let directory = scratch("not-cycling");
    let store = store_in(&directory);

    let mut at = an_instant();
    let mut last = None;
    for launch in 0..RECENT_LAUNCHES as u32 + 1 {
        let record = starts::decide(
            at,
            4000 + launch,
            // The predecessor wrote state until thirty seconds ago, having launched eight hours
            // before that.
            &LastStateWrite::At(if launch == 0 {
                at
            } else {
                at - Duration::from_secs(30)
            }),
            None,
            None,
            &starts::history(&store),
        );
        starts::append(&store, &record).expect("append the launch");
        last = Some(record);
        at += Duration::from_secs(8 * 3600) + Duration::from_secs(30);
    }

    let last = last.expect("every launch produced a record");
    assert_eq!(last.cycling, None, "{last:#?}");
    assert_eq!(last.short_runs, 0);
    assert_eq!(last.previous_run_was_short, Some(false));
    assert_eq!(
        last.previous_exit,
        PreviousExit::CLEAN,
        "and every one of those exits was an orderly one"
    );

    let _ = std::fs::remove_dir_all(&directory);
}

// --- The file, and what it refuses to lose ------------------------------------------------------

#[test]
fn each_launch_appends_one_line_and_leaves_every_earlier_line_alone() {
    // Append-only, and `O_APPEND` rather than the write-temp-then-rename this crate uses for
    // whole-file state: rewriting the file to add a line would put the entire history at risk of
    // exactly the crash the history exists to record.
    let directory = scratch("append");
    let store = store_in(&directory);

    let first = starts::decide(
        an_instant(),
        4242,
        &LastStateWrite::Never,
        None,
        None,
        &History::NothingRecorded,
    );
    starts::append(&store, &first).expect("append the first");
    let second = starts::decide(
        an_instant() + Duration::from_secs(60),
        4243,
        &LastStateWrite::At(an_instant() + Duration::from_secs(50)),
        None,
        None,
        &starts::history(&store),
    );
    starts::append(&store, &second).expect("append the second");

    let raw = std::fs::read_to_string(directory.join(STARTS_FILE)).expect("the record is readable");
    assert_eq!(
        raw.lines().count(),
        2,
        "one line per launch, and one line only:\n{raw}"
    );
    assert!(
        raw.ends_with('\n'),
        "each line is terminated, so an appended one cannot land on the end of the last:\n{raw}"
    );

    match starts::history(&store) {
        History::Recorded { launches, recent } => {
            assert_eq!(launches, 2);
            assert_eq!(recent[0], first, "the first launch survives the second");
            assert_eq!(recent[1], second);
        }
        other => panic!("two appended records are a history of two: {other:?}"),
    }

    let _ = std::fs::remove_dir_all(&directory);
}

#[test]
fn a_line_the_reader_cannot_understand_makes_the_history_unreadable_rather_than_shorter() {
    // A skipped line is a launch that silently never happened, and this file exists precisely so
    // that launches stop being silent. A shorter history is the shape of a healthy one.
    let directory = scratch("bad-line");
    let store = store_in(&directory);
    std::fs::create_dir_all(&directory).expect("create the state directory");

    let good = starts::decide(
        an_instant(),
        4242,
        &LastStateWrite::Never,
        None,
        None,
        &History::NothingRecorded,
    );
    starts::append(&store, &good).expect("append");
    std::fs::OpenOptions::new()
        .append(true)
        .open(directory.join(STARTS_FILE))
        .and_then(|mut file| std::io::Write::write_all(&mut file, b"{ half a rec\n"))
        .expect("append a line no reader can parse");

    match starts::history(&store) {
        History::Unreadable(why) => {
            assert!(
                why.contains("line 2"),
                "the reason says which line, or nobody can fix it: {why}"
            );
            assert!(why.contains(STARTS_FILE), "and which file: {why}");
        }
        other => panic!("a record with an unparsable line is unreadable, not shorter: {other:?}"),
    }
    assert!(
        !starts::history(&store).determined(),
        "and status must therefore treat the question as unanswered"
    );

    let _ = std::fs::remove_dir_all(&directory);
}

#[test]
fn an_absent_record_file_is_nothing_recorded_and_not_a_failure_to_read_one() {
    let directory = scratch("absent");
    let store = store_in(&directory);

    let history = starts::history(&store);
    assert_eq!(history, History::NothingRecorded);
    assert!(history.determined(), "an absence is an answer");
    assert_eq!(history.launches().value, Some(0));
    assert_eq!(history.last(), None);

    let _ = std::fs::remove_dir_all(&directory);
}

// --- Two real monitors --------------------------------------------------------------------------

#[test]
fn a_first_launch_a_clean_restart_and_a_killed_monitor_are_three_different_records() {
    // The end-to-end case, and the only honest way to prove the middle two: the evidence that a
    // monitor exited cleanly is a pid record the lock *cleared*, and the evidence that one died is a
    // pid the kernel left behind by releasing an `flock` without the holder getting to tidy up.
    // Neither is producible by writing a file.
    let directory = scratch("three-launches");

    // 1. Nothing has ever run here.
    let (ok, _, stderr) = amon_run(&directory, &["watch"]);
    assert!(ok, "a bounded `amon watch` stops cleanly:\n{stderr}");
    assert!(
        stderr.contains("launch 1 recorded"),
        "and it says so at the moment it happens, into the log launchd keeps:\n{stderr}"
    );

    // 2. A restart after that clean stop.
    let (ok, _, _) = amon_run(&directory, &["watch"]);
    assert!(ok, "and so does the second");

    // 3. A monitor that is killed, and the launch that follows it.
    let mut victim = amon(&directory, &["watch"], Some("8000"));
    let victim_pid = writer_pid_once_published(&directory, &mut victim);
    // This test kills its own child. Nothing in the product signals anything — the
    // observe-never-act rule protects agent sessions — but a monitor dying mid-run is the case this
    // seam is about, and a hand-written lock file would prove nothing about the kernel.
    assert_eq!(
        unsafe { libc::kill(victim_pid as libc::pid_t, libc::SIGKILL) },
        0,
        "the SIGKILL must actually have been delivered"
    );
    assert!(
        !victim.wait().expect("reap the victim").success(),
        "a killed process did not exit cleanly, and its status must show it"
    );
    let (ok, _, stderr) = amon_run(&directory, &["watch"]);
    assert!(
        ok,
        "the successor takes a lock nobody holds and runs:\n{stderr}"
    );
    assert!(
        stderr.contains("did not exit cleanly") && stderr.contains(&victim_pid.to_string()),
        "and says whose death it is reporting:\n{stderr}"
    );

    let store = store_in(&directory);
    let recorded = match starts::history(&store) {
        History::Recorded { launches, recent } => {
            assert_eq!(launches, 4, "four launches, four lines");
            recent
        }
        other => panic!("four real launches are a history of four: {other:?}"),
    };

    assert_eq!(
        recorded[0].previous_exit,
        PreviousExit::ABSENT,
        "the first launch in an untouched directory had no predecessor: {:#?}",
        recorded[0]
    );
    assert_eq!(recorded[0].downtime_secs.value, None);

    assert_eq!(
        recorded[1].previous_exit,
        PreviousExit::CLEAN,
        "the second followed a monitor that released the lock: {:#?}",
        recorded[1]
    );
    assert!(
        recorded[1].downtime_secs.value.is_some(),
        "and the gap to its predecessor's last write is a real figure: {:?}",
        recorded[1].downtime_secs
    );

    assert_eq!(
        recorded[3].previous_exit,
        PreviousExit::UNCLEAN,
        "the fourth followed a SIGKILL: {:#?}",
        recorded[3]
    );
    assert_eq!(
        recorded[3].previous_pid,
        Some(victim_pid),
        "and it names the monitor that died"
    );
    assert!(
        recorded[3].downtime_secs.value.is_some(),
        "a killed monitor writes no shutdown record, and the downtime is measured anyway — from \
         the last state write: {:?}",
        recorded[3].downtime_secs
    );
    assert!(
        recorded[3].previous_uptime_secs.value.is_some(),
        "and the run it cut short has a length: {:?}",
        recorded[3].previous_uptime_secs
    );

    assert_ne!(
        recorded[1].previous_exit, recorded[3].previous_exit,
        "the whole criterion: a clean exit and an unclean one are distinguishable after the fact"
    );

    // Every launch is numbered, in order, without gaps: a record that renumbered from one after a
    // restart would hide the restart it was recording.
    let numbered: Vec<Option<u64>> = recorded
        .iter()
        .map(|record| record.launches.value)
        .collect();
    assert_eq!(numbered, vec![Some(1), Some(2), Some(3), Some(4)]);

    let _ = std::fs::remove_dir_all(&directory);
}

#[test]
fn the_launch_and_its_restart_count_reach_a_display_through_the_state_file() {
    // The criterion about the display. `starts.jsonl` is the durable record, but a human looks at a
    // screen, and a crash loop nobody looks up is the silent gap this seam exists to close. So the
    // launch is republished in the fast tier's payload and decoded through `acmon::tiers` — the same
    // schema the monitor wrote — rather than hand-parsed by whatever draws it.
    let directory = scratch("published");

    let (ok, _, stderr) = amon_run(&directory, &["watch"]);
    assert!(ok, "{stderr}");
    let (ok, _, stderr) = amon_run(&directory, &["watch"]);
    assert!(ok, "{stderr}");

    let store = store_in(&directory);
    let state = store
        .read_tiered_state(STATE_FILE)
        .expect("readable")
        .expect("a monitor ran here, so it is present");

    let payload = match tiers::published(&state, acmon::schedule::Tier::Fast)
        .expect("the fast payload decodes")
    {
        Some((Published::Fast(payload), _)) => payload,
        other => panic!("expected a fast payload, got {other:?}"),
    };

    assert_eq!(
        payload.launch.launches.value,
        Some(2),
        "the restart count is on screen without anyone reading starts.jsonl: {:#?}",
        payload.launch
    );
    assert_eq!(payload.launch.previous_exit, PreviousExit::CLEAN);
    assert_eq!(
        payload.launch_not_recorded, None,
        "and the durable append succeeded, which is a separate fact from the figures above"
    );

    // The published line and the appended line are the same record, not two accounts of one launch.
    let appended = starts::history(&store)
        .last()
        .cloned()
        .expect("two launches are on record");
    assert_eq!(
        payload.launch, appended,
        "the state file and the record must not be able to disagree about a launch"
    );

    let _ = std::fs::remove_dir_all(&directory);
}

#[test]
fn the_monitor_records_its_own_launch_so_nothing_supervises_it_but_launchd() {
    // N7 and decision 31, as behaviour. The gap is made visible by the monitor writing one line
    // about itself — not by a second job watching the first, which can die exactly as quietly and
    // would leave two silent failures instead of one. So the pid on the line is the monitor's own,
    // and when the run ends nothing at all is left running.
    let directory = scratch("no-watchdog");

    let monitor = amon(&directory, &["watch"], Some("500"));
    let monitor_pid = monitor.id();
    let output = monitor.wait_with_output().expect("the monitor exits");
    assert!(
        output.status.success(),
        "stderr was:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let store = store_in(&directory);
    let recorded = starts::history(&store)
        .last()
        .cloned()
        .expect("the launch was recorded");
    assert_eq!(
        recorded.pid, monitor_pid,
        "the launch was recorded by the process that launched, not by anything watching it"
    );
    assert!(
        !acmon::real_world::process_exists(monitor_pid as libc::pid_t),
        "and when the monitor stops, nothing it started is still running"
    );

    let _ = std::fs::remove_dir_all(&directory);
}

// --- Helpers ------------------------------------------------------------------------------------

/// A fixed instant, so nothing below depends on when the suite ran.
///
/// Well clear of the epoch in both directions: the tests subtract as well as add, and a stamp near
/// `UNIX_EPOCH` would make an ordinary subtraction fail for a reason that has nothing to do with
/// what is being tested.
fn an_instant() -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(1_787_000_000)
}

/// One record on file, as `decide` would have written it at that instant.
fn launch_at(at: SystemTime, pid: u32) -> StartRecord {
    starts::decide(
        at,
        pid,
        &LastStateWrite::Never,
        None,
        None,
        &History::NothingRecorded,
    )
}

fn recorded(recent: Vec<StartRecord>) -> History {
    History::Recorded {
        launches: recent.len() as u64,
        recent,
    }
}

/// A state directory that is this test's alone, removed on the way in.
///
/// No test may use the developer's own `~/.local/state/acmon`: it holds the real launch history of
/// the machine this project is written on, and a test that appended to it would be writing fiction
/// into the record it is meant to be proving.
fn scratch(name: &str) -> PathBuf {
    let directory =
        std::env::temp_dir().join(format!("acmon-seam17-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).expect("create the scratch directory");
    directory
}

fn store_in(directory: &Path) -> StateStore {
    let directory = directory.to_str().expect("utf-8");
    StateStore::new(
        Paths::from_values(Some(directory), Some(directory), None)
            .expect("explicit directories need no home"),
    )
}

/// Spawn `amon` with its state directory relocated and, optionally, a bounded run window.
///
/// The same shape seam 13 uses, for the same reason: `amon watch` is a resident monitor, so a test
/// either bounds its run or has to signal it, and the bound is what keeps these tests from
/// asserting anything about elapsed time.
fn amon(state_dir: &Path, arguments: &[&str], run_ms: Option<&str>) -> Child {
    let mut command = Command::new(env!("CARGO_BIN_EXE_amon"));
    command
        .args(arguments)
        .env(acmon::state::STATE_DIR_VARIABLE, state_dir)
        // The pre-split memory file is still `~/.acmon/state.json`, so relocating the state
        // directory alone would leave a running monitor writing the developer's own memory.
        .env("ACMON_STATE", state_dir.join("legacy-memory.json"))
        .env("ACMON_NOTIFY_CONFIG", state_dir.join("no-such-notify.toml"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(run) = run_ms {
        command.env(acmon::watch::RUN_VARIABLE, run);
    }
    command.spawn().expect("amon is built and runnable")
}

/// Run `amon` to completion: (succeeded, stdout, stderr).
fn amon_run(state_dir: &Path, arguments: &[&str]) -> (bool, String, String) {
    let child = amon(state_dir, arguments, Some("500"));
    let output = child.wait_with_output().expect("amon terminates");
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// Wait until *this* child is the writer named in the state file, and return its pid.
///
/// Naming the child specifically matters here in a way it does not in seam 13: these tests run
/// several monitors in one directory in turn, so `state.json` already names an earlier one. A helper
/// that returned whatever pid it found would hand back a dead monitor's, and the `SIGKILL` below
/// would fail with `ESRCH` while the test read as having killed something.
///
/// Bounded, and it checks the child is still alive every turn: a measurement believed from a process
/// that had already exited is the mistake `AGENTS.md` records twice.
fn writer_pid_once_published(state_dir: &Path, child: &mut Child) -> u32 {
    let store = store_in(state_dir);
    let expected = child.id();

    for _ in 0..400 {
        if let Some(status) = child.try_wait().expect("child status is readable") {
            panic!("`amon watch` exited ({status}) before publishing {STATE_FILE}");
        }
        if store
            .read_tiered_state(STATE_FILE)
            .expect("readable if present")
            .is_some_and(|state| state.writer_pid() == expected)
        {
            return expected;
        }
        std::thread::sleep(Duration::from_millis(25));
    }

    panic!("`amon watch` (pid {expected}) never named itself the writer in {STATE_FILE}");
}
