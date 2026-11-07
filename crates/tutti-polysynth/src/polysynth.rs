//! The synth: a graph node that owns an allocator and a fixed set of voices,
//! and renders their sum.
//!
//! It decodes MIDI and drives allocation (`crate::voice`), per-note control
//! (`crate::synth_voice`), glide, unison and tuning. Per-note messages are
//! routed here because only this module holds both the note-id map and the
//! voices.
//!
//! Rendering works in control steps of at most `CONTROL_BLOCK` frames: each
//! step the voices and any glide write lane targets into the voice bank, which
//! renders the step and ramps every lane to its targets. The mix lives in the
//! bank, so no scratch is sized to a block.

use crate::SynthConfig;
use crate::bank::{CONTROL_BLOCK, VoiceBank};
use crate::mpe::MpeControl;
use crate::synth_voice::SynthVoice;
use crate::{AllocationResult, Portamento, UnisonEngine, VoiceAllocator, VoiceAllocatorConfig};
use tutti_midi_protocol::widen;
use tutti_midi_runtime::{BlockSplitter, Overrun, Step};
use tutti_midi_types::convert::{bend_u32_to_signed_f32, u16_to_unit_f32, u32_to_unit_f32};
use tutti_midi_types::mpe::PitchBendSensitivity;
use tutti_midi_types::ump::{MidiEvent, RPN_BANK_MPE, RPN_INDEX_PER_NOTE_PITCH_BEND_SENSITIVITY};
use tutti_midi_types::{NoteId, cc};
use tutti_types::{Amplitude, Param, Tail};
use tutti_ump::{
    CCNumber, Message, MidiChannel, NoteNumber, Pitch, RegisteredPerNoteController, Ump, Unknown,
    Voice1, Voice2, decode,
};

use std::sync::Arc;

/// A polyphonic subtractive synthesizer: a MIDI-driven voice allocator over a
/// SIMD voice bank.
///
/// Construct one from a [`SynthConfig`] with [`PolySynth::new`]. It is a
/// graph node (`tutti_graph::Node`) with no audio input, stereo output and
/// one MIDI event input; each event is applied on its own frame. Inserting it
/// hands back a `tutti_graph::ParamSet` over its live params, by
/// `UnitParam`: `Volume` (master gain, linear), and with a unison engine
/// `Detune` (cents) and `StereoSpread` (0..1).
///
/// The event input accepts MIDI 1.0 and MIDI 2.0 (UMP) channel-voice
/// messages, including MPE and MIDI 2.0 per-note controllers when
/// [`SynthConfig::mpe_enabled`] is set. Up to 512 events per block are
/// applied; any beyond that are dropped.
///
/// Rendering allocates nothing and takes no lock. Construction, forking and
/// changing the unison voice count allocate and belong on the control thread.
/// The oscillator, filter and envelope are fixed at construction: to change
/// them, build a new synth and replace the node.
///
/// # Examples
///
/// ```
/// use tutti_types::{Hz, Resonance, SampleRate, Samples};
/// use tutti_graph::{Prepare, Solo};
/// use tutti_polysynth::{FilterType, OscillatorType, PolySynth, SynthConfig};
///
/// let synth = PolySynth::new(SynthConfig {
///     oscillator: OscillatorType::Saw,
///     max_voices: 8,
///     filter: FilterType::Moog { cutoff: Hz(2_000.0), resonance: Resonance(0.5) },
///     ..Default::default()
/// })?;
/// assert_eq!(synth.active_voice_count(), 0);
///
/// // Alone in a graph; a host wires a clip or keyboard to its event input.
/// let mut solo = Solo::new(synth, Prepare::new(SampleRate(48_000.0), Samples(64)));
/// let out = solo.render(64);
/// assert_eq!(out.len(), 2); // stereo
/// # Ok::<(), tutti_polysynth::Error>(())
/// ```
#[derive(Debug)]
pub struct PolySynth {
    config: SynthConfig,
    allocator: VoiceAllocator,
    voices: Vec<SynthVoice>,
    /// Every sub-voice's oscillator, filter and envelope state, in SIMD lanes.
    bank: VoiceBank,
    portamento: Option<Portamento>,
    unison: Option<UnisonEngine>,
    pitch_bend: f32,
    master_volume: Param<Amplitude>,
    /// The block's MIDI, sorted by offset: its event input's.
    midi_buffer: Vec<MidiEvent>,
    /// Voices that finished this span. Sized to `max_voices` and only cleared,
    /// so it never allocates on the audio thread.
    finished_indices: Vec<usize>,
}

