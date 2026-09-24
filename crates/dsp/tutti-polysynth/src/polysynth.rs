//! The synth itself: an [`AudioUnit`] that owns a MIDI inbox, an allocator and
//! a fixed set of voices, and renders their sum.
//!
//! This is where MIDI becomes sound. Everything else in the crate is a piece
//! this module drives — allocation (`crate::voice`), per-note DSP
//! (`crate::synth_voice`), glide, unison and tuning — and the MIDI 2.0 decoding
//! that routes a per-note message to exactly one voice lives here because only
//! this module holds both the note-id map and the voices.
//!
//! Both render paths are real-time and share one renderer: `process` splits
//! its block at MIDI event boundaries so events land on the right frame, and
//! `tick` is the same renderer run for a single frame. Neither allocates.
//!
//! The renderer works in control steps of at most `CONTROL_BLOCK` frames:
//! each step the voices (and any glide) write their lane targets into the
//! voice bank, and the bank renders the step, ramping every lane to its
//! targets sample by sample. The mix of one step lives in the bank, so no
//! scratch is sized to a block — any block length renders every frame.

use crate::bank::{VoiceBank, CONTROL_BLOCK};
use crate::synth_voice::SynthVoice;
use crate::SynthConfig;
use crate::{AllocationResult, Portamento, UnisonEngine, VoiceAllocator, VoiceAllocatorConfig};
use tutti_core::{
    Amplitude, AudioUnit, BufferMut, BufferRef, ChannelLayout, Param, SignalFrame, Tail,
    MAX_BUFFER_SIZE,
};
use tutti_midi_runtime::{MidiInPort, MidiSender};
use tutti_midi_types::ump::MidiEvent;
use tutti_midi_types::{cc, MidiUnitId, MidiUnitIn, NoteId};
use tutti_midi_types::{CCNumber, MidiChannel};

use std::sync::Arc;

/// Convert a Q7.25 fixed-point pitch (Registered Per-Note Controller #3, M2-104
/// §7.4.15.2) to a fractional MIDI note number: 7 integer bits = the 12-TET note,
/// 25 fractional bits = fraction of one semitone (HCU).
#[inline]
fn q7_25_to_fractional_note(bits: u32) -> f32 {
    bits as f32 / (1u32 << 25) as f32
}

/// Convert a Q7.9 fixed-point pitch (Note-On Attribute #3, M2-104 §7.4.15.3) to a
/// fractional MIDI note number: 7 integer bits + 9 fractional bits.
#[inline]
fn q7_9_to_fractional_note(bits: u16) -> f32 {
    bits as f32 / (1u16 << 9) as f32
}

/// Polyphonic subtractive synthesizer: a MIDI-driven voice allocator over a
/// SIMD voice bank.
///
/// Construct one from a [`SynthConfig`] via [`PolySynth::new`]. The synth always
/// owns a lock-free MIDI inbox; callers push events via [`PolySynth::midi_sender`].
/// For offline export, a [`MidiSnapshotReader`] is layered over that inbox via
/// [`PolySynth::set_midi_source`] (on an isolated clone, which has no live
/// inbox of its own).
///
/// [`MidiSnapshotReader`]: tutti_midi_runtime::MidiSnapshotReader
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
    /// This synth's MIDI input endpoint: routing address, push mailbox, and the
    /// current pull source (the live receiver by default; an override installs a
    /// `MidiClipSource`/`MidiSnapshotReader`). See [`MidiInPort`] for the fundsp
    /// clone/isolate sharing semantics all three obey.
    midi: MidiInPort,
    midi_buffer: Vec<MidiEvent>,
    /// Voices that finished this block, collected during the render loop and
    /// drained after it.
    ///
    /// Sized to `max_voices` at construction and only ever `clear()`ed, so it
    /// never reallocates: the worst case is every voice releasing at once,
    /// which is exactly its capacity. Growing it would be a `malloc` on the
    /// audio thread, which is what the `debug_assert` in `tick` watches for.
    finished_indices: Vec<usize>,
}

impl PolySynth {
    /// Build a synth from a [`SynthConfig`]. Configure the config the idiomatic
    /// Bevy way — `Default` plus struct-update — e.g.
    /// `SynthConfig { oscillator: OscillatorType::Saw, max_voices: 8, ..default() }`.
    ///
    /// Every voice — and every unison sub-voice within it — is built here, so
    /// this allocates in proportion to `max_voices * unison.voice_count` and
    /// belongs off the audio thread.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidConfig`](crate::Error::InvalidConfig) if
    /// `max_voices` is 0. There is **no upper bound**: the per-block
    /// finished-voice list is sized to `max_voices` here and only ever
    /// `clear()`ed, so it never reallocates however large that is.
    ///
    /// It used to reject anything above 16, which was the inline capacity of
    /// a `SmallVec` — a real constraint, since a spill would have put a
    /// `malloc` in the audio callback, but one the `Vec` removes rather than
    /// enforces. Every voice is fully constructed up front, so `max_voices`
    /// is a memory and CPU budget.
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
            midi: MidiInPort::new(),
            midi_buffer: vec![MidiEvent::noop(); 256],
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

    /// This synth's MIDI input endpoint — routing address, push mailbox, and the
    /// source-install slot, in one borrow.
    ///
    /// The whole-port accessor exists so a host can reach all three through a
    /// single downcast — one borrow beats three, and it keeps the routing
    /// address, the mailbox and the source slot from drifting apart.
    /// `tutti_soundfont::SoundFontUnit::midi_port` is the same shape.
    pub fn midi_port(&self) -> &MidiInPort {
        &self.midi
    }

    /// Producer handle for this synth's MIDI inbox. Cheap to clone; insert
    /// into a `MidiBus` or hand to anything that pushes MIDI events.
    pub fn midi_sender(&self) -> MidiSender {
        self.midi.sender()
    }

    /// Layer a MIDI source over the live inbox. Used by offline export for a
    /// [`MidiSnapshotReader`], or by clip playback for a
    /// [`tutti_midi_runtime::MidiClipSource`]. Both the source and the inbox
    /// are polled, so clip playback does not silence live input.
    ///
    /// The install is visible across fundsp's clone-on-commit (see
    /// [`MidiInPort`]), so the same instance reaches the box the audio thread runs.
    ///
    /// [`MidiSnapshotReader`]: tutti_midi_runtime::MidiSnapshotReader
    pub fn set_midi_source(&mut self, source: Arc<dyn MidiUnitIn>) {
        self.midi.install(source);
    }

    /// Drop a previously-layered source; subsequent ticks poll only the
    /// live `MidiReceiver`.
    pub fn clear_midi_source(&mut self) {
        self.midi.clear();
    }

    fn poll_count(&mut self, block_size: usize) -> usize {
        self.midi.poll(block_size, &mut self.midi_buffer)
    }

    fn poll_midi_events(&mut self) {
        // Single-sample tick path: treat the "block" as one sample
        // wide so a scheduler can still target this position.
        let count = self.poll_count(1);
        for i in 0..count {
            let event = self.midi_buffer[i];
            self.process_midi_event(&event);
        }
    }

    fn poll_midi_events_sorted(&mut self, block_size: usize) -> usize {
        let count = self.poll_count(block_size);
        if count > 1 {
            self.midi_buffer[..count].sort_unstable_by_key(|e| e.frame_offset);
        }
        count
    }

    /// Whether per-note (MPE) expression is applied at render time.
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

    /// Turn per-note (MPE) expression on or off, affecting sounding notes as
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

    /// Set the master output gain as a linear [`Amplitude`]: `1.0` is unity,
    /// `0.0` silence, and values above unity boost.
    ///
    /// Only the lower bound is enforced. A ceiling here would constrain nothing
    /// — `volume_atomic()` writes the same cell on the modulation path with no
    /// cap — and a boost above unity is legal for an `Amplitude`.
    ///
    /// Applied once per block to the summed mix, not per voice.
    ///
    /// `&self`, matching every other atomic-backed volume setter in the engine
    /// (`BusStripNode`, `ClickSettings`): the write lands in a shared cell, and
    /// [`volume_atomic`](Self::volume_atomic) hands that same cell to the
    /// modulation path — so `&mut` would advertise an exclusivity this type
    /// does not have.
    pub fn set_volume(&self, volume: f32) {
        self.master_volume.store(Amplitude(volume.max(0.0)));
    }

    /// The current master gain as a linear amplitude. Reflects modulation
    /// written through [`volume_atomic`](Self::volume_atomic) as well as
    /// [`set_volume`](Self::set_volume) — they share one cell.
    pub fn volume(&self) -> f32 {
        self.master_volume.load().get()
    }

    /// The shared master-volume atomic, for control-rate modulation
    /// (`tutti_mod::ModParams`). Clones the `Arc`; call it during setup, not
    /// from the audio path.
    pub fn volume_atomic(&self) -> Arc<tutti_core::AtomicF32> {
        self.master_volume.as_atomic()
    }

    /// The shared unison-detune atomic (cents), for control-rate modulation.
    /// `None` when this synth has no unison engine.
    pub fn detune_atomic(&self) -> Option<Arc<tutti_core::AtomicF32>> {
        self.unison.as_ref().map(|u| u.detune_atomic())
    }

    /// The shared unison-stereo-spread atomic (0..1), for control-rate modulation.
    /// `None` when this synth has no unison engine.
    pub fn spread_atomic(&self) -> Option<Arc<tutti_core::AtomicF32>> {
        self.unison.as_ref().map(|u| u.spread_atomic())
    }

    /// How many voices are currently sounding, counting those in their release
    /// stage. A voice stays counted until its envelope reaches silence, which is
    /// why this can exceed the number of keys held.
    pub fn active_voice_count(&self) -> usize {
        self.voices.iter().filter(|v| v.is_active()).count()
    }

    /// This synth's unison settings, or `None` when it was built without a
    /// [`UnisonConfig`](crate::UnisonConfig). A synth built without one cannot
    /// gain unison later —
    /// every unison setter below is a no-op on it.
    pub fn unison_config(&self) -> Option<&crate::UnisonConfig> {
        self.unison.as_ref().map(|u| u.config())
    }

    /// Sub-voices stacked per note, clamped to 1..=16. Returns `1` when this
    /// synth has no unison engine, so the total oscillator count is always
    /// `max_voices * this`.
    pub fn unison_voice_count(&self) -> usize {
        self.unison.as_ref().map_or(1, |u| u.voice_count())
    }

    /// Set the unison detune in [`Cents`](tutti_core::Cents), applied
    /// symmetrically either side of the written pitch. Negative values clamp to
    /// zero. Recomputes the per-sub-voice pitch ratios immediately, so it
    /// affects sounding notes as well as future ones. No-op without unison.
    pub fn set_unison_detune(&mut self, cents: impl Into<tutti_core::Cents>) {
        if let Some(unison) = &mut self.unison {
            unison.set_detune(cents);
        }
    }

    /// Set how wide the sub-voices are panned, 0.0 (all centre) to 1.0
    /// (outermost pair hard left and right); out-of-range values are clamped.
    /// Takes effect on sounding notes. No-op without unison.
    pub fn set_unison_stereo_spread(&mut self, spread: f32) {
        if let Some(unison) = &mut self.unison {
            unison.set_stereo_spread(spread);
        }
    }

    /// Set the number of stacked sub-voices per note, clamped to 1..=16.
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

    /// Replace all unison settings at once. Carries the same allocation warning
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

    /// Seed the phase-randomisation generator, making a render with
    /// `phase_randomize` reproducible. A seed of `0` is remapped to `1`, since
    /// the xorshift generator cannot leave that state. No-op without unison.
    pub fn seed_unison_rng(&mut self, seed: u32) {
        if let Some(unison) = &mut self.unison {
            unison.seed_rng(seed);
        }
    }

    /// The computed per-sub-voice pitch ratio, pan, phase offset and amplitude
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
                    self.set_voice_tuning(id, q7_9_to_fractional_note(p.to_bits()));
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
                        self.set_voice_tuning(id, q7_25_to_fractional_note(v.to_bits()));
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
        if let Some(ref porta) = self.portamento {
            if porta.is_gliding() {
                return;
            }
        }

