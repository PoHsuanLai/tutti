use std::io::Read;

use crate::binary_reader::ReadExt;
use crate::chunk::{next_chunk, skip_pad};
use crate::error::Error;
use crate::read_counter::ReadCounter;

pub(crate) struct SoundFontSampleData {
    pub bits_per_sample: u8,
    pub wave_data: Vec<i16>,
}

impl SoundFontSampleData {
    pub(crate) fn new<R: Read>(reader: &mut R) -> Result<Self, Error> {
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

        while reader.bytes_read() < end {
            let (id, size) = next_chunk(reader, end)?;

            match id.as_bytes() {
                // The low bytes of a 24-bit pool: not read.
                b"sm24" => reader.discard_data(size)?,
                b"smpl" => wave_data = Some(reader.read_wave_data(size)?),
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

        Ok(Self {
            bits_per_sample: 16,
            wave_data,
        })
    }
}
