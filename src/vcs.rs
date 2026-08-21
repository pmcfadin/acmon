//! Version control status of agent workspaces.
//!
//! Pure. Holds no clock, runs no subprocess, touches no filesystem. The types and
//! classification logic live here; [`World`](crate::World) and
//! [`RealWorld`](crate::RealWorld) own the I/O.

/// The four workspace states.
///
/// A deliberately separate vocabulary from session liveness: a dirty workspace whose
/// session is waiting is different from a clean one, and an unknown state is different
/// from both. The liveness module decides whether a session is doing work; this module
/// says whether losing the workspace would lose uncommitted changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkspaceState {
    /// No uncommitted work. The workspace can be deleted without losing anything.
    Clean,
    /// Uncommitted work exists and a live session is working here. The session's process
    /// is driving the workspace, so the risk is covered by session monitoring.
    DirtyDriven,
    /// Uncommitted work exists and no live session is working here. The workspace holds
    /// changes that would be lost if it were deleted, and nothing is actively using them.
    /// **This is the at-risk state ticket #7 exists to detect.**
    DirtyStranded,
    /// Version control could not determine the state. Carries the reason, which must be
    /// true rather than merely plausible.
    Unknown(Unreadable),
}

/// Why version control could not say whether a workspace holds uncommitted work.
///
/// Each variant must be TRUE when reported. An "unreadable" that is merely plausible
/// reads as a finding rather than as ignorance, which is worse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unreadable {
    /// No repository at this path, so "uncommitted" has no meaning here. An answer,
    /// not a failure.
    ///
    /// **This is an answer, not a read failure.** There is no version control to query,
    /// so the state is not unknown — it is known to be irrelevant. Treated as safe
    /// rather than at-risk because there is nothing to lose.
    NotVersionControlled,
    /// The path no longer exists.
    ///
    /// **This is an answer, not a read failure.** The path is gone, so there is nothing
    /// left to read. Treated as safe because the workspace itself is already lost —
    /// reporting it as at-risk would be noise, since there is nothing left to protect.
    PathGone,
    /// The version-control query ran and failed, or could not be run at all. Carries
    /// what it said.
    ///
    /// **This is a read FAILURE.** Version control exists but refused to answer, or
    /// produced an error. The workspace might hold uncommitted work and we cannot tell,
    /// so this is treated as at-risk — the precautionary principle.
    QueryFailed(String),
    /// The query did not answer inside the budget.
    ///
    /// **This is a read FAILURE.** Version control exists but did not respond in time.
    /// The workspace might hold uncommitted work and we cannot tell, so this is treated
    /// as at-risk.
    TimedOut,
    /// The slow tier has not read this workspace yet, so nothing has been asked of version
    /// control at all.
    ///
    /// **This is a statement about the monitor, not about the workspace.** The slow tier reads
    /// a bounded slice of workspaces per pass (#27) — a full git sweep costs seconds of CPU and
    /// the whole loop is budgeted as a duty cycle — so a workspace that has just been
    /// discovered waits its turn, stalest first. Until then its state is genuinely unknown.
    ///
    /// Deliberately **not at-risk**, and this is the one arm where that needs arguing.
    /// [`Unreadable::QueryFailed`] is precautionary because git *was* asked and would not
    /// answer, which is evidence. Here git has not been asked, so treating it as at-risk would
    /// alert about every workspace on the machine every time the monitor started, and an alerting
    /// path that cries wolf on startup is one a reader learns to ignore. It is not silent either:
    /// the slow tier publishes how many workspaces are still pending its first read, so "not
    /// looked at yet" is a visible count rather than an absence.
    NotYetRead,
}

/// What version control reported about one workspace. Facts only, no verdict.
///
/// The verdict — clean, dirty-driven, dirty-stranded, or unknown — is produced by
/// [`classify`], which combines these facts with the session's liveness. This struct
/// carries only what version control said, so the I/O and the logic are separate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VcsFacts {
    /// The repository root. This is the workspace's canonical identity: several observed
    /// working directories inside one repository are ONE workspace, not several.
    ///
    /// Multiple processes working in subdirectories of the same repository all report
    /// the same root here, which is what makes them one workspace. A process that chdir'd
    /// into a subdirectory to run a build is still working in the same repository, and
    /// its work still protects the workspace from being reported as stranded.
    pub root: String,
    /// How many entries version control reported as differing from the last commit —
    /// modified, staged, or untracked. Untracked counts: the workspace whose loss
    /// motivated this project held files git had never seen.
    ///
    /// This is a count of **entries**, not a boolean "is dirty". A workspace with 47
    /// uncommitted files is materially different from one with 1, and counting them costs
    /// nothing extra — `git status --porcelain` reports one line per entry, so the count
    /// is the number of non-blank lines.
    pub uncommitted_entries: usize,
    /// Whether this is a linked worktree rather than a repository's primary one. An
    /// ATTRIBUTE of a workspace, recorded because it changes where the real .git lives.
    /// It is never a filter on discovery.
    ///
    /// A linked worktree's `.git` is a **file** pointing at the real location, not a
    /// directory. This distinction is observable with a single `stat` and falls out of
    /// the same walk that finds the repository root, so it costs nothing extra to record.
    pub linked_worktree: bool,
}

