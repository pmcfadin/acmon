//! Which directory a session is working in, and how that maps to its transcript.
//!
//! Claude Code stores a session's transcript under `~/.claude/projects/<namespace>/`,
//! where the namespace is the absolute working directory with certain characters
//! replaced by hyphens. Getting that replacement wrong is the source of two measured
//! defects in the tool this project replaces — see
//! `docs/observability-mechanics.md` §4.3. Both failed the same way: the namespace was
//! not found, and the session was reported as absent rather than as unattributed.
//!
//! Because the namespace mapping is not invertible (three distinct characters collapse
//! onto one), the reverse direction is done as a verified search over directories that
//! actually exist: descend the filesystem one level at a time, keeping only children
//! whose forward mapping matches the next span of the namespace. On the target machine,
//! of 109 recorded namespaces, 32 resolved to exactly one existing directory, 0 were
//! ambiguous, and 77 named directories that no longer exist, with the whole sweep costing
//! 42 ms. That measured result justifies the approach and its cost.

use crate::world::PathUnavailable;

/// The characters that collapse to `-` in a transcript namespace.
///
/// The underscore is the one that is easy to miss. On the machine behind the mechanics
/// document, **0 of 113 recorded namespaces contained an underscore**, so a mapping that
/// preserves them matches nothing at all — for every workspace, silently.
const COLLAPSED: [char; 3] = ['/', '.', '_'];

/// Map a workspace path to the transcript namespace it would be recorded under.
///
/// **Forward only.** Three distinct characters collapse onto one, so the result does not
/// determine the path it came from. There is deliberately no inverse of this function,
/// and there cannot be a correct one: a namespace containing `-` could have come from a
/// separator, a dot, an underscore, or a literal hyphen.
pub fn namespace_for(path: &str) -> String {
    path.chars()
        .map(|c| if COLLAPSED.contains(&c) { '-' } else { c })
        .collect()
}

/// Find which of the recorded namespaces a workspace path belongs to.
///
/// Compared case-insensitively, and the *recorded* spelling is returned rather than the
/// mapped one. APFS is case-insensitive but case-preserving, so a namespace can be
/// recorded as `WorkforceOS` while the live process reports a lowercase cwd; the
/// recorded spelling is the one that names a real directory.
///
/// Matching is on the whole namespace. A path that maps to a prefix of a recorded
/// namespace is not a match — attributing a session to a neighbouring directory is
/// worse than admitting the workspace is unrecognised.
pub fn recorded_namespace(path: &str, recorded: &[String]) -> Option<String> {
    let mapped = namespace_for(path);
    recorded
        .iter()
        .find(|candidate| candidate.eq_ignore_ascii_case(&mapped))
        .cloned()
}

/// Where a session is working, and whether that workspace has a transcript recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workspace {
    /// The directory the session is working in, exactly as the kernel reports it.
    pub path: String,
    /// The recorded transcript namespace for this workspace, or why none was matched.
    pub namespace: Result<String, NamespaceUnmatched>,
}

/// Why a workspace has no recorded transcript namespace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NamespaceUnmatched {
    /// The workspace mapped cleanly, but nothing by that name is recorded. Carries the
    /// mapped form so the absence can be checked by hand rather than merely believed.
    NotRecorded { mapped: String },
    /// The recorded namespaces could not be listed, so nothing can be said either way.
    /// This is not the same as a workspace having no transcript.
    ListingFailed(String),
    /// This CLI is not one we know how to find transcripts for. Carries the cli id that
    /// was unrecognised, so it can be checked rather than silently falling through to a
    /// store that would attribute the session wrongly.
    UnknownCli(String),
}

impl std::fmt::Display for NamespaceUnmatched {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NamespaceUnmatched::NotRecorded { mapped } => {
                write!(f, "no transcript namespace {mapped} is recorded")
            }
            NamespaceUnmatched::ListingFailed(why) => {
                write!(f, "recorded namespaces could not be listed: {why}")
            }
            NamespaceUnmatched::UnknownCli(cli) => {
                write!(f, "no transcript store is known for CLI {cli}")
            }
        }
    }
}

