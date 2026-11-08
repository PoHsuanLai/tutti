use std::io;

use thiserror::Error;

use std::fmt::{self, Display};

/// `Result` with this crate's [`enum@Error`] as the failure type.
pub type Result<T> = std::result::Result<T, Error>;

/// A RIFF four-character code as the messages show it: its bytes, with any
/// that are not printable ASCII as `?` (the reader has already made them so).
struct Code<'a>(&'a [u8; 4]);

impl Display for Code<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.iter().try_for_each(|&b| write!(f, "{}", b as char))
    }
}

/// Why a SoundFont could not be loaded.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum Error {
    /// Reading the file failed. The cause is the [`source`](std::error::Error::source).
    #[error("reading the SoundFont failed")]
    Io(#[from] io::Error),
    /// The file does not start with a RIFF chunk.
    #[error("the RIFF chunk was not found")]
    RiffChunkNotFound,
    /// The RIFF chunk's form type is not `sfbk`.
    #[error("the type of the RIFF chunk must be '{}', but was '{}'", Code(.expected), Code(.actual))]
    InvalidRiffChunkType {
        /// The form type the format requires, as four ASCII bytes.
        expected: [u8; 4],
        /// The form type the file has; a byte that is not printable ASCII is `?`.
        actual: [u8; 4],
    },
    /// A LIST chunk is missing where the format has one.
    #[error("the LIST chunk was not found")]
    ListChunkNotFound,
    /// A LIST chunk has the wrong type.
    #[error("the type of the LIST chunk must be '{}', but was '{}'", Code(.expected), Code(.actual))]
    InvalidListChunkType {
        /// The list type the format requires here, as four ASCII bytes.
        expected: [u8; 4],
        /// The list type the file has; a byte that is not printable ASCII is `?`.
        actual: [u8; 4],
    },
    /// A LIST chunk holds a sub-chunk the format does not define there.
    #[error("the INFO list contains an unknown ID '{}'", Code(.0))]
    ListContainsUnknownId([u8; 4]),
    /// The file has no usable `smpl` chunk.
    #[error("no valid sample data was found")]
    SampleDataNotFound,
    /// The sample data is compressed (SoundFont 3).
    #[error("SoundFont3 is not yet supported")]
    UnsupportedSampleFormat,
    /// A required sub-chunk is missing.
    #[error("the '{}' sub-chunk was not found", Code(.0))]
    SubChunkNotFound([u8; 4]),
    /// The `phdr` chunk is empty or its size is not a whole record count.
    #[error("the preset list is invalid")]
    InvalidPresetList,
    /// A preset zone names an instrument that does not exist.
    #[error(
        "the preset with the ID '{preset_id}' contains an invalid instrument ID '{instrument_id}'"
    )]
    InvalidInstrumentId {
        /// The preset's index.
        preset_id: usize,
        /// The instrument index the zone names.
        instrument_id: usize,
    },
    /// A preset has no zone.
    #[error("the preset with the ID '{0}' has no zone")]
    InvalidPreset(usize),
    /// The file has no preset.
    #[error("no valid preset was found")]
    PresetNotFound,
    /// The `inst` chunk is empty or its size is not a whole record count.
    #[error("the instrument list is invalid")]
    InvalidInstrumentList,
    /// An instrument zone names a sample that does not exist.
    #[error(
        "the instrument with the ID '{instrument_id}' contains an invalid sample ID '{sample_id}'"
    )]
    InvalidSampleId {
        /// The instrument's index.
        instrument_id: usize,
        /// The sample index the zone names.
        sample_id: usize,
    },
    /// An instrument has no zone.
    #[error("the instrument with the ID '{0}' has no zone")]
    InvalidInstrument(usize),
    /// The file has no instrument.
    #[error("no valid instrument was found")]
    InstrumentNotFound,
    /// The `shdr` chunk is empty or its size is not a whole record count.
    #[error("the sample header list is invalid")]
    InvalidSampleHeaderList,
    /// A bag chunk is empty or its size is not a whole record count.
    #[error("the zone list is invalid")]
    InvalidZoneList,
    /// A zone list has no zone.
    #[error("no valid zone was found")]
    ZoneNotFound,
    /// A generator chunk is empty or its size is not a whole record count.
    #[error("the generator list is invalid")]
    InvalidGeneratorList,
    /// A region's sample range or loop lies outside the sample data.
    #[error("sanity check failed")]
    SanityCheckFailed,
}
