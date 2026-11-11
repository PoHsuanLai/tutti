// Derived from rustysynth 1.3.6 (MIT), Copyright (C) 2022 Nobuaki Tanaka, after
// MeltySynth (Copyright (C) 2021 Nobuaki Tanaka). The MIT notice is
// LICENSE-RUSTYSYNTH.txt, at the root of this crate.

use std::f32::consts;

use tutti_dsp::{Biquad, DirectForm1, SOLVE_CEILING, SvfControls, SvfModel, SvfType};
use tutti_midi_protocol::u32_to_unit_f32;
use tutti_sf2::Side;
use tutti_types::{Db, Hz, Q, SampleRate, Semitones};
use tutti_ump::{MidiChannel, RegisteredPerNoteController};
use tutti_voicing::NoteControl;

/// The longest an exclusive class's quick release takes, in seconds, to fall
/// 100 dB as every release time does (section 8.1.2, generator 38): the floor
/// every release is given at its start, to avoid a pop. The spec says "rapidly"
/// and no more (SF2 2.04 section 8.1.2, generator 57); this is tutti's figure,
/// the shortest release a voice can have.
pub(crate) const QUICK_RELEASE: f32 = 0.01;

use crate::channel::Channel;
use crate::lfo::Lfo;
use crate::modulation_envelope::ModulationEnvelope;
use crate::oscillator::Oscillator;
use crate::region_ex::RegionEx;
use crate::region_pair::RegionPair;
use crate::render::Render;
use crate::soundfont_math::SoundFontMath;
use crate::volume_envelope::VolumeEnvelope;

