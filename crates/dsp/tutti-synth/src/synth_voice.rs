//! Internal voice implementation for PolySynth.

use crate::{FilterModConfig, FilterType, OscillatorType, SvfMode, SynthConfig};
use crate::{MpeVoiceState, UnisonEngine};
use tutti_core::dsp::{
    adsr_live, bandpass_q, dc, highpass_q, lowpass_q, moog, notch_q, pass, pink, poly_pulse, saw,
    sine, triangle, var,
};
use tutti_core::{AudioUnit, Hz, Phase, PhaseIncrement, Semitones, Shared};

extern crate alloc;
use alloc::vec::Vec;

#[derive(Clone)]
struct SubVoice {
    pitch: Shared,
    dsp: Box<dyn AudioUnit>,
}

#[derive(Clone)]
pub(crate) struct SynthVoice {
    note: u8,
    channel: u8,
    velocity: f32,
    gate: Shared,
    filter_cutoff: Shared,
    base_filter_cutoff: f32,
    filter_resonance: Shared,
    base_filter_resonance: f32,
    mod_wheel_value: f32,
    velocity_mod_value: f32,
    cc_cutoff_value: f32,
    cc_resonance_value: f32,
    filter_mod: FilterModConfig,
    lfo_phase: Phase,
    envelope_level: f32,
    active: bool,
    mpe: MpeVoiceState,
    mpe_enabled: bool,
    mpe_pitch_bend_range: Semitones,
    base_note_freq: Hz,
    sub_voices: Vec<SubVoice>,
    config: SynthConfig,
    sample_rate: tutti_core::SampleRate,
}

impl SynthVoice {
    pub(crate) fn note(&self) -> u8 {
        self.note
    }
    pub(crate) fn channel(&self) -> u8 {
        self.channel
    }
    /// The voice's base (pre-bend) frequency — reflects any Pitch 7.25 / 7.9
    /// tuning override. Observability for tests.
    #[cfg(test)]
    pub(crate) fn base_note_freq(&self) -> Hz {
        self.base_note_freq
    }
    pub(crate) fn is_active(&self) -> bool {
        self.active
    }
    pub(crate) fn gate_value(&self) -> f32 {
        self.gate.value()
    }
    #[cfg(test)]
    pub fn mpe_state(&self) -> &MpeVoiceState {
        &self.mpe
    }

    pub(crate) fn set_envelope_level(&mut self, level: f32) {
        self.envelope_level = level;
    }
    pub(crate) fn deactivate(&mut self) {
        self.active = false;
    }

    /// Update note and channel for legato retrigger (without re-gating).
    pub(crate) fn update_legato(&mut self, note: u8, channel: u8) {
        self.note = note;
        self.channel = channel;
    }

    pub(crate) fn from_config(config: &SynthConfig, unison_count: usize) -> Self {
        let gate = tutti_core::shared(0.0);
        let base_filter_cutoff = match &config.filter {
            FilterType::Moog { cutoff, .. } => cutoff.get(),
            FilterType::Svf { cutoff, .. } => cutoff.get(),
            FilterType::None => 20000.0,
        };
        let filter_cutoff = tutti_core::shared(base_filter_cutoff);

        // The one place `Resonance` and `Q` deliberately merge: both feed a
        // single `Shared` so the modulation path has one resonance handle
        // regardless of which filter is running. They are unwrapped rather
        // than converted because there is no meaningful conversion between
        // them — the scalar is re-typed at the node that consumes it.
        let base_filter_resonance = match &config.filter {
            FilterType::Moog { resonance, .. } => resonance.get(),
            FilterType::Svf { q, .. } => q.get(),
            FilterType::None => 0.0,
        };
        let filter_resonance = tutti_core::shared(base_filter_resonance);

        let count = unison_count.max(1);
        let mut sub_voices = Vec::with_capacity(count);
        for _ in 0..count {
            let pitch = tutti_core::shared(440.0);
            let mut dsp =
                build_sub_voice_dsp(config, &pitch, &gate, &filter_cutoff, &filter_resonance);
            dsp.set_sample_rate(config.sample_rate);

            let num_outputs = dsp.outputs();
            let mut init_buf = [0.0f32; 2];
            for _ in 0..100 {
                dsp.tick(&[], &mut init_buf[..num_outputs]);
            }

            sub_voices.push(SubVoice { pitch, dsp });
        }

        Self {
            note: 0,
            channel: 0,
            velocity: 0.0,
            gate,
            filter_cutoff,
            base_filter_cutoff,
            filter_resonance,
            base_filter_resonance,
            mod_wheel_value: 0.0,
            velocity_mod_value: 1.0,
            cc_cutoff_value: 0.5,
            cc_resonance_value: 0.0,
            filter_mod: config.filter_mod,
            lfo_phase: Phase::START,
            envelope_level: 0.0,
            active: false,
            mpe: MpeVoiceState::default(),
            mpe_enabled: config.mpe_enabled,
            mpe_pitch_bend_range: config.mpe_pitch_bend_range,
            base_note_freq: Hz(440.0),
            sub_voices,
            config: config.clone(),
            sample_rate: config.sample_rate,
        }
    }

