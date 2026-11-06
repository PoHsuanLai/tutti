//! Polyphonic voice allocator with stealing strategies, mono/legato modes,
//! and sustain/sostenuto pedal handling. RT-safe after construction.
//!
//! Voices are addressed by [`NoteId`] — the MIDI 2.0 per-note identity — not by
//! bare note number. Two notes that share a note number (same pitch on different
//! channels, or two same-pitch notes distinguished by per-note addressing on one
//! channel) therefore occupy independent voices. On the classic-MPE / MIDI-1 path
//! the id is `NoteId::from_channel_note(channel, note)`, so behaviour is unchanged;
//! native MIDI 2.0 per-note messages address the exact voice by the same id.

use tutti_midi_types::midi2::num::u7;
use tutti_midi_types::{MidiChannel, NoteId, PerNoteMap};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct VoiceId(u64);

impl VoiceId {
    pub const fn new(id: u64) -> Self {
        Self(id)
    }

    fn next(self) -> Self {
        Self(self.0 + 1)
    }
}

/// Which sounding voice gets taken when every slot is busy and another note
/// arrives.
///
/// Only the tiebreak among *active* voices differs between these. A voice that
/// is already releasing or stolen is always preferred over any active one
/// whatever the strategy says, and among those the quietest goes first — so a
/// chord fading out is consumed before a held note is cut off.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum AllocationStrategy {
    /// Take the voice that started longest ago. The default, and the safe
    /// general choice: it cuts the note the player is least likely to still be
    /// listening for.
    #[default]
    Oldest,
    /// Take the voice with the lowest current envelope level — the least
    /// audible cut, but it can steal a held note that happens to sit in a decay
    /// dip.
    Quietest,
    /// Take the highest-pitched sounding note. Preserves bass lines.
    HighestNote,
    /// Take the lowest-pitched sounding note. Preserves melody.
    LowestNote,
    /// Take the most recently started voice, so a held chord survives a run of
    /// fast notes at the cost of dropping the new ones.
    Newest,
    /// Steal nothing: the note is dropped and never sounds. The allocator
    /// returns `Unavailable` and no voice is disturbed.
    NoSteal,
}

/// How many notes may sound at once, and whether a new note restarts the
/// envelope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum VoiceMode {
    /// Polyphonic: notes sound simultaneously up to `max_voices`, then the
    /// [`AllocationStrategy`] decides. The default.
    #[default]
    Poly,
    /// Monophonic: one note at a time on a channel. A new note releases the
    /// sounding one and retriggers the envelope from attack. Only slot 0 is
    /// used, however large `max_voices` is.
    Mono,
    /// Monophonic with legato: a new note while one is already sounding takes
    /// over the same voice without retriggering the envelope, so the note
    /// glides or steps in at the envelope's current level. The first note of a
    /// phrase (nothing sounding) triggers normally. This is what makes
    /// [`PortamentoMode::LegatoOnly`](crate::PortamentoMode::LegatoOnly)
    /// meaningful.
    Legato,
}

/// What holds a sounding slot open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Holds {
    key: bool,
    sustain: bool,
    sostenuto: bool,
}

/// Where a slot is in its life. Holds exist only while it is active.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum VoiceState {
    #[default]
    Idle,
    Active(Holds),
    Releasing,
}

#[derive(Debug, Clone)]
pub(crate) struct VoiceSlot {
    voice_id: VoiceId,
    /// MIDI 2.0 per-note identity for this voice (the allocation key).
    id: NoteId,
    note: u7,
    channel: MidiChannel,
    velocity: f32,
    envelope_level: f32,
    start_time: u64,
    state: VoiceState,
}

impl Default for VoiceSlot {
    fn default() -> Self {
        Self {
            voice_id: VoiceId::default(),
            id: NoteId::default(),
            note: u7::new(0),
            channel: MidiChannel::FIRST,
            velocity: 0.0,
            envelope_level: 0.0,
            start_time: 0,
            state: VoiceState::Idle,
        }
    }
}

impl VoiceSlot {
    #[inline]
    pub(crate) fn voice_id(&self) -> VoiceId {
        self.voice_id
    }

    #[inline]
    pub(crate) fn id(&self) -> NoteId {
        self.id
    }

    #[inline]
    pub(crate) fn note(&self) -> u7 {
        self.note
    }

    #[inline]
    pub(crate) fn channel(&self) -> MidiChannel {
        self.channel
    }

    #[inline]
    pub(crate) fn envelope_level(&self) -> f32 {
        self.envelope_level
    }

    #[inline]
    pub(crate) fn start_time(&self) -> u64 {
        self.start_time
    }

    #[inline]
    pub(crate) fn state(&self) -> VoiceState {
        self.state
    }

