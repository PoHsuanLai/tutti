//! Synthesizer building blocks for Tutti.
//!
//! Provides polyphonic synthesis via [`PolySynth`], built from a [`SynthConfig`].
//! Configure the config the idiomatic Bevy way — `Default` plus struct-update:
//!
//! ```ignore
//! let synth = PolySynth::new(SynthConfig {
//!     oscillator: OscillatorType::Saw,
//!     max_voices: 8,
//!     filter: FilterType::Moog { cutoff: 2000.0, resonance: 0.7 },
//!     envelope: EnvelopeConfig { attack: 0.01, decay: 0.2, sustain: 0.6, release: 0.3 },
//!     ..default()
//! })?;
//!
//! let synth_id = graph.add(synth);
//! ```

pub mod error;

mod node_id;
pub use error::{Error, Result};

mod voice;
pub(crate) use voice::{AllocationResult, MpeVoiceState, VoiceAllocator, VoiceAllocatorConfig};
// Public: these appear in `SynthConfig`'s fields.
pub use voice::{AllocationStrategy, VoiceMode};

mod unison;
#[cfg(feature = "midi")]
pub(crate) use unison::UnisonEngine;
pub(crate) use unison::UnisonVoiceParams;
// Public: `UnisonConfig` appears in `SynthConfig`.
pub use unison::UnisonConfig;

mod portamento;
pub(crate) use portamento::Portamento;
// Public: these appear in `SynthConfig` (`portamento` field + its config).
pub use portamento::{PortamentoConfig, PortamentoCurve, PortamentoMode};

mod tuning;
// Public: `Tuning` appears in `SynthConfig`.
pub use tuning::Tuning;

#[cfg(feature = "soundfont")]
mod soundfont;
#[cfg(all(feature = "soundfont", feature = "bevy_asset"))]
pub use soundfont::SoundFontAsset;
#[cfg(feature = "soundfont")]
pub use soundfont::{
    promote_pending_soundfonts, soundfont_playback_system, PendingSoundFontUnit, PlaySoundFont,
    SoundFont, SoundFontAssetLoader, SoundFontAssetLoaderError, SoundFontUnit, SynthesizerSettings,
    TuttiSoundFontPlugin,
};

mod synth;
pub use synth::{
    EnvelopeConfig, FilterModConfig, FilterType, OscillatorType, SvfMode, SynthConfig,
};

#[cfg(feature = "midi")]
mod polysynth;
#[cfg(feature = "midi")]
pub use polysynth::PolySynth;

#[cfg(feature = "midi")]
mod synth_voice;
