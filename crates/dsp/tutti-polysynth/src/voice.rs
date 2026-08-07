//! Polyphonic voice allocator with stealing strategies, mono/legato modes,
//! and sustain/sostenuto pedal handling. RT-safe after construction.
//!
//! Voices are addressed by [`NoteId`] — the MIDI 2.0 per-note identity — not by
//! bare note number. Two notes that share a note number (same pitch on different
//! channels, or two same-pitch notes distinguished by per-note addressing on one
//! channel) therefore occupy independent voices. On the classic-MPE / MIDI-1 path
//! the id is `NoteId::from_channel_note(channel, note)`, so behaviour is unchanged;
//! native MIDI 2.0 per-note messages address the exact voice by the same id.

use tutti_midi_types::{NoteId, PerNoteMap};

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

/// Slide/timbre (CC74) center: 0.5 is "no timbre shift". The modulation math
/// keys off this center (`4^(slide - SLIDE_CENTER)`), so both `Default` and
/// `reset` must land here — a note with no CC74 must not shift its filter.
pub(crate) const SLIDE_CENTER: f32 = 0.5;

/// Per-voice MPE expression state.
#[derive(Debug, Clone, Copy)]
pub struct MpeVoiceState {
    /// Per-note pitch bend in semitones (range: -48..+48).
    pub pitch_bend_semitones: tutti_core::Semitones,
    /// Per-note pressure (0.0..1.0), from channel pressure.
    pub pressure: f32,
    /// Per-note slide/timbre (0.0..1.0), from CC74. Centered at [`SLIDE_CENTER`].
    pub slide: f32,
    /// Per-note gain (0.0..1.0), from per-note Volume (CC7). `1.0` is unity.
    pub gain: f32,
    /// Detached (M2-104 §7.4.5, Per-Note Management D=1): once set, this voice
    /// keeps its current per-note controller values but stops responding to any
    /// further per-note controllers — the note plays out frozen. Distinct from
    /// Reset (S), which snaps values back to defaults but keeps responding.
    pub detached: bool,
}

impl Default for MpeVoiceState {
    fn default() -> Self {
        Self {
            pitch_bend_semitones: tutti_core::Semitones(0.0),
            pressure: 0.0,
            slide: SLIDE_CENTER,
            gain: 1.0,
            detached: false,
        }
    }
}

