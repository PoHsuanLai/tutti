#![doc = include_str!("../README.md")]
//! `tutti-sf2` reads a SoundFont 2 (`.sf2`) file into data: the presets, the
//! instruments, the sample headers and the 16-bit sample data. It holds nothing
//! that plays; `tutti-soundfont` is the player over it.
//!
//! The code is the file-reading half of Nobuaki Tanaka's
//! [rustysynth](https://github.com/sinshu/rustysynth) (ported from
//! [MeltySynth](https://github.com/sinshu/meltysynth)); see [the license](LICENSE.txt).
//!
//! Generator values stay raw, in the unit the SoundFont 2.04 specification gives
//! each (`GeneratorType`); turning a value into seconds or hertz is the player's.
//! A file with a compressed (SoundFont 3) `smpl` chunk is refused with
//! `Error::UnsupportedSampleFormat`; the `sm24` chunk and the modulator chunks are
//! skipped.
//!
//! Sample positions are `tutti_types::Samples`, a rate is a `SampleRate` and a
//! pitch correction `Cents`. A preset's bank is a `Bank` (a MIDI bank 0 to 127, or
//! percussion) and its program a `U7`; a preset whose file values are neither
//! cannot be played but is kept, as the specification says (section 7.2), and
//! reports `None`. An exclusive class is an `Option<NonZeroU8>`: `None` is class 0.
//! `SoundFont::open` reads a path and `SoundFont::from_reader` any `Read`. The
//! crate's failure type is `Error`, with a `Result` alias.
//!
//! ```no_run
//! use tutti_sf2::SoundFont;
//!
//! let soundfont = SoundFont::open("piano.sf2")?;
//! for preset in soundfont.presets() {
//!     if let (Some(bank), Some(patch)) = (preset.bank(), preset.patch()) {
//!         println!("{bank}:{} {}", patch.get(), preset.name());
//!     }
//! }
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
#![cfg_attr(docsrs, feature(doc_cfg))]
#![forbid(unsafe_code)]

mod bank;
mod binary_reader;
mod chunk;
mod error;
mod four_cc;
mod generator;
mod generator_type;
mod instrument;
mod instrument_generator;
mod instrument_info;
mod instrument_region;
mod loop_mode;
mod preset;
mod preset_generator;
mod preset_info;
mod preset_region;
mod read_counter;
mod sample_header;
mod soundfont;
mod soundfont_info;
mod soundfont_parameters;
mod soundfont_sampledata;
mod soundfont_version;
mod summed_generator;
mod zone;
mod zone_info;

use std::ops::RangeInclusive;

pub use bank::{Bank, BankError};
pub use error::{Error, Result};
pub use generator_type::{GeneratorType, WrongLevelGenerator};
pub use instrument::Instrument;
pub use instrument_generator::InstrumentGenerator;
pub use instrument_region::InstrumentRegion;
pub use loop_mode::LoopMode;
pub use preset::Preset;
pub use preset_generator::PresetGenerator;
pub use preset_region::PresetRegion;
pub use sample_header::{NEAREST_PRACTICAL_RATE, SampleHeader, Side};
pub use soundfont::SoundFont;
pub use soundfont_info::SoundFontInfo;
pub use soundfont_version::SoundFontVersion;
pub use summed_generator::SummedGenerator;

/// The inclusive range a range generator packs: the low byte is the first
/// value, the high byte the last.
fn generator_range(raw: i16) -> RangeInclusive<u8> {
    let [low, high] = raw.to_le_bytes();
    low..=high
}