    pub(crate) fn note_on(
        &mut self,
        note: u8,
        channel: u8,
        velocity: f32,
        base_freq: impl Into<Hz>,
        unison: Option<&mut UnisonEngine>,
    ) {
        let base_freq = base_freq.into();
        self.note = note;
        self.channel = channel;
        self.velocity = velocity;
        self.gate.set(1.0);
        self.active = true;
        self.lfo_phase = Phase::START;
        self.base_note_freq = base_freq;
        self.mpe.reset();

        if let Some(unison) = unison {
            unison.randomize_phases();
            for (i, sub) in self.sub_voices.iter_mut().enumerate() {
                let params = unison.voice_params(i);
                sub.pitch.set(base_freq.get() * params.freq_ratio);
            }
        } else {
            for sub in &mut self.sub_voices {
                sub.pitch.set(base_freq.get());
            }
        }
    }

    pub(crate) fn note_off(&mut self) {
        self.gate.set(0.0);
    }

    pub(crate) fn set_pitch(&mut self, base_freq: impl Into<Hz>, unison: Option<&UnisonEngine>) {
        let base_freq = base_freq.into().get();
        if let Some(unison) = unison {
            for (i, sub) in self.sub_voices.iter_mut().enumerate() {
                let params = unison.voice_params(i);
                sub.pitch.set(base_freq * params.freq_ratio);
            }
        } else {
            for sub in &mut self.sub_voices {
                sub.pitch.set(base_freq);
            }
        }
    }

    pub(crate) fn reset(&mut self) {
        self.note = 0;
        self.channel = 0;
        self.velocity = 0.0;
        self.envelope_level = 0.0;
        self.active = false;
        self.gate.set(0.0);
        self.mod_wheel_value = 0.0;
        self.velocity_mod_value = 1.0;
        self.cc_cutoff_value = 0.5;
        self.cc_resonance_value = 0.0;
        self.lfo_phase = Phase::START;
        self.mpe.reset();
        self.base_note_freq = Hz(440.0);
        self.filter_cutoff.set(self.base_filter_cutoff);
        self.filter_resonance.set(self.base_filter_resonance);
        for sub in &mut self.sub_voices {
            sub.dsp.reset();
        }
    }

    #[cfg(test)]
    pub fn sub_voice_count(&self) -> usize {
        self.sub_voices.len()
    }

    pub(crate) fn process_block_stereo(
        &mut self,
        unison: Option<&UnisonEngine>,
        left: &mut [f32],
        right: &mut [f32],
        offset: usize,
        count: usize,
    ) -> f32 {
        let mut peak = 0.0f32;
        for i in 0..count {
            let (l, r) = self.tick_stereo(unison);
            left[offset + i] += l;
            right[offset + i] += r;
            peak = peak.max(l.abs().max(r.abs()));
        }
        peak
    }

