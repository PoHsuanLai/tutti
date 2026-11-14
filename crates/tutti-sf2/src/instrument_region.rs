use std::ops::RangeInclusive;

use std::num::NonZeroU8;

use tutti_types::{Cents, SampleRate, Samples};
use tutti_ump::{NoteNumber, U7};

use crate::error::Error;
use crate::generator_range;
use crate::generator_type::GeneratorType;
use crate::instrument_generator::InstrumentGenerator;
use crate::loop_mode::LoopMode;
use crate::sample_header::playing_rate;
use crate::sample_header::{SampleHeader, Side};
use crate::zone::Zone;

/// Contains all SoundFont generator parameters needed to synthesize a note.
#[derive(Debug, Clone)]
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
    pub(crate) sample_side: Option<Side>,
}

impl InstrumentRegion {
    fn new(
        instrument_id: usize,
        global: &Zone<InstrumentGenerator>,
        local: &Zone<InstrumentGenerator>,
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

        for &(kind, value) in global.generators.iter().chain(&local.generators) {
            gs[GeneratorType::from(kind) as usize] = value;
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
            sample_side: sample.side(),
        })
    }

    pub(crate) fn create(
        instrument_id: usize,
        zones: &[Zone<InstrumentGenerator>],
        samples: &[SampleHeader],
    ) -> Result<Vec<InstrumentRegion>, Error> {
        // Is the first one the global zone?
        if zones[0].generators.is_empty()
            || zones[0].generators.last().unwrap().0 != InstrumentGenerator::SampleId
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
    pub fn generator(&self, generator: InstrumentGenerator) -> i16 {
        self.gs[GeneratorType::from(generator) as usize]
    }

    /// The keys the region sounds on.
    #[must_use]
    pub fn key_range(&self) -> RangeInclusive<u8> {
        generator_range(self.generator(InstrumentGenerator::KeyRange))
    }

    /// The velocities the region sounds at.
    #[must_use]
    pub fn velocity_range(&self) -> RangeInclusive<u8> {
        generator_range(self.generator(InstrumentGenerator::VelocityRange))
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
    // sample data in a defective file, which `SoundFont::from_reader` refuses and a
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

    fn address_offset(&self, coarse: InstrumentGenerator, fine: InstrumentGenerator) -> i32 {
        32768 * i32::from(self.generator(coarse)) + i32::from(self.generator(fine))
    }

    /// The start address offset, in frames. Signed, a relative count (SF2 2.04
    /// section 9.3): no unit type covers a count that can be negative.
    #[must_use]
    pub(crate) fn start_address_offset(&self) -> i32 {
        self.address_offset(
            InstrumentGenerator::StartAddressCoarseOffset,
            InstrumentGenerator::StartAddressOffset,
        )
    }

    /// The end address offset, in frames.
    #[must_use]
    pub(crate) fn end_address_offset(&self) -> i32 {
        self.address_offset(
            InstrumentGenerator::EndAddressCoarseOffset,
            InstrumentGenerator::EndAddressOffset,
        )
    }

    /// The loop start address offset, in frames.
    #[must_use]
    pub(crate) fn start_loop_address_offset(&self) -> i32 {
        self.address_offset(
            InstrumentGenerator::StartLoopAddressCoarseOffset,
            InstrumentGenerator::StartLoopAddressOffset,
        )
    }

    /// The loop end address offset, in frames.
    #[must_use]
    pub(crate) fn end_loop_address_offset(&self) -> i32 {
        self.address_offset(
            InstrumentGenerator::EndLoopAddressCoarseOffset,
            InstrumentGenerator::EndLoopAddressOffset,
        )
    }

    /// The rate the sample plays at: the rate its header states, or
    /// [`NEAREST_PRACTICAL_RATE`](crate::NEAREST_PRACTICAL_RATE) when it states 0.
    #[must_use]
    pub fn sample_rate(&self) -> SampleRate {
        playing_rate(self.sample_sample_rate)
    }

    /// The sample header's pitch correction.
    #[must_use]
    pub fn sample_pitch_correction(&self) -> Cents {
        Cents(self.sample_pitch_correction as f32)
    }

    /// How the sample loops.
    #[must_use]
    pub fn sample_modes(&self) -> LoopMode {
        LoopMode::from_i16(self.generator(InstrumentGenerator::SampleModes))
    }

    /// The exclusive class (generator 57), or `None` for class 0. A value
    /// outside the generator's 0 to 127 reads as none as well.
    #[must_use]
    pub fn exclusive_class(&self) -> Option<NonZeroU8> {
        let class = self.generator(InstrumentGenerator::ExclusiveClass);
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
        match self.generator(InstrumentGenerator::OverridingRootKey) {
            -1 => self.sample_original_pitch,
            key => i32::from(key),
        }
    }

    /// The side the region's sample is meant for (section 7.10), when it is
    /// one of a stereo pair's.
    #[must_use]
    pub fn sample_side(&self) -> Option<Side> {
        self.sample_side
    }

    /// The key generator 46 (`keynum`) forces for the region, if the zone sets
    /// one (section 8.1.2: instrument level only, 0 to 127). A value outside
    /// the range reads as none.
    #[must_use]
    pub fn forced_key(&self) -> Option<NoteNumber> {
        u8::try_from(self.generator(InstrumentGenerator::KeyNumber))
            .ok()
            .and_then(NoteNumber::new)
    }

    /// The velocity generator 47 (`velocity`) forces for the region, if the
    /// zone sets one (instrument level only, 1 to 127). A value outside the
    /// range reads as none.
    #[must_use]
    pub fn forced_velocity(&self) -> Option<U7> {
        u8::try_from(self.generator(InstrumentGenerator::Velocity))
            .ok()
            .filter(|v| *v >= 1)
            .and_then(U7::new)
    }

    /// The index of the region's sample header.
    #[must_use]
    pub fn sample_id(&self) -> usize {
        usize::from(self.generator(InstrumentGenerator::SampleId) as u16)
    }
}
