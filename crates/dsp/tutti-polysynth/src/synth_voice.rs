//! One sounding note's *control* state: what it is playing, its per-note MPE
//! expression, and the modulation that turns both into per-lane targets.
//!
//! Separate from `crate::voice` because that module decides *which* slot a note
//! gets and this one decides what the slot sounds like. Separate from
//! `crate::bank` because this holds no DSP state at all: the oscillator, filter
//! and envelope of every sub-voice live in the bank's SIMD lanes, and a
//! [`SynthVoice`] reaches them only through [`SynthVoice::drive`], once per
//! control step.
//!
//! That split is what the fundsp operator-DSL chain this replaced could not
//! make. Each sub-voice used to be an opaque `Box<dyn AudioUnit>` reading its
//! pitch, gate, cutoff and resonance out of four `Shared` atomics every sample
//! — atomics shared between clones, which is why `isolate` had to rebuild every
//! voice. Here those are plain fields, derived at the point of use each control
//! step; nothing a clone could alias.
//!
//! Nothing here allocates. The gate edges `note_on`/`note_off` record are
//! applied to the lanes by the next `drive`, which the render calls before the
//! first frame after the MIDI event that caused them — so an edge still lands
//! on the event's frame.

use crate::bank::VoiceBank;
use crate::unison::MAX_UNISON_VOICES;
use crate::{FilterModConfig, FilterType, SynthConfig};
use crate::{MpeVoiceState, UnisonEngine, UnisonVoiceParams};
use tutti_core::{Amplitude, Depth, Hz, Pan, Phase, PhaseIncrement, Resonance, Semitones};

/// A gate edge waiting for the next control step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GateEdge {
    None,
    /// Note-on. `fresh` when the voice was silent, so its oscillators may be
    /// reset to their start phases without a click.
    On {
        fresh: bool,
    },
    Off,
}

#[derive(Clone)]
pub(crate) struct SynthVoice {
    note: u8,
    channel: u8,
    velocity: f32,
    gate: bool,
    edge: GateEdge,
    /// Jump the lanes' pitch and filter to their targets at the next control
    /// step instead of ramping — set by a note-on, so a new note does not
    /// glide in from the previous one's pitch. A glide is portamento's job.
    snap_tone: bool,
    /// Jump the lanes' output gains too. Only for a voice starting from
    /// silence: a stolen or retriggered voice is still sounding, and stepping
    /// its gain from the old velocity to the new one is a click, so it ramps.
    snap_gain: bool,
    /// Each sub-voice's start phase for the next fresh start, drawn per voice
    /// at its note-on. Held here rather than read from the shared unison table
    /// at start time, where the notes of a chord would all read the last draw.
    start_phases: [Phase; MAX_UNISON_VOICES],
    /// The pitch the voice sounds before per-note bend and unison detune: the
    /// note (or its tuning override) under the global bend and any glide.
    pitch: Hz,
    /// The authored corner frequency; every modulation source multiplies it.
    base_filter_cutoff: Hz,
    /// The authored ladder resonance. Only the ladder reads a resonance per
    /// lane: the SVF's `Q` is fixed for the synth, so CC71 reaches only
    /// [`FilterType::Moog`], as it always has.
    base_filter_resonance: Resonance,
    mod_wheel_value: f32,
    velocity_mod_value: f32,
    /// Normalized CC74 (Brightness) position, `0..1` centered at `0.5` — a
    /// controller value, not a frequency. Mapped onto `base_filter_cutoff` as
    /// a `4^(v - 0.5)` factor in `modulated_cutoff`.
    cc_cutoff_value: f32,
    /// Normalized CC71 (Resonance) position, `0..1` with `0.0` inactive — a
    /// controller value, not a `Resonance`. Blends `base_filter_resonance`
    /// toward the 0.95 ceiling in `modulated_resonance`.
    cc_resonance_value: f32,
    filter_mod: FilterModConfig,
    lfo_phase: Phase,
    envelope_level: f32,
    active: bool,
    mpe: MpeVoiceState,
    mpe_enabled: bool,
    mpe_pitch_bend_range: Semitones,
    base_note_freq: Hz,
    sub_voices: usize,
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
    /// `1.0` while the key (or a pedal) holds the gate open, `0.0` after.
    pub(crate) fn gate_value(&self) -> f32 {
        if self.gate {
            1.0
        } else {
            0.0
        }
    }
    #[cfg(test)]
    pub fn mpe_state(&self) -> &MpeVoiceState {
        &self.mpe
    }