/// What a recorded transcript namespace turned out to name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NamespaceResolution {
    /// Exactly one existing directory maps to this namespace. Proven by listing real
    /// directory entries, never by inverting the mapping.
    Resolved(String),
    /// More than one existing directory maps to it. Carries all of them, because naming
    /// one would be a guess and reporting none would hide that the workspace exists.
    Ambiguous(Vec<String>),
    /// Every step was searched and no existing directory maps to it. The workspace was
    /// deleted, moved, or renamed. This is an ANSWER.
    NoLongerExists,
    /// The search hit its bound before it finished, so absence was never established.
    /// Distinct from `NoLongerExists` on purpose: reporting "gone" when the search merely
    /// gave up is the calm plausible wrong answer this project exists to remove.
    SearchExhausted,
}

impl std::fmt::Display for NamespaceResolution {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NamespaceResolution::Resolved(path) => write!(f, "{path}"),
            NamespaceResolution::Ambiguous(candidates) => {
                write!(f, "ambiguous: {} candidates", candidates.len())
            }
            NamespaceResolution::NoLongerExists => write!(f, "no longer exists"),
            NamespaceResolution::SearchExhausted => write!(f, "search incomplete"),
        }
    }
}

/// The most directory listings one namespace's search may make before giving up.
pub const SEARCH_BUDGET: usize = 4096;

/// Consume `mapped` from the front of `remaining`, ASCII-case-insensitively.
///
/// Returns what is left, or `None` when `remaining` does not begin with `mapped`. Written
/// with character iterators rather than byte slices so that a path containing anything
/// outside ASCII neither panics nor silently fails to match — see the call site.
fn strip_mapped_prefix<'a>(remaining: &'a str, mapped: &str) -> Option<&'a str> {
    let mut rest = remaining;
    for expected in mapped.chars() {
        let mut characters = rest.chars();
        let actual = characters.next()?;
        if !actual.eq_ignore_ascii_case(&expected) {
            return None;
        }
        rest = characters.as_str();
    }
    Some(rest)
}

