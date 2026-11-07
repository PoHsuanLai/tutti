use std::ops::RangeInclusive;

use tutti_ump::{NoteNumber, U7};

use crate::error::SoundFontError;
use crate::generator::Generator;
use crate::generator_range;
use crate::generator_type::GeneratorType;
use crate::instrument::Instrument;
use crate::zone::Zone;

fn set_parameter(gs: &mut [i16; GeneratorType::COUNT], generator: &Generator) {
    let index = generator.generator_type as usize;

    // Unknown generators should be ignored.
    if index < gs.len() {
        gs[index] = generator.value as i16;
    }
}

/// Indicates how instrument parameters should be modified within a preset.
#[derive(Debug)]
#[non_exhaustive]
pub struct PresetRegion {
    pub(crate) gs: [i16; GeneratorType::COUNT],
    pub(crate) instrument: usize,
}

impl PresetRegion {
    fn new(
        preset_id: usize,
        global: &Zone,
        local: &Zone,
        samples: &[Instrument],
    ) -> Result<Self, SoundFontError> {
        let mut gs: [i16; GeneratorType::COUNT] = [0; GeneratorType::COUNT];
        gs[GeneratorType::KeyRange as usize] = 0x7F00;
        gs[GeneratorType::VelocityRange as usize] = 0x7F00;

        for generator in global.generators.iter() {
            set_parameter(&mut gs, generator);
        }

        for generator in local.generators.iter() {
            set_parameter(&mut gs, generator);
        }

        let instrument_id = gs[GeneratorType::Instrument as usize] as usize;
        if instrument_id >= samples.len() {
            return Err(SoundFontError::InvalidInstrumentId {
                preset_id,
                instrument_id,
            });
        }

        Ok(Self {
            gs,
            instrument: instrument_id,
        })
    }

    pub(crate) fn create(
        preset_id: usize,
        zones: &[Zone],
        instruments: &[Instrument],
    ) -> Result<Vec<PresetRegion>, SoundFontError> {
        // Is the first one the global zone?
        if zones[0].generators.is_empty()
            || zones[0].generators.last().unwrap().generator_type
                != GeneratorType::Instrument as u16
        {
            // The first one is the global zone.
            let global = &zones[0];

            // The global zone is regarded as the base setting of subsequent zones.
            let count = zones.len() - 1;
            let mut regions: Vec<PresetRegion> = Vec::new();
            for i in 0..count {
                regions.push(PresetRegion::new(
                    preset_id,
                    global,
                    &zones[i + 1],
                    instruments,
                )?);
            }

            Ok(regions)
        } else {
            // No global zone.
            let count = zones.len();
            let mut regions: Vec<PresetRegion> = Vec::new();
            for zone in zones.iter().take(count) {
                regions.push(PresetRegion::new(
                    preset_id,
                    &Zone::empty(),
                    zone,
                    instruments,
                )?);
            }

            Ok(regions)
        }
    }

    /// Whether the region applies to `key` at `velocity`.
    #[must_use]
    pub fn contains(&self, key: NoteNumber, velocity: U7) -> bool {
        self.key_range().contains(&key.get()) && self.velocity_range().contains(&velocity.get())
    }

    /// The raw value of `generator`, in the unit the SoundFont 2.04 spec gives it.
    #[must_use]
    pub fn generator(&self, generator: GeneratorType) -> i16 {
        self.gs[generator as usize]
    }

    /// The keys the region applies to.
    #[must_use]
    pub fn key_range(&self) -> RangeInclusive<u8> {
        generator_range(self.generator(GeneratorType::KeyRange))
    }

    /// The velocities the region applies to.
    #[must_use]
    pub fn velocity_range(&self) -> RangeInclusive<u8> {
        generator_range(self.generator(GeneratorType::VelocityRange))
    }

    /// The index of the region's instrument.
    #[must_use]
    pub fn instrument_id(&self) -> usize {
        self.instrument
    }
}
