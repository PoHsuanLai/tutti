use tutti_sf2::{GeneratorType, InstrumentRegion, LoopMode, PresetRegion};

use crate::soundfont_math::SoundFontMath;

#[non_exhaustive]
pub(crate) struct RegionPair<'a> {
    pub(crate) preset: &'a PresetRegion,
    pub(crate) instrument: &'a InstrumentRegion,
}

impl<'a> RegionPair<'a> {
    pub(crate) fn new(preset: &'a PresetRegion, instrument: &'a InstrumentRegion) -> Self {
        Self { preset, instrument }
    }

    fn gs(&self, generator: GeneratorType) -> i32 {
        i32::from(self.preset.generator(generator))
            + i32::from(self.instrument.generator(generator))
    }

    pub(crate) fn get_sample_start(&self) -> i32 {
        self.instrument.sample_start()
    }

    pub(crate) fn get_sample_end(&self) -> i32 {
        self.instrument.sample_end()
    }

    pub(crate) fn get_sample_start_loop(&self) -> i32 {
        self.instrument.sample_start_loop()
    }

    pub(crate) fn get_sample_end_loop(&self) -> i32 {
        self.instrument.sample_end_loop()
    }

    pub(crate) fn get_modulation_lfo_to_pitch(&self) -> i32 {
        self.gs(GeneratorType::ModulationLfoToPitch)
    }

    pub(crate) fn get_vibrato_lfo_to_pitch(&self) -> i32 {
        self.gs(GeneratorType::VibratoLfoToPitch)
    }

    pub(crate) fn get_modulation_envelope_to_pitch(&self) -> i32 {
        self.gs(GeneratorType::ModulationEnvelopeToPitch)
    }

    pub(crate) fn get_initial_filter_cutoff_frequency(&self) -> f32 {
        SoundFontMath::cents_to_hertz(self.gs(GeneratorType::InitialFilterCutoffFrequency) as f32)
    }

    pub(crate) fn get_initial_filter_q(&self) -> f32 {
        0.1_f32 * self.gs(GeneratorType::InitialFilterQ) as f32
    }

    pub(crate) fn get_modulation_lfo_to_filter_cutoff_frequency(&self) -> i32 {
        self.gs(GeneratorType::ModulationLfoToFilterCutoffFrequency)
    }

    pub(crate) fn get_modulation_envelope_to_filter_cutoff_frequency(&self) -> i32 {
        self.gs(GeneratorType::ModulationEnvelopeToFilterCutoffFrequency)
    }

    pub(crate) fn get_modulation_lfo_to_volume(&self) -> f32 {
        0.1_f32 * self.gs(GeneratorType::ModulationLfoToVolume) as f32
    }

    pub(crate) fn get_chorus_effects_send(&self) -> f32 {
        0.1_f32 * self.gs(GeneratorType::ChorusEffectsSend) as f32
    }

    pub(crate) fn get_reverb_effects_send(&self) -> f32 {
        0.1_f32 * self.gs(GeneratorType::ReverbEffectsSend) as f32
    }

    pub(crate) fn get_pan(&self) -> f32 {
        0.1_f32 * self.gs(GeneratorType::Pan) as f32
    }

    pub(crate) fn get_delay_modulation_lfo(&self) -> f32 {
        SoundFontMath::timecents_to_seconds(self.gs(GeneratorType::DelayModulationLfo) as f32)
    }

    pub(crate) fn get_frequency_modulation_lfo(&self) -> f32 {
        SoundFontMath::cents_to_hertz(self.gs(GeneratorType::FrequencyModulationLfo) as f32)
    }

    pub(crate) fn get_delay_vibrato_lfo(&self) -> f32 {
        SoundFontMath::timecents_to_seconds(self.gs(GeneratorType::DelayVibratoLfo) as f32)
    }

    pub(crate) fn get_frequency_vibrato_lfo(&self) -> f32 {
        SoundFontMath::cents_to_hertz(self.gs(GeneratorType::FrequencyVibratoLfo) as f32)
    }

