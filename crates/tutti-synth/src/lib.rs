//! Synthesizer building blocks for Tutti.
//!
//! Provides polyphonic synthesis via [`SynthHandle`] (accessed through `engine.synth()`).
//!
//! ```ignore
//! let synth = engine.synth()
//!     .saw()
//!     .poly(8)
//!     .filter_moog(2000.0, 0.7)
//!     .adsr(0.01, 0.2, 0.6, 0.3)
//!     .unison(3, 15.0, 0.5)
//!     .build()?;
//!
//! let synth_id = engine.graph_mut(|net| net.add(synth).master());
//! engine.note_on(synth_id, Note::C4, 100);
//! ```

pub mod error;
pub use error::{Error, Result};

mod voice;
pub(crate) use voice::{
    AllocationResult, AllocationStrategy, MpeVoiceState, VoiceAllocator, VoiceAllocatorConfig,
    VoiceMode,
};

mod unison;
#[cfg(feature = "midi")]
pub(crate) use unison::UnisonEngine;
pub(crate) use unison::{UnisonConfig, UnisonVoiceParams};

mod portamento;
pub(crate) use portamento::{Portamento, PortamentoConfig, PortamentoCurve, PortamentoMode};

mod tuning;
pub(crate) use tuning::Tuning;

#[cfg(feature = "soundfont")]
mod soundfont;
#[cfg(all(feature = "soundfont", feature = "bevy_asset"))]
pub use soundfont::SoundFontAsset;
#[cfg(feature = "soundfont")]
pub use soundfont::{
    sf2, Sf2Builder, SoundFont, SoundFontHandle, SoundFontSystem, SoundFontUnit,
    SynthesizerSettings,
};

mod builder;
#[cfg(feature = "midi")]
pub use builder::PolySynth;
pub(crate) use builder::{EnvelopeConfig, FilterType, OscillatorType, SvfMode, SynthBuilder};

#[cfg(feature = "midi")]
mod handle;
#[cfg(feature = "midi")]
pub use handle::SynthHandle;

#[cfg(feature = "soundfont")]
pub mod ecs;
#[cfg(feature = "soundfont")]
pub use ecs::{
    promote_pending_soundfonts, soundfont_playback_system, PendingSoundFontUnit, PlaySoundFont,
    SoundFontAssetLoader, SoundFontAssetLoaderError, SoundFontRes, TuttiSoundFontPlugin,
};