        // Multiplicative, matching every other bend site. All four must agree,
        // and the ratio is the only derivation all four *can* share:
        // `handle_note_on` bends `porta.current()` and both glide sites bend
        // `porta.tick()` — interpolated frequencies with no note number to look
        // up in the tuning table.
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
    /// The glide is ticked per frame and the voices are aimed at where it will
    /// be at the *end* of the step; the bank ramps each lane there sample by
    /// sample. So a glide moves the pitch every frame in `process` as it does
    /// in `tick`. The block path used to tick the glide `block_len` times and
    /// then render the whole sub-block at the final pitch: a staircase, one
    /// step per MIDI sub-block, that `tick` never produced.
    fn control_step(&mut self, frames: usize) {
        if let Some(ref mut porta) = self.portamento {
            if porta.is_gliding() {
                let mut freq = porta.current();
                for _ in 0..frames {
                    freq = porta.tick();
                }
                // Hoisted: neither operand changes inside a step (MIDI is
                // handled between steps).
                let bend_multiplier =
                    (self.config.pitch_bend_range * self.pitch_bend).to_pitch_ratio();
                for voice in self.voices.iter_mut().filter(|v| v.is_active()) {
                    voice.set_pitch(freq * bend_multiplier);
                }
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
    /// bounded by a block size: this used to mix into `[f32; MAX_BUFFER_SIZE]`
    /// stack arrays and clamp `size` to them, which in a release build silently
    /// rendered only the first 64 frames of a longer block.
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

    /// Render `left.len()` frames into planar slices, MIDI included — the
    /// block path without `BufferMut`'s 64-frame channel stride, so a test can
    /// hand it a block of any length.
    #[cfg(test)]
    pub(crate) fn render_planar(&mut self, left: &mut [f32], right: &mut [f32]) {
        assert_eq!(left.len(), right.len());
        let size = left.len();
        if size == 0 {
            return;
        }
        if let Some(unison) = &mut self.unison {
            unison.sync_from_atomics();
        }
        let midi_count = self.poll_midi_events_sorted(size);
        let volume = self.master_volume.load().get();
        self.render_events(size, midi_count, volume, &mut |i, l, r| {
            left[i] = l;
            right[i] = r;
        });
    }
}

impl AudioUnit for PolySynth {
    fn reset(&mut self) {
        for voice in &mut self.voices {
            voice.reset();
        }
        self.bank.reset();
        self.allocator.reset();
        if let Some(ref mut porta) = self.portamento {
            porta.reset(440.0);
        }
    }

    /// Sever every live handle this clone shares with the original synth, so a
    /// worker thread can tick it concurrently with live playback safely.
    ///
    /// `clone()` shares two layers of live state by `Arc` (correct for the
    /// commit-clone, where only the original is ticked, but unsafe for an offline
    /// render ticked on a worker thread while the live synth keeps playing):
    ///
    /// 1. **MIDI input** — the [`MidiInPort`]. A shared inbox is drained to
    ///    exactly one consumer, so the worker would *steal* the live synth's
    ///    note-ons/offs/CC, and a shared source cell means clearing here would
    ///    sever the live clip. Both are fixed by [`MidiInPort::isolate`], which
    ///    mints a fresh private mailbox + source cell (nothing holds this new
    ///    sender, so the port stays permanently empty).
    ///
    /// 2. **Sounding voices.** Voice state is plain data — the per-sub-voice
    ///    `Shared` atomics a clone used to alias are gone with the fundsp chain
    ///    that read them — but a clone still carries the live synth's sounding
    ///    notes, which an offline render must not replay. The voices and the
    ///    bank are reset to a clean, inactive set, which is exactly the correct
    ///    event-free render state, and the allocator is reset to agree the
    ///    slots are free.
    ///
    /// After `isolate()` the synth reads nothing from, and writes nothing into,
    /// the live world — it renders silence until its own (now-empty) inbox feeds
    /// it events, which it never will.
    fn isolate(&mut self) {
        // Fresh private mailbox + source cell, same unit id — severs both the
        // shared inbox (no event theft) and the shared source (clearing here
        // can't disturb the live clip). See [`MidiInPort::isolate`].
        self.midi.isolate();

        // A clean, inactive voice set (see #2 above).
        for voice in &mut self.voices {
            voice.reset();
        }
        self.bank.reset();
        self.allocator.reset();
    }

    fn set_sample_rate(&mut self, sample_rate: tutti_core::SampleRate) {
        self.bank.set_sample_rate(sample_rate);
        if let Some(ref mut porta) = self.portamento {
            porta.set_sample_rate(sample_rate);
        }
    }

    fn tick(&mut self, _input: &[f32], output: &mut [f32]) {
        self.poll_midi_events();

        // Fold any control-rate detune/spread modulation in (no-op when unchanged).
        if let Some(unison) = &mut self.unison {
            unison.sync_from_atomics();
        }

        // The block renderer, one frame long: `tick` and `process` share every
        // line of DSP, so they cannot drift apart.
        let volume = self.master_volume.load().get();
        let mut frame = [0.0f32; 2];
        self.render_span(0, 1, volume, &mut |_, l, r| frame = [l, r]);

        output[0] = frame[0];
        if output.len() > 1 {
            output[1] = frame[1];
        }
    }

    fn process(&mut self, size: usize, _input: &BufferRef, output: &mut BufferMut) {
        if size == 0 {
            return;
        }

        // Fold any control-rate detune/spread modulation into the unison params
        // once per block before rendering (a no-op when nothing moved).
        if let Some(unison) = &mut self.unison {
            unison.sync_from_atomics();
        }

        let midi_count = self.poll_midi_events_sorted(size);
        let stereo = ChannelLayout::from(output.channels()).is_multi();

        // The renderer has no block ceiling; `BufferMut` does. Each of its
        // channels holds exactly `MAX_BUFFER_SIZE` frames — frame 64 of channel
        // 0 *is* frame 0 of channel 1 — so a larger `size` cannot have come
        // from a well-formed buffer, and the debug assert catches that contract
        // breach in tests. In release the synth still renders all `size`
        // frames, so its clock and every MIDI offset stay where the host put
        // them, and writes the frames the buffer can hold.
        debug_assert!(
            size <= MAX_BUFFER_SIZE,
            "process size {size} exceeds the BufferMut channel stride ({MAX_BUFFER_SIZE})"
        );

        let volume = self.master_volume.load().get();
        self.render_events(size, midi_count, volume, &mut |i, l, r| {
            if i < MAX_BUFFER_SIZE {
                output.set_f32(0, i, l);
                if stereo {
                    output.set_f32(1, i, r);
                }
            }
        });
    }

    fn inputs(&self) -> usize {
        0
    }

    fn outputs(&self) -> usize {
        2
    }

    fn route(&mut self, _input: &SignalFrame, _frequency: f64) -> SignalFrame {
        SignalFrame::new(self.outputs())
    }

    /// The amplitude envelope's release stage.
    ///
    /// This node takes no audio input — it is driven by MIDI — so "after the
    /// input stops" means after the last note-off, at which point every
    /// still-sounding voice rings for its release time. That time is an authored
    /// parameter rather than an estimate, which is what makes this exact.
    ///
    /// A bounce that ends at the last note-off without it chops the release off
    /// every note in the project.
    fn tail(&mut self) -> Tail {
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

    fn set(&mut self, _setting: tutti_core::Setting) {}

    fn get_id(&self) -> u64 {
        crate::node_id::POLY_SYNTH_ID
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
    }

    fn footprint(&self) -> usize {
        core::mem::size_of::<Self>()
            + self.voices.capacity() * core::mem::size_of::<SynthVoice>()
            + self.bank.footprint()
            + self.midi_buffer.capacity() * core::mem::size_of::<MidiEvent>()
    }
}

impl PolySynth {
    /// This unit's MIDI routing address.
    pub fn midi_unit_id(&self) -> MidiUnitId {
        self.midi.unit_id()
    }
}

impl tutti_mod::ModParams for PolySynth {
    /// The synth's control-rate-modulatable params: `Volume`, and — only when
    /// this synth has a unison engine — `Detune` and `StereoSpread`.
    ///
    /// `Volume` mirrors the master atomic, which the mix is scaled by once per
    /// block. `Detune`/`StereoSpread` mirror the unison atomics, folded back
    /// into the per-sub-voice params once per block; they change pitch ratios
    /// and pans, never anything per-sample, so block-rate folding is exact
    /// rather than an approximation.
    ///
    /// Everything else returns `None`. Discrete params (voice count) are
    /// deliberately not modulatable, and a foreign `ParamAddr::Id` is not this
    /// synth's vocabulary — a WASM node's param names are, which is why that
    /// arm belongs to the extension host and not here.
    fn mod_target(
        &self,
        param: tutti_core::ParamAddr,
        base: f32,
        min: f32,
        max: f32,
    ) -> Option<Arc<dyn tutti_mod::ModTarget>> {
        use tutti_core::{ParamAddr, UnitParam};
        let atomic = match param {
            ParamAddr::Unit(UnitParam::Volume) => self.volume_atomic(),
            ParamAddr::Unit(UnitParam::Detune) => self.detune_atomic()?,
            ParamAddr::Unit(UnitParam::StereoSpread) => self.spread_atomic()?,
            _ => return None,
        };
        Some(Arc::new(tutti_mod::AtomicTarget::with_mirror(
            base, min, max, atomic,
        )))
    }
}

impl Clone for PolySynth {
    fn clone(&self) -> Self {
        // The `MidiInPort` clone shares the mailbox + source cell (so an
        // outstanding sender and any install keep reaching the running box);
        // `isolate()` is what severs it for an offline render.
        Self {
            config: self.config.clone(),
            allocator: self.allocator.clone(),
            voices: self.voices.clone(),
            bank: self.bank.clone(),
            portamento: self.portamento.clone(),
            unison: self.unison.clone(),
            pitch_bend: self.pitch_bend,
            master_volume: self.master_volume.clone(),
            midi: self.midi.clone(),
            midi_buffer: vec![MidiEvent::noop(); 256],
            finished_indices: Vec::with_capacity(self.config.max_voices),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        EnvelopeConfig, FilterType, OscillatorType, PortamentoConfig, PortamentoCurve,
        PortamentoMode, SynthConfig, UnisonConfig, VoiceMode,
    };
    use tutti_core::{Amplitude, Hz, Resonance, Seconds, Spread};
    use tutti_midi_types::convert::{
        midi1_cc_to_midi2, midi1_pitch_bend_to_midi2, midi1_velocity_to_midi2,
    };
    use tutti_midi_types::{CCNumber, MidiChannel, MidiGroup};

    /// Build a `PolySynth` from a config, unwrapping the result.
    fn synth(config: SynthConfig) -> PolySynth {
        PolySynth::new(config).expect("synth builds")
    }

    /// A full-ceiling block renders every frame it was asked for.
    ///
    /// `size == MAX_BUFFER_SIZE` is the largest block `BufferMut` can carry —
    /// each channel is exactly that many frames — so this is the boundary where
    /// an off-by-one in writing the output would show. Blocks longer than that
    /// cannot be expressed through `process`; the renderer behind it handles
    /// any length, which `a_long_block_renders_every_frame_like_short_blocks`
    /// covers.
    #[test]
    fn a_full_ceiling_block_renders_every_frame() {
        use tutti_core::BufferVec;

        let mut synth = synth(SynthConfig {
            sample_rate: tutti_core::SampleRate::SR_44K1,
            max_voices: 4,
            // Near-instant attack, so the note is audible within the first block
            // rather than still ramping up from silence.
            envelope: EnvelopeConfig {
                attack: Seconds(0.001),
                decay: Seconds(0.0),
                sustain: Amplitude(1.0),
                release: Seconds(0.5),
            },
            ..Default::default()
        });
        queue_midi(&synth, &[ev_note_on(0, 69, 100)]);

        let input = BufferVec::new(2);
        let mut output = BufferVec::new(2);

        // Several full-ceiling blocks, not one: the voice's own smoothing ramps
        // mean the first blocks after a note-on are legitimately near-silent, so a
        // single block proves nothing about the buffer sizing.
        let mut peak = 0.0f32;
        for _ in 0..4 {
            synth.process(
                MAX_BUFFER_SIZE,
                &input.buffer_ref(),
                &mut output.buffer_mut(),
            );
            let left = output.buffer_ref().channel_f32(0);
            // Read all MAX_BUFFER_SIZE frames — indexing the full width is itself
            // the check that `process` wrote them.
            peak = left[..MAX_BUFFER_SIZE]
                .iter()
                .fold(peak, |a, s| a.max(s.abs()));
        }

        assert!(
            peak > 0.0,
            "a held note must produce audio across full-ceiling blocks"
        );
        assert_eq!(
            synth.active_voice_count(),
            1,
            "the note is still held after rendering"
        );
    }

    /// Push events directly through the synth's own MIDI sender.
    fn queue_midi(synth: &PolySynth, events: &[MidiEvent]) {
        synth.midi_sender().queue(events);
    }

    // --- Test event builders (MIDI 1.0 7-bit values upconverted to UMP CV2) ---

    fn ev_note_on(channel: u8, note: u8, vel: u8) -> MidiEvent {
        MidiEvent::note_on(
            MidiGroup::FIRST,
            MidiChannel::new(channel),
            note,
            midi1_velocity_to_midi2(vel),
        )
    }

    fn ev_note_off(channel: u8, note: u8) -> MidiEvent {
        MidiEvent::note_off(MidiGroup::FIRST, MidiChannel::new(channel), note, 0)
    }

    fn ev_cc(channel: u8, cc_num: CCNumber, value: u8) -> MidiEvent {
        MidiEvent::cc(
            MidiGroup::FIRST,
            MidiChannel::new(channel),
            cc_num,
            midi1_cc_to_midi2(value),
        )
    }

    fn ev_bend(channel: u8, bend14: u16) -> MidiEvent {
        MidiEvent::pitch_bend(
            MidiGroup::FIRST,
            MidiChannel::new(channel),
            midi1_pitch_bend_to_midi2(bend14),
        )
    }

    fn ev_aftertouch(channel: u8, pressure: u8) -> MidiEvent {
        MidiEvent::channel_pressure(
            MidiGroup::FIRST,
            MidiChannel::new(channel),
            midi1_cc_to_midi2(pressure),
        )
    }

    /// Regression: an offline render clones the live synth and ticks the clone
    /// on a worker thread. Before `isolate()`, the clone shared the live MIDI
    /// receiver, so ticking it drained — *stole* — the events the live synth
    /// needed (each event is delivered to exactly one consumer), garbling live
    /// playback for the whole render. After `isolate()` the clone has its own
    /// dead inbox: events sent on the original's sender reach ONLY the original,
    /// and the clone receives nothing.
    #[test]
    fn isolate_severs_shared_midi_inbox_no_theft() {
        let mut live = synth(SynthConfig {
            sample_rate: tutti_core::SampleRate::SR_44K1,
            max_voices: 4,
            voice_mode: VoiceMode::Poly,
            oscillator: OscillatorType::Sine,
            ..Default::default()
        });

        // The render's clone, isolated as `rebind_net_transport` does.
        let mut render = live.clone();
        render.isolate();

        // Queue a note via the LIVE synth's sender (what the app holds).
        queue_midi(&live, &[ev_note_on(0, 60, 100)]);

        // Tick the render clone FIRST — if it still shared the inbox it would
        // drain the note here, stealing it from the live synth.
        let mut out = [0.0f32; 2];
        render.tick(&[], &mut out);
        assert_eq!(
            render.active_voice_count(),
            0,
            "isolated clone must not receive events from the original's sender"
        );

        // The live synth still gets its note — nothing was stolen.
        live.tick(&[], &mut out);
        assert_eq!(
            live.active_voice_count(),
            1,
            "live synth must still receive its note after the clone is ticked"
        );
    }

    /// A source that emits one note-on at offset 0 on its first poll — enough to
    /// prove it was the thing polled (activates exactly one voice).
    struct NoteOnceSource {
        note: u8,
    }
    impl MidiUnitIn for NoteOnceSource {
        fn poll_unit(&self, _unit: MidiUnitId, _block: usize, buffer: &mut [MidiEvent]) -> usize {
            if buffer.is_empty() {
                return 0;
            }
            buffer[0] = ev_note_on(0, self.note, 100);
            1
        }
    }

    /// The regression guard for [[plugin-source-install-shared-cell]] on the synth
    /// side: installing a clip source on ONE clone must be visible to ANOTHER
    /// clone, because fundsp runs a different clone than the one the install call
    /// mutates. A per-clone `Option<Arc<…>>` fails this silently — the install
    /// succeeds and synth clip playback never reaches the audio thread.
    #[test]
    fn midi_source_install_propagates_across_clones() {
        let live = synth(SynthConfig {
            sample_rate: tutti_core::SampleRate::SR_44K1,
            max_voices: 4,
            voice_mode: VoiceMode::Poly,
            oscillator: OscillatorType::Sine,
            ..Default::default()
        });
        // `clone_a` is the ECS-handle-style clone the install call mutates;
        // `audio_clone` is the box the audio thread would run.
        let mut clone_a = live.clone();
        let mut audio_clone = live.clone();

        clone_a.set_midi_source(Arc::new(NoteOnceSource { note: 60 }));

        // The audio clone must see the install (shared slot) and activate a voice.
        let mut out = [0.0f32; 2];
        audio_clone.tick(&[], &mut out);
        assert_eq!(
            audio_clone.active_voice_count(),
            1,
            "install on clone_a must reach audio_clone via the shared slot"
        );

        // Clearing on one clone clears for the other.
        clone_a.clear_midi_source();
        let mut fresh = live.clone();
        fresh.tick(&[], &mut out);
        assert_eq!(
            fresh.active_voice_count(),
            0,
            "clear on clone_a must propagate — no source polled"
        );
    }

    /// Regression: the deeper half of the same bug. `SynthVoice` used to hold
    /// its `gate`/`pitch`/`filter_*` as `Shared` (`Arc<AtomicU32>`), which
    /// `clone()` aliases — and the voice *wrote* them every tick. So a worker
    /// ticking the render clone would stomp the atomics the live voice reads
    /// into its output, even with the MIDI inbox already severed. The voice
    /// bank made those plain fields, so the property is now structural; this
    /// keeps it pinned. Driving the clone's voice must leave the live voice's
    /// gate untouched.
    #[test]
    fn isolate_unaliases_voice_shared_params() {
        let mut live = synth(SynthConfig {
            sample_rate: tutti_core::SampleRate::SR_44K1,
            max_voices: 4,
            voice_mode: VoiceMode::Poly,
            oscillator: OscillatorType::Sine,
            ..Default::default()
        });

        // Activate a voice on the live synth and tick so its gate Shared = 1.0.
        queue_midi(&live, &[ev_note_on(0, 60, 100)]);
        let mut out = [0.0f32; 2];
        live.tick(&[], &mut out);
        assert_eq!(live.active_voice_count(), 1);
        let live_gate_before = live.voices[0].gate_value();
        assert_eq!(live_gate_before, 1.0, "live voice gate should be open");

        // Clone + isolate (the render path). Then drive a note-off through the
        // clone's *own* (fresh) machinery: gate the clone's voice shut.
        let mut render = live.clone();
        render.isolate();
        // A clone that still ALIASED the voice Shared would, by note_off on its
        // voice, also slam the live voice's gate to 0.0.
        if let Some(v) = render.voices.get_mut(0) {
            v.note_off();
        }
        render.tick(&[], &mut out);

        // The live voice's gate must be untouched by the clone's note_off.
        assert_eq!(
            live.voices[0].gate_value(),
            1.0,
            "live voice gate must stay open — clone's voice Shared is unaliased"
        );
    }

    #[test]
    fn test_unison_creates_subvoices() {
        // Create synth with 3-voice unison
        let synth = synth(SynthConfig {
            sample_rate: tutti_core::SampleRate::SR_44K1,
            max_voices: 4,
            voice_mode: VoiceMode::Poly,
            oscillator: OscillatorType::Saw,
            unison: Some(UnisonConfig {
                voice_count: 3,
                detune_cents: tutti_core::Cents(15.0),
                stereo_spread: Spread(0.5),
                phase_randomize: false,
            }),
            ..Default::default()
        });

        // Each voice should have 3 sub-voices
        assert_eq!(synth.voices[0].sub_voice_count(), 3);
        assert_eq!(synth.voices[1].sub_voice_count(), 3);

        // Unison engine should be present
        assert!(synth.unison.is_some());
    }

    #[test]
    fn mod_params_volume_moves_the_master_atomic() {
        use tutti_core::{ParamAddr, UnitParam};
        use tutti_mod::{LayerKey, ModParams};

        let synth = synth(SynthConfig {
            sample_rate: tutti_core::SampleRate::SR_44K1,
            max_voices: 2,
            ..Default::default()
        });
        let vol_atomic = synth.volume_atomic();
        let target = synth
            .mod_target(ParamAddr::Unit(UnitParam::Volume), 1.0, 0.0, 1.0)
            .expect("volume is modulatable");

        target.accumulate(LayerKey(1), -0.4);
        assert!((target.final_value() - 0.6).abs() < 1e-4);
        assert!(
            (vol_atomic.load(core::sync::atomic::Ordering::Acquire) - 0.6).abs() < 1e-4,
            "the synth's master-volume atomic reflects the modulation"
        );

        // A foreign (plugin) id is not the synth's vocabulary.
        assert!(synth.mod_target(ParamAddr::Id(0), 0.5, 0.0, 1.0).is_none());
    }

    #[test]
    fn mod_params_detune_is_present_with_unison_absent_without() {
        use tutti_core::{ParamAddr, UnitParam};
        use tutti_mod::ModParams;

        let with_unison = synth(SynthConfig {
            unison: Some(UnisonConfig {
                voice_count: 3,
                detune_cents: tutti_core::Cents(10.0),
                stereo_spread: Spread(0.5),
                phase_randomize: false,
            }),
            ..Default::default()
        });
        assert!(with_unison
            .mod_target(ParamAddr::Unit(UnitParam::Detune), 10.0, 0.0, 100.0)
            .is_some());
        assert!(with_unison
            .mod_target(ParamAddr::Unit(UnitParam::StereoSpread), 0.5, 0.0, 1.0)
            .is_some());

        let no_unison = synth(SynthConfig::default());
        assert!(no_unison
            .mod_target(ParamAddr::Unit(UnitParam::Detune), 0.0, 0.0, 100.0)
            .is_none());
    }

    #[test]
    fn mod_params_detune_recomputes_unison_on_process() {
        use tutti_core::{ParamAddr, UnitParam};
        use tutti_mod::{LayerKey, ModParams};

        let mut synth = synth(SynthConfig {
            sample_rate: tutti_core::SampleRate::SR_44K1,
            max_voices: 2,
            oscillator: OscillatorType::Saw,
            unison: Some(UnisonConfig {
                voice_count: 3,
                detune_cents: tutti_core::Cents(0.0),
                stereo_spread: Spread(0.0),
                phase_randomize: false,
            }),
            ..Default::default()
        });

        // Detune starts at 0 → all sub-voices at unity freq ratio.
        // Index 0 is an edge sub-voice (position -1), so detune actually moves it
        // (the center voice at index 1 stays at unity by construction).
        let ratio_before = synth.unison.as_ref().unwrap().voice_params(0).freq_ratio;
        assert!((ratio_before - 1.0).abs() < 1e-4, "no detune yet");

        // Route a modulation offset into the detune atomic, then run a block:
        // `sync_from_atomics` must fold it into a recompute.
        let target = synth
            .mod_target(ParamAddr::Unit(UnitParam::Detune), 0.0, 0.0, 100.0)
            .expect("detune modulatable");
        target.accumulate(LayerKey(1), 30.0);

        let mut out = [0.0f32; 64];
        synth.tick(&[], &mut out);

        let ratio_after = synth.unison.as_ref().unwrap().voice_params(0).freq_ratio;
        assert!(
            (ratio_after - 1.0).abs() > 1e-4,
            "detune modulation reached the unison voice params after a block \
             (before={ratio_before}, after={ratio_after})"
        );
    }

    #[test]
    fn test_unison_stereo_output() {
        let mut synth = synth(SynthConfig {
            sample_rate: tutti_core::SampleRate::SR_44K1,
            max_voices: 2,
            voice_mode: VoiceMode::Poly,
            oscillator: OscillatorType::Saw,
            envelope: EnvelopeConfig {
                attack: Seconds(0.001),
                decay: Seconds(0.1),
                sustain: Amplitude(0.8),
                release: Seconds(0.1),
            }, // Fast attack to get output quickly
            unison: Some(UnisonConfig {
                voice_count: 3,
                detune_cents: tutti_core::Cents(15.0),
                stereo_spread: Spread(1.0), // Full stereo spread
                phase_randomize: false,
            }),
            ..Default::default()
        });

        // Verify unison is set up
        assert!(synth.unison.is_some());
        assert_eq!(synth.voices[0].sub_voice_count(), 3);

        // Trigger a note via registry
        let note_on = ev_note_on(0, 60, 100);
        queue_midi(&synth, &[note_on]);

        // Process samples and accumulate max output
        // Note: FunDSP EnvelopeIn samples at 2ms intervals (~88 samples at 44100Hz)
        // so several hundred samples are needed to see envelope output
        let mut output = [0.0f32; 2];
        let mut max_left = 0.0f32;
        let mut max_right = 0.0f32;

        for _ in 0..2000 {
            synth.tick(&[], &mut output);
            max_left = max_left.max(output[0].abs());
            max_right = max_right.max(output[1].abs());
        }

        // With full stereo spread, both channels should carry output
        // (the left/right sub-voices should be panned to opposite sides)
        assert!(
            max_left > 0.0,
            "Expected non-zero left channel output, got {}",
            max_left
        );
        assert!(
            max_right > 0.0,
            "Expected non-zero right channel output, got {}",
            max_right
        );
    }

    #[test]
    fn test_no_unison_single_subvoice() {
        // Create synth without unison
        let synth = synth(SynthConfig {
            sample_rate: tutti_core::SampleRate::SR_44K1,
            max_voices: 4,
            voice_mode: VoiceMode::Poly,
            oscillator: OscillatorType::Sine,
            ..Default::default()
        });

        // Each voice should have 1 sub-voice (no unison)
        assert_eq!(synth.voices[0].sub_voice_count(), 1);

        // Unison engine should be None
        assert!(synth.unison.is_none());
    }

    /// One sub-voice's chain — oscillator, envelope, gains — is silent while
    /// its gate is closed and sounds once it opens.
    ///
    /// This drove fundsp's `var(&pitch) >> (saw() * (var(&gate) >>
    /// adsr_live(..)))` until that chain was replaced by the voice bank; the
    /// claim is the same, made of the chain the synth now runs.
    ///
    /// *Mutation:* dropping `gate_on`'s `enter(Attack)` (so the gate never
    /// opens the envelope) fails the second half.
    #[test]
    fn test_basic_dsp_chain() {
        use crate::bank::VoiceBank;
        use tutti_core::Phase;

        let mut bank = VoiceBank::new(
            OscillatorType::Saw,
            &FilterType::None,
            &EnvelopeConfig::new(Seconds(0.001), Seconds(0.1), Amplitude(0.8), Seconds(0.1)),
            tutti_core::SampleRate(44100.0),
            1,
            1,
        );
        bank.set_pitch(0, Hz(440.0));
        bank.set_gains(0, Amplitude::UNITY, Amplitude::UNITY);
        bank.snap_gain(0);
        bank.start(0, Phase(0.3));

        // Gate closed: the oscillator runs, the envelope holds it at zero.
        let mut closed = 0.0f32;
        for _ in 0..32 {
            bank.mark_live(0);
            let (l, _) = bank.render(16);
            closed = l.iter().fold(closed, |a, s| a.max(s.abs()));
        }
        assert_eq!(closed, 0.0, "a closed gate must be exact silence");

        bank.gate_on(0);
        let mut max_out = 0.0f32;
        for _ in 0..32 {
            bank.mark_live(0);
            let (l, _) = bank.render(16);
            max_out = l.iter().fold(max_out, |a, s| a.max(s.abs()));
        }
        assert!(
            max_out > 0.1,
            "Chain should produce output after triggering gate, max={}",
            max_out
        );
    }

    #[test]
    fn test_dynamic_unison_resize() {
        // Create synth with 2-voice unison
        let mut synth = synth(SynthConfig {
            sample_rate: tutti_core::SampleRate::SR_44K1,
            max_voices: 2,
            voice_mode: VoiceMode::Poly,
            oscillator: OscillatorType::Saw,
            unison: Some(UnisonConfig {
                voice_count: 2,
                detune_cents: tutti_core::Cents(10.0),
                stereo_spread: Spread(0.5),
                phase_randomize: false,
            }),
            ..Default::default()
        });

        // Initial state: 2 sub-voices per polyphonic voice
        assert_eq!(synth.voices[0].sub_voice_count(), 2);
        assert_eq!(synth.voices[1].sub_voice_count(), 2);
        assert_eq!(synth.unison_voice_count(), 2);

        // Increase to 5 unison voices
        synth.set_unison_voice_count(5);
        assert_eq!(synth.voices[0].sub_voice_count(), 5);
        assert_eq!(synth.voices[1].sub_voice_count(), 5);
        assert_eq!(synth.unison_voice_count(), 5);

        // Decrease to 3 unison voices
        synth.set_unison_voice_count(3);
        assert_eq!(synth.voices[0].sub_voice_count(), 3);
        assert_eq!(synth.voices[1].sub_voice_count(), 3);
        assert_eq!(synth.unison_voice_count(), 3);

        // Verify it still produces sound
        let note_on = ev_note_on(0, 60, 100);
        queue_midi(&synth, &[note_on]);

        let mut output = [0.0f32; 2];
        let mut max_out = 0.0f32;
        for _ in 0..2000 {
            synth.tick(&[], &mut output);
            max_out = max_out.max(output[0].abs().max(output[1].abs()));
        }

        assert!(
            max_out > 0.0,
            "Synth should produce output after resize, got max={}",
            max_out
        );
    }

    #[test]
    fn test_note_off_respects_channel() {
        let mut synth = synth(SynthConfig {
            sample_rate: tutti_core::SampleRate::SR_44K1,
            max_voices: 4,
            voice_mode: VoiceMode::Poly,
            oscillator: OscillatorType::Sine,
            envelope: EnvelopeConfig {
                attack: Seconds(0.001),
                decay: Seconds(0.1),
                sustain: Amplitude(1.0),
                release: Seconds(0.1),
            },
            ..Default::default()
        });

        // Play same note (C4=60) on channel 0 and channel 1
        // note_on(MidiGroup::new(frame_offset), MidiChannel::new(channel), note, velocity)
        let note_on_ch0 = ev_note_on(0, 60, 100);
        let note_on_ch1 = ev_note_on(1, 60, 100);
        queue_midi(&synth, &[note_on_ch0, note_on_ch1]);

        // Process to trigger both notes
        let mut output = [0.0f32; 2];
        synth.tick(&[], &mut output);
        assert_eq!(synth.active_voice_count(), 2, "Should have 2 active voices");

        // Note off on channel 0 only
        // note_off(MidiGroup::new(frame_offset), MidiChannel::new(channel), note, velocity)
        let note_off_ch0 = ev_note_off(0, 60);
        queue_midi(&synth, &[note_off_ch0]);
        synth.tick(&[], &mut output);

        // Channel 1's voice should still be active (gate=1.0)
        // Channel 0's voice should be releasing (gate=0.0) but still active
        // until envelope finishes
        let mut ch1_still_gated = false;
        for voice in &synth.voices {
            if voice.is_active() && voice.channel() == 1 && voice.note() == 60 {
                assert!(
                    voice.gate_value() > 0.0,
                    "Channel 1 voice should still have gate open"
                );
                ch1_still_gated = true;
            }
        }
        assert!(
            ch1_still_gated,
            "Channel 1 voice should still be active and gated"
        );
    }

    #[test]
    fn test_cc64_sustain_pedal_holds_notes() {
        let mut synth = synth(SynthConfig {
            sample_rate: tutti_core::SampleRate::SR_44K1,
            max_voices: 4,
            voice_mode: VoiceMode::Poly,
            oscillator: OscillatorType::Sine,
            envelope: EnvelopeConfig {
                attack: Seconds(0.001),
                decay: Seconds(0.0),
                sustain: Amplitude(1.0),
                release: Seconds(0.05),
            },
            ..Default::default()
        });

        // Play note
        let note_on = ev_note_on(0, 60, 100);
        queue_midi(&synth, &[note_on]);
        let mut output = [0.0f32; 2];
        synth.tick(&[], &mut output);
        assert_eq!(synth.active_voice_count(), 1);

        // Press sustain pedal (CC64 >= 64 = on)
        let sustain_on = ev_cc(0, cc::SUSTAIN, 127);
        queue_midi(&synth, &[sustain_on]);
        synth.tick(&[], &mut output);

        // Release note — voice should stay active (sustained)
        let note_off = ev_note_off(0, 60);
        queue_midi(&synth, &[note_off]);
        synth.tick(&[], &mut output);

        // Voice is still active due to sustain pedal
        assert!(
            synth.voices[0].gate_value() > 0.0 || synth.voices[0].is_active(),
            "Voice should still be held by sustain pedal"
        );

        // Release sustain pedal (CC64 < 64 = off)
        let sustain_off = ev_cc(0, cc::SUSTAIN, 0);
        queue_midi(&synth, &[sustain_off]);
        synth.tick(&[], &mut output);

        // Voice should now be releasing (gate off)
        let voice = &synth.voices[0];
        assert!(
            voice.gate_value() == 0.0,
            "Voice should release after sustain pedal off"
        );
    }

    #[test]
    fn test_cc66_sostenuto_pedal_holds_notes() {
        let mut synth = synth(SynthConfig {
            sample_rate: tutti_core::SampleRate::SR_44K1,
            max_voices: 4,
            voice_mode: VoiceMode::Poly,
            oscillator: OscillatorType::Sine,
            envelope: EnvelopeConfig {
                attack: Seconds(0.001),
                decay: Seconds(0.0),
                sustain: Amplitude(1.0),
                release: Seconds(0.05),
            },
            ..Default::default()
        });

        // Play note, then press sostenuto
        let note_on = ev_note_on(0, 60, 100);
        queue_midi(&synth, &[note_on]);
        let mut output = [0.0f32; 2];
        synth.tick(&[], &mut output);

        let sostenuto_on = ev_cc(0, cc::SOSTENUTO, 127);
        queue_midi(&synth, &[sostenuto_on]);
        synth.tick(&[], &mut output);

        // Release note — should be held by sostenuto
        let note_off = ev_note_off(0, 60);
        queue_midi(&synth, &[note_off]);
        synth.tick(&[], &mut output);

        assert!(
            synth.voices[0].is_active(),
            "Voice should be held by sostenuto"
        );

        // Play a NEW note AFTER sostenuto — this should NOT be held
        let note_on2 = ev_note_on(0, 64, 100);
        queue_midi(&synth, &[note_on2]);
        synth.tick(&[], &mut output);
        let note_off2 = ev_note_off(0, 64);
        queue_midi(&synth, &[note_off2]);
        synth.tick(&[], &mut output);

        // Second note's voice should be releasing (gate off)
        let voice2 = &synth.voices[1];
        assert_eq!(
            voice2.gate_value(),
            0.0,
            "New note after sostenuto should release normally"
        );

        // Release sostenuto — original note should now release
        let sostenuto_off = ev_cc(0, cc::SOSTENUTO, 0);
        queue_midi(&synth, &[sostenuto_off]);
        synth.tick(&[], &mut output);

        assert_eq!(
            synth.voices[0].gate_value(),
            0.0,
            "Original note should release after sostenuto off"
        );
    }

    #[test]
    fn test_cc120_all_sound_off() {
        let mut synth = synth(SynthConfig {
            sample_rate: tutti_core::SampleRate::SR_44K1,
            max_voices: 4,
            voice_mode: VoiceMode::Poly,
            oscillator: OscillatorType::Sine,
            envelope: EnvelopeConfig {
                attack: Seconds(0.001),
                decay: Seconds(0.0),
                sustain: Amplitude(1.0),
                release: Seconds(5.0),
            }, // Very long release
            ..Default::default()
        });

        // Play multiple notes
        let events: Vec<MidiEvent> = (60..64).map(|n| ev_note_on(0, n, 100)).collect();
        queue_midi(&synth, &events);
        let mut output = [0.0f32; 2];
        synth.tick(&[], &mut output);
        assert_eq!(synth.active_voice_count(), 4);

        // CC120 = All Sound Off (immediate silence)
        let all_sound_off = ev_cc(0, cc::ALL_SOUND_OFF, 0);
        queue_midi(&synth, &[all_sound_off]);
        synth.tick(&[], &mut output);

        // All voices should be immediately reset (not just releasing)
        assert_eq!(
            synth.active_voice_count(),
            0,
            "All Sound Off should immediately silence all voices"
        );
    }

    #[test]
    fn test_cc123_all_notes_off() {
        let mut synth = synth(SynthConfig {
            sample_rate: tutti_core::SampleRate::SR_44K1,
            max_voices: 4,
            voice_mode: VoiceMode::Poly,
            oscillator: OscillatorType::Sine,
            envelope: EnvelopeConfig {
                attack: Seconds(0.001),
                decay: Seconds(0.0),
                sustain: Amplitude(1.0),
                release: Seconds(5.0),
            }, // Long release
            ..Default::default()
        });

        // Play multiple notes
        let events: Vec<MidiEvent> = (60..64).map(|n| ev_note_on(0, n, 100)).collect();
        queue_midi(&synth, &events);
        let mut output = [0.0f32; 2];
        synth.tick(&[], &mut output);
        assert_eq!(synth.active_voice_count(), 4);

        // CC123 = All Notes Off (release with envelope tail)
        let all_notes_off = ev_cc(0, cc::ALL_NOTES_OFF, 0);
        queue_midi(&synth, &[all_notes_off]);
        synth.tick(&[], &mut output);

        // Voices should still be active (releasing with long tail)
        // but gates should be off
        for voice in &synth.voices {
            if voice.is_active() {
                assert_eq!(
                    voice.gate_value(),
                    0.0,
                    "All Notes Off should release (gate off), not silence"
                );
            }
        }
    }

    #[test]
    fn test_cc123_respects_channel() {
        let mut synth = synth(SynthConfig {
            sample_rate: tutti_core::SampleRate::SR_44K1,
            max_voices: 4,
            voice_mode: VoiceMode::Poly,
            oscillator: OscillatorType::Sine,
            envelope: EnvelopeConfig {
                attack: Seconds(0.001),
                decay: Seconds(0.0),
                sustain: Amplitude(1.0),
                release: Seconds(5.0),
            },
            ..Default::default()
        });

        // Play notes on channel 0 and channel 1
        let note_ch0 = ev_note_on(0, 60, 100);
        let note_ch1 = ev_note_on(1, 64, 100);
        queue_midi(&synth, &[note_ch0, note_ch1]);
        let mut output = [0.0f32; 2];
        synth.tick(&[], &mut output);
        assert_eq!(synth.active_voice_count(), 2);

        // All Notes Off on channel 0 only
        let all_notes_off = ev_cc(0, cc::ALL_NOTES_OFF, 0);
        queue_midi(&synth, &[all_notes_off]);
        synth.tick(&[], &mut output);

        // Channel 1 voice should still have gate open
        let ch1_voice = synth
            .voices
            .iter()
            .find(|v| v.is_active() && v.channel() == 1);
        assert!(
            ch1_voice.is_some(),
            "Channel 1 voice should still be active"
        );
        assert!(
            ch1_voice.unwrap().gate_value() > 0.0,
            "Channel 1 voice gate should still be open"
        );
    }

    #[test]
    fn test_velocity_zero_note_on_is_note_off() {
        let mut synth = synth(SynthConfig {
            sample_rate: tutti_core::SampleRate::SR_44K1,
            max_voices: 4,
            voice_mode: VoiceMode::Poly,
            oscillator: OscillatorType::Sine,
            envelope: EnvelopeConfig {
                attack: Seconds(0.001),
                decay: Seconds(0.0),
                sustain: Amplitude(1.0),
                release: Seconds(0.05),
            },
            ..Default::default()
        });

        // Play note
        let note_on = ev_note_on(0, 60, 100);
        queue_midi(&synth, &[note_on]);
        let mut output = [0.0f32; 2];
        synth.tick(&[], &mut output);
        assert_eq!(synth.active_voice_count(), 1);
        assert!(synth.voices[0].gate_value() > 0.0);

        // Note on with velocity 0 = note off (MIDI standard)
        let vel0_off = ev_note_on(0, 60, 0);
        queue_midi(&synth, &[vel0_off]);
        synth.tick(&[], &mut output);

        assert_eq!(
            synth.voices[0].gate_value(),
            0.0,
            "Note-on with velocity 0 should act as note-off"
        );
    }

    #[test]
    fn test_legato_mode_no_retrigger() {
        let mut synth = synth(SynthConfig {
            sample_rate: tutti_core::SampleRate::SR_44K1,
            max_voices: 1,
            voice_mode: VoiceMode::Legato,
            oscillator: OscillatorType::Sine,
            envelope: EnvelopeConfig {
                attack: Seconds(0.001),
                decay: Seconds(0.0),
                sustain: Amplitude(1.0),
                release: Seconds(0.1),
            },
            ..Default::default()
        });

        assert_eq!(synth.config.voice_mode, VoiceMode::Legato);

        // First note triggers normally
        let note1 = ev_note_on(0, 60, 100);
        queue_midi(&synth, &[note1]);
        let mut output = [0.0f32; 2];
        synth.tick(&[], &mut output);
        assert_eq!(synth.active_voice_count(), 1);
        assert_eq!(synth.voices[0].note(), 60);

        // Second note should legato (update pitch, no retrigger)
        let note2 = ev_note_on(0, 64, 100);
        queue_midi(&synth, &[note2]);
        synth.tick(&[], &mut output);

        assert_eq!(
            synth.active_voice_count(),
            1,
            "Legato should use same voice"
        );
        assert_eq!(synth.voices[0].note(), 64, "Voice should have new note");
        assert!(
            synth.voices[0].gate_value() > 0.0,
            "Gate should stay open (no retrigger)"
        );
    }

    #[test]
    fn test_mono_mode_retrigger() {
        let mut synth = synth(SynthConfig {
            sample_rate: tutti_core::SampleRate::SR_44K1,
            max_voices: 1,
            voice_mode: VoiceMode::Mono,
            oscillator: OscillatorType::Sine,
            envelope: EnvelopeConfig {
                attack: Seconds(0.001),
                decay: Seconds(0.0),
                sustain: Amplitude(1.0),
                release: Seconds(0.1),
            },
            ..Default::default()
        });

        assert_eq!(synth.config.voice_mode, VoiceMode::Mono);

        // First note
        let note1 = ev_note_on(0, 60, 100);
        queue_midi(&synth, &[note1]);
        let mut output = [0.0f32; 2];
        synth.tick(&[], &mut output);
        assert_eq!(synth.voices[0].note(), 60);

        // Second note should retrigger (new allocation, not legato)
        let note2 = ev_note_on(0, 64, 100);
        queue_midi(&synth, &[note2]);
        synth.tick(&[], &mut output);
        assert_eq!(synth.voices[0].note(), 64);
    }

    /// One wheel position means one pitch, whether the note was already
    /// sounding or is struck after the bend.
    ///
    /// It did not. `handle_note_on` multiplies the base frequency by
    /// `2^(semitones/12)`, while `apply_pitch_bend` fed `note + semitones` into
    /// `Tuning::fractional_note_to_freq`, which interpolates between *table*
    /// entries. Under equal temperament the table is geometric and the two
    /// agree exactly, which is why every existing test passed. Under any
    /// unequal scale they do not: measured at **3.9 cents** on
    /// `Tuning::just_intonation` — a held note and a new note, same wheel,
    /// audibly different pitch.
    ///
    /// Pinned in cents rather than Hz because the defect is a pitch error, and
    /// because a ratio comparison stays meaningful if the fixture's reference
    /// frequency ever changes.
    #[test]
    fn a_bend_lands_on_the_same_pitch_whether_the_note_was_already_sounding() {
        let config = || SynthConfig {
            sample_rate: tutti_core::SampleRate::SR_44K1,
            max_voices: 2,
            voice_mode: VoiceMode::Poly,
            oscillator: OscillatorType::Sine,
            // An unequal table is the whole point: on equal temperament the two
            // derivations coincide and this test cannot fail.
            tuning: crate::Tuning::just_intonation(),
            envelope: EnvelopeConfig {
                attack: Seconds(0.001),
                decay: Seconds(0.0),
                sustain: Amplitude(1.0),
                release: Seconds(0.1),
            },
            ..Default::default()
        };
        // Full up-bend: +1 semitone at the default 2-semitone range.
        let bend_up = ev_bend(0, 16383);

        // Held: sound the note, then bend it.
        let mut held = synth(config());
        let mut out = [0.0f32; 2];
        queue_midi(&held, &[ev_note_on(0, 60, 100)]);
        held.tick(&[], &mut out);
        queue_midi(&held, &[bend_up]);
        held.tick(&[], &mut out);
        let held_freq = held.voices[0].sounding_freq();

        // Struck: bend first, then sound the note.
        let mut struck = synth(config());
        queue_midi(&struck, &[bend_up]);
        struck.tick(&[], &mut out);
        queue_midi(&struck, &[ev_note_on(0, 60, 100)]);
        struck.tick(&[], &mut out);
        let struck_freq = struck.voices[0].sounding_freq();

        let delta = tutti_core::Cents::from_pitch_ratio(held_freq.get() / struck_freq.get());
        assert!(
            delta.get().abs() < 0.5,
            "same wheel position, two pitches: held {held_freq:?} vs struck {struck_freq:?} \
             ({delta:?} apart)"
        );
    }

    #[test]
    fn test_portamento_with_pitch_bend() {
        let mut synth = synth(SynthConfig {
            sample_rate: tutti_core::SampleRate::SR_44K1,
            max_voices: 2,
            voice_mode: VoiceMode::Poly,
            oscillator: OscillatorType::Sine,
            envelope: EnvelopeConfig {
                attack: Seconds(0.001),
                decay: Seconds(0.0),
                sustain: Amplitude(1.0),
                release: Seconds(0.1),
            },
            portamento: Some(PortamentoConfig {
                mode: PortamentoMode::Always,
                curve: PortamentoCurve::Linear,
                time: tutti_core::Seconds(0.05), // 50ms glide
                constant_time: true,
            }),
            ..Default::default()
        });

        // Play first note to initialize portamento
        let note1 = ev_note_on(0, 60, 100);
        queue_midi(&synth, &[note1]);
        let mut output = [0.0f32; 2];
        for _ in 0..4410 {
            synth.tick(&[], &mut output);
        }

        // Play second note — triggers portamento glide
        let note2 = ev_note_on(0, 72, 100);
        queue_midi(&synth, &[note2]);
        synth.tick(&[], &mut output);

        // Apply pitch bend while portamento is gliding
        let bend_up = ev_bend(0, 16383);
        queue_midi(&synth, &[bend_up]);

        // Process samples during glide — should not crash or produce silence
        let mut max_out = 0.0f32;
        for _ in 0..2205 {
            synth.tick(&[], &mut output);
            max_out = max_out.max(output[0].abs().max(output[1].abs()));
        }

        assert!(
            max_out > 0.0,
            "Should produce audio during portamento+bend, got max={}",
            max_out
        );
    }

    #[test]
    fn test_zero_voices_returns_error() {
        let result = PolySynth::new(SynthConfig {
            sample_rate: tutti_core::SampleRate::SR_44K1,
            max_voices: 0,
            ..Default::default()
        });
        assert!(result.is_err(), "max_voices=0 should return error");
    }

    #[test]
    fn test_polysynth_reset() {
        let mut synth = synth(SynthConfig {
            sample_rate: tutti_core::SampleRate::SR_44K1,
            max_voices: 4,
            voice_mode: VoiceMode::Poly,
            oscillator: OscillatorType::Sine,
            envelope: EnvelopeConfig {
                attack: Seconds(0.001),
                decay: Seconds(0.0),
                sustain: Amplitude(1.0),
                release: Seconds(5.0),
            },
            ..Default::default()
        });

        // Play notes
        let events: Vec<MidiEvent> = (60..64).map(|n| ev_note_on(0, n, 100)).collect();
        queue_midi(&synth, &events);
        let mut output = [0.0f32; 2];
        synth.tick(&[], &mut output);
        assert_eq!(synth.active_voice_count(), 4);

        // Reset
        synth.reset();
        assert_eq!(
            synth.active_voice_count(),
            0,
            "Reset should clear all voices"
        );
    }

    #[test]
    fn test_mpe_per_voice_pitch_bend() {
        let mut synth = synth(SynthConfig {
            sample_rate: tutti_core::SampleRate::SR_44K1,
            max_voices: 4,
            voice_mode: VoiceMode::Poly,
            oscillator: OscillatorType::Sine,
            envelope: EnvelopeConfig {
                attack: Seconds(0.001),
                decay: Seconds(0.0),
                sustain: Amplitude(1.0),
                release: Seconds(0.1),
            },
            mpe_enabled: true,
            mpe_pitch_bend_range: tutti_core::Semitones(48.0),
            ..Default::default()
        });

        // Play note on channel 1 (MPE member channel)
        let note_on = ev_note_on(1, 60, 100);
        queue_midi(&synth, &[note_on]);
        let mut output = [0.0f32; 2];
        synth.tick(&[], &mut output);
        assert_eq!(synth.active_voice_count(), 1);

        // Play note on channel 2
        let note_on2 = ev_note_on(2, 64, 100);
        queue_midi(&synth, &[note_on2]);
        synth.tick(&[], &mut output);
        assert_eq!(synth.active_voice_count(), 2);

        // A native per-note pitch bend addressed to the ch1 note (60) must affect
        // only that voice. Classic-MPE channel bends are rewritten to this native
        // form at the input edge (`MpeIngest`); the synth is zone-agnostic and only
        // ever sees native per-note messages.
        let bend =
            MidiEvent::per_note_pitch_bend(MidiGroup::FIRST, MidiChannel::new(1), 60, 0xFFFF_FFFF);
        queue_midi(&synth, &[bend]);
        synth.tick(&[], &mut output);

        let voice_ch1 = synth
            .voices
            .iter()
            .find(|v| v.is_active() && v.channel() == 1)
            .unwrap();
        let voice_ch2 = synth
            .voices
            .iter()
            .find(|v| v.is_active() && v.channel() == 2)
            .unwrap();

        assert!(
            voice_ch1.mpe_state().pitch_bend_semitones.get() > 40.0,
            "the addressed note should have a large pitch bend, got {}",
            voice_ch1.mpe_state().pitch_bend_semitones
        );
        assert!(
            voice_ch2.mpe_state().pitch_bend_semitones.get().abs() < 0.01,
            "the other note should be untouched, got {}",
            voice_ch2.mpe_state().pitch_bend_semitones
        );
    }

    #[test]
    fn test_midi2_per_note_pitch_bend_addresses_one_voice() {
        // MIDI 2.0 native per-note pitch bend carries the note number on the
        // wire, so it must move ONLY the addressed voice — even when both notes
        // sound on the same channel (where the classic per-channel MPE handler
        // would have bent both). This is the per-note-addressing proof.
        let mut synth = synth(SynthConfig {
            sample_rate: tutti_core::SampleRate::SR_44K1,
            max_voices: 4,
            voice_mode: VoiceMode::Poly,
            oscillator: OscillatorType::Sine,
            envelope: EnvelopeConfig {
                attack: Seconds(0.001),
                decay: Seconds(0.0),
                sustain: Amplitude(1.0),
                release: Seconds(0.1),
            },
            mpe_enabled: true,
            mpe_pitch_bend_range: tutti_core::Semitones(48.0),
            ..Default::default()
        });

        // Two notes, same channel, different pitches.
        queue_midi(&synth, &[ev_note_on(1, 60, 100), ev_note_on(1, 64, 100)]);
        let mut output = [0.0f32; 2];
        synth.tick(&[], &mut output);
        assert_eq!(synth.active_voice_count(), 2);

        // Native per-note pitch bend addressed to note 60 only (full positive).
        let bend =
            MidiEvent::per_note_pitch_bend(MidiGroup::FIRST, MidiChannel::new(1), 60, 0xFFFF_FFFF);
        queue_midi(&synth, &[bend]);
        synth.tick(&[], &mut output);

        let voice_60 = synth
            .voices
            .iter()
            .find(|v| v.is_active() && v.note() == 60)
            .unwrap();
        let voice_64 = synth
            .voices
            .iter()
            .find(|v| v.is_active() && v.note() == 64)
            .unwrap();

        assert!(
            voice_60.mpe_state().pitch_bend_semitones.get() > 40.0,
            "note 60 should be bent, got {}",
            voice_60.mpe_state().pitch_bend_semitones
        );
        assert!(
            voice_64.mpe_state().pitch_bend_semitones.get().abs() < 0.01,
            "note 64 (same channel) must be untouched, got {}",
            voice_64.mpe_state().pitch_bend_semitones
        );
    }

    #[test]
    fn test_midi2_per_note_management_reset_zeroes_only_addressed_voice() {
        // MIDI 2.0 Per-Note Management {reset:true} must reset ONLY the addressed
        // note's per-note expression, leaving another sounding voice's bend intact.
        let mut synth = synth(SynthConfig {
            sample_rate: tutti_core::SampleRate::SR_44K1,
            max_voices: 4,
            voice_mode: VoiceMode::Poly,
            oscillator: OscillatorType::Sine,
            envelope: EnvelopeConfig {
                attack: Seconds(0.001),
                decay: Seconds(0.0),
                sustain: Amplitude(1.0),
                release: Seconds(0.1),
            },
            mpe_enabled: true,
            mpe_pitch_bend_range: tutti_core::Semitones(48.0),
            ..Default::default()
        });

        // Two notes on the same channel.
        queue_midi(&synth, &[ev_note_on(1, 60, 100), ev_note_on(1, 64, 100)]);
        let mut output = [0.0f32; 2];
        synth.tick(&[], &mut output);
        assert_eq!(synth.active_voice_count(), 2);

        // Bend BOTH notes fully (per-note, so each is addressed independently).
        queue_midi(
            &synth,
            &[
                MidiEvent::per_note_pitch_bend(
                    MidiGroup::FIRST,
                    MidiChannel::new(1),
                    60,
                    0xFFFF_FFFF,
                ),
                MidiEvent::per_note_pitch_bend(
                    MidiGroup::FIRST,
                    MidiChannel::new(1),
                    64,
                    0xFFFF_FFFF,
                ),
            ],
        );
        synth.tick(&[], &mut output);

        let bent = |synth: &PolySynth, note: u8| {
            synth
                .voices
                .iter()
                .find(|v| v.is_active() && v.note() == note)
                .unwrap()
                .mpe_state()
                .pitch_bend_semitones
                .get()
        };
        assert!(bent(&synth, 60) > 40.0, "note 60 should start bent");
        assert!(bent(&synth, 64) > 40.0, "note 64 should start bent");

        // Per-Note Management Reset addressed to note 60 only.
        queue_midi(
            &synth,
            &[MidiEvent::per_note_management(
                MidiGroup::FIRST,
                MidiChannel::new(1),
                60,
                false,
                true,
            )],
        );
        synth.tick(&[], &mut output);

        assert!(
            bent(&synth, 60).abs() < 0.01,
            "note 60 per-note expression must be reset to 0, got {}",
            bent(&synth, 60)
        );
        assert!(
            bent(&synth, 64) > 40.0,
            "note 64 (unaddressed) must keep its bend, got {}",
            bent(&synth, 64)
        );
    }

    #[test]
    fn test_pitch_7_25_retunes_only_the_addressed_note() {
        // Registered Per-Note Controller #3: Pitch 7.25 (M2-104 §7.4.15.2) sets an
        // absolute pitch for the addressed note. Note 60 retuned to note 69.0
        // (A440) should sound at 440 Hz; another note is untouched.
        let mut synth = synth(SynthConfig {
            sample_rate: tutti_core::SampleRate::SR_44K1,
            max_voices: 4,
            voice_mode: VoiceMode::Poly,
            oscillator: OscillatorType::Sine,
            envelope: EnvelopeConfig {
                attack: Seconds(0.001),
                decay: Seconds(0.0),
                sustain: Amplitude(1.0),
                release: Seconds(0.1),
            },
            mpe_enabled: true,
            ..Default::default()
        });

        queue_midi(&synth, &[ev_note_on(1, 60, 100), ev_note_on(1, 64, 100)]);
        let mut output = [0.0f32; 2];
        synth.tick(&[], &mut output);

        // Pitch 7.25 for note 60 = 69.0 (A440). Q7.25: 69 << 25. Registered
        // per-note controller index 3 (Pitch 7.25).
        let pitch = MidiEvent::per_note_controller(
            MidiGroup::FIRST,
            MidiChannel::new(1),
            60,
            3,
            69u32 << 25,
            true,
        );
        queue_midi(&synth, &[pitch]);
        synth.tick(&[], &mut output);

        let freq = |synth: &PolySynth, note: u8| {
            synth
                .voices
                .iter()
                .find(|v| v.is_active() && v.note() == note)
                .unwrap()
                .base_note_freq()
                .get()
        };
        assert!(
            (freq(&synth, 60) - 440.0).abs() < 1.0,
            "note 60 retuned to A440, got {}",
            freq(&synth, 60)
        );
        // Note 64's default pitch (~329.6 Hz) must be untouched.
        assert!(
            (freq(&synth, 64) - 329.6).abs() < 2.0,
            "note 64 must keep its default pitch, got {}",
            freq(&synth, 64)
        );
    }

    #[test]
    fn test_per_note_management_detach_freezes_the_note() {
        // Detach (D=1): the addressed voice keeps its current per-note bend but
        // stops responding to further per-note controllers (M2-104 §7.4.5).
        let mut synth = synth(SynthConfig {
            sample_rate: tutti_core::SampleRate::SR_44K1,
            max_voices: 4,
            voice_mode: VoiceMode::Poly,
            oscillator: OscillatorType::Sine,
            envelope: EnvelopeConfig {
                attack: Seconds(0.001),
                decay: Seconds(0.0),
                sustain: Amplitude(1.0),
                release: Seconds(0.1),
            },
            mpe_enabled: true,
            mpe_pitch_bend_range: tutti_core::Semitones(48.0),
            ..Default::default()
        });

        queue_midi(&synth, &[ev_note_on(1, 60, 100)]);
        let mut output = [0.0f32; 2];
        synth.tick(&[], &mut output);

        // Bend fully, then Detach.
        queue_midi(
            &synth,
            &[MidiEvent::per_note_pitch_bend(
                MidiGroup::FIRST,
                MidiChannel::new(1),
                60,
                0xFFFF_FFFF,
            )],
        );
        synth.tick(&[], &mut output);
        let bent = |synth: &PolySynth| {
            synth
                .voices
                .iter()
                .find(|v| v.is_active() && v.note() == 60)
                .unwrap()
                .mpe_state()
                .pitch_bend_semitones
                .get()
        };
        let frozen_at = bent(&synth);
        assert!(frozen_at > 40.0, "note should be bent before detach");

        // Detach (D=1, S=0).
        queue_midi(
            &synth,
            &[MidiEvent::per_note_management(
                MidiGroup::FIRST,
                MidiChannel::new(1),
                60,
                true,
                false,
            )],
        );
        synth.tick(&[], &mut output);

        // A further per-note bend to zero must be IGNORED — the note holds its value.
        queue_midi(
            &synth,
            &[MidiEvent::per_note_pitch_bend(
                MidiGroup::FIRST,
                MidiChannel::new(1),
                60,
                0x8000_0000,
            )],
        );
        synth.tick(&[], &mut output);
        assert!(
            (bent(&synth) - frozen_at).abs() < 0.01,
            "detached note must ignore further per-note bend (held {frozen_at}, got {})",
            bent(&synth)
        );
    }

    #[test]
    fn test_per_note_pitch_bend_sensitivity_rpn_updates_range() {
        // RPN #00/07 sets the per-note pitch-bend range (M2-104 §7.4.13). After a
        // 12-semitone sensitivity, a full per-note bend should reach ~12 semitones
        // (not the default 48).
        let mut synth = synth(SynthConfig {
            sample_rate: tutti_core::SampleRate::SR_44K1,
            max_voices: 4,
            voice_mode: VoiceMode::Poly,
            oscillator: OscillatorType::Sine,
            envelope: EnvelopeConfig {
                attack: Seconds(0.001),
                decay: Seconds(0.0),
                sustain: Amplitude(1.0),
                release: Seconds(0.1),
            },
            mpe_enabled: true,
            mpe_pitch_bend_range: tutti_core::Semitones(48.0),
            ..Default::default()
        });

        // Sensitivity RPN: 12 semitones (7.25 fixed-point).
        let sens = tutti_midi_types::mpe::PitchBendSensitivity::from_semitones(12);
        let rpn = MidiEvent::registered_controller(
            MidiGroup::FIRST,
            MidiChannel::new(1),
            tutti_midi_types::ump::RPN_BANK_MPE,
            tutti_midi_types::ump::RPN_INDEX_PER_NOTE_PITCH_BEND_SENSITIVITY,
            sens.to_rpn_bits(),
        );
        queue_midi(&synth, &[ev_note_on(1, 60, 100), rpn]);
        let mut output = [0.0f32; 2];
        synth.tick(&[], &mut output);

        // Full per-note bend up — now clamped to the 12-semitone range.
        queue_midi(
            &synth,
            &[MidiEvent::per_note_pitch_bend(
                MidiGroup::FIRST,
                MidiChannel::new(1),
                60,
                0xFFFF_FFFF,
            )],
        );
        synth.tick(&[], &mut output);
        let bend = synth
            .voices
            .iter()
            .find(|v| v.is_active() && v.note() == 60)
            .unwrap()
            .mpe_state()
            .pitch_bend_semitones
            .get();
        assert!(
            (bend - 12.0).abs() < 0.5,
            "per-note bend should clamp to the 12-semitone sensitivity, got {bend}"
        );
    }

    #[test]
    fn test_reset_all_controllers_spares_per_note() {
        // Reset All Controllers (CC121) resets channel controllers + global pitch
        // bend but must NOT touch per-note controllers (M2-104 Appendix B.2).
        let mut synth = synth(SynthConfig {
            sample_rate: tutti_core::SampleRate::SR_44K1,
            max_voices: 4,
            voice_mode: VoiceMode::Poly,
            oscillator: OscillatorType::Sine,
            envelope: EnvelopeConfig {
                attack: Seconds(0.001),
                decay: Seconds(0.0),
                sustain: Amplitude(1.0),
                release: Seconds(0.1),
            },
            mpe_enabled: true,
            mpe_pitch_bend_range: tutti_core::Semitones(48.0),
            ..Default::default()
        });

        queue_midi(&synth, &[ev_note_on(1, 60, 100)]);
        let mut output = [0.0f32; 2];
        synth.tick(&[], &mut output);
        // Set a per-note bend.
        queue_midi(
            &synth,
            &[MidiEvent::per_note_pitch_bend(
                MidiGroup::FIRST,
                MidiChannel::new(1),
                60,
                0xFFFF_FFFF,
            )],
        );
        synth.tick(&[], &mut output);

        // Reset All Controllers on channel 1.
        queue_midi(&synth, &[ev_cc(1, cc::RESET_ALL, 0)]);
        synth.tick(&[], &mut output);

        let bend = synth
            .voices
            .iter()
            .find(|v| v.is_active() && v.note() == 60)
            .unwrap()
            .mpe_state()
            .pitch_bend_semitones
            .get();
        assert!(
            bend > 40.0,
            "Reset All Controllers must NOT clear per-note bend, got {bend}"
        );
    }

    #[test]
    fn test_midi2_registered_per_note_controller_brightness_sets_only_addressed_slide() {
        // A Registered Per-Note Controller for Brightness (CC74 / SoundController
        // index 5) addressed to note X must set ONLY note X's slide.
        let mut synth = synth(SynthConfig {
            sample_rate: tutti_core::SampleRate::SR_44K1,
            max_voices: 4,
            voice_mode: VoiceMode::Poly,
            oscillator: OscillatorType::Saw,
            filter: FilterType::Moog {
                cutoff: Hz(1000.0),
                resonance: Resonance(0.5),
            },
            envelope: EnvelopeConfig {
                attack: Seconds(0.001),
                decay: Seconds(0.0),
                sustain: Amplitude(1.0),
                release: Seconds(0.1),
            },
            mpe_enabled: true,
            ..Default::default()
        });

        // Two notes on the same channel.
        queue_midi(&synth, &[ev_note_on(1, 60, 100), ev_note_on(1, 64, 100)]);
        let mut output = [0.0f32; 2];
        synth.tick(&[], &mut output);
        assert_eq!(synth.active_voice_count(), 2);

        // Registered per-note Brightness (index 74) addressed to note 60, full value.
        queue_midi(
            &synth,
            &[MidiEvent::per_note_controller(
                MidiGroup::FIRST,
                MidiChannel::new(1),
                60,
                74,
                0xFFFF_FFFF,
                true,
            )],
        );
        synth.tick(&[], &mut output);

        let slide = |synth: &PolySynth, note: u8| {
            synth
                .voices
                .iter()
                .find(|v| v.is_active() && v.note() == note)
                .unwrap()
                .mpe_state()
                .slide
        };
        assert!(
            (slide(&synth, 60) - 1.0).abs() < 0.01,
            "note 60 slide should be full, got {}",
            slide(&synth, 60)
        );
        // The unaddressed voice must keep its neutral default (center = no timbre
        // shift), not be dragged along with note 60.
        assert!(
            (slide(&synth, 64) - crate::voice::SLIDE_CENTER).abs() < 0.01,
            "note 64 (unaddressed) slide must stay at center, got {}",
            slide(&synth, 64)
        );
    }

    #[test]
    fn test_midi2_per_note_gain_addresses_only_one_voice() {
        // Per-note Volume (CC7) — as clip Gain lanes emit it (Assignable index 7)
        // and as a Registered Volume controller — must reach the addressed voice
        // as gain, and leave others at unity.
        let mut synth = synth(SynthConfig {
            sample_rate: tutti_core::SampleRate::SR_44K1,
            max_voices: 4,
            voice_mode: VoiceMode::Poly,
            oscillator: OscillatorType::Sine,
            envelope: EnvelopeConfig {
                attack: Seconds(0.001),
                decay: Seconds(0.0),
                sustain: Amplitude(1.0),
                release: Seconds(0.1),
            },
            mpe_enabled: true,
            ..Default::default()
        });

        queue_midi(&synth, &[ev_note_on(1, 60, 100), ev_note_on(1, 64, 100)]);
        let mut output = [0.0f32; 2];
        synth.tick(&[], &mut output);

        let gain = |synth: &PolySynth, note: u8| {
            synth
                .voices
                .iter()
                .find(|v| v.is_active() && v.note() == note)
                .unwrap()
                .mpe_state()
                .gain
                .get()
        };
        // Fresh voices are at unity gain.
        assert!((gain(&synth, 60) - 1.0).abs() < 0.01);
        assert!((gain(&synth, 64) - 1.0).abs() < 0.01);

        // Assignable per-note CC7 (the clip Gain-lane encoding) → half gain on 60.
        queue_midi(
            &synth,
            &[MidiEvent::per_note_controller(
                MidiGroup::FIRST,
                MidiChannel::new(1),
                60,
                7,
                0x8000_0000,
                false,
            )],
        );
        synth.tick(&[], &mut output);
        assert!(
            (gain(&synth, 60) - 0.5).abs() < 0.02,
            "note 60 gain should follow CC7, got {}",
            gain(&synth, 60)
        );
        assert!(
            (gain(&synth, 64) - 1.0).abs() < 0.01,
            "note 64 (unaddressed) gain must stay unity, got {}",
            gain(&synth, 64)
        );
    }

    #[test]
    fn test_mpe_per_voice_pressure() {
        let mut synth = synth(SynthConfig {
            sample_rate: tutti_core::SampleRate::SR_44K1,
            max_voices: 4,
            voice_mode: VoiceMode::Poly,
            oscillator: OscillatorType::Sine,
            envelope: EnvelopeConfig {
                attack: Seconds(0.001),
                decay: Seconds(0.0),
                sustain: Amplitude(1.0),
                release: Seconds(0.1),
            },
            mpe_enabled: true,
            ..Default::default()
        });

        // Play notes on two different channels
        let note1 = ev_note_on(1, 60, 100);
        let note2 = ev_note_on(2, 64, 100);
        queue_midi(&synth, &[note1, note2]);
        let mut output = [0.0f32; 2];
        synth.tick(&[], &mut output);

        // Native per-note pressure addressed to the ch1 note (60) only. Classic-MPE
        // channel pressure is rewritten to this form at the input edge.
        let pressure =
            MidiEvent::poly_pressure(MidiGroup::FIRST, MidiChannel::new(1), 60, 0xFFFF_FFFF);
        queue_midi(&synth, &[pressure]);
        synth.tick(&[], &mut output);

        let voice_ch1 = synth
            .voices
            .iter()
            .find(|v| v.is_active() && v.channel() == 1)
            .unwrap();
        let voice_ch2 = synth
            .voices
            .iter()
            .find(|v| v.is_active() && v.channel() == 2)
            .unwrap();

        assert!(
            (voice_ch1.mpe_state().pressure - 1.0).abs() < 0.01,
            "the addressed note should have full pressure"
        );
        assert!(
            voice_ch2.mpe_state().pressure.abs() < 0.01,
            "the other note should have no pressure"
        );
    }

    #[test]
    fn test_mpe_per_voice_slide() {
        let mut synth = synth(SynthConfig {
            sample_rate: tutti_core::SampleRate::SR_44K1,
            max_voices: 4,
            voice_mode: VoiceMode::Poly,
            oscillator: OscillatorType::Saw,
            filter: FilterType::Moog {
                cutoff: Hz(1000.0),
                resonance: Resonance(0.5),
            },
            envelope: EnvelopeConfig {
                attack: Seconds(0.001),
                decay: Seconds(0.0),
                sustain: Amplitude(1.0),
                release: Seconds(0.1),
            },
            mpe_enabled: true,
            ..Default::default()
        });

        // Play note on channel 1
        let note1 = ev_note_on(1, 60, 100);
        queue_midi(&synth, &[note1]);
        let mut output = [0.0f32; 2];
        synth.tick(&[], &mut output);

        // Native per-note CC74 (slide) addressed to the ch1 note (60). Classic-MPE
        // channel CC74 is rewritten to this form at the input edge.
        let slide = MidiEvent::per_note_controller(
            MidiGroup::FIRST,
            MidiChannel::new(1),
            60,
            74,
            0xFFFF_FFFF,
            false,
        );
        queue_midi(&synth, &[slide]);
        synth.tick(&[], &mut output);

        let voice = synth
            .voices
            .iter()
            .find(|v| v.is_active() && v.channel() == 1)
            .unwrap();
        assert!(
            (voice.mpe_state().slide - 1.0).abs() < 0.01,
            "the addressed note should have full slide"
        );
    }

    #[test]
    fn test_mpe_disabled_global_pitch_bend() {
        let mut synth = synth(SynthConfig {
            sample_rate: tutti_core::SampleRate::SR_44K1,
            max_voices: 4,
            voice_mode: VoiceMode::Poly,
            oscillator: OscillatorType::Sine,
            envelope: EnvelopeConfig {
                attack: Seconds(0.001),
                decay: Seconds(0.0),
                sustain: Amplitude(1.0),
                release: Seconds(0.1),
            },
            ..Default::default()
        });

        // MPE is disabled (default) - pitch bend should be global
        let note1 = ev_note_on(0, 60, 100);
        let note2 = ev_note_on(0, 64, 100);
        queue_midi(&synth, &[note1, note2]);
        let mut output = [0.0f32; 2];
        synth.tick(&[], &mut output);

        // Global pitch bend should NOT set MPE state
        let bend = ev_bend(0, 16383);
        queue_midi(&synth, &[bend]);
        synth.tick(&[], &mut output);

        // Both voices should have zero MPE pitch bend (global bend is handled differently)
        for voice in &synth.voices {
            if voice.is_active() {
                assert!(
                    voice.mpe_state().pitch_bend_semitones.get().abs() < 0.01,
                    "Non-MPE mode should not set MPE pitch bend"
                );
            }
        }
    }

    #[test]
    fn test_mpe_pressure_affects_amplitude() {
        let mut synth = synth(SynthConfig {
            sample_rate: tutti_core::SampleRate::SR_44K1,
            max_voices: 1,
            voice_mode: VoiceMode::Poly,
            oscillator: OscillatorType::Sine,
            envelope: EnvelopeConfig {
                attack: Seconds(0.001),
                decay: Seconds(0.0),
                sustain: Amplitude(1.0),
                release: Seconds(0.1),
            },
            mpe_enabled: true,
            ..Default::default()
        });

        // Play note
        let note = ev_note_on(1, 60, 100);
        queue_midi(&synth, &[note]);

        // Render baseline with no pressure
        let mut output = [0.0f32; 2];
        let mut max_no_pressure = 0.0f32;
        for _ in 0..2000 {
            synth.tick(&[], &mut output);
            max_no_pressure = max_no_pressure.max(output[0].abs().max(output[1].abs()));
        }

        // Apply full pressure as a native per-note pressure on the note (60).
        let pressure =
            MidiEvent::poly_pressure(MidiGroup::FIRST, MidiChannel::new(1), 60, 0xFFFF_FFFF);
        queue_midi(&synth, &[pressure]);

        let mut max_with_pressure = 0.0f32;
        for _ in 0..2000 {
            synth.tick(&[], &mut output);
            max_with_pressure = max_with_pressure.max(output[0].abs().max(output[1].abs()));
        }

        assert!(
            max_with_pressure > max_no_pressure * 1.1,
            "Pressure should increase amplitude: no_pressure={}, with_pressure={}",
            max_no_pressure,
            max_with_pressure
        );
    }

    #[test]
    fn test_mpe_note_on_resets_expression() {
        let mut synth = synth(SynthConfig {
            sample_rate: tutti_core::SampleRate::SR_44K1,
            max_voices: 4,
            voice_mode: VoiceMode::Poly,
            oscillator: OscillatorType::Sine,
            envelope: EnvelopeConfig {
                attack: Seconds(0.001),
                decay: Seconds(0.0),
                sustain: Amplitude(1.0),
                release: Seconds(0.05),
            },
            mpe_enabled: true,
            ..Default::default()
        });

        // Play note, set pressure, release, play again
        let note_on = ev_note_on(1, 60, 100);
        queue_midi(&synth, &[note_on]);
        let mut output = [0.0f32; 2];
        synth.tick(&[], &mut output);

        let pressure = ev_aftertouch(1, 127);
        queue_midi(&synth, &[pressure]);
        synth.tick(&[], &mut output);

        // Release and wait for voice to finish
        let note_off = ev_note_off(1, 60);
        queue_midi(&synth, &[note_off]);
        for _ in 0..10000 {
            synth.tick(&[], &mut output);
        }

        // Play again on channel 1
        let note_on2 = ev_note_on(1, 60, 100);
        queue_midi(&synth, &[note_on2]);
        synth.tick(&[], &mut output);

        let voice = synth
            .voices
            .iter()
            .find(|v| v.is_active() && v.channel() == 1)
            .unwrap();
        assert!(
            voice.mpe_state().pressure.abs() < 0.01,
            "New note should have reset MPE pressure"
        );
    }

    /// Whether MPE is on must be *askable*.
    ///
    /// It was construction-only with no getter, while the per-note setters accept
    /// unconditionally and `apply_mpe_modulation` drops everything at render
    /// time. Since the pitch-bend-sensitivity RPN *is* live either way, a host
    /// could send an MCM, watch sensitivity apply, and reasonably conclude MPE
    /// was on — with no way to find out otherwise.
    #[test]
    fn mpe_enabled_is_observable() {
        let off = PolySynth::new(SynthConfig {
            mpe_enabled: false,
            ..Default::default()
        })
        .unwrap();
        assert!(!off.mpe_enabled());

        let on = PolySynth::new(SynthConfig {
            mpe_enabled: true,
            ..Default::default()
        })
        .unwrap();
        assert!(on.mpe_enabled());
    }

    /// Enabling MPE at runtime reaches voices that are already sounding.
    ///
    /// Per-note expression sent while disabled is dropped at render, so the
    /// setter has to affect live voices for a mid-performance enable to mean
    /// anything.
    #[test]
    fn enabling_mpe_applies_to_a_sounding_voice() {
        let mut synth = PolySynth::new(SynthConfig {
            max_voices: 4,
            oscillator: OscillatorType::Sine,
            mpe_enabled: false,
            mpe_pitch_bend_range: tutti_core::Semitones(48.0),
            ..Default::default()
        })
        .unwrap();

        let mut output = [0.0f32; 2];
        queue_midi(&synth, &[ev_note_on(1, 60, 100)]);
        synth.tick(&[], &mut output);
        assert_eq!(synth.active_voice_count(), 1);

        synth.set_mpe_enabled(true);
        assert!(synth.mpe_enabled());
        assert!(
            synth
                .voices
                .iter()
                .find(|v| v.is_active())
                .unwrap()
                .mpe_enabled(),
            "the sounding voice must adopt the new mode, or a mid-performance \
             enable silently does nothing until the next note"
        );
    }

    /// Disabling MPE clears per-note state rather than merely ignoring it.
    ///
    /// The setters store unconditionally, so expression received while disabled
    /// would otherwise sit latent and snap into effect the instant MPE is turned
    /// back on — a sounding note jumping in pitch from a bend it received much
    /// earlier.
    #[test]
    fn disabling_mpe_clears_latent_per_note_state() {
        let mut synth = PolySynth::new(SynthConfig {
            max_voices: 4,
            oscillator: OscillatorType::Sine,
            mpe_enabled: true,
            mpe_pitch_bend_range: tutti_core::Semitones(48.0),
            ..Default::default()
        })
        .unwrap();

        let mut output = [0.0f32; 2];
        queue_midi(&synth, &[ev_note_on(1, 60, 100)]);
        synth.tick(&[], &mut output);

        // Bend the sounding note well off centre.
        let bend =
            MidiEvent::per_note_pitch_bend(MidiGroup::FIRST, MidiChannel::new(1), 60, 0xFFFF_FFFF);
        queue_midi(&synth, &[bend]);
        synth.tick(&[], &mut output);
        assert!(
            synth
                .voices
                .iter()
                .find(|v| v.is_active())
                .unwrap()
                .mpe_state()
                .pitch_bend_semitones
                .get()
                .abs()
                > 0.01,
            "precondition: the bend applied while MPE was on"
        );

        synth.set_mpe_enabled(false);

        assert!(
            synth
                .voices
                .iter()
                .find(|v| v.is_active())
                .unwrap()
                .mpe_state()
                .pitch_bend_semitones
                .get()
                .abs()
                < 0.01,
            "per-note state survived being disabled — it would snap back into \
             effect on the next enable"
        );
    }

    /// A block far longer than `MAX_BUFFER_SIZE` renders every frame, with
    /// MIDI landing inside it past frame 64 — and is sample-identical to the
    /// same audio rendered as eight 64-frame blocks.
    ///
    /// This is design doc 013's D4: the block path mixed into
    /// `[f32; MAX_BUFFER_SIZE]` stack arrays and clamped `size` to them, so in
    /// release a 512-frame block rendered 64 frames and silence. The renderer
    /// now has no block-sized scratch at all; the identity with short blocks is
    /// what shows the long block is not merely non-silent but *right* —
    /// control steps, glides and event offsets all land where they would have.
    ///
    /// *Mutation:* clamping `size` to `MAX_BUFFER_SIZE` at the top of
    /// `render_events` fails this (frame 64 onward goes silent and the event at
    /// 200 is never reached in the long block).
    #[test]
    fn a_long_block_renders_every_frame_like_short_blocks() {
        let config = || SynthConfig {
            sample_rate: tutti_core::SampleRate::from(48_000.0),
            max_voices: 4,
            oscillator: OscillatorType::Saw,
            filter: FilterType::Svf {
                cutoff: Hz(3_000.0),
                q: tutti_core::Q(1.0),
                mode: crate::SvfMode::Lowpass,
            },
            portamento: Some(PortamentoConfig {
                mode: PortamentoMode::Always,
                curve: PortamentoCurve::Linear,
                time: Seconds(0.004),
                constant_time: true,
            }),
            ..Default::default()
        };

        // One 512-frame block: a note at 0, a second note at 200 (gliding in),
        // the first released at 300.
        let mut long = synth(config());
        queue_midi(
            &long,
            &[
                ev_note_on(0, 60, 100),
                ev_note_on(0, 67, 100).with_frame_offset(200),
                ev_note_off(0, 60).with_frame_offset(300),
            ],
        );
        let (mut l_long, mut r_long) = (vec![0.0f32; 512], vec![0.0f32; 512]);
        long.render_planar(&mut l_long, &mut r_long);

        // The same events, rebased into eight 64-frame blocks.
        let mut short = synth(config());
        let (mut l_short, mut r_short) = (Vec::new(), Vec::new());
        for block in 0..8 {
            match block {
                0 => queue_midi(&short, &[ev_note_on(0, 60, 100)]),
                3 => queue_midi(&short, &[ev_note_on(0, 67, 100).with_frame_offset(8)]),
                4 => queue_midi(&short, &[ev_note_off(0, 60).with_frame_offset(44)]),
                _ => {}
            }
            let (mut l, mut r) = ([0.0f32; 64], [0.0f32; 64]);
            short.render_planar(&mut l, &mut r);
            l_short.extend_from_slice(&l);
            r_short.extend_from_slice(&r);
        }

        let tail_peak = l_long[448..].iter().fold(0.0f32, |a, s| a.max(s.abs()));
        assert!(
            tail_peak > 0.01,
            "the last 64 frames of the long block are silent"
        );
        assert_eq!(
            l_long, l_short,
            "left: one 512-frame block differs from eight 64-frame blocks"
        );
        assert_eq!(
            r_long, r_short,
            "right: one 512-frame block differs from eight 64-frame blocks"
        );
        assert_eq!(long.active_voice_count(), short.active_voice_count());
    }

    /// Sub-voices added under a held note sound at once, without waiting for
    /// the next note-on.
    ///
    /// Growing unison from 1 to 3 at full spread moves the existing sub-voice
    /// hard left and adds two more, centre and hard right. If the added lanes
    /// started idle the right channel would go silent until the key was struck
    /// again.
    ///
    /// *Mutation:* leaving the added lanes as allocated (idle) in
    /// `VoiceBank::resize_stride` fails this.
    #[test]
    fn unison_grown_under_a_held_note_sounds_immediately() {
        let mut synth = synth(SynthConfig {
            sample_rate: tutti_core::SampleRate::from(48_000.0),
            max_voices: 2,
            oscillator: OscillatorType::Saw,
            unison: Some(UnisonConfig {
                voice_count: 1,
                detune_cents: tutti_core::Cents(12.0),
                stereo_spread: Spread(1.0),
                phase_randomize: false,
            }),
            ..Default::default()
        });
        queue_midi(&synth, &[ev_note_on(0, 57, 100)]);
        let (mut l, mut r) = ([0.0f32; 64], [0.0f32; 64]);
        for _ in 0..16 {
            synth.render_planar(&mut l, &mut r);
        }

        synth.set_unison_voice_count(3);
        let rms = |x: &[f32]| (x.iter().map(|s| s * s).sum::<f32>() / x.len() as f32).sqrt();
        let (mut left, mut right) = (Vec::new(), Vec::new());
        for _ in 0..8 {
            synth.render_planar(&mut l, &mut r);
            left.extend_from_slice(&l);
            right.extend_from_slice(&r);
        }
        assert!(rms(&left) > 0.05, "the held note went quiet on resize");
        assert!(
            rms(&right) > 0.5 * rms(&left),
            "the added sub-voices are silent: left rms {}, right rms {}",
            rms(&left),
            rms(&right)
        );
    }

    /// A portamento glide sounds the same through `process` as through `tick`.
    ///
    /// This is design doc 013's D5. `tick` has always glided per sample, but
    /// `process` ticked the glide `block_len` times and then rendered the whole
    /// MIDI sub-block at the final pitch: a staircase, one step per block,
    /// always ahead of the true glide. The phase error that leaves accumulates
    /// over the glide, so the two paths' *waveforms* drift apart even though
    /// their pitch at any block boundary agrees. Now both run the same
    /// renderer, and inside a control step the pitch ramps per sample.
    ///
    /// Not asserted bit-identical: `tick` takes a control step every sample and
    /// `process` every 16, and between them the pitch ramps linearly rather
    /// than along the glide's exponential curve.
    ///
    /// *Mutation:* rendering each control step at its target pitch (zeroing
    /// `inc_step` and loading `inc_target` at the top of `render_group`) fails
    /// this at 0.33 against a 0.0056 tolerance; the correct renderer measures
    /// 0.0018.
    #[test]
    fn process_and_tick_agree_through_a_glide() {
        let config = || SynthConfig {
            sample_rate: tutti_core::SampleRate::from(48_000.0),
            max_voices: 1,
            voice_mode: VoiceMode::Mono,
            oscillator: OscillatorType::Sine,
            envelope: EnvelopeConfig::new(
                Seconds(0.001),
                Seconds(0.0),
                Amplitude(1.0),
                Seconds(0.1),
            ),
            portamento: Some(PortamentoConfig {
                mode: PortamentoMode::Always,
                curve: PortamentoCurve::Linear,
                time: Seconds(0.05),
                constant_time: true,
            }),
            ..Default::default()
        };
        const BLOCKS: usize = 60;

        let mut ticked = synth(config());
        let mut blocked = synth(config());
        let mut via_tick = Vec::new();
        let mut via_process = Vec::new();
        for block in 0..BLOCKS {
            let events: &[MidiEvent] = match block {
                0 => &[ev_note_on(0, 48, 100)],
                // Three octaves up over 50 ms, well after the attack.
                10 => &[ev_note_on(0, 84, 100)],
                _ => &[],
            };
            queue_midi(&ticked, events);
            queue_midi(&blocked, events);

            let mut frame = [0.0f32; 2];
            for _ in 0..64 {
                ticked.tick(&[], &mut frame);
                via_tick.push(frame[0]);
            }
            let (mut l, mut r) = ([0.0f32; 64], [0.0f32; 64]);
            blocked.render_planar(&mut l, &mut r);
            via_process.extend_from_slice(&l);
        }

        let glide = 10 * 64..BLOCKS * 64;
        let worst = glide
            .clone()
            .map(|i| (via_tick[i] - via_process[i]).abs())
            .fold(0.0f32, f32::max);
        let level = via_tick[glide].iter().fold(0.0f32, |a, s| a.max(s.abs()));
        assert!(level > 0.1, "the glide is inaudible");
        assert!(
            worst < 0.01 * level,
            "tick and process diverge by {worst} (level {level}) during the glide"
        );
    }

    /// Each note of a chord struck together gets its own stack of random
    /// start phases, and the lanes really start on them.
    ///
    /// The phases used to live in the shared unison table, drawn once per
    /// note-on and read when each voice started — so every note of a chord
    /// read the last draw and all stacks started identically, the transient
    /// spike `phase_randomize` exists to avoid, summed across the chord.
    ///
    /// Both notes are the same pitch (on two channels) with no detune, so all
    /// six lanes run at one increment and their phases after the first block
    /// differ exactly as their start phases did.
    ///
    /// *Mutation:* starting lanes from the shared table again
    /// (`unison.voice_params(sub).phase_offset` in `drive`) fails this.
    #[test]
    fn a_chord_gets_a_distinct_phase_stack_per_voice() {
        let mut synth = synth(SynthConfig {
            sample_rate: tutti_core::SampleRate::from(48_000.0),
            max_voices: 2,
            oscillator: OscillatorType::Saw,
            unison: Some(UnisonConfig {
                voice_count: 3,
                detune_cents: tutti_core::Cents(0.0),
                stereo_spread: Spread(0.0),
                phase_randomize: true,
            }),
            ..Default::default()
        });
        synth.seed_unison_rng(99);
        queue_midi(&synth, &[ev_note_on(0, 60, 100), ev_note_on(1, 60, 100)]);
        let (mut l, mut r) = ([0.0f32; 64], [0.0f32; 64]);
        synth.render_planar(&mut l, &mut r);

        let phases: Vec<Vec<f32>> = (0..2)
            .map(|v| {
                (0..3)
                    .map(|s| synth.bank.phase(synth.bank.lane(v, s)))
                    .collect()
            })
            .collect();
        assert!(
            synth.voices.iter().all(|v| v.is_active()),
            "both notes should be sounding"
        );
        assert_ne!(
            phases[0], phases[1],
            "both voices of the chord started in phase: {phases:?}"
        );
        for stack in &phases {
            assert!(
                stack.windows(2).all(|w| (w[0] - w[1]).abs() > 1e-4),
                "a stack's sub-voices share a phase: {stack:?}"
            );
        }
    }

    /// A stolen voice changes level without a click.
    ///
    /// The new note's velocity differs from the old one's. The voice is still
    /// sounding, so its gain must ramp to the new level; stepping it in one
    /// sample puts a discontinuity of `x * (g_old - g_new)` in the output.
    ///
    /// The steal is timed to land near a peak of a 110 Hz sine, where that step
    /// is largest: the steady sine moves at most `2π·110/48000 ≈ 0.014` of its
    /// peak per sample, and the step would be about 0.7 of it.
    ///
    /// *Mutations, each run:* snapping gains on every note-on (`snap_gain =
    /// true` rather than `fresh` in `note_on`) fails this; so does replacing the
    /// gain glide with a jump to the target, which is what a ramp across the
    /// control step amounts to in `tick` (one frame per step).
    #[test]
    fn a_steal_at_a_different_velocity_does_not_click() {
        let mut synth = synth(SynthConfig {
            sample_rate: tutti_core::SampleRate::from(48_000.0),
            max_voices: 1,
            allocation_strategy: crate::AllocationStrategy::Oldest,
            oscillator: OscillatorType::Sine,
            envelope: EnvelopeConfig::new(
                Seconds(0.001),
                Seconds(0.0),
                Amplitude(1.0),
                Seconds(0.1),
            ),
            ..Default::default()
        });
        queue_midi(&synth, &[ev_note_on(0, 45, 127)]);
        let mut frame = [0.0f32; 2];
        let mut peak = 0.0f32;
        for _ in 0..4800 {
            synth.tick(&[], &mut frame);
            peak = peak.max(frame[0].abs());
        }
        // Advance to near a peak, then steal on another channel at low velocity.
        let mut prev = frame[0];
        while frame[0].abs() < 0.8 * peak {
            synth.tick(&[], &mut frame);
            prev = frame[0];
        }
        queue_midi(&synth, &[ev_note_on(1, 45, 20)]);
        let mut worst = 0.0f32;
        for _ in 0..256 {
            synth.tick(&[], &mut frame);
            worst = worst.max((frame[0] - prev).abs());
            prev = frame[0];
        }
        assert_eq!(
            synth.active_voice_count(),
            1,
            "the steal should reuse the one voice"
        );
        assert!(
            worst < 0.2 * peak,
            "the steal stepped the output by {worst} (peak {peak})"
        );
    }

    /// Reset All Controllers returns CC74 to its centre, leaving the cutoff
    /// where the patch put it.
    ///
    /// It used to set the CC74 position to 0.0, which the `4^(v - 0.5)` map
    /// reads as half the cutoff.
    ///
    /// *Mutation:* `set_cc_cutoff(0.0)` in the `RESET_ALL` arm fails this.
    #[test]
    fn reset_all_controllers_leaves_the_cutoff_alone() {
        let config = || SynthConfig {
            sample_rate: tutti_core::SampleRate::from(48_000.0),
            max_voices: 1,
            oscillator: OscillatorType::Saw,
            filter: FilterType::Svf {
                cutoff: Hz(600.0),
                q: tutti_core::Q(0.707),
                mode: crate::SvfMode::Lowpass,
            },
            ..Default::default()
        };
        let render = |reset: bool| {
            let mut s = synth(config());
            let mut events = vec![ev_note_on(0, 45, 100)];
            if reset {
                events.push(ev_cc(0, cc::RESET_ALL, 0));
            }
            queue_midi(&s, &events);
            let (mut l, mut r) = ([0.0f32; 64], [0.0f32; 64]);
            let mut out = Vec::new();
            for _ in 0..60 {
                s.render_planar(&mut l, &mut r);
                out.extend_from_slice(&l);
            }
            out
        };
        let rms = |x: &[f32]| (x.iter().map(|s| s * s).sum::<f32>() / x.len() as f32).sqrt();
        let (plain, reset) = (render(false), render(true));
        let (a, b) = (rms(&plain[1024..]), rms(&reset[1024..]));
        assert!(
            (a - b).abs() / a < 0.01,
            "Reset All Controllers moved the level from {a} to {b}"
        );
    }

    /// The voices and the bank use the engine's clamped unison count, not the
    /// config's raw one.
    ///
    /// *Mutation:* sizing from `u.voice_count` (the raw config value) in
    /// `PolySynth::new` fails this.
    #[test]
    fn an_oversized_unison_count_is_clamped_everywhere() {
        let synth = synth(SynthConfig {
            max_voices: 2,
            unison: Some(UnisonConfig {
                voice_count: 40,
                ..Default::default()
            }),
            ..Default::default()
        });
        assert_eq!(synth.unison_voice_count(), 16);
        assert_eq!(synth.voices[0].sub_voice_count(), 16);
        assert_eq!(
            synth.bank.lane(1, 0),
            16,
            "the bank's stride disagrees with the engine"
        );
    }
}
