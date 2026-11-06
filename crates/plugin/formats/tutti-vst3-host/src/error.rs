//! Error types returned by plugin loading and processing.

use std::path::PathBuf;
use thiserror::Error;

/// The loading step a [`Vst3Error`] belongs to.
///
/// Re-exported from `tutti-plugin-types`, which shares it across all plugin
/// formats; VST3 uses every phase.
pub use tutti_plugin_types::LoadStage;

/// Convenience alias for `Result<T, Vst3Error>`.
pub type Result<T> = std::result::Result<T, Vst3Error>;

/// Why a load failed before the plugin could be instantiated.
#[derive(Error, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum LoadCause {
    /// The path does not exist.
    #[error("Plugin file not found")]
    FileNotFound,
    /// The module could not be opened or its factory resolved. The text is the
    /// OS loader's or the module entry's own message, which has no structure.
    #[error("{0}")]
    Module(String),
    /// The factory exports no classes.
    #[error("VST3 factory contains no classes")]
    NoClasses,
    /// The factory exports no audio-processor class.
    #[error("No audio processor classes found in VST3")]
    NoAudioClass,
    /// The bundle has no audio class with the requested display name.
    #[error("no audio class named {wanted:?}; this bundle exports {offered:?}")]
    ClassNotFound {
        /// The display name asked for.
        wanted: String,
        /// The audio classes the bundle does export.
        offered: Vec<String>,
    },
    /// The class was created but does not implement `IAudioProcessor`.
    #[error("VST3 plugin does not support IAudioProcessor")]
    NoAudioProcessor,
}

/// Errors produced while loading, initializing, or driving a VST3 plugin.
#[derive(Error, Debug)]
#[non_exhaustive]
pub enum Vst3Error {
    /// A file-system or factory-level failure that aborted loading before the
    /// plugin could be instantiated. `stage` pinpoints which step failed.
    #[error("Failed to load plugin at {path}: {stage} - {cause}")]
    LoadFailed {
        /// Bundle or DSO path the host attempted to load.
        path: PathBuf,
        /// Which loading step failed.
        stage: LoadStage,
        /// What went wrong.
        cause: LoadCause,
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

    /// Feature requested by the host is not supported by this plugin — 64-bit
    /// processing, a prefetch mode, a bus arrangement.
    ///
    /// Editor failures are [`Vst3Error::EditorError`], not this: "the plugin
    /// publishes no view" and "the view refused this platform's window type"
    /// are both editor outcomes a caller handles at the editor call site.
    #[error("Feature not supported: {0}")]
    NotSupported(String),

    /// Opening, embedding, resizing or closing the plugin's native editor
    /// failed — including a plugin that publishes no view at all.
    #[error("Editor error: {0}")]
    EditorError(String),

    /// Parameter index, id, or value fell outside the plugin's allowed range.
    #[error("Invalid parameter: {0}")]
    InvalidParameter(String),

    /// Saving or restoring plugin state failed — truncated data, mismatched
    /// format, or the plugin rejected the stream.
    #[error("State error: {0}")]
    StateError(String),
}
