//! Subprocess lifetime guard.
//!
//! [`ProcessGuard`] owns the `plugin-server` child process and its
//! bridge thread. `Drop` kills the subprocess and removes the socket
//! file. Held behind an `Arc` inside [`super::PluginClient`] and
//! [`PluginHandle`](tutti_plugin_host::PluginHandle) — the subprocess dies when the LAST Arc
//! drops (i.e., after both the graph has released the node *and* every
//! user-held handle has dropped).

use std::process::ExitStatus;
use std::sync::{Mutex, PoisonError};
use tutti_plugin_ipc::BridgeConfig;
use tutti_plugin_ipc::BridgeThread;
use tutti_plugin_ipc::Reaped;

pub(crate) struct ProcessGuard {
    /// Behind a lock only so [`exited`](Self::exited) can ask from `&self`
    /// (`try_wait` takes `&mut`). Never locked on the audio thread.
    process: Mutex<Option<Reaped>>,
    _bridge_thread: Option<BridgeThread>,
    config: BridgeConfig,
}

impl ProcessGuard {
    /// Own the launched `process`; it is killed and reaped when the guard
    /// drops ([`Reaped`]).
    pub(crate) fn launched(process: Reaped, config: BridgeConfig) -> Self {
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
            .map(|process| process.id())
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

    /// Kills the server without waiting for it, so the bridge thread reads an
    /// EOF and the socket closes. [`Drop`] reaps it. Not for the audio thread
    /// (a lock and a syscall).
    pub(crate) fn terminate(&self) {
        let mut process = self.process.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(process) = process.as_mut() {
            let _ = process.kill();
        }
    }

    /// Test-only: create a guard without a real subprocess. Used by
    /// `handle_over` in the mock-server tests.
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
        // Killed and reaped as it drops ([`Reaped`]), before the socket goes.
        drop(process.take());
        let _ = std::fs::remove_file(&self.config.socket_path);
    }
}
