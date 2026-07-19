//! Error types for VST2 plugin hosting.

use std::path::PathBuf;
use thiserror::Error;

/// Convenience alias for results returned by this crate.
pub type Result<T> = std::result::Result<T, Vst2Error>;

/// Plugin-load phase label. The shared superset lives in `tutti-plugin-types`;
/// VST2 uses the Opening/Factory/Instantiation/Initialization subset (no
/// distinct Scanning, Setup, or Activation phase). Re-exported so `Vst2Error`
/// and callers keep referring to `crate::error::LoadStage`.
pub use tutti_plugin_types::LoadStage;

/// All error conditions reported by the VST2 host.
#[derive(Debug, Error)]
pub enum Vst2Error {
    #[error("Failed to load plugin at {path}: {stage} - {reason}")]
    LoadFailed {
        path: PathBuf,
        stage: LoadStage,
        reason: String,
    },

    #[error("Editor error: {0}")]
    EditorError(String),

    #[error("State save error: {0}")]
    StateSaveError(String),

    #[error("State restore error: {0}")]
    StateRestoreError(String),

    #[error("Processing error: {0}")]
    ProcessError(String),
}