impl PolySynth {
    /// Creates a synth from a [`SynthConfig`], usually written as `Default`
    /// plus struct update:
    /// `SynthConfig { oscillator: OscillatorType::Saw, max_voices: 8, ..Default::default() }`.
    ///
    /// Every voice — and every unison sub-voice within it — is built here, so
    /// this allocates in proportion to `max_voices * unison.voice_count` and
    /// belongs off the audio thread.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidConfig`](crate::Error::InvalidConfig) if
    /// `max_voices` is 0. There is no upper bound: every voice is built up
    /// front, so `max_voices` is a memory and CPU budget, not a real-time
    /// risk.
    pub fn new(config: SynthConfig) -> crate::Result<Self> {
        if config.max_voices == 0 {
            return Err(crate::Error::InvalidConfig(
                "max_voices must be at least 1".into(),
            ));
        }
        let max_voices = config.max_voices;
        let allocator_config = VoiceAllocatorConfig {
            max_voices: config.max_voices,
            mode: config.voice_mode,
            strategy: config.allocation_strategy,
        };
        let allocator = VoiceAllocator::new(allocator_config);

        let unison = config.unison.as_ref().map(|u| UnisonEngine::new(u.clone()));
        // The engine's clamped count: voices and bank must match its params.
        let unison_count = unison.as_ref().map_or(1, UnisonEngine::voice_count);

        let voices = (0..config.max_voices)
            .map(|_| SynthVoice::from_config(&config, unison_count))
            .collect();
        let bank = Self::build_bank(&config, unison_count);

        let portamento = config
            .portamento
            .as_ref()
            .map(|p| Portamento::new(p.clone(), config.sample_rate));

        let master_volume = Param::new(Amplitude::UNITY);

        Ok(Self {
            config,
            allocator,
            voices,
            bank,
            portamento,
            unison,
            pitch_bend: 0.0,
            master_volume,
            midi_buffer: vec![MidiEvent::noop(); node::MIDI_BUFFER],
            finished_indices: Vec::with_capacity(max_voices),
        })
    }

    fn build_bank(config: &SynthConfig, unison_count: usize) -> VoiceBank {
        VoiceBank::new(
            config.oscillator,
            &config.filter,
            &config.envelope,
            config.sample_rate,
            config.max_voices,
            unison_count,
        )
    }

    /// Applies one MIDI event now, as if it arrived at the start of the next
    /// rendered block.
    ///
    /// Takes `&mut self`, so it cannot reach a synth that is already in a
    /// graph; there, send MIDI on the event input instead. Useful for setting
    /// a synth up before insertion or driving it by hand.
    pub fn apply_midi(&mut self, ump: Ump) {
        self.process_midi_event(ump);
    }

    /// Returns whether per-note (MPE) expression is applied at render time.
    ///
    /// The per-note controllers and the bend-sensitivity RPN are accepted
    /// either way, so a host cannot tell from their effect whether MPE is on.
    pub fn mpe_enabled(&self) -> bool {
        self.config.mpe_enabled
    }

    /// Turns per-note (MPE) expression on or off, for sounding notes as well
    /// as future ones. Disabling resets each voice's per-note state, so nothing
    /// received while off snaps into effect when it is re-enabled.
    pub fn set_mpe_enabled(&mut self, enabled: bool) {
        self.config.mpe_enabled = enabled;
        for voice in &mut self.voices {
            voice.set_mpe_enabled(enabled);
        }
    }

    /// A master volume as [`set_volume`](Self::set_volume) stores it:
    /// floored at 0. Shared by the setter and a driven port's value.
    #[inline]
    pub(crate) fn clamp_volume(volume: f32) -> f32 {
        volume.max(0.0)
    }

    /// Sets the master output gain as a linear [`Amplitude`]: `1.0` (the
    /// default) is unity, `0.0` silence, and values above unity boost.
    /// Negative values clamp to 0.
    ///
    /// Applied once per block to the summed mix, not per voice. The write is
    /// a lock-free store into the cell the node reads, the same one the
    /// synth's `ParamSet` addresses as `Volume` and
    /// [`volume_atomic`](Self::volume_atomic) returns, so it reaches a synth
    /// that is already rendering.
    pub fn set_volume(&self, volume: f32) {
        self.master_volume
            .store(Amplitude(Self::clamp_volume(volume)));
    }