/// Classify a workspace's state from version control facts and session liveness.
///
/// The four states are:
///
/// - **Clean**: no uncommitted work exists. Safe.
/// - **DirtyDriven**: uncommitted work exists and a live session is working here. The
///   session's process is driving the workspace, so monitoring the session is sufficient.
/// - **DirtyStranded**: uncommitted work exists and no live session is working here. This
///   is the at-risk state #7 exists to detect — changes that would be lost if the
///   workspace were deleted, with nothing actively using them.
/// - **Unknown**: version control could not determine the state. Whether this is at-risk
///   depends on the reason — see [`WorkspaceState::at_risk`].
///
/// # Arguments
///
/// - `facts`: What version control said, or why it could not answer.
/// - `session_driving`: Whether a live session's process is working in this workspace.
///   This comes from session liveness; it is not derivable from VCS facts alone.
pub fn classify(facts: &Result<VcsFacts, Unreadable>, session_driving: bool) -> WorkspaceState {
    match facts {
        Err(reason) => WorkspaceState::Unknown(reason.clone()),
        Ok(vcs) if vcs.uncommitted_entries == 0 => WorkspaceState::Clean,
        Ok(_) if session_driving => WorkspaceState::DirtyDriven,
        Ok(_) => WorkspaceState::DirtyStranded,
    }
}

impl WorkspaceState {
    /// Whether this workspace state represents an at-risk condition.
    ///
    /// True for:
    ///
    /// - **DirtyStranded**: uncommitted work exists and no live session is driving it.
    ///   This is the primary at-risk state.
    /// - **Unknown(QueryFailed)**: version control failed to answer. The workspace might
    ///   hold uncommitted work and we cannot tell, so the precautionary principle applies.
    /// - **Unknown(TimedOut)**: version control did not respond in time. Same reasoning
    ///   as QueryFailed — we cannot tell, so we assume risk.
    ///
    /// False for:
    ///
    /// - **Clean**: no uncommitted work.
    /// - **DirtyDriven**: uncommitted work exists but a live session is driving it, so
    ///   session monitoring is sufficient.
    /// - **Unknown(NotVersionControlled)**: there is no version control, so there is
    ///   nothing to lose. This is an ANSWER, not a failure — the workspace is not at risk
    ///   because it never held versioned changes in the first place.
    /// - **Unknown(PathGone)**: the workspace no longer exists. This is an ANSWER, not a
    ///   failure — the workspace is already lost, so reporting it as at-risk would be
    ///   noise. There is nothing left to protect.
    ///
    /// The subtlety: `NotVersionControlled` and `PathGone` are both `Unknown`, but they
    /// are answers rather than read failures. They say "there is nothing here" and "the
    /// path is gone", which are states of the world rather than states of our knowledge.
    /// `QueryFailed` and `TimedOut` are genuine unknowns — version control exists but
    /// would not tell us — and those ARE at-risk.
    pub fn at_risk(&self) -> bool {
        match self {
            WorkspaceState::DirtyStranded => true,
            WorkspaceState::Unknown(Unreadable::QueryFailed(_)) => true,
            WorkspaceState::Unknown(Unreadable::TimedOut) => true,
            WorkspaceState::Clean
            | WorkspaceState::DirtyDriven
            | WorkspaceState::Unknown(Unreadable::NotVersionControlled)
            | WorkspaceState::Unknown(Unreadable::PathGone)
            | WorkspaceState::Unknown(Unreadable::NotYetRead) => false,
        }
    }
}

impl std::fmt::Display for WorkspaceState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WorkspaceState::Clean => write!(f, "CLEAN"),
            WorkspaceState::DirtyDriven => write!(f, "DIRTY-DRIVEN"),
            WorkspaceState::DirtyStranded => write!(f, "DIRTY-STRANDED"),
            WorkspaceState::Unknown(_) => write!(f, "UNKNOWN"),
        }
    }
}

impl std::fmt::Display for Unreadable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Unreadable::NotVersionControlled => write!(f, "no repository"),
            Unreadable::PathGone => write!(f, "path gone"),
            Unreadable::QueryFailed(_) => write!(f, "query failed"),
            Unreadable::TimedOut => write!(f, "timed out"),
            Unreadable::NotYetRead => write!(f, "not read yet"),
        }
    }
}
