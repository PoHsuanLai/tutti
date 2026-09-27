#![doc = include_str!("../README.md")]
//!
//! ## Items
//!
//! - [`PolySynth`]: the synth, a graph node with one MIDI event input.
//! - [`SynthConfig`]: everything a synth is built from, with
//!   [`OscillatorType`], [`FilterType`] and [`SvfMode`], [`EnvelopeConfig`]
//!   and [`FilterModConfig`].
//! - Voice allocation: [`VoiceMode`] and [`AllocationStrategy`].
//! - [`UnisonConfig`], [`PortamentoConfig`] (with [`PortamentoMode`] and
//!   [`PortamentoCurve`]) and [`Tuning`].
//! - [`enum@Error`] and [`Result`]: what construction can fail with.

mod error;
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
// `PolySynth::fork_instance`: the synth in a fork of the
// graph (an export), with its clip.
mod fork;
pub use polysynth::PolySynth;

mod synth_voice;

mod bank;
mod kernel;