    /// Returns the current master gain as a linear amplitude. Reflects modulation
    /// written through [`volume_atomic`](Self::volume_atomic) as well as
    /// [`set_volume`](Self::set_volume) — they share one cell.
    pub fn volume(&self) -> f32 {
        self.master_volume.load().get()
    }

    /// Returns the shared master-volume cell, for control-rate modulation (the
    /// cell the synth's `ParamSet` addresses as `Volume`). Clones the `Arc`;
    /// call it during setup, not from the audio path.
    pub fn volume_atomic(&self) -> Arc<tutti_types::AtomicF32> {
        self.master_volume.as_atomic()
    }

    /// Returns the shared unison-detune cell (cents), for control-rate
    /// modulation.
    /// `None` when this synth has no unison engine.
    pub fn detune_atomic(&self) -> Option<Arc<tutti_types::AtomicF32>> {
        self.unison.as_ref().map(|u| u.detune_atomic())
    }

    /// Returns the shared unison stereo-spread cell (0..1), for control-rate
    /// modulation.
    /// `None` when this synth has no unison engine.
    pub fn spread_atomic(&self) -> Option<Arc<tutti_types::AtomicF32>> {
        self.unison.as_ref().map(|u| u.spread_atomic())
    }

    /// Returns how many voices are currently sounding, counting those in their release
    /// stage. A voice stays counted until its envelope reaches silence, which is
    /// why this can exceed the number of keys held.
    pub fn active_voice_count(&self) -> usize {
        self.voices.iter().filter(|v| v.is_active()).count()
    }

    /// Returns this synth's unison settings, or `None` when it was built
    /// without a [`UnisonConfig`](crate::UnisonConfig). A synth built without
    /// one cannot gain unison later: every unison setter is a no-op on it.
    pub fn unison_config(&self) -> Option<&crate::UnisonConfig> {
        self.unison.as_ref().map(|u| u.config())
    }

    /// Returns the number of sub-voices stacked per note (1..=16), or `1` when this
    /// synth has no unison engine, so the total oscillator count is always
    /// `max_voices * this`.
    pub fn unison_voice_count(&self) -> usize {
        self.unison.as_ref().map_or(1, |u| u.voice_count())
    }

    /// Sets the unison detune in [`Cents`](tutti_types::Cents), applied
    /// symmetrically either side of the written pitch. Negative values clamp to
    /// zero. Recomputes the per-sub-voice pitch ratios immediately, so it
    /// affects sounding notes as well as future ones. No-op without unison.
    pub fn set_unison_detune(&mut self, cents: impl Into<tutti_types::Cents>) {
        if let Some(unison) = &mut self.unison {
            unison.set_detune(cents);
        }
    }

    /// Sets how wide the sub-voices are panned, 0.0 (all centre) to 1.0
    /// (outermost pair hard left and right); out-of-range values are clamped.
    /// Takes effect on sounding notes. No-op without unison.
    pub fn set_unison_stereo_spread(&mut self, spread: f32) {
        if let Some(unison) = &mut self.unison {
            unison.set_stereo_spread(spread);
        }
    }

    /// Sets the number of stacked sub-voices per note, clamped to 1..=16.
    ///
    /// Allocates: the voice bank is rebuilt at the new width (sounding
    /// sub-voices keep their state). Call it from the control thread, never the
    /// audio thread.
    ///
    /// No-op without unison.
    pub fn set_unison_voice_count(&mut self, count: u8) {
        if let Some(unison) = &mut self.unison {
            unison.set_voice_count(count);
            let new_count = unison.voice_count();
            self.resize_unison(new_count);
        }
    }

    /// Replaces all unison settings at once. Carries the same allocation warning
    /// as [`set_unison_voice_count`](Self::set_unison_voice_count) when the
    /// count grows. No-op without unison.
    pub fn set_unison_config(&mut self, config: crate::UnisonConfig) {
        if let Some(unison) = &mut self.unison {
            unison.set_config(config);
            let new_count = unison.voice_count();
            self.resize_unison(new_count);
        }
    }

    fn resize_unison(&mut self, count: usize) {
        self.bank.resize_stride(count);
        for voice in &mut self.voices {
            voice.resize_unison(count);
        }
    }

    /// Seeds the phase-randomisation generator, making a render with
    /// `phase_randomize` reproducible. A seed of `0` is remapped to `1`, since
    /// the xorshift generator cannot leave that state. No-op without unison.
    pub fn seed_unison_rng(&mut self, seed: u32) {
        if let Some(unison) = &mut self.unison {
            unison.seed_rng(seed);
        }
    }

