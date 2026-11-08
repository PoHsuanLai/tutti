// Derived from rustysynth 1.3.6 (MIT), Copyright (C) 2022 Nobuaki Tanaka, after
// MeltySynth (Copyright (C) 2021 Nobuaki Tanaka). The MIT notice is
// LICENSE-RUSTYSYNTH.txt, at the root of this crate.

use std::f32::consts;

/// The longest an exclusive class's quick release takes, in seconds, to fall
/// 100 dB as every release time does (section 8.1.2, generator 38): the floor
/// every release is given at its start, to avoid a pop. The spec says "rapidly"
/// and no more (SF2 2.04 section 8.1.2, generator 57); this is tutti's figure,
/// the shortest release a voice can have.
pub(crate) const QUICK_RELEASE: f32 = 0.01;

use crate::bi_quad_filter::BiQuadFilter;
use crate::channel::Channel;
use crate::lfo::Lfo;
use crate::modulation_envelope::ModulationEnvelope;
use crate::oscillator::Oscillator;
use crate::region_ex::RegionEx;
use crate::region_pair::RegionPair;
use crate::render::Render;
use crate::soundfont_math::SoundFontMath;
use crate::volume_envelope::VolumeEnvelope;

#[derive(Debug, Clone, PartialEq, Eq)]
enum VoiceState {
    Playing,
    Released,
}

#[derive(Debug, Clone)]
#[non_exhaustive]
pub(crate) struct Voice {
    vol_env: VolumeEnvelope,
    mod_env: ModulationEnvelope,

    vib_lfo: Lfo,
    mod_lfo: Lfo,

    oscillator: Oscillator,
    filter: BiQuadFilter,

    block: Vec<f32>,

    // A sudden change in the mix gain will cause pop noise.
    // To avoid this, we save the mix gain of the previous block,
    // and smooth out the gain if the gap between the current and previous gain is too large.
    // The actual smoothing process is done in the WriteBlock method of the Synthesizer class.
    pub(crate) previous_mix_gain_left: f32,
    pub(crate) previous_mix_gain_right: f32,
    pub(crate) current_mix_gain_left: f32,
    pub(crate) current_mix_gain_right: f32,

    pub(crate) previous_reverb_send: f32,
    pub(crate) previous_chorus_send: f32,
    pub(crate) current_reverb_send: f32,
    pub(crate) current_chorus_send: f32,

    channel: i32,
    key: i32,
    velocity: i32,

    note_gain: f32,

    cutoff: f32,
    resonance: f32,

    vib_lfo_to_pitch: f32,
    mod_lfo_to_pitch: f32,
    mod_env_to_pitch: f32,

    mod_lfo_to_cutoff: i32,
    mod_env_to_cutoff: i32,
    dynamic_cutoff: bool,

    mod_lfo_to_volume: f32,
    dynamic_volume: bool,

    instrument_pan: f32,
    instrument_reverb: f32,
    instrument_chorus: f32,

    // Some instruments require fast cutoff change, which can cause pop noise.
    // This is used to smooth out the cutoff frequency.
    smoothed_cutoff: f32,

    voice_state: VoiceState,
    /// Time elapsed in samples, so a voice's first block takes its gains whole.
    voice_length: usize,
}

impl Voice {
    pub(crate) fn new(settings: &Render) -> Self {
        Self {
            vol_env: VolumeEnvelope::new(settings),
            mod_env: ModulationEnvelope::new(settings),
            vib_lfo: Lfo::new(settings),
            mod_lfo: Lfo::new(settings),
            oscillator: Oscillator::new(settings),
            filter: BiQuadFilter::new(settings),
            block: vec![0_f32; settings.block_len()],
            previous_mix_gain_left: 0_f32,
            previous_mix_gain_right: 0_f32,
            current_mix_gain_left: 0_f32,
            current_mix_gain_right: 0_f32,
            previous_reverb_send: 0_f32,
            previous_chorus_send: 0_f32,
            current_reverb_send: 0_f32,
            current_chorus_send: 0_f32,
            channel: 0,
            key: 0,
            velocity: 0,
            note_gain: 0_f32,
            cutoff: 0_f32,
            resonance: 0_f32,
            vib_lfo_to_pitch: 0_f32,
            mod_lfo_to_pitch: 0_f32,
            mod_env_to_pitch: 0_f32,
            mod_lfo_to_cutoff: 0,
            mod_env_to_cutoff: 0,
            dynamic_cutoff: false,
            mod_lfo_to_volume: 0_f32,
            dynamic_volume: false,
            instrument_pan: 0_f32,
            instrument_reverb: 0_f32,
            instrument_chorus: 0_f32,
            smoothed_cutoff: 0_f32,
            voice_state: VoiceState::Playing,
            voice_length: 0,
        }
    }