/// Find which existing directory a recorded namespace names.
///
/// `list` is asked for the entries of a directory and returns only its **sub-directory
/// names**, or `None` if the directory could not be read. Injected rather than called
/// directly so that this function is pure: the search logic is where the subtlety is, and
/// it must be testable without building directory trees on disk.
///
/// The search descends the filesystem one level at a time. At each step, it asks for the
/// children of the current directory and keeps only those whose forward mapping matches
/// the next span of the namespace. Because APFS is case-insensitive but case-preserving,
/// matching is done case-insensitively, but results are built from the real entry names
/// the filesystem returned. This fixes one of the two measured defects documented in
/// `docs/observability-mechanics.md` §4.3, where a namespace recorded as `WorkforceOS`
/// failed to match a live directory spelled `workforceos`.
pub fn resolve_namespace(
    namespace: &str,
    list: &dyn Fn(&str) -> Option<Vec<String>>,
) -> NamespaceResolution {
    let mut call_count = 0;
    let mut results = Vec::new();

    // Helper to recursively search for matching paths
    fn search(
        current: &str,
        remaining: &str,
        list: &dyn Fn(&str) -> Option<Vec<String>>,
        call_count: &mut usize,
        results: &mut Vec<String>,
    ) -> bool {
        // Check budget before proceeding
        if *call_count >= SEARCH_BUDGET {
            return false; // Budget exhausted
        }

        // If remaining is empty, current is a result
        if remaining.is_empty() {
            results.push(current.to_string());
            return true;
        }

        // Remaining must start with '-'
        if !remaining.starts_with('-') {
            return true; // This branch dies, but search can continue elsewhere
        }

        // Strip the leading '-'
        let remaining = &remaining[1..];

        // If remaining is now empty, the namespace was just "-", so the result is "/"
        if remaining.is_empty() {
            results.push("/".to_string());
            return true;
        }

        // List the children of current directory
        let dir_to_list = if current.is_empty() { "/" } else { current };
        *call_count += 1;

        let Some(children) = list(dir_to_list) else {
            return true; // Unreadable directory kills only this branch
        };

        let mut budget_ok = true;

        // For each child, check if it matches
        for child in children {
            let mapped = namespace_for(&child);

            // Consume the child's mapped name from the front of what remains, comparing
            // character by character.
            //
            // Deliberately NOT `remaining[..mapped.len()]`. That is a byte offset, and a
            // byte offset taken from one string and applied to another lands inside a
            // multi-byte character as soon as any path contains something outside ASCII —
            // which panics. Mixing a byte length with `chars().nth()` fails the other way,
            // matching nothing and silently reporting the workspace as gone. Both were
            // present here and both are reachable on an ordinary machine.
            let Some(rest) = strip_mapped_prefix(remaining, &mapped) else {
                continue;
            };

            let child_path = if current.is_empty() {
                format!("/{child}")
            } else {
                format!("{current}/{child}")
            };

            if rest.is_empty() {
                // The child's name accounts for all of what remained: this is a result.
                results.push(child_path);
            } else if rest.starts_with('-') {
                // More namespace to account for, and it resumes at a separator.
                //
                // Do NOT split the namespace on `-` instead of doing this. Directory names
                // contain hyphens of their own, and `agentic_coding_monitor` maps to
                // `agentic-coding-monitor`, which is one directory wearing three apparent
                // segments.
                if !search(&child_path, rest, list, call_count, results) {
                    budget_ok = false;
                    break;
                }
            }
            // Anything else means this child's name is a prefix of a longer directory
            // name in the namespace, e.g. `work` against `workforceos`. Not a match.
        }

        budget_ok
    }

    let budget_ok = search("", namespace, list, &mut call_count, &mut results);

    // If budget was exhausted, return SearchExhausted even if results were found
    if !budget_ok {
        return NamespaceResolution::SearchExhausted;
    }

    // Deduplicate results
    results.sort();
    results.dedup();

    match results.len() {
        0 => NamespaceResolution::NoLongerExists,
        1 => NamespaceResolution::Resolved(results.into_iter().next().unwrap()),
        _ => NamespaceResolution::Ambiguous(results),
    }
}

/// Why a session's workspace could not be determined at all.
///
/// Rendered in the workspace column, so it must be true and it must be short.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkspaceUnknown {
    /// The process exited before its working directory could be read.
    ProcessExited,
    /// The process is alive but its working directory is not readable by this user.
    PermissionDenied,
    /// The namespace names more than one existing directory, so naming one would be a guess.
    Ambiguous { candidates: usize },
    /// No existing directory maps to the namespace: the workspace is gone.
    WorkspaceGone,
    /// The search for the workspace was abandoned before absence could be established.
    SearchIncomplete,
}

impl From<&PathUnavailable> for WorkspaceUnknown {
    fn from(reason: &PathUnavailable) -> Self {
        match reason {
            PathUnavailable::ProcessExited => WorkspaceUnknown::ProcessExited,
            PathUnavailable::PermissionDenied => WorkspaceUnknown::PermissionDenied,
        }
    }
}

impl std::fmt::Display for WorkspaceUnknown {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WorkspaceUnknown::ProcessExited => write!(f, "unknown: exited"),
            WorkspaceUnknown::PermissionDenied => write!(f, "unknown: no-perm"),
            WorkspaceUnknown::Ambiguous { candidates } => {
                write!(f, "ambiguous: {candidates} candidates")
            }
            WorkspaceUnknown::WorkspaceGone => write!(f, "gone"),
            WorkspaceUnknown::SearchIncomplete => write!(f, "search incomplete"),
        }
    }
}
