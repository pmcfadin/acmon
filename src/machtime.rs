//! Mach absolute time, and the one conversion that must never be skipped.
//!
//! Every `*_time` field in the kernel's per-process resource ledger is a count of
//! **mach absolute time ticks, not nanoseconds**. On Apple Silicon a tick is 41.6667 ns,
//! so treating the raw count as nanoseconds understates every duration by that factor
//! while leaving the numbers internally consistent — which is why the mistake survives
//! review. See `docs/observability-mechanics.md` §2.3.
//!
//! The defence is in the types: a tick count is a [`MachTicks`], which is not a
//! duration and cannot be used as one. The only way to get a [`std::time::Duration`]
//! out of it is through a [`MachTimebase`] read from the running machine.

use std::time::Duration;

/// A count of mach absolute time ticks. **Not** nanoseconds.
///
/// Deliberately not convertible to a duration on its own: a tick has no fixed length
/// until the machine says what it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct MachTicks(pub u64);

/// The running machine's tick length, as the ratio `numer / denom` nanoseconds.
///
/// Apple Silicon reports 125/3; Intel Macs report 1/1. Nothing may assume either —
/// the value is read at runtime from `mach_timebase_info()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MachTimebase {
    numer: u32,
    denom: u32,
}

impl MachTimebase {
    /// Build a timebase from a known ratio.
    ///
    /// # Panics
    ///
    /// If `denom` is zero. The kernel never reports that; a zero here is an authoring
    /// error, and a silent fallback would hide it.
    pub fn new(numer: u32, denom: u32) -> Self {
        assert!(
            denom != 0,
            "a mach timebase denominator of zero is not a ratio"
        );
        MachTimebase { numer, denom }
    }

    /// Convert a tick count into wall-clock time.
    ///
    /// Widened to `u128` before multiplying: a machine up for weeks produces tick
    /// counts whose product with the numerator does not fit in `u64`, and a silent wrap
    /// there would report a huge duration as a small one.
    pub fn duration(&self, ticks: MachTicks) -> Duration {
        let nanos = u128::from(ticks.0) * u128::from(self.numer) / u128::from(self.denom);
        let secs = (nanos / 1_000_000_000) as u64;
        let subsec = (nanos % 1_000_000_000) as u32;
        Duration::new(secs, subsec)
    }
}
