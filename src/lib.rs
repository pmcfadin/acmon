//! acmon — measuring what AI coding agents cost on a managed macOS machine.

pub mod collect;
pub mod detect;
mod isotime;
pub mod liveness;
pub mod machtime;
pub mod real_world;
pub mod render;
pub mod vcs;
pub mod workspace;
pub mod world;

pub use collect::{collect, CollectError, Identity, Session, Snapshot, WorkspaceReport};
pub use detect::Detector;
pub use real_world::RealWorld;
pub use world::{PathUnavailable, ProcessRecord, ProcessSnapshot, World, WorldError};
