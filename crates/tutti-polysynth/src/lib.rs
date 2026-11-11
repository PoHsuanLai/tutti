#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]
#![cfg_attr(docsrs, feature(doc_cfg))]
//!
//! ## Items
//!
//! - [`PolySynth`]: the synth, a graph node with one MIDI event input.
//! - [`SynthConfig`]: everything a synth is built from, with
//!   [`OscillatorType`], [`FilterType`] and [`SvfMode`], [`Adsr`]
//!   and [`FilterModConfig`].
//! - Voice allocation: [`VoiceMode`] and [`AllocationStrategy`].
//! - [`UnisonConfig`], [`PortamentoConfig`] (with [`PortamentoMode`] and
//!   the `tutti_curve::Law` of its `curve`) and [`Tuning`].
//! - [`enum@Error`] and [`Result`]: what construction can fail with.

mod error;
pub use error::{Error, Result};

mod note_id;
mod voice;
pub(crate) use voice::{VoiceAllocator, VoiceAllocatorConfig};

mod mpe;
pub(crate) use mpe::MpeVoiceState;
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
pub use portamento::{PortamentoConfig, PortamentoMode};

mod tuning;
// Public: `Tuning` appears in `SynthConfig`.
pub use tuning::Tuning;

mod synth;
pub use synth::{FilterModConfig, FilterType, OscillatorType, SvfMode, SynthConfig};
// The amplitude envelope `SynthConfig::envelope` takes; one type with the
// modulation sources' note envelope.
pub use tutti_types::Adsr;

mod polysynth;
// `PolySynth::fork_instance`: the synth in a fork of the
// graph (an export), with its clip.
mod fork;
pub use polysynth::{PolySynth, SYNTH_PARAMS};

mod synth_voice;

mod bank;
mod kernel;
