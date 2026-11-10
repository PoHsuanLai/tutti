#![doc = include_str!("../README.md")]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![forbid(unsafe_code)]

mod bank;
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

pub use bank::{Bank, BankError};
pub use error::{Error, Result};
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
