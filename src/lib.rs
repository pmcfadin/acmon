//! acmon — measuring what AI coding agents cost on a managed macOS machine.

pub mod collect;
pub mod detect;
pub mod real_world;
pub mod render;
pub mod world;

pub use collect::{collect, CollectError, Session, Snapshot};
pub use detect::Detector;
pub use real_world::RealWorld;
pub use world::{ProcessRecord, ProcessSnapshot, World, WorldError};