    /// Returns the computed per-sub-voice pitch ratio, pan, phase offset and amplitude
    /// — one entry per active sub-voice. Derived state, recomputed on every
    /// unison setter; for inspection, not for driving the audio path.
    /// `None` without unison.
    pub fn unison_params(&self) -> Option<&[crate::UnisonVoiceParams]> {
        self.unison.as_ref().map(|u| u.all_params())
    }

    /// Decodes one packet and applies it. The only place the synth reads the
    /// wire: everything below takes typed values.
    ///
    /// The group is ignored: one synth is one instrument, whatever group its
    /// notes arrive on. This is a sink, so a message it has no use for
    /// (including one `decode` cannot frame) is unused, not an error.
    fn process_midi_event(&mut self, ump: Ump) {
        match decode(ump) {
            Ok(Message::Midi1(address, voice)) => self.voice1(address.channel, voice),
            Ok(Message::Midi2(address, voice)) => self.voice2(address.channel, voice),
            // Not channel voice: a synth has no clock, sysex, flex data or
            // stream state to follow.
            Ok(
                Message::Utility(_)
                | Message::System(..)
                | Message::Sysex7(..)
                | Message::Data128(..)
                | Message::Flex(..)
                | Message::Stream(_),
            ) => {}
            // A reserved or undefined packet, with its words kept in
            // `Unknown`: nothing here forwards them, so they are unused.
            Err(Unknown(_)) => {}
        }
    }

    /// A MIDI 1.0 channel voice message, widened to the MIDI 2.0 message that
    /// means the same (M2-104 App. D.3), so a note sounds the same on either wire.
    fn voice1(&mut self, channel: MidiChannel, voice: Voice1) {
        self.voice2(channel, widen(voice));
    }

    /// A MIDI 2.0 channel voice message.
    fn voice2(&mut self, channel: MidiChannel, voice: Voice2) {
        match voice {
            // Native MIDI 2.0 velocity 0 is a Note Off here, as it always has
            // been (M2-104 §7.4.2 keeps it a Note On; that is for the
            // translation stage). A widened MIDI 1.0 Note On at velocity 0
            // never gets here: it arrives as a `NoteOff`.
            Voice2::NoteOn {
                note, velocity: 0, ..
            } => self.handle_note_off(note, channel),
            Voice2::NoteOn {
                note,
                velocity,
                attribute,
            } => {
                self.handle_note_on(note, u16_to_unit_f32(velocity), channel);
                // Attribute Pitch 7.9 (M2-104 §7.4.15.3) is an absolute
                // per-note tuning override delivered at note start. Applied
                // after allocation so it lands on the voice this note claimed.
                if let Some(pitch) = attribute.pitch() {
                    let id = NoteId::from_channel_note(channel, note.get());
                    self.set_voice_tuning(id, pitch.note_number());
                }
            }
            // Release velocity is not used: the release is the envelope's.
            Voice2::NoteOff { note, .. } => self.handle_note_off(note, channel),
            Voice2::ControlChange { controller, value } => {
                self.handle_cc(controller, u32_to_unit_f32(value), channel);
            }
            Voice2::PitchBend(bend) => self.set_pitch_bend(bend_u32_to_signed_f32(bend)),
            // A native per-note message addresses one voice by note id, so
            // two same-pitch notes stay independent even on one channel.
            Voice2::PerNotePitchBend { note, value } => {
                let id = NoteId::from_channel_note(channel, note.get());
                let semitones = tutti_types::Semitones(
                    bend_u32_to_signed_f32(value) * self.config.mpe_pitch_bend_range.get(),
                );
                self.set_voice_mpe(id, MpeControl::PitchBend(semitones));
            }
            Voice2::PolyPressure { note, value } => {
                let id = NoteId::from_channel_note(channel, note.get());
                self.set_voice_mpe(id, MpeControl::Pressure(u32_to_unit_f32(value)));
            }
            // Only the dimensions a voice can apply are honoured: brightness
            // is slide and volume is per-note gain.
            Voice2::AssignablePerNote {
                note,
                controller,
                value,
            } => {
                let id = NoteId::from_channel_note(channel, note.get());
                let data = u32_to_unit_f32(value);
                // The index is a wire byte, not a 7-bit value: 0xCA is not
                // CC74.
                match controller {
                    c if c == cc::BRIGHTNESS.get() => {
                        self.set_voice_mpe(id, MpeControl::Slide(data));
                    }
                    c if c == cc::VOLUME.get() => self.set_voice_mpe(id, MpeControl::Gain(data)),
                    _ => {}
                }
            }
            Voice2::RegisteredPerNote {
                note,
                controller,
                value,
            } => {
                let id = NoteId::from_channel_note(channel, note.get());
                match controller {
                    RegisteredPerNoteController::Volume => {
                        self.set_voice_mpe(id, MpeControl::Gain(u32_to_unit_f32(value)));
                    }
                    RegisteredPerNoteController::Brightness => {
                        self.set_voice_mpe(id, MpeControl::Slide(u32_to_unit_f32(value)));
                    }
                    // Pitch 7.25 (M2-104 §7.4.15.2): an absolute per-note
                    // tuning override; the pitch bend offsets from it.
                    RegisteredPerNoteController::Pitch => {
                        self.set_voice_tuning(id, Pitch::from_q7_25(value).note_number());
                    }
                    // Pan and the rest have no per-note DSP yet.
                    _ => {}
                }
            }
            // Detach freezes the live note's controllers and Reset returns
            // them to defaults. With both set, Detach wins: a new note
            // already starts at defaults.
            Voice2::PerNoteManagement {
                note,
                detach,
                reset,
            } => {
                let id = NoteId::from_channel_note(channel, note.get());
                if detach {
                    self.detach_voice_mpe(id);
                } else if reset {
                    self.reset_voice_mpe(id);
                }
            }
            // Sensitivity of Per-Note Pitch Bend (RPN 0/7, M2-104 §7.4.13): a
            // 7.25 semitone range that the following per-note bends sweep.
            Voice2::Registered { bank, index, value }
                if bank.get() == RPN_BANK_MPE
                    && index.get() == RPN_INDEX_PER_NOTE_PITCH_BEND_SENSITIVITY =>
            {
                let range = PitchBendSensitivity::from_rpn_bits(value).as_semitones_f32();
                self.set_mpe_pitch_bend_range(tutti_types::Semitones(range));
            }
            // Controllers and messages with no target in a synth.
            Voice2::Registered { .. }
            | Voice2::Assignable { .. }
            | Voice2::RelativeRegistered { .. }
            | Voice2::RelativeAssignable { .. }
            | Voice2::ProgramChange { .. }
            | Voice2::ChannelPressure(_) => {}
        }
    }

