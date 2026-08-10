//! Polyphonic subtractive and wavetable synthesis for tutti.
//!
//! One type does the work: [`PolySynth`], an `AudioUnit` built from a
//! [`SynthConfig`] and driven by MIDI. It takes no audio input — notes arrive
//! through its own lock-free MIDI inbox — and renders stereo.
//!
//! ```
//! use tutti_polysynth::{
//!     EnvelopeConfig, FilterType, OscillatorType, PolySynth, SynthConfig,
//! };
//! use tutti_core::{Amplitude, Hz, Resonance, Seconds};
//!
//! let synth = PolySynth::new(SynthConfig {
//!     oscillator: OscillatorType::Saw,
//!     max_voices: 8,
//!     filter: FilterType::Moog {
//!         cutoff: Hz(2000.0),
//!         resonance: Resonance(0.7),
//!     },
//!     envelope: EnvelopeConfig {
//!         attack: Seconds(0.01),
//!         decay: Seconds(0.2),
//!         sustain: Amplitude(0.6),
//!         release: Seconds(0.3),
//!     },
//!     ..Default::default()
//! })?;
//!
//! let sender = synth.midi_sender();
//! # Ok::<(), tutti_polysynth::Error>(())
//! ```
//!
//! # What is fixed at construction, and what is not
//!
//! The DSP chain each voice runs is assembled once from the oscillator, filter
//! and envelope, so those three need a new synth to change. Unison detune,
//! spread and sub-voice count, master volume, and MPE enablement all have live
//! setters. `max_voices` is capped at 16 — the ceiling exists so the per-block
//! finished-voice list stays inline and the audio callback never allocates.
//!
//! # Not a SoundFont player
//!
//! `.sf2` playback is [`tutti-soundfont`]'s, a peer crate rather than a feature
//! of this one: a sample player shares no voice engine, envelope model or
//! filter with a subtractive synth, so the two have nothing to hold in common
//! beyond the `AudioUnit` trait.
//!
//! [`tutti-soundfont`]: https://docs.rs/tutti-soundfont

pub mod error;

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
