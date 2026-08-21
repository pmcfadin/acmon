//! Seam 11 — two binaries, and the verb surface of the one that measures.
//!
//! The failure this seam exists to prevent: `amon` grows a verb list faster than it grows
//! implementations, and an invocation of a verb that does nothing yet exits zero. A caller —
//! a human, a LaunchAgent, a CI step — reads that zero as "the monitor is running". That is
//! the calm, plausible, wrong answer this project exists to eliminate, arriving through the
//! exit code rather than through a number on a screen.
//!
//! So every verb `amon` advertises must either do its job or fail loudly saying it cannot
//! yet. There is no third state, and no verb may be advertised in help while being absent
//! from the parser.

use std::process::Command;

use acmon::cli::{
    parse_agtop, parse_amon, AgtopRequest, AmonRequest, AmonVerb, CliError, VerbState, ONCE_FLAG,
};

// --- Parsing: the verb surface ---

#[test]
fn a_bare_invocation_is_an_error_rather_than_a_silent_success() {
    // `amon` alone has not asked for anything. Printing help and exiting zero would be a
    // process that did nothing and reported success, which is the whole hazard above.
    let parsed = parse_amon(Vec::<String>::new());

    assert!(
        matches!(parsed, Err(CliError::NoVerb)),
        "a bare invocation should be NoVerb, got {parsed:?}"
    );
}

#[test]
fn help_asked_for_explicitly_is_not_an_error() {
    // The distinction that matters: help *requested* is a job done, help *fallen back to* is
    // not.
    for flag in ["--help", "-h"] {
        let parsed = parse_amon(vec![flag.to_string()]);
        assert!(
            matches!(parsed, Ok(AmonRequest::Help)),
            "{flag} should parse as Help, got {parsed:?}"
        );
    }
}

#[test]
fn a_known_verb_parses_to_that_verb() {
    let parsed = parse_amon(vec!["watch".to_string()]);

    assert!(
        matches!(
            parsed,
            Ok(AmonRequest::Verb {
                verb: AmonVerb::Watch,
                foreground: false
            })
        ),
        "`watch` should parse to the Watch verb, got {parsed:?}"
    );
}

#[test]
fn an_unknown_verb_names_what_it_did_not_recognise() {
    // "unknown command" without the word is a reason the reader cannot act on — they cannot
    // tell a typo from an unsupported feature.
    let parsed = parse_amon(vec!["frobnicate".to_string()]);

    match parsed {
        Err(CliError::UnknownVerb(name)) => assert_eq!(name, "frobnicate"),
        other => panic!("expected UnknownVerb(\"frobnicate\"), got {other:?}"),
    }
}

#[test]
fn every_advertised_verb_is_one_the_parser_accepts() {
    // The drift this catches: a verb added to the help text and forgotten in the parser, so
    // the documentation promises something that errors as unknown.
    for verb in AmonVerb::all() {
        let parsed = parse_amon(vec![verb.name().to_string()]);
        assert!(
            matches!(parsed, Ok(AmonRequest::Verb { verb: parsed_verb, .. }) if parsed_verb == *verb),
            "help advertises `{}`, so the parser must accept it; got {parsed:?}",
            verb.name()
        );
    }
}

#[test]
fn the_usage_text_lists_every_verb_with_a_summary() {
    // The reverse drift: a verb the parser accepts but help never mentions, so it is
    // undiscoverable.
    let usage = acmon::cli::amon_usage();

    for verb in AmonVerb::all() {
        assert!(
            usage.contains(verb.name()),
            "usage text should list `{}`:\n{usage}",
            verb.name()
        );
        assert!(
            !verb.summary().is_empty(),
            "`{}` should carry a one-line summary",
            verb.name()
        );
    }
}

#[test]
fn a_verb_that_cannot_do_its_job_states_what_will_deliver_it() {
    // A verb that is recognised but not built says so, and says where the work is tracked.
    // "not implemented" alone leaves a reader unable to tell abandoned from forthcoming.
    //
    // `watch` is the awkward case and the reason Partial exists: it holds the single-writer
    // lock (#26) around a collection loop that is not built (#27), so it does real work and
    // still cannot monitor. Both halves have to be named, or a reader seeing the lock work
    // would conclude the verb works.
    let unbuilt: Vec<_> = AmonVerb::all()
        .iter()
        .filter(|verb| !matches!(verb.state(), VerbState::Available))
        .collect();

    assert!(
        !unbuilt.is_empty(),
        "this seam is meaningless once every verb is available; delete it then"
    );

    for verb in unbuilt {
        match verb.state() {
            VerbState::Planned { tracked_as } => assert!(
                !tracked_as.is_empty(),
                "planned verb `{}` should name the work that delivers it",
                verb.name()
            ),
            VerbState::Partial { built, tracked_as } => {
                assert!(
                    !built.is_empty(),
                    "partly built verb `{}` should name what it already has",
                    verb.name()
                );
                assert!(
                    !tracked_as.is_empty(),
                    "partly built verb `{}` should name the work that finishes it",
                    verb.name()
                );
            }
            VerbState::Available => unreachable!(),
        }
    }
}

