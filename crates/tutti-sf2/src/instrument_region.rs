use std::ops::RangeInclusive;

use std::num::NonZeroU8;

use tutti_types::{Cents, SampleRate, Samples};
use tutti_ump::{NoteNumber, U7};

use crate::error::Error;
use crate::generator::Generator;
use crate::generator_range;
use crate::generator_type::GeneratorType;
use crate::loop_mode::LoopMode;
use crate::sample_header::SampleHeader;
use crate::zone::Zone;

fn set_parameter(gs: &mut [i16; GeneratorType::COUNT], generator: &Generator) {
    let index = generator.generator_type as usize;

    // Unknown generators should be ignored.
    if index < gs.len() {
        gs[index] = generator.value as i16;
    }
}

/// Contains all SoundFont generator parameters needed to synthesize a note.
#[derive(Debug)]
#[non_exhaustive]
pub struct InstrumentRegion {
    pub(crate) gs: [i16; GeneratorType::COUNT],
    pub(crate) sample_start: u32,
    pub(crate) sample_end: u32,
    pub(crate) sample_start_loop: u32,
    pub(crate) sample_end_loop: u32,
    pub(crate) sample_sample_rate: u32,
    pub(crate) sample_original_pitch: i32,
    pub(crate) sample_pitch_correction: i32,
}

impl InstrumentRegion {
    fn new(
        instrument_id: usize,
        global: &Zone,
        local: &Zone,
        samples: &[SampleHeader],
    ) -> Result<Self, Error> {
        let mut gs: [i16; GeneratorType::COUNT] = [0; GeneratorType::COUNT];
        gs[GeneratorType::InitialFilterCutoffFrequency as usize] = 13500;
        gs[GeneratorType::DelayModulationLfo as usize] = -12000;
        gs[GeneratorType::DelayVibratoLfo as usize] = -12000;
        gs[GeneratorType::DelayModulationEnvelope as usize] = -12000;
        gs[GeneratorType::AttackModulationEnvelope as usize] = -12000;
        gs[GeneratorType::HoldModulationEnvelope as usize] = -12000;
        gs[GeneratorType::DecayModulationEnvelope as usize] = -12000;
        gs[GeneratorType::ReleaseModulationEnvelope as usize] = -12000;
        gs[GeneratorType::DelayVolumeEnvelope as usize] = -12000;
        gs[GeneratorType::AttackVolumeEnvelope as usize] = -12000;
        gs[GeneratorType::HoldVolumeEnvelope as usize] = -12000;
        gs[GeneratorType::DecayVolumeEnvelope as usize] = -12000;
        gs[GeneratorType::ReleaseVolumeEnvelope as usize] = -12000;
        gs[GeneratorType::KeyRange as usize] = 0x7F00;
        gs[GeneratorType::VelocityRange as usize] = 0x7F00;
        gs[GeneratorType::KeyNumber as usize] = -1;
        gs[GeneratorType::Velocity as usize] = -1;
        gs[GeneratorType::ScaleTuning as usize] = 100;
        gs[GeneratorType::OverridingRootKey as usize] = -1;

        for generator in global.generators.iter() {
            set_parameter(&mut gs, generator);
        }

        for generator in local.generators.iter() {
            set_parameter(&mut gs, generator);
        }

        // The generator is a 16-bit word holding an index; read as unsigned, so an
        // index of 32768 or more is not sign-extended into a huge one.
        let sample_id = usize::from(gs[GeneratorType::SampleId as usize] as u16);
        if sample_id >= samples.len() {
            return Err(Error::InvalidSampleId {
                instrument_id,
                sample_id,
            });
        }
        let sample = &samples[sample_id];

        Ok(Self {
            gs,
            sample_start: sample.start,
            sample_end: sample.end,
            sample_start_loop: sample.start_loop,
            sample_end_loop: sample.end_loop,
            sample_sample_rate: sample.sample_rate,
            sample_original_pitch: sample.original_pitch as i32,
            sample_pitch_correction: sample.pitch_correction as i32,
        })
    }

    pub(crate) fn create(
        instrument_id: usize,
        zones: &[Zone],
        samples: &[SampleHeader],
    ) -> Result<Vec<InstrumentRegion>, Error> {
        // Is the first one the global zone?
        if zones[0].generators.is_empty()
            || zones[0].generators.last().unwrap().generator_type != GeneratorType::SampleId as u16
        {
            // The first one is the global zone.
            let global = &zones[0];

            // The global zone is regarded as the base setting of subsequent zones.
            let count = zones.len() - 1;
            let mut regions: Vec<InstrumentRegion> = Vec::new();
            for i in 0..count {
                regions.push(InstrumentRegion::new(
                    instrument_id,
                    global,
                    &zones[i + 1],
                    samples,
                )?);
            }

            Ok(regions)
        } else {
            // No global zone.
            let count = zones.len();
            let mut regions: Vec<InstrumentRegion> = Vec::new();
            for zone in zones.iter().take(count) {
                regions.push(InstrumentRegion::new(
                    instrument_id,
                    &Zone::empty(),
                    zone,
                    samples,
                )?);
            }

            Ok(regions)
        }
    }

    /// Whether the region sounds `key` at `velocity`.
    #[must_use]
    pub fn contains(&self, key: NoteNumber, velocity: U7) -> bool {
        self.key_range().contains(&key.get()) && self.velocity_range().contains(&velocity.get())
    }

