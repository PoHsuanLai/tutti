//! Error types returned by plugin loading and processing.

use std::path::PathBuf;
use thiserror::Error;

/// Plugin-load phase label. The shared superset lives in `tutti-plugin-types`;
/// VST3 uses every phase. Re-exported so `Vst3Error` and downstream callers
/// keep referring to `crate::error::LoadStage`.
pub use tutti_plugin_types::LoadStage;

/// Convenience alias for `Result<T, Vst3Error>`.
pub type Result<T> = std::result::Result<T, Vst3Error>;

/// Errors produced while loading, initializing, or driving a VST3 plugin.
#[derive(Error, Debug)]
pub enum Vst3Error {
    /// A file-system or factory-level failure that aborted loading before the
    /// plugin could be instantiated. `stage` pinpoints which step failed.
    #[error("Failed to load plugin at {path}: {stage} - {reason}")]
    LoadFailed {
        /// Bundle or DSO path the host attempted to load.
        path: PathBuf,
        /// Which loading step failed.
        stage: LoadStage,
        /// Human-readable cause, for logs rather than matching.
        reason: String,
    },

    /// A plugin call returned a non-OK `tresult`. `code` is the raw VST3 return
    /// code as defined in `pluginterfaces/base/funknown.h`.
    #[error("Plugin error at {stage}: code {code}")]
    PluginError {
        /// Which step the failing call belonged to.
        stage: LoadStage,
        /// The raw VST3 `tresult`, passed through unmapped so a caller can
        /// distinguish the SDK's specific codes.
        code: i32,
    },

    /// Operation requires the plugin to be in the active (processing) state.
    #[error("Plugin is not active")]
    NotActive,

    /// Feature requested by the host is not supported by this plugin (e.g.
    /// 64-bit processing, an editor view).
    #[error("Feature not supported: {0}")]
    NotSupported(String),

    /// Parameter index, id, or value fell outside the plugin's allowed range.
    #[error("Invalid parameter: {0}")]
    InvalidParameter(String),

    /// Saving or restoring plugin state failed — truncated data, mismatched
    /// format, or the plugin rejected the stream.
    #[error("State error: {0}")]
    StateError(String),
}
