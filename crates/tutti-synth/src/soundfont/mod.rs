//! SoundFont (.sf2) synthesis via RustySynth.

mod builder;
mod manager;
mod unit;

pub use builder::{sf2, Sf2Builder};
pub use manager::{SoundFontHandle, SoundFontSystem};
pub use rustysynth::{SoundFont, SynthesizerSettings};
pub use unit::SoundFontUnit;

#[cfg(feature = "bevy_asset")]
pub use rustysynth::SoundFontAsset;
