use std::io::Read;

use crate::binary_reader::ReadExt;
use crate::error::Error;

#[non_exhaustive]
pub(crate) struct PresetInfo {
    pub(crate) name: String,
    pub(crate) patch_number: u16,
    pub(crate) bank_number: u16,
    pub(crate) zone_start_index: i32,
    pub(crate) zone_end_index: i32,
    pub(crate) library: i32,
    pub(crate) genre: i32,
    pub(crate) morphology: i32,
}

impl PresetInfo {
    fn new<R: Read>(reader: &mut R) -> Result<Self, Error> {
        let name = reader.read_fixed_length_string(20)?;
        let patch_number = reader.read_u16()?;
        let bank_number = reader.read_u16()?;
        let zone_start_index = reader.read_u16()? as i32;
        let library = reader.read_i32()?;
        let genre = reader.read_i32()?;
        let morphology = reader.read_i32()?;

        Ok(Self {
            name,
            patch_number,
            bank_number,
            zone_start_index,
            zone_end_index: 0,
            library,
            genre,
            morphology,
        })
    }

    pub(crate) fn read_from_chunk<R: Read>(
        reader: &mut R,
        size: usize,
    ) -> Result<Vec<PresetInfo>, Error> {
        if size == 0 || !size.is_multiple_of(38) {
            return Err(Error::InvalidPresetList);
        }

        let count = size / 38;

        let mut presets: Vec<PresetInfo> = Vec::new();
        for _i in 0..count {
            presets.push(PresetInfo::new(reader)?);
        }

        for i in 0..(count - 1) {
            presets[i].zone_end_index = presets[i + 1].zone_start_index - 1;
        }

        Ok(presets)
    }
}
