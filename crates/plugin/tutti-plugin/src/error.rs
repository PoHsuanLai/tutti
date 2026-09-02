//! The host's error type, and the conversions that widen and narrow it.
//!
//! [`BridgeError`] is the rich host-side error: it names the subprocess, the
//! shared memory and the wire, none of which a format loader knows about. The
//! lean [`PluginError`] the shared `PluginFormatHost` trait speaks is re-exported
//! here, and the two `From` impls move a value between them at the IPC boundary.

use std::path::PathBuf;
use thiserror::Error;

/// Plugin-load phase label, shared with the format-specific host crates via
/// `tutti-plugin-types`. Re-exported so `BridgeError` and callers keep
/// referring to `crate::error::LoadStage`.
pub use tutti_plugin_types::LoadStage;

/// Structured editor-open failures, shared via `tutti-plugin-types`.
/// Re-exported so callers keep referring to `crate::error::EditorError`.
pub use tutti_plugin_types::EditorError;

/// Re-exported alongside [`EditorError`], for the same reason: a caller naming
/// `crate::error::StateError` should not have to know which crate defines it.
pub use tutti_plugin_types::{Delivered, StateError};

/// The lean, format-agnostic error the shared `PluginFormatHost` trait speaks.
/// Re-exported so callers keep referring to `crate::error::PluginError`.
pub use tutti_plugin_types::PluginError;

/// Anything that can go wrong hosting a plugin out of process.
///
/// Covers both the plugin's own failures and the bridge's — the subprocess, the
/// socket and the shared-memory slab are all represented, which is what
/// distinguishes this from the format-agnostic [`PluginError`].
#[derive(Error, Debug)]
pub enum BridgeError {
    /// The host could not reach the subprocess over its control socket.
    #[error("Bridge connection failed: {0}")]
    ConnectionFailed(String),

    /// The plugin failed to load, with the stage it reached before failing.
    #[error("Plugin load failed at {stage} stage: {path}\n  Reason: {reason}")]
    LoadFailed {
        /// The plugin that failed to load.
        path: PathBuf,
        /// How far the load got, which narrows the cause.
        stage: LoadStage,
        /// Human-readable reason, from the loader or the plugin itself.
        reason: String,
    },

    /// The plugin returned a format-defined failure code.
    #[error("Plugin error at {stage}: code {code:#x}")]
    PluginError {
        /// How far the load got before the plugin refused.
        stage: LoadStage,
        /// The format's own result code, rendered in hex.
        code: i32,
    },

