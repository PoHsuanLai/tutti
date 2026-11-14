use crate::generator_type::{GeneratorType, WrongLevelGenerator};

/// A generator that may appear in an instrument zone: every [`GeneratorType`]
/// except `instrument`, which SF2 2.04 section 8.1.2 limits to preset zones. A
/// record of it in an instrument zone is ignored when the file is read, so an
/// [`InstrumentRegion`](crate::InstrumentRegion) holds none.
macro_rules! instrument_generators {
    ($($kind:ident,)*) => {
        /// A generator an instrument zone may hold.
        #[derive(Debug, Clone, Copy, PartialEq, Hash, Eq)]
        #[non_exhaustive]
        pub enum InstrumentGenerator {
            $(
                #[doc = concat!("[`GeneratorType::", stringify!($kind), "`].")]
                $kind,
            )*
        }

        impl From<InstrumentGenerator> for GeneratorType {
            fn from(generator: InstrumentGenerator) -> Self {
                match generator {
                    $(InstrumentGenerator::$kind => GeneratorType::$kind,)*
                }
            }
        }

        impl TryFrom<GeneratorType> for InstrumentGenerator {
            type Error = WrongLevelGenerator;

            fn try_from(generator: GeneratorType) -> Result<Self, Self::Error> {
                match generator {
                    $(GeneratorType::$kind => Ok(Self::$kind),)*
                    GeneratorType::Instrument => Err(WrongLevelGenerator::new(generator)),
                }
            }
        }
    };
}

instrument_generators! {
        StartAddressOffset,
        EndAddressOffset,
        StartLoopAddressOffset,
        EndLoopAddressOffset,
        StartAddressCoarseOffset,
        ModulationLfoToPitch,
        VibratoLfoToPitch,
        ModulationEnvelopeToPitch,
        InitialFilterCutoffFrequency,
        InitialFilterQ,
        ModulationLfoToFilterCutoffFrequency,
        ModulationEnvelopeToFilterCutoffFrequency,
        EndAddressCoarseOffset,
        ModulationLfoToVolume,
        ChorusEffectsSend,
        ReverbEffectsSend,
        Pan,
        DelayModulationLfo,
        FrequencyModulationLfo,
        DelayVibratoLfo,
        FrequencyVibratoLfo,
        DelayModulationEnvelope,
        AttackModulationEnvelope,
        HoldModulationEnvelope,
        DecayModulationEnvelope,
        SustainModulationEnvelope,
        ReleaseModulationEnvelope,
        KeyNumberToModulationEnvelopeHold,
        KeyNumberToModulationEnvelopeDecay,
        DelayVolumeEnvelope,
        AttackVolumeEnvelope,
        HoldVolumeEnvelope,
        DecayVolumeEnvelope,
        SustainVolumeEnvelope,
        ReleaseVolumeEnvelope,
        KeyNumberToVolumeEnvelopeHold,
        KeyNumberToVolumeEnvelopeDecay,
        KeyRange,
        VelocityRange,
        StartLoopAddressCoarseOffset,
        KeyNumber,
        Velocity,
        InitialAttenuation,
        EndLoopAddressCoarseOffset,
        CoarseTune,
        FineTune,
        SampleId,
        SampleModes,
        ScaleTuning,
        ExclusiveClass,
        OverridingRootKey,
}

#[cfg(test)]
mod tests {
    use super::InstrumentGenerator;
    use crate::generator_type::{GeneratorType, WrongLevelGenerator};

    /// SF2 2.04 section 8.1.2: `instrument` is the one generator an instrument zone
    /// cannot hold; the other 51 convert, each back to itself.
    /// Spec: sf2.pdta.igen.instrument.illegal
    #[test]
    fn the_instrument_generator_cannot_be_an_instrument_level_generator() {
        let mut instrument_level = 0;
        for id in 0..=u16::MAX {
            let Some(generator) = GeneratorType::from_id(id) else {
                continue;
            };
            match InstrumentGenerator::try_from(generator) {
                Ok(kind) => {
                    assert_ne!(generator, GeneratorType::Instrument);
                    assert_eq!(GeneratorType::from(kind), generator);
                    instrument_level += 1;
                }
                Err(error) => {
                    assert_eq!(generator, GeneratorType::Instrument);
                    assert_eq!(error, WrongLevelGenerator::new(generator));
                }
            }
        }
        assert_eq!(instrument_level, 51);
    }
}
