//! Centralized error type for the bevy-tutti audio engine.
//!
//! Wraps all subsystem errors so `?` propagates naturally across crate boundaries.

use thiserror::Error;

/// Aggregate error type covering every subsystem re-exported by bevy-tutti.
///
/// Each variant wraps a subsystem error via `#[from]`, so `?` propagates across
/// crate boundaries. Device failures arrive as one [`Device`](Self::Device)
/// variant rather than a per-CPAL-call spread — [`tutti_cpal::Error`] owns that
/// distinction.
#[derive(Error, Debug)]
pub enum Error {
    /// Wraps an error from [`tutti_core::Error`].
    #[error(transparent)]
    Core(#[from] tutti_core::Error),

    /// Audio device failure — enumeration, stream construction, or playback.
    ///
    /// Wraps [`tutti_cpal::Error`], which owns every CPAL concern.
    #[error(transparent)]
    Device(#[from] tutti_cpal::Error),

    /// MIDI subsystem failure.
    ///
    /// Wraps [`tutti_midi_hardware::Error`], and is therefore gated on
    /// **`midi-hardware`** rather than `midi`: the error type comes from the OS
    /// port layer, and the software-only build has no fallible MIDI step —
    /// building a bus, a routing table and the block phases cannot fail.
    #[cfg(feature = "midi-hardware")]
    #[error("MIDI: {0}")]
    Midi(#[from] tutti_midi_hardware::Error),

    /// Polyphonic-synth subsystem failure.
    ///
    /// Wraps [`tutti_polysynth::Error`].
    #[cfg(feature = "synth")]
    #[error("Synth: {0}")]
    Synth(#[from] tutti_polysynth::Error),

    /// SoundFont subsystem failure.
    ///
    /// Wraps [`tutti_soundfont::Error`]. A separate variant from [`Self::Synth`]
    /// because the two are separate crates with disjoint failure modes — a
    /// rejected `.sf2` is not a bad `SynthConfig`, and the feature axes are
    /// independent (`soundfont` no longer implies `synth`).
    #[cfg(feature = "soundfont")]
    #[error("SoundFont: {0}")]
    SoundFont(#[from] tutti_soundfont::Error),

    /// Sampler subsystem failure.
    ///
    /// Wraps [`tutti_sampler::Error`].
    #[cfg(feature = "sampler")]
    #[error("Sampler: {0}")]
    Sampler(#[from] tutti_sampler::Error),

    /// Spatial-audio subsystem failure.
    ///
    /// Wraps [`tutti_spatial::VbapError`] — VBAP speaker-layout construction.
    /// HRTF has its own error type and does not route through here.
    #[cfg(feature = "spatial")]
    #[error("Spatial: {0}")]
    Spatial(#[from] tutti_spatial::VbapError),

    /// Plugin-host bridge failure (VST2/VST3/CLAP).
    ///
    /// Wraps [`tutti_plugin::BridgeError`].
    #[cfg(feature = "plugin")]
    #[error("Plugin: {0}")]
    Plugin(#[from] tutti_plugin::BridgeError),

    /// Offline rendering / export subsystem failure.
    ///
    /// Wraps [`tutti_export::Error`].
    #[cfg(feature = "export")]
    #[error("Export: {0}")]
    Export(#[from] tutti_export::Error),

    /// Standard I/O error encountered while reading or writing audio data.
    ///
    /// Wraps [`std::io::Error`].
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Convenience alias for `Result<T, `[`enum@Error`]`>` used throughout this crate.
pub type Result<T> = core::result::Result<T, Error>;
