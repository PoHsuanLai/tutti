// Derived from rustysynth 1.3.6 (MIT), Copyright (C) 2022 Nobuaki Tanaka, after
// MeltySynth (Copyright (C) 2021 Nobuaki Tanaka). The MIT notice is
// LICENSE-RUSTYSYNTH.txt, at the root of this crate.

use tutti_sf2::{GeneratorType, InstrumentRegion, LoopMode, PresetRegion};

use crate::soundfont_math::SoundFontMath;

#[non_exhaustive]
pub(crate) struct RegionPair<'a> {
    pub(crate) preset: &'a PresetRegion,
    pub(crate) instrument: &'a InstrumentRegion,
}

/// The range of a value generator, from the minimum and maximum columns of the
/// generator summary (SF2 2.04 section 8.1.3). `None` for what is not summed
/// into a voice's parameter: the address offsets (their range is the sample's),
/// the ranges, the indexes, the sample modes and the instrument-only
/// generators.
fn limits(generator: GeneratorType) -> Option<(i32, i32)> {
    use GeneratorType::*;
    Some(match generator {
        ModulationLfoToPitch
        | VibratoLfoToPitch
        | ModulationEnvelopeToPitch
        | ModulationLfoToFilterCutoffFrequency
        | ModulationEnvelopeToFilterCutoffFrequency => (-12000, 12000),
        InitialFilterCutoffFrequency => (1500, 13500),
        InitialFilterQ => (0, 960),
        ModulationLfoToVolume => (-960, 960),
        Pan => (-500, 500),
        DelayModulationLfo
        | DelayVibratoLfo
        | DelayModulationEnvelope
        | HoldModulationEnvelope
        | DelayVolumeEnvelope
        | HoldVolumeEnvelope => (-12000, 5000),
        FrequencyModulationLfo | FrequencyVibratoLfo => (-16000, 4500),
        AttackModulationEnvelope
        | DecayModulationEnvelope
        | ReleaseModulationEnvelope
        | AttackVolumeEnvelope
        | DecayVolumeEnvelope
        | ReleaseVolumeEnvelope => (-12000, 8000),
        SustainModulationEnvelope => (0, 1000),
        KeyNumberToModulationEnvelopeHold
        | KeyNumberToModulationEnvelopeDecay
        | KeyNumberToVolumeEnvelopeHold
        | KeyNumberToVolumeEnvelopeDecay => (-1200, 1200),
        SustainVolumeEnvelope | InitialAttenuation => (0, 1440),
        CoarseTune => (-120, 120),
        FineTune => (-99, 99),
        ScaleTuning => (0, 1200),
        _ => return None,
    })
}

impl<'a> RegionPair<'a> {
    pub(crate) fn new(preset: &'a PresetRegion, instrument: &'a InstrumentRegion) -> Self {
        Self { preset, instrument }
    }

    /// The generator's value in a voice: the preset's added to the instrument's
    /// (section 9.4), then replaced by the nearest value of its range when the
    /// sum is out of it (section 10.5; the ranges are section 8.1.3's).
    fn gs(&self, generator: GeneratorType) -> i32 {
        let sum = self.sum(generator);
        match limits(generator) {
            Some((min, max)) => sum.clamp(min, max),
            None => sum,
        }
    }

    /// The preset's generator added to the instrument's, in the generator's own
    /// units, before any range.
    fn sum(&self, generator: GeneratorType) -> i32 {
        i32::from(self.preset.generator(generator))
            + i32::from(self.instrument.generator(generator))
    }

    pub(crate) fn sample_start(&self) -> i32 {
        self.instrument.sample_start().get() as i32
    }

    pub(crate) fn sample_end(&self) -> i32 {
        self.instrument.sample_end().get() as i32
    }

    pub(crate) fn sample_start_loop(&self) -> i32 {
        self.instrument.sample_start_loop().get() as i32
    }

    pub(crate) fn sample_end_loop(&self) -> i32 {
        self.instrument.sample_end_loop().get() as i32
    }

    pub(crate) fn modulation_lfo_to_pitch(&self) -> i32 {
        self.gs(GeneratorType::ModulationLfoToPitch)
    }