/// The voice's low-pass for a cutoff in hertz and a resonance as a linear peak
/// height above the DC gain (SF2 2.04 section 8.1.3, generators 8 and 9: the
/// cutoff is the filter's corner and the Q is the height of the resonant peak
/// in decibels, here already converted). `None` from [`SOLVE_CEILING`] of the
/// rate up: the filter would do nothing there.
///
/// The `Q` handed to the design is not the peak height: a 2-pole low-pass of
/// quality `q` peaks a little above `q`, so the figure is lowered by
/// rustysynth's fit, whose peak is within 3% of the one asked for. A resonance
/// of 1 (0 dB) lands on Butterworth.
fn low_pass(cutoff: f32, resonance: f32, rate: SampleRate) -> Option<Biquad> {
    const PEAK_OFFSET: f32 = 1.0 - core::f32::consts::FRAC_1_SQRT_2;
    if f64::from(cutoff) >= SOLVE_CEILING * rate.get() {
        return None;
    }
    let q = resonance - PEAK_OFFSET / (1.0 + 6.0 * (resonance - 1.0));
    let controls = SvfControls {
        ty: SvfType::LowPass,
        freq: Hz(cutoff),
        q: Q(q),
        gain: Db(0.0),
    };
    Some(SvfModel::new(controls, rate).biquad())
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum VoiceState {
    Playing,
    Released,
}

/// What the note's per-note controllers have set.
#[derive(Debug, Clone, Copy)]
struct NoteControls {
    /// The sounding pitch in semitones (the told pitch with the per-note bend
    /// in it, `tutti-voicing`'s), if one was set; else the key's.
    pitch: Option<f32>,
    /// Per-note volume, `0..=1`.
    volume: f32,
    /// Per-note pan in the channel's `-50..=50`, if one was set.
    pan: Option<f32>,
}

impl NoteControls {
    const DEFAULT: Self = Self {
        pitch: None,
        volume: 1_f32,
        pan: None,
    };
}

#[derive(Debug, Clone)]
#[non_exhaustive]
pub(crate) struct Voice {
    vol_env: VolumeEnvelope,
    mod_env: ModulationEnvelope,

    vib_lfo: Lfo,
    mod_lfo: Lfo,

    oscillator: Oscillator,
    rate: SampleRate,
    /// The low-pass, or `None` while the cutoff is too near Nyquist to filter.
    low_pass: Option<Biquad>,
    history: DirectForm1,

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

    // The note's per-note controllers (M2-104 section 7.4); the defaults of
    // `NoteControls::DEFAULT` until a message sets one.
    note: NoteControls,

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
            rate: SampleRate(f64::from(settings.hz())),
            low_pass: None,
            history: DirectForm1::default(),
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
            note: NoteControls::DEFAULT,
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
        // Generators 46 and 47 (SF2 2.04 section 8.1.2) force the key and the
        // velocity of the zone: everything keyed on either reads the forced one.
        let key = region
            .instrument
            .forced_key()
            .map_or(key, |k| i32::from(k.get()));
        let velocity = region
            .instrument
            .forced_velocity()
            .map_or(velocity, |v| i32::from(v.get()));
        self.channel = channel;
        self.key = key;
        self.velocity = velocity;
        self.note = NoteControls::DEFAULT;

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

        // A sample of a stereo pair sounds on its own side (section 7.10); any
        // other takes the zone's pan.
        self.instrument_pan = match region.instrument.sample_side() {
            Some(Side::Left) => -50_f32,
            Some(Side::Right) => 50_f32,
            None => SoundFontMath::clamp(region.pan(), -50_f32, 50_f32),
        };
        // Tenths of a percent to a fraction of full level (section 8.1.2,
        // generators 15 and 16).
        self.instrument_reverb = 0.001_f32 * region.reverb_effects_send() as f32;
        self.instrument_chorus = 0.001_f32 * region.chorus_effects_send() as f32;

        RegionEx::start_volume_envelope(&mut self.vol_env, region, key, velocity);
        RegionEx::start_modulation_envelope(&mut self.mod_env, region, key, velocity);
        RegionEx::start_vibrato(&mut self.vib_lfo, region, key, velocity);
        RegionEx::start_modulation(&mut self.mod_lfo, region, key, velocity);
        RegionEx::start_oscillator(&mut self.oscillator, region);
        self.history.reset();
        self.low_pass = low_pass(self.cutoff, self.resonance, self.rate);

        self.smoothed_cutoff = self.cutoff;

        self.voice_state = VoiceState::Playing;
        self.voice_length = 0;
    }

    /// Takes a per-note controller's value (M2-104 section 7.4). What a
    /// SoundFont voice can carry: the note's sounding pitch (its told Pitch
    /// 7.25 or Attribute Pitch 7.9 and its per-note bend, summed by
    /// `tutti-voicing`; it replaces its key's, and the key still
    /// chooses the sample, envelope scaling and zone, as the voice was
    /// started), Registered Volume (the GM law, as CC 7)
    /// and Registered Pan (as CC 10, replacing the channel's pan for this
    /// note). Everything else (pressure, modulation, breath, balance, the
    /// sound controllers, the effects depths, the assignable controllers) has
    /// no generator in a SoundFont voice to drive and is dropped.
    pub(crate) fn control(&mut self, control: NoteControl) {
        match control {
            NoteControl::Pitch(pitch) => self.note.pitch = Some(pitch.note_number()),
            NoteControl::Registered(RegisteredPerNoteController::Volume, value) => {
                self.note.volume = u32_to_unit_f32(value);
            }
            NoteControl::Registered(RegisteredPerNoteController::Pan, value) => {
                self.note.pan = Some(100_f32 * u32_to_unit_f32(value) - 50_f32);
            }
            NoteControl::Pressure(_)
            | NoteControl::Registered(..)
            | NoteControl::Assignable(..) => {}
        }
    }

    /// Per-Note Management Reset (M2-104 section 7.4.5): the note's per-note
    /// controllers return to their defaults.
    pub(crate) fn reset_controls(&mut self) {
        self.note = NoteControls::DEFAULT;
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

    pub(crate) fn process(
        &mut self,
        data: &[i16],
        channels: &[Channel],
        bends: &[Semitones; MidiChannel::COUNT as usize],
    ) -> bool {
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
        let channel_pitch_change = channel_info.tune() + bends[self.channel as usize].0;
        let pitch = self.note.pitch.unwrap_or(self.key as f32)
            + vib_pitch_change
            + mod_pitch_change
            + channel_pitch_change;
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

            self.low_pass = low_pass(self.smoothed_cutoff, self.resonance, self.rate);
        }
        match &self.low_pass {
            Some(section) => self.history.process(section, &mut self.block[..]),
            None => self.history.hold(&self.block[..]),
        }

        self.previous_mix_gain_left = self.current_mix_gain_left;
        self.previous_mix_gain_right = self.current_mix_gain_right;
        self.previous_reverb_send = self.current_reverb_send;
        self.previous_chorus_send = self.current_chorus_send;

        // According to the GM spec, the following value should be squared.
        let ve = channel_info.volume() * channel_info.expression();
        let channel_gain = ve * ve;

        // A per-note Volume is the channel's law (the GM square) applied to
        // the one note.
        let mut mix_gain = self.note_gain
            * channel_gain
            * self.note.volume
            * self.note.volume
            * self.vol_env.value();
        if self.dynamic_volume {
            let decibels = self.mod_lfo_to_volume * self.mod_lfo.value();
            mix_gain *= SoundFontMath::decibels_to_linear(decibels);
        }

        let pan = self.note.pan.unwrap_or_else(|| channel_info.pan());
        let angle = (consts::PI / 200_f32) * (pan + self.instrument_pan + 50_f32);
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

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: SampleRate = SampleRate(44_100.0);
    const CUTOFF: f32 = 500.0;

    /// The steady-state gain of the voice's low-pass at `hz`, measured by
    /// running a sine through the section the voice runs.
    fn gain(resonance: f32, hz: f32) -> f64 {
        let section = low_pass(CUTOFF, resonance, RATE).expect("below Nyquist");
        let mut state = DirectForm1::default();
        let w = core::f64::consts::TAU * f64::from(hz) / RATE.get();
        let mut peak = 0.0f64;
        for chunk in 0..1_000 {
            let mut block: Vec<f32> = (0..8)
                .map(|n| (w * f64::from(chunk * 8 + n)).sin() as f32)
                .collect();
            state.process(&section, &mut block);
            if chunk >= 800 {
                peak = block.iter().fold(peak, |p, &y| p.max(f64::from(y).abs()));
            }
        }
        peak
    }

    /// The RBJ low-pass closed form, from the analog prototype
    /// `1 / (s^2 + s / q + 1)` under the bilinear map: `|H|` at `hz` for a
    /// quality `q`, independent of the code under test.
    fn closed_form(q: f64, hz: f64) -> f64 {
        let s = (core::f64::consts::PI * hz / RATE.get()).tan()
            / (core::f64::consts::PI * f64::from(CUTOFF) / RATE.get()).tan();
        1.0 / ((1.0 - s * s).powi(2) + (s / q).powi(2)).sqrt()
    }

    /// The `q` the voice asks for at a resonance, SF2 2.04 section 8.1.3.
    fn q_of(resonance: f64) -> f64 {
        resonance - (1.0 - 0.5f64.sqrt()) / (1.0 + 6.0 * (resonance - 1.0))
    }

    /// SF2 2.04 section 8.1.3, initialFilterFc and initialFilterQ: the filter is
    /// a 2-pole low-pass whose gain at DC is 0 dB whatever the Q.
    #[test]
    fn the_low_pass_has_unity_gain_at_dc_whatever_the_resonance() {
        for db in [0.0f32, 6.0, 12.0, 24.0] {
            let g = gain(SoundFontMath::decibels_to_linear(db), 10.0);
            assert!((g - 1.0).abs() < 0.01, "{db} dB: {g}");
        }
    }

    /// At the cutoff the 2-pole response is `q` (RBJ: `|H(w0)| = Q`), and the
    /// resonant peak is the spec's initialFilterQ above DC, within the 3% the
    /// fit promises.
    #[test]
    fn the_gain_at_the_cutoff_is_q_and_the_peak_is_the_spec_peak_height() {
        for db in [0.0f32, 6.0, 12.0, 24.0] {
            let resonance = SoundFontMath::decibels_to_linear(db);
            let at_cutoff = gain(resonance, CUTOFF);
            let q = q_of(f64::from(resonance));
            assert!(
                (at_cutoff / q - 1.0).abs() < 0.02,
                "{db} dB: {at_cutoff} vs {q}"
            );
            let peak = (300..700)
                .step_by(5)
                .map(|hz| gain(resonance, hz as f32))
                .fold(0.0f64, f64::max);
            let want = f64::from(resonance);
            let peak_db = 20.0 * peak.log10();
            // 0 dB asks for no peak: the Butterworth maximum is 1.
            assert!(
                (peak / want - 1.0).abs() < 0.03 || (db == 0.0 && (peak_db < 0.1)),
                "{db} dB: peak {peak_db:.2} dB"
            );
        }
    }

    /// Above the cutoff the response follows the closed form and falls at
    /// 12 dB an octave (a 2-pole low-pass).
    #[test]
    fn the_stopband_follows_the_closed_form_at_twelve_db_per_octave() {
        let resonance = SoundFontMath::decibels_to_linear(6.0);
        let q = q_of(f64::from(resonance));
        for hz in [2_000.0f32, 4_000.0, 8_000.0] {
            let got = gain(resonance, hz);
            let want = closed_form(q, f64::from(hz));
            assert!((got / want - 1.0).abs() < 0.02, "{hz} Hz: {got} vs {want}");
        }
        let octave = 20.0 * (gain(resonance, 4_000.0) / gain(resonance, 2_000.0)).log10();
        assert!((octave + 12.0).abs() < 1.0, "{octave} dB per octave");
    }

    /// A cutoff at the solve ceiling or above is a bypass.
    #[test]
    fn a_cutoff_near_nyquist_is_no_filter() {
        assert!(low_pass(0.4 * RATE.get() as f32, 1.0, RATE).is_some());
        assert!(low_pass(0.5 * RATE.get() as f32, 1.0, RATE).is_none());
    }
}
