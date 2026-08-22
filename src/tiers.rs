//! What each tier observes, how one pass reuses the others' facts, and what gets published.
//!
//! The problem this module solves. A collection is one function — [`collect_as`] — and it must
//! stay one function, because the display and the monitor have to see the same machine (PRD
//! decision 32). But its observations do not cost the same: enumerating processes is near-free,
//! sweeping directories is milliseconds, and asking `git` about seventy workspaces is seconds.
//! Running the cheap ones at the pace of the expensive one is what made the whole collection cost
//! ~2.5 s against a one-second budget.
//!
//! So the collection is not split. What is split is **when each observation was taken**. A pass
//! runs the whole assembly with exactly one tier *live* — its observations go to the operating
//! system — while the other two answer from the last facts they gathered, each carrying the
//! instant it was gathered at. A fast pass therefore reasons about live processes and
//! minute-old git facts, and says so, per tier, on disk (F21, F30).
//!
//! Two consequences worth stating plainly:
//!
//! - **A tier's facts are as old as its own last pass, never older and never younger.** That is
//!   the whole contract behind per-tier stamps. [`Observed`] is the only place a fact can be
//!   read from without going to the operating system, and every entry in it is stamped.
//! - **An observation no tier has made yet is reported as missing, with a reason.** Never as an
//!   empty list, a zero, or a plausible default. A workspace the slow tier has not reached is
//!   [`Unreadable::NotYetRead`] and is published in a pending count; a process ledger no fast
//!   pass has read is a stated read failure.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use crate::collect::{collect_as, CollectError, Identity, Role, Session, Snapshot};
use crate::liveness::Thresholds;
use crate::meter::{tier_name, Measured, SelfReport};
use crate::schedule::{stalest_first, Budgets, Completion, Coverage, Tier};
use crate::vcs::{Unreadable, VcsFacts};
use crate::workspace::NamespaceResolution;
use crate::world::{
    ActivityUnavailable, CodexSession, LoadAverage, ProcessSnapshot, Resources,
    ResourcesUnavailable, StateRead, Sweep, World, WorldError,
};

/// A tier's facts, with the instant they were observed at.
#[derive(Debug, Clone)]
pub struct Stamped<T> {
    pub facts: T,
    /// The instant the pass that gathered these facts **started**.
    ///
    /// The start, not the finish. Facts are gathered across a pass, so the oldest of them is as
    /// old as its beginning, and stamping with the finish would describe every fact in the pass
    /// as younger than it is. F30 turns on exactly this: a workspace committed 50 s ago must not
    /// appear at-risk under a 1 s stamp.
    pub observed_at: SystemTime,
    /// The same instant in the loop's own elapsed time, which is monotonic where a
    /// [`SystemTime`] is not.
    pub at: Duration,
    pub took: Duration,
    /// How many passes of this tier have completed, this one included.
    pub sequence: u64,
    /// What the machine as a whole was carrying while these facts were gathered.
    pub load: Result<LoadAverage, String>,
}

/// What the fast tier reads: near-free signals, no subprocess where the platform allows it.
///
/// `libproc` for the enumeration and the resource ledger (NF5), plus the **names** of the
/// recorded transcript namespaces, which is one directory listing.
///
/// Measured on the machine this was built on: the process enumeration is 12.7 ms for 1048
/// processes, 20 resource ledgers are 68 µs, and listing 91 namespace names is 0.9 ms.
#[derive(Debug, Clone)]
pub struct FastFacts {
    pub snapshot: Result<ProcessSnapshot, WorldError>,
    /// Read only for the processes a detector matched, keyed by pid.
    pub resources: HashMap<i32, Result<Resources, ResourcesUnavailable>>,
    pub namespaces: Result<Vec<String>, WorldError>,
}

/// What the medium tier reads: the filesystem searches.
///
/// Resolving a recorded namespace onto a directory that still exists, sweeping the neighbourhoods
/// that repositories live in, and reading when each namespace's transcripts last changed. All
/// three are bounded directory walks — no subprocess, but far from free.
///
/// The transcript activity read is here **because it was measured**, not because it looks like a
/// search. It is a directory listing plus a `stat` per transcript, once per namespace: 60.3 ms for
/// 91 namespaces, which was 69% of the whole fast pass while it lived in the fast tier and was
/// single-handedly the reason the collection could not fit its budget. What it costs the accuracy
/// of is a silence measurement against a ten-minute quiet threshold — a reading up to one medium
/// interval old, against a threshold an order of magnitude larger.
#[derive(Debug, Clone, Default)]
pub struct MediumFacts {
    pub resolutions: HashMap<String, NamespaceResolution>,
    /// The sweep, and the roots it was asked about, so a reader can tell a sweep that found
    /// nothing from one that was never asked about anywhere.
    pub sweep: Option<(Vec<String>, Sweep)>,
    pub activity: HashMap<String, Result<SystemTime, ActivityUnavailable>>,
}

/// One workspace's version-control facts, and when `git` was actually asked.
#[derive(Debug, Clone)]
pub struct ReadVcs {
    pub facts: Result<VcsFacts, Unreadable>,
    pub observed_at: SystemTime,
    pub at: Duration,
}

/// What the slow tier reads: `git` and the Codex transcript index.
///
/// Read a **slice** at a time. A full sweep of 34 workspaces costs 2.7 s and 70 cost 5.0 s
/// sequentially; at any interval short enough to be useful that is far outside the 1%-of-a-core
/// budget, so each pass reads the stalest slice it can afford and every workspace carries the
/// instant it was last read.
///
/// The arithmetic that follows from the budget, and it is worth stating rather than discovering:
/// `git status` has a measured median of 59 ms, and a slow tier allowed roughly a third of a
/// second every two minutes gets through about five repositories a pass. On a machine with
/// seventy of them a **full refresh of the at-risk panel therefore takes twenty to thirty
/// minutes**, and after a restart it takes that long to become exhaustive. That is not a
/// concession, it is the price of NF9: the alternative is a monitor that spends several percent
/// of a core asking git the same questions. What makes it safe is that nothing is
/// misrepresented — every row carries the instant it was read, and a workspace that has never
/// been read is [`Unreadable::NotYetRead`], counted in a published pending total and never
/// reported as clean.
#[derive(Debug, Clone, Default)]
pub struct SlowFacts {
    pub codex: Option<Result<Vec<CodexSession>, WorldError>>,
    /// Keyed by lower-cased path, because APFS is case-insensitive but case-preserving and the
    /// same workspace arrives spelled differently from different sources.
    pub vcs: HashMap<String, ReadVcs>,
    /// How many workspaces this pass read, out of how many there were.
    pub coverage: Coverage,
    /// The slice size the next pass should try, having seen what this one cost.
    pub slice: usize,
}

