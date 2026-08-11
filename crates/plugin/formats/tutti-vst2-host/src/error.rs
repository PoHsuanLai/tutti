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
    /// The plugin could not be loaded. `stage` says how far the load got, which
    /// separates "the file is not a VST2 binary" from "the plugin loaded and
    /// then refused to initialise".
    #[error("Failed to load plugin at {path}: {stage} - {reason}")]
    LoadFailed {
        /// The binary the load was attempted against.
        path: PathBuf,
        /// The phase that failed — Opening, Factory, Instantiation or
        /// Initialization for VST2.
        stage: LoadStage,
        /// Human-readable cause, for logs rather than for matching on.
        reason: String,
    },

    /// Opening, embedding, resizing or closing the plugin's native editor
    /// failed.
    #[error("Editor error: {0}")]
    EditorError(String),

    /// The plugin rejected a state chunk handed to `effSetChunk`, or the chunk
    /// was malformed for this plugin.
    #[error("State restore error: {0}")]
    StateRestoreError(String),
}
