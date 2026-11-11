//! Environment variables a test sets for the process it runs in.

use std::ffi::OsStr;

/// Environment variables a test set, unset again when the guard drops.
///
/// Setting the environment is `unsafe` because another thread of the process
/// may be reading it. This is the one place the suites do it, and it is safe to
/// call on the terms every caller is under: nextest runs each test in a
/// process of its own, and a test sets its variables before it starts a thread
/// or a plugin that reads them. A test that shares variables with tests in
/// other processes (the probe's switches) also holds the
/// [`machine_lock`](fn@crate::machine_lock).
#[derive(Debug, Default)]
pub struct ScopedEnv(Vec<String>);

impl ScopedEnv {
    /// A guard that has set nothing yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets `key` to `value` until the guard drops.
    pub fn set(&mut self, key: &str, value: impl AsRef<OsStr>) {
        #[allow(
            unsafe_code,
            reason = "the suites' one environment write; see the type"
        )]
        // SAFETY: no other thread of this process reads the environment while
        // a test sets it (the type's doc: one test per process, set before any
        // thread starts).
        unsafe {
            std::env::set_var(key, value);
        }
        self.0.push(key.to_owned());
    }
}

impl Drop for ScopedEnv {
    fn drop(&mut self) {
        for key in &self.0 {
            #[allow(unsafe_code, reason = "undoes `set`; see the type")]
            // SAFETY: as in `set`: the threads a test started have stopped
            // reading the environment by the time its guard drops.
            unsafe {
                std::env::remove_var(key);
            }
        }
    }
}
