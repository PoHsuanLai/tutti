use std::io::Read;

use crate::binary_reader::ReadExt;
use crate::chunk::{next_chunk, skip_pad};
use crate::error::Error;
use crate::read_counter::ReadCounter;
use crate::soundfont_version::SoundFontVersion;

/// The sample pool as the file gives it: the `smpl` points, and the `sm24` low
/// bytes when the file's version and sizes let them count.
#[derive(Debug)]
pub(crate) enum Pool {
    Pcm16(Vec<i16>),
    Pcm24 { high: Vec<i16>, low: Vec<u8> },
}

impl Pool {
    pub(crate) fn len(&self) -> usize {
        match self {
            Pool::Pcm16(points) => points.len(),
            Pool::Pcm24 { high, .. } => high.len(),
        }
    }

    pub(crate) fn high(&self) -> &[i16] {
        match self {
            Pool::Pcm16(points) => points,
            Pool::Pcm24 { high, .. } => high,
        }
    }

    pub(crate) fn view(&self) -> SamplePool<'_> {
        match self {
            Pool::Pcm16(points) => SamplePool::Pcm16(points),
            Pool::Pcm24 { high, low } => SamplePool::Pcm24(Pcm24 { high, low }),
        }
    }
}

/// The sample points a SoundFont plays: 16 bits from `smpl`, or 24 bits when
/// `sm24` joins them (SF2 2.04 section 6.2).
///
/// An `sm24` the specification says to ignore (no `smpl`, an `ifil` older than
/// 2.04, a size other than half of `smpl`'s plus one byte when that half is odd)
/// never reaches a player: the pool is [`Pcm16`](Self::Pcm16).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SamplePool<'a> {
    /// The `smpl` points alone, signed 16-bit.
    Pcm16(&'a [i16]),
    /// The `smpl` points with their `sm24` low bytes.
    Pcm24(Pcm24<'a>),
}

/// A 24-bit pool: each `smpl` point is the high 16 bits of a signed 24-bit point
/// and its `sm24` byte the low 8. The two slices have the same length.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pcm24<'a> {
    high: &'a [i16],
    low: &'a [u8],
}

impl<'a> Pcm24<'a> {
    /// The `smpl` points: the high 16 bits of each point.
    #[must_use]
    pub fn high(&self) -> &'a [i16] {
        self.high
    }

    /// The `sm24` bytes: the low 8 bits of each point.
    #[must_use]
    pub fn low(&self) -> &'a [u8] {
        self.low
    }

    /// The signed 24-bit point at `index`, or `None` past the end.
    #[must_use]
    pub fn point(&self, index: usize) -> Option<i32> {
        let high = i32::from(*self.high.get(index)?);
        Some(high << 8 | i32::from(*self.low.get(index)?))
    }
}

pub(crate) struct SoundFontSampleData {
    pub pool: Pool,
}

impl SoundFontSampleData {
    /// Reads the `sdta` list. `version` is the file's `ifil`: it decides whether
    /// an `sm24` chunk counts.
    pub(crate) fn new<R: Read>(reader: &mut R, version: &SoundFontVersion) -> Result<Self, Error> {
        let chunk_id = reader.read_four_cc()?;
        if chunk_id != b"LIST" {
            return Err(Error::ListChunkNotFound);
        }

        let end = reader.read_u32()? as usize;
        let reader = &mut ReadCounter::new(reader);

        let list_type = reader.read_four_cc()?;
        if list_type != b"sdta" {
            return Err(Error::InvalidListChunkType {
                expected: *b"sdta",
                actual: *list_type.as_bytes(),
            });
        }

        let mut wave_data: Option<Vec<i16>> = None;
        let mut smpl_size = 0;
        let mut sm24: Option<Vec<u8>> = None;

        while reader.bytes_read() < end {
            let (id, size) = next_chunk(reader, end)?;

            match id.as_bytes() {
                b"sm24" => sm24 = Some(reader.read_bytes(size)?),
                b"smpl" => {
                    smpl_size = size;
                    wave_data = Some(reader.read_wave_data(size)?);
                }
                // Only the INFO list may hold what a reader does not know.
                _ => {
                    return Err(Error::UnknownChunk {
                        list: *b"sdta",
                        id: *id.as_bytes(),
                    });
                }
            }
            skip_pad(reader, size, end)?;
        }

        // A file with no `smpl`, or an empty one, has no sample data.
        let wave_data = wave_data.unwrap_or_default();

        // SoundFont3 compressed sample format: the first four bytes read "OggS".
        let ogg_magic = [i16::from_le_bytes(*b"Og"), i16::from_le_bytes(*b"gS")];
        if wave_data.starts_with(&ogg_magic) {
            return Err(Error::UnsupportedSampleFormat);
        }

        // Section 6.2: `sm24` counts with an `smpl`, an `ifil` of 2.04 or later,
        // and a size of half of `smpl`'s, plus a byte when that half is odd.
        let half = smpl_size / 2;
        let counts = smpl_size > 0
            && (version.major, version.minor) >= (2, 4)
            && sm24
                .as_ref()
                .is_some_and(|low| low.len() == half + half % 2);
        let pool = match sm24 {
            Some(mut low) if counts => {
                // The byte that pads an odd half belongs to no point.
                low.truncate(wave_data.len());
                Pool::Pcm24 {
                    high: wave_data,
                    low,
                }
            }
            _ => Pool::Pcm16(wave_data),
        };

        Ok(Self { pool })
    }
}