/// How many **repositories** the first slow pass attempts, before anything has been measured.
///
/// Four, because `git status` has a measured median of 59 ms and a maximum of 455 ms, so four is
/// the most that reliably fits a 300 ms budget without having measured this machine yet. The first
/// pass is the one with no measurement to size itself from, so it is also the one most likely to
/// overrun; it grows from here as fast as [`crate::schedule::resized_slice`] allows.
pub const FIRST_SLICE: usize = 4;

/// Everything every tier has observed, each with its own stamp.
#[derive(Debug, Clone, Default)]
pub struct Observed {
    pub fast: Option<Stamped<FastFacts>>,
    pub medium: Option<Stamped<MediumFacts>>,
    pub slow: Option<Stamped<SlowFacts>>,
}

impl Observed {
    /// Whether every tier has completed at least one pass.
    ///
    /// The loop writes nothing durable and announces nothing until this is true, because a
    /// collection assembled before the searches and the git reads have happened describes a
    /// machine whose workspaces are all unknown — and writing that over the memory an earlier
    /// run left would throw away exactly the remembered workspaces that make the safety net
    /// durable.
    ///
    /// It is deliberately **not** conditional on the slow tier having read every workspace. It
    /// does not need to be: a workspace the slow tier has not reached is
    /// [`Unreadable::NotYetRead`], which is not at-risk and therefore never announced, so the
    /// monitor can start alerting on what it does know without crying wolf about what it does
    /// not. The pending count is published either way.
    pub fn every_tier_has_run(&self) -> bool {
        self.fast.is_some() && self.medium.is_some() && self.slow.is_some()
    }

    /// When this tier last observed anything.
    pub fn stamp(&self, tier: Tier) -> Option<SystemTime> {
        match tier {
            Tier::Fast => self.fast.as_ref().map(|s| s.observed_at),
            Tier::Medium => self.medium.as_ref().map(|s| s.observed_at),
            Tier::Slow => self.slow.as_ref().map(|s| s.observed_at),
        }
    }

    /// The slice size the next slow pass should attempt.
    pub fn slice(&self) -> usize {
        self.slow
            .as_ref()
            .map(|slow| slow.facts.slice.max(1))
            .unwrap_or(FIRST_SLICE)
    }
}

/// Everything a pass gathered, before it is stamped and folded into [`Observed`].
#[derive(Debug, Default)]
struct Gathered {
    snapshot: Option<Result<ProcessSnapshot, WorldError>>,
    resources: HashMap<i32, Result<Resources, ResourcesUnavailable>>,
    namespaces: Option<Result<Vec<String>, WorldError>>,
    activity: HashMap<String, Result<SystemTime, ActivityUnavailable>>,
    resolutions: HashMap<String, NamespaceResolution>,
    sweep: Option<(Vec<String>, Sweep)>,
    codex: Option<Result<Vec<CodexSession>, WorldError>>,
    vcs: HashMap<String, ReadVcs>,
    /// How many workspaces the slow tier was asked about, and how many it read this pass.
    asked: usize,
    read: usize,
    never_read: usize,
}

/// A [`World`] that serves two tiers from what they last observed and lets the third do the
/// observing.
///
/// This is the mechanism that makes tiering a property of *when* rather than a fork in *what*:
/// [`collect_as`] runs against this exactly as it runs against the real machine, so the monitor
/// and the display cannot come to disagree about how a fact is derived — only about how old it is.
///
/// Everything that is not a tiered observation passes straight through: the state and dedupe
/// files, the notification channels, the configuration, the output width, and
/// [`World::repository_root`], which is a handful of `stat` calls rather than a tier's worth of
/// work.
pub struct TieredWorld<'a> {
    observed: &'a Observed,
    delegate: &'a (dyn World + Sync),
    live: Tier,
    now: SystemTime,
    at: Duration,
    slice: usize,
    gathered: Mutex<Gathered>,
}

impl<'a> TieredWorld<'a> {
    /// A world for one pass of `live`, over what the other tiers already know.
    pub fn new(
        observed: &'a Observed,
        delegate: &'a (dyn World + Sync),
        live: Tier,
        now: SystemTime,
        at: Duration,
        slice: usize,
    ) -> TieredWorld<'a> {
        TieredWorld {
            observed,
            delegate,
            live,
            now,
            at,
            slice,
            gathered: Mutex::new(Gathered::default()),
        }
    }

    fn is_live(&self, tier: Tier) -> bool {
        self.live == tier
    }

    fn fast(&self) -> Option<&FastFacts> {
        self.observed.fast.as_ref().map(|stamped| &stamped.facts)
    }

    fn medium(&self) -> Option<&MediumFacts> {
        self.observed.medium.as_ref().map(|stamped| &stamped.facts)
    }

    fn slow(&self) -> Option<&SlowFacts> {
        self.observed.slow.as_ref().map(|stamped| &stamped.facts)
    }

    /// Everything this pass observed, ready to be stamped.
    fn into_gathered(self) -> Gathered {
        self.gathered
            .into_inner()
            .expect("no pass holds this lock across a panic")
    }
}

/// Why a tier that has not run yet cannot answer.
///
/// One sentence, shaped for wherever it lands — a workspace's `UNKNOWN` reason, a session's
/// missing ledger, a namespace with no activity. It names the tier, because "unknown" without
/// that is a dead end for a reader wondering whether to wait or to investigate.
fn not_yet(tier: Tier, what: &str) -> String {
    format!(
        "no {} pass has read {what} yet, so this is not known rather than absent",
        tier_name(tier)
    )
}

