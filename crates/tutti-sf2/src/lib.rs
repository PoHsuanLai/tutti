//! A SoundFont 2 (`.sf2`) file as data: [`SoundFont`] parses one into its
//! presets, instruments, sample headers and sample data, and holds nothing
//! that plays. `tutti-soundfont` is the player over it.
//!
//! Generator values stay raw, in the unit the SoundFont 2.04 specification
//! gives each ([`GeneratorType`]); turning a value into seconds or hertz is
//! the player's.
//!
//! The `smpl` chunk is read as 16-bit samples. A file with a compressed
//! (SoundFont 3) `smpl` chunk is refused with
//! [`SoundFontError::UnsupportedSampleFormat`], and the `sm24` chunk and the
//! modulator chunks are skipped.

#![forbid(unsafe_code)]

mod binary_reader;
mod error;
mod four_cc;
mod generator;
mod generator_type;
mod instrument;
mod instrument_info;
mod instrument_region;
mod loop_mode;
mod preset;
mod preset_info;
mod preset_region;
mod read_counter;
mod sample_header;
mod soundfont;
mod soundfont_info;
mod soundfont_parameters;
mod soundfont_sampledata;
mod soundfont_version;
mod zone;
mod zone_info;

use std::ops::RangeInclusive;

pub use error::SoundFontError;
pub use four_cc::FourCC;
pub use generator_type::GeneratorType;
pub use instrument::Instrument;
pub use instrument_region::InstrumentRegion;
pub use loop_mode::LoopMode;
pub use preset::Preset;
pub use preset_region::PresetRegion;
pub use sample_header::SampleHeader;
pub use soundfont::SoundFont;
pub use soundfont_info::SoundFontInfo;
pub use soundfont_version::SoundFontVersion;

/// The inclusive range a range generator packs: the low byte is the first
/// value, the high byte the last.
fn generator_range(raw: i16) -> RangeInclusive<u8> {
    let [low, high] = raw.to_le_bytes();
    low..=high
}
