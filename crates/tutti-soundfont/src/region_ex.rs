// Derived from rustysynth 1.3.6 (MIT), Copyright (C) 2022 Nobuaki Tanaka, after
// MeltySynth (Copyright (C) 2021 Nobuaki Tanaka). The MIT notice is
// LICENSE-RUSTYSYNTH.txt, at the root of this crate.

use crate::lfo::Lfo;
use crate::modulation_envelope::ModulationEnvelope;
use crate::oscillator::Oscillator;
use crate::region_pair::RegionPair;
use crate::soundfont_math::SoundFontMath;
use crate::volume_envelope::VolumeEnvelope;

#[non_exhaustive]
pub(crate) struct RegionEx {}

impl RegionEx {
    pub(crate) fn start_oscillator(oscillator: &mut Oscillator, region: &RegionPair) {
        let sample_rate = region.instrument.sample_rate().get() as i32;
        let loop_mode = region.sample_modes();
        let start = region.sample_start();
        let end = region.sample_end();
        let start_loop = region.sample_start_loop();
        let end_loop = region.sample_end_loop();
        let root_key = region.root_key();
        let coarse_tune = region.coarse_tune();
        let fine_tune = region.fine_tune();
        let scale_tuning = region.scale_tuning();

        oscillator.start(
            loop_mode,
            sample_rate,
            start,
            end,
            start_loop,
            end_loop,
            root_key,
            coarse_tune,
            fine_tune,
            scale_tuning,
        );
    }

    pub(crate) fn start_volume_envelope(
        envelope: &mut VolumeEnvelope,
        region: &RegionPair,
        key: i32,
        _velocity: i32,
    ) {
        // If the release time is shorter than 10 ms, it will be clamped to 10 ms to avoid pop noise.

        let delay = region.delay_volume_envelope();
        let attack = region.attack_volume_envelope();
        let hold = region.hold_volume_envelope()
            * SoundFontMath::key_number_to_multiplying_factor(
                region.key_number_to_volume_envelope_hold(),
                key,
            );
        let decay = region.decay_volume_envelope()
            * SoundFontMath::key_number_to_multiplying_factor(
                region.key_number_to_volume_envelope_decay(),
                key,
            );
        let sustain = SoundFontMath::decibels_to_linear(-region.sustain_volume_envelope());
        let release = region.release_time();

        envelope.start(delay, attack, hold, decay, sustain, release);
    }

    pub(crate) fn start_modulation_envelope(
        envelope: &mut ModulationEnvelope,
        region: &RegionPair,
        key: i32,
        velocity: i32,
    ) {
        // According to the implementation of TinySoundFont, the attack time should be adjusted by the velocity.

        let delay = region.delay_modulation_envelope();
        let attack = region.attack_modulation_envelope() * ((145 - velocity) as f32 / 144_f32);
        let hold = region.hold_modulation_envelope()
            * SoundFontMath::key_number_to_multiplying_factor(
                region.key_number_to_modulation_envelope_hold(),
                key,
            );
        let decay = region.decay_modulation_envelope()
            * SoundFontMath::key_number_to_multiplying_factor(
                region.key_number_to_modulation_envelope_decay(),
                key,
            );
        let sustain = 1_f32 - region.sustain_modulation_envelope() / 100_f32;
        let release = region.release_modulation_envelope();

        envelope.start(delay, attack, hold, decay, sustain, release);
    }

    pub(crate) fn start_vibrato(lfo: &mut Lfo, region: &RegionPair, _key: i32, _velocity: i32) {
        lfo.start(region.delay_vibrato_lfo(), region.frequency_vibrato_lfo());
    }

    pub(crate) fn start_modulation(lfo: &mut Lfo, region: &RegionPair, _key: i32, _velocity: i32) {
        lfo.start(
            region.delay_modulation_lfo(),
            region.frequency_modulation_lfo(),
        );
    }
}
