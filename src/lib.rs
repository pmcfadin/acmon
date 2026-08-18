//! acmon — measuring what AI coding agents cost on a managed macOS machine.

pub mod collect;
pub mod detect;
mod isotime;
pub mod liveness;
pub mod machtime;
pub mod memory;
pub mod notify;
pub mod real_world;
pub mod render;
pub mod vcs;
pub mod workspace;
pub mod world;

pub use collect::{
    collect, CollectError, Identity, NotifyHealth, Remembered, Session, Snapshot, WorkspaceReport,
};
pub use detect::Detector;
pub use memory::Memory;
pub use real_world::RealWorld;
pub use world::{
    NotifyConfig, NotifyOutcome, PathUnavailable, ProcessRecord, ProcessSnapshot, StateRead, World,
    WorldError,
};
