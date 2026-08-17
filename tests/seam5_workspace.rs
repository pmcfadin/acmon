//! Seam 5 — mapping a workspace path to the transcript namespace it is recorded under.
//!
//! Pure, like seam 4, and for the same reason: the two defects this replaces both
//! produced a calm "no session found" rather than an error. Every fixture below was
//! observed in `~/.claude/projects` on a real machine and is recorded in
//! `docs/observability-mechanics.md` §4.3.

use acmon::workspace::{namespace_for, recorded_namespace, resolve_namespace, NamespaceResolution};
use std::collections::HashMap;

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

// ===== Namespace resolution tests =====
//
// Build an in-memory directory tree as a map from path to child names. The listing
// function is injected so the search logic can be tested without building real directory
// trees on disk.

fn make_lister(tree: HashMap<String, Vec<String>>) -> impl Fn(&str) -> Option<Vec<String>> {
    move |path: &str| tree.get(path).cloned()
}

#[test]
fn a_straightforward_path_resolves_uniquely() {
    let tree = HashMap::from([
        ("/".to_string(), vec!["Users".to_string()]),
        ("/Users".to_string(), vec!["pmcfadin".to_string()]),
        ("/Users/pmcfadin".to_string(), vec!["projects".to_string()]),
        (
            "/Users/pmcfadin/projects".to_string(),
            vec!["my-tool".to_string()],
        ),
    ]);
    let list = make_lister(tree);

    let result = resolve_namespace("-Users-pmcfadin-projects-my-tool", &list);

    assert_eq!(
        result,
        NamespaceResolution::Resolved("/Users/pmcfadin/projects/my-tool".to_string()),
        "a namespace that maps to exactly one existing directory must resolve to that path"
    );
}

#[test]
fn a_directory_containing_underscores_resolves_from_its_hyphenated_namespace() {
    // Regression: the underscore is the character that is easy to miss, and missing it
    // is one of the two measured defects documented in §4.3.
    let tree = HashMap::from([
        ("/".to_string(), vec!["Users".to_string()]),
        ("/Users".to_string(), vec!["x".to_string()]),
        (
            "/Users/x".to_string(),
            vec!["agentic_coding_monitor".to_string()],
        ),
    ]);
    let list = make_lister(tree);

    let result = resolve_namespace("-Users-x-agentic-coding-monitor", &list);

    assert_eq!(
        result,
        NamespaceResolution::Resolved("/Users/x/agentic_coding_monitor".to_string()),
        "an underscore in a directory name must be handled by the forward mapping"
    );
}

#[test]
fn a_case_mismatch_resolves_and_returns_the_filesystems_spelling() {
    // Regression: APFS is case-insensitive but case-preserving, so a namespace recorded
    // as `WorkforceOS` must still match a directory listed as `workforceos`, and the
    // returned path must carry the filesystem's spelling, not the namespace's.
    let tree = HashMap::from([
        ("/".to_string(), vec!["Users".to_string()]),
        ("/Users".to_string(), vec!["pmcfadin".to_string()]),
        ("/Users/pmcfadin".to_string(), vec!["projects".to_string()]),
        (
            "/Users/pmcfadin/projects".to_string(),
            vec!["workforceos".to_string()],
        ),
    ]);
    let list = make_lister(tree);

    let result = resolve_namespace("-Users-pmcfadin-projects-WorkforceOS", &list);

    assert_eq!(
        result,
        NamespaceResolution::Resolved("/Users/pmcfadin/projects/workforceos".to_string()),
        "matching must be case-insensitive, and the result must carry the filesystem's spelling"
    );
}

#[test]
fn two_directories_mapping_to_the_same_namespace_produces_ambiguous_not_a_guess() {
    // `a.b` and `a_b` both map to `a-b`. The search must find both and report ambiguity,
    // not pick one. Picking one would report a guessed path as known, which is worse than
    // admitting it is unknown.
    let tree = HashMap::from([
        ("/".to_string(), vec!["Users".to_string()]),
        ("/Users".to_string(), vec!["x".to_string()]),
        (
            "/Users/x".to_string(),
            vec!["a.b".to_string(), "a_b".to_string()],
        ),
    ]);
    let list = make_lister(tree);

    let result = resolve_namespace("-Users-x-a-b", &list);

    match result {
        NamespaceResolution::Ambiguous(mut candidates) => {
            candidates.sort();
            assert_eq!(
                candidates,
                vec!["/Users/x/a.b", "/Users/x/a_b"],
                "both directories must be reported, proving the code did not pick one"
            );
        }
        _ => panic!("expected Ambiguous, got {result:?}"),
    }
}

#[test]
fn a_namespace_whose_directory_does_not_exist_resolves_to_no_longer_exists() {
    let tree = HashMap::from([
        ("/".to_string(), vec!["Users".to_string()]),
        ("/Users".to_string(), vec!["pmcfadin".to_string()]),
        ("/Users/pmcfadin".to_string(), vec!["projects".to_string()]),
        // No "deleted-workspace" under projects
    ]);
    let list = make_lister(tree);

    let result = resolve_namespace("-Users-pmcfadin-projects-deleted-workspace", &list);

    assert_eq!(
        result,
        NamespaceResolution::NoLongerExists,
        "a namespace naming a non-existent directory is an answer, not an error"
    );
}