    #[inline]
    pub(crate) fn is_idle(&self) -> bool {
        self.state == VoiceState::Idle
    }

    #[inline]
    pub(crate) fn is_releasing(&self) -> bool {
        self.state == VoiceState::Releasing
    }

    #[inline]
    pub(crate) fn is_active(&self) -> bool {
        matches!(self.state, VoiceState::Active(_))
    }

    #[inline]
    pub(crate) fn is_key_held(&self) -> bool {
        self.holds().is_some_and(|h| h.key)
    }

    #[inline]
    pub(crate) fn is_sustained(&self) -> bool {
        self.holds().is_some_and(|h| h.sustain)
    }

    #[inline]
    pub(crate) fn is_sostenuto_held(&self) -> bool {
        self.holds().is_some_and(|h| h.sostenuto)
    }

    fn holds(&self) -> Option<Holds> {
        match self.state {
            VoiceState::Active(h) => Some(h),
            _ => None,
        }
    }

    /// Edits the holds of an active slot; any other slot has none.
    fn hold(&mut self, f: impl FnOnce(&mut Holds)) {
        if let VoiceState::Active(h) = &mut self.state {
            f(h);
        }
    }

    /// Starts a new voice in this slot, key held and nothing else.
    pub(crate) fn activate(
        &mut self,
        voice_id: VoiceId,
        id: NoteId,
        note: u7,
        channel: MidiChannel,
        velocity: f32,
        time: u64,
    ) {
        self.voice_id = voice_id;
        self.id = id;
        self.note = note;
        self.channel = channel;
        self.velocity = velocity;
        self.envelope_level = 0.0;
        self.start_time = time;
        self.state = VoiceState::Active(Holds {
            key: true,
            sustain: false,
            sostenuto: false,
        });
    }

    pub(crate) fn release(&mut self) {
        self.state = VoiceState::Releasing;
    }

    pub(crate) fn silence(&mut self) {
        self.state = VoiceState::Idle;
    }

    pub(crate) fn set_key_held(&mut self, held: bool) {
        self.hold(|h| h.key = held);
    }

    pub(crate) fn set_sustained(&mut self, sustained: bool) {
        self.hold(|h| h.sustain = sustained);
    }

    pub(crate) fn set_sostenuto_held(&mut self, held: bool) {
        self.hold(|h| h.sostenuto = held);
    }

    pub(crate) fn set_envelope_level(&mut self, level: f32) {
        self.envelope_level = level;
    }

    /// Update identity, note and velocity for legato retrigger (the slot keeps
    /// sounding but now represents a new note, hence a new [`NoteId`]).
    pub(crate) fn update_note(&mut self, id: NoteId, note: u7, velocity: f32) {
        self.id = id;
        self.note = note;
        self.velocity = velocity;
    }
}

#[derive(Debug, Clone)]
pub struct VoiceAllocatorConfig {
    pub max_voices: usize,
    pub strategy: AllocationStrategy,
    pub mode: VoiceMode,
}

impl Default for VoiceAllocatorConfig {
    fn default() -> Self {
        Self {
            max_voices: 16,
            strategy: AllocationStrategy::Oldest,
            mode: VoiceMode::Poly,
        }
    }
}

#[derive(Debug, Clone)]
pub enum AllocationResult {
    Allocated { slot_index: usize },
    Stolen { slot_index: usize },
    LegatoRetrigger { slot_index: usize },
    Unavailable,
}

impl AllocationResult {
    /// The slot the note landed in, if it got one.
    pub fn slot_index(&self) -> Option<usize> {
        match *self {
            Self::Allocated { slot_index }
            | Self::Stolen { slot_index }
            | Self::LegatoRetrigger { slot_index } => Some(slot_index),
            Self::Unavailable => None,
        }
    }
}

/// Score a voice slot for stealing. Lower score = better steal candidate.
/// Returns (priority, tiebreaker) where priority 0 = releasing, 1 = active.
/// The tiebreaker depends on the allocation strategy.
fn steal_score(slot: &VoiceSlot, strategy: AllocationStrategy) -> Option<(u8, u64)> {
    match slot.state() {
        VoiceState::Releasing => {
            // Priority 0: prefer releasing. Tiebreak by lowest envelope.
            Some((0, slot.envelope_level().to_bits() as u64))
        }
        VoiceState::Active(_) => {
            let tiebreaker = match strategy {
                AllocationStrategy::Oldest => slot.start_time(),
                AllocationStrategy::Newest => u64::MAX - slot.start_time(),
                AllocationStrategy::Quietest => slot.envelope_level().to_bits() as u64,
                AllocationStrategy::HighestNote => u64::MAX - u64::from(u8::from(slot.note())),
                AllocationStrategy::LowestNote => u64::from(u8::from(slot.note())),
                AllocationStrategy::NoSteal => return None,
            };
            Some((1, tiebreaker))
        }
        VoiceState::Idle => None,
    }
}

