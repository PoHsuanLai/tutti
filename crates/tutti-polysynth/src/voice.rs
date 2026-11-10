//! Polyphonic voice allocator with stealing strategies, mono/legato modes,
//! and sustain/sostenuto pedal handling. RT-safe after construction.
//!
//! Voices are addressed by [`NoteId`] — the MIDI 2.0 per-note identity — not by
//! bare note number. Two notes that share a note number (same pitch on different
//! channels, or two same-pitch notes distinguished by per-note addressing on one
//! channel) therefore occupy independent voices. On the classic-MPE / MIDI-1 path
//! the id is `NoteId::from_channel_note(channel, note)`, so behaviour is unchanged;
//! native MIDI 2.0 per-note messages address the exact voice by the same id.

use crate::note_id::PerNoteMap;
use tutti_types::{Amplitude, Semitones};
use tutti_ump::Voice2;
use tutti_ump::{MidiChannel, NoteId, NoteNumber};
use tutti_voicing::{
    Command, Mode, Order, Patch, Retrigger, Rules, Steal, VoiceSpec, VoiceView, Voicing, Zones,
};

pub use tutti_voicing::VoiceId;

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

/// Where a slot is in its life. What holds an active voice open is the core's
/// to say (`VoiceAllocator::holds_of`), not copied here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum VoiceState {
    #[default]
    Idle,
    Active,
    Releasing,
}

#[derive(Debug, Clone)]
pub(crate) struct VoiceSlot {
    voice_id: VoiceId,
    /// MIDI 2.0 per-note identity for this voice (the allocation key).
    id: NoteId,
    note: NoteNumber,
    channel: MidiChannel,
    velocity: f32,
    envelope_level: f32,
    state: VoiceState,
}