impl World for TieredWorld<'_> {
    fn process_snapshot(&self) -> Result<ProcessSnapshot, WorldError> {
        if self.is_live(Tier::Fast) {
            let observed = self.delegate.process_snapshot();
            self.gathered
                .lock()
                .expect("uncontended")
                .snapshot
                .get_or_insert_with(|| observed.clone());
            return observed;
        }

        match self.fast().map(|facts| facts.snapshot.clone()) {
            Some(snapshot) => snapshot,
            None => Err(WorldError::ProcessEnumeration(not_yet(
                Tier::Fast,
                "the process table",
            ))),
        }
    }

    fn resources(&self, pid: i32) -> Result<Resources, ResourcesUnavailable> {
        if self.is_live(Tier::Fast) {
            let read = self.delegate.resources(pid);
            self.gathered
                .lock()
                .expect("uncontended")
                .resources
                .insert(pid, read.clone());
            return read;
        }

        match self.fast().and_then(|facts| facts.resources.get(&pid)) {
            Some(read) => read.clone(),
            // Never `ProcessExited`: that is a claim about the process, established by asking.
            // This is a claim about the monitor, which has not asked.
            None => Err(ResourcesUnavailable::AllReadersFailed(not_yet(
                Tier::Fast,
                &format!("pid {pid}'s resource ledger"),
            ))),
        }
    }

    fn recorded_namespaces(&self) -> Result<Vec<String>, WorldError> {
        if self.is_live(Tier::Fast) {
            let listed = self.delegate.recorded_namespaces();
            self.gathered
                .lock()
                .expect("uncontended")
                .namespaces
                .get_or_insert_with(|| listed.clone());
            return listed;
        }

        match self.fast().map(|facts| facts.namespaces.clone()) {
            Some(namespaces) => namespaces,
            None => Err(WorldError::NamespaceListing(not_yet(
                Tier::Fast,
                "the transcript stores",
            ))),
        }
    }

    fn namespace_activity(&self, namespace: &str) -> Result<SystemTime, ActivityUnavailable> {
        if self.is_live(Tier::Medium) {
            let read = self.delegate.namespace_activity(namespace);
            self.gathered
                .lock()
                .expect("uncontended")
                .activity
                .insert(namespace.to_string(), read.clone());
            return read;
        }

        match self
            .medium()
            .and_then(|facts| facts.activity.get(namespace))
        {
            Some(read) => read.clone(),
            None => Err(ActivityUnavailable::Unreadable(not_yet(
                Tier::Medium,
                &format!("namespace {namespace}"),
            ))),
        }
    }

    fn resolve_namespace(&self, namespace: &str) -> NamespaceResolution {
        if self.is_live(Tier::Medium) {
            let resolved = self.delegate.resolve_namespace(namespace);
            self.gathered
                .lock()
                .expect("uncontended")
                .resolutions
                .insert(namespace.to_string(), resolved.clone());
            return resolved;
        }

        match self
            .medium()
            .and_then(|facts| facts.resolutions.get(namespace))
        {
            Some(resolved) => resolved.clone(),
            // `SearchExhausted`, never `NoLongerExists`: the search has not happened, so absence
            // was never established, and reporting "gone" for a directory nobody looked for is
            // the calm plausible wrong answer this project exists to remove.
            None => NamespaceResolution::SearchExhausted,
        }
    }

    fn sweep_for_repositories(&self, roots: &[String]) -> Sweep {
        if self.is_live(Tier::Medium) {
            let swept = self.delegate.sweep_for_repositories(roots);
            self.gathered
                .lock()
                .expect("uncontended")
                .sweep
                .get_or_insert_with(|| (roots.to_vec(), swept.clone()));
            return swept;
        }

        match self.medium().and_then(|facts| facts.sweep.clone()) {
            Some((_, swept)) => swept,
            // `complete: false` is what makes this loud: the collection reports partial coverage
            // and the panel says so, rather than presenting an empty sweep as an exhaustive one.
            None => Sweep {
                repositories: Vec::new(),
                complete: false,
                directories_visited: 0,
            },
        }
    }

    fn codex_sessions(&self) -> Result<Vec<CodexSession>, WorldError> {
        if self.is_live(Tier::Slow) {
            let read = self.delegate.codex_sessions();
            self.gathered
                .lock()
                .expect("uncontended")
                .codex
                .get_or_insert_with(|| read.clone());
            return read;
        }

        match self.slow().and_then(|facts| facts.codex.clone()) {
            Some(read) => read,
            None => Err(WorldError::CodexIndex(not_yet(
                Tier::Slow,
                "the Codex session index",
            ))),
        }
    }

    fn vcs_facts(&self, path: &str) -> Result<VcsFacts, Unreadable> {
        // Reached only through the batch below in a live slow pass; a single-path call in any
        // other tier is answered from the cache like everything else.
        self.vcs_facts_batch(std::slice::from_ref(&path.to_string()))
            .into_iter()
            .next()
            .unwrap_or(Err(Unreadable::NotYetRead))
    }

    fn vcs_facts_batch(&self, paths: &[String]) -> Vec<Result<VcsFacts, Unreadable>> {
        let cached = self.slow().map(|facts| &facts.vcs);
        let carried = |path: &str| cached.and_then(|vcs| vcs.get(&path.to_lowercase()).cloned());

        if !self.is_live(Tier::Slow) {
            return paths
                .iter()
                .map(|path| {
                    carried(path)
                        .map(|read| read.facts)
                        .unwrap_or(Err(Unreadable::NotYetRead))
                })
                .collect();
        }

        // The stalest slice this pass can afford. Never-read workspaces first, so a workspace
        // that has just appeared is the one asked about next rather than the one that waits
        // longest.
        //
        // "Never read" is the **absence** of a cached entry, which is why nothing is ever cached
        // for a workspace this pass did not reach. Recording a not-read marker instead would give
        // it a fresh age, and every workspace waiting its turn would look freshly read — the
        // slice would then re-read the same handful forever while the rest were starved, and the
        // pending count would fall to zero having established nothing.
        let age: Vec<Option<Duration>> = paths
            .iter()
            .map(|path| carried(path).map(|read| self.at.saturating_sub(read.at)))
            .collect();

        // The slice counts **repositories**, not candidates, and the difference is not a detail.
        // Candidate cost is wildly non-uniform: a path that is not in a repository is answered by
        // a couple of `stat` calls, while one that is costs a `git status` — a measured median of
        // 59 ms. Sizing the slice by candidates was measured doing exactly what that predicts: a
        // slice of 64 non-repositories cost 5 ms, the adaptive sizing concluded there was room for
        // four times as many, and the next slice happened to contain the machine's real
        // repositories and took 1791 ms against a 300 ms budget. The estimate was not wrong about
        // the pass it measured; it was measuring the wrong thing.
        //
        // So the cheap candidates are all read every pass — deferring a `stat` buys nothing — and
        // the budget is spent on the ones that cost. `repository_root` is what tells them apart,
        // and it is the same handful of `stat` calls the collection already makes.
        let order = stalest_first(&age, paths.len());
        let mut chosen = Vec::new();
        let mut expensive = 0;
        for index in order {
            if self.delegate.repository_root(&paths[index]).is_some() {
                if expensive >= self.slice {
                    continue;
                }
                expensive += 1;
            }
            chosen.push(index);
        }

        let asked: Vec<String> = chosen.iter().map(|index| paths[*index].clone()).collect();
        let fresh = self.delegate.vcs_facts_batch(&asked);

        let mut answers: HashMap<usize, Result<VcsFacts, Unreadable>> = HashMap::new();
        for (position, index) in chosen.iter().enumerate() {
            let facts = fresh
                .get(position)
                .cloned()
                // A batch that answered about fewer paths than it was asked about has broken its
                // contract. Said, rather than filled in: an invented `Ok` would report a
                // workspace clean on the strength of an answer nobody gave.
                .unwrap_or_else(|| {
                    Err(Unreadable::QueryFailed(format!(
                        "the batch was asked about {} workspaces and answered about {}",
                        asked.len(),
                        fresh.len()
                    )))
                });
            answers.insert(*index, facts);
        }

        let mut gathered = self.gathered.lock().expect("uncontended");
        let mut result = Vec::with_capacity(paths.len());
        let mut never_read = 0;
        for (index, path) in paths.iter().enumerate() {
            let read = match answers.remove(&index) {
                Some(facts) => Some(ReadVcs {
                    facts,
                    observed_at: self.now,
                    at: self.at,
                }),
                None => carried(path),
            };
            match read {
                Some(read) => {
                    result.push(read.facts.clone());
                    gathered.vcs.insert(path.to_lowercase(), read);
                }
                None => {
                    // Nothing cached and nothing read: this workspace is waiting its turn. It goes
                    // into the answer as `NotYetRead` and into the pending count, and pointedly
                    // NOT into the cache — see the note above the slice.
                    never_read += 1;
                    result.push(Err(Unreadable::NotYetRead));
                }
            }
        }
        gathered.asked = paths.len();
        gathered.read = chosen.len();
        gathered.never_read = never_read;

        result
    }

    // --- Everything that is not a tiered observation passes straight through. ---

    fn repository_root(&self, path: &str) -> Option<(String, bool)> {
        self.delegate.repository_root(path)
    }

    fn output_width(&self) -> u16 {
        self.delegate.output_width()
    }

    fn load_average(&self) -> Result<LoadAverage, String> {
        self.delegate.load_average()
    }

    fn read_state(&self) -> StateRead {
        self.delegate.read_state()
    }

    fn path_notices(&self) -> Vec<String> {
        self.delegate.path_notices()
    }

    fn write_state(&self, contents: &str) -> Result<(), String> {
        self.delegate.write_state(contents)
    }

    fn read_notified(&self) -> StateRead {
        self.delegate.read_notified()
    }

    fn write_notified(&self, contents: &str) -> Result<(), String> {
        self.delegate.write_notified(contents)
    }

    fn read_notify_config(&self) -> crate::world::NotifyConfig {
        self.delegate.read_notify_config()
    }

    fn read_detector_config(&self) -> crate::world::DetectorConfig {
        self.delegate.read_detector_config()
    }

    fn notify_local(&self, command: &str, payload: &str) -> crate::world::NotifyOutcome {
        self.delegate.notify_local(command, payload)
    }

    fn notify_remote(&self, url: &str, payload: &str) -> crate::world::NotifyOutcome {
        self.delegate.notify_remote(url, payload)
    }

    fn notify_local_batch(&self, command: &str, payloads: &[String]) -> crate::DeliveryReport {
        self.delegate.notify_local_batch(command, payloads)
    }

    fn notify_remote_batch(&self, url: &str, payloads: &[String]) -> crate::DeliveryReport {
        self.delegate.notify_remote_batch(url, payloads)
    }
}