    /// The channel pitch bend is global: the whole synth bends. Under MPE a
    /// member-channel bend is rewritten to a Per-Note Pitch Bend at the input
    /// edge (see `MpeIngest`), so one that reaches here is genuinely global.
    fn set_pitch_bend(&mut self, bend: f32) {
        self.pitch_bend = bend;
        self.apply_pitch_bend();
    }

    fn handle_note_on(&mut self, note: NoteNumber, vel_norm: f32, channel: MidiChannel) {
        let id = NoteId::from_channel_note(channel, note.get());
        let result = self.allocator.allocate(id, note, channel, vel_norm);

        let slot_index = result.slot_index();
        let is_legato = matches!(result, AllocationResult::LegatoRetrigger { .. });

        if let Some(slot_index) = slot_index {
            let base_freq = self
                .config
                .tuning
                .fractional_note_to_freq(f32::from(note.get()));
            let bend_multiplier = (self.config.pitch_bend_range * self.pitch_bend).to_pitch_ratio();

            let target_freq = if let Some(ref mut porta) = self.portamento {
                porta.set_target(base_freq, is_legato);
                porta.current() * bend_multiplier
            } else {
                base_freq * bend_multiplier
            };

            let voice = &mut self.voices[slot_index];
            if !is_legato && voice.is_fresh() {
                // A fresh draw for this voice alone, held by the voice: the
                // notes of a chord each get their own stack of phases.
                if let Some(unison) = &mut self.unison {
                    voice.set_start_phases(unison.randomize_phases());
                }
            }
            voice.set_velocity_mod(vel_norm);
            if is_legato {
                voice.set_pitch(target_freq);
                voice.update_legato(note, channel);
            } else {
                voice.note_on(note, channel, vel_norm, target_freq);
            }
        }
    }

    fn handle_note_off(&mut self, note: NoteNumber, channel: MidiChannel) {
        let id = NoteId::from_channel_note(channel, note.get());
        // `release` resolves the exact voice by id and reports whether it truly
        // stopped (vs. held by a pedal). Gate that voice by index — never by a
        // (note, channel) scan, which would alias two same-pitch voices.
        if let Some(slot_index) = self.allocator.release(id, channel) {
            self.voices[slot_index].note_off();
        }
    }