impl MpeVoiceState {
    /// Reset (S): controllers back to defaults; the voice keeps responding.
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Detach (D): stop responding to further per-note controllers, but hold the
    /// current values until the note ends.
    pub fn detach(&mut self) {
        self.detached = true;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AllocationStrategy {
    #[default]
    Oldest,
    Quietest,
    HighestNote,
    LowestNote,
    Newest,
    NoSteal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VoiceMode {
    #[default]
    Poly,
    Mono,
    Legato,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum VoiceState {
    #[default]
    Idle,
    Active,
    Releasing,
    Stolen,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct VoiceSlot {
    voice_id: VoiceId,
    /// MIDI 2.0 per-note identity for this voice (the allocation key).
    id: NoteId,
    note: u8,
    channel: u8,
    velocity: f32,
    envelope_level: f32,
    start_time: u64,
    state: VoiceState,
    key_held: bool,
    sustained: bool,
    sostenuto_held: bool,
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
    pub(crate) fn note(&self) -> u8 {
        self.note
    }

    #[inline]
    pub(crate) fn channel(&self) -> u8 {
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
    pub(crate) fn is_key_held(&self) -> bool {
        self.key_held
    }

    #[inline]
    pub(crate) fn is_sustained(&self) -> bool {
        self.sustained
    }

    #[inline]
    pub(crate) fn is_sostenuto_held(&self) -> bool {
        self.sostenuto_held
    }

    /// Sets all fields for a new voice activation.
    pub(crate) fn activate(
        &mut self,
        voice_id: VoiceId,
        id: NoteId,
        note: u8,
        channel: u8,
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
        self.state = VoiceState::Active;
        self.key_held = true;
        self.sustained = false;
        self.sostenuto_held = false;
    }

    pub(crate) fn set_state(&mut self, state: VoiceState) {
        self.state = state;
    }

    pub(crate) fn set_key_held(&mut self, held: bool) {
        self.key_held = held;
    }

    pub(crate) fn set_sustained(&mut self, sustained: bool) {
        self.sustained = sustained;
    }

    pub(crate) fn set_sostenuto_held(&mut self, held: bool) {
        self.sostenuto_held = held;
    }

    pub(crate) fn set_envelope_level(&mut self, level: f32) {
        self.envelope_level = level;
    }

    /// Update identity, note and velocity for legato retrigger (the slot keeps
    /// sounding but now represents a new note, hence a new [`NoteId`]).
    pub(crate) fn update_note(&mut self, id: NoteId, note: u8, velocity: f32) {
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

/// Score a voice slot for stealing. Lower score = better steal candidate.
/// Returns (priority, tiebreaker) where priority 0 = releasing/stolen, 1 = active.
/// The tiebreaker depends on the allocation strategy.
fn steal_score(slot: &VoiceSlot, strategy: AllocationStrategy) -> Option<(u8, u64)> {
    match slot.state() {
        VoiceState::Releasing | VoiceState::Stolen => {
            // Priority 0: prefer releasing/stolen. Tiebreak by lowest envelope.
            Some((0, slot.envelope_level().to_bits() as u64))
        }
        VoiceState::Active => {
            let tiebreaker = match strategy {
                AllocationStrategy::Oldest => slot.start_time(),
                AllocationStrategy::Newest => u64::MAX - slot.start_time(),
                AllocationStrategy::Quietest => slot.envelope_level().to_bits() as u64,
                AllocationStrategy::HighestNote => u64::MAX - u64::from(slot.note()),
                AllocationStrategy::LowestNote => u64::from(slot.note()),
                AllocationStrategy::NoSteal => return None,
            };
            Some((1, tiebreaker))
        }
        VoiceState::Idle => None,
    }
}

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
    legato_last_note: Option<u8>,
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
        note: u8,
        channel: u8,
        velocity: f32,
    ) -> AllocationResult {
        if self.config.mode != VoiceMode::Poly {
            return self.allocate_mono_legato(id, note, channel, velocity);
        }

        // Same identity retriggering: release the prior voice for this exact id.
        if let Some(existing_slot) = self.id_to_slot.get(id).copied() {
            self.slots[existing_slot].set_state(VoiceState::Releasing);
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
            self.slots[slot_index].set_state(VoiceState::Stolen);

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

    /// Process a note-off for `id`. Returns the slot whose voice should now be
    /// gated off (the note actually stopped sounding), or `None` when the note
    /// is held by sustain/sostenuto or no live voice matched `id`. The caller
    /// gates the returned slot's voice directly — no note/channel re-scan.
    pub fn release(&mut self, id: NoteId, channel: u8) -> Option<usize> {
        let mut released = None;
        if let Some(slot_index) = self.id_to_slot.get(id).copied() {
            let slot = &mut self.slots[slot_index];

            if slot.channel() == channel && slot.state() == VoiceState::Active {
                slot.set_key_held(false);
                if self.sustain_pedal[usize::from(channel)] {
                    slot.set_sustained(true);
                } else if slot.is_sostenuto_held() {
                } else {
                    slot.set_state(VoiceState::Releasing);
                    self.id_to_slot.remove(id);
                    released = Some(slot_index);
                }
            }
        }

        if self.config.mode == VoiceMode::Legato && self.legato_last_note == Some(id.note_number())
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
                self.slots[i].set_state(VoiceState::Idle);
                break;
            }
        }
    }

    pub fn sustain_pedal(&mut self, channel: u8, on: bool) {
        if channel >= 16 {
            return;
        }

        self.sustain_pedal[usize::from(channel)] = on;

        if !on {
            for i in 0..self.slots.len() {
                let slot = &mut self.slots[i];
                if slot.channel() == channel && slot.is_sustained() {
                    slot.set_sustained(false);
                    if slot.state() == VoiceState::Active
                        && !slot.is_sostenuto_held()
                        && !slot.is_key_held()
                    {
                        slot.set_state(VoiceState::Releasing);
                        let id = slot.id();
                        self.id_to_slot.remove(id);
                    }
                }
            }
        }
    }

    pub fn sostenuto_pedal(&mut self, channel: u8, on: bool) {
        if channel >= 16 {
            return;
        }

        self.sostenuto_pedal[usize::from(channel)] = on;

        if on {
            for slot in &mut self.slots {
                if slot.channel() == channel && slot.state() == VoiceState::Active {
                    slot.set_sostenuto_held(true);
                }
            }
        } else {
            for i in 0..self.slots.len() {
                let slot = &mut self.slots[i];
                if slot.channel() == channel && slot.is_sostenuto_held() {
                    slot.set_sostenuto_held(false);
                    if slot.state() == VoiceState::Active
                        && !slot.is_sustained()
                        && !slot.is_key_held()
                    {
                        slot.set_state(VoiceState::Releasing);
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
        self.slots
            .iter()
            .filter(|s| s.state() != VoiceState::Idle)
            .count()
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

    pub fn all_notes_off(&mut self, channel: u8) {
        for i in 0..self.slots.len() {
            let slot = &mut self.slots[i];
            if slot.channel() == channel && slot.state() == VoiceState::Active {
                slot.set_state(VoiceState::Releasing);
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

    pub fn all_sound_off(&mut self, channel: u8) {
        for i in 0..self.slots.len() {
            if self.slots[i].channel() == channel {
                let id = self.slots[i].id();
                self.id_to_slot.remove(id);
                let slot = &mut self.slots[i];
                slot.set_state(VoiceState::Idle);
                slot.set_sustained(false);
                slot.set_sostenuto_held(false);
            }
        }
    }

    fn find_idle_slot(&self) -> Option<usize> {
        self.slots
            .iter()
            .position(|s| s.state() == VoiceState::Idle)
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
        note: u8,
        channel: u8,
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
        note: u8,
        channel: u8,
        velocity: f32,
    ) -> AllocationResult {
        let active_slot = self
            .slots
            .iter()
            .position(|s| s.state() == VoiceState::Active && s.channel() == channel);

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
                    self.slots[slot_index].set_state(VoiceState::Releasing);
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
mod tests {
    use super::*;

    #[test]
    fn test_basic_allocation() {
        let config = VoiceAllocatorConfig {
            max_voices: 4,
            ..Default::default()
        };
        let mut alloc = VoiceAllocator::new(config);

        // Allocate first note
        let result = alloc.allocate(NoteId::from_channel_note(0, 60), 60, 0, 0.8);
        assert!(matches!(
            result,
            AllocationResult::Allocated { slot_index: 0 }
        ));
        assert_eq!(alloc.active_count(), 1);

        // Allocate second note
        let result = alloc.allocate(NoteId::from_channel_note(0, 64), 64, 0, 0.7);
        assert!(matches!(
            result,
            AllocationResult::Allocated { slot_index: 1 }
        ));
        assert_eq!(alloc.active_count(), 2);
    }

    #[test]
    fn test_voice_stealing_oldest() {
        let config = VoiceAllocatorConfig {
            max_voices: 2,
            strategy: AllocationStrategy::Oldest,
            ..Default::default()
        };
        let mut alloc = VoiceAllocator::new(config);

        // Fill all voices
        alloc.allocate(NoteId::from_channel_note(0, 60), 60, 0, 0.8);
        alloc.advance_time(100);
        alloc.allocate(NoteId::from_channel_note(0, 64), 64, 0, 0.7);
        alloc.advance_time(100);

        // Third note should steal the oldest (note 60)
        let result = alloc.allocate(NoteId::from_channel_note(0, 67), 67, 0, 0.9);
        assert!(matches!(result, AllocationResult::Stolen { slot_index: 0 }));
    }

    #[test]
    fn test_release() {
        let config = VoiceAllocatorConfig::default();
        let mut alloc = VoiceAllocator::new(config);

        alloc.allocate(NoteId::from_channel_note(0, 60), 60, 0, 0.8);
        assert_eq!(alloc.active_count(), 1);

        alloc.release(NoteId::from_channel_note(0, 60), 0);
        // Voice should be releasing, not idle
        assert!(alloc.slots()[0].state() == VoiceState::Releasing);

        // After voice_finished, should be idle
        let voice_id = alloc.slots()[0].voice_id();
        alloc.voice_finished(voice_id);
        assert!(alloc.slots()[0].state() == VoiceState::Idle);
    }

    #[test]
    fn test_sustain_pedal() {
        let config = VoiceAllocatorConfig::default();
        let mut alloc = VoiceAllocator::new(config);

        alloc.allocate(NoteId::from_channel_note(0, 60), 60, 0, 0.8);
        alloc.sustain_pedal(0, true);
        alloc.release(NoteId::from_channel_note(0, 60), 0);

        // Should be sustained, not releasing
        assert!(alloc.slots()[0].is_sustained());
        assert_eq!(alloc.slots()[0].state(), VoiceState::Active);

        // Pedal off should release
        alloc.sustain_pedal(0, false);
        assert!(!alloc.slots()[0].is_sustained());
        assert_eq!(alloc.slots()[0].state(), VoiceState::Releasing);
    }

    #[test]
    fn test_mono_mode() {
        let config = VoiceAllocatorConfig {
            max_voices: 4,
            mode: VoiceMode::Mono,
            ..Default::default()
        };
        let mut alloc = VoiceAllocator::new(config);

        let result1 = alloc.allocate(NoteId::from_channel_note(0, 60), 60, 0, 0.8);
        assert!(matches!(
            result1,
            AllocationResult::Allocated { slot_index: 0 }
        ));
        assert_eq!(alloc.active_count(), 1);
        assert_eq!(alloc.slots()[0].note(), 60);

        // Second note should retrigger in slot 0 (mono = always slot 0)
        let result2 = alloc.allocate(NoteId::from_channel_note(0, 64), 64, 0, 0.7);
        assert!(matches!(
            result2,
            AllocationResult::Allocated { slot_index: 0 }
        ));
        assert_eq!(alloc.active_count(), 1);
        assert_eq!(alloc.slots()[0].note(), 64);

        // Note mapping should be updated
        assert!(alloc
            .id_to_slot
            .get(NoteId::from_channel_note(0, 60))
            .is_none());
        assert_eq!(
            alloc
                .id_to_slot
                .get(NoteId::from_channel_note(0, 64))
                .copied(),
            Some(0)
        );
    }

    #[test]
    fn test_legato_mode() {
        let config = VoiceAllocatorConfig {
            max_voices: 4,
            mode: VoiceMode::Legato,
            ..Default::default()
        };
        let mut alloc = VoiceAllocator::new(config);

        // First note triggers normally
        let result1 = alloc.allocate(NoteId::from_channel_note(0, 60), 60, 0, 0.8);
        assert!(matches!(result1, AllocationResult::Allocated { .. }));

        // Second note should be legato (no retrigger)
        let result2 = alloc.allocate(NoteId::from_channel_note(0, 64), 64, 0, 0.7);
        assert!(matches!(result2, AllocationResult::LegatoRetrigger { .. }));

        // Should still have only 1 active voice
        assert_eq!(alloc.active_count(), 1);
        assert_eq!(alloc.slots()[0].note(), 64);
    }

    #[test]
    fn test_no_steal() {
        let config = VoiceAllocatorConfig {
            max_voices: 2,
            strategy: AllocationStrategy::NoSteal,
            ..Default::default()
        };
        let mut alloc = VoiceAllocator::new(config);

        alloc.allocate(NoteId::from_channel_note(0, 60), 60, 0, 0.8);
        alloc.allocate(NoteId::from_channel_note(0, 64), 64, 0, 0.7);

        // Third note should fail
        let result = alloc.allocate(NoteId::from_channel_note(0, 67), 67, 0, 0.9);
        assert!(matches!(result, AllocationResult::Unavailable));
    }

    #[test]
    fn test_voice_stealing_quietest() {
        let config = VoiceAllocatorConfig {
            max_voices: 2,
            strategy: AllocationStrategy::Quietest,
            ..Default::default()
        };
        let mut alloc = VoiceAllocator::new(config);

        // Allocate two voices
        let result1 = alloc.allocate(NoteId::from_channel_note(0, 60), 60, 0, 0.8);
        let slot_1 = match result1 {
            AllocationResult::Allocated { slot_index } => slot_index,
            _ => panic!("Expected allocation"),
        };

        let result2 = alloc.allocate(NoteId::from_channel_note(0, 64), 64, 0, 0.7);
        let slot_2 = match result2 {
            AllocationResult::Allocated { slot_index } => slot_index,
            _ => panic!("Expected allocation"),
        };

        // Set envelope levels - slot 0 is quieter
        alloc.update_envelope_level(slot_1, 0.2);
        alloc.update_envelope_level(slot_2, 0.8);

        // Third note should steal the quietest (slot 0, note 60)
        let result = alloc.allocate(NoteId::from_channel_note(0, 67), 67, 0, 0.9);
        match result {
            AllocationResult::Stolen { slot_index } => {
                assert_eq!(slot_index, 0);
            }
            _ => panic!("Expected stealing, got {:?}", result),
        }
    }

    #[test]
    fn test_voice_stealing_highest_note() {
        let config = VoiceAllocatorConfig {
            max_voices: 2,
            strategy: AllocationStrategy::HighestNote,
            ..Default::default()
        };
        let mut alloc = VoiceAllocator::new(config);

        // Allocate C4 (60) and G4 (67)
        alloc.allocate(NoteId::from_channel_note(0, 60), 60, 0, 0.8); // slot 0
        alloc.allocate(NoteId::from_channel_note(0, 67), 67, 0, 0.8); // slot 1 - higher note

        // Third note should steal the highest (G4 = 67)
        let result = alloc.allocate(NoteId::from_channel_note(0, 72), 72, 0, 0.9);
        match result {
            AllocationResult::Stolen { slot_index } => {
                assert_eq!(slot_index, 1, "Should steal slot with highest note (67)");
            }
            _ => panic!("Expected stealing"),
        }
    }

    #[test]
    fn test_voice_stealing_lowest_note() {
        let config = VoiceAllocatorConfig {
            max_voices: 2,
            strategy: AllocationStrategy::LowestNote,
            ..Default::default()
        };
        let mut alloc = VoiceAllocator::new(config);

        // Allocate C4 (60) and G4 (67)
        alloc.allocate(NoteId::from_channel_note(0, 60), 60, 0, 0.8); // slot 0 - lower note
        alloc.allocate(NoteId::from_channel_note(0, 67), 67, 0, 0.8); // slot 1

        // Third note should steal the lowest (C4 = 60)
        let result = alloc.allocate(NoteId::from_channel_note(0, 72), 72, 0, 0.9);
        match result {
            AllocationResult::Stolen { slot_index } => {
                assert_eq!(slot_index, 0, "Should steal slot with lowest note (60)");
            }
            _ => panic!("Expected stealing"),
        }
    }

    #[test]
    fn test_voice_stealing_newest() {
        let config = VoiceAllocatorConfig {
            max_voices: 2,
            strategy: AllocationStrategy::Newest,
            ..Default::default()
        };
        let mut alloc = VoiceAllocator::new(config);

        alloc.allocate(NoteId::from_channel_note(0, 60), 60, 0, 0.8); // slot 0 - older
        alloc.advance_time(100);
        alloc.allocate(NoteId::from_channel_note(0, 64), 64, 0, 0.7); // slot 1 - newer
        alloc.advance_time(100);

        // Third note should steal the newest (note 64 in slot 1)
        let result = alloc.allocate(NoteId::from_channel_note(0, 67), 67, 0, 0.9);
        match result {
            AllocationResult::Stolen { slot_index } => {
                assert_eq!(slot_index, 1, "Should steal newest voice");
            }
            _ => panic!("Expected stealing"),
        }
    }

    #[test]
    fn test_sostenuto_pedal() {
        let config = VoiceAllocatorConfig::default();
        let mut alloc = VoiceAllocator::new(config);

        // Play note, then press sostenuto
        alloc.allocate(NoteId::from_channel_note(0, 60), 60, 0, 0.8);
        alloc.sostenuto_pedal(0, true);

        // Release key - should be held by sostenuto
        alloc.release(NoteId::from_channel_note(0, 60), 0);
        assert_eq!(alloc.slots()[0].state(), VoiceState::Active);
        assert!(alloc.slots()[0].is_sostenuto_held());

        // New note played AFTER sostenuto down should NOT be held
        alloc.allocate(NoteId::from_channel_note(0, 64), 64, 0, 0.7);
        assert!(
            !alloc.slots()[1].is_sostenuto_held(),
            "New notes should not be sostenuto-held"
        );
        alloc.release(NoteId::from_channel_note(0, 64), 0);
        assert_eq!(
            alloc.slots()[1].state(),
            VoiceState::Releasing,
            "New note should release normally"
        );

        // Original note still held
        assert_eq!(alloc.slots()[0].state(), VoiceState::Active);

        // Sostenuto off should release the held note (key was released)
        alloc.sostenuto_pedal(0, false);
        assert!(!alloc.slots()[0].is_sostenuto_held());
        assert_eq!(alloc.slots()[0].state(), VoiceState::Releasing);
    }

    #[test]
    fn test_sostenuto_key_still_held() {
        let config = VoiceAllocatorConfig::default();
        let mut alloc = VoiceAllocator::new(config);

        // Play note, then press sostenuto
        alloc.allocate(NoteId::from_channel_note(0, 60), 60, 0, 0.8);
        alloc.sostenuto_pedal(0, true);

        // DON'T release key — sostenuto off should NOT release
        alloc.sostenuto_pedal(0, false);
        assert_eq!(alloc.slots()[0].state(), VoiceState::Active);
        assert!(!alloc.slots()[0].is_sostenuto_held());
        assert!(alloc.slots()[0].is_key_held());
    }

    #[test]
    fn test_all_notes_off() {
        let config = VoiceAllocatorConfig::default();
        let mut alloc = VoiceAllocator::new(config);

        // Play notes on channel 0 and 1
        alloc.allocate(NoteId::from_channel_note(0, 60), 60, 0, 0.8);
        alloc.allocate(NoteId::from_channel_note(0, 64), 64, 0, 0.7);
        alloc.allocate(NoteId::from_channel_note(1, 67), 67, 1, 0.9);

        assert_eq!(alloc.active_count(), 3);

        // All notes off on channel 0 only
        alloc.all_notes_off(0);

        // Channel 0 notes should be releasing
        assert_eq!(alloc.slots()[0].state(), VoiceState::Releasing);
        assert_eq!(alloc.slots()[1].state(), VoiceState::Releasing);
        // Channel 1 note still active
        assert_eq!(alloc.slots()[2].state(), VoiceState::Active);
    }

    #[test]
    fn test_all_sound_off() {
        let config = VoiceAllocatorConfig::default();
        let mut alloc = VoiceAllocator::new(config);

        // Play notes
        alloc.allocate(NoteId::from_channel_note(0, 60), 60, 0, 0.8);
        alloc.allocate(NoteId::from_channel_note(0, 64), 64, 0, 0.7);

        // All sound off - immediate silence
        alloc.all_sound_off(0);

        // Should be immediately idle (no release phase)
        assert_eq!(alloc.slots()[0].state(), VoiceState::Idle);
        assert_eq!(alloc.slots()[1].state(), VoiceState::Idle);
        assert_eq!(alloc.active_count(), 0);
    }

    #[test]
    fn test_reset() {
        let config = VoiceAllocatorConfig::default();
        let mut alloc = VoiceAllocator::new(config);

        // Play notes with pedals
        alloc.allocate(NoteId::from_channel_note(0, 60), 60, 0, 0.8);
        alloc.allocate(NoteId::from_channel_note(0, 64), 64, 0, 0.7);
        alloc.sustain_pedal(0, true);
        alloc.sostenuto_pedal(0, true);

        assert_eq!(alloc.active_count(), 2);

        // Reset everything
        alloc.reset();

        assert_eq!(alloc.active_count(), 0);
        // Pedals should be cleared
        alloc.allocate(NoteId::from_channel_note(0, 60), 60, 0, 0.8);
        alloc.release(NoteId::from_channel_note(0, 60), 0);
        // Without sustain, should go to releasing
        assert_eq!(alloc.slots()[0].state(), VoiceState::Releasing);
    }

    #[test]
    fn test_retrigger_same_note() {
        let config = VoiceAllocatorConfig::default();
        let mut alloc = VoiceAllocator::new(config);

        // Play same note twice
        alloc.allocate(NoteId::from_channel_note(0, 60), 60, 0, 0.8);
        assert_eq!(alloc.slots()[0].state(), VoiceState::Active);

        // Same note again should release old and allocate new
        let result = alloc.allocate(NoteId::from_channel_note(0, 60), 60, 0, 0.9);
        match result {
            AllocationResult::Allocated { slot_index } => {
                // Old voice should be releasing, new one allocated
                assert!(slot_index == 0 || slot_index == 1);
            }
            _ => panic!("Expected new allocation"),
        }
    }

    #[test]
    fn test_prefer_stealing_releasing_voices() {
        let config = VoiceAllocatorConfig {
            max_voices: 2,
            strategy: AllocationStrategy::Oldest,
            ..Default::default()
        };
        let mut alloc = VoiceAllocator::new(config);

        // Allocate and release one voice
        alloc.allocate(NoteId::from_channel_note(0, 60), 60, 0, 0.8);
        alloc.advance_time(100);
        alloc.allocate(NoteId::from_channel_note(0, 64), 64, 0, 0.7);
        alloc.advance_time(100);
        alloc.release(NoteId::from_channel_note(0, 60), 0); // Now slot 0 is releasing

        // New note should prefer the releasing voice over active
        let result = alloc.allocate(NoteId::from_channel_note(0, 67), 67, 0, 0.9);
        match result {
            AllocationResult::Stolen { slot_index } => {
                assert_eq!(slot_index, 0, "Should prefer stealing releasing voice");
            }
            _ => panic!("Expected stealing"),
        }
    }

    #[test]
    fn test_steal_score_idle_returns_none() {
        let slot = VoiceSlot::default();
        assert!(steal_score(&slot, AllocationStrategy::Oldest).is_none());
    }

    #[test]
    fn test_steal_score_releasing_lower_priority_than_active() {
        let mut releasing = VoiceSlot::default();
        releasing.activate(
            VoiceId::new(1),
            NoteId::from_channel_note(0, 60),
            60,
            0,
            0.8,
            0,
        );
        releasing.set_state(VoiceState::Releasing);

        let mut active = VoiceSlot::default();
        active.activate(
            VoiceId::new(2),
            NoteId::from_channel_note(0, 64),
            64,
            0,
            0.7,
            100,
        );

        let r_score = steal_score(&releasing, AllocationStrategy::Oldest).unwrap();
        let a_score = steal_score(&active, AllocationStrategy::Oldest).unwrap();
        assert!(
            r_score < a_score,
            "Releasing should be preferred over active"
        );
    }

    #[test]
    fn test_steal_score_oldest_strategy() {
        let mut old = VoiceSlot::default();
        old.activate(
            VoiceId::new(1),
            NoteId::from_channel_note(0, 60),
            60,
            0,
            0.8,
            100,
        );

        let mut new = VoiceSlot::default();
        new.activate(
            VoiceId::new(2),
            NoteId::from_channel_note(0, 64),
            64,
            0,
            0.7,
            200,
        );

        let old_score = steal_score(&old, AllocationStrategy::Oldest).unwrap();
        let new_score = steal_score(&new, AllocationStrategy::Oldest).unwrap();
        assert!(
            old_score < new_score,
            "Oldest voice should have lower score"
        );
    }

    #[test]
    fn test_steal_score_no_steal_returns_none() {
        let mut slot = VoiceSlot::default();
        slot.activate(
            VoiceId::new(1),
            NoteId::from_channel_note(0, 60),
            60,
            0,
            0.8,
            0,
        );
        assert!(steal_score(&slot, AllocationStrategy::NoSteal).is_none());
    }
}