#[derive(Debug)]
pub struct VoiceAllocator {
    config: VoiceAllocatorConfig,
    slots: Vec<VoiceSlot>,
    next_voice_id: VoiceId,
    current_time: u64,
    /// [`NoteId`] → active slot index. Keyed by full per-note identity, so two
    /// notes sharing a note number map to distinct slots.
    id_to_slot: PerNoteMap<usize, 128>,
    sustain_pedal: [bool; 16],
    sostenuto_pedal: [bool; 16],
    legato_last_note: Option<u7>,
}

impl VoiceAllocator {
    pub fn new(config: VoiceAllocatorConfig) -> Self {
        let slots = (0..config.max_voices)
            .map(|_| VoiceSlot::default())
            .collect();

        Self {
            config,
            slots,
            next_voice_id: VoiceId::new(1),
            current_time: 0,
            id_to_slot: PerNoteMap::new(),
            sustain_pedal: [false; 16],
            sostenuto_pedal: [false; 16],
            legato_last_note: None,
        }
    }

    pub fn allocate(
        &mut self,
        id: NoteId,
        note: u7,
        channel: MidiChannel,
        velocity: f32,
    ) -> AllocationResult {
        if self.config.mode != VoiceMode::Poly {
            return self.allocate_mono_legato(id, note, channel, velocity);
        }

        // Same identity retriggering: release the prior voice for this exact id.
        if let Some(existing_slot) = self.id_to_slot.get(id).copied() {
            self.slots[existing_slot].release();
            self.id_to_slot.remove(id);
        }

        if let Some(slot_index) = self.find_idle_slot() {
            return self.activate_slot(slot_index, id, note, channel, velocity);
        }

        if self.config.strategy == AllocationStrategy::NoSteal {
            return AllocationResult::Unavailable;
        }

        if let Some(slot_index) = self.find_slot_to_steal() {
            let old_id = self.slots[slot_index].id();
            self.id_to_slot.remove(old_id);

            let voice_id = self.next_voice_id;
            self.next_voice_id = self.next_voice_id.next();

            self.slots[slot_index].activate(
                voice_id,
                id,
                note,
                channel,
                velocity,
                self.current_time,
            );

            self.id_to_slot.insert(id, slot_index);

            AllocationResult::Stolen { slot_index }
        } else {
            AllocationResult::Unavailable
        }
    }

    /// Processes a note-off for `id`. Returns the slot whose voice should now be
    /// gated off (the note actually stopped sounding), or `None` when the note
    /// is held by sustain/sostenuto or no live voice matched `id`. The caller
    /// gates the returned slot's voice directly — no note/channel re-scan.
    pub fn release(&mut self, id: NoteId, channel: MidiChannel) -> Option<usize> {
        let mut released = None;
        if let Some(slot_index) = self.id_to_slot.get(id).copied() {
            let slot = &mut self.slots[slot_index];

            if slot.channel() == channel && slot.is_active() {
                slot.set_key_held(false);
                if self.sustain_pedal[usize::from(channel.get())] {
                    slot.set_sustained(true);
                } else if !slot.is_sostenuto_held() {
                    slot.release();
                    self.id_to_slot.remove(id);
                    released = Some(slot_index);
                }
            }
        }

        if self.config.mode == VoiceMode::Legato
            && self.legato_last_note == Some(u7::new(id.note_number()))
        {
            self.legato_last_note = None;
        }

        released
    }

    /// Call when envelope reaches zero to free the slot for reuse.
    pub fn voice_finished(&mut self, voice_id: VoiceId) {
        for i in 0..self.slots.len() {
            if self.slots[i].voice_id() == voice_id {
                let id = self.slots[i].id();
                if self.id_to_slot.get(id).copied() == Some(i) {
                    self.id_to_slot.remove(id);
                }
                self.slots[i].silence();
                break;
            }
        }
    }

    pub fn sustain_pedal(&mut self, channel: MidiChannel, on: bool) {
        self.sustain_pedal[usize::from(channel.get())] = on;

        if !on {
            for i in 0..self.slots.len() {
                let slot = &mut self.slots[i];
                if slot.channel() == channel && slot.is_sustained() {
                    slot.set_sustained(false);
                    if slot.is_active() && !slot.is_sostenuto_held() && !slot.is_key_held() {
                        slot.release();
                        let id = slot.id();
                        self.id_to_slot.remove(id);
                    }
                }
            }
        }
    }