    pub(crate) fn vibrato_lfo_to_pitch(&self) -> i32 {
        self.gs(GeneratorType::VibratoLfoToPitch)
    }

    pub(crate) fn modulation_envelope_to_pitch(&self) -> i32 {
        self.gs(GeneratorType::ModulationEnvelopeToPitch)
    }

    pub(crate) fn initial_filter_cutoff_frequency(&self) -> f32 {
        SoundFontMath::cents_to_hertz(self.gs(GeneratorType::InitialFilterCutoffFrequency) as f32)
    }

    pub(crate) fn initial_filter_q(&self) -> f32 {
        0.1_f32 * self.gs(GeneratorType::InitialFilterQ) as f32
    }

    pub(crate) fn modulation_lfo_to_filter_cutoff_frequency(&self) -> i32 {
        self.gs(GeneratorType::ModulationLfoToFilterCutoffFrequency)
    }

    pub(crate) fn modulation_envelope_to_filter_cutoff_frequency(&self) -> i32 {
        self.gs(GeneratorType::ModulationEnvelopeToFilterCutoffFrequency)
    }

    pub(crate) fn modulation_lfo_to_volume(&self) -> f32 {
        0.1_f32 * self.gs(GeneratorType::ModulationLfoToVolume) as f32
    }

    /// The chorus send generator's sum over both zones, in tenths of a percent
    /// and **not** limited to its range: the default modulator of CC 93 is
    /// added to this sum before the range is applied (section 8.4.9), which is
    /// the voice's to do.
    pub(crate) fn chorus_effects_send(&self) -> i32 {
        self.sum(GeneratorType::ChorusEffectsSend)
    }

    /// The reverb send generator's sum, as
    /// [`chorus_effects_send`](Self::chorus_effects_send) (section 8.4.8).
    pub(crate) fn reverb_effects_send(&self) -> i32 {
        self.sum(GeneratorType::ReverbEffectsSend)
    }

    pub(crate) fn pan(&self) -> f32 {
        0.1_f32 * self.gs(GeneratorType::Pan) as f32
    }

    pub(crate) fn delay_modulation_lfo(&self) -> f32 {
        SoundFontMath::timecents_to_seconds(self.gs(GeneratorType::DelayModulationLfo) as f32)
    }

    pub(crate) fn frequency_modulation_lfo(&self) -> f32 {
        SoundFontMath::cents_to_hertz(self.gs(GeneratorType::FrequencyModulationLfo) as f32)
    }

    pub(crate) fn delay_vibrato_lfo(&self) -> f32 {
        SoundFontMath::timecents_to_seconds(self.gs(GeneratorType::DelayVibratoLfo) as f32)
    }

    pub(crate) fn frequency_vibrato_lfo(&self) -> f32 {
        SoundFontMath::cents_to_hertz(self.gs(GeneratorType::FrequencyVibratoLfo) as f32)
    }

    pub(crate) fn delay_modulation_envelope(&self) -> f32 {
        SoundFontMath::timecents_to_seconds(self.gs(GeneratorType::DelayModulationEnvelope) as f32)
    }

    pub(crate) fn attack_modulation_envelope(&self) -> f32 {
        SoundFontMath::timecents_to_seconds(self.gs(GeneratorType::AttackModulationEnvelope) as f32)
    }

    pub(crate) fn hold_modulation_envelope(&self) -> f32 {
        SoundFontMath::timecents_to_seconds(self.gs(GeneratorType::HoldModulationEnvelope) as f32)
    }

    pub(crate) fn decay_modulation_envelope(&self) -> f32 {
        SoundFontMath::timecents_to_seconds(self.gs(GeneratorType::DecayModulationEnvelope) as f32)
    }

    pub(crate) fn sustain_modulation_envelope(&self) -> f32 {
        0.1_f32 * self.gs(GeneratorType::SustainModulationEnvelope) as f32
    }

    pub(crate) fn release_modulation_envelope(&self) -> f32 {
        SoundFontMath::timecents_to_seconds(self.gs(GeneratorType::ReleaseModulationEnvelope) as f32)
    }

    pub(crate) fn key_number_to_modulation_envelope_hold(&self) -> i32 {
        self.gs(GeneratorType::KeyNumberToModulationEnvelopeHold)
    }

