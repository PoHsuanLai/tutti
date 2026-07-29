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

    /// Audio processing failed — a 64-bit buffer was passed to a 32-bit-only
    /// plugin, or a similar setup-time fault.
    ///
    /// Carries an owned `String`, so this variant must **not** be constructed
    /// on the audio thread. The two conditions raised from inside `process`
    /// itself have their own allocation-free variants below.
    #[error("Processing error: {0}")]
    ProcessError(String),

    /// The plugin returned `CLAP_PROCESS_ERROR` from `process`.
    ///
    /// Split out of [`ClapError::ProcessError`] because it is raised **on the
    /// audio thread**, where the `String` the old shape required was a heap
    /// allocation inside the callback. A plugin in an error state typically
    /// returns ERROR on every block, so that allocation repeated per block —
    /// on the very path where the host has already decided something is wrong.
    ///
    /// The message is fixed, so `Display` can render it without storing it.
    #[error("Processing error: plugin returned CLAP_PROCESS_ERROR")]
    PluginReturnedError,

    /// A `process` call asked for more frames than the instance was activated
    /// for (CLAP's `max_frames_count`).
    ///
    /// Also raised on the audio thread, and also formerly a `format!`ed
    /// `String`. The two numbers ride as fields and are rendered by `Display`
    /// off-thread, which is where the message is actually read.
    ///
    /// Recovering means growing the scratch off the audio thread via
    /// `set_max_block_size`; the host cannot resize inside the callback.
    #[error(
        "Processing error: block size {requested} exceeds activated max_frames {max_frames}; \
         grow it off the audio thread with `set_max_block_size`"
    )]
    BlockTooLarge { requested: u32, max_frames: u32 },

    /// The plugin's `start_processing` returned false.
    ///
    /// Raised from `ensure_processing`, which `process` calls on the audio
    /// thread to self-start after activation — so this was the third `String`
    /// allocated inside the callback. It also repeats: nothing marks the
    /// instance unusable, so every subsequent block re-attempts the call and
    /// re-allocates on failure.
    #[error("Processing error: plugin refused to start processing")]
    StartProcessingFailed,

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