    pub(crate) fn get_delay_modulation_envelope(&self) -> f32 {
        SoundFontMath::timecents_to_seconds(self.gs(GeneratorType::DelayModulationEnvelope) as f32)
    }

    pub(crate) fn get_attack_modulation_envelope(&self) -> f32 {
        SoundFontMath::timecents_to_seconds(self.gs(GeneratorType::AttackModulationEnvelope) as f32)
    }

    pub(crate) fn get_hold_modulation_envelope(&self) -> f32 {
        SoundFontMath::timecents_to_seconds(self.gs(GeneratorType::HoldModulationEnvelope) as f32)
    }

    pub(crate) fn get_decay_modulation_envelope(&self) -> f32 {
        SoundFontMath::timecents_to_seconds(self.gs(GeneratorType::DecayModulationEnvelope) as f32)
    }

    pub(crate) fn get_sustain_modulation_envelope(&self) -> f32 {
        0.1_f32 * self.gs(GeneratorType::SustainModulationEnvelope) as f32
    }

    pub(crate) fn get_release_modulation_envelope(&self) -> f32 {
        SoundFontMath::timecents_to_seconds(self.gs(GeneratorType::ReleaseModulationEnvelope) as f32)
    }

    pub(crate) fn get_key_number_to_modulation_envelope_hold(&self) -> i32 {
        self.gs(GeneratorType::KeyNumberToModulationEnvelopeHold)
    }

    pub(crate) fn get_key_number_to_modulation_envelope_decay(&self) -> i32 {
        self.gs(GeneratorType::KeyNumberToModulationEnvelopeDecay)
    }

    pub(crate) fn get_delay_volume_envelope(&self) -> f32 {
        SoundFontMath::timecents_to_seconds(self.gs(GeneratorType::DelayVolumeEnvelope) as f32)
    }

    pub(crate) fn get_attack_volume_envelope(&self) -> f32 {
        SoundFontMath::timecents_to_seconds(self.gs(GeneratorType::AttackVolumeEnvelope) as f32)
    }

    pub(crate) fn get_hold_volume_envelope(&self) -> f32 {
        SoundFontMath::timecents_to_seconds(self.gs(GeneratorType::HoldVolumeEnvelope) as f32)
    }

    pub(crate) fn get_decay_volume_envelope(&self) -> f32 {
        SoundFontMath::timecents_to_seconds(self.gs(GeneratorType::DecayVolumeEnvelope) as f32)
    }

    pub(crate) fn get_sustain_volume_envelope(&self) -> f32 {
        0.1_f32 * self.gs(GeneratorType::SustainVolumeEnvelope) as f32
    }

    pub(crate) fn get_release_volume_envelope(&self) -> f32 {
        SoundFontMath::timecents_to_seconds(self.gs(GeneratorType::ReleaseVolumeEnvelope) as f32)
    }

    pub(crate) fn get_key_number_to_volume_envelope_hold(&self) -> i32 {
        self.gs(GeneratorType::KeyNumberToVolumeEnvelopeHold)
    }

    pub(crate) fn get_key_number_to_volume_envelope_decay(&self) -> i32 {
        self.gs(GeneratorType::KeyNumberToVolumeEnvelopeDecay)
    }

    pub(crate) fn get_initial_attenuation(&self) -> f32 {
        0.1_f32 * self.gs(GeneratorType::InitialAttenuation) as f32
    }

    pub(crate) fn get_coarse_tune(&self) -> i32 {
        self.gs(GeneratorType::CoarseTune)
    }

    pub(crate) fn get_fine_tune(&self) -> i32 {
        self.gs(GeneratorType::FineTune) + self.instrument.sample_pitch_correction()
    }

    pub(crate) fn get_sample_modes(&self) -> LoopMode {
        self.instrument.sample_modes()
    }

    pub(crate) fn get_scale_tuning(&self) -> i32 {
        self.gs(GeneratorType::ScaleTuning)
    }

    pub(crate) fn get_exclusive_class(&self) -> i32 {
        self.instrument.exclusive_class()
    }

    pub(crate) fn get_root_key(&self) -> i32 {
        self.instrument.root_key()
    }
}