    /// Index of the voice currently bound to `id` (matches the allocator slot).
    #[inline]
    fn voice_index_for_id(&self, id: NoteId) -> Option<usize> {
        self.allocator
            .slots()
            .iter()
            .position(|s| s.id() == id && !s.is_idle())
    }

    /// Apply `f` to the single voice bound to `id` (the per-note addressing
    /// primitive every MIDI 2.0 per-note message routes through). No-op if no
    /// voice currently holds `id`, so other voices are never disturbed.
    #[inline]
    fn with_voice_for_id(&mut self, id: NoteId, f: impl FnOnce(&mut SynthVoice)) {
        if let Some(i) = self.voice_index_for_id(id) {
            f(&mut self.voices[i]);
        }
    }

    /// Applies one per-note controller to the voice addressed by `id` alone.
    fn set_voice_mpe(&mut self, id: NoteId, control: MpeControl) {
        self.with_voice_for_id(id, |v| v.apply_mpe(control));
    }

    /// Absolute per-note tuning override (Pitch 7.25 / 7.9). `fractional_note` is
    /// the target as a fractional MIDI note number (integer = 12-TET note, e.g.
    /// 69.0 = A440); the voice's tuning table maps it to a frequency. Pitch bend
    /// then offsets from this. Only the addressed voice is retuned.
    fn set_voice_tuning(&mut self, id: NoteId, fractional_note: f32) {
        let freq = self.config.tuning.fractional_note_to_freq(fractional_note);
        if let Some(i) = self.voice_index_for_id(id) {
            self.voices[i].set_tuning_freq(freq);
        }
    }

    /// MIDI 2.0 Per-Note Management *Reset* (M2-104 §7.4.15): snap the addressed
    /// voice's per-note controllers (pitch bend, pressure, slide, gain) back to
    /// their note-on defaults, leaving the note sounding. Others are untouched.
    fn reset_voice_mpe(&mut self, id: NoteId) {
        self.with_voice_for_id(id, |v| v.reset_mpe());
    }

    /// MIDI 2.0 Per-Note Management *Detach* (M2-104 §7.4.5, D=1): the addressed
    /// voice holds its current per-note controllers but stops responding to
    /// further ones, playing out frozen.
    fn detach_voice_mpe(&mut self, id: NoteId) {
        self.with_voice_for_id(id, |v| v.detach_mpe());
    }

    /// Set the per-note pitch-bend range (semitones) from the sensitivity RPN
    /// (M2-104 §7.4.13). Updates both the scaling range used to convert an
    /// incoming per-note bend to semitones and every voice's clamp range, so
    /// currently-sounding notes and future ones share the new sensitivity.
    fn set_mpe_pitch_bend_range(&mut self, range: tutti_types::Semitones) {
        self.config.mpe_pitch_bend_range = range;
        for voice in &mut self.voices {
            voice.set_mpe_pitch_bend_range(range);
        }
    }

    fn handle_cc(&mut self, cc_num: CCNumber, value: f32, channel: MidiChannel) {
        let on = value >= 0.5;
        match cc_num {
            cc::MOD_WHEEL => {
                self.voices.iter_mut().for_each(|v| v.set_mod_wheel(value));
            }
            cc::RESONANCE => {
                self.voices
                    .iter_mut()
                    .for_each(|v| v.set_filter_resonance(value));
            }
            // CC74 (Brightness). Under MPE a member-channel CC74 is rewritten to a
            // native per-note controller at the input edge (routed per-voice via
            // the `AssignablePerNoteController` arm above), so a channel-wide CC74
            // reaching here is a plain global brightness/cutoff.
            cc::BRIGHTNESS => {
                self.voices.iter_mut().for_each(|v| v.set_cc_cutoff(value));
            }
            cc::SUSTAIN => {
                self.allocator.sustain_pedal(channel, on);
                if !on {
                    self.sync_voice_gates();
                }
            }
            cc::SOSTENUTO => {
                self.allocator.sostenuto_pedal(channel, on);
                if !on {
                    self.sync_voice_gates();
                }
            }
            // Reset All Controllers (M2-104 Appendix B.2): reset the *channel*
            // controllers (mod wheel, cutoff, resonance) and the global pitch bend
            // to their defaults — but explicitly NOT the per-note controllers,
            // which the spec says RAC must leave alone (they belong to individual
            // notes, not the channel).
            cc::RESET_ALL => {
                for v in &mut self.voices {
                    v.set_mod_wheel(0.0);
                    // Centre, not zero: CC74 maps to a `4^(v - 0.5)` cutoff
                    // factor, so 0.0 would halve every voice's cutoff.
                    v.set_cc_cutoff(0.5);
                    v.set_filter_resonance(0.0);
                }
                self.pitch_bend = 0.0;
                self.apply_pitch_bend();
            }
            cc::ALL_SOUND_OFF => {
                for (i, voice) in self.voices.iter_mut().enumerate() {
                    if voice.channel() == channel {
                        voice.reset();
                        self.bank.kill_voice(i);
                    }
                }
                self.allocator.all_sound_off(channel);
            }
            cc::ALL_NOTES_OFF => {
                self.voices
                    .iter_mut()
                    .filter(|v| v.is_active() && v.channel() == channel)
                    .for_each(|v| v.note_off());
                self.allocator.all_notes_off(channel);
            }
            _ => {}
        }
    }

