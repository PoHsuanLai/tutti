use crate::generator_type::{GeneratorType, WrongLevelGenerator};
use crate::instrument_generator::InstrumentGenerator;
use crate::preset_generator::PresetGenerator;

macro_rules! summed_generators {
    ($($kind:ident,)*) => {
        /// A generator whose preset value is added onto its instrument value (SF2 2.04
        /// section 9.4): every value generator legal at both levels. The ranges
        /// intersect instead, and the address offsets, `keynum`, `velocity`,
        /// `sampleModes`, `exclusiveClass`, `overridingRootKey`, `sampleID` and
        /// `instrument` belong to one level only.
        #[derive(Debug, Clone, Copy, PartialEq, Hash, Eq)]
        #[non_exhaustive]
        pub enum SummedGenerator {
            $(
                #[doc = concat!("[`GeneratorType::", stringify!($kind), "`].")]
                $kind,
            )*
        }

        impl From<SummedGenerator> for GeneratorType {
            fn from(generator: SummedGenerator) -> Self {
                match generator {
                    $(SummedGenerator::$kind => GeneratorType::$kind,)*
                }
            }
        }

        impl From<SummedGenerator> for PresetGenerator {
            fn from(generator: SummedGenerator) -> Self {
                match generator {
                    $(SummedGenerator::$kind => PresetGenerator::$kind,)*
                }
            }
        }

        impl From<SummedGenerator> for InstrumentGenerator {
            fn from(generator: SummedGenerator) -> Self {
                match generator {
                    $(SummedGenerator::$kind => InstrumentGenerator::$kind,)*
                }
            }
        }

        impl TryFrom<GeneratorType> for SummedGenerator {
            type Error = WrongLevelGenerator;

            fn try_from(generator: GeneratorType) -> Result<Self, Self::Error> {
                match generator {
                    $(GeneratorType::$kind => Ok(Self::$kind),)*
                    GeneratorType::StartAddressOffset |
                    GeneratorType::EndAddressOffset |
                    GeneratorType::StartLoopAddressOffset |
                    GeneratorType::EndLoopAddressOffset |
                    GeneratorType::StartAddressCoarseOffset |
                    GeneratorType::EndAddressCoarseOffset |
                    GeneratorType::StartLoopAddressCoarseOffset |
                    GeneratorType::EndLoopAddressCoarseOffset |
                    GeneratorType::KeyNumber |
                    GeneratorType::Velocity |
                    GeneratorType::SampleModes |
                    GeneratorType::ExclusiveClass |
                    GeneratorType::OverridingRootKey |
                    GeneratorType::SampleId |
                    GeneratorType::Instrument |
                    GeneratorType::KeyRange |
                    GeneratorType::VelocityRange => Err(WrongLevelGenerator::new(generator)),
                }
            }
        }
    };
}

summed_generators! {
        ModulationLfoToPitch,
        VibratoLfoToPitch,
        ModulationEnvelopeToPitch,
        InitialFilterCutoffFrequency,
        InitialFilterQ,
        ModulationLfoToFilterCutoffFrequency,
        ModulationEnvelopeToFilterCutoffFrequency,
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
        InitialAttenuation,
        CoarseTune,
        FineTune,
        ScaleTuning,
}

#[cfg(test)]
mod tests {
    use super::SummedGenerator;
    use crate::generator_type::{GeneratorType, WrongLevelGenerator};
    use crate::instrument_generator::InstrumentGenerator;
    use crate::preset_generator::PresetGenerator;

    /// SF2 2.04 section 9.4: the preset value of a value generator adds to the
    /// instrument's. The 8 address offsets, `keynum`, `velocity`, `sampleModes`,
    /// `exclusiveClass`, `overridingRootKey`, `sampleID`, `instrument` and the two
    /// ranges are not summed; the other 35 convert, and each reads the same
    /// generator at the preset and the instrument level.
    #[test]
    fn only_a_value_generator_legal_at_both_levels_is_summed() {
        let not_summed = [
            GeneratorType::StartAddressOffset,
            GeneratorType::EndAddressOffset,
            GeneratorType::StartLoopAddressOffset,
            GeneratorType::EndLoopAddressOffset,
            GeneratorType::StartAddressCoarseOffset,
            GeneratorType::EndAddressCoarseOffset,
            GeneratorType::StartLoopAddressCoarseOffset,
            GeneratorType::EndLoopAddressCoarseOffset,
            GeneratorType::KeyNumber,
            GeneratorType::Velocity,
            GeneratorType::SampleModes,
            GeneratorType::ExclusiveClass,
            GeneratorType::OverridingRootKey,
            GeneratorType::SampleId,
            GeneratorType::Instrument,
            GeneratorType::KeyRange,
            GeneratorType::VelocityRange,
        ];
        let mut summed = 0;
        for id in 0..=u16::MAX {
            let Some(generator) = GeneratorType::from_id(id) else {
                continue;
            };
            match SummedGenerator::try_from(generator) {
                Ok(kind) => {
                    assert!(!not_summed.contains(&generator), "{generator:?}");
                    assert_eq!(GeneratorType::from(kind), generator);
                    assert_eq!(GeneratorType::from(PresetGenerator::from(kind)), generator);
                    assert_eq!(
                        GeneratorType::from(InstrumentGenerator::from(kind)),
                        generator
                    );
                    summed += 1;
                }
                Err(error) => {
                    assert!(not_summed.contains(&generator), "{generator:?}");
                    assert_eq!(error, WrongLevelGenerator::new(generator));
                }
            }
        }
        assert_eq!(summed, 35);
    }
}