    /// A plugin bundle exists but holds no binary this architecture can load.
    #[error(
        "Could not resolve plugin bundle to a binary: {path}\n  \
         Probed Contents/{{{arch_subdirs}}} — the bundle may not ship a build \
         for this architecture."
    )]
    BundleResolutionFailed {
        /// The bundle that could not be resolved.
        path: PathBuf,
        /// The `Contents/<arch>` subdirectories that were probed, so a
        /// wrong-architecture bundle reads as such rather than as "corrupt".
        arch_subdirs: String,
    },

    /// The `plugin-server` executable could not be located.
    #[error(
        "plugin-server binary not found. Build it with `cargo build -p tutti-plugin-server` \
         and either place it next to the application binary or set \
         TUTTI_PLUGIN_SERVER=/path/to/plugin-server"
    )]
    ServerNotFound,

    /// No catalog entry matches the requested name.
    #[error("No plugin named {name:?} in catalog")]
    PluginNotFound {
        /// The name that was looked up.
        name: String,
    },

    /// The catalog records this plugin as having brought a scan down.
    ///
    /// Only [`Plugins::open`](crate::catalog::Plugins::open) returns this —
    /// [`Plugin::open`](crate::catalog::Plugin::open) has no catalog and so no
    /// crash history. Carrying `reason` rather than answering a bool is what
    /// lets a host say *which* plugin misbehaved and offer to load it anyway,
    /// which is the unguarded door.
    ///
    /// False positives are expected: the scanner's dead-man's pedal fires on a
    /// force-quit, a power loss, or an OOM kill as readily as on a real crash.
    /// The record stores the file's mtime, so a reinstall or vendor update
    /// re-admits the plugin without the host doing anything.
    #[error("Plugin at {path} is blacklisted: {reason}")]
    Blacklisted {
        /// The plugin the catalog refuses to open.
        path: std::path::PathBuf,
        /// Why it was blacklisted, so a host can offer to load it anyway.
        reason: String,
    },

    /// The subprocess replied with a message the host was not waiting for.
    #[error("Unexpected bridge message: expected {expected}, got {got}")]
    UnexpectedMessage {
        /// The variant name the host awaited.
        expected: &'static str,
        /// The message that actually arrived, `Debug`-formatted.
        got: String,
    },

    /// Host and subprocess speak different wire versions and refuse to proceed.
    #[error("Plugin protocol version mismatch: host speaks {expected}, subprocess speaks {got} (rebuild the plugin-server)")]
    ProtocolMismatch {
        /// The host's [`PROTOCOL_VERSION`](crate::protocol::PROTOCOL_VERSION).
        expected: u32,
        /// The version the subprocess reported at handshake.
        got: u32,
    },

    /// The control channel to the subprocess failed.
    #[error("IPC error: {0}")]
    IpcError(String),

    /// The shared-memory audio slab could not be created, mapped or validated.
    #[error("Shared memory error: {0}")]
    SharedMemoryError(String),

    /// The subprocess did not answer within the operation's deadline.
    #[error("Timeout after {duration_ms}ms: {operation}")]
    Timeout {
        /// What the host was waiting on.
        operation: String,
        /// How long it waited, in milliseconds.
        duration_ms: u64,
        /// Whether the deadline expired **part-way through a frame**, leaving
        /// bytes of it consumed.
        ///
        /// This is the difference between a timeout a caller may resume from and
        /// one it may not. Nothing consumed means the stream is still on a frame
        /// boundary and the next read starts a whole message; a partial frame
        /// means the remaining bytes will be misread as a length prefix, and
        /// every later frame with them. The desynchronisation is silent at the
        /// point it happens and surfaces later as a decode error, so the flag
        /// has to be carried out from the only place that knows: the read loop.
        partial: bool,
    },

    /// The subprocess died. Any plugin state it held is gone.
    #[error("Bridge process crashed")]
    ProcessCrashed,

    /// The plugin could not serialize its state.
    #[error("Failed to save plugin state: {0}")]
    StateSaveError(String),

    /// The plugin rejected a state chunk it was asked to restore.
    #[error("Failed to restore plugin state: {0}")]
    StateRestoreError(String),

    /// The plugin failed while processing audio.
    #[error("Plugin process error: {0}")]
    ProcessError(String),

    /// The plugin's editor could not be opened, embedded or closed.
    #[error("Plugin editor error: {0}")]
    EditorError(String),

    /// A message could not be framed, encoded or decoded.
    #[error("Protocol error: {0}")]
    ProtocolError(String),

    /// An underlying I/O operation failed.
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    /// A wire message could not be bincode-encoded or -decoded.
    #[error("Serialization error: {0}")]
    Serialization(#[from] bincode::Error),
}

/// A host operation's result, erroring as [`BridgeError`].
pub type Result<T> = std::result::Result<T, BridgeError>;

/// Widen the lean, format-agnostic [`PluginError`] (what the shared
/// `PluginFormatHost` trait returns) into the host's richer `BridgeError` at
/// the IPC boundary. Lets the pipeline/session `?` a trait-method result inside
/// a `BridgeError`-returning function without losing the message.
impl From<PluginError> for BridgeError {
    fn from(e: PluginError) -> Self {
        match e {
            PluginError::Load { stage, reason } => BridgeError::LoadFailed {
                path: PathBuf::new(),
                stage,
                reason,
            },
            PluginError::State(msg) => BridgeError::StateSaveError(msg),
            PluginError::Process(msg) => BridgeError::ProcessError(msg),
            PluginError::Editor(err) => BridgeError::EditorError(err.to_string()),
            PluginError::Other(msg) => BridgeError::ProcessError(msg),
        }
    }
}

/// Narrow a `BridgeError` into the lean [`PluginError`] the shared trait
/// speaks. The reverse of [`From<PluginError>`](BridgeError) — used inside the
/// format loaders' trait-method bodies, whose helpers still produce
/// `BridgeError`, so the value can flow out as a `PluginError`. The IPC-only
/// `BridgeError` variants (`ServerNotFound`, `ProtocolMismatch`, `IpcError`, …)
/// collapse into `PluginError::Other`, preserving their `Display` text.
impl From<BridgeError> for PluginError {
    fn from(e: BridgeError) -> Self {
        match e {
            BridgeError::LoadFailed { stage, reason, .. } => PluginError::Load { stage, reason },
            BridgeError::StateSaveError(msg) | BridgeError::StateRestoreError(msg) => {
                PluginError::State(msg)
            }
            BridgeError::ProcessError(msg) => PluginError::Process(msg),
            BridgeError::EditorError(msg) => PluginError::Editor(EditorError::PluginError(msg)),
            other => PluginError::Other(other.to_string()),
        }
    }
}

impl BridgeError {
    /// Build an `UnexpectedMessage` error by Debug-formatting the received
    /// `BridgeMessage`. Centralizes the three call sites in `client/` that
    /// would otherwise each hand-roll a `format!` + `ProtocolError`.
    pub(crate) fn unexpected_message(
        expected: &'static str,
        got: &crate::protocol::BridgeMessage,
    ) -> Self {
        Self::UnexpectedMessage {
            expected,
            got: format!("{got:?}"),
        }
    }

