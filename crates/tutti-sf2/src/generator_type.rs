/// The SoundFont 2.04 generator numbers (section 8.1.2), the index of a
/// generator in a region's table.
#[derive(Debug, Clone, Copy, PartialEq, Hash, Eq)]
#[repr(u8)]
#[non_exhaustive]
pub enum GeneratorType {
    /// `startAddrsOffset`, generator 0.
    StartAddressOffset = 0,
    /// `endAddrsOffset`, generator 1.
    EndAddressOffset = 1,
    /// `startloopAddrsOffset`, generator 2.
    StartLoopAddressOffset = 2,
    /// `endloopAddrsOffset`, generator 3.
    EndLoopAddressOffset = 3,
    /// `startAddrsCoarseOffset`, generator 4.
    StartAddressCoarseOffset = 4,
    /// `modLfoToPitch`, generator 5.
    ModulationLfoToPitch = 5,
    /// `vibLfoToPitch`, generator 6.
    VibratoLfoToPitch = 6,
    /// `modEnvToPitch`, generator 7.
    ModulationEnvelopeToPitch = 7,
    /// `initialFilterFc`, generator 8.
    InitialFilterCutoffFrequency = 8,
    /// `initialFilterQ`, generator 9.
    InitialFilterQ = 9,
    /// `modLfoToFilterFc`, generator 10.
    ModulationLfoToFilterCutoffFrequency = 10,
    /// `modEnvToFilterFc`, generator 11.
    ModulationEnvelopeToFilterCutoffFrequency = 11,
    /// `endAddrsCoarseOffset`, generator 12.
    EndAddressCoarseOffset = 12,
    /// `modLfoToVolume`, generator 13.
    ModulationLfoToVolume = 13,
    /// `chorusEffectsSend`, generator 15.
    ChorusEffectsSend = 15,
    /// `reverbEffectsSend`, generator 16.
    ReverbEffectsSend = 16,
    /// `pan`, generator 17.
    Pan = 17,
    /// `delayModLFO`, generator 21.
    DelayModulationLfo = 21,
    /// `freqModLFO`, generator 22.
    FrequencyModulationLfo = 22,
    /// `delayVibLFO`, generator 23.
    DelayVibratoLfo = 23,
    /// `freqVibLFO`, generator 24.
    FrequencyVibratoLfo = 24,
    /// `delayModEnv`, generator 25.
    DelayModulationEnvelope = 25,
    /// `attackModEnv`, generator 26.
    AttackModulationEnvelope = 26,
    /// `holdModEnv`, generator 27.
    HoldModulationEnvelope = 27,
    /// `decayModEnv`, generator 28.
    DecayModulationEnvelope = 28,
    /// `sustainModEnv`, generator 29.
    SustainModulationEnvelope = 29,
    /// `releaseModEnv`, generator 30.
    ReleaseModulationEnvelope = 30,
    /// `keynumToModEnvHold`, generator 31.
    KeyNumberToModulationEnvelopeHold = 31,
    /// `keynumToModEnvDecay`, generator 32.
    KeyNumberToModulationEnvelopeDecay = 32,
    /// `delayVolEnv`, generator 33.
    DelayVolumeEnvelope = 33,
    /// `attackVolEnv`, generator 34.
    AttackVolumeEnvelope = 34,
    /// `holdVolEnv`, generator 35.
    HoldVolumeEnvelope = 35,
    /// `decayVolEnv`, generator 36.
    DecayVolumeEnvelope = 36,
    /// `sustainVolEnv`, generator 37.
    SustainVolumeEnvelope = 37,
    /// `releaseVolEnv`, generator 38.
    ReleaseVolumeEnvelope = 38,
    /// `keynumToVolEnvHold`, generator 39.
    KeyNumberToVolumeEnvelopeHold = 39,
    /// `keynumToVolEnvDecay`, generator 40.
    KeyNumberToVolumeEnvelopeDecay = 40,
    /// `instrument`, generator 41.
    Instrument = 41,
    /// `keyRange`, generator 43.
    KeyRange = 43,
    /// `velRange`, generator 44.
    VelocityRange = 44,
    /// `startloopAddrsCoarseOffset`, generator 45.
    StartLoopAddressCoarseOffset = 45,
    /// `keynum`, generator 46.
    KeyNumber = 46,
    /// `velocity`, generator 47.
    Velocity = 47,
    /// `initialAttenuation`, generator 48.
    InitialAttenuation = 48,
    /// `endloopAddrsCoarseOffset`, generator 50.
    EndLoopAddressCoarseOffset = 50,
    /// `coarseTune`, generator 51.
    CoarseTune = 51,
    /// `fineTune`, generator 52.
    FineTune = 52,
    /// `sampleID`, generator 53.
    SampleId = 53,
    /// `sampleModes`, generator 54.
    SampleModes = 54,
    /// `scaleTuning`, generator 56.
    ScaleTuning = 56,
    /// `exclusiveClass`, generator 57.
    ExclusiveClass = 57,
    /// `overridingRootKey`, generator 58.
    OverridingRootKey = 58,
}

