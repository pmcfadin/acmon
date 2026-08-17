//! Seam 4 — mach ticks to wall-clock duration.
//!
//! A fourth seam, and a purely arithmetic one. It exists because the units error it
//! guards against is invisible by inspection: reading ticks as nanoseconds keeps every
//! derived number internally consistent while understating all of them by the same
//! factor. The only defence is a conversion that cannot be bypassed, tested against
//! values worked out independently of the code.

use acmon::machtime::{MachTicks, MachTimebase};
use std::time::Duration;

#[test]
fn a_second_of_ticks_converts_to_a_second() {
    // numer=125, denom=3 was read from mach_timebase_info() on the machine behind
    // docs/observability-mechanics.md — a fixture, not a constant the code may assume.
    // 1e9 ns / (125/3 ns per tick) = 24,000,000 ticks, worked out by hand.
    let apple_silicon = MachTimebase::new(125, 3);

    assert_eq!(
        apple_silicon.duration(MachTicks(24_000_000)),
        Duration::from_secs(1)
    );
}

#[test]
fn reading_ticks_as_nanoseconds_understates_by_the_timebase_ratio() {
    // The exact error this type exists to prevent. A monitor that skips the conversion
    // reports 24 ms where the truth is a full second.
    let ticks = MachTicks(24_000_000);

    let converted = MachTimebase::new(125, 3).duration(ticks);
    let unconverted = Duration::from_nanos(ticks.0);

    assert!(
        converted.as_secs_f64() / unconverted.as_secs_f64() > 41.0,
        "converting must change the answer by the timebase ratio (~41.67x), \
         got {converted:?} against {unconverted:?}"
    );
}

#[test]
fn a_one_to_one_timebase_leaves_ticks_unchanged() {
    // Not every machine reports Apple's ratio; Intel Macs report 1/1. The conversion
    // must use what the running machine says, so this must be an identity.
    assert_eq!(
        MachTimebase::new(1, 1).duration(MachTicks(1_000)),
        Duration::from_nanos(1_000)
    );
}

#[test]
fn a_ledger_from_a_long_lived_machine_converts_without_overflowing() {
    // The mechanics document measured a session whose children had used 32,317 s of
    // CPU. The numerator multiplies the tick count, so the arithmetic has to be wider
    // than u64 to stay honest on a machine that has been up for weeks.
    let very_large = MachTicks(u64::MAX / 2);

    let converted = MachTimebase::new(125, 3).duration(very_large);

    assert!(
        converted.as_secs() > 32_317,
        "a huge tick count must convert to a huge duration, got {converted:?}"
    );
}