/// What one tier's pass produced.
pub struct Pass {
    pub tier: Tier,
    /// The whole assembled collection, as of this pass.
    pub snapshot: Result<Snapshot, CollectError>,
    /// The facts this pass gathered, stamped, ready to fold into [`Observed`].
    pub facts: TierFacts,
    pub observed_at: SystemTime,
    pub at: Duration,
    pub took: Duration,
    pub completion: Completion,
    pub load: Result<LoadAverage, String>,
    /// Which role the assembly ran in, and therefore whether it wrote or announced anything.
    pub role: Role,
}

/// One tier's gathered facts, in the shape [`Observed`] holds them.
pub enum TierFacts {
    Fast(FastFacts),
    Medium(MediumFacts),
    Slow(SlowFacts),
}

/// Run one tier's pass: observe that tier, reuse the others, assemble, and report.
///
/// `role` is what decides whether this pass writes the memory file and asks a notification
/// channel anything — the same gate the display relies on (#10), and the reason a warming-up
/// monitor can build a payload without announcing a workspace nobody has looked at.
///
/// Takes `&(dyn World + Sync)` because the medium and slow passes run off the loop's thread, so
/// the world has to be shareable. That is also what keeps a 2.7 s git sweep from delaying the
/// fast tier past its own interval.
#[allow(clippy::too_many_arguments)]
pub fn run_pass(
    tier: Tier,
    observed: &Observed,
    world: &(dyn World + Sync),
    now: SystemTime,
    at: Duration,
    thresholds: &Thresholds,
    role: Role,
    budgets: &Budgets,
) -> Pass {
    let slice = observed.slice();
    let load = world.load_average();
    let tiered = TieredWorld::new(observed, world, tier, now, at, slice);

    let started = std::time::Instant::now();
    let snapshot = collect_as(&tiered, now, thresholds, role);
    let took = started.elapsed();

    let gathered = tiered.into_gathered();
    let facts = match tier {
        Tier::Fast => TierFacts::Fast(FastFacts {
            snapshot: gathered.snapshot.unwrap_or_else(|| {
                Err(WorldError::ProcessEnumeration(
                    "the pass ended before the process table was read".to_string(),
                ))
            }),
            resources: gathered.resources,
            namespaces: gathered.namespaces.unwrap_or_else(|| {
                Err(WorldError::NamespaceListing(
                    "the pass ended before the transcript stores were listed".to_string(),
                ))
            }),
        }),
        Tier::Medium => TierFacts::Medium(MediumFacts {
            resolutions: gathered.resolutions,
            sweep: gathered.sweep,
            activity: gathered.activity,
        }),
        Tier::Slow => {
            let coverage = Coverage {
                total: gathered.asked,
                read: gathered.read,
                never_read: gathered.never_read,
            };
            TierFacts::Slow(SlowFacts {
                codex: gathered.codex,
                vcs: gathered.vcs,
                coverage,
                slice: crate::schedule::resized_slice(slice, took, budgets.slow),
            })
        }
    };

    Pass {
        tier,
        snapshot,
        facts,
        observed_at: now,
        at,
        took,
        completion: Completion::of(took, budgets.budget(tier)),
        load,
        role,
    }
}

