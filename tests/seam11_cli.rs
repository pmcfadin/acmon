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

use acmon::cli::{parse_amon, AmonRequest, AmonVerb, CliError, VerbState};

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
        matches!(parsed, Ok(AmonRequest::Verb(AmonVerb::Watch))),
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
            matches!(parsed, Ok(AmonRequest::Verb(v)) if v == *verb),
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
fn a_planned_verb_states_what_will_deliver_it() {
    // A verb that is recognised but not built says so, and says where the work is tracked.
    // "not implemented" alone leaves a reader unable to tell abandoned from forthcoming.
    let planned: Vec<_> = AmonVerb::all()
        .iter()
        .filter(|v| matches!(v.state(), VerbState::Planned { .. }))
        .collect();

    assert!(
        !planned.is_empty(),
        "this seam is meaningless once every verb is available; delete it then"
    );

    for verb in planned {
        match verb.state() {
            VerbState::Planned { tracked_as } => assert!(
                !tracked_as.is_empty(),
                "planned verb `{}` should name the work that delivers it",
                verb.name()
            ),
            VerbState::Available => unreachable!(),
        }
    }
}

// --- The binaries themselves ---
//
// These spawn the built binaries, which is the only way to assert on an exit code. Kept to
// the few cases where the exit code *is* the behaviour under test — this repo pays the exec
// tax it measures, and a spawn per assertion would be us doing the thing we complain about.

fn run(binary: &str, args: &[&str]) -> (bool, String, String) {
    let output = Command::new(binary)
        .args(args)
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
fn a_planned_verb_exits_non_zero_rather_than_succeeding_at_nothing() {
    // The load-bearing case. A LaunchAgent that runs `amon watch` and sees zero will report
    // a healthy monitor for as long as the verb remains unbuilt.
    for verb in AmonVerb::all() {
        if !matches!(verb.state(), VerbState::Planned { .. }) {
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
    let (success, stdout, stderr) = run(env!("CARGO_BIN_EXE_agtop"), &[]);

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
