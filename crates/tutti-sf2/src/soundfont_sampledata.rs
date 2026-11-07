use std::io::Read;

use crate::binary_reader::ReadExt;
use crate::error::SoundFontError;
use crate::four_cc::FourCC;
use crate::read_counter::ReadCounter;

pub(crate) struct SoundFontSampleData {
    pub bits_per_sample: i32,
    pub wave_data: Vec<i16>,
}

impl SoundFontSampleData {
    pub(crate) fn new<R: Read>(reader: &mut R) -> Result<Self, SoundFontError> {
        let chunk_id = reader.read_four_cc()?;
        if chunk_id != b"LIST" {
            return Err(SoundFontError::ListChunkNotFound);
        }

        let end = reader.read_u32()? as usize;
        let reader = &mut ReadCounter::new(reader);

        let list_type = reader.read_four_cc()?;
        if list_type != b"sdta" {
            return Err(SoundFontError::InvalidListChunkType {
                expected: FourCC::from_bytes(*b"sdta"),
                actual: list_type,
            });
        }

        let mut wave_data: Option<Vec<i16>> = None;

        while reader.bytes_read() < end {
            let id = reader.read_four_cc()?;
            let size = reader.read_u32()? as usize;

            match id.as_bytes() {
                b"smpl" => wave_data = Some(reader.read_wave_data(size)?),
                b"sm24" => reader.discard_data(size)?,
                _ => return Err(SoundFontError::ListContainsUnknownId(id)),
            }
        }

        let Some(wave_data) = wave_data else {
            return Err(SoundFontError::SampleDataNotFound);
        };

        if wave_data.len() < 2 {
            return Err(SoundFontError::SampleDataNotFound);
        }

        // SoundFont3 compressed sample format: the first four bytes read "OggS".
        let ogg_magic = [i16::from_le_bytes(*b"Og"), i16::from_le_bytes(*b"gS")];
        if wave_data[..2] == ogg_magic {
            return Err(SoundFontError::UnsupportedSampleFormat);
        }

        Ok(Self {
            bits_per_sample: 16,
            wave_data,
        })
    }
}
