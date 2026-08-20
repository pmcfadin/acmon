//! The lifecycle around `amon watch`: take the lock, publish who the writer is, release.
//!
//! What lives here is everything that is true of the monitor's run regardless of what the run
//! collects — F18 and F19. The tiered collection loop itself is #27, and until it lands this
//! lifecycle has nothing to wrap: `watch` therefore ends in [`WatchStopped::LoopNotBuilt`] and
//! `amon` exits non-zero. That refusal is deliberate. A `watch` that took the lock, collected
//! nothing and exited zero would report a healthy monitor to a LaunchAgent for as long as the
//! loop stayed unbuilt, which is the calm, plausible, wrong answer this project exists to
//! eliminate, arriving through an exit code.
//!
//! What it does do before stopping is real, and is what the lock is for: it publishes
//! `state.json` naming its own pid as the writer. A reader can therefore see who holds the
//! writer role, and see — from the absence of any tier — that no fact has been collected yet.

use std::time::Duration;

use crate::lock::{LockRefusal, WatchLock};
use crate::state::{Paths, StateStore, TieredState, STATE_FILE};

/// How long `amon watch` holds the lock before releasing it, in milliseconds.
///
/// Zero unless set, because with the collection loop unbuilt (#27) there is nothing to hold it
/// for. It exists so that a test can have two real processes contend for one lock: the
/// behaviour under test is one kernel refusing one process, and neither side can be stubbed
/// without the test becoming a test of itself. Once #27 lands, the loop owns this lifetime and
/// this goes away.
pub const HOLD_VARIABLE: &str = "ACMON_WATCH_HOLD_MS";

/// What `amon watch` was asked to do.
#[derive(Debug, Clone)]
pub struct WatchOptions {
    /// `--foreground`, for debugging. Deliberately changes nothing about the lock: two writers
    /// is two writers regardless of intent (F19).
    pub foreground: bool,
    pub paths: Paths,
    pub hold: Duration,
}

impl WatchOptions {
    /// Resolved from this machine's environment.
    pub fn from_environment(foreground: bool) -> Result<WatchOptions, String> {
        WatchOptions::from_values(
            foreground,
            Paths::from_environment()?,
            std::env::var(HOLD_VARIABLE).ok().as_deref(),
        )
    }

    /// The same, with the environment passed in, so it is testable without mutating a
    /// process-wide variable that every other test in the binary shares.
    pub fn from_values(
        foreground: bool,
        paths: Paths,
        hold: Option<&str>,
    ) -> Result<WatchOptions, String> {
        Ok(WatchOptions {
            foreground,
            paths,
            hold: hold_from_value(hold)?,
        })
    }
}

/// Parse the hold window, refusing a value it cannot read rather than falling back to zero.
///
/// A mistyped duration that silently became "do not hold at all" would turn a deliberate
/// setting into its opposite without saying so.
pub fn hold_from_value(value: Option<&str>) -> Result<Duration, String> {
    let Some(text) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(Duration::ZERO);
    };

    text.parse::<u64>().map(Duration::from_millis).map_err(|_| {
        format!("{HOLD_VARIABLE} must be a whole number of milliseconds, not {text:?}")
    })
}

/// Why the monitor's run ended.
///
/// Every arm is a non-zero exit. There is no success arm yet, because there is no loop yet.
#[derive(Debug)]
pub enum WatchStopped {
    /// Another writer holds the lock. The refusal names it.
    LockRefused(LockRefusal),
    /// The lock was held, but the state file could not be published.
    StateUnwritable(String),
    /// The run finished and the lock would not release. Reported rather than swallowed: the
    /// next start would be refused by a lock nobody is using, and the reason must not be a
    /// mystery when that happens.
    LockNotReleased(String),
    /// The lock was taken, the writer published, the lock released — and the thing the lock
    /// exists to protect has not been built.
    LoopNotBuilt { tracked_as: &'static str },
}

/// Run the monitor's lifecycle.
///
/// `notice` receives each thing worth saying as it happens rather than at the end, because a
/// resident monitor that took over a dead predecessor's lock must say so at the moment it
/// happens, not once its run is over.
pub fn watch(options: &WatchOptions, notice: &mut dyn FnMut(&str)) -> WatchStopped {
    let state_dir = options.paths.state_dir().to_path_buf();

    // Before the first write, always. Anything written before this point could be written by
    // two processes at once, which is the whole failure.
    let lock = match WatchLock::acquire(&state_dir) {
        Ok(lock) => lock,
        Err(refusal) => return WatchStopped::LockRefused(refusal),
    };

    if let Some(predecessor) = lock.took_over_from() {
        notice(&format!(
            "took over the state lock from pid {}, which {} — it did not release it, so its \
             run ended without a clean exit",
            predecessor.pid,
            if predecessor.still_running {
                "is still running but does not hold the lock"
            } else {
                "is no longer running"
            }
        ));
    }
    if let Some(reason) = lock.unreadable_record() {
        notice(&format!(
            "the previous lock record could not be read: {reason}. The lock is this process's \
             now, but who held it last is not known"
        ));
    }

    notice(&format!(
        "holding the state lock {} as pid {}{}",
        lock.path().display(),
        lock.holder_pid(),
        if options.foreground {
            " (foreground; the lock applies here exactly as it does under launchd)"
        } else {
            ""
        }
    ));

    let store = StateStore::new(options.paths.clone());
    let state = TieredState::new(lock.holder_pid());
    if let Err(reason) = store.write_tiered_state(STATE_FILE, &state) {
        // Release before reporting, so a start that failed for an unrelated reason does not
        // leave a lock behind that reads as a crash.
        if let Err(release) = lock.release() {
            return WatchStopped::StateUnwritable(format!("{reason} (and then {release})"));
        }
        return WatchStopped::StateUnwritable(reason);
    }

    notice(&format!(
        "published {} naming pid {} as the sole writer; no tier has been collected",
        state_dir.join(STATE_FILE).display(),
        lock.holder_pid()
    ));

    if !options.hold.is_zero() {
        std::thread::sleep(options.hold);
    }

    if let Err(reason) = lock.release() {
        return WatchStopped::LockNotReleased(reason);
    }

    WatchStopped::LoopNotBuilt { tracked_as: "#27" }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_absent_hold_window_is_no_hold_at_all() {
        assert_eq!(
            hold_from_value(None).expect("absent is fine"),
            Duration::ZERO
        );
        assert_eq!(
            hold_from_value(Some("   ")).expect("blank reads as absent"),
            Duration::ZERO
        );
    }

    #[test]
    fn a_hold_window_that_cannot_be_read_is_refused_rather_than_treated_as_zero() {
        let reason = hold_from_value(Some("a while")).expect_err("not a number");
        assert!(reason.contains(HOLD_VARIABLE), "{reason}");
        assert!(reason.contains("a while"), "{reason}");
    }

    #[test]
    fn a_hold_window_is_read_as_milliseconds() {
        assert_eq!(
            hold_from_value(Some("1500")).expect("a number"),
            Duration::from_millis(1500)
        );
    }
}