    pub fn sostenuto_pedal(&mut self, channel: MidiChannel, on: bool) {
        self.sostenuto_pedal[usize::from(channel.get())] = on;

        if on {
            for slot in &mut self.slots {
                if slot.channel() == channel && slot.is_active() {
                    slot.set_sostenuto_held(true);
                }
            }
        } else {
            for i in 0..self.slots.len() {
                let slot = &mut self.slots[i];
                if slot.channel() == channel && slot.is_sostenuto_held() {
                    slot.set_sostenuto_held(false);
                    if slot.is_active() && !slot.is_sustained() && !slot.is_key_held() {
                        slot.release();
                        let id = slot.id();
                        self.id_to_slot.remove(id);
                    }
                }
            }
        }
    }

    pub fn update_envelope_level(&mut self, slot_index: usize, level: f32) {
        if slot_index < self.slots.len() {
            self.slots[slot_index].set_envelope_level(level);
        }
    }

    pub fn advance_time(&mut self, samples: u64) {
        self.current_time = self.current_time.wrapping_add(samples);
    }

    #[cfg(test)]
    pub fn active_count(&self) -> usize {
        self.slots.iter().filter(|s| !s.is_idle()).count()
    }

    pub(crate) fn slots(&self) -> &[VoiceSlot] {
        &self.slots
    }

    pub fn reset(&mut self) {
        for slot in &mut self.slots {
            *slot = VoiceSlot::default();
        }
        self.id_to_slot = PerNoteMap::new();
        self.sustain_pedal = [false; 16];
        self.sostenuto_pedal = [false; 16];
        self.legato_last_note = None;
    }

    pub fn all_notes_off(&mut self, channel: MidiChannel) {
        for i in 0..self.slots.len() {
            let slot = &mut self.slots[i];
            if slot.channel() == channel && slot.is_active() {
                slot.release();
                let id = slot.id();
                self.id_to_slot.remove(id);
            }
            let slot = &mut self.slots[i];
            if slot.channel() == channel {
                slot.set_sustained(false);
                slot.set_sostenuto_held(false);
            }
        }
    }

    pub fn all_sound_off(&mut self, channel: MidiChannel) {
        for i in 0..self.slots.len() {
            if self.slots[i].channel() == channel {
                let id = self.slots[i].id();
                self.id_to_slot.remove(id);
                let slot = &mut self.slots[i];
                slot.silence();
                slot.set_sustained(false);
                slot.set_sostenuto_held(false);
            }
        }
    }

    fn find_idle_slot(&self) -> Option<usize> {
        self.slots.iter().position(|s| s.is_idle())
    }

    fn find_slot_to_steal(&self) -> Option<usize> {
        self.slots
            .iter()
            .enumerate()
            .filter_map(|(i, s)| steal_score(s, self.config.strategy).map(|score| (i, score)))
            .min_by_key(|(_, score)| *score)
            .map(|(i, _)| i)
    }

    fn activate_slot(
        &mut self,
        slot_index: usize,
        id: NoteId,
        note: u7,
        channel: MidiChannel,
        velocity: f32,
    ) -> AllocationResult {
        let voice_id = self.next_voice_id;
        self.next_voice_id = self.next_voice_id.next();

        self.slots[slot_index].activate(voice_id, id, note, channel, velocity, self.current_time);

        self.id_to_slot.insert(id, slot_index);

        AllocationResult::Allocated { slot_index }
    }

    fn allocate_mono_legato(
        &mut self,
        id: NoteId,
        note: u7,
        channel: MidiChannel,
        velocity: f32,
    ) -> AllocationResult {
        let active_slot = self
            .slots
            .iter()
            .position(|s| s.is_active() && s.channel() == channel);

        match (self.config.mode, active_slot, self.legato_last_note) {
            (VoiceMode::Legato, Some(slot_index), Some(_)) => {
                let old_id = self.slots[slot_index].id();
                self.id_to_slot.remove(old_id);
                self.slots[slot_index].update_note(id, note, velocity);
                self.id_to_slot.insert(id, slot_index);
                self.legato_last_note = Some(note);

                AllocationResult::LegatoRetrigger { slot_index }
            }
            _ => {
                if let Some(slot_index) = active_slot {
                    let old_id = self.slots[slot_index].id();
                    self.id_to_slot.remove(old_id);
                    self.slots[slot_index].release();
                }

                self.legato_last_note = Some(note);
                self.activate_slot(0, id, note, channel, velocity)
            }
        }
    }
}

impl Clone for VoiceAllocator {
    fn clone(&self) -> Self {
        Self {
            config: self.config.clone(),
            slots: self.slots.clone(),
            next_voice_id: self.next_voice_id,
            current_time: self.current_time,
            id_to_slot: self.id_to_slot.clone(),
            sustain_pedal: self.sustain_pedal,
            sostenuto_pedal: self.sostenuto_pedal,
            legato_last_note: self.legato_last_note,
        }
    }
}

#[cfg(test)]
mod tests;
