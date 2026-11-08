use tutti_ump::U7;

use crate::bank::Bank;
use crate::error::Error;
use crate::instrument::Instrument;
use crate::preset_info::PresetInfo;
use crate::preset_region::PresetRegion;
use crate::zone::Zone;

/// Represents a preset in the SoundFont.
#[derive(Debug)]
#[non_exhaustive]
pub struct Preset {
    pub(crate) name: String,
    pub(crate) patch_number: u16,
    pub(crate) bank_number: u16,
    pub(crate) library: i32,
    pub(crate) genre: i32,
    pub(crate) morphology: i32,
    pub(crate) regions: Vec<PresetRegion>,
}

impl Preset {
    fn new(
        info: &PresetInfo,
        preset_id: usize,
        zones: &[Zone],
        instruments: &[Instrument],
    ) -> Result<Self, Error> {
        let name = info.name.clone();

        let zone_count = info.zone_end_index - info.zone_start_index + 1;
        if zone_count <= 0 {
            return Err(Error::InvalidPreset(preset_id));
        }

        let span_start = info.zone_start_index as usize;
        let span_end = span_start + zone_count as usize;
        let zone_span = &zones[span_start..span_end];
        let regions = PresetRegion::create(preset_id, zone_span, instruments)?;

        Ok(Self {
            name,
            patch_number: info.patch_number,
            bank_number: info.bank_number,
            library: info.library,
            genre: info.genre,
            morphology: info.morphology,
            regions,
        })
    }

    pub(crate) fn create(
        infos: &[PresetInfo],
        zones: &[Zone],
        instruments: &[Instrument],
    ) -> Result<Vec<Preset>, Error> {
        if infos.len() <= 1 {
            return Err(Error::PresetNotFound);
        }

        // The last one is the terminator.
        let count = infos.len() - 1;

        let mut presets: Vec<Preset> = Vec::new();
        for (preset_id, info) in infos.iter().take(count).enumerate() {
            presets.push(Preset::new(info, preset_id, zones, instruments)?);
        }

        Ok(presets)
    }

    /// The name of the preset.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The MIDI program the preset answers (`wPreset`, 0 to 127), or `None`
    /// when the file's value is not a MIDI program: the preset cannot be played
    /// but is kept (SF2 2.04 section 7.2).
    #[must_use]
    pub fn patch(&self) -> Option<U7> {
        u8::try_from(self.patch_number).ok().and_then(U7::new)
    }

    /// The bank the preset answers (`wBank`), or `None` when the file's value
    /// is not one: the preset cannot be played but is kept (section 7.2).
    #[must_use]
    pub fn bank(&self) -> Option<Bank> {
        Bank::new(self.bank_number)
    }

    /// The library info.
    pub fn library(&self) -> i32 {
        self.library
    }

    /// The genre info.
    pub fn genre(&self) -> i32 {
        self.genre
    }

    /// The morphology info.
    pub fn morphology(&self) -> i32 {
        self.morphology
    }

    /// The regions of the preset.
    pub fn regions(&self) -> &[PresetRegion] {
        &self.regions[..]
    }
}