    /// Turn a server-reported error message into a `LoadFailed` at the
    /// `Opening` stage. The stage is coarse because the server's own
    /// error-message protocol doesn't carry a stage tag.
    pub(crate) fn load_from_server(path: &std::path::Path, message: String) -> Self {
        Self::LoadFailed {
            path: path.to_path_buf(),
            stage: LoadStage::Opening,
            reason: message,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every `PluginError` a trait method can return must survive the widening
    /// into `BridgeError` with its payload intact. The load stage matters most:
    /// it is what the host reports to the user, and the conversion rebuilds the
    /// variant field by field rather than forwarding it.
    #[test]
    fn widening_a_plugin_error_keeps_its_payload() {
        let widened = BridgeError::from(PluginError::Load {
            stage: LoadStage::Factory,
            reason: "no factory entry point".into(),
        });
        match widened {
            BridgeError::LoadFailed { stage, reason, .. } => {
                assert_eq!(stage, LoadStage::Factory);
                assert_eq!(reason, "no factory entry point");
            }
            other => panic!("expected LoadFailed, got {other:?}"),
        }

        assert!(matches!(
            BridgeError::from(PluginError::Process("underrun".into())),
            BridgeError::ProcessError(m) if m == "underrun"
        ));
        assert!(matches!(
            BridgeError::from(PluginError::State("truncated".into())),
            BridgeError::StateSaveError(m) if m == "truncated"
        ));

        // The editor arm is the lossy one going out: `BridgeError` has no
        // structured editor error, so the typed variant is flattened with
        // `to_string()` and only its Display text crosses. Assert against the
        // variant's own Display rather than a copied literal, so rewording the
        // `#[error(...)]` cannot leave this passing on a stale string.
        let crashed = EditorError::PluginCrashed;
        match BridgeError::from(PluginError::Editor(crashed)) {
            BridgeError::EditorError(msg) => {
                assert_eq!(msg, EditorError::PluginCrashed.to_string())
            }
            other => panic!("expected EditorError, got {other:?}"),
        }
        // A variant carrying a field must keep that field's text in the message.
        match BridgeError::from(PluginError::Editor(EditorError::GuiNotSupported {
            format: "vst3".into(),
        })) {
            BridgeError::EditorError(msg) => assert!(
                msg.contains("vst3"),
                "the flattened message dropped the format: {msg:?}"
            ),
            other => panic!("expected EditorError, got {other:?}"),
        }
    }

    /// Narrowing back is deliberately lossy in two places, and both are the
    /// point of the test rather than an accident to be tolerated.
    #[test]
    fn narrowing_a_bridge_error_collapses_only_where_it_must() {
        // A load error round-trips: stage and reason are the two fields the
        // lean type also carries.
        let narrowed = PluginError::from(BridgeError::LoadFailed {
            path: PathBuf::from("/x.vst3"),
            stage: LoadStage::Activation,
            reason: "refused".into(),
        });
        match narrowed {
            PluginError::Load { stage, reason } => {
                assert_eq!(stage, LoadStage::Activation);
                assert_eq!(reason, "refused");
            }
            other => panic!("expected Load, got {other:?}"),
        }

        // Save and restore are two BridgeError variants but one PluginError
        // variant, so the direction of a state failure is not recoverable from
        // the narrowed value — only its message is.
        for e in [
            BridgeError::StateSaveError("boom".into()),
            BridgeError::StateRestoreError("boom".into()),
        ] {
            assert!(
                matches!(PluginError::from(e), PluginError::State(m) if m == "boom"),
                "both state directions must narrow to PluginError::State"
            );
        }

        // The IPC-only variants have no lean counterpart at all and collapse
        // into `Other` — carrying their Display text, which is the only thing
        // that survives and so the only thing worth asserting.
        let crashed = PluginError::from(BridgeError::ProcessCrashed);
        match crashed {
            PluginError::Other(msg) => assert_eq!(msg, BridgeError::ProcessCrashed.to_string()),
            other => panic!("expected Other, got {other:?}"),
        }
        assert!(matches!(
            PluginError::from(BridgeError::ServerNotFound),
            PluginError::Other(_)
        ));

        // Coming back, every editor failure re-enters as
        // `EditorError::PluginError` regardless of what it was on the way out —
        // the round trip is not idempotent, and this is where that is visible.
        // `PluginCrashed` in, `PluginError(<its text>)` out.
        let there_and_back = PluginError::from(BridgeError::from(PluginError::Editor(
            EditorError::PluginCrashed,
        )));
        match there_and_back {
            PluginError::Editor(EditorError::PluginError(msg)) => {
                assert_eq!(msg, EditorError::PluginCrashed.to_string());
            }
            other => panic!("expected Editor(PluginError), got {other:?}"),
        }
    }
}
