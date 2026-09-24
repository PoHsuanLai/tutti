//! Polyphonic subtractive and wavetable synthesis for the Tutti audio engine.
//!
//! One type does the work: [`PolySynth`], an `AudioUnit` built from a
//! [`SynthConfig`] and driven by MIDI. It takes no audio input — notes arrive
//! through its own lock-free MIDI inbox, reached via
//! [`midi_sender`](PolySynth::midi_sender) — and renders stereo.
//!
//! Around it sit the voice engine's parts, all configured through
//! [`SynthConfig`]: allocation ([`AllocationStrategy`], [`VoiceMode`]), unison
//! ([`UnisonConfig`]), portamento ([`PortamentoConfig`]) and [`Tuning`].
//!
//! `.sf2` playback is [`tutti-soundfont`]'s, a peer crate rather than a feature
//! of this one: a sample player shares no voice engine, envelope model or filter
//! with a subtractive synth, so the two have nothing to hold in common beyond
//! the `AudioUnit` trait.
//!
//! The quick start, what is fixed at construction, the `max_voices` ceiling and
//! the features are in the crate README, included below.
//!
//! [`tutti-soundfont`]: https://docs.rs/tutti-soundfont
#![doc = include_str!("../README.md")]

mod error;

mod node_id;
pub use error::{Error, Result};

mod voice;
pub(crate) use voice::{AllocationResult, MpeVoiceState, VoiceAllocator, VoiceAllocatorConfig};
// Public: these appear in `SynthConfig`'s fields.
pub use voice::{AllocationStrategy, VoiceMode};

mod unison;
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

mod synth;
pub use synth::{
    EnvelopeConfig, FilterModConfig, FilterType, OscillatorType, SvfMode, SynthConfig,
};

mod polysynth;
pub use polysynth::PolySynth;

mod synth_voice;

mod bank;
mod kernel;
