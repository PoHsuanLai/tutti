//! [`PluginFailure`], why a hosted plugin is dead or is being replaced.
//!
//! The bridge latches one at the site that notices a death, and the same value
//! reaches every reader: `Health::crash_cause`, `PluginInvalidation::Crashed`,
//! the host's `PluginStatus::Dead`, and the session's `PluginChange::Died`
//! and (inside its `ReloadCause`) `Reloading` and `GaveUp`. It holds only what a
//! backend or the host latches; why a *replacement* failed is the session's
//! own `RecoveryFailure`. It is `Clone` (a latch is read many times and an
//! event outlives the call that raised it), so an error that is not `Clone`
//! (`BridgeError` holds an `io::Error`) is held in an `Arc` and reached as
//! [`source`](std::error::Error::source).

use std::sync::Arc;
use std::time::Duration;

use thiserror::Error;

use crate::BridgeError;

/// Why a hosted plugin is dead.
///
/// The variants from [`Connect`](Self::Connect) to [`Bridge`](Self::Bridge) are
/// a served plugin's death, latched by the bridge; [`Terminated`](Self::Terminated)
/// is the host ending it; [`Unreported`](Self::Unreported) is a backend that
/// could not say.
///
/// A cause that came from an underlying error carries it as the variant's
/// [`source`](std::error::Error::source), not as text.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum PluginFailure {
    /// The bridge thread could not connect to the plugin-server's realtime
    /// socket: the plugin never ran.
    Connect {
        /// What the connect returned.
        source: Arc<BridgeError>,
    },
    /// The server's handshake on the realtime socket failed: it sent no
    /// `Ready`, sent something else first, or spoke another protocol version
    /// ([`BridgeError::ProtocolMismatch`]).
    Handshake {
        /// What the handshake read.
        source: Arc<BridgeError>,
    },
    /// The control stream closed mid-session: the server is gone
    /// ([`BridgeError::ProcessCrashed`]).
    StreamClosed,
    /// An offline render found the server process had exited, with the status
    /// the OS reported.
    ServerExited {
        /// How it exited.
        status: std::process::ExitStatus,
    },
    /// The server did not answer within an operation's deadline
    /// ([`BridgeError::Timeout`]).
    TimedOut {
        /// The timeout.
        source: Arc<BridgeError>,
    },
    /// An I/O operation on the control stream failed ([`BridgeError::Io`]).
    Io {
        /// The I/O error.
        source: Arc<BridgeError>,
    },
    /// The server broke the protocol: a message the host was not waiting for,
    /// or one that would not frame, encode or decode
    /// ([`BridgeError::UnexpectedMessage`], `ProtocolError`, `Serialization`).
    Protocol {
        /// The violation.
        source: Arc<BridgeError>,
    },
    /// Any other error that ended a call on the control stream.
    Bridge {
        /// The error.
        source: Arc<BridgeError>,
    },
    /// The host ended the plugin's server.
    Terminated {
        /// Why the host did it.
        reason: TerminationReason,
    },
    /// A backend reported the plugin dead and could not say why.
    Unreported,
}

impl std::fmt::Display for PluginFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Connect { source } => write!(f, "could not connect to plugin-server: {source}"),
            Self::Handshake { source } => write!(f, "plugin-server handshake failed: {source}"),
            Self::StreamClosed => f.write_str("the plugin-server's stream closed"),
            Self::ServerExited { status } => write!(f, "plugin-server exited: {status}"),
            Self::TimedOut { source } => {
                write!(f, "the plugin-server stopped answering: {source}")
            }
            Self::Io { source } => write!(f, "the plugin-server's stream failed: {source}"),
            Self::Protocol { source } => {
                write!(f, "the plugin-server broke the protocol: {source}")
            }
            Self::Bridge { source } => {
                write!(f, "the plugin-server's connection failed: {source}")
            }
            Self::Terminated { reason } => write!(f, "the plugin-server was ended: {reason}"),
            Self::Unreported => {
                f.write_str("the plugin died for a reason its backend did not report")
            }
        }
    }
}

/// The error behind a failure is its [`source`](std::error::Error::source):
/// the [`BridgeError`] itself (not the `Arc` that shares it), so a caller
/// downcasts to the type it knows.
impl std::error::Error for PluginFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Connect { source }
            | Self::Handshake { source }
            | Self::TimedOut { source }
            | Self::Io { source }
            | Self::Protocol { source }
            | Self::Bridge { source } => Some(&**source),
            Self::Terminated { .. }
            | Self::StreamClosed
            | Self::ServerExited { .. }
            | Self::Unreported => None,
        }
    }
}

impl PluginFailure {
    /// The failure a call on the control stream ended with, from the error it
    /// returned: the one place a stream's [`BridgeError`] becomes a cause.
    #[must_use]
    pub fn from_stream(error: BridgeError) -> Self {
        match error {
            BridgeError::ProcessCrashed => Self::StreamClosed,
            BridgeError::Timeout { .. } => Self::TimedOut {
                source: Arc::new(error),
            },
            BridgeError::Io(_) => Self::Io {
                source: Arc::new(error),
            },
            BridgeError::UnexpectedMessage { .. }
            | BridgeError::ProtocolError(_)
            | BridgeError::Serialization(_) => Self::Protocol {
                source: Arc::new(error),
            },
            other => Self::Bridge {
                source: Arc::new(other),
            },
        }
    }
}

/// Why the host ended a plugin's server ([`PluginFailure::Terminated`]).
#[derive(Error, Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum TerminationReason {
    /// The plugin stayed alive but stopped answering: blocks were abandoned
    /// for `held` and no reply came.
    #[error("the plugin stopped answering ({blocks} blocks abandoned over {held:?}, no reply)")]
    Hung {
        /// Blocks the bridge gave up waiting for.
        blocks: u64,
        /// How long it had been starved.
        held: Duration,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error;

    /// A stream error keeps its kind and its error as `source`: the wire's
    /// `ProcessCrashed` is a closed stream, an I/O error is `Io` with the
    /// `BridgeError` behind it, and a timeout is `TimedOut`.
    #[test]
    fn a_stream_error_keeps_its_kind_and_its_source() {
        assert!(matches!(
            PluginFailure::from_stream(BridgeError::ProcessCrashed),
            PluginFailure::StreamClosed
        ));
        let io = PluginFailure::from_stream(BridgeError::Io(std::io::Error::other("boom")));
        assert!(matches!(io, PluginFailure::Io { .. }));
        let source = io.source().expect("an I/O failure has a source");
        assert!(matches!(
            source.downcast_ref::<BridgeError>(),
            Some(BridgeError::Io(e)) if e.to_string() == "boom"
        ));
        let late = PluginFailure::from_stream(BridgeError::Timeout {
            operation: "x".into(),
            duration_ms: 5,
            partial: false,
        });
        assert!(matches!(late, PluginFailure::TimedOut { .. }));
        let proto = PluginFailure::from_stream(BridgeError::ProtocolError("bad".into()));
        assert!(matches!(proto, PluginFailure::Protocol { .. }));
        assert!(matches!(
            PluginFailure::from_stream(BridgeError::ServerNotFound),
            PluginFailure::Bridge { .. }
        ));
    }
}