#[test]
fn the_foreground_flag_is_only_a_flag_for_watch() {
    // `--foreground` is `watch`'s alone. Accepted silently elsewhere, it would read as
    // honoured — and the reader would believe they had asked for something they had not.
    let parsed = parse_amon(vec!["watch".to_string(), "--foreground".to_string()]);
    assert!(
        matches!(
            parsed,
            Ok(AmonRequest::Verb {
                verb: AmonVerb::Watch,
                foreground: true
            })
        ),
        "`watch --foreground` should parse as watch in the foreground, got {parsed:?}"
    );

    for verb in AmonVerb::all() {
        if *verb == AmonVerb::Watch {
            continue;
        }
        let parsed = parse_amon(vec![verb.name().to_string(), "--foreground".to_string()]);
        match parsed {
            Err(CliError::FlagNotValidFor { flag, verb: named }) => {
                assert_eq!(flag, "--foreground");
                assert_eq!(named, *verb);
            }
            other => panic!(
                "`{} --foreground` should be refused rather than ignored, got {other:?}",
                verb.name()
            ),
        }
    }
}

#[test]
fn an_argument_the_parser_does_not_understand_is_refused_rather_than_ignored() {
    let parsed = parse_amon(vec!["watch".to_string(), "--daemonize".to_string()]);

    match parsed {
        Err(CliError::UnexpectedArgument(argument)) => assert_eq!(argument, "--daemonize"),
        other => panic!("expected UnexpectedArgument(\"--daemonize\"), got {other:?}"),
    }
}

#[test]
fn the_usage_text_says_the_foreground_flag_is_still_subject_to_the_lock() {
    // The misreading this heads off: `--foreground` as a way to run a second monitor "just to
    // look". Two writers is two writers regardless of intent, and help has to say so where the
    // flag is discovered rather than in a ticket.
    let usage = acmon::cli::amon_usage();

    assert!(
        usage.contains("--foreground"),
        "help must document the flag:\n{usage}"
    );
    assert!(
        usage.contains("lock"),
        "help must say the flag is still subject to the lock:\n{usage}"
    );
}

#[test]
fn the_usage_text_names_the_launch_agent_as_the_only_file_written_outside_our_own_directories() {
    // F24 requires this to be *documented*, not merely true, and help is where somebody about to
    // run `amon install` is standing. The claim is specific enough to be checkable: two
    // directories this tool owns, one plist outside them, and no `sudo` anywhere.
    let usage = acmon::cli::amon_usage();

    for expected in [
        "~/.config/acmon/",
        "~/.local/state/acmon/",
        "~/Library/LaunchAgents/",
        acmon::launchd::LABEL,
        "sudo",
    ] {
        assert!(
            usage.contains(expected),
            "help must name {expected} where an installer would read it:\n{usage}"
        );
    }
    assert!(
        usage.to_lowercase().contains("only file"),
        "and it must say that the plist is the only file written outside them:\n{usage}"
    );
}

// --- Parsing: the display's surface ---

#[test]
fn a_bare_agtop_is_the_full_screen_because_that_is_what_the_tool_is_for() {
    // The opposite of `amon`, deliberately. A bare `amon` has asked for nothing, while a bare
    // `agtop` has asked for the thing the binary exists to do.
    assert_eq!(
        parse_agtop(Vec::<String>::new()),
        Ok(AgtopRequest::Live),
        "a bare `agtop` should take the screen"
    );
}

#[test]
fn the_one_shot_flag_asks_for_lines_instead_of_a_screen() {
    assert_eq!(
        parse_agtop(vec![ONCE_FLAG.to_string()]),
        Ok(AgtopRequest::Once)
    );
}

#[test]
fn agtop_asked_for_help_is_not_an_error() {
    for flag in ["--help", "-h"] {
        assert_eq!(
            parse_agtop(vec![flag.to_string()]),
            Ok(AgtopRequest::Help),
            "{flag} should parse as Help"
        );
    }
}

#[test]
fn agtop_refuses_an_argument_it_does_not_understand_rather_than_ignoring_it() {
    // Including the monitor's verbs. `agtop watch` reading as a successful monitor start would
    // undo the whole point of there being two names.
    for argument in ["watch", "--follow", "-x"] {
        match parse_agtop(vec![argument.to_string()]) {
            Err(CliError::UnexpectedArgument(named)) => assert_eq!(named, argument),
            other => panic!("`agtop {argument}` should be refused, got {other:?}"),
        }
    }
}

#[test]
fn agtops_usage_text_documents_both_modes_and_the_absence_of_keybindings() {
    let usage = acmon::cli::agtop_usage();

    assert!(
        usage.contains(ONCE_FLAG),
        "the pipeable mode has to be discoverable:\n{usage}"
    );
    assert!(
        usage.contains("read-only"),
        "the display's central promise belongs where the display is discovered:\n{usage}"
    );
    assert!(
        usage.contains("keybindings"),
        "the absence of keybindings is a requirement, not an omission, so help says so:\n{usage}"
    );
}

