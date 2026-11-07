//! A SoundFont synthesizer: [`Synthesizer`] plays the presets of a
//! [`tutti_sf2::SoundFont`] from MIDI channel messages and renders stereo
//! audio.
//!
//! The synthesis is a port of Nobuaki Tanaka's
//! [rustysynth](https://github.com/sinshu/rustysynth) 1.3.6, which a test
//! pins sample for sample; `tutti-sf2` is the file-reading half.

#![forbid(unsafe_code)]

mod array_math;
mod bi_quad_filter;
mod channel;
mod chorus;
mod envelope_stage;
mod error;
mod lfo;
mod modulation_envelope;
mod oscillator;
mod region_ex;
mod region_pair;
mod reverb;
mod soundfont_math;
mod synthesizer;
mod synthesizer_settings;
mod voice;
mod voice_collection;
mod volume_envelope;

pub use self::error::SynthesizerError;
pub use self::synthesizer::{Stop, Synthesizer};
pub use self::synthesizer_settings::SynthesizerSettings;