impl Observed {
    /// Fold a completed pass into what is known, stamping it.
    ///
    /// The medium and slow tiers keep their previous facts when a pass gathered none — a pass
    /// that could not run because the fast tier had not yet read the process table must not
    /// erase what the last successful pass established.
    pub fn absorb(&mut self, pass: &Pass, sequence: u64) {
        match &pass.facts {
            TierFacts::Fast(facts) => {
                self.fast = Some(Stamped {
                    facts: facts.clone(),
                    observed_at: pass.observed_at,
                    at: pass.at,
                    took: pass.took,
                    sequence,
                    load: pass.load.clone(),
                });
            }
            TierFacts::Medium(facts) => {
                let carried = match (&self.medium, facts.sweep.is_none()) {
                    (Some(previous), true) => MediumFacts {
                        resolutions: if facts.resolutions.is_empty() {
                            previous.facts.resolutions.clone()
                        } else {
                            facts.resolutions.clone()
                        },
                        sweep: previous.facts.sweep.clone(),
                        activity: if facts.activity.is_empty() {
                            previous.facts.activity.clone()
                        } else {
                            facts.activity.clone()
                        },
                    },
                    _ => facts.clone(),
                };
                self.medium = Some(Stamped {
                    facts: carried,
                    observed_at: pass.observed_at,
                    at: pass.at,
                    took: pass.took,
                    sequence,
                    load: pass.load.clone(),
                });
            }
            TierFacts::Slow(facts) => {
                let carried = if facts.vcs.is_empty() && facts.codex.is_none() {
                    match &self.slow {
                        Some(previous) => previous.facts.clone(),
                        None => facts.clone(),
                    }
                } else {
                    SlowFacts {
                        codex: facts
                            .codex
                            .clone()
                            .or_else(|| self.slow.as_ref().and_then(|s| s.facts.codex.clone())),
                        ..facts.clone()
                    }
                };
                self.slow = Some(Stamped {
                    facts: carried,
                    observed_at: pass.observed_at,
                    at: pass.at,
                    took: pass.took,
                    sequence,
                    load: pass.load.clone(),
                });
            }
        }
    }
}

// --- What gets published -----------------------------------------------------------------
//
// The shapes below are the on-disk contract between `amon` and every reader of `state.json`.
// Flat, named, and made of strings and `Measured` figures rather than of this crate's internal
// enums — deliberately. A payload that serialised the collector's own types would change shape
// every time one of them was refactored, and a reader would have no way to tell a schema change
// from a machine that had gone quiet.

/// What every tier's payload says about the pass that produced it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PassEnvelope {
    /// `fast`, `medium` or `slow`.
    pub tier: String,
    /// How many passes of this tier have completed since the monitor started.
    pub sequence: u64,
    /// When the pass started, ISO 8601. The same instant as this tier's stamp in the state file;
    /// carried inside the payload too so a payload lifted out of the file still says how old it is.
    pub started_at: String,
    pub took_ms: u128,
    pub budget_ms: u128,
    /// Why this pass exceeded its budget, when it did. `null` when it did not.
    pub overran: Option<String>,
    /// What the machine as a whole was carrying, so a sample taken under heavy load is
    /// identifiable afterwards.
    pub load: Measured<LoadAverage>,
    /// Whether every tier had run at least once when this pass was taken.
    pub every_tier_has_run: bool,
    /// Whether this pass was allowed to announce anything.
    ///
    /// False while the monitor is warming up — before every workspace has been read once. A
    /// reader looking at an at-risk workspace and no alert needs to be able to tell "nothing was
    /// announced because nothing had to be" from "nothing was announced because this monitor is
    /// not announcing yet".
    pub announcing: bool,
}

