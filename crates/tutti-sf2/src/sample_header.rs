use std::io::Read;

use tutti_types::{Cents, SampleRate, Samples};

use crate::binary_reader::ReadExt;
use crate::error::Error;

/// Represents a sample in the SoundFont.
#[derive(Debug)]
#[non_exhaustive]
pub struct SampleHeader {
    pub(crate) name: String,
    pub(crate) start: i32,
    pub(crate) end: i32,
    pub(crate) start_loop: i32,
    pub(crate) end_loop: i32,
    pub(crate) sample_rate: i32,
    pub(crate) original_pitch: u8,
    pub(crate) pitch_correction: i8,
    pub(crate) link: u16,
    pub(crate) sample_type: u16,
}

impl SampleHeader {
    fn new<R: Read>(reader: &mut R) -> Result<Self, Error> {
        let name = reader.read_fixed_length_string(20)?;
        let start = reader.read_i32()?;
        let end = reader.read_i32()?;
        let start_loop = reader.read_i32()?;
        let end_loop = reader.read_i32()?;
        let sample_rate = reader.read_i32()?;
        let original_pitch = reader.read_u8()?;
        let pitch_correction = reader.read_i8()?;
        let link = reader.read_u16()?;
        let sample_type = reader.read_u16()?;

        Ok(Self {
            name,
            start,
            end,
            start_loop,
            end_loop,
            sample_rate,
            original_pitch,
            pitch_correction,
            link,
            sample_type,
        })
    }

    pub(crate) fn read_from_chunk<R: Read>(
        reader: &mut R,
        size: usize,
    ) -> Result<Vec<SampleHeader>, Error> {
        if size == 0 || !size.is_multiple_of(46) {
            return Err(Error::InvalidSampleHeaderList);
        }

        let count = size / 46 - 1;

        let mut headers: Vec<SampleHeader> = Vec::new();
        for _i in 0..count {
            headers.push(SampleHeader::new(reader)?);
        }

        // The last one is the terminator.
        SampleHeader::new(reader)?;

        Ok(headers)
    }

    /// The name of the sample.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The start point of the sample in the sample data: an absolute index of
    /// 16-bit sample points (SF2 2.04 section 9.3), counted from the first.
    #[must_use]
    pub fn start(&self) -> Samples {
        Samples(self.start as u32 as usize)
    }

    /// The end point of the sample in the sample data.
    #[must_use]
    pub fn end(&self) -> Samples {
        Samples(self.end as u32 as usize)
    }

    /// The loop start point of the sample in the sample data.
    #[must_use]
    pub fn start_loop(&self) -> Samples {
        Samples(self.start_loop as u32 as usize)
    }

    /// The loop end point of the sample in the sample data.
    #[must_use]
    pub fn end_loop(&self) -> Samples {
        Samples(self.end_loop as u32 as usize)
    }

    /// The rate the sample was recorded at.
    #[must_use]
    pub fn sample_rate(&self) -> SampleRate {
        SampleRate(f64::from(self.sample_rate as u32))
    }

    /// The key number of the recorded pitch of the sample: 0 to 127, or 255
    /// for an unpitched sample (section 7.10). A raw byte, not a `NoteNumber`,
    /// because 255 is not a key.
    #[must_use]
    pub fn original_pitch(&self) -> u8 {
        self.original_pitch
    }

    /// The pitch correction that should be applied to the sample on playback.
    #[must_use]
    pub fn pitch_correction(&self) -> Cents {
        Cents(f32::from(self.pitch_correction))
    }

    /// The link info: the index of the sample this one is linked with.
    #[must_use]
    pub fn link(&self) -> u16 {
        self.link
    }

    /// The sample type: the `SFSampleLink` word (mono, right, left, linked, ROM).
    #[must_use]
    pub fn sample_type(&self) -> u16 {
        self.sample_type
    }
}