    /// The raw value of `generator`, in the unit the SoundFont 2.04 spec gives it.
    #[must_use]
    pub fn generator(&self, generator: GeneratorType) -> i16 {
        self.gs[generator as usize]
    }

    /// The keys the region sounds on.
    #[must_use]
    pub fn key_range(&self) -> RangeInclusive<u8> {
        generator_range(self.generator(GeneratorType::KeyRange))
    }

    /// The velocities the region sounds at.
    #[must_use]
    pub fn velocity_range(&self) -> RangeInclusive<u8> {
        generator_range(self.generator(GeneratorType::VelocityRange))
    }

    /// The first sample frame, with the start address offsets applied. The
    /// loader has checked that it is not before the sample data.
    #[must_use]
    pub fn sample_start(&self) -> Samples {
        Samples(self.raw_sample_start().max(0) as usize)
    }

    /// The last sample frame, with the end address offsets applied.
    #[must_use]
    pub fn sample_end(&self) -> Samples {
        Samples(self.raw_sample_end().max(0) as usize)
    }

    /// The loop start frame, with the loop address offsets applied.
    #[must_use]
    pub fn sample_start_loop(&self) -> Samples {
        Samples(self.raw_sample_start_loop().max(0) as usize)
    }

    /// The loop end frame, with the loop address offsets applied.
    #[must_use]
    pub fn sample_end_loop(&self) -> Samples {
        Samples(self.raw_sample_end_loop().max(0) as usize)
    }

    // The addresses as signed frame counts: an offset can take one before the
    // sample data in a defective file, which `SoundFont::new` refuses and a
    // `Samples` cannot say.
    pub(crate) fn raw_sample_start(&self) -> i64 {
        i64::from(self.sample_start) + i64::from(self.start_address_offset())
    }

    pub(crate) fn raw_sample_end(&self) -> i64 {
        i64::from(self.sample_end) + i64::from(self.end_address_offset())
    }

    pub(crate) fn raw_sample_start_loop(&self) -> i64 {
        i64::from(self.sample_start_loop) + i64::from(self.start_loop_address_offset())
    }

    pub(crate) fn raw_sample_end_loop(&self) -> i64 {
        i64::from(self.sample_end_loop) + i64::from(self.end_loop_address_offset())
    }

    fn address_offset(&self, coarse: GeneratorType, fine: GeneratorType) -> i32 {
        32768 * i32::from(self.generator(coarse)) + i32::from(self.generator(fine))
    }

    /// The start address offset, in frames. Signed, a relative count (SF2 2.04
    /// section 9.3): no unit type covers a count that can be negative.
    #[must_use]
    pub fn start_address_offset(&self) -> i32 {
        self.address_offset(
            GeneratorType::StartAddressCoarseOffset,
            GeneratorType::StartAddressOffset,
        )
    }

    /// The end address offset, in frames.
    #[must_use]
    pub fn end_address_offset(&self) -> i32 {
        self.address_offset(
            GeneratorType::EndAddressCoarseOffset,
            GeneratorType::EndAddressOffset,
        )
    }

    /// The loop start address offset, in frames.
    #[must_use]
    pub fn start_loop_address_offset(&self) -> i32 {
        self.address_offset(
            GeneratorType::StartLoopAddressCoarseOffset,
            GeneratorType::StartLoopAddressOffset,
        )
    }

    /// The loop end address offset, in frames.
    #[must_use]
    pub fn end_loop_address_offset(&self) -> i32 {
        self.address_offset(
            GeneratorType::EndLoopAddressCoarseOffset,
            GeneratorType::EndLoopAddressOffset,
        )
    }

    /// The rate the sample was recorded at.
    #[must_use]
    pub fn sample_rate(&self) -> SampleRate {
        SampleRate(f64::from(self.sample_sample_rate))
    }

    /// The sample header's pitch correction.
    #[must_use]
    pub fn sample_pitch_correction(&self) -> Cents {
        Cents(self.sample_pitch_correction as f32)
    }

    /// How the sample loops.
    #[must_use]
    pub fn sample_modes(&self) -> LoopMode {
        LoopMode::from_i16(self.generator(GeneratorType::SampleModes))
    }

    /// The exclusive class (generator 57), or `None` for class 0, "no exclusive
    /// class". A value outside the generator's 0 to 127 reads as none as well.
    #[must_use]
    pub fn exclusive_class(&self) -> Option<NonZeroU8> {
        let class = self.generator(GeneratorType::ExclusiveClass);
        u8::try_from(class)
            .ok()
            .filter(|c| *c <= 127)
            .and_then(NonZeroU8::new)
    }

    /// The key the sample sounds at its recorded pitch: the overriding root
    /// key if the region has one, else the sample header's. An `i32`, not a
    /// `NoteNumber`: the header's 255 means unpitched, which is not a key.
    #[must_use]
    pub fn root_key(&self) -> i32 {
        match self.generator(GeneratorType::OverridingRootKey) {
            -1 => self.sample_original_pitch,
            key => i32::from(key),
        }
    }

    /// The index of the region's sample header.
    #[must_use]
    pub fn sample_id(&self) -> usize {
        usize::from(self.generator(GeneratorType::SampleId) as u16)
    }
}