/// One session, as the state file publishes it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionRow {
    /// The pid, when this session was found as a live process.
    pub pid: Option<i32>,
    /// The transcript this session is recorded under, when it was found that way.
    pub recorded_as: Option<String>,
    pub cli: String,
    /// `ACTIVE`, `WAITING`, `STALLED` or `UNKNOWN`.
    pub state: String,
    /// Which observation produced that state.
    pub method: String,
    /// Whether the verdict was inferred from circumstance rather than observed.
    pub inferred: bool,
    /// Why the state is `UNKNOWN`, when it is.
    pub unknown_why: Option<String>,
    /// Whether that unknown is a structural limit rather than a fault that may clear.
    pub unknown_is_structural: bool,
    pub workspace: Measured<String>,
    pub own_cpu_ms: Measured<u128>,
    pub children_cpu_ms: Measured<u128>,
    pub current_memory_bytes: Measured<u64>,
    pub peak_memory_bytes: Measured<u64>,
    pub bytes_written: Measured<u64>,
    /// Which reader answered: `proc_pid_rusage` or `ps`.
    pub source: Measured<String>,
    /// When these figures were read, if they are a remembered reading rather than a live one.
    ///
    /// `null` for a live reading. A remembered figure presented at a live timestamp is exactly
    /// the plausible wrong answer this project exists to remove.
    pub remembered_at: Option<String>,
}

/// One workspace, as the at-risk panel needs it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceRow {
    pub path: String,
    /// `CLEAN`, `DIRTY-DRIVEN`, `DIRTY-STRANDED` or `UNKNOWN`.
    pub state: String,
    pub at_risk: bool,
    /// Why the state is `UNKNOWN`, when it is.
    pub unknown_why: Option<String>,
    pub uncommitted_entries: Option<usize>,
    pub linked_worktree: bool,
    /// When `git` was last asked about **this** workspace, ISO 8601.
    ///
    /// Per row, not per tier, because the slow tier reads a slice at a time: two rows in one
    /// payload can legitimately be minutes apart in age, and one stamp for the pass would
    /// misdescribe all but the newest. `null` means it has never been asked.
    pub observed_at: Option<String>,
}

/// The fast tier's payload: the sessions, and the monitor's account of itself.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FastPayload {
    pub pass: PassEnvelope,
    pub sessions: Vec<SessionRow>,
    /// What the monitor costs, measured by the monitor (F25, G7).
    pub monitor: SelfReport,
    /// This launch: its downtime, whether the run before it exited cleanly, and how many launches
    /// are on record here (F23).
    ///
    /// Run-scoped rather than pass-scoped, like `monitor` beside it, and republished unchanged on
    /// every fast pass. It is here rather than in a file of its own because a display reading
    /// `state.json` must be able to see that the monitor it is drawing has been cycling — a crash
    /// loop nobody looks up is the silent gap this field exists to close.
    pub launch: crate::starts::StartRecord,
    /// Why this launch could not be appended to `starts.jsonl`, when it could not.
    ///
    /// The facts above are decided before the append is attempted, so they survive a state
    /// directory that has gone read-only. What does not survive is the *durable* history, and that
    /// loss is said out loud here rather than showing up later as a record with a launch missing
    /// from it.
    pub launch_not_recorded: Option<String>,
    /// How many notifications this pass decided were worth making, and what became of them.
    pub notify: NotifyRow,
    /// Which tier the silence behind each session's state was read by.
    ///
    /// On disk, in words, because it is the one thing about this payload a reader could otherwise
    /// get wrong: the rows are the fast tier's, but the transcript activity a `WAITING` rests on is
    /// the medium tier's, so the age of that evidence is the `Medium` stamp in this file and not
    /// the `Fast` one. A payload that let a reader assume otherwise would present ten-minute-old
    /// silence at a ten-second stamp.
    pub silence_read_by: String,
}

/// The medium tier's payload: what the searches found.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MediumPayload {
    pub pass: PassEnvelope,
    /// How many directories the sweep visited, so both its cost and its bound are checkable.
    pub directories_visited: usize,
    /// False when the sweep hit its bound before finishing. A partial sweep presented as
    /// complete is a silent cap in a safety net.
    pub sweep_complete: bool,
    pub repositories_found: usize,
    /// Recorded namespaces that could not be turned into a directory, and what the search
    /// concluded about each.
    pub unlocated: Vec<(String, String)>,
}

/// The slow tier's payload: the workspaces, with per-row ages and its own coverage.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SlowPayload {
    pub pass: PassEnvelope,
    pub workspaces: Vec<WorkspaceRow>,
    pub at_risk: usize,
    /// How many workspaces this pass actually asked `git` about.
    pub read_this_pass: usize,
    /// How many workspaces have never been read at all.
    ///
    /// The number that says the at-risk panel is not yet exhaustive. Zero is the steady state.
    pub never_read: usize,
    /// How many workspaces the next pass will attempt.
    pub next_slice: usize,
}

/// What one pass's notifications came to.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NotifyRow {
    pub notable: usize,
    pub delivered: usize,
    pub failed: usize,
    pub not_attempted: usize,
    /// Why nothing was attempted, when nothing was.
    pub reason: Option<String>,
    /// Present when the collection was read-only, which for the monitor means it is warming up.
    pub read_only: Option<String>,
}

fn iso(time: SystemTime) -> String {
    crate::isotime::iso8601_from_unix_seconds(crate::isotime::unix_seconds(time))
}

/// The envelope every tier's payload opens with.
pub fn envelope(pass: &Pass, sequence: u64, observed: &Observed) -> PassEnvelope {
    PassEnvelope {
        tier: tier_name(pass.tier).to_string(),
        sequence,
        started_at: iso(pass.observed_at),
        took_ms: pass.took.as_millis(),
        budget_ms: Budgets::DEFAULT.budget(pass.tier).as_millis(),
        overran: pass.completion.why(),
        load: Measured::from(pass.load.clone()),
        every_tier_has_run: observed.every_tier_has_run(),
        announcing: matches!(pass.role, Role::Monitor),
    }
}

/// The fast tier's payload.
pub fn fast_payload(
    pass: &Pass,
    sequence: u64,
    observed: &Observed,
    monitor: SelfReport,
    launch: &crate::starts::Launch,
) -> FastPayload {
    let snapshot = pass.snapshot.as_ref();
    FastPayload {
        pass: envelope(pass, sequence, observed),
        sessions: snapshot
            .map(|snapshot| snapshot.sessions.iter().map(session_row).collect())
            .unwrap_or_default(),
        monitor,
        launch: launch.record.clone(),
        launch_not_recorded: launch.not_recorded.clone(),
        notify: snapshot.map(notify_row).unwrap_or(NotifyRow {
            notable: 0,
            delivered: 0,
            failed: 0,
            not_attempted: 0,
            reason: Some("the collection failed, so nothing was decided or delivered".to_string()),
            read_only: None,
        }),
        silence_read_by: tier_name(Tier::Medium).to_string(),
    }
}

