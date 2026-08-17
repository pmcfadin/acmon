//! Which directory a session is working in, and how that maps to its transcript.
//!
//! Claude Code stores a session's transcript under `~/.claude/projects/<namespace>/`,
//! where the namespace is the absolute working directory with certain characters
//! replaced by hyphens. Getting that replacement wrong is the source of two measured
//! defects in the tool this project replaces — see
//! `docs/observability-mechanics.md` §4.3. Both failed the same way: the namespace was
//! not found, and the session was reported as absent rather than as unattributed.

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
        }
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
        }
    }
}