    pub(crate) fn start(&mut self, region: &RegionPair, channel: i32, key: i32, velocity: i32) {
        self.channel = channel;
        self.key = key;
        self.velocity = velocity;

        if velocity > 0 {
            // According to the Polyphone's implementation, the initial attenuation should be reduced to 40%.
            // I'm not sure why, but this indeed improves the loudness variability.
            let sample_attenuation = 0.4_f32 * region.initial_attenuation();
            let filter_attenuation = 0.5_f32 * region.initial_filter_q();
            let decibels = 2_f32 * SoundFontMath::linear_to_decibels(velocity as f32 / 127_f32)
                - sample_attenuation
                - filter_attenuation;
            self.note_gain = SoundFontMath::decibels_to_linear(decibels);
        } else {
            self.note_gain = 0_f32;
        }

        self.cutoff = region.initial_filter_cutoff_frequency();
        self.resonance = SoundFontMath::decibels_to_linear(region.initial_filter_q());

        self.vib_lfo_to_pitch = 0.01_f32 * region.vibrato_lfo_to_pitch() as f32;
        self.mod_lfo_to_pitch = 0.01_f32 * region.modulation_lfo_to_pitch() as f32;
        self.mod_env_to_pitch = 0.01_f32 * region.modulation_envelope_to_pitch() as f32;

        self.mod_lfo_to_cutoff = region.modulation_lfo_to_filter_cutoff_frequency();
        self.mod_env_to_cutoff = region.modulation_envelope_to_filter_cutoff_frequency();
        self.dynamic_cutoff = self.mod_lfo_to_cutoff != 0 || self.mod_env_to_cutoff != 0;

        self.mod_lfo_to_volume = region.modulation_lfo_to_volume();
        self.dynamic_volume = self.mod_lfo_to_volume > 0.05_f32;

        self.instrument_pan = SoundFontMath::clamp(region.pan(), -50_f32, 50_f32);
        self.instrument_reverb = 0.01_f32 * region.reverb_effects_send();
        self.instrument_chorus = 0.01_f32 * region.chorus_effects_send();

        RegionEx::start_volume_envelope(&mut self.vol_env, region, key, velocity);
        RegionEx::start_modulation_envelope(&mut self.mod_env, region, key, velocity);
        RegionEx::start_vibrato(&mut self.vib_lfo, region, key, velocity);
        RegionEx::start_modulation(&mut self.mod_lfo, region, key, velocity);
        RegionEx::start_oscillator(&mut self.oscillator, region);
        self.filter.clear_buffer();
        self.filter.set_low_pass_filter(self.cutoff, self.resonance);

        self.smoothed_cutoff = self.cutoff;

        self.voice_state = VoiceState::Playing;
        self.voice_length = 0;
    }

    /// Lets the voice go into its release at once (SF2 2.04 section 9.1.7: the
    /// envelope enters its release phase from its current value, so a voice
    /// released in its delay stage, where that value is 0, never sounds). The
    /// volume and modulation envelopes' release stages start, and a loop that
    /// runs until the key is up ends. A voice already released is left alone.
    pub(crate) fn release(&mut self) {
        if self.voice_state == VoiceState::Playing {
            self.vol_env.release();
            self.mod_env.release();
            self.oscillator.release();
            self.voice_state = VoiceState::Released;
        }
    }

    /// Lets the voice go into the quick release of an exclusive class (SF2
    /// 2.04 section 8.1.2, generator 57: "rapidly terminated"): its volume
    /// envelope falls over [`QUICK_RELEASE`] at the longest, whatever its
    /// own release is. A voice already releasing is sped up, never slowed.
    pub(crate) fn damp(&mut self) {
        self.vol_env.release_within(QUICK_RELEASE);
        self.release();
    }