    /// The voice's amplitude right now: its envelope times its gain. What the
    /// allocator compares when it steals the quietest voice.
    pub(crate) fn output_level(&self, bank: &VoiceBank, index: usize) -> f32 {
        bank.envelope(bank.lane(index, 0)).get() * self.voice_gain()
    }

    /// Whether this voice is silent, so a note-on would start it fresh.
    pub(crate) fn is_fresh(&self) -> bool {
        !self.active
    }

    /// Take the start phases for this voice's next fresh start.
    pub(crate) fn set_start_phases(&mut self, params: &[UnisonVoiceParams]) {
        for (slot, p) in self.start_phases.iter_mut().zip(params) {
            *slot = p.phase_offset;
        }
    }
    pub(crate) fn set_envelope_level(&mut self, level: f32) {
        self.envelope_level = level;
    }
    pub(crate) fn deactivate(&mut self) {
        self.active = false;
    }

    /// Whether the voice has finished: gate closed, no edge pending, and the
    /// envelope's release run out on its lanes.
    pub(crate) fn is_finished(&self, bank: &VoiceBank, index: usize) -> bool {
        !self.gate && self.edge == GateEdge::None && bank.is_idle(bank.lane(index, 0))
    }

    /// Update note and channel for legato retrigger (without re-gating).
    pub(crate) fn update_legato(&mut self, note: u8, channel: u8) {
        self.note = note;
        self.channel = channel;
    }

    pub(crate) fn from_config(config: &SynthConfig, unison_count: usize) -> Self {
        let (base_filter_cutoff, base_filter_resonance) = match config.filter {
            FilterType::Moog { cutoff, resonance } => (cutoff, resonance),
            FilterType::Svf { cutoff, .. } => (cutoff, Resonance::NONE),
            FilterType::None => (Hz(20000.0), Resonance::NONE),
        };

        Self {
            note: 0,
            channel: 0,
            velocity: 0.0,
            gate: false,
            edge: GateEdge::None,
            snap_tone: false,
            snap_gain: false,
            start_phases: [Phase::START; MAX_UNISON_VOICES],
            pitch: Hz(440.0),
            base_filter_cutoff,
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
            sub_voices: unison_count.max(1),
        }
    }

    pub(crate) fn note_on(
        &mut self,
        note: u8,
        channel: u8,
        velocity: f32,
        base_freq: impl Into<Hz>,
    ) {
        let base_freq = base_freq.into();
        let fresh = !self.active;
        self.note = note;
        self.channel = channel;
        self.velocity = velocity;
        self.gate = true;
        self.edge = GateEdge::On { fresh };
        self.snap_tone = true;
        self.snap_gain = fresh;
        self.active = true;
        self.lfo_phase = Phase::START;
        self.base_note_freq = base_freq;
        self.pitch = base_freq;
        self.mpe.reset();
    }

    pub(crate) fn note_off(&mut self) {
        self.gate = false;
        self.edge = GateEdge::Off;
    }

    /// Retune the voice. Takes effect at the next control step, ramped across
    /// it, so a bend or glide moves the pitch every sample.
    pub(crate) fn set_pitch(&mut self, freq: impl Into<Hz>) {
        self.pitch = freq.into();
    }

    /// Return the voice to its just-built state. The caller silences its lanes
    /// (`VoiceBank::kill`); this resets only the control state.
    pub(crate) fn reset(&mut self) {
        self.note = 0;
        self.channel = 0;
        self.velocity = 0.0;
        self.envelope_level = 0.0;
        self.active = false;
        self.gate = false;
        self.edge = GateEdge::None;
        self.snap_tone = false;
        self.snap_gain = false;
        self.mod_wheel_value = 0.0;
        self.velocity_mod_value = 1.0;
        self.cc_cutoff_value = 0.5;
        self.cc_resonance_value = 0.0;
        self.lfo_phase = Phase::START;
        self.mpe.reset();
        self.base_note_freq = Hz(440.0);
        self.pitch = Hz(440.0);
    }

    #[cfg(test)]
    pub fn sub_voice_count(&self) -> usize {
        self.sub_voices
    }