impl Default for VoiceSlot {
    fn default() -> Self {
        Self {
            voice_id: VoiceId(0),
            id: NoteId::default(),
            note: NoteNumber::default(),
            channel: MidiChannel::FIRST,
            velocity: 0.0,
            envelope_level: 0.0,
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

    #[cfg(test)]
    pub(crate) fn note(&self) -> NoteNumber {
        self.note
    }

    #[cfg(test)]
    pub(crate) fn velocity(&self) -> f32 {
        self.velocity
    }

    #[cfg(test)]
    pub(crate) fn channel(&self) -> MidiChannel {
        self.channel
    }

    #[inline]
    pub(crate) fn envelope_level(&self) -> f32 {
        self.envelope_level
    }

    #[inline]
    pub(crate) fn is_idle(&self) -> bool {
        self.state == VoiceState::Idle
    }

    #[cfg(test)]
    #[inline]
    pub(crate) fn is_releasing(&self) -> bool {
        self.state == VoiceState::Releasing
    }

    #[cfg(test)]
    #[inline]
    pub(crate) fn is_active(&self) -> bool {
        self.state == VoiceState::Active
    }

    /// Starts a new voice in this slot, key held and nothing else.
    pub(crate) fn activate(
        &mut self,
        voice_id: VoiceId,
        id: NoteId,
        note: NoteNumber,
        channel: MidiChannel,
        velocity: f32,
    ) {
        self.voice_id = voice_id;
        self.id = id;
        self.note = note;
        self.channel = channel;
        self.velocity = velocity;
        self.envelope_level = 0.0;
        self.state = VoiceState::Active;
    }

    pub(crate) fn release(&mut self) {
        self.state = VoiceState::Releasing;
    }

    pub(crate) fn silence(&mut self) {
        self.state = VoiceState::Idle;
    }

    pub(crate) fn set_envelope_level(&mut self, level: f32) {
        self.envelope_level = level;
    }

    /// Update identity, note and velocity for legato retrigger (the slot keeps
    /// sounding but now represents a new note, hence a new [`NoteId`]).
    pub(crate) fn update_note(&mut self, id: NoteId, note: NoteNumber, velocity: f32) {
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
    /// What a full pitch bend spans on a channel until RPN 0 says otherwise.
    pub bend_range: Semitones,
}

impl Default for VoiceAllocatorConfig {
    fn default() -> Self {
        Self {
            max_voices: 16,
            strategy: AllocationStrategy::Oldest,
            mode: VoiceMode::Poly,
            bend_range: Semitones(2.0),
        }
    }
}

impl VoiceAllocatorConfig {
    /// This synth's rules: a value of the shared note logic.
    fn rules(&self) -> Rules {
        Rules {
            mode: match self.mode {
                VoiceMode::Poly => Mode::Poly,
                VoiceMode::Mono => Mode::Mono,
                VoiceMode::Legato => Mode::Legato,
            },
            retrigger: Retrigger::Replace,
            polyphony: self.max_voices,
            steal: match self.strategy {
                AllocationStrategy::NoSteal => Steal::Never,
                AllocationStrategy::Oldest => Steal::By(Order::Oldest),
                AllocationStrategy::Newest => Steal::By(Order::Newest),
                AllocationStrategy::Quietest => Steal::By(Order::Quietest),
                AllocationStrategy::HighestNote => Steal::By(Order::HighestNote),
                AllocationStrategy::LowestNote => Steal::By(Order::LowestNote),
            },
            exclusive_groups: false,
            sostenuto: true,
            bend_range: self.bend_range,
        }
    }
}

/// Where a note landed. The synth acts on the gates; this is what the
/// allocator's tests read.
#[derive(Debug, Clone)]
#[cfg_attr(not(test), expect(dead_code, reason = "read by the allocator's tests"))]
pub enum AllocationResult {
    Allocated { slot_index: usize },
    Stolen { slot_index: usize },
    LegatoRetrigger { slot_index: usize },
    Unavailable,
}

#[cfg_attr(not(test), expect(dead_code, reason = "read by the allocator's tests"))]
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

/// A note is one voice here (its unison copies are lanes of that voice).
struct OneVoice;

impl Patch for OneVoice {
    fn voices(&self, _: MidiChannel, _: NoteNumber, _: u16, spec: &mut dyn FnMut(VoiceSpec)) {
        spec(VoiceSpec {
            group: 0,
            scope: 0,
            tag: 0,
        });
    }
}

/// What a command asks the synth to do to a voice, after the slot table has
/// followed it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Gate {
    /// Let the voice at this slot go into its release.
    Off(usize),
    /// Silence the voice at this slot at once.
    Cut(usize),
    /// The channel's pitch moved: its notes sound this far from their keys.
    Retune(MidiChannel, Semitones),
    /// Start (or, `legato`, move) the voice at this slot to this note.
    Start {
        slot: usize,
        key: NoteNumber,
        channel: MidiChannel,
        velocity: f32,
        legato: bool,
    },
}

/// The synth's half of the note logic: the slot table. The decisions are
/// `tutti_voicing::Voicing`'s; this carries out its commands on slots and keeps
/// what only the synth knows (the per-note identity, the envelope level).
#[derive(Debug)]
pub struct VoiceAllocator {
    config: VoiceAllocatorConfig,
    core: Voicing,
    slots: Vec<VoiceSlot>,
    /// [`NoteId`] → active slot index. Keyed by full per-note identity, so two
    /// notes sharing a note number map to distinct slots.
    id_to_slot: PerNoteMap<usize, 128>,
    views: Vec<VoiceView>,
    commands: Vec<Command>,
    gates: Vec<Gate>,
}

impl VoiceAllocator {
    pub fn new(config: VoiceAllocatorConfig) -> Self {
        let rules = config.rules();
        let slots = (0..config.max_voices)
            .map(|_| VoiceSlot::default())
            .collect();
        Self {
            core: Voicing::new(rules),
            views: Vec::with_capacity(config.max_voices),
            commands: Vec::with_capacity(2 * config.max_voices + 64),
            gates: Vec::with_capacity(2 * config.max_voices + 64),
            config,
            slots,
            id_to_slot: PerNoteMap::new(),
        }
    }

    /// Runs `message` through the core and carries out what it returns.
    /// `strike` is the note being started, if the message starts one.
    fn run(
        &mut self,
        channel: MidiChannel,
        message: &Voice2,
        strike: Option<(NoteId, f32)>,
    ) -> AllocationResult {
        // Only a note message reads the voices (to forget the ones that ended,
        // to choose a victim, to return to a held key); every other message
        // passes none. Sorted by id.
        self.views.clear();
        if matches!(message, Voice2::NoteOn { .. } | Voice2::NoteOff { .. }) {
            for s in self.slots.iter().filter(|s| !s.is_idle()) {
                let at = self.views.partition_point(|v| v.id < s.voice_id());
                self.views.insert(
                    at,
                    VoiceView {
                        id: s.voice_id(),
                        level: Amplitude(s.envelope_level()),
                    },
                );
            }
        }
        let cmds = self.core.step(channel, message, &OneVoice, &self.views);
        self.commands.clear();
        self.commands.extend_from_slice(cmds);
        let mut result = AllocationResult::Unavailable;
        for i in 0..self.commands.len() {
            match self.commands[i] {
                Command::Start {
                    id,
                    channel,
                    key,
                    steal,
                    velocity: velocity16,
                    ..
                } => {
                    let (note_id, velocity) = Self::strike_of(strike, channel, key, velocity16);
                    let stolen = steal.and_then(|v| self.slot_of_voice(v));
                    let Some(slot_index) =
                        stolen.or_else(|| self.slots.iter().position(VoiceSlot::is_idle))
                    else {
                        continue;
                    };
                    if stolen.is_some() {
                        self.unmap(slot_index);
                    }
                    self.slots[slot_index].activate(id, note_id, key, channel, velocity);
                    self.id_to_slot.insert(note_id, slot_index);
                    self.gates.push(Gate::Start {
                        slot: slot_index,
                        key,
                        channel,
                        velocity,
                        legato: false,
                    });
                    result = if stolen.is_some() && self.config.mode == VoiceMode::Poly {
                        AllocationResult::Stolen { slot_index }
                    } else {
                        AllocationResult::Allocated { slot_index }
                    };
                }
                Command::Retarget {
                    id,
                    key,
                    velocity: velocity16,
                } => {
                    if let Some(slot_index) = self.slot_of_voice(id) {
                        let channel = self.slots[slot_index].channel;
                        let (note_id, velocity) = Self::strike_of(strike, channel, key, velocity16);
                        self.unmap(slot_index);
                        self.slots[slot_index].update_note(note_id, key, velocity);
                        self.id_to_slot.insert(note_id, slot_index);
                        self.gates.push(Gate::Start {
                            slot: slot_index,
                            key,
                            channel,
                            velocity,
                            legato: true,
                        });
                        result = AllocationResult::LegatoRetrigger { slot_index };
                    }
                }
                Command::Retune { channel, bend } => {
                    self.gates.push(Gate::Retune(channel, bend));
                }
                // `Damp` is an exclusive class's, which `PolySynth`'s rules
                // never turn on; if it came, its own release is the answer.
                Command::Release { id, .. } | Command::Damp(id) => {
                    if let Some(slot_index) = self.slot_of_voice(id) {
                        self.unmap(slot_index);
                        self.slots[slot_index].release();
                        self.gates.push(Gate::Off(slot_index));
                    }
                }
                Command::Cut(id) => {
                    if let Some(slot_index) = self.slot_of_voice(id) {
                        self.unmap(slot_index);
                        self.slots[slot_index].silence();
                        self.gates.push(Gate::Cut(slot_index));
                    }
                }
            }
        }
        result
    }

    /// The id and velocity of the note a start or move is for: the strike's own
    /// when it is that key, else the key the core returned to (a mono note-off),
    /// named the way a MIDI 1.0 note is and at the velocity it was struck.
    fn strike_of(
        strike: Option<(NoteId, f32)>,
        channel: MidiChannel,
        key: NoteNumber,
        velocity16: u16,
    ) -> (NoteId, f32) {
        match strike {
            Some((id, v)) if id.channel_note().is_some_and(|(_, note)| note == key) => (id, v),
            _ => (
                NoteId::from_channel_note(channel, key),
                f32::from(velocity16) / 65535.0,
            ),
        }
    }

    /// Forgets the per-note id the slot is reached by, if it is still the one
    /// that names this slot: a later voice of the same key may have the id by
    /// now, and a stolen releasing voice must not take it with it.
    fn unmap(&mut self, slot_index: usize) {
        let id = self.slots[slot_index].id();
        if self.id_to_slot.get(id).copied() == Some(slot_index) {
            self.id_to_slot.remove(id);
        }
    }

    fn slot_of_voice(&self, id: VoiceId) -> Option<usize> {
        self.slots
            .iter()
            .position(|s| !s.is_idle() && s.voice_id() == id)
    }

    /// What to do to voices now (the commands' effect on the synth's voices),
    /// taken in order.
    pub(crate) fn take_gates(&mut self) -> Vec<Gate> {
        std::mem::take(&mut self.gates)
    }

    /// Gives back the buffer `take_gates` lent, emptied, so it is reused.
    pub(crate) fn return_gates(&mut self, mut gates: Vec<Gate>) {
        gates.clear();
        // Nothing pushes a gate between `take_gates` and here (the synth
        // carries them out), so `self.gates` is the empty buffer `take` left.
        debug_assert!(self.gates.is_empty() && self.gates.capacity() == 0);
        self.gates = gates;
    }

    pub fn allocate(
        &mut self,
        id: NoteId,
        note: NoteNumber,
        channel: MidiChannel,
        velocity: f32,
    ) -> AllocationResult {
        // 16 bits, never 0 (a zero velocity is a note-off).
        let v16 = ((velocity.clamp(0.0, 1.0) * 65535.0) as u16).max(1);
        let on = Voice2::NoteOn {
            note,
            velocity: v16,
            attribute: tutti_ump::Attribute::None,
        };
        self.run(channel, &on, Some((id, velocity)))
    }

    /// Processes a note-off for `id`. Returns the slot whose voice should now be
    /// gated off (the note actually stopped sounding), or `None` when the note
    /// is held by sustain/sostenuto or no live voice matched `id`.
    pub fn release(&mut self, id: NoteId, channel: MidiChannel) -> Option<usize> {
        // Every id a caller holds comes from `from_channel_note`, which names a pair.
        let (_, note) = id
            .channel_note()
            .expect("a voice id is made from a channel and a note");
        let off = Voice2::NoteOff {
            note,
            velocity: 0,
            attribute: tutti_ump::Attribute::None,
        };
        let before = self.gates.len();
        self.run(channel, &off, None);
        self.gates[before..].iter().find_map(|g| match g {
            Gate::Off(i) => Some(*i),
            Gate::Cut(_) | Gate::Retune(..) | Gate::Start { .. } => None,
        })
    }

    /// Call when envelope reaches zero to free the slot for reuse.
    pub fn voice_finished(&mut self, voice_id: VoiceId) {
        if let Some(i) = self.slot_of_voice(voice_id) {
            let id = self.slots[i].id();
            if self.id_to_slot.get(id).copied() == Some(i) {
                self.id_to_slot.remove(id);
            }
            self.slots[i].silence();
        }
    }

    fn control(&mut self, channel: MidiChannel, controller: tutti_ump::CCNumber, down: bool) {
        let cc = Voice2::ControlChange {
            controller,
            value: if down { u32::MAX } else { 0 },
        };
        self.run(channel, &cc, None);
    }

    pub fn sustain_pedal(&mut self, channel: MidiChannel, on: bool) {
        self.control(channel, tutti_ump::CCNumber::SUSTAIN, on);
    }

    pub fn sostenuto_pedal(&mut self, channel: MidiChannel, on: bool) {
        self.control(channel, tutti_ump::CCNumber::SOSTENUTO, on);
    }

    /// Reset All Controllers: the pedals come up.
    pub fn reset_controllers(&mut self, channel: MidiChannel) {
        self.control(channel, tutti_ump::CCNumber::RESET_ALL, false);
    }

    pub fn all_notes_off(&mut self, channel: MidiChannel) {
        self.control(channel, tutti_ump::CCNumber::ALL_NOTES_OFF, false);
    }

    pub fn all_sound_off(&mut self, channel: MidiChannel) {
        self.control(channel, tutti_ump::CCNumber::ALL_SOUND_OFF, false);
    }

    pub fn update_envelope_level(&mut self, slot_index: usize, level: f32) {
        if slot_index < self.slots.len() {
            self.slots[slot_index].set_envelope_level(level);
        }
    }

    /// Which channels are MPE zones: a master's pitch bend adds to each of
    /// its members' own.
    pub fn set_zones(&mut self, zones: Zones) {
        self.core.set_zones(zones);
    }

    #[cfg(test)]
    pub(crate) fn core_phase(&self, slot_index: usize) -> Option<tutti_voicing::Phase> {
        let s = self.slots.get(slot_index).filter(|s| !s.is_idle())?;
        self.core.phase(s.voice_id())
    }

    #[cfg(test)]
    pub(crate) fn core_knows(&self, id: VoiceId) -> bool {
        self.core.phase(id).is_some()
    }

    /// The gates holding the voice in `slot_index`, if it is held at all.
    #[cfg(test)]
    pub(crate) fn holds_of(&self, slot_index: usize) -> Option<tutti_voicing::Holds> {
        let s = self.slots.get(slot_index).filter(|s| !s.is_idle())?;
        self.core.holds(s.voice_id())
    }

    /// Runs a message that starts and releases no note (a bend, a sensitivity)
    /// through the core.
    pub fn message(&mut self, channel: MidiChannel, message: &Voice2) {
        self.run(channel, message, None);
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
        self.core.reset();
        self.gates.clear();
    }
}

impl Clone for VoiceAllocator {
    fn clone(&self) -> Self {
        Self {
            config: self.config.clone(),
            core: self.core.clone(),
            slots: self.slots.clone(),
            id_to_slot: self.id_to_slot.clone(),
            views: Vec::with_capacity(self.config.max_voices),
            commands: Vec::with_capacity(2 * self.config.max_voices + 64),
            gates: Vec::with_capacity(2 * self.config.max_voices + 64),
        }
    }
}

#[cfg(test)]
mod tests;