/// The medium tier's payload.
pub fn medium_payload(pass: &Pass, sequence: u64, observed: &Observed) -> MediumPayload {
    let sweep = observed
        .medium
        .as_ref()
        .and_then(|stamped| stamped.facts.sweep.as_ref());
    MediumPayload {
        pass: envelope(pass, sequence, observed),
        directories_visited: sweep
            .map(|(_, sweep)| sweep.directories_visited)
            .unwrap_or(0),
        sweep_complete: pass
            .snapshot
            .as_ref()
            .map(|snapshot| snapshot.sweep_complete)
            .unwrap_or(false),
        repositories_found: sweep
            .map(|(_, sweep)| sweep.repositories.len())
            .unwrap_or(0),
        unlocated: pass
            .snapshot
            .as_ref()
            .map(|snapshot| {
                snapshot
                    .unlocated
                    .iter()
                    .map(|(namespace, resolution)| (namespace.clone(), resolution.to_string()))
                    .collect()
            })
            .unwrap_or_default(),
    }
}

/// The slow tier's payload, with each workspace's own age.
pub fn slow_payload(pass: &Pass, sequence: u64, observed: &Observed) -> SlowPayload {
    let read = observed
        .slow
        .as_ref()
        .map(|stamped| &stamped.facts.vcs)
        .cloned()
        .unwrap_or_default();

    let workspaces: Vec<WorkspaceRow> = pass
        .snapshot
        .as_ref()
        .map(|snapshot| {
            snapshot
                .workspaces
                .iter()
                .map(|workspace| WorkspaceRow {
                    path: workspace.path.clone(),
                    state: workspace.state.to_string(),
                    at_risk: workspace.state.at_risk(),
                    unknown_why: match &workspace.state {
                        crate::vcs::WorkspaceState::Unknown(why) => Some(why.to_string()),
                        _ => None,
                    },
                    uncommitted_entries: workspace.uncommitted_entries,
                    linked_worktree: workspace.linked_worktree,
                    // `None` exactly when the slow tier has never read this workspace, because
                    // nothing is cached for one it has not reached.
                    observed_at: read
                        .get(&workspace.path.to_lowercase())
                        .map(|read| iso(read.observed_at)),
                })
                .collect()
        })
        .unwrap_or_default();

    let coverage = observed
        .slow
        .as_ref()
        .map(|stamped| stamped.facts.coverage)
        .unwrap_or(Coverage {
            total: 0,
            read: 0,
            never_read: 0,
        });

    SlowPayload {
        pass: envelope(pass, sequence, observed),
        at_risk: workspaces.iter().filter(|row| row.at_risk).count(),
        workspaces,
        read_this_pass: coverage.read,
        never_read: coverage.never_read,
        next_slice: observed.slice(),
    }
}

/// The stamp a tier's facts should be published under.
///
/// For the fast and medium tiers this is the pass's own start. For the slow tier it is the
/// **oldest** workspace reading it published, because that tier reads a slice at a time: stamping
/// it with the pass that just ran would describe a ten-minute-old git fact as seconds old, which
/// is the one thing F30 exists to prevent.
pub fn stamp_for(pass: &Pass, observed: &Observed) -> SystemTime {
    match pass.tier {
        Tier::Fast | Tier::Medium => pass.observed_at,
        Tier::Slow => observed
            .slow
            .as_ref()
            .and_then(|stamped| {
                stamped
                    .facts
                    .vcs
                    .values()
                    .map(|read| read.observed_at)
                    .min()
            })
            .unwrap_or(pass.observed_at),
    }
}

fn notify_row(snapshot: &Snapshot) -> NotifyRow {
    let health = &snapshot.remembered.notify_health;
    NotifyRow {
        notable: health.notable,
        delivered: health.local_delivered + health.remote_delivered,
        failed: health.local_failed + health.remote_failed,
        not_attempted: health.not_attempted(),
        reason: health.not_attempted_reason.clone(),
        read_only: health.read_only.map(|why| why.to_string()),
    }
}

fn session_row(session: &Session) -> SessionRow {
    let (pid, recorded_as) = match &session.identity {
        Identity::Process { pid } => (Some(*pid), None),
        Identity::Transcript { recorded_as } => (None, Some(recorded_as.clone())),
    };

    let unknown = session.liveness_unknown();

    // The live reading if there is one, otherwise the remembered one — and never both, which is
    // the invariant `collect` already enforces: `last_reading` is only ever present alongside an
    // unreadable ledger.
    let (resources, remembered_at) = match (&session.resources, &session.last_reading) {
        (Ok(resources), _) => (Ok(resources.clone()), None),
        (Err(_), Some(reading)) => (Ok(reading.resources.clone()), Some(iso(reading.taken_at))),
        (Err(why), None) => (Err(why.to_string()), None),
    };

    let figure = |pick: fn(&Resources) -> Result<u64, String>| match &resources {
        Ok(resources) => Measured::from(pick(resources)),
        Err(why) => Measured::unavailable(why.clone()),
    };

    SessionRow {
        pid,
        recorded_as,
        cli: session.cli.clone(),
        state: session.liveness.state.label().to_string(),
        method: session.liveness.method.to_string(),
        inferred: session.liveness.method.is_inferred(),
        unknown_why: unknown.as_ref().map(|why| why.to_string()),
        unknown_is_structural: unknown.map(|why| why.is_structural()).unwrap_or(false),
        workspace: Measured::from(
            session
                .workspace
                .as_ref()
                .map(|workspace| workspace.path.clone())
                .map_err(|why| why.to_string()),
        ),
        own_cpu_ms: match &resources {
            Ok(resources) => Measured::from(
                resources
                    .own_cpu
                    .as_ref()
                    .map(|cpu| cpu.as_millis())
                    .map_err(|why| why.to_string()),
            ),
            Err(why) => Measured::unavailable(why.clone()),
        },
        children_cpu_ms: match &resources {
            Ok(resources) => Measured::from(
                resources
                    .children_cpu
                    .as_ref()
                    .map(|cpu| cpu.as_millis())
                    .map_err(|why| why.to_string()),
            ),
            Err(why) => Measured::unavailable(why.clone()),
        },
        current_memory_bytes: figure(|resources| {
            resources
                .current_memory
                .as_ref()
                .copied()
                .map_err(|why| why.to_string())
        }),
        peak_memory_bytes: figure(|resources| {
            resources
                .peak_memory
                .as_ref()
                .copied()
                .map_err(|why| why.to_string())
        }),
        bytes_written: figure(|resources| {
            resources
                .bytes_written
                .as_ref()
                .copied()
                .map_err(|why| why.to_string())
        }),
        source: match &resources {
            Ok(resources) => Measured::known(resources.source.to_string()),
            Err(why) => Measured::unavailable(why.clone()),
        },
        remembered_at,
    }
}