    /// The frequency the voice sounds at before unison detune — *post*-bend,
    /// unlike [`base_note_freq`](Self::base_note_freq). The first sub-voice
    /// sounds exactly this without unison.
    pub(crate) fn sounding_freq(&self) -> Hz {
        if self.mpe_enabled && self.mpe.pitch_bend_semitones.get().abs() > 0.001 {
            Hz(self.pitch.get() * self.mpe.pitch_bend_semitones.to_pitch_ratio())
        } else {
            self.pitch
        }
    }

    /// Write this voice's targets into its lanes for the next `frames` frames,
    /// applying any pending gate edge first. `index` is the voice's slot.
    ///
    /// The per-control-step replacement for the per-sample `tick_stereo` the
    /// fundsp chain had: modulation is evaluated here, once, and the bank ramps
    /// the lanes to it.
    pub(crate) fn drive(
        &mut self,
        bank: &mut VoiceBank,
        index: usize,
        frames: usize,
        unison: Option<&UnisonEngine>,
    ) {
        let edge = core::mem::replace(&mut self.edge, GateEdge::None);
        for sub in 0..self.sub_voices {
            let lane = bank.lane(index, sub);
            match edge {
                GateEdge::On { fresh } => {
                    if fresh {
                        bank.start(lane, self.start_phases[sub.min(MAX_UNISON_VOICES - 1)]);
                    }
                    bank.gate_on(lane);
                }
                GateEdge::Off => bank.gate_off(lane),
                GateEdge::None => {}
            }
        }

        let cutoff = self.modulated_cutoff(frames, bank.sample_rate());
        let resonance = self.modulated_resonance();
        let freq = self.sounding_freq().get();
        let gain = self.voice_gain();

        // One coefficient computation for the whole stack: the sub-voices
        // share the voice's cutoff.
        bank.set_filter(bank.lane(index, 0), self.sub_voices, cutoff, resonance);

        for sub in 0..self.sub_voices {
            let lane = bank.lane(index, sub);
            let (ratio, pan, amplitude) = match unison {
                Some(u) => {
                    let p = u.voice_params(sub);
                    (p.freq_ratio, p.pan, p.amplitude)
                }
                None => (1.0, Pan::CENTER, Amplitude::UNITY),
            };
            bank.set_pitch(lane, Hz(freq * ratio));
            // Constant-power pan law, then the unison voice's own gain and the
            // voice's. Evaluated per control step, not per sample.
            let (pan, g) = (pan.get(), amplitude.get() * gain);
            bank.set_gains(
                lane,
                Amplitude(((1.0 - pan) * 0.5).sqrt() * g),
                Amplitude(((1.0 + pan) * 0.5).sqrt() * g),
            );
            if self.snap_tone {
                bank.snap_tone(lane);
            }
            if self.snap_gain {
                bank.snap_gain(lane);
            }
            bank.mark_live(lane);
        }
        self.snap_tone = false;
        self.snap_gain = false;
    }

    /// Velocity, MPE pressure and per-note gain, combined.
    fn voice_gain(&self) -> f32 {
        let (pressure_gain, note_gain) = if self.mpe_enabled {
            (1.0 + self.mpe.pressure * 0.5, self.mpe.gain.get())
        } else {
            (1.0, 1.0)
        };
        self.velocity * pressure_gain * note_gain
    }

