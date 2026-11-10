//! A child process that cannot outlive its owner.

use std::ops::{Deref, DerefMut};
use std::process::Child;

/// A child process that is killed and reaped when it drops.
///
/// std's [`Child`] has no `Drop`: a child dropped on an error path, or by a
/// panic between spawn and wait, keeps running, and once it exits it is a
/// zombie until this process ends. Every child the plugin crates spawn (the
/// plugin-server, the probe) is held as one of these from the moment it is
/// spawned, so no path out of the code that holds it leaves one behind
/// (doc 027 D11).
///
/// It derefs to the [`Child`], so `id`, `try_wait`, `kill` and `wait` are
/// std's own. A child that already exited (or was reaped through the deref)
/// is dropped harmlessly: `kill` answers `Ok` and `wait` its status at once.
#[derive(Debug)]
pub struct Reaped(Child);

impl Reaped {
    /// Own `child` from now on.
    #[must_use]
    pub fn new(child: Child) -> Self {
        Self(child)
    }
}

impl Deref for Reaped {
    type Target = Child;

    fn deref(&self) -> &Child {
        &self.0
    }
}

impl DerefMut for Reaped {
    fn deref_mut(&mut self) -> &mut Child {
        &mut self.0
    }
}

impl Drop for Reaped {
    fn drop(&mut self) {
        // No `wait` when the kill failed: a process this one cannot signal
        // could block the reap forever, and a leaked process is the lesser harm.
        if self.0.kill().is_ok() {
            let _ = self.0.wait();
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};

    fn exists(pid: u32) -> bool {
        Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success()
    }

    /// **A child is killed and reaped when its guard unwinds**: a panic
    /// between spawn and wait leaves no running process and no zombie (a
    /// zombie still answers `kill -0`). Moved here with the guard from
    /// `tutti-plugin`'s probe, which had its own copy.
    ///
    /// Mutations (run), each fails: `drop` doing nothing (the child runs on);
    /// `drop` killing without `wait` (the zombie answers).
    #[test]
    fn a_child_is_killed_and_reaped_when_its_guard_unwinds() {
        let child = Command::new("sleep").arg("30").spawn().unwrap();
        let pid = child.id();
        let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = Reaped::new(child);
            panic!("between spawn and wait");
        }));
        assert!(unwound.is_err());
        assert!(!exists(pid), "child {pid} survived its guard");
    }
}