    fn apply_pitch_bend(&mut self) {
        if let Some(ref porta) = self.portamento
            && porta.is_gliding()
        {
            return;
        }

        // The bend is a frequency ratio at every site (note-on and glide bend
        // interpolated frequencies, which have no note number). Adding it to
        // the note number instead would differ on an unequal tuning table.
        let bend_multiplier = (self.config.pitch_bend_range * self.pitch_bend).to_pitch_ratio();
        let tuning = &self.config.tuning;
        self.voices
            .iter_mut()
            .filter(|v| v.is_active())
            .for_each(|voice| {
                let base_freq = tuning.fractional_note_to_freq(f32::from(voice.note().get()));
                voice.set_pitch(base_freq * bend_multiplier);
            });
    }

    fn sync_voice_gates(&mut self) {
        let slots = self.allocator.slots();
        for (i, voice) in self.voices.iter_mut().enumerate() {
            if voice.is_active()
                && voice.gate_value() > 0.0
                && i < slots.len()
                && slots[i].is_releasing()
            {
                voice.note_off();
            }
        }
    }

    fn mark_voice_finished(&mut self, slot_index: usize) {
        let slots = self.allocator.slots();
        if slot_index < slots.len() {
            let voice_id = slots[slot_index].voice_id();
            self.allocator.voice_finished(voice_id);
        }
    }

    /// One control step: advance any glide by `frames`, then have every
    /// sounding voice write its lane targets.
    ///
    /// The glide advances frame by frame and the voices are aimed at where it will
    /// be at the *end* of the step; the bank ramps each lane there sample by
    /// sample. So a glide moves the pitch every frame at any block length.
    /// Rendering a whole sub-block at the glide's final pitch would instead
    /// produce a staircase, one step per MIDI sub-block.
    fn control_step(&mut self, frames: usize) {
        if let Some(ref mut porta) = self.portamento
            && porta.is_gliding()
        {
            let freq = porta.advance(frames);
            // Hoisted: neither operand changes inside a step (MIDI is
            // handled between steps).
            let bend_multiplier = (self.config.pitch_bend_range * self.pitch_bend).to_pitch_ratio();
            for voice in self.voices.iter_mut().filter(|v| v.is_active()) {
                voice.set_pitch(freq * bend_multiplier);
            }
        }

        let unison = self.unison.as_ref();
        for (i, voice) in self.voices.iter_mut().enumerate() {
            if voice.is_active() {
                voice.drive(&mut self.bank, i, frames, unison);
            }
        }
    }

    /// Render `frames` frames with no MIDI inside them, handing each frame to
    /// `emit` as `(start + offset, left, right)` scaled by `volume`.
    ///
    /// Works in control steps of at most [`CONTROL_BLOCK`] frames, so it
    /// renders every frame of any `frames` with no scratch sized to a block.
    ///
    /// The steps sit on a fixed grid of [`CONTROL_BLOCK`] frames from the block
    /// start, cut additionally at each MIDI event, so a host block that is a
    /// multiple of the grid renders sample-identically however it is sized.
    fn render_span(
        &mut self,
        start: usize,
        frames: usize,
        volume: f32,
        emit: &mut impl FnMut(usize, f32, f32),
    ) {
        let mut done = 0;
        while done < frames {
            let to_grid = CONTROL_BLOCK - (start + done) % CONTROL_BLOCK;
            let n = (frames - done).min(to_grid);
            self.control_step(n);
            let (left, right) = self.bank.render(n);
            for (i, (&l, &r)) in left.iter().zip(right).enumerate() {
                emit(start + done + i, l * volume, r * volume);
            }
            done += n;
        }
        self.retire_finished_voices();
        self.allocator.advance_time(frames as u64);
    }

