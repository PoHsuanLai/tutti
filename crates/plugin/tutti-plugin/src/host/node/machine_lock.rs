//! One machine-wide lock for every timing-sensitive test in this crate: the
//! unit suites here (`process_pipeline_tests`, `real_stall_tests`) and the
//! integration suites that drive a real `plugin-server`
//! (`tests/support/clap_probe.rs`'s `cross_process_lock`, the same path).
//!
//! # Why a directory, not a `static Mutex`
//!
//! `cargo nextest` runs every test in its own **process**, so a process-local
//! `Mutex` is uncontended in each one and serializes nothing. That is how
//! `process_pipeline_tests` came to run beside the plugin-fork suites
//! (`tests/clap_fork.rs`), whose hung-server cases keep a `plugin-server`
//! spinning a core for a second at a time. The pipeline tests' bridge answers
//! each block within a few milliseconds (`dispatch::process_timeout`), so a
//! core taken away mid-run turns an on-time reply into an abandoned one — and
//! `a_timed_out_reply_is_drained_rather_than_paired_with_a_later_block`,
//! which counts abandoned replies exactly, read 2 where it staged 1 (macOS CI,
//! three cores). `create_dir` is atomic across processes and fails with
//! `AlreadyExists` on every OS this builds for.
//!
//! A stale lock (a holder that crashed) is stolen after [`STALE_AFTER`],
//! rather than hung on: no OS cleans a directory up the way it drops a
//! `flock`.

use std::path::PathBuf;
use std::time::{Duration, Instant};

/// The path every suite agrees on. The integration suites spell the same
/// literal in `tests/support/clap_probe.rs` (they cannot name this module); a
/// change here must change it there.
pub(crate) fn path() -> PathBuf {
    std::env::temp_dir().join("tutti-plugin-clap-probe.lock")
}

/// How long to wait before assuming the holder died. The integration suites'
/// figure: long enough for their slowest holder (`real_plugin_pressure.rs`).
const STALE_AFTER: Duration = Duration::from_secs(120);

/// Held for one test; released on drop, including on a failing assertion's
/// unwind.
pub(crate) struct Guard;

impl Drop for Guard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir(path());
    }
}

/// Take the machine, waiting for whoever holds it.
pub(crate) fn acquire() -> Guard {
    let p = path();
    let deadline = Instant::now() + STALE_AFTER;
    loop {
        match std::fs::create_dir(&p) {
            Ok(()) => return Guard,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                if Instant::now() >= deadline {
                    let _ = std::fs::remove_dir(&p);
                    continue;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            // An unusable temp dir must not fail the suites; running
            // unserialized is what they did before this existed.
            Err(_) => return Guard,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **Held, the lock excludes another process**: what a second test
    /// process would try (`create_dir` on the same path) is refused while a
    /// guard lives, and succeeds once it drops.
    ///
    /// Mutation: make `acquire` a process-local `static Mutex` (the bug this
    /// module replaced) → nothing is created on disk → the first
    /// `create_dir` succeeds → fails.
    #[test]
    fn a_held_lock_refuses_another_process() {
        let guard = acquire();
        let err = std::fs::create_dir(path()).expect_err("the lock is held on disk");
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
        drop(guard);
        std::fs::create_dir(path()).expect("released on drop");
        std::fs::remove_dir(path()).expect("clean up");
    }
}