// --- The binaries themselves ---
//
// These spawn the built binaries, which is the only way to assert on an exit code. Kept to
// the few cases where the exit code *is* the behaviour under test — this repo pays the exec
// tax it measures, and a spawn per assertion would be us doing the thing we complain about.

/// Run a binary with its state directory relocated.
///
/// The relocation matters now that `amon watch` takes a lock and publishes a state file: a
/// suite that used the developer's own `~/.local/state/acmon/` would write real state as a side
/// effect of testing an exit code.
fn scratch_state_dir() -> std::path::PathBuf {
    std::env::temp_dir().join(format!("acmon-seam11-{}-state", std::process::id()))
}

fn run(binary: &str, args: &[&str]) -> (bool, String, String) {
    let state_dir = scratch_state_dir();

    let output = Command::new(binary)
        .args(args)
        .env(acmon::state::STATE_DIR_VARIABLE, &state_dir)
        .output()
        .unwrap_or_else(|error| panic!("failed to run {binary} {args:?}: {error}"));

    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[test]
fn amon_help_exits_zero_and_lists_its_verbs_on_stdout() {
    let (success, stdout, _) = run(env!("CARGO_BIN_EXE_amon"), &["--help"]);

    assert!(success, "`amon --help` should exit zero");
    for verb in AmonVerb::all() {
        assert!(
            stdout.contains(verb.name()),
            "`amon --help` should list `{}` on stdout:\n{stdout}",
            verb.name()
        );
    }
}

#[test]
fn amon_with_no_arguments_exits_non_zero() {
    let (success, _, stderr) = run(env!("CARGO_BIN_EXE_amon"), &[]);

    assert!(
        !success,
        "a bare `amon` did nothing, so it must not report success"
    );
    assert!(
        !stderr.trim().is_empty(),
        "a bare `amon` should say what it wanted on stderr"
    );
}

#[test]
fn a_verb_that_cannot_do_its_job_exits_non_zero_rather_than_succeeding_at_nothing() {
    // The load-bearing case. A LaunchAgent that runs `amon watch` and sees zero will report
    // a healthy monitor for as long as the verb remains unbuilt — and `watch` is the one that
    // does some of its work, which makes it the one most likely to be read as working.
    for verb in AmonVerb::all() {
        if matches!(verb.state(), VerbState::Available) {
            continue;
        }

        let (success, stdout, stderr) = run(env!("CARGO_BIN_EXE_amon"), &[verb.name()]);

        assert!(
            !success,
            "`amon {}` is not implemented, so it must exit non-zero",
            verb.name()
        );
        assert!(
            stderr.contains(verb.name()),
            "`amon {}` should name the verb it cannot run yet; stderr was:\n{stderr}",
            verb.name()
        );
        assert!(
            stdout.trim().is_empty(),
            "a refusal belongs on stderr, not stdout; stdout was:\n{stdout}"
        );
    }

    // `amon watch` publishes a state file before it stops, so this test is one of the few that
    // leaves anything behind.
    let _ = std::fs::remove_dir_all(scratch_state_dir());
}

#[test]
fn amon_rejects_an_unknown_verb_non_zero_and_names_it() {
    let (success, _, stderr) = run(env!("CARGO_BIN_EXE_amon"), &["frobnicate"]);

    assert!(!success, "an unknown verb should exit non-zero");
    assert!(
        stderr.contains("frobnicate"),
        "stderr should name the unrecognised verb:\n{stderr}"
    );
}

#[test]
fn agtop_never_exits_zero_having_printed_nothing() {
    // Fail loud, never fail to zero. Whatever the state of this machine, exactly one of
    // these is true: a rendering on stdout with success, or a stated reason on stderr with
    // failure. Silence-and-success is the outcome that must be impossible, because it is
    // indistinguishable from a machine with no agents running.
    //
    // Asserted through `--once`, which is the mode that prints lines. A bare `agtop` takes the
    // screen, and there is no screen to take from a test harness — its refusal to draw into a
    // pipe is asserted in seam 14, alongside the rest of the display's behaviour.
    let (success, stdout, stderr) = run(env!("CARGO_BIN_EXE_agtop"), &[ONCE_FLAG]);

    if success {
        assert!(
            !stdout.trim().is_empty(),
            "agtop exited zero, so it must have rendered something"
        );
    } else {
        assert!(
            !stderr.trim().is_empty(),
            "agtop failed, so it must have stated why"
        );
    }
}

#[test]
fn agtop_does_not_advertise_the_monitors_verbs() {
    // The split is only useful if the two names mean different things. `agtop watch` reading
    // as a successful monitor start would undo it.
    let (success, _, _) = run(env!("CARGO_BIN_EXE_agtop"), &["watch"]);

    assert!(
        !success,
        "`agtop watch` should be rejected — watching is the monitor's job"
    );
}
