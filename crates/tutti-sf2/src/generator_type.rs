/// The SoundFont 2.04 generator numbers (section 8.1.2), the index of a
/// generator in a region's table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    /// `unused1`, generator 14.
    Unused1 = 14,
    /// `chorusEffectsSend`, generator 15.
    ChorusEffectsSend = 15,
    /// `reverbEffectsSend`, generator 16.
    ReverbEffectsSend = 16,
    /// `pan`, generator 17.
    Pan = 17,
    /// `unused2`, generator 18.
    Unused2 = 18,
    /// `unused3`, generator 19.
    Unused3 = 19,
    /// `unused4`, generator 20.
    Unused4 = 20,
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
    /// `reserved1`, generator 42.
    Reserved1 = 42,
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
    /// `reserved2`, generator 49.
    Reserved2 = 49,
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
    /// `reserved3`, generator 55.
    Reserved3 = 55,
    /// `scaleTuning`, generator 56.
    ScaleTuning = 56,
    /// `exclusiveClass`, generator 57.
    ExclusiveClass = 57,
    /// `overridingRootKey`, generator 58.
    OverridingRootKey = 58,
    /// `unused5`, generator 59.
    Unused5 = 59,
    /// `endOper`, generator 60.
    UnusedEnd = 60,
}

impl GeneratorType {
    /// The number of generator slots a region keeps.
    pub const COUNT: usize = 61;
}