#[test]
fn an_unreadable_intermediate_directory_kills_only_that_branch() {
    // `/Users/x/a/tool` is unreadable, but `/Users/x/b/tool` is readable and matches.
    let tree = HashMap::from([
        ("/".to_string(), vec!["Users".to_string()]),
        ("/Users".to_string(), vec!["x".to_string()]),
        (
            "/Users/x".to_string(),
            vec!["a".to_string(), "b".to_string()],
        ),
        // "/Users/x/a" is not in the map, so listing it returns None
        ("/Users/x/b".to_string(), vec!["tool".to_string()]),
    ]);
    let list = make_lister(tree);

    let result = resolve_namespace("-Users-x-b-tool", &list);

    assert_eq!(
        result,
        NamespaceResolution::Resolved("/Users/x/b/tool".to_string()),
        "an unreadable directory must kill only its branch, not the whole search"
    );
}

/// The budget must stop an unbounded search, and must NOT report the stop as an absence.
///
/// The earlier version of this test asserted that a two-directory tree resolved, with a
/// comment saying the budget was "verified by code inspection". That is a test that cannot
/// fail: it passes whether the budget exists or not.
///
/// A tree that genuinely exhausts the budget is easy once you stop trying to build it as a
/// map. The lister below answers *every* path with the same two children, `a` and `a-a`,
/// which is an infinitely deep tree. Both children match a namespace of repeated `a`s as a
/// prefix, but they consume different amounts of it — one character versus three — so the
/// number of branches explored follows the Fibonacci sequence in the namespace's length.
/// Twenty repetitions is about 10,000 branches against a budget of 4096, and the deepest
/// branch is only twenty levels, so it overruns the budget without ever risking the stack.
#[test]
fn exhausting_the_search_budget_reports_exhaustion_and_not_absence() {
    use acmon::workspace::SEARCH_BUDGET;
    use std::cell::Cell;

    let listings = Cell::new(0usize);
    let endless = |_path: &str| {
        listings.set(listings.get() + 1);
        Some(vec!["a".to_string(), "a-a".to_string()])
    };

    // First establish that this lister answers at all. Without this, an overrun and a
    // malformed namespace would be indistinguishable, and the assertion below would prove
    // nothing about the budget.
    assert_eq!(
        resolve_namespace("-a", &endless),
        NamespaceResolution::Resolved("/a".to_string()),
        "the endless lister must resolve a short namespace, or the overrun below could be \
         caused by the namespace rather than by the tree"
    );

    listings.set(0);
    let overrun = resolve_namespace(&"-a".repeat(20), &endless);

    assert_eq!(
        overrun,
        NamespaceResolution::SearchExhausted,
        "a search that ran out of budget must say so; reporting NoLongerExists would claim \
         the workspace is gone when the search simply gave up, and reporting Resolved would \
         name one candidate while others went unexplored"
    );
    assert!(
        listings.get() >= SEARCH_BUDGET,
        "the search stopped before reaching its budget ({} listings against a budget of \
         {SEARCH_BUDGET}), so this test did not exercise the budget at all",
        listings.get()
    );
}

#[test]
fn a_namespace_not_starting_with_hyphen_resolves_to_nothing() {
    // A malformed namespace (not starting with `-`) should resolve to nothing rather than
    // panicking.
    let tree = HashMap::from([("/".to_string(), vec!["Users".to_string()])]);
    let list = make_lister(tree);

    let result = resolve_namespace("Users-x-tool", &list);

    assert_eq!(
        result,
        NamespaceResolution::NoLongerExists,
        "a malformed namespace must resolve to nothing, not panic"
    );
}

#[test]
fn the_namespace_for_root_is_a_single_hyphen_and_resolves_to_slash() {
    // The namespace for `/` is `-`. This must resolve to `/` itself.
    let tree = HashMap::from([("/".to_string(), vec!["Users".to_string()])]);
    let list = make_lister(tree);

    let result = resolve_namespace("-", &list);

    assert_eq!(
        result,
        NamespaceResolution::Resolved("/".to_string()),
        "the namespace `-` must resolve to `/`"
    );
}

/// A non-ASCII directory name must not break the search.
///
/// The search compares a child's mapped name against the front of what remains of the
/// namespace. Doing that with byte indices is wrong twice over: slicing at a byte offset
/// that lands inside a multi-byte character panics, and mixing a byte length with a
/// character index silently fails to match. Both are reachable on a machine whose paths
/// contain anything outside ASCII, which is most machines.
#[test]
fn a_non_ascii_directory_name_resolves_and_does_not_panic() {
    let tree = HashMap::from([
        // `ab` is the trap: it is compared first and its mapped length in BYTES lands in
        // the middle of the `é` in the sibling's name.
        ("/".to_string(), vec!["ab".to_string(), "aé".to_string()]),
        ("/aé".to_string(), vec!["x".to_string()]),
    ]);
    let list = make_lister(tree);

    assert_eq!(
        resolve_namespace("-aé-x", &list),
        NamespaceResolution::Resolved("/aé/x".to_string())
    );
}