    pub(crate) fn key_number_to_modulation_envelope_decay(&self) -> i32 {
        self.gs(GeneratorType::KeyNumberToModulationEnvelopeDecay)
    }

    pub(crate) fn delay_volume_envelope(&self) -> f32 {
        SoundFontMath::timecents_to_seconds(self.gs(GeneratorType::DelayVolumeEnvelope) as f32)
    }

    pub(crate) fn attack_volume_envelope(&self) -> f32 {
        SoundFontMath::timecents_to_seconds(self.gs(GeneratorType::AttackVolumeEnvelope) as f32)
    }

    pub(crate) fn hold_volume_envelope(&self) -> f32 {
        SoundFontMath::timecents_to_seconds(self.gs(GeneratorType::HoldVolumeEnvelope) as f32)
    }

    pub(crate) fn decay_volume_envelope(&self) -> f32 {
        SoundFontMath::timecents_to_seconds(self.gs(GeneratorType::DecayVolumeEnvelope) as f32)
    }

    pub(crate) fn sustain_volume_envelope(&self) -> f32 {
        0.1_f32 * self.gs(GeneratorType::SustainVolumeEnvelope) as f32
    }

    pub(crate) fn release_volume_envelope(&self) -> f32 {
        SoundFontMath::timecents_to_seconds(self.gs(GeneratorType::ReleaseVolumeEnvelope) as f32)
    }

    /// The volume envelope's release in seconds, as a voice starts it: never
    /// below the 10 ms that avoids a pop.
    pub(crate) fn release_time(&self) -> f32 {
        SoundFontMath::max(self.release_volume_envelope(), 0.01_f32)
    }

    pub(crate) fn key_number_to_volume_envelope_hold(&self) -> i32 {
        self.gs(GeneratorType::KeyNumberToVolumeEnvelopeHold)
    }

    pub(crate) fn key_number_to_volume_envelope_decay(&self) -> i32 {
        self.gs(GeneratorType::KeyNumberToVolumeEnvelopeDecay)
    }

    pub(crate) fn initial_attenuation(&self) -> f32 {
        0.1_f32 * self.gs(GeneratorType::InitialAttenuation) as f32
    }

    pub(crate) fn coarse_tune(&self) -> i32 {
        self.gs(GeneratorType::CoarseTune)
    }

    pub(crate) fn fine_tune(&self) -> i32 {
        self.gs(GeneratorType::FineTune) + self.instrument.sample_pitch_correction().get() as i32
    }

    pub(crate) fn sample_modes(&self) -> LoopMode {
        self.instrument.sample_modes()
    }

    pub(crate) fn scale_tuning(&self) -> i32 {
        self.gs(GeneratorType::ScaleTuning)
    }

    pub(crate) fn root_key(&self) -> i32 {
        self.instrument.root_key()
    }
}

/// SF2 2.04 section 9.4: the generators of a preset zone add to those of an
/// instrument zone in the generator's own unit, and section 9.3 says what each
/// unit is. The zones are built in memory.
#[cfg(test)]
mod tests {
    use super::RegionPair;
    use tutti_fixtures::sf2::{self as font, Inst, Preset, generator};

    /// The first pair of a font whose preset zone has `preset` generators and
    /// whose instrument zone has `instrument`.
    fn pair(preset: font::Zone, instrument: font::Zone) -> std::sync::Arc<tutti_sf2::SoundFont> {
        let bytes = font::bytes(
            &[Preset {
                name: "p",
                bank: 0,
                program: 0,
                zones: vec![(preset, 0)],
            }],
            &[Inst {
                name: "i",
                zones: vec![instrument],
            }],
        );
        std::sync::Arc::new(
            tutti_sf2::SoundFont::new(&mut std::io::Cursor::new(bytes))
                .expect("the built SoundFont parses"),
        )
    }

