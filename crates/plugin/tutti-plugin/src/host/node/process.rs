//! Subprocess lifetime guard.
//!
//! [`ProcessGuard`] owns the `plugin-server` child process and its
//! bridge thread. `Drop` kills the subprocess and removes the socket
//! file. Held behind an `Arc` inside [`super::PluginClient`] and
//! [`crate::host::handles::PluginHandle`] — the subprocess dies when the LAST Arc
//! drops (i.e., after both the fundsp graph has released the AudioUnit
//! *and* every user-held handle has dropped).

use crate::host::ipc_client::audio::BridgeThread;
use crate::util::config::BridgeConfig;
use std::process::{Child, ExitStatus};
use std::sync::{Mutex, PoisonError};

pub(crate) struct ProcessGuard {
    /// Behind a lock only so [`exited`](Self::exited) can ask from `&self`
    /// (`try_wait` takes `&mut`). Never locked on the audio thread.
    process: Mutex<Option<Child>>,
    _bridge_thread: Option<BridgeThread>,
    config: BridgeConfig,
}

impl ProcessGuard {
    /// Own `process` from the moment it is launched, before anything else
    /// that can fail: a bare `Child` dropped on an error path is neither
    /// killed nor waited (std's `Child` has no `Drop`), which leaks a running
    /// server and then a zombie.
    pub(crate) fn launched(process: Child, config: BridgeConfig) -> Self {
        Self {
            process: Mutex::new(Some(process)),
            _bridge_thread: None,
            config,
        }
    }

    /// The OS process id of the guarded `plugin-server`, or `None` for a
    /// test guard that owns no subprocess.
    pub(crate) fn pid(&self) -> Option<u32> {
        self.process
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .map(Child::id)
    }

    /// Hand over the bridge thread once it exists; it shuts down with the
    /// process.
    pub(crate) fn attach(&mut self, bridge_thread: BridgeThread) {
        self._bridge_thread = Some(bridge_thread);
    }

    /// The server's exit status if it has exited, without blocking.
    ///
    /// The bridge notices a dead server only when it next reads the socket,
    /// which it does only for a command. An offline fork waiting for a block
    /// the dead server will never publish sends nothing while it waits, so
    /// it asks the process instead. Not for the audio thread (a lock and a
    /// syscall).
    pub(crate) fn exited(&self) -> Option<ExitStatus> {
        let mut process = self.process.lock().unwrap_or_else(PoisonError::into_inner);
        process.as_mut()?.try_wait().ok().flatten()
    }

    /// Test-only: create a guard without a real subprocess. Used by
    /// `PluginHandle::from_bridge_and_metadata` in mock-server tests.
    #[cfg(test)]
    pub(crate) fn for_test(config: BridgeConfig) -> Self {
        Self {
            process: Mutex::new(None),
            _bridge_thread: None,
            config,
        }
    }
}

impl Drop for ProcessGuard {
    fn drop(&mut self) {
        let process = self
            .process
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(mut process) = process.take() {
            // Skip wait() if kill() failed to avoid hanging on an
            // unkillable process. (A child `exited` already reaped answers
            // `kill` with `Ok` and `wait` with its status at once.)
            if process.kill().is_ok() {
                let _ = process.wait();
            }
        }
        let _ = std::fs::remove_file(&self.config.socket_path);
    }
}