    pub(crate) fn tick_stereo(&mut self, unison: Option<&UnisonEngine>) -> (f32, f32) {
        self.update_modulated_filter();
        self.apply_mpe_modulation(unison);

        let mut left = 0.0f32;
        let mut right = 0.0f32;
        let mut out_buf = [0.0f32; 2];

        for (i, sub) in self.sub_voices.iter_mut().enumerate() {
            let num_outputs = sub.dsp.outputs();
            out_buf[0] = 0.0;
            out_buf[1] = 0.0;
            sub.dsp.tick(&[], &mut out_buf[..num_outputs]);

            let (pan_pos, amplitude) = if let Some(u) = unison {
                let p = u.voice_params(i);
                (p.pan, p.amplitude)
            } else {
                (0.0, 1.0)
            };

            let left_gain = ((1.0 - pan_pos) * 0.5).sqrt() * amplitude;
            let right_gain = ((1.0 + pan_pos) * 0.5).sqrt() * amplitude;
            let mono_sample = out_buf[0];
            left += mono_sample * left_gain;
            right += mono_sample * right_gain;
        }

        let (pressure_gain, note_gain) = if self.mpe_enabled {
            (1.0 + self.mpe.pressure * 0.5, self.mpe.gain)
        } else {
            (1.0, 1.0)
        };

        let voice_gain = self.velocity * pressure_gain * note_gain;
        (left * voice_gain, right * voice_gain)
    }

    fn update_modulated_filter(&mut self) {
        let fm = &self.filter_mod;
        let has_filter_mod =
            fm.mod_wheel_depth > 0.0 || fm.velocity_depth > 0.0 || fm.lfo_depth > 0.0;
        let has_cc_cutoff = self.cc_cutoff_value != 0.5;
        let has_cc_resonance = self.cc_resonance_value != 0.0;

        if !has_filter_mod && !has_cc_cutoff && !has_cc_resonance {
            return;
        }

        if has_filter_mod || has_cc_cutoff {
            let mut cutoff = self.base_filter_cutoff;

            if fm.mod_wheel_depth > 0.0 {
                cutoff *= 1.0 + self.mod_wheel_value * fm.mod_wheel_depth;
            }

            if fm.velocity_depth > 0.0 {
                let vel_mult = 1.0 - fm.velocity_depth * 0.5
                    + self.velocity_mod_value * fm.velocity_depth * 0.5;
                cutoff *= vel_mult;
            }

            if fm.lfo_depth > 0.0 && fm.lfo_rate > 0.0 {
                // The named converter: this used to narrow the rate to f32
                // before dividing, computing the step at f32 precision.
                //
                // `advance` rather than `% 1.0`: the remainder operator keeps
                // the dividend's sign, so it is not a wrap for a negative
                // phase. The `lfo_rate > 0.0` guard above makes that
                // unreachable today, which is exactly how it would survive
                // until the first reverse LFO.
                let phase_inc = PhaseIncrement::per_sample(Hz(fm.lfo_rate), self.sample_rate);
                self.lfo_phase = self.lfo_phase.advance(phase_inc);

                let lfo_val = self.lfo_phase.to_radians().get().sin();
                cutoff *= 1.0 + lfo_val * fm.lfo_depth * 0.5;
            }

            if has_cc_cutoff {
                let factor = (4.0_f32).powf(self.cc_cutoff_value - 0.5);
                cutoff *= factor;
            }

            self.filter_cutoff.set(cutoff);
        }

        if has_cc_resonance {
            let base_res = self.base_filter_resonance;
            let max_res = 0.95;
            let res = base_res + self.cc_resonance_value * (max_res - base_res);
            self.filter_resonance.set(res);
        }
    }

    pub(crate) fn set_sample_rate(&mut self, sample_rate: tutti_core::SampleRate) {
        self.sample_rate = sample_rate;
        for sub in &mut self.sub_voices {
            sub.dsp.set_sample_rate(sample_rate);
        }
    }

    pub(crate) fn set_mod_wheel(&mut self, value: f32) {
        self.mod_wheel_value = value;
    }

