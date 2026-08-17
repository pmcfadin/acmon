//! Seam 5 — mapping a workspace path to the transcript namespace it is recorded under.
//!
//! Pure, like seam 4, and for the same reason: the two defects this replaces both
//! produced a calm "no session found" rather than an error. Every fixture below was
//! observed in `~/.claude/projects` on a real machine and is recorded in
//! `docs/observability-mechanics.md` §4.3.

use acmon::workspace::{namespace_for, recorded_namespace};

/// Namespaces that genuinely exist on the machine these fixtures came from.
fn recorded() -> Vec<String> {
    [
        "-Users-pmcfadin-projects-agentic-coding-monitor",
        "-Users-pmcfadin-projects-WorkforceOS",
        "-Users-pmcfadin-projects-workforceos--claude-worktrees-obs-increment-3",
        "-Users-pmcfadin-projects-workforceos-mvp",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

#[test]
fn separators_dots_and_underscores_all_become_hyphens() {
    // The underscore is the rule that is easy to miss, and missing it is one of the two
    // measured defects: 0 of 113 namespaces on the machine contain an underscore, so a
    // mapping that keeps them matches nothing, ever.
    assert_eq!(
        namespace_for("/Users/pmcfadin/projects/agentic_coding_monitor"),
        "-Users-pmcfadin-projects-agentic-coding-monitor"
    );
}

#[test]
fn a_dot_becomes_a_hyphen_so_a_hidden_directory_doubles_one() {
    // Observed: a worktree under `.claude` produces two hyphens in a row, from the
    // slash and then the dot. A mapping that handled only separators would produce one.
    assert_eq!(
        namespace_for("/Users/pmcfadin/projects/workforceos/.claude/worktrees/obs-increment-3"),
        "-Users-pmcfadin-projects-workforceos--claude-worktrees-obs-increment-3"
    );
}

#[test]
fn a_workspace_path_containing_underscores_attributes_to_its_recorded_namespace() {
    // Regression. This previously resolved to nothing, and the tool reported no session
    // for a workspace that had one — a calm, plausible, wrong answer.
    let matched = recorded_namespace(
        "/Users/pmcfadin/projects/agentic_coding_monitor",
        &recorded(),
    );

    assert_eq!(
        matched.as_deref(),
        Some("-Users-pmcfadin-projects-agentic-coding-monitor"),
        "a workspace with underscores must attribute to the hyphenated namespace"
    );
}

#[test]
fn a_namespace_recorded_with_different_capitalisation_still_attributes() {
    // Regression. APFS is case-insensitive but case-preserving, so the directory was
    // recorded as WorkforceOS while the live process reports a lowercase cwd. Testing
    // the constructed path with an existence check succeeds while a string comparison
    // against the listing fails, which is why the comparison must be case-insensitive.
    let matched = recorded_namespace("/Users/pmcfadin/projects/workforceos", &recorded());

    assert_eq!(
        matched.as_deref(),
        Some("-Users-pmcfadin-projects-WorkforceOS"),
        "attribution must be case-insensitive, and must return the recorded spelling"
    );
}

#[test]
fn the_mapping_is_not_invertible_which_is_why_it_is_only_ever_done_forward() {
    // Three characters collapse onto one, so a namespace does not determine a path.
    // This is the evidence for that rule rather than a restatement of it: no reverse
    // function exists in this module, and none can.
    let with_underscore = namespace_for("/Users/pmcfadin/projects/my_tool");
    let with_hyphen = namespace_for("/Users/pmcfadin/projects/my-tool");
    let with_dot = namespace_for("/Users/pmcfadin/projects/my.tool");

    assert_eq!(with_underscore, with_hyphen);
    assert_eq!(with_hyphen, with_dot);
}

#[test]
fn an_unrecorded_workspace_matches_nothing_rather_than_the_closest_thing() {
    // A near miss must not be treated as a hit. `workforceos-mvp` is recorded and
    // `workforceos-mvp2` is not; answering with the former would attribute a session to
    // the wrong directory, which is worse than admitting ignorance.
    let matched = recorded_namespace("/Users/pmcfadin/projects/workforceos-mvp2", &recorded());

    assert_eq!(matched, None);
}

#[test]
fn matching_needs_the_whole_namespace_not_a_prefix_of_one() {
    // `/Users/pmcfadin/projects` maps to a prefix of several recorded namespaces. It is
    // not itself one of them, and must not borrow their identity.
    let matched = recorded_namespace("/Users/pmcfadin/projects", &recorded());

    assert_eq!(matched, None);
}
