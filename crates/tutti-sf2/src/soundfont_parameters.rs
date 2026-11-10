use std::io::Read;

use crate::binary_reader::ReadExt;
use crate::chunk::{next_chunk, skip_pad};
use crate::error::Error;
use crate::generator::Generator;
use crate::instrument::Instrument;
use crate::instrument_info::InstrumentInfo;
use crate::preset::Preset;
use crate::preset_info::PresetInfo;
use crate::read_counter::ReadCounter;
use crate::sample_header::SampleHeader;
use crate::zone::Zone;
use crate::zone_info::ZoneInfo;

#[non_exhaustive]
pub(crate) struct SoundFontParameters {
    pub(crate) sample_headers: Vec<SampleHeader>,
    pub(crate) presets: Vec<Preset>,
    pub(crate) instruments: Vec<Instrument>,
}

impl SoundFontParameters {
    pub(crate) fn new<R: Read>(reader: &mut R) -> Result<Self, Error> {
        let chunk_id = reader.read_four_cc()?;
        if chunk_id != b"LIST" {
            return Err(Error::ListChunkNotFound);
        }

        let end = reader.read_u32()? as usize;
        let reader = &mut ReadCounter::new(reader);

        let list_type = reader.read_four_cc()?;
        if list_type != b"pdta" {
            return Err(Error::InvalidListChunkType {
                expected: *b"pdta",
                actual: *list_type.as_bytes(),
            });
        }

        let mut preset_infos: Option<Vec<PresetInfo>> = None;
        let mut preset_bag: Option<Vec<ZoneInfo>> = None;
        let mut preset_generators: Option<Vec<Generator>> = None;
        let mut instrument_infos: Option<Vec<InstrumentInfo>> = None;
        let mut instrument_bag: Option<Vec<ZoneInfo>> = None;
        let mut instrument_generators: Option<Vec<Generator>> = None;
        let mut sample_headers: Option<Vec<SampleHeader>> = None;

        while reader.bytes_read() < end {
            let (id, size) = next_chunk(reader, end)?;

            match id.as_bytes() {
                b"phdr" => preset_infos = Some(PresetInfo::read_from_chunk(reader, size)?),
                b"pbag" => preset_bag = Some(ZoneInfo::read_from_chunk(reader, size)?),
                b"pmod" => reader.discard_data(size)?,
                b"pgen" => preset_generators = Some(Generator::read_from_chunk(reader, size)?),
                b"inst" => instrument_infos = Some(InstrumentInfo::read_from_chunk(reader, size)?),
                b"ibag" => instrument_bag = Some(ZoneInfo::read_from_chunk(reader, size)?),
                b"imod" => reader.discard_data(size)?,
                b"igen" => instrument_generators = Some(Generator::read_from_chunk(reader, size)?),
                b"shdr" => sample_headers = Some(SampleHeader::read_from_chunk(reader, size)?),
                // A reader ignores what it does not know (section 4).
                _ => reader.discard_data(size)?,
            }
            skip_pad(reader, size, end)?;
        }

        let preset_infos = preset_infos.ok_or(Error::SubChunkNotFound(*b"PHDR"))?;

        let preset_bag = preset_bag.ok_or(Error::SubChunkNotFound(*b"PBAG"))?;

        let preset_generators = preset_generators.ok_or(Error::SubChunkNotFound(*b"PGEN"))?;

        let instrument_infos = instrument_infos.ok_or(Error::SubChunkNotFound(*b"INST"))?;

        let instrument_bag = instrument_bag.ok_or(Error::SubChunkNotFound(*b"IBAG"))?;

        let instrument_generators =
            instrument_generators.ok_or(Error::SubChunkNotFound(*b"IGEN"))?;

        let sample_headers = sample_headers.ok_or(Error::SubChunkNotFound(*b"SHDR"))?;

        let instrument_zones = Zone::create(&instrument_bag, &instrument_generators)?;
        let instruments =
            Instrument::create(&instrument_infos, &instrument_zones, &sample_headers)?;

        let preset_zones = Zone::create(&preset_bag, &preset_generators)?;
        let presets = Preset::create(&preset_infos, &preset_zones, &instruments)?;

        Ok(Self {
            sample_headers,
            presets,
            instruments,
        })
    }
}
