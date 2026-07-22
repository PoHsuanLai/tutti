//! Polyphonic synthesizer implementing [`AudioUnit`].

use crate::synth_voice::SynthVoice;
use crate::SynthConfig;
use crate::{AllocationResult, Portamento, UnisonEngine, VoiceAllocator, VoiceAllocatorConfig};
use smallvec::SmallVec;
use tutti_midi_types::{cc, MidiSource, MidiUnitId, NoteId};
use tutti_core::{AudioUnit, BufferMut, BufferRef, Shared, SignalFrame};
use tutti_midi_types::ump::MidiEvent;
use tutti_midi_runtime::{MidiEventSlot, MidiReceiver, MidiSender};

extern crate alloc;
use alloc::sync::Arc;
use alloc::vec::Vec;

use arc_swap::ArcSwapOption;

const FINISHED_NOTES_CAPACITY: usize = 16;

/// `Sized` wrapper so a `dyn MidiSource` trait object can live in an
/// [`ArcSwapOption`] (arc-swap needs the stored `Arc`'s pointee to be `Sized`).
struct MidiSourceHandle(Arc<dyn MidiSource>);

/// Polyphonic synthesizer combining tutti-synth building blocks with FunDSP.
///
/// Construct one from a [`SynthConfig`] via [`PolySynth::new`]. The synth always
/// owns a lock-free MIDI inbox; callers push events via [`PolySynth::midi_sender`].
/// For offline export, the inbox source can be replaced with a
/// [`MidiSnapshotReader`] via [`PolySynth::set_midi_source`].
///
/// [`MidiSnapshotReader`]: tutti_midi_runtime::MidiSnapshotReader
pub struct PolySynth {
    config: SynthConfig,
    allocator: VoiceAllocator,
    voices: Vec<SynthVoice>,
    portamento: Option<Portamento>,
    unison: Option<UnisonEngine>,
    pitch_bend: f32,
    master_volume: Shared,
    midi_unit_id: MidiUnitId,
    midi_sender: MidiSender,
    midi_receiver: MidiReceiver,
    /// Optional override (e.g. `MidiSnapshotReader` for offline export
    /// or `MidiClipSource` for beat-scheduled clip playback). When set,
    /// `tick()`/`process()` poll this instead of `midi_receiver`.
    ///
    /// A **shared** `Arc<ArcSwapOption<…>>`, not a per-clone `Option`: fundsp's
    /// frontend/backend split runs a *different* clone than the one
    /// `set_midi_source` mutates, and `node_mut`/clone edits are discarded by
    /// `Net::migrate` on commit. Sharing the slot makes an install on any clone
    /// visible to the running box, lock-free — otherwise clip playback silently
    /// never reaches the audio thread. `isolate()` (offline export) deliberately
    /// swaps in a FRESH private slot to sever this sharing.
    midi_source_override: Arc<ArcSwapOption<MidiSourceHandle>>,
    midi_buffer: Vec<MidiEvent>,
    mix_buffer: [f32; 2],
    finished_indices: SmallVec<[usize; FINISHED_NOTES_CAPACITY]>,
    /// Running absolute-sample counter. Incremented by the block size
    /// in `process()` (or `1` in `tick()`) and handed to MIDI sources
    /// so beat-scheduled events can compute their `frame_offset`
    /// relative to the start of the next block.
    sample_pos: u64,
}

impl PolySynth {
    /// Build a synth from a [`SynthConfig`]. Configure the config the idiomatic
    /// Bevy way — `Default` plus struct-update — e.g.
    /// `SynthConfig { oscillator: OscillatorType::Saw, max_voices: 8, ..default() }`.
    ///
    /// Returns [`Err`] if `max_voices` is 0.
    pub fn new(config: SynthConfig) -> crate::Result<Self> {
        if config.max_voices == 0 {
            return Err(crate::Error::InvalidConfig(
                "max_voices must be at least 1".into(),
            ));
        }

        let allocator_config = VoiceAllocatorConfig {
            max_voices: config.max_voices,
            mode: config.voice_mode,
            strategy: config.allocation_strategy,
        };
        let allocator = VoiceAllocator::new(allocator_config);

        let unison = config.unison.as_ref().map(|u| UnisonEngine::new(u.clone()));
        let unison_count = config
            .unison
            .as_ref()
            .map_or(1, |u| usize::from(u.voice_count));

        let mut voices = Vec::with_capacity(config.max_voices);
        for _ in 0..config.max_voices {
            let mut voice = SynthVoice::from_config(&config, unison_count);
            voice.set_sample_rate(tutti_core::SampleRate(config.sample_rate));
            voices.push(voice);
        }

        let portamento = config
            .portamento
            .as_ref()
            .map(|p| Portamento::new(p.clone(), config.sample_rate));

        let master_volume = tutti_core::shared(1.0);

        let midi_unit_id = MidiUnitId::next();
        let (midi_sender, midi_receiver) = MidiEventSlot::pair(midi_unit_id);

        Ok(Self {
            config,
            allocator,
            voices,
            portamento,
            unison,
            pitch_bend: 0.0,
            master_volume,
            midi_unit_id,
            midi_sender,
            midi_receiver,
            midi_source_override: Arc::new(ArcSwapOption::empty()),
            midi_buffer: vec![MidiEvent::noop(); 256],
            mix_buffer: [0.0; 2],
            finished_indices: SmallVec::new(),
            sample_pos: 0,
        })
    }

    /// Producer handle for this synth's MIDI inbox. Cheap to clone; insert
    /// into a `MidiBus` or hand to anything that pushes MIDI events.
    pub fn midi_sender(&self) -> MidiSender {
        self.midi_sender.clone()
    }

