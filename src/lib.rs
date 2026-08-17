//! acmon — measuring what AI coding agents cost on a managed macOS machine.

pub mod collect;
pub mod detect;
pub mod machtime;
pub mod real_world;
pub mod render;
pub mod world;

pub use collect::{collect, CollectError, Session, Snapshot};
pub use detect::Detector;
pub use real_world::RealWorld;
pub use world::{ExePathUnavailable, ProcessRecord, ProcessSnapshot, World, WorldError};
