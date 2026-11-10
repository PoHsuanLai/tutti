//! One machine-wide lock for every timing-sensitive plugin test: what one test
//! process holds while it drives a real `plugin-server`, or depends on
//! answers within a few milliseconds, so two such tests never run at once.
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

/// The path every test process agrees on.
fn path() -> PathBuf {
    std::env::temp_dir().join("tutti-plugin-clap-probe.lock")
}

/// How long to wait before assuming the holder died: long enough for the
/// slowest holder (`tutti-plugin`'s `real_plugin_pressure.rs`, which spawns up
/// to eight servers paced to a 1.333 ms block, slower again on a shared CI
/// runner). Being generous costs only how long a wedged holder delays the next
/// test; being tight steals the lock from a live one.
const STALE_AFTER: Duration = Duration::from_secs(120);

/// Held for one test; released on drop, including on a failing assertion's
/// unwind.
#[must_use = "the lock is released when this drops"]
#[derive(Debug)]
pub struct MachineLock(Option<PathBuf>);

impl Drop for MachineLock {
    fn drop(&mut self) {
        if let Some(p) = &self.0 {
            let _ = std::fs::remove_dir(p);
        }
    }
}

/// Take the machine, waiting for whoever holds it.
pub fn machine_lock() -> MachineLock {
    acquire_at(&path())
}

/// [`machine_lock`] on a given path, so a test can exercise the protocol on a
/// private directory instead of racing every other process for the real one.
fn acquire_at(p: &std::path::Path) -> MachineLock {
    let p = p.to_path_buf();
    let deadline = Instant::now() + STALE_AFTER;
    loop {
        match std::fs::create_dir(&p) {
            Ok(()) => return MachineLock(Some(p)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                if Instant::now() >= deadline {
                    let _ = std::fs::remove_dir(&p);
                    continue;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            // An unusable temp dir must not fail the suites; fall back to
            // running unserialized.
            Err(_) => return MachineLock(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **Held, the lock excludes another process**: what a second test
    /// process would try (`create_dir` on the same path) is refused while a
    /// guard lives, and the path is free once it drops. It runs on a private
    /// path: the machine-wide one is contended by every other test process
    /// (and other worktrees), so a raw `create_dir` or `remove_dir` on it
    /// would race them and could delete a lock another process holds.
    ///
    /// Mutation: make `acquire_at` a process-local `static Mutex` (the bug this
    /// module replaced) → nothing is created on disk → the first
    /// `create_dir` succeeds → fails.
    #[test]
    fn a_held_lock_refuses_another_process() {
        let dir = tempfile::tempdir().expect("a private directory");
        let p = dir.path().join("lock");
        let guard = acquire_at(&p);
        let err = std::fs::create_dir(&p).expect_err("the lock is held on disk");
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
        drop(guard);
        std::fs::create_dir(&p).expect("released on drop");
    }

    /// The real path round-trips: `acquire` waits for any holder, then
    /// releases on drop (without touching the shared path raw).
    #[test]
    fn the_machine_wide_lock_can_be_taken_again_after_a_release() {
        drop(machine_lock());
        drop(machine_lock());
    }
}
