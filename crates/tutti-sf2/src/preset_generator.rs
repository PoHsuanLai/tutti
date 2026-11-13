use crate::generator_type::GeneratorType;

/// The error of converting an instrument-only [`GeneratorType`] to a
/// [`PresetGenerator`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("{0:?} is a generator of instrument zones only")]
pub struct InstrumentOnlyGenerator(GeneratorType);

macro_rules! preset_generators {
    ($($kind:ident,)*) => {
        /// A generator that may appear in a preset zone: every [`GeneratorType`]
        /// except those SF2 2.04 sections 8.1.2 and 8.1.3 limit to instrument
        /// zones (the sample address offsets, `keynum`, `velocity`, `sampleModes`,
        /// `exclusiveClass`, `overridingRootKey` and `sampleID`). A record of one of
        /// those in a preset zone is ignored when the file is read, so a
        /// [`PresetRegion`](crate::PresetRegion) holds none.
        #[derive(Debug, Clone, Copy, PartialEq, Hash, Eq)]
        #[non_exhaustive]
        pub enum PresetGenerator {
            $(
                #[doc = concat!("[`GeneratorType::", stringify!($kind), "`].")]
                $kind,
            )*
        }

        impl From<PresetGenerator> for GeneratorType {
            fn from(generator: PresetGenerator) -> Self {
                match generator {
                    $(PresetGenerator::$kind => GeneratorType::$kind,)*
                }
            }
        }

        impl TryFrom<GeneratorType> for PresetGenerator {
            type Error = InstrumentOnlyGenerator;

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
                    GeneratorType::SampleId => Err(InstrumentOnlyGenerator(generator)),
                }
            }
        }
    };
}

preset_generators! {
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
        Instrument,
        KeyRange,
        VelocityRange,
        InitialAttenuation,
        CoarseTune,
        FineTune,
        ScaleTuning,
}

#[cfg(test)]
mod tests {
    use super::{InstrumentOnlyGenerator, PresetGenerator};
    use crate::generator_type::GeneratorType;

    /// SF2 2.04 sections 8.1.2 and 8.1.3: the sample address offsets (fine and
    /// coarse), `keynum`, `velocity`, `sampleModes`, `exclusiveClass`,
    /// `overridingRootKey` and `sampleID` are instrument-only, and no other
    /// generator is: 38 of the 52 convert, each back to itself.
    #[test]
    fn an_instrument_only_generator_cannot_be_a_preset_generator() {
        let instrument_only = [
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
        ];
        let mut preset_level = 0;
        for id in 0..=u16::MAX {
            let Some(generator) = GeneratorType::from_id(id) else {
                continue;
            };
            match PresetGenerator::try_from(generator) {
                Ok(preset) => {
                    assert!(!instrument_only.contains(&generator), "{generator:?}");
                    assert_eq!(GeneratorType::from(preset), generator);
                    preset_level += 1;
                }
                Err(error) => {
                    assert!(instrument_only.contains(&generator), "{generator:?}");
                    assert_eq!(error, InstrumentOnlyGenerator(generator));
                }
            }
        }
        assert_eq!(preset_level, 38);
    }
}