    /// Report each sounding voice's level to the allocator and free the ones
    /// whose release has run out.
    fn retire_finished_voices(&mut self) {
        self.finished_indices.clear();
        for (i, voice) in self.voices.iter_mut().enumerate() {
            if !voice.is_active() {
                continue;
            }
            let level = voice.output_level(&self.bank, i);
            voice.set_envelope_level(level);
            self.allocator.update_envelope_level(i, level);
            if voice.is_finished(&self.bank, i) {
                voice.deactivate();
                self.finished_indices.push(i);
            }
        }

        // Index, not `mem::take`: taking would free the `Vec` on the audio thread.
        for i in 0..self.finished_indices.len() {
            let slot_index = self.finished_indices[i];
            self.mark_voice_finished(slot_index);
        }
    }

    /// Render `size` frames, applying the first `midi_count` entries of the
    /// (sorted) `midi_buffer` at their frame offsets.
    ///
    /// The block is split at each event so the event lands on its frame, and
    /// the pieces go to [`render_span`](Self::render_span). Nothing here is
    /// bounded by a block size, so any block length renders every frame.
    fn render_events(
        &mut self,
        size: usize,
        midi_count: usize,
        volume: f32,
        emit: &mut impl FnMut(usize, f32, f32),
    ) {
        let mut walk = BlockSplitter::new(size, Overrun::Defer);
        while let Some(step) = walk.step(&self.midi_buffer[..midi_count]) {
            match step {
                Step::Event(event) => self.process_midi_event(Ump(event.data)),
                Step::Span(span) => self.render_span(span.start, span.len(), volume, emit),
            }
        }
    }
}

impl PolySynth {
    /// Silence every voice and forget the glide: a freshly prepared synth's
    /// state. Its `Param` cells and MPE mode are untouched.
    pub(crate) fn reset_voices(&mut self) {
        for voice in &mut self.voices {
            voice.reset();
        }
        self.bank.reset();
        self.allocator.reset();
        if let Some(ref mut porta) = self.portamento {
            porta.reset(440.0);
        }
    }

    /// Detach the control cells (master volume, unison detune and spread)
    /// at their values now: this synth reads and writes cells of its own
    /// from here on. A fork's half of `fork_instance`.
    pub(crate) fn detach_controls(&mut self) {
        self.master_volume.detach();
        if let Some(unison) = &mut self.unison {
            unison.detach();
        }
    }

    /// Re-rate the voice bank and the glide.
    pub(crate) fn set_rate(&mut self, sample_rate: tutti_types::SampleRate) {
        self.bank.set_sample_rate(sample_rate);
        if let Some(ref mut porta) = self.portamento {
            porta.set_sample_rate(sample_rate);
        }
    }
}

impl Clone for PolySynth {
    fn clone(&self) -> Self {
        // The clone's MIDI scratch is its own (a clone is a fork's template,
        // and later a fork: it plays only its own event input).
        Self {
            config: self.config.clone(),
            allocator: self.allocator.clone(),
            voices: self.voices.clone(),
            bank: self.bank.clone(),
            portamento: self.portamento.clone(),
            unison: self.unison.clone(),
            pitch_bend: self.pitch_bend,
            master_volume: self.master_volume.clone(),
            midi_buffer: vec![MidiEvent::noop(); node::MIDI_BUFFER],
            finished_indices: Vec::with_capacity(self.config.max_voices),
        }
    }
}

#[cfg(test)]
mod tests;

impl PolySynth {
    /// The amplitude envelope's release stage, as the node's tail.
    ///
    /// This node takes no audio input — it is driven by MIDI — so "after the
    /// input stops" means after the last note-off, at which point every
    /// still-sounding voice rings for its release time. That time is an
    /// authored parameter rather than an estimate, which is what makes this
    /// exact.
    ///
    /// A bounce that ends at the last note-off without it chops the release
    /// off every note in the project.
    pub(crate) fn release_tail(&self) -> Tail {
        match self
            .config
            .envelope
            .release
            .to_samples_ceil(self.config.sample_rate)
        {
            s if s.is_zero() => Tail::None,
            s => Tail::Finite(s),
        }
    }
}

mod node;
pub use node::SYNTH_PARAMS;
