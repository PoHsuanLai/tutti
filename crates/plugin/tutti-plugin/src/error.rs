use std::path::PathBuf;
use thiserror::Error;

/// Plugin-load phase label, shared with the format-specific host crates via
/// `tutti-plugin-types`. Re-exported so `BridgeError` and callers keep
/// referring to `crate::error::LoadStage`.
pub use tutti_plugin_types::LoadStage;

/// Structured editor-open failures, shared via `tutti-plugin-types`.
/// Re-exported so callers keep referring to `crate::error::EditorError`.
pub use tutti_plugin_types::EditorError;

/// The lean, format-agnostic error the shared `PluginFormatHost` trait speaks.
/// Re-exported so callers keep referring to `crate::error::PluginError`.
pub use tutti_plugin_types::PluginError;

#[derive(Error, Debug)]
pub enum BridgeError {
    #[error("Bridge connection failed: {0}")]
    ConnectionFailed(String),

    #[error("Plugin load failed at {stage} stage: {path}\n  Reason: {reason}")]
    LoadFailed {
        path: PathBuf,
        stage: LoadStage,
        reason: String,
    },

    #[error("Plugin error at {stage}: code {code:#x}")]
    PluginError { stage: LoadStage, code: i32 },

    #[error("Could not resolve plugin bundle to a binary: {path}")]
    BundleResolutionFailed { path: PathBuf },

    #[error(
        "plugin-server binary not found. Build it with `cargo build -p tutti-plugin-server` \
         and either place it next to the application binary or set \
         TUTTI_PLUGIN_SERVER=/path/to/plugin-server"
    )]
    ServerNotFound,

    #[error("No plugin named {name:?} in catalog")]
    PluginNotFound { name: String },

    #[error("Unexpected bridge message: expected {expected}, got {got}")]
    UnexpectedMessage { expected: &'static str, got: String },

    #[error("Plugin protocol version mismatch: host speaks {expected}, subprocess speaks {got} (rebuild the plugin-server)")]
    ProtocolMismatch { expected: u32, got: u32 },

    #[error("IPC error: {0}")]
    IpcError(String),

    #[error("Shared memory error: {0}")]
    SharedMemoryError(String),

    #[error("Timeout after {duration_ms}ms: {operation}")]
    Timeout { operation: String, duration_ms: u64 },

    #[error("Bridge process crashed")]
    ProcessCrashed,

    #[error("Failed to save plugin state: {0}")]
    StateSaveError(String),

    #[error("Failed to restore plugin state: {0}")]
    StateRestoreError(String),

    #[error("Plugin process error: {0}")]
    ProcessError(String),

    #[error("Plugin editor error: {0}")]
    EditorError(String),

    #[error("Protocol error: {0}")]
    ProtocolError(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Serialization error: {0}")]
    Serialization(#[from] bincode::Error),
}

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
