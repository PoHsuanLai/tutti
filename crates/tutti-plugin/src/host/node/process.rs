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
use std::process::Child;

pub(crate) struct ProcessGuard {
    process: Option<Child>,
    _bridge_thread: Option<BridgeThread>,
    config: BridgeConfig,
}

impl ProcessGuard {
    pub(crate) fn new(process: Child, bridge_thread: BridgeThread, config: BridgeConfig) -> Self {
        Self {
            process: Some(process),
            _bridge_thread: Some(bridge_thread),
            config,
        }
    }

    /// Test-only: create a guard without a real subprocess. Used by
    /// `PluginHandle::from_bridge_and_metadata` in mock-server tests.
    #[cfg(test)]
    pub(crate) fn for_test(config: BridgeConfig) -> Self {
        Self {
            process: None,
            _bridge_thread: None,
            config,
        }
    }
}

impl Drop for ProcessGuard {
    fn drop(&mut self) {
        if let Some(mut process) = self.process.take() {
            // Skip wait() if kill() failed to avoid hanging on an
            // unkillable process.
            if process.kill().is_ok() {
                let _ = process.wait();
            }
        }
        let _ = std::fs::remove_file(&self.config.socket_path);
    }
}