    /// The volume envelope's level now: what the core steals by.
    pub(crate) fn level(&self) -> f32 {
        self.vol_env.value()
    }

    pub(crate) fn process(&mut self, data: &[i16], channels: &[Channel]) -> bool {
        if self.note_gain < SoundFontMath::NON_AUDIBLE {
            return false;
        }

        let channel_info = &channels[self.channel as usize];

        if !self.vol_env.process(self.block.len()) {
            return false;
        }

        self.mod_env.process(self.block.len());
        self.vib_lfo.process();
        self.mod_lfo.process();

        let vib_pitch_change =
            (0.01_f32 * channel_info.modulation() + self.vib_lfo_to_pitch) * self.vib_lfo.value();
        let mod_pitch_change = self.mod_lfo_to_pitch * self.mod_lfo.value()
            + self.mod_env_to_pitch * self.mod_env.value();
        let channel_pitch_change = channel_info.tune() + channel_info.pitch_bend();
        let pitch = self.key as f32 + vib_pitch_change + mod_pitch_change + channel_pitch_change;
        if !self.oscillator.process(data, &mut self.block[..], pitch) {
            return false;
        }

        if self.dynamic_cutoff {
            let cents = self.mod_lfo_to_cutoff as f32 * self.mod_lfo.value()
                + self.mod_env_to_cutoff as f32 * self.mod_env.value();
            let factor = SoundFontMath::cents_to_multiplying_factor(cents);
            let new_cutoff = factor * self.cutoff;

            // The cutoff change is limited within x0.5 and x2 to reduce pop noise.
            let lower_limit = 0.5_f32 * self.smoothed_cutoff;
            let upper_limit = 2_f32 * self.smoothed_cutoff;
            self.smoothed_cutoff = SoundFontMath::clamp(new_cutoff, lower_limit, upper_limit);

            self.filter
                .set_low_pass_filter(self.smoothed_cutoff, self.resonance);
        }
        self.filter.process(&mut self.block[..]);

        self.previous_mix_gain_left = self.current_mix_gain_left;
        self.previous_mix_gain_right = self.current_mix_gain_right;
        self.previous_reverb_send = self.current_reverb_send;
        self.previous_chorus_send = self.current_chorus_send;

        // According to the GM spec, the following value should be squared.
        let ve = channel_info.volume() * channel_info.expression();
        let channel_gain = ve * ve;

        let mut mix_gain = self.note_gain * channel_gain * self.vol_env.value();
        if self.dynamic_volume {
            let decibels = self.mod_lfo_to_volume * self.mod_lfo.value();
            mix_gain *= SoundFontMath::decibels_to_linear(decibels);
        }

        let angle = (consts::PI / 200_f32) * (channel_info.pan() + self.instrument_pan + 50_f32);
        if angle <= 0_f32 {
            self.current_mix_gain_left = mix_gain;
            self.current_mix_gain_right = 0_f32;
        } else if angle >= SoundFontMath::HALF_PI {
            self.current_mix_gain_left = 0_f32;
            self.current_mix_gain_right = mix_gain;
        } else {
            self.current_mix_gain_left = mix_gain * angle.cos();
            self.current_mix_gain_right = mix_gain * angle.sin();
        }

        self.current_reverb_send = SoundFontMath::clamp(
            channel_info.reverb_send() + self.instrument_reverb,
            0_f32,
            1_f32,
        );
        self.current_chorus_send = SoundFontMath::clamp(
            channel_info.chorus_send() + self.instrument_chorus,
            0_f32,
            1_f32,
        );

        if self.voice_length == 0 {
            self.previous_mix_gain_left = self.current_mix_gain_left;
            self.previous_mix_gain_right = self.current_mix_gain_right;
            self.previous_reverb_send = self.current_reverb_send;
            self.previous_chorus_send = self.current_chorus_send;
        }

        self.voice_length += self.block.len();

        true
    }

    pub(crate) fn block(&self) -> &[f32] {
        &self.block
    }
}