    /// The cutoff every modulation source leaves, advancing the filter LFO by
    /// `frames`.
    ///
    /// All sources multiply the base and compound, as [`FilterModConfig`]
    /// documents. MPE slide compounds with them too: it used to *overwrite*
    /// the cutoff the others had computed, and — because nothing recomputed the
    /// cutoff once every source was idle — a slide that returned to center left
    /// the filter wherever the slide last put it.
    fn modulated_cutoff(&mut self, frames: usize, sample_rate: tutti_core::SampleRate) -> Hz {
        let fm = &self.filter_mod;
        let mut cutoff = self.base_filter_cutoff.get();

        // `cutoff` is a bare `f32` multiplier chain, so each depth comes off its
        // type at the multiply rather than the struct carrying bare floats for
        // the arithmetic's sake.
        if fm.mod_wheel_depth > Depth(0.0) {
            cutoff *= 1.0 + self.mod_wheel_value * fm.mod_wheel_depth.get();
        }

        if fm.velocity_depth > Depth(0.0) {
            let vel_mult = 1.0 - fm.velocity_depth.get() * 0.5
                + self.velocity_mod_value * fm.velocity_depth.get() * 0.5;
            cutoff *= vel_mult;
        }

        if fm.lfo_depth > Depth(0.0) && fm.lfo_rate > Hz(0.0) {
            // `advance` rather than `% 1.0`: the remainder operator keeps the
            // dividend's sign, so it is not a wrap for a negative phase.
            let step = PhaseIncrement::per_sample(fm.lfo_rate, sample_rate) * frames as f32;
            self.lfo_phase = self.lfo_phase.advance(step);
            let lfo_val = self.lfo_phase.to_radians().sin();
            cutoff *= 1.0 + lfo_val * fm.lfo_depth.get() * 0.5;
        }

        if self.cc_cutoff_value != 0.5 {
            cutoff *= (4.0_f32).powf(self.cc_cutoff_value - 0.5);
        }

        // Slide modulates the cutoff around its center.
        if self.mpe_enabled && (self.mpe.slide - crate::voice::SLIDE_CENTER).abs() > 0.001 {
            cutoff *= (4.0_f32).powf(self.mpe.slide - crate::voice::SLIDE_CENTER);
        }

        Hz(cutoff)
    }

    /// The ladder resonance after CC71, which blends the base toward a 0.95
    /// ceiling.
    fn modulated_resonance(&self) -> Resonance {
        if self.cc_resonance_value == 0.0 {
            return self.base_filter_resonance;
        }
        let base = self.base_filter_resonance.get();
        let max_res = 0.95;
        Resonance(base + self.cc_resonance_value * (max_res - base))
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
        // The clamp is per-note Volume's `0..1` range, narrower than
        // `Amplitude`'s own floor-at-zero bound.
        self.mpe.gain = Amplitude(gain.clamp(0.0, 1.0));
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

    /// Whether this voice applies its per-note (MPE) state at render time.
    ///
    /// Test-only: production code asks [`PolySynth::mpe_enabled`], which reads
    /// the config the voices are built from. This observes that a runtime toggle
    /// actually reached an already-sounding voice.
    ///
    /// [`PolySynth::mpe_enabled`]: crate::PolySynth::mpe_enabled
    #[cfg(test)]
    pub(crate) fn mpe_enabled(&self) -> bool {
        self.mpe_enabled
    }

    /// Turn per-note (MPE) response on or off for this voice.
    ///
    /// Switching **off** also resets the per-note state rather than merely
    /// ignoring it. The setters store unconditionally, so state accumulated
    /// while disabled would otherwise sit latent and snap into effect the
    /// instant MPE is re-enabled — a sounding note jumping in pitch or gain from
    /// bends it received minutes earlier. Resetting makes "off" mean the note
    /// renders at its note-on defaults, which is what both the caller and the
    /// listener expect.
    pub(crate) fn set_mpe_enabled(&mut self, enabled: bool) {
        if self.mpe_enabled == enabled {
            return;
        }
        self.mpe_enabled = enabled;
        if !enabled {
            self.reset_mpe();
        }
    }

    /// Override this voice's pitch to an absolute frequency (M2-104 §7.4.15.2/3:
    /// Registered Per-Note Controller #3 Pitch 7.25 and Note-On Attribute #3
    /// Pitch 7.9). The note number loses its pitch meaning and becomes an index;
    /// `freq` is the sounding pitch. This updates `base_note_freq` so per-note /
    /// channel pitch bend correctly acts as an **offset from** the overridden
    /// pitch (per the spec), and drives the oscillators to it immediately.
    pub(crate) fn set_tuning_freq(&mut self, freq: impl Into<Hz>) {
        let freq = freq.into();
        self.base_note_freq = freq;
        self.set_pitch(freq);
    }

    /// Change the number of unison sub-voices this voice drives. The lanes are
    /// resized by the bank (`VoiceBank::resize_stride`), which starts a new
    /// sub-voice as a copy of the first; this records the count and snaps the
    /// lanes' pitch at the next control step, so the copy takes its own
    /// detuned pitch at once. Gains ramp to the new pans: the lanes are
    /// sounding.
    pub(crate) fn resize_unison(&mut self, new_count: usize) {
        self.sub_voices = new_count.max(1);
        self.snap_tone = true;
    }
}
