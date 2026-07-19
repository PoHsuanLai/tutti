//! Error types for CLAP plugin hosting.

use std::path::PathBuf;
use thiserror::Error;

/// Convenience alias for results returned by this crate.
pub type Result<T> = std::result::Result<T, ClapError>;

/// Plugin-load phase label. The shared superset lives in `tutti-plugin-types`;
/// CLAP uses the Opening/Factory/Instantiation/Initialization/Activation
/// subset (no distinct Scanning or Setup phase). Re-exported so `ClapError`
/// and callers keep referring to `crate::error::LoadStage`.
pub use tutti_plugin_types::LoadStage;

/// All error conditions reported by the CLAP host.
#[derive(Debug, Error)]
pub enum ClapError {
    /// The plugin could not be loaded. `stage` pinpoints which step failed.
    #[error("Failed to load plugin at {path}: {stage} - {reason}")]
    LoadFailed {
        path: PathBuf,
        stage: LoadStage,
        reason: String,
    },

    /// Audio processing failed — the plugin returned `CLAP_PROCESS_ERROR`
    /// or a 64-bit buffer was passed to a 32-bit-only plugin.
    #[error("Processing error: {0}")]
    ProcessError(String),

    /// Saving or loading plugin state failed.
    #[error("State error: {0}")]
    StateError(String),

    /// An operation that requires an active plugin was called on an
    /// inactive instance.
    #[error("Plugin not activated")]
    NotActivated,

    /// A requested capability is not advertised by the plugin (e.g. activating
    /// as `ClapActive<f64>` when the plugin is 32-bit only).
    #[error("Not supported: {0}")]
    NotSupported(String),

    /// A parameter ID or value was rejected by the plugin.
    #[error("Invalid parameter: {0}")]
    InvalidParameter(String),

    /// Editor/GUI creation, resize, or teardown failed.
    #[error("GUI error: {0}")]
    GuiError(String),

    /// Underlying IO failure (file system, stream).
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}
