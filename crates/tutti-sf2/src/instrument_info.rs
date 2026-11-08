use std::io::Read;

use crate::binary_reader::ReadExt;
use crate::error::Error;

#[non_exhaustive]
pub(crate) struct InstrumentInfo {
    pub(crate) name: String,
    pub(crate) zone_start_index: i32,
    pub(crate) zone_end_index: i32,
}

impl InstrumentInfo {
    fn new<R: Read>(reader: &mut R) -> Result<Self, Error> {
        let name = reader.read_fixed_length_string(20)?;
        let zone_start_index = reader.read_u16()? as i32;

        Ok(Self {
            name,
            zone_start_index,
            zone_end_index: 0,
        })
    }

    pub(crate) fn read_from_chunk<R: Read>(
        reader: &mut R,
        size: usize,
    ) -> Result<Vec<InstrumentInfo>, Error> {
        if size == 0 || !size.is_multiple_of(22) {
            return Err(Error::InvalidInstrumentList);
        }

        let count = size / 22;

        let mut instruments: Vec<InstrumentInfo> = Vec::new();
        for _i in 0..count {
            instruments.push(InstrumentInfo::new(reader)?);
        }

        for i in 0..(count - 1) {
            instruments[i].zone_end_index = instruments[i + 1].zone_start_index - 1;
        }

        Ok(instruments)
    }
}