    pub(crate) fn set_velocity_mod(&mut self, value: f32) {
        self.velocity_mod_value = value;
    }

    pub(crate) fn set_cc_cutoff(&mut self, value: f32) {
        self.cc_cutoff_value = value;
    }

    pub(crate) fn set_filter_resonance(&mut self, value: f32) {
        self.cc_resonance_value = value;
    }

    pub(crate) fn set_mpe_pitch_bend(&mut self, semitones: impl Into<Semitones>) {
        if self.mpe.detached {
            return;
        }
        let semitones = semitones.into().get();
        let range = self.mpe_pitch_bend_range.get();
        self.mpe.pitch_bend_semitones = Semitones(semitones.clamp(-range, range));
    }

    pub(crate) fn set_mpe_pressure(&mut self, pressure: f32) {
        if self.mpe.detached {
            return;
        }
        self.mpe.pressure = pressure.clamp(0.0, 1.0);
    }

    pub(crate) fn set_mpe_slide(&mut self, slide: f32) {
        if self.mpe.detached {
            return;
        }
        self.mpe.slide = slide.clamp(0.0, 1.0);
    }

    pub(crate) fn set_mpe_gain(&mut self, gain: f32) {
        if self.mpe.detached {
            return;
        }
        self.mpe.gain = gain.clamp(0.0, 1.0);
    }

    /// Reset this voice's per-note expression (pitch bend, pressure, slide) to
    /// their defaults. Backs MIDI 2.0 Per-Note Management *Reset* (M2-104
    /// §7.4.5, S=1): the voice keeps sounding *and* keeps responding, only its
    /// accumulated per-note controllers snap back to the note-on baseline.
    pub(crate) fn reset_mpe(&mut self) {
        self.mpe.reset();
    }

    /// Detach this voice's per-note controllers (M2-104 §7.4.5, D=1): it keeps
    /// its current per-note values but stops responding to any further per-note
    /// controllers, playing out frozen. The opposite intent from [`reset_mpe`].
    pub(crate) fn detach_mpe(&mut self) {
        self.mpe.detach();
    }

    /// Update the per-note pitch-bend range (semitones) this voice clamps to.
    /// Set by the per-note pitch-bend sensitivity RPN (M2-104 §7.4.13).
    pub(crate) fn set_mpe_pitch_bend_range(&mut self, range: Semitones) {
        self.mpe_pitch_bend_range = range;
    }

    /// Override this voice's pitch to an absolute frequency (M2-104 §7.4.15.2/3:
    /// Registered Per-Note Controller #3 Pitch 7.25 and Note-On Attribute #3
    /// Pitch 7.9). The note number loses its pitch meaning and becomes an index;
    /// `freq` is the sounding pitch. This updates `base_note_freq` so per-note /
    /// channel pitch bend correctly acts as an **offset from** the overridden
    /// pitch (per the spec), and drives the oscillators to it immediately.
    pub(crate) fn set_tuning_freq(&mut self, freq: impl Into<Hz>, unison: Option<&UnisonEngine>) {
        let freq = freq.into();
        self.base_note_freq = freq;
        self.set_pitch(freq, unison);
    }

    fn apply_mpe_modulation(&mut self, unison: Option<&UnisonEngine>) {
        if !self.mpe_enabled {
            return;
        }

        let pitch_bend_semitones = self.mpe.pitch_bend_semitones;
        if pitch_bend_semitones.get().abs() > 0.001 {
            let multiplier = pitch_bend_semitones.to_pitch_ratio();
            let freq = self.base_note_freq.get() * multiplier;
            if let Some(u) = unison {
                for (i, sub) in self.sub_voices.iter_mut().enumerate() {
                    let params = u.voice_params(i);
                    sub.pitch.set(freq * params.freq_ratio);
                }
            } else {
                for sub in &mut self.sub_voices {
                    sub.pitch.set(freq);
                }
            }
        }

        // Slide modulates filter cutoff around its center; skip only when the
        // slide is at center (no timbre shift), leaving the base cutoff intact.
        if (self.mpe.slide - crate::voice::SLIDE_CENTER).abs() > 0.001 {
            let factor = (4.0_f32).powf(self.mpe.slide - crate::voice::SLIDE_CENTER);
            self.filter_cutoff.set(self.base_filter_cutoff * factor);
        }
    }