    fn with<R>(font: &tutti_sf2::SoundFont, f: impl FnOnce(&RegionPair<'_>) -> R) -> R {
        let preset_zone = &font.presets()[0].regions()[0];
        let instrument_zone = &font.instruments()[preset_zone.instrument_id()].regions()[0];
        f(&RegionPair::new(preset_zone, instrument_zone))
    }

    /// 9.4: a value generator at the preset level adds to the instrument's. The
    /// initial attenuation is in centibels (9.3), the pair reports decibels:
    /// 100 + 50 centibels is 15 dB.
    #[test]
    fn a_preset_generator_adds_to_the_instruments_in_its_own_unit() {
        let font = pair(
            vec![(generator::INITIAL_ATTENUATION, 100)],
            vec![(generator::INITIAL_ATTENUATION, 50)],
        );
        let attenuation = with(&font, |p| p.initial_attenuation());
        assert!((attenuation - 15.0).abs() < 1e-4, "{attenuation}");
    }

    /// 9.4 with 9.3: times add in timecents and are converted once. A delay of
    /// 1200 at the instrument and -1200 at the preset is zero timecents: one
    /// second, not the product of 2 s and 0.5 s computed any other way, and a
    /// zone with neither is the default (-12000, about a millisecond).
    #[test]
    fn times_add_in_timecents() {
        const DELAY_VOL_ENV: u16 = 33;
        let font = pair(vec![(DELAY_VOL_ENV, -1200)], vec![(DELAY_VOL_ENV, 1200)]);
        let delay = with(&font, |p| p.delay_volume_envelope());
        assert!((delay - 1.0).abs() < 1e-6, "{delay}");
        let default = pair(vec![], vec![]);
        let delay = with(&default, |p| p.delay_volume_envelope());
        assert!((delay - 2f32.powf(-10.0)).abs() < 1e-6, "{delay}");
    }

    /// SF2 2.04 sections 9.4 and 10.5: a sum of a preset and an instrument value
    /// that is out of the generator's range (section 8.1.3) is replaced by the
    /// nearest value of the range. A release of 8000 timecents (101.6 s, the
    /// range's top) at both levels is 8000, not 16000; an attenuation of 1000
    /// and 1000 centibels is the 1440 of the range (144 dB); a pan of -400 and
    /// -400 is hard left, -500. A sum inside the range is not touched.
    #[test]
    fn an_out_of_range_sum_is_replaced_by_the_nearest_value() {
        let both = |generator, amount| pair(vec![(generator, amount)], vec![(generator, amount)]);
        let font = both(generator::RELEASE_VOL_ENV, 8000);
        let release = with(&font, |p| p.release_time());
        assert!(
            (release - 2f32.powf(8000.0 / 1200.0)).abs() < 1e-2,
            "{release}"
        );
        let font = both(generator::INITIAL_ATTENUATION, 1000);
        assert_eq!(with(&font, |p| p.initial_attenuation()), 144.0);
        let font = both(17, -400);
        assert_eq!(with(&font, |p| p.pan()), -50.0);
        let font = pair(
            vec![(generator::RELEASE_VOL_ENV, 1200)],
            vec![(generator::RELEASE_VOL_ENV, 1200)],
        );
        let release = with(&font, |p| p.release_time());
        assert!((release - 4.0).abs() < 1e-4, "inside the range: {release}");
    }

    /// The release is never shorter than 10 ms, whatever the generators sum to
    /// (a tutti floor, against a pop): the default of about a millisecond is
    /// raised to it.
    #[test]
    fn a_release_is_at_least_ten_milliseconds() {
        let default = pair(vec![], vec![]);
        assert_eq!(with(&default, |p| p.release_time()), 0.01);
        let long = pair(vec![], vec![(generator::RELEASE_VOL_ENV, 1200)]);
        assert!((with(&long, |p| p.release_time()) - 2.0).abs() < 1e-5);
    }

    /// 9.3, absolute percent: pan and the effect sends are in 0.1% units, so a
    /// value of 500 is 50% (pan hard right is +50, hard left -50). The send is
    /// the sum in tenths, unlimited (the voice adds CC 91's modulator first).
    #[test]
    fn pan_and_sends_are_in_tenths_of_a_percent() {
        const PAN: u16 = 17;
        const REVERB_SEND: u16 = 16;
        let font = pair(vec![], vec![(PAN, -500), (REVERB_SEND, 250)]);
        assert!((with(&font, |p| p.pan()) + 50.0).abs() < 1e-4);
        assert_eq!(with(&font, |p| p.reverb_effects_send()), 250);
    }
}
