//! acmon — measuring what AI coding agents cost on a managed macOS machine.

pub mod cli;
pub mod collect;
pub mod deliver;
pub mod detect;
pub mod display;
mod isotime;
pub mod launchd;
pub mod liveness;
pub mod lock;
pub mod machtime;
pub mod memory;
pub mod notify;
pub mod real_world;
pub mod render;
pub mod state;
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
    NotifyConfig, NotifyOutcome, PathUnavailable, ProcessRecord, ProcessSnapshot, StateRead, World,
    WorldError,
};
