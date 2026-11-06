//! The synth itself: a graph node that owns an
//! allocator and a fixed set of voices, and renders their sum.
//!
//! This is where MIDI becomes sound. Everything else in the crate is a piece
//! this module drives — allocation (`crate::voice`), per-note DSP
//! (`crate::synth_voice`), glide, unison and tuning — and the MIDI 2.0 decoding
//! that routes a per-note message to exactly one voice lives here because only
//! this module holds both the note-id map and the voices.
//!
//! The render path is real-time: `Node::process` splits its block at MIDI
//! event boundaries so events land on the right frame. It never allocates.
//!
//! The renderer works in control steps of at most `CONTROL_BLOCK` frames:
//! each step the voices (and any glide) write their lane targets into the
//! voice bank, and the bank renders the step, ramping every lane to its
//! targets sample by sample. The mix of one step lives in the bank, so no
//! scratch is sized to a block — any block length renders every frame.

use crate::SynthConfig;
use crate::bank::{CONTROL_BLOCK, VoiceBank};
use crate::synth_voice::SynthVoice;
use crate::{AllocationResult, Portamento, UnisonEngine, VoiceAllocator, VoiceAllocatorConfig};
use tutti_core::{Amplitude, Param, Tail};
use tutti_midi_types::ump::MidiEvent;
use tutti_midi_types::{CCNumber, MidiChannel};
use tutti_midi_types::{NoteId, cc};

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
/// use tutti_core::{Hz, Resonance, SampleRate, Samples};
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
    /// MIDI queued by this crate's unit tests for the next block they drive
    /// by hand (`tests::Hand`). Test builds only: the synth itself plays
    /// nothing but its event input.
    #[cfg(test)]
    queued: Vec<MidiEvent>,
    /// Voices that finished this block, collected during the render loop and
    /// drained after it.
    ///
    /// Sized to `max_voices` at construction and only ever `clear()`ed, so it
    /// never reallocates: the worst case is every voice releasing at once,
    /// which is exactly its capacity. Growing it would be a `malloc` on the
    /// audio thread, which the allocation gates (`tests/rt_no_alloc.rs`)
    /// watch for.
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
        // Read before `config` is moved into the struct below; this is the
        // one-time sizing that keeps `finished_indices` off the allocator for
        // the rest of its life.
        let max_voices = config.max_voices;
        let allocator_config = VoiceAllocatorConfig {
            max_voices: config.max_voices,
            mode: config.voice_mode,
            strategy: config.allocation_strategy,
        };
        let allocator = VoiceAllocator::new(allocator_config);

        let unison = config.unison.as_ref().map(|u| UnisonEngine::new(u.clone()));
        // The engine's clamped count, not the config's raw one: the voices and
        // the bank must agree with the sub-voice params the engine produces.
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
            #[cfg(test)]
            queued: Vec::new(),
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
    pub fn apply_midi(&mut self, event: &MidiEvent) {
        self.process_midi_event(event);
    }

    /// Returns whether per-note (MPE) expression is applied at render time.
    ///
    /// Ask before concluding MPE is active: the per-note setters accept
    /// unconditionally, and the per-note pitch-bend *sensitivity* RPN applies
    /// whether or not this is on. So an MPE host could send an MCM, observe
    /// sensitivity take effect, and reasonably infer MPE was enabled when it was
    /// not — with every per-note bend, pressure, slide and gain being dropped at
    /// render time. This is the accessor that distinguishes the two.
    pub fn mpe_enabled(&self) -> bool {
        self.config.mpe_enabled
    }

    /// Turns per-note (MPE) expression on or off, affecting sounding notes as
    /// well as future ones.
    ///
    /// Disabling **resets** each voice's per-note state rather than merely
    /// ignoring it. The per-note setters store unconditionally, so expression
    /// received while disabled would otherwise sit latent and snap into effect
    /// the instant MPE is re-enabled — a sounding note jumping in pitch or gain
    /// from bends it received minutes earlier.
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
    pub fn volume_atomic(&self) -> Arc<tutti_core::AtomicF32> {
        self.master_volume.as_atomic()
    }

    /// Returns the shared unison-detune cell (cents), for control-rate
    /// modulation.
    /// `None` when this synth has no unison engine.
    pub fn detune_atomic(&self) -> Option<Arc<tutti_core::AtomicF32>> {
        self.unison.as_ref().map(|u| u.detune_atomic())
    }

    /// Returns the shared unison stereo-spread cell (0..1), for control-rate
    /// modulation.
    /// `None` when this synth has no unison engine.
    pub fn spread_atomic(&self) -> Option<Arc<tutti_core::AtomicF32>> {
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

    /// Sets the unison detune in [`Cents`](tutti_core::Cents), applied
    /// symmetrically either side of the written pitch. Negative values clamp to
    /// zero. Recomputes the per-sub-voice pitch ratios immediately, so it
    /// affects sounding notes as well as future ones. No-op without unison.
    pub fn set_unison_detune(&mut self, cents: impl Into<tutti_core::Cents>) {
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
    /// **Allocates**: the voice bank is rebuilt at the new width (sounding
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

    fn process_midi_event(&mut self, event: &MidiEvent) {
        use tutti_midi_types::convert::{bend_u32_to_signed_f32, u16_to_unit_f32, u32_to_unit_f32};
        use tutti_midi_types::midi2::channel_voice2::{ChannelVoice2 as Cv2, NoteAttribute};
        use tutti_midi_types::midi2::{Channeled, UmpMessage};

        // `normalize` folds velocity-0 NoteOn→NoteOff and promotes any inbound
        // MIDI 1.0 channel voice to MIDI 2.0, leaving a single vocabulary to match.
        let normalized = tutti_midi_types::normalize(event);
        let Ok(UmpMessage::ChannelVoice2(cv2)) = UmpMessage::try_from(normalized.data_words())
        else {
            return;
        };
        let channel = u8::from(cv2.channel());
        match cv2 {
            Cv2::NoteOn(m) => {
                let note = u8::from(m.note_number());
                self.handle_note_on(note, u16_to_unit_f32(m.velocity()), channel);
                // Note-On Attribute #3: Pitch 7.9 (M2-104 §7.4.15.3) — an absolute
                // per-note tuning override delivered at note start. Applied after
                // allocation so it lands on the voice this note just claimed.
                if let Some(NoteAttribute::Pitch7_9(p)) = m.attribute() {
                    let id = NoteId::from_channel_note(MidiChannel::new(channel), note);
                    self.set_voice_tuning(id, tutti_midi_types::q7_9_to_f32(p.to_bits()));
                }
            }
            Cv2::NoteOff(m) => {
                self.handle_note_off(u8::from(m.note_number()), channel);
            }
            Cv2::ControlChange(m) => {
                self.handle_cc(
                    // Wire boundary: midi2's `u7` is already in range.
                    CCNumber::new(u8::from(m.control())),
                    u32_to_unit_f32(m.control_change_data()),
                    channel,
                );
            }
            // Channel pitch bend is a *global* (whole-synth) bend. Under MPE, a
            // member-channel bend is rewritten to a native Per-Note Pitch Bend at
            // the input edge (see `MpeIngest`), so anything reaching here — the
            // master-channel bend, or a non-MPE bend — is genuinely global.
            Cv2::ChannelPitchBend(m) => {
                self.pitch_bend = bend_u32_to_signed_f32(m.pitch_bend_data());
                self.apply_pitch_bend();
            }
            // MIDI 2.0 native per-note messages address one voice by note-id —
            // two same-pitch notes stay independent even on one channel.
            Cv2::PerNotePitchBend(m) => {
                let id =
                    NoteId::from_channel_note(MidiChannel::new(channel), u8::from(m.note_number()));
                self.set_voice_mpe_pitch_bend(id, bend_u32_to_signed_f32(m.pitch_bend_data()));
            }
            Cv2::KeyPressure(m) => {
                let id =
                    NoteId::from_channel_note(MidiChannel::new(channel), u8::from(m.note_number()));
                self.set_voice_mpe_pressure(id, u32_to_unit_f32(m.key_pressure_data()));
            }
            // Assignable per-note controllers carry a raw index. Only the dims
            // a synth voice can apply are honored: CC74 → slide, CC7 → per-note
            // gain.
            Cv2::AssignablePerNoteController(m) => {
                let id =
                    NoteId::from_channel_note(MidiChannel::new(channel), u8::from(m.note_number()));
                let data = u32_to_unit_f32(m.controller_data());
                // `m.index()` is the raw per-note controller index off the
                // wire (a `u8`, not a `u7`), so it is masked into range here.
                match CCNumber::new(m.index()) {
                    cc::BRIGHTNESS => self.set_voice_mpe_slide(id, data),
                    cc::VOLUME => self.set_voice_mpe_gain(id, data),
                    _ => {}
                }
            }
            // Registered per-note controllers carry a *semantic* controller enum
            // (index resolved to Volume/Pan/Brightness/…). Only the dims a synth
            // voice can apply are honored: Volume → per-note gain, Brightness
            // (CC74 / SoundController index 5) → slide. Pan stays recognized-but-unwired
            // (no per-note pan DSP on `SynthVoice` yet — don't invent it).
            Cv2::RegisteredPerNoteController(m) => {
                use tutti_midi_types::midi2::channel_voice2::Controller;
                let id =
                    NoteId::from_channel_note(MidiChannel::new(channel), u8::from(m.note_number()));
                match m.controller() {
                    Controller::Volume(data) => {
                        self.set_voice_mpe_gain(id, u32_to_unit_f32(data));
                    }
                    Controller::Brightness(data)
                    | Controller::SoundController { index: 5, data } => {
                        self.set_voice_mpe_slide(id, u32_to_unit_f32(data));
                    }
                    // Registered Per-Note Controller #3: Pitch 7.25 (M2-104
                    // §7.4.15.2) — an absolute per-note tuning override. The note
                    // number becomes an index; the Q7.25 value is the sounding
                    // pitch (integer = 12-TET note, fraction = fraction of a
                    // semitone). Pitch bend then offsets from it.
                    Controller::Pitch7_25(v) => {
                        self.set_voice_tuning(id, tutti_midi_types::q7_25_to_f32(v.to_bits()));
                    }
                    _ => {}
                }
            }
            // MIDI 2.0 Per-Note Management (M2-104 §7.4.5). D and S are distinct:
            //
            // **Detach (D=1)**: currently-playing notes on this note number keep
            // their current per-note controller values but stop responding to any
            // further per-note controllers (they play out frozen).
            //
            // **Reset (S=1)**: reset the note's per-note controllers to defaults
            // (pitch bend→0, pressure→0, slide→center) while it keeps sounding
            // *and* keeps responding.
            //
            // **D=1 & S=1**: the spec detaches the *currently playing* note (it
            // holds its values) while the Reset "applies to future notes only".
            // In this voice model a new note-on already starts at defaults, so the
            // future-notes reset needs no state — the live voice is simply detached.
            // Hence: detach wins for the live voice when both bits are set.
            Cv2::PerNoteManagement(m) => {
                let id =
                    NoteId::from_channel_note(MidiChannel::new(channel), u8::from(m.note_number()));
                if m.detach() {
                    self.detach_voice_mpe(id);
                } else if m.reset() {
                    self.reset_voice_mpe(id);
                }
            }
            // Registered Controller for Sensitivity of Per-Note Pitch Bend
            // (RPN #00/07, M2-104 §7.4.13). The 32-bit data is a 7.25 fixed-point
            // semitone range shared by all note numbers on the channel; it sets
            // the range subsequent Per-Note Pitch Bend messages sweep.
            Cv2::RegisteredController(m)
                if u8::from(m.bank()) == tutti_midi_types::ump::RPN_BANK_MPE
                    && u8::from(m.index())
                        == tutti_midi_types::ump::RPN_INDEX_PER_NOTE_PITCH_BEND_SENSITIVITY =>
            {
                let range =
                    tutti_midi_types::mpe::PitchBendSensitivity::from_rpn_bits(m.controller_data())
                        .as_semitones_f32();
                self.set_mpe_pitch_bend_range(tutti_core::Semitones(range));
            }
            _ => {}
        }
    }

    fn handle_note_on(&mut self, note: u8, vel_norm: f32, channel: u8) {
        let id = NoteId::from_channel_note(MidiChannel::new(channel), note);
        let result = self.allocator.allocate(id, note, channel, vel_norm);

        let slot_index = match result {
            AllocationResult::Allocated { slot_index } => Some(slot_index),
            AllocationResult::Stolen { slot_index } => Some(slot_index),
            AllocationResult::LegatoRetrigger { slot_index } => Some(slot_index),
            AllocationResult::Unavailable => None,
        };
        let is_legato = matches!(result, AllocationResult::LegatoRetrigger { .. });

        if let Some(slot_index) = slot_index {
            let base_freq = self.config.tuning.fractional_note_to_freq(f32::from(note));
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

    fn handle_note_off(&mut self, note: u8, channel: u8) {
        let id = NoteId::from_channel_note(MidiChannel::new(channel), note);
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
            .position(|s| s.id() == id && s.state() != crate::voice::VoiceState::Idle)
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

    /// MIDI 2.0 per-note pitch bend: modulate only the voice addressed by `id`.
    fn set_voice_mpe_pitch_bend(&mut self, id: NoteId, bend_norm: f32) {
        let semitones = tutti_core::Semitones(bend_norm * self.config.mpe_pitch_bend_range.get());
        self.with_voice_for_id(id, |v| v.set_mpe_pitch_bend(semitones));
    }

    /// MIDI 2.0 per-note pressure (poly key pressure): only the addressed voice.
    fn set_voice_mpe_pressure(&mut self, id: NoteId, norm: f32) {
        self.with_voice_for_id(id, |v| v.set_mpe_pressure(norm));
    }

    /// MIDI 2.0 per-note slide (CC74 / Brightness): only the addressed voice.
    fn set_voice_mpe_slide(&mut self, id: NoteId, value: f32) {
        self.with_voice_for_id(id, |v| v.set_mpe_slide(value));
    }

    /// MIDI 2.0 per-note gain (per-note Volume / CC7): only the addressed voice.
    fn set_voice_mpe_gain(&mut self, id: NoteId, value: f32) {
        self.with_voice_for_id(id, |v| v.set_mpe_gain(value));
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
    fn set_mpe_pitch_bend_range(&mut self, range: tutti_core::Semitones) {
        self.config.mpe_pitch_bend_range = range;
        for voice in &mut self.voices {
            voice.set_mpe_pitch_bend_range(range);
        }
    }

    fn handle_cc(&mut self, cc_num: CCNumber, value: f32, channel: u8) {
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

        // Multiplicative, matching every other bend site. They must all agree,
        // and the ratio is the only derivation they all *can* share:
        // `handle_note_on` bends `porta.current()` and the glide
        // (`control_step`) bends `porta.advance(..)` — interpolated
        // frequencies with no note number to look up in the tuning table.
        //
        // Adding the bend to the note number and re-entering
        // `fractional_note_to_freq` is the tempting alternative and is wrong
        // here. On an equal-tempered table it gives the same number; on any
        // unequal one it does not, so the same wheel position moves a held note
        // and a newly-struck note to different pitches — 3.9 cents apart on
        // just intonation. Agreeing everywhere is worth more than the table's
        // shape applying to the bend interval itself.
        let bend_multiplier = (self.config.pitch_bend_range * self.pitch_bend).to_pitch_ratio();
        let tuning = &self.config.tuning;
        self.voices
            .iter_mut()
            .filter(|v| v.is_active())
            .for_each(|voice| {
                let base_freq = tuning.fractional_note_to_freq(f32::from(voice.note()));
                voice.set_pitch(base_freq * bend_multiplier);
            });
    }

    fn sync_voice_gates(&mut self) {
        let slots = self.allocator.slots();
        for (i, voice) in self.voices.iter_mut().enumerate() {
            if voice.is_active()
                && voice.gate_value() > 0.0
                && i < slots.len()
                && slots[i].state() == crate::voice::VoiceState::Releasing
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
    /// The steps sit on a fixed grid of [`CONTROL_BLOCK`] frames measured from
    /// the block start, cut additionally at each MIDI event. So where the
    /// control points fall does not depend on where events split the block —
    /// and for any host block that is a multiple of the grid, not on the block
    /// size either: one 512-frame block renders sample-identically to eight
    /// 64-frame ones.
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

        // Index rather than `mem::take`: `take` would swap in a fresh `Vec` and
        // free the outgoing one here, on the audio thread, then re-allocate it
        // next span. (`drain` would not borrow-check: `mark_voice_finished`
        // takes `&mut self`.) `clear()` above resets the length each span.
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
        let mut block_start = 0;
        let mut event_idx = 0;

        while block_start < size {
            let block_end = if event_idx < midi_count {
                let next_offset = (self.midi_buffer[event_idx].frame_offset as usize).min(size);
                if next_offset <= block_start {
                    let event = self.midi_buffer[event_idx];
                    self.process_midi_event(&event);
                    event_idx += 1;
                    continue;
                }
                next_offset
            } else {
                size
            };

            self.render_span(block_start, block_end - block_start, volume, emit);
            block_start = block_end;
        }

        while event_idx < midi_count {
            let event = self.midi_buffer[event_idx];
            self.process_midi_event(&event);
            event_idx += 1;
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
    pub(crate) fn set_rate(&mut self, sample_rate: tutti_core::SampleRate) {
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
            #[cfg(test)]
            queued: Vec::new(),
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
