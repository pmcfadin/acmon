//! acmon — measuring what AI coding agents cost on a managed macOS machine.

pub mod cli;
pub mod collect;
pub mod deliver;
pub mod detect;
pub mod display;
/// Public because `state.json` publishes every instant as ISO 8601 (#25), so **every** reader of
/// that file needs the same conversion its writer used. Two implementations of the same format
/// would eventually disagree about a leap year, and the disagreement would surface as a fact of
/// the wrong age rather than as an error.
pub mod isotime;
pub mod launchd;
pub mod liveness;
pub mod lock;
pub mod machtime;
pub mod memory;
pub mod meter;
pub mod notify;
pub mod real_world;
pub mod render;
pub mod schedule;
pub mod starts;
pub mod state;
pub mod tiers;
pub mod vcs;
pub mod watch;
pub mod workspace;
pub mod world;

pub use collect::{
    collect, collect_as, CollectError, Identity, LivenessUnknown, NotifyHealth, Persistence,
    Remembered, Role, Session, Snapshot, WorkspaceReport,
};
pub use deliver::DeliveryReport;
pub use detect::Detector;
pub use memory::Memory;
pub use real_world::RealWorld;
pub use world::{
    LoadAverage, NotifyConfig, NotifyOutcome, PathUnavailable, ProcessRecord, ProcessSnapshot,
    StateRead, World, WorldError,
};
