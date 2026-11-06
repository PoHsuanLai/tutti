//! Error types for CLAP plugin hosting.

use std::path::PathBuf;
use thiserror::Error;

/// Convenience alias for results returned by this crate.
pub type Result<T> = std::result::Result<T, ClapError>;

/// The phase a plugin load or activation reached before failing, carried by
/// [`ClapError::LoadFailed`].
///
/// Shared by all format hosts; CLAP reports the Opening, Factory,
/// Instantiation, Initialization and Activation phases.
pub use tutti_plugin_types::LoadStage;

/// An error reported by the CLAP host.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ClapError {
    /// The plugin could not be loaded. `stage` pinpoints which step failed.
    #[error("Failed to load plugin at {path}: {stage} - {reason}")]
    LoadFailed {
        /// Bundle or library path that was being loaded.
        path: PathBuf,
        /// Which step of the load sequence failed.
        stage: LoadStage,
        /// Human-readable cause, from the OS loader or the plugin's own
        /// refusal.
        reason: String,
    },

    /// Audio processing failed — a 64-bit buffer was passed to a 32-bit-only
    /// plugin, or a similar setup-time fault.
    ///
    /// Carries an owned `String`, so it must **not** be constructed on the
    /// audio thread. The conditions raised from inside `process` have their own
    /// allocation-free variants below.
    #[error("Processing error: {0}")]
    ProcessError(String),

    /// The plugin returned `CLAP_PROCESS_ERROR` from `process`.
    ///
    /// Fieldless so it can be raised on the audio thread without allocating: a
    /// plugin in an error state usually returns ERROR every block.
    #[error("Processing error: plugin returned CLAP_PROCESS_ERROR")]
    PluginReturnedError,

    /// A `process` call asked for more frames than the instance was activated
    /// for (CLAP's `max_frames_count`).
    ///
    /// Audio-thread-raised, so the numbers ride as fields and `Display`
    /// formats them off-thread. Recovering means activating again with a larger
    /// `Prepare::max_block`; the host cannot resize inside the callback.
    #[error(
        "Processing error: block size {requested} exceeds activated max_frames {max_frames}; \
         activate again with a larger `max_block` off the audio thread"
    )]
    BlockTooLarge {
        /// Frames the `process` call asked for.
        requested: u32,
        /// Frames the instance was activated for — the ceiling `requested`
        /// exceeded.
        max_frames: u32,
    },

    /// The plugin's `start_processing` returned false.
    ///
    /// Also fieldless: `process` calls `ensure_processing` on the audio thread
    /// to self-start, and nothing marks the instance unusable, so a refusal
    /// repeats every block.
    #[error("Processing error: plugin refused to start processing")]
    StartProcessingFailed,

    /// The block's sample width is not the one the processor was activated
    /// with. The plugin was not called.
    #[error("Processing error: the block's sample format is not the activated one")]
    FormatMismatch,

    /// The port layout needs more channels or ports than the activation's
    /// scratch was sized for. The plugin was not called.
    #[error("Processing error: the port layout is larger than the activation sized for")]
    LayoutMismatch,

    /// A channel slice of the buffer is shorter than the block. The plugin was
    /// not called.
    #[error("Processing error: a channel buffer is shorter than the block")]
    BufferTooShort,

    /// Saving or loading plugin state failed.
    #[error("State error: {0}")]
    StateError(String),

    /// An operation that requires an active plugin was called on an
    /// inactive instance.
    #[error("Plugin not activated")]
    NotActive,

    /// A requested capability is not advertised by the plugin (e.g. activating
    /// at `f64` when the plugin is 32-bit only).
    #[error("Not supported: {0}")]
    NotSupported(String),

    /// A parameter ID or value was rejected by the plugin.
    #[error("Invalid parameter: {0}")]
    InvalidParameter(String),

    /// Editor/GUI creation, resize, or teardown failed.
    #[error("GUI error: {0}")]
    EditorError(String),

    /// Underlying IO failure (file system, stream).
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}