// --- Reading it back ---------------------------------------------------------------------
//
// The monitor writes these payloads and the display reads them, and the two must not each own
// half of the schema. So the decoding lives here, beside the encoding: a field renamed on one
// side fails to compile on the other, rather than turning into an absent figure on a screen.

/// One tier's payload, decoded, with the stamp the state file carried it under.
#[derive(Debug, Clone, PartialEq)]
pub enum Published {
    Fast(Box<FastPayload>),
    Medium(Box<MediumPayload>),
    Slow(Box<SlowPayload>),
}

/// Decode a tier's payload out of a published state file.
///
/// `Ok(None)` when that tier has not been published — a monitor that has just started, which is a
/// real state and not a fault. `Err` when there is a payload and it could not be understood, which
/// is a different thing entirely and must not read as absence: a schema the reader does not know
/// means the figures on screen would be a guess.
pub fn published(
    state: &crate::state::TieredState,
    tier: Tier,
) -> Result<Option<(Published, SystemTime)>, String> {
    let Some(data) = state.tier_data(tier) else {
        return Ok(None);
    };
    let stamp = state.tier_timestamp(tier).ok_or_else(|| {
        format!(
            "the {} tier has a payload and no timestamp, so nothing in it has a knowable age",
            tier_name(tier)
        )
    })?;

    let decoded = match tier {
        Tier::Fast => serde_json::from_value::<FastPayload>(data.clone())
            .map(|payload| Published::Fast(Box::new(payload))),
        Tier::Medium => serde_json::from_value::<MediumPayload>(data.clone())
            .map(|payload| Published::Medium(Box::new(payload))),
        Tier::Slow => serde_json::from_value::<SlowPayload>(data.clone())
            .map(|payload| Published::Slow(Box::new(payload))),
    }
    .map_err(|error| {
        format!(
            "the {} tier's payload could not be read: {error}",
            tier_name(tier)
        )
    })?;

    Ok(Some((decoded, stamp)))
}

/// What the monitor published about its own cost, ready for the display's meter row.
///
/// Both figures are `Result`, and neither is ever a zero standing in for an absence: a duty cycle
/// of 0% is a monitor that is running and idle, which is the one reading none of the failures here
/// mean.
#[derive(Debug, Clone, PartialEq)]
pub struct PublishedMeters {
    /// What the most recent fast pass cost — the collection overhead figure the display draws.
    pub overhead: Result<Duration, String>,
    /// The monitor's duty cycle over its trailing window, as a fraction of one core.
    pub duty_cycle: Result<f64, String>,
    /// When the pass that produced these figures ran. The age of the meter, not of the machine.
    pub taken_at: SystemTime,
    /// Every tier's most recent pass duration, with how old that pass was when it was published.
    ///
    /// Three figures rather than one, because a monitor with three cadences has three overheads
    /// and flattening them would hide the only one that is expensive.
    pub per_tier: Vec<(String, Duration, Duration)>,
}

/// The monitor's own figures, out of a published state file.
///
/// Reads only the fast tier, deliberately: the self-report is published there because it is
/// measured afresh on every fast pass, so it is the youngest thing in the file rather than
/// something a reader has to correlate across three stamps.
pub fn published_meters(
    state: &crate::state::TieredState,
) -> Result<Option<PublishedMeters>, String> {
    let Some((Published::Fast(payload), stamp)) = published(state, Tier::Fast)? else {
        return Ok(None);
    };

    let figure = |measured: &Measured<f64>| match (&measured.value, &measured.unavailable) {
        (Some(value), _) => Ok(*value),
        (None, Some(why)) => Err(why.clone()),
        (None, None) => Err(
            "the monitor published neither a figure nor a reason, which is a fault in its own \
             metering"
                .to_string(),
        ),
    };

    let fast = payload
        .monitor
        .last_pass
        .iter()
        .find(|pass| pass.tier == tier_name(Tier::Fast));

    Ok(Some(PublishedMeters {
        overhead: match fast {
            Some(pass) => Ok(Duration::from_millis(pass.took_ms as u64)),
            None => Err(
                "the monitor has published no fast pass, so there is no collection overhead to \
                 show yet"
                    .to_string(),
            ),
        },
        duty_cycle: figure(&payload.monitor.duty_cycle),
        taken_at: stamp,
        per_tier: payload
            .monitor
            .last_pass
            .iter()
            .map(|pass| {
                (
                    pass.tier.clone(),
                    Duration::from_millis(pass.took_ms as u64),
                    Duration::from_millis(pass.age_ms as u64),
                )
            })
            .collect(),
    }))
}

/// How many of a collection's sessions were observed as live processes.
///
/// What the pace decision rests on (F22). Counted from `Identity::Process`, which means the
/// process was in the enumeration — an observation, not an inference from silence.
pub fn live_sessions(snapshot: &Snapshot) -> usize {
    snapshot
        .sessions
        .iter()
        .filter(|session| matches!(session.identity, Identity::Process { .. }))
        .count()
}
