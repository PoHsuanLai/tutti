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

    #[test]
    fn test_load_stage_display() {
        assert_eq!(LoadStage::Scanning.to_string(), "scanning");
        assert_eq!(LoadStage::Opening.to_string(), "opening library");
        assert_eq!(LoadStage::Factory.to_string(), "getting factory");
        assert_eq!(LoadStage::Instantiation.to_string(), "creating instance");
        assert_eq!(
            LoadStage::Initialization.to_string(),
            "initializing processor"
        );
        assert_eq!(LoadStage::Setup.to_string(), "setting up audio");
        assert_eq!(LoadStage::Activation.to_string(), "activating");
    }

    #[test]
    fn test_bridge_error_display() {
        let err = BridgeError::ConnectionFailed("timeout".to_string());
        assert!(err.to_string().contains("timeout"));

        let err = BridgeError::Timeout {
            operation: "load".to_string(),
            duration_ms: 5000,
        };
        assert!(err.to_string().contains("5000ms"));
        assert!(err.to_string().contains("load"));

        let err = BridgeError::ProcessCrashed;
        assert_eq!(err.to_string(), "Bridge process crashed");
    }

    #[test]
    fn test_state_and_editor_errors() {
        let err = BridgeError::StateSaveError("failed to serialize".into());
        assert!(err.to_string().contains("save"));
        assert!(err.to_string().contains("failed to serialize"));

        let err = BridgeError::StateRestoreError("corrupt data".into());
        assert!(err.to_string().contains("restore"));
        assert!(err.to_string().contains("corrupt data"));

        let err = BridgeError::EditorError("no window handle".into());
        assert!(err.to_string().contains("editor"));
        assert!(err.to_string().contains("no window handle"));
    }
}