impl GeneratorType {
    /// The number of generator slots a region keeps: one per generator number up to
    /// the last named one.
    pub const COUNT: usize = Self::OverridingRootKey as usize + 1;

    /// The generator of number `id`, or `None` for a number the spec leaves unused or
    /// reserved, and for one past the last generator: section 8.1.2 has a file's
    /// reader ignore those.
    pub(crate) fn from_id(id: u16) -> Option<Self> {
        match id {
            0 => Some(Self::StartAddressOffset),
            1 => Some(Self::EndAddressOffset),
            2 => Some(Self::StartLoopAddressOffset),
            3 => Some(Self::EndLoopAddressOffset),
            4 => Some(Self::StartAddressCoarseOffset),
            5 => Some(Self::ModulationLfoToPitch),
            6 => Some(Self::VibratoLfoToPitch),
            7 => Some(Self::ModulationEnvelopeToPitch),
            8 => Some(Self::InitialFilterCutoffFrequency),
            9 => Some(Self::InitialFilterQ),
            10 => Some(Self::ModulationLfoToFilterCutoffFrequency),
            11 => Some(Self::ModulationEnvelopeToFilterCutoffFrequency),
            12 => Some(Self::EndAddressCoarseOffset),
            13 => Some(Self::ModulationLfoToVolume),
            15 => Some(Self::ChorusEffectsSend),
            16 => Some(Self::ReverbEffectsSend),
            17 => Some(Self::Pan),
            21 => Some(Self::DelayModulationLfo),
            22 => Some(Self::FrequencyModulationLfo),
            23 => Some(Self::DelayVibratoLfo),
            24 => Some(Self::FrequencyVibratoLfo),
            25 => Some(Self::DelayModulationEnvelope),
            26 => Some(Self::AttackModulationEnvelope),
            27 => Some(Self::HoldModulationEnvelope),
            28 => Some(Self::DecayModulationEnvelope),
            29 => Some(Self::SustainModulationEnvelope),
            30 => Some(Self::ReleaseModulationEnvelope),
            31 => Some(Self::KeyNumberToModulationEnvelopeHold),
            32 => Some(Self::KeyNumberToModulationEnvelopeDecay),
            33 => Some(Self::DelayVolumeEnvelope),
            34 => Some(Self::AttackVolumeEnvelope),
            35 => Some(Self::HoldVolumeEnvelope),
            36 => Some(Self::DecayVolumeEnvelope),
            37 => Some(Self::SustainVolumeEnvelope),
            38 => Some(Self::ReleaseVolumeEnvelope),
            39 => Some(Self::KeyNumberToVolumeEnvelopeHold),
            40 => Some(Self::KeyNumberToVolumeEnvelopeDecay),
            41 => Some(Self::Instrument),
            43 => Some(Self::KeyRange),
            44 => Some(Self::VelocityRange),
            45 => Some(Self::StartLoopAddressCoarseOffset),
            46 => Some(Self::KeyNumber),
            47 => Some(Self::Velocity),
            48 => Some(Self::InitialAttenuation),
            50 => Some(Self::EndLoopAddressCoarseOffset),
            51 => Some(Self::CoarseTune),
            52 => Some(Self::FineTune),
            53 => Some(Self::SampleId),
            54 => Some(Self::SampleModes),
            56 => Some(Self::ScaleTuning),
            57 => Some(Self::ExclusiveClass),
            58 => Some(Self::OverridingRootKey),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::GeneratorType;

    /// The numbers section 8.1.2 leaves unused or reserved (14, 18 to 20, 42, 49, 55,
    /// 59), `endOper` (60) and every number past it name no generator; every named
    /// generator is found by its own number.
    #[test]
    fn a_number_names_a_generator_only_where_the_spec_defines_one() {
        for id in [14, 18, 19, 20, 42, 49, 55, 59, 60, 61, 1000, u16::MAX] {
            assert_eq!(GeneratorType::from_id(id), None, "generator number {id}");
        }
        let named: Vec<u16> = (0..=u16::MAX)
            .filter(|&id| GeneratorType::from_id(id).is_some())
            .collect();
        assert_eq!(
            named.len(),
            52,
            "the 59 numbers 0 to 58, less the 7 unused or reserved"
        );
        for id in named {
            let g = GeneratorType::from_id(id).unwrap();
            assert_eq!(g as u16, id, "{g:?}");
        }
    }
}