    pub(crate) fn resize_unison(&mut self, new_count: usize) {
        let new_count = new_count.max(1);
        let current_count = self.sub_voices.len();

        if new_count == current_count {
            return;
        }

        if new_count > current_count {
            for _ in current_count..new_count {
                let pitch = tutti_core::shared(440.0);
                let mut dsp = build_sub_voice_dsp(
                    &self.config,
                    &pitch,
                    &self.gate,
                    &self.filter_cutoff,
                    &self.filter_resonance,
                );
                dsp.set_sample_rate(self.sample_rate);

                let num_outputs = dsp.outputs();
                let mut init_buf = [0.0f32; 2];
                for _ in 0..100 {
                    dsp.tick(&[], &mut init_buf[..num_outputs]);
                }

                self.sub_voices.push(SubVoice { pitch, dsp });
            }
        } else {
            self.sub_voices.truncate(new_count);
        }
    }

    pub(crate) fn footprint(&self) -> usize {
        self.sub_voices.iter().map(|s| s.dsp.footprint()).sum()
    }

    pub(crate) fn allocate(&mut self) {
        for sub in &mut self.sub_voices {
            sub.dsp.allocate();
        }
    }
}

fn build_sub_voice_dsp(
    config: &SynthConfig,
    pitch: &Shared,
    gate: &Shared,
    filter_cutoff: &Shared,
    filter_resonance: &Shared,
) -> Box<dyn AudioUnit> {
    let env = &config.envelope;

    macro_rules! with_filter {
        ($osc:expr) => {
            match &config.filter {
                FilterType::None => {
                    let envelope =
                        var(gate) >> adsr_live(env.attack, env.decay, env.sustain, env.release);
                    Box::new($osc * envelope) as Box<dyn AudioUnit>
                }
                FilterType::Moog { .. } => {
                    let envelope =
                        var(gate) >> adsr_live(env.attack, env.decay, env.sustain, env.release);
                    Box::new(
                        ($osc | var(filter_cutoff) | var(filter_resonance))
                            >> moog::<f32>()
                            >> (envelope * pass()),
                    )
                }
                FilterType::Svf { q, mode, .. } => {
                    let envelope =
                        var(gate) >> adsr_live(env.attack, env.decay, env.sustain, env.release);
                    match mode {
                        SvfMode::Lowpass => Box::new(
                            ($osc | var(filter_cutoff))
                                >> lowpass_q::<f32>(q.get())
                                >> (envelope * pass()),
                        ),
                        SvfMode::Highpass => Box::new(
                            ($osc | var(filter_cutoff))
                                >> highpass_q::<f32>(q.get())
                                >> (envelope * pass()),
                        ),
                        SvfMode::Bandpass => Box::new(
                            ($osc | var(filter_cutoff))
                                >> bandpass_q::<f32>(q.get())
                                >> (envelope * pass()),
                        ),
                        SvfMode::Notch => Box::new(
                            ($osc | var(filter_cutoff))
                                >> notch_q::<f32>(q.get())
                                >> (envelope * pass()),
                        ),
                    }
                }
            }
        };
    }

    match &config.oscillator {
        OscillatorType::Sine => with_filter!(var(pitch) >> sine::<f32>()),
        OscillatorType::Saw => with_filter!(var(pitch) >> saw()),
        OscillatorType::Square { pulse_width } => {
            let pw = *pulse_width;
            with_filter!((var(pitch) | dc(pw)) >> poly_pulse::<f32>())
        }
        OscillatorType::Triangle => with_filter!(var(pitch) >> triangle()),
        OscillatorType::Noise => with_filter!(pink::<f32>()),
    }
}