    /// Override the MIDI source. Used by offline export to swap the live
    /// receiver for a [`MidiSnapshotReader`], or by clip playback to
    /// install a [`tutti_midi_runtime::MidiClipSource`] /
    /// [`tutti_midi_runtime::CompositeMidiSource`].
    ///
    /// The source is held in an `Arc`, so the same instance survives
    /// the unit-clone fundsp performs on each `commit()`.
    ///
    /// [`MidiSnapshotReader`]: tutti_midi_runtime::MidiSnapshotReader
    pub fn set_midi_source(&mut self, source: Arc<dyn MidiSource>) {
        self.midi_source_override
            .store(Some(Arc::new(MidiSourceHandle(source))));
    }

    /// Drop a previously-installed override; subsequent ticks poll the
    /// live `MidiReceiver` again.
    pub fn clear_midi_source(&mut self) {
        self.midi_source_override.store(None);
    }

    fn poll_count(&mut self, block_size: usize) -> usize {
        let block_start = self.sample_pos;
        // `load()` is lock-free; the guard holds the current source for the poll.
        let source = self.midi_source_override.load();
        match source.as_ref() {
            Some(handle) => handle.0.poll_into(
                self.midi_unit_id,
                block_start,
                block_size,
                &mut self.midi_buffer,
            ),
            None => self.midi_receiver.poll_into(&mut self.midi_buffer),
        }
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

    pub fn set_volume(&mut self, volume: f32) {
        self.master_volume.set(volume.clamp(0.0, 1.0));
    }

    pub fn volume(&self) -> f32 {
        self.master_volume.value()
    }

    pub fn active_voice_count(&self) -> usize {
        self.voices.iter().filter(|v| v.is_active()).count()
    }

    pub fn unison_config(&self) -> Option<&crate::UnisonConfig> {
        self.unison.as_ref().map(|u| u.config())
    }

    pub fn unison_voice_count(&self) -> usize {
        self.unison.as_ref().map_or(1, |u| u.voice_count())
    }

    pub fn set_unison_detune(&mut self, cents: impl Into<tutti_core::Cents>) {
        if let Some(unison) = &mut self.unison {
            unison.set_detune(cents);
        }
    }

    pub fn set_unison_stereo_spread(&mut self, spread: f32) {
        if let Some(unison) = &mut self.unison {
            unison.set_stereo_spread(spread);
        }
    }

    pub fn set_unison_voice_count(&mut self, count: u8) {
        if let Some(unison) = &mut self.unison {
            unison.set_voice_count(count);
            let new_count = unison.voice_count();
            for voice in &mut self.voices {
                voice.resize_unison(new_count);
            }
        }
    }

    pub fn set_unison_config(&mut self, config: crate::UnisonConfig) {
        if let Some(unison) = &mut self.unison {
            unison.set_config(config);
            let new_count = unison.voice_count();
            for voice in &mut self.voices {
                voice.resize_unison(new_count);
            }
        }
    }

    pub fn seed_unison_rng(&mut self, seed: u32) {
        if let Some(unison) = &mut self.unison {
            unison.seed_rng(seed);
        }
    }

    pub fn unison_params(&self) -> Option<&[crate::UnisonVoiceParams]> {
        self.unison.as_ref().map(|u| u.all_params())
    }

    fn process_midi_event(&mut self, event: &MidiEvent) {
        use tutti_midi_types::convert::{
            bend_u32_to_signed_f32, u16_to_unit_f32, u32_to_unit_f32,
        };
        use tutti_midi_types::midi2::channel_voice2::ChannelVoice2 as Cv2;
        use tutti_midi_types::midi2::{Channeled, UmpMessage};

        // `normalize` folds velocity-0 NoteOn→NoteOff and promotes any inbound
        // MIDI 1.0 channel voice to MIDI 2.0, so we match a single vocabulary.
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
            }
            Cv2::NoteOff(m) => {
                self.handle_note_off(u8::from(m.note_number()), channel);
            }
            Cv2::ControlChange(m) => {
                self.handle_cc(
                    u8::from(m.control()),
                    u32_to_unit_f32(m.control_change_data()),
                    channel,
                );
            }
            Cv2::ChannelPitchBend(m) => {
                let value = bend_u32_to_signed_f32(m.pitch_bend_data());
                if self.config.mpe_enabled {
                    self.handle_mpe_pitch_bend_normalized(channel, value);
                } else {
                    self.pitch_bend = value;
                    self.apply_pitch_bend();
                }
            }
            Cv2::ChannelPressure(m) if self.config.mpe_enabled => {
                self.handle_mpe_pressure_normalized(
                    channel,
                    u32_to_unit_f32(m.channel_pressure_data()),
                );
            }
            // MIDI 2.0 native per-note messages address one voice by note-id —
            // two same-pitch notes stay independent even on one channel.
            Cv2::PerNotePitchBend(m) => {
                let id = NoteId::from_channel_note(channel, u8::from(m.note_number()));
                self.set_voice_mpe_pitch_bend(id, bend_u32_to_signed_f32(m.pitch_bend_data()));
            }
            Cv2::KeyPressure(m) => {
                let id = NoteId::from_channel_note(channel, u8::from(m.note_number()));
                self.set_voice_mpe_pressure(id, u32_to_unit_f32(m.key_pressure_data()));
            }
            // Assignable per-note controllers carry a raw index. We honor the dims
            // the synth voice can apply: CC74 → slide, CC7 → per-note gain.
            Cv2::AssignablePerNoteController(m) => {
                let id = NoteId::from_channel_note(channel, u8::from(m.note_number()));
                let data = u32_to_unit_f32(m.controller_data());
                match m.index() {
                    cc::BRIGHTNESS => self.set_voice_mpe_slide(id, data),
                    cc::VOLUME => self.set_voice_mpe_gain(id, data),
                    _ => {}
                }
            }
            // Registered per-note controllers carry a *semantic* controller enum
            // (index resolved to Volume/Pan/Brightness/…). We honor the dims the
            // synth voice can apply: Volume → per-note gain, Brightness (CC74 /
            // SoundController index 5) → slide. Pan stays recognized-but-unwired
            // (no per-note pan DSP on `SynthVoice` yet — don't invent it).
            Cv2::RegisteredPerNoteController(m) => {
                use tutti_midi_types::midi2::channel_voice2::Controller;
                let id = NoteId::from_channel_note(channel, u8::from(m.note_number()));
                match m.controller() {
                    Controller::Volume(data) => {
                        self.set_voice_mpe_gain(id, u32_to_unit_f32(data));
                    }
                    Controller::Brightness(data)
                    | Controller::SoundController { index: 5, data } => {
                        self.set_voice_mpe_slide(id, u32_to_unit_f32(data));
                    }
                    _ => {}
                }
            }
            // MIDI 2.0 Per-Note Management (M2-104 §7.4.15).
            //
            // **Reset (S)**: reset the addressed note's per-note controllers to
            // their defaults — pitch bend→0, pressure→0, slide→center — while the
            // note keeps sounding. Fully honored via `reset_voice_mpe`.
            //
            // **Detach (D)**: spec-intent is to detach ongoing per-note controllers
            // from the note so a subsequent note-off / same-pitch note-on does not
            // disturb them. `SynthVoice` has no separate "detached controller"
            // lifetime — per-note expression lives *on* the voice and dies with it —
            // so there is nothing to re-home. We treat Detach as a lighter-touch
            // reset of the voice's accumulated per-note expression: it clears the
            // current per-note state but does NOT (and cannot, given this voice
            // model) preserve controllers past the voice's own lifetime. This is
            // spec-faithful in effect (per-note controllers stop influencing the
            // note) without pretending to a controller-persistence model the synth
            // does not have.
            Cv2::PerNoteManagement(m) => {
                let id = NoteId::from_channel_note(channel, u8::from(m.note_number()));
                if m.reset() || m.detach() {
                    self.reset_voice_mpe(id);
                }
            }
            _ => {}
        }
    }

    fn handle_note_on(&mut self, note: u8, vel_norm: f32, channel: u8) {
        let id = NoteId::from_channel_note(channel, note);
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
            let bend_semitones = self.pitch_bend * self.config.pitch_bend_range.get();
            let bend_multiplier = 2.0_f32.powf(bend_semitones / 12.0);

            let target_freq = if let Some(ref mut porta) = self.portamento {
                porta.set_target(base_freq, is_legato);
                porta.current().get() * bend_multiplier
            } else {
                base_freq * bend_multiplier
            };

            let voice = &mut self.voices[slot_index];
            voice.set_velocity_mod(vel_norm);
            if is_legato {
                voice.set_pitch(target_freq, self.unison.as_ref());
                voice.update_legato(note, channel);
            } else {
                voice.note_on(note, channel, vel_norm, target_freq, self.unison.as_mut());
            }
        }
    }

    fn handle_note_off(&mut self, note: u8, channel: u8) {
        let id = NoteId::from_channel_note(channel, note);
        // `release` resolves the exact voice by id and tells us whether it truly
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

    /// MIDI 2.0 Per-Note Management *Reset* (M2-104 §7.4.15): snap the addressed
    /// voice's per-note controllers (pitch bend, pressure, slide, gain) back to
    /// their note-on defaults, leaving the note sounding. Others are untouched.
    fn reset_voice_mpe(&mut self, id: NoteId) {
        self.with_voice_for_id(id, |v| v.reset_mpe());
    }

    fn handle_cc(&mut self, cc_num: u8, value: f32, channel: u8) {
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
            cc::BRIGHTNESS => {
                if self.config.mpe_enabled {
                    self.handle_mpe_slide(channel, value);
                } else {
                    self.voices.iter_mut().for_each(|v| v.set_cc_cutoff(value));
                }
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
            cc::ALL_SOUND_OFF => {
                self.voices
                    .iter_mut()
                    .filter(|v| v.channel() == channel)
                    .for_each(|v| v.reset());
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

        let bend_semitones = self.pitch_bend * self.config.pitch_bend_range.get();
        let tuning = &self.config.tuning;
        let unison = self.unison.as_ref();
        self.voices
            .iter_mut()
            .filter(|v| v.is_active())
            .for_each(|voice| {
                let bent_freq =
                    tuning.fractional_note_to_freq(f32::from(voice.note()) + bend_semitones);
                voice.set_pitch(bent_freq, unison);
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

    fn handle_mpe_pitch_bend_normalized(&mut self, channel: u8, bend_norm: f32) {
        let semitones = tutti_core::Semitones(bend_norm * self.config.mpe_pitch_bend_range.get());
        for voice in &mut self.voices {
            if voice.is_active() && voice.channel() == channel {
                voice.set_mpe_pitch_bend(semitones);
            }
        }
    }

    fn handle_mpe_pressure_normalized(&mut self, channel: u8, norm: f32) {
        for voice in &mut self.voices {
            if voice.is_active() && voice.channel() == channel {
                voice.set_mpe_pressure(norm);
            }
        }
    }

    fn handle_mpe_slide(&mut self, channel: u8, value: f32) {
        for voice in &mut self.voices {
            if voice.is_active() && voice.channel() == channel {
                voice.set_mpe_slide(value);
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
}

impl AudioUnit for PolySynth {
    fn reset(&mut self) {
        for voice in &mut self.voices {
            voice.reset();
        }
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
    /// 1. **MIDI inbox** — `midi_receiver` + `midi_source_override`. A shared
    ///    inbox is drained to exactly one consumer, so the worker would *steal*
    ///    the live synth's note-ons/offs/CC. Fixed by minting a fresh,
    ///    unconnected sender/receiver pair (nothing holds this sender, so the
    ///    receiver stays permanently empty) and dropping the override.
    ///
    /// 2. **Per-voice `Shared` params** — every [`SynthVoice`] (and its
    ///    sub-voices) holds `gate`/`pitch`/`filter_cutoff`/`filter_resonance` as
    ///    `Shared` (`Arc<AtomicU32>`), which `#[derive(Clone)]` aliases. The
    ///    voice's `tick` *writes* these every sample (envelope→filter, pitch
    ///    glide), so a worker ticking the clone would stomp the atomics the live
    ///    voice reads into its output → continuous garbage. Fixed by rebuilding
    ///    the voice set from config: `from_config` mints fresh `Shared`s, and a
    ///    clean, inactive voice set is exactly the correct event-free render
    ///    state. The allocator is reset to agree the slots are free.
    ///
    /// After `isolate()` the synth reads nothing from, and writes nothing into,
    /// the live world — it renders silence until its own (now-empty) inbox feeds
    /// it events, which it never will.
    fn isolate(&mut self) {
        let (sender, receiver) = MidiEventSlot::pair(self.midi_unit_id);
        self.midi_sender = sender;
        self.midi_receiver = receiver;
        // Swap in a FRESH private slot (not `store(None)` on the shared one):
        // the override slot is now shared across clones, so clearing the shared
        // slot would sever the LIVE synth's clip source mid-export. A brand-new
        // empty slot detaches this export clone alone, which is exactly what
        // isolation wants — this clone renders silence, the live synth is
        // untouched.
        self.midi_source_override = Arc::new(ArcSwapOption::empty());

        // Rebuild voices with fresh `Shared` atomics (see #2 above).
        let unison_count = self
            .config
            .unison
            .as_ref()
            .map_or(1, |u| usize::from(u.voice_count));
        self.voices.clear();
        for _ in 0..self.config.max_voices {
            let mut voice = SynthVoice::from_config(&self.config, unison_count);
            voice.set_sample_rate(tutti_core::SampleRate(self.config.sample_rate));
            self.voices.push(voice);
        }
        self.allocator.reset();
    }

    fn set_sample_rate(&mut self, sample_rate: tutti_core::SampleRate) {
        let sample_rate: f64 = sample_rate.get();
        for voice in &mut self.voices {
            voice.set_sample_rate(tutti_core::SampleRate(sample_rate));
        }
        if let Some(ref mut porta) = self.portamento {
            porta.set_sample_rate(sample_rate);
        }
    }

    fn tick(&mut self, _input: &[f32], output: &mut [f32]) {
        self.poll_midi_events();

        if let Some(ref mut porta) = self.portamento {
            if porta.is_gliding() {
                let porta_freq = porta.tick().get();
                let bend_semitones = self.pitch_bend * self.config.pitch_bend_range.get();
                let bend_multiplier = 2.0_f32.powf(bend_semitones / 12.0);
                let freq = porta_freq * bend_multiplier;
                let unison_ref = self.unison.as_ref();
                for voice in &mut self.voices {
                    if voice.is_active() {
                        voice.set_pitch(freq, unison_ref);
                    }
                }
            }
        }

        self.mix_buffer = [0.0, 0.0];
        self.finished_indices.clear();

        for (i, voice) in self.voices.iter_mut().enumerate() {
            if voice.is_active() {
                let (left, right) = voice.tick_stereo(self.unison.as_ref());

                let level = left.abs().max(right.abs());
                voice.set_envelope_level(level);
                self.allocator.update_envelope_level(i, level);

                if voice.gate_value() == 0.0 && level < 0.0001 {
                    voice.deactivate();
                    self.finished_indices.push(i);
                }

                self.mix_buffer[0] += left;
                self.mix_buffer[1] += right;
            }
        }

        for slot_index in core::mem::take(&mut self.finished_indices) {
            self.mark_voice_finished(slot_index);
        }

        self.allocator.advance_time(1);
        self.sample_pos = self.sample_pos.wrapping_add(1);

        let volume = self.master_volume.value();
        output[0] = self.mix_buffer[0] * volume;
        if output.len() > 1 {
            output[1] = self.mix_buffer[1] * volume;
        }
    }

    fn process(&mut self, size: usize, _input: &BufferRef, output: &mut BufferMut) {
        if size == 0 {
            return;
        }

        let midi_count = self.poll_midi_events_sorted(size);
        let stereo = output.channels() > 1;

        let mut mix_left = [0.0f32; 64];
        let mut mix_right = [0.0f32; 64];

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

            let block_len = block_end - block_start;

            if let Some(ref mut porta) = self.portamento {
                if porta.is_gliding() {
                    for _ in 0..block_len {
                        let porta_freq = porta.tick().get();
                        let bend_semitones = self.pitch_bend * self.config.pitch_bend_range.get();
                        let bend_multiplier = 2.0_f32.powf(bend_semitones / 12.0);
                        let freq = porta_freq * bend_multiplier;
                        let unison_ref = self.unison.as_ref();
                        for voice in &mut self.voices {
                            if voice.is_active() {
                                voice.set_pitch(freq, unison_ref);
                            }
                        }
                    }
                }
            }

            mix_left[block_start..block_end].fill(0.0);
            mix_right[block_start..block_end].fill(0.0);

            self.finished_indices.clear();

            for (i, voice) in self.voices.iter_mut().enumerate() {
                if voice.is_active() {
                    let peak = voice.process_block_stereo(
                        self.unison.as_ref(),
                        &mut mix_left,
                        &mut mix_right,
                        block_start,
                        block_len,
                    );

                    voice.set_envelope_level(peak);
                    self.allocator.update_envelope_level(i, peak);

                    if voice.gate_value() == 0.0 && peak < 0.0001 {
                        voice.deactivate();
                        self.finished_indices.push(i);
                    }
                }
            }

            for slot_index in core::mem::take(&mut self.finished_indices) {
                self.mark_voice_finished(slot_index);
            }

            self.allocator.advance_time(block_len as u64);
            block_start = block_end;
        }

        while event_idx < midi_count {
            let event = self.midi_buffer[event_idx];
            self.process_midi_event(&event);
            event_idx += 1;
        }

        let volume = self.master_volume.value();
        for i in 0..size {
            output.set_f32(0, i, mix_left[i] * volume);
            if stereo {
                output.set_f32(1, i, mix_right[i] * volume);
            }
        }

        self.sample_pos = self.sample_pos.wrapping_add(size as u64);
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
            + self.voices.iter().map(|v| v.footprint()).sum::<usize>()
            + self.midi_buffer.capacity() * core::mem::size_of::<MidiEvent>()
    }

    fn allocate(&mut self) {
        for voice in &mut self.voices {
            voice.allocate();
        }
    }
}

impl PolySynth {
    /// This unit's MIDI routing address.
    pub fn midi_unit_id(&self) -> MidiUnitId {
        self.midi_unit_id
    }
}

impl Clone for PolySynth {
    fn clone(&self) -> Self {
        // Share the underlying MIDI inbox so events queued via any
        // outstanding `MidiSender` keep reaching whichever PolySynth
        // fundsp is currently polling. Same for the MIDI source override:
        // the `Arc<ArcSwapOption>` SLOT is shared (Arc clone), so an install
        // on any clone is seen by the box the audio thread runs — a plain
        // `Option` clone (the old bug) gave each clone a private slot that
        // never propagated.
        Self {
            config: self.config.clone(),
            allocator: self.allocator.clone(),
            voices: self.voices.clone(),
            portamento: self.portamento.clone(),
            unison: self.unison.clone(),
            pitch_bend: self.pitch_bend,
            master_volume: self.master_volume.clone(),
            midi_unit_id: self.midi_unit_id,
            midi_sender: self.midi_sender.clone(),
            midi_receiver: self.midi_receiver.clone(),
            midi_source_override: self.midi_source_override.clone(),
            midi_buffer: vec![MidiEvent::noop(); 256],
            mix_buffer: [0.0; 2],
            finished_indices: SmallVec::new(),
            sample_pos: 0,
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
    use tutti_midi_types::convert::{
        midi1_cc_to_midi2, midi1_pitch_bend_to_midi2, midi1_velocity_to_midi2,
    };

    /// Build a `PolySynth` from a config, unwrapping the result.
    fn synth(config: SynthConfig) -> PolySynth {
        PolySynth::new(config).expect("synth builds")
    }

    /// Push events directly through the synth's own MIDI sender.
    fn queue_midi(synth: &PolySynth, events: &[MidiEvent]) {
        synth.midi_sender().queue(events);
    }

    // --- Test event builders (MIDI 1.0 7-bit values upconverted to UMP CV2) ---

    fn ev_note_on(channel: u8, note: u8, vel: u8) -> MidiEvent {
        MidiEvent::note_on(0, channel, note, midi1_velocity_to_midi2(vel))
    }

    fn ev_note_off(channel: u8, note: u8) -> MidiEvent {
        MidiEvent::note_off(0, channel, note, 0)
    }

    fn ev_cc(channel: u8, cc_num: u8, value: u8) -> MidiEvent {
        MidiEvent::cc(0, channel, cc_num, midi1_cc_to_midi2(value))
    }

    fn ev_bend(channel: u8, bend14: u16) -> MidiEvent {
        MidiEvent::pitch_bend(0, channel, midi1_pitch_bend_to_midi2(bend14))
    }

    fn ev_aftertouch(channel: u8, pressure: u8) -> MidiEvent {
        MidiEvent::channel_pressure(0, channel, midi1_cc_to_midi2(pressure))
    }

    #[test]
    fn test_polysynth_midi() {
        let mut synth = synth(SynthConfig {
            sample_rate: 44100.0,
            max_voices: 4,
            voice_mode: VoiceMode::Poly,
            oscillator: OscillatorType::Sine,
            ..Default::default()
        });

        // Queue a note on via registry
        let note_on = ev_note_on(0, 60, 100);
        queue_midi(&synth, &[note_on]);

        // Process one sample to trigger the note
        let mut output = [0.0f32; 2];
        synth.tick(&[], &mut output);

        assert_eq!(synth.active_voice_count(), 1);
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
            sample_rate: 44100.0,
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
    impl MidiSource for NoteOnceSource {
        fn poll_into(
            &self,
            _unit: MidiUnitId,
            _start: u64,
            _block: usize,
            buffer: &mut [MidiEvent],
        ) -> usize {
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
    /// mutates. With the old per-clone `Option<Arc<…>>` this failed silently, so
    /// synth clip playback never reached the audio thread.
    #[test]
    fn midi_source_install_propagates_across_clones() {
        let live = synth(SynthConfig {
            sample_rate: 44100.0,
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

    /// Regression: the deeper half of the same bug. `SynthVoice` holds its
    /// `gate`/`pitch`/`filter_*` as `Shared` (`Arc<AtomicU32>`), which `clone()`
    /// aliases — and the voice *writes* them every tick. So a worker ticking the
    /// render clone would stomp the atomics the live voice reads into its output,
    /// even with the MIDI inbox already severed. `isolate()` must rebuild the
    /// voices with fresh `Shared`s. Prove the clone's voice params are unaliased:
    /// driving the clone's voice leaves the live voice's gate untouched.
    #[test]
    fn isolate_unaliases_voice_shared_params() {
        let mut live = synth(SynthConfig {
            sample_rate: 44100.0,
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
            sample_rate: 44100.0,
            max_voices: 4,
            voice_mode: VoiceMode::Poly,
            oscillator: OscillatorType::Saw,
            unison: Some(UnisonConfig {
                voice_count: 3,
                detune_cents: tutti_core::Cents(15.0),
                stereo_spread: 0.5,
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
    fn test_unison_stereo_output() {
        let mut synth = synth(SynthConfig {
            sample_rate: 44100.0,
            max_voices: 2,
            voice_mode: VoiceMode::Poly,
            oscillator: OscillatorType::Saw,
            envelope: EnvelopeConfig {
                attack: 0.001,
                decay: 0.1,
                sustain: 0.8,
                release: 0.1,
            }, // Fast attack to get output quickly
            unison: Some(UnisonConfig {
                voice_count: 3,
                detune_cents: tutti_core::Cents(15.0),
                stereo_spread: 1.0, // Full stereo spread
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
        // so we need several hundred samples to see envelope output
        let mut output = [0.0f32; 2];
        let mut max_left = 0.0f32;
        let mut max_right = 0.0f32;

        for _ in 0..2000 {
            synth.tick(&[], &mut output);
            max_left = max_left.max(output[0].abs());
            max_right = max_right.max(output[1].abs());
        }

        // With full stereo spread, we should get output on both channels
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
            sample_rate: 44100.0,
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

    #[test]
    fn test_basic_dsp_chain() {
        use tutti_core::dsp::{adsr_live, saw, var};
        use tutti_core::AudioUnit;

        let pitch = tutti_core::shared(440.0);
        let gate = tutti_core::shared(0.0);

        let mut osc: Box<dyn AudioUnit> = Box::new(var(&pitch) >> saw());
        osc.set_sample_rate(tutti_core::SampleRate(44100.0));

        let mut out = [0.0f32; 1];
        osc.tick(&[], &mut out);
        assert!(out[0] != 0.0, "Oscillator should produce output");

        let mut chain: Box<dyn AudioUnit> =
            Box::new(var(&pitch) >> (saw() * (var(&gate) >> adsr_live(0.001, 0.1, 0.8, 0.1))));
        chain.set_sample_rate(tutti_core::SampleRate(44100.0));

        // Process one sample with gate=0 to initialize envelope
        let mut out2 = [0.0f32; 1];
        chain.tick(&[], &mut out2);

        // Trigger gate
        gate.set(1.0);

        // EnvelopeIn samples at 2ms intervals (about 88 samples at 44100Hz)
        // So we need to process more samples to see the envelope respond
        let mut max_out = 0.0f32;
        for _ in 0..500 {
            chain.tick(&[], &mut out2);
            max_out = max_out.max(out2[0].abs());
        }
        assert!(
            max_out > 0.0001,
            "Chain should produce output after triggering gate, max={}",
            max_out
        );
    }

    #[test]
    fn test_dynamic_unison_resize() {
        // Create synth with 2-voice unison
        let mut synth = synth(SynthConfig {
            sample_rate: 44100.0,
            max_voices: 2,
            voice_mode: VoiceMode::Poly,
            oscillator: OscillatorType::Saw,
            unison: Some(UnisonConfig {
                voice_count: 2,
                detune_cents: tutti_core::Cents(10.0),
                stereo_spread: 0.5,
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
            sample_rate: 44100.0,
            max_voices: 4,
            voice_mode: VoiceMode::Poly,
            oscillator: OscillatorType::Sine,
            envelope: EnvelopeConfig {
                attack: 0.001,
                decay: 0.1,
                sustain: 1.0,
                release: 0.1,
            },
            ..Default::default()
        });

        // Play same note (C4=60) on channel 0 and channel 1
        // note_on(frame_offset, channel, note, velocity)
        let note_on_ch0 = ev_note_on(0, 60, 100);
        let note_on_ch1 = ev_note_on(1, 60, 100);
        queue_midi(&synth, &[note_on_ch0, note_on_ch1]);

        // Process to trigger both notes
        let mut output = [0.0f32; 2];
        synth.tick(&[], &mut output);
        assert_eq!(synth.active_voice_count(), 2, "Should have 2 active voices");

        // Note off on channel 0 only
        // note_off(frame_offset, channel, note, velocity)
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
            sample_rate: 44100.0,
            max_voices: 4,
            voice_mode: VoiceMode::Poly,
            oscillator: OscillatorType::Sine,
            envelope: EnvelopeConfig {
                attack: 0.001,
                decay: 0.0,
                sustain: 1.0,
                release: 0.05,
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
        let sustain_on = ev_cc(0, 64, 127);
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
        let sustain_off = ev_cc(0, 64, 0);
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
            sample_rate: 44100.0,
            max_voices: 4,
            voice_mode: VoiceMode::Poly,
            oscillator: OscillatorType::Sine,
            envelope: EnvelopeConfig {
                attack: 0.001,
                decay: 0.0,
                sustain: 1.0,
                release: 0.05,
            },
            ..Default::default()
        });

        // Play note, then press sostenuto
        let note_on = ev_note_on(0, 60, 100);
        queue_midi(&synth, &[note_on]);
        let mut output = [0.0f32; 2];
        synth.tick(&[], &mut output);

        let sostenuto_on = ev_cc(0, 66, 127);
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
        let sostenuto_off = ev_cc(0, 66, 0);
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
            sample_rate: 44100.0,
            max_voices: 4,
            voice_mode: VoiceMode::Poly,
            oscillator: OscillatorType::Sine,
            envelope: EnvelopeConfig {
                attack: 0.001,
                decay: 0.0,
                sustain: 1.0,
                release: 5.0,
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
        let all_sound_off = ev_cc(0, 120, 0);
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
            sample_rate: 44100.0,
            max_voices: 4,
            voice_mode: VoiceMode::Poly,
            oscillator: OscillatorType::Sine,
            envelope: EnvelopeConfig {
                attack: 0.001,
                decay: 0.0,
                sustain: 1.0,
                release: 5.0,
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
        let all_notes_off = ev_cc(0, 123, 0);
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
            sample_rate: 44100.0,
            max_voices: 4,
            voice_mode: VoiceMode::Poly,
            oscillator: OscillatorType::Sine,
            envelope: EnvelopeConfig {
                attack: 0.001,
                decay: 0.0,
                sustain: 1.0,
                release: 5.0,
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
        let all_notes_off = ev_cc(0, 123, 0);
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
            sample_rate: 44100.0,
            max_voices: 4,
            voice_mode: VoiceMode::Poly,
            oscillator: OscillatorType::Sine,
            envelope: EnvelopeConfig {
                attack: 0.001,
                decay: 0.0,
                sustain: 1.0,
                release: 0.05,
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
    fn test_voice_stealing_in_polysynth() {
        let mut synth = synth(SynthConfig {
            sample_rate: 44100.0,
            max_voices: 2,
            voice_mode: VoiceMode::Poly,
            oscillator: OscillatorType::Sine,
            envelope: EnvelopeConfig {
                attack: 0.001,
                decay: 0.0,
                sustain: 1.0,
                release: 5.0,
            }, // Long release so voices stay active
            ..Default::default()
        });

        // Fill all 2 voices
        let note1 = ev_note_on(0, 60, 100);
        let note2 = ev_note_on(0, 64, 100);
        queue_midi(&synth, &[note1, note2]);
        let mut output = [0.0f32; 2];
        synth.tick(&[], &mut output);
        assert_eq!(synth.active_voice_count(), 2);

        // Third note should steal a voice
        let note3 = ev_note_on(0, 67, 100);
        queue_midi(&synth, &[note3]);
        synth.tick(&[], &mut output);

        // Should still have voices active, with the stolen one now playing note 67
        let has_note67 = synth.voices.iter().any(|v| v.is_active() && v.note() == 67);
        assert!(has_note67, "Stolen voice should now play note 67");
    }

    #[test]
    fn test_legato_mode_no_retrigger() {
        let mut synth = synth(SynthConfig {
            sample_rate: 44100.0,
            max_voices: 1,
            voice_mode: VoiceMode::Legato,
            oscillator: OscillatorType::Sine,
            envelope: EnvelopeConfig {
                attack: 0.001,
                decay: 0.0,
                sustain: 1.0,
                release: 0.1,
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
            sample_rate: 44100.0,
            max_voices: 1,
            voice_mode: VoiceMode::Mono,
            oscillator: OscillatorType::Sine,
            envelope: EnvelopeConfig {
                attack: 0.001,
                decay: 0.0,
                sustain: 1.0,
                release: 0.1,
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

    #[test]
    fn test_portamento_with_pitch_bend() {
        let mut synth = synth(SynthConfig {
            sample_rate: 44100.0,
            max_voices: 2,
            voice_mode: VoiceMode::Poly,
            oscillator: OscillatorType::Sine,
            envelope: EnvelopeConfig {
                attack: 0.001,
                decay: 0.0,
                sustain: 1.0,
                release: 0.1,
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
            sample_rate: 44100.0,
            max_voices: 0,
            ..Default::default()
        });
        assert!(result.is_err(), "max_voices=0 should return error");
    }

    #[test]
    fn test_polysynth_reset() {
        let mut synth = synth(SynthConfig {
            sample_rate: 44100.0,
            max_voices: 4,
            voice_mode: VoiceMode::Poly,
            oscillator: OscillatorType::Sine,
            envelope: EnvelopeConfig {
                attack: 0.001,
                decay: 0.0,
                sustain: 1.0,
                release: 5.0,
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
            sample_rate: 44100.0,
            max_voices: 4,
            voice_mode: VoiceMode::Poly,
            oscillator: OscillatorType::Sine,
            envelope: EnvelopeConfig {
                attack: 0.001,
                decay: 0.0,
                sustain: 1.0,
                release: 0.1,
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

        // Pitch bend on channel 1 should only affect voice on channel 1
        let bend = ev_bend(1, 16383);
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
            "Channel 1 should have large pitch bend, got {}",
            voice_ch1.mpe_state().pitch_bend_semitones
        );
        assert!(
            voice_ch2.mpe_state().pitch_bend_semitones.get().abs() < 0.01,
            "Channel 2 should have no pitch bend, got {}",
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
            sample_rate: 44100.0,
            max_voices: 4,
            voice_mode: VoiceMode::Poly,
            oscillator: OscillatorType::Sine,
            envelope: EnvelopeConfig {
                attack: 0.001,
                decay: 0.0,
                sustain: 1.0,
                release: 0.1,
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
        let bend = MidiEvent::per_note_pitch_bend(0, 1, 60, 0xFFFF_FFFF);
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
            sample_rate: 44100.0,
            max_voices: 4,
            voice_mode: VoiceMode::Poly,
            oscillator: OscillatorType::Sine,
            envelope: EnvelopeConfig {
                attack: 0.001,
                decay: 0.0,
                sustain: 1.0,
                release: 0.1,
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
                MidiEvent::per_note_pitch_bend(0, 1, 60, 0xFFFF_FFFF),
                MidiEvent::per_note_pitch_bend(0, 1, 64, 0xFFFF_FFFF),
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
        queue_midi(&synth, &[MidiEvent::per_note_management(0, 1, 60, false, true)]);
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
    fn test_midi2_registered_per_note_controller_brightness_sets_only_addressed_slide() {
        // A Registered Per-Note Controller for Brightness (CC74 / SoundController
        // index 5) addressed to note X must set ONLY note X's slide.
        let mut synth = synth(SynthConfig {
            sample_rate: 44100.0,
            max_voices: 4,
            voice_mode: VoiceMode::Poly,
            oscillator: OscillatorType::Saw,
            filter: FilterType::Moog {
                cutoff: 1000.0,
                resonance: 0.5,
            },
            envelope: EnvelopeConfig {
                attack: 0.001,
                decay: 0.0,
                sustain: 1.0,
                release: 0.1,
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
            &[MidiEvent::per_note_controller(0, 1, 60, 74, 0xFFFF_FFFF, true)],
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
            sample_rate: 44100.0,
            max_voices: 4,
            voice_mode: VoiceMode::Poly,
            oscillator: OscillatorType::Sine,
            envelope: EnvelopeConfig {
                attack: 0.001,
                decay: 0.0,
                sustain: 1.0,
                release: 0.1,
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
        };
        // Fresh voices are at unity gain.
        assert!((gain(&synth, 60) - 1.0).abs() < 0.01);
        assert!((gain(&synth, 64) - 1.0).abs() < 0.01);

        // Assignable per-note CC7 (the clip Gain-lane encoding) → half gain on 60.
        queue_midi(
            &synth,
            &[MidiEvent::per_note_controller(0, 1, 60, 7, 0x8000_0000, false)],
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
            sample_rate: 44100.0,
            max_voices: 4,
            voice_mode: VoiceMode::Poly,
            oscillator: OscillatorType::Sine,
            envelope: EnvelopeConfig {
                attack: 0.001,
                decay: 0.0,
                sustain: 1.0,
                release: 0.1,
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

        // Channel pressure on channel 1 only
        let pressure = ev_aftertouch(1, 127);
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
            "Channel 1 should have full pressure"
        );
        assert!(
            voice_ch2.mpe_state().pressure.abs() < 0.01,
            "Channel 2 should have no pressure"
        );
    }

    #[test]
    fn test_mpe_per_voice_slide() {
        let mut synth = synth(SynthConfig {
            sample_rate: 44100.0,
            max_voices: 4,
            voice_mode: VoiceMode::Poly,
            oscillator: OscillatorType::Saw,
            filter: FilterType::Moog {
                cutoff: 1000.0,
                resonance: 0.5,
            },
            envelope: EnvelopeConfig {
                attack: 0.001,
                decay: 0.0,
                sustain: 1.0,
                release: 0.1,
            },
            mpe_enabled: true,
            ..Default::default()
        });

        // Play note on channel 1
        let note1 = ev_note_on(1, 60, 100);
        queue_midi(&synth, &[note1]);
        let mut output = [0.0f32; 2];
        synth.tick(&[], &mut output);

        // CC74 (slide) on channel 1
        let slide = ev_cc(1, 74, 127);
        queue_midi(&synth, &[slide]);
        synth.tick(&[], &mut output);

        let voice = synth
            .voices
            .iter()
            .find(|v| v.is_active() && v.channel() == 1)
            .unwrap();
        assert!(
            (voice.mpe_state().slide - 1.0).abs() < 0.01,
            "Channel 1 should have full slide"
        );
    }

    #[test]
    fn test_mpe_disabled_global_pitch_bend() {
        let mut synth = synth(SynthConfig {
            sample_rate: 44100.0,
            max_voices: 4,
            voice_mode: VoiceMode::Poly,
            oscillator: OscillatorType::Sine,
            envelope: EnvelopeConfig {
                attack: 0.001,
                decay: 0.0,
                sustain: 1.0,
                release: 0.1,
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
            sample_rate: 44100.0,
            max_voices: 1,
            voice_mode: VoiceMode::Poly,
            oscillator: OscillatorType::Sine,
            envelope: EnvelopeConfig {
                attack: 0.001,
                decay: 0.0,
                sustain: 1.0,
                release: 0.1,
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

        // Apply full pressure
        let pressure = ev_aftertouch(1, 127);
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
            sample_rate: 44100.0,
            max_voices: 4,
            voice_mode: VoiceMode::Poly,
            oscillator: OscillatorType::Sine,
            envelope: EnvelopeConfig {
                attack: 0.001,
                decay: 0.0,
                sustain: 1.0,
                release: 0.05,
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
}
