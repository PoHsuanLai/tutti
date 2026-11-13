//! The fallback backend of the main loop, for a platform that is neither unix nor
//! Windows: nothing to wait on or to key liveness on, so a read blocks.

use crate::transport::Transport;
use tutti_plugin_types::{BridgeResult, MainThread};

pub(crate) type Listener = interprocess::local_socket::Listener;
pub(crate) type Stream = interprocess::local_socket::Stream;

/// The reactor's window-system half. Nothing to pump here.
#[derive(Debug, Default)]
pub(crate) struct Native;

impl Native {
    /// Waits until `transport`'s socket is readable (`Ok(true)`) or `timeout`
    /// passes (`Ok(false)`), then pumps the window system's events.
    ///
    /// The socket's wait is the transport's ([`wait_readable`] for a real
    /// socket), so a test's scripted transport drives the same loop.
    ///
    /// # Errors
    ///
    /// The transport's wait fails.
    pub(crate) fn wait(
        &mut self,
        transport: &mut dyn Transport,
        main: &MainThread,
        timeout: std::time::Duration,
    ) -> BridgeResult<bool> {
        let readable = transport.wait_readable(timeout)?;
        self.pump(main);
        Ok(readable)
    }

    /// Pumps the window system's events: none yet.
    fn pump(&mut self, _main: &MainThread) {}
}

/// Nothing to wait on: let the read block.
///
/// # Errors
///
/// Never.
pub(crate) fn wait_readable(_stream: &Stream, _timeout: std::time::Duration) -> BridgeResult<bool> {
    Ok(true)
}

/// Neither Unix nor Windows: nothing to key on, so wait in `accept` as before.
pub(crate) fn wait_for_connection(_listener: &Listener) -> BridgeResult<()> {
    Ok(())
}

/// Nothing is recorded.
pub(crate) fn record_parent_pid() {}
