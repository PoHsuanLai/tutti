//! The per-note controller state of MIDI 2.0 (M2-104 section 7.4), as one pure
//! step.
//!
//! [`PerNoteState::step`] reads one message and returns the [`NoteEvent`]s it
//! means: a note started (with the id it was minted), a controller's value for
//! a note, a note's controllers reset. It owns no voice and renders nothing, so
//! an instrument that allocates voices ([`Voicing`](crate::Voicing)) and a
//! plugin host that addresses notes by id use the one copy of the rules:
//!
//! - **State is per (Group, channel, Note Number)** (section 2.1.2: a Group's
//!   channels are separate from every other Group's). A Registered or
//!   Assignable Per-Note Controller, Polyphonic Pressure and Per-Note Pitch
//!   Bend are held whether or not a note sounds, and the next Note On starts
//!   with them (section 7.4.15.2, Appendix C.3). A Note Number holds
//!   [`NOTE_CONTROLS`] distinct controllers; past that the one set longest ago
//!   is forgotten.
//! - **Every started note has an id of its own**, a [`NoteId`] above
//!   [`NoteId::MAX_CHANNEL_NOTE`] that names no (channel, note) pair, so two
//!   notes on one number are two ids. The messages of a number address the
//!   notes started on it that have not been detached or [`end`](PerNoteState::end)ed:
//!   a Note Off does not end a note, so a tail still responds (section 7.4.5:
//!   after a Detach, "all currently playing notes and previous notes" stop
//!   responding, so before it they respond). A number keeps [`LIVE_NOTES`] of
//!   them, tutti's bound; past it the oldest is dropped.
//! - **Per-Note Management** (section 7.4.5): Detach makes the notes of the
//!   number stop taking per-note values (they keep what they have; no event is
//!   sent, the next Note On has a new id); Reset clears the number's held
//!   values, for future notes, and resets the notes not detached; with both,
//!   Detach runs first.
//! - **Attribute Pitch 7.9** on a Note On is that note's own Pitch, and is not
//!   held (section 7.4.15.3).
//! - **The per-note bend sensitivity** (RPN 0/7, section 7.4.13) is one value
//!   for each channel, and a bend is handed out already scaled by it; a new
//!   sensitivity hands each live note of a number that holds a bend its new size.
//! - Reset All Controllers does not reach it (Appendix B.2): nothing here reads
//!   a Control Change.
//!
//! The state is sized in [`PerNoteState::new`], about 270 KiB for each Group
//! (128 notes on 16 channels), and `step` allocates nothing.

use tutti_midi_protocol::{PitchBendSensitivity, bend_u32_to_signed_f32};
use tutti_types::Semitones;
use tutti_ump::{
    Address, Attribute, MidiChannel, NoteId, NoteNumber, Pitch, RegisteredPerNoteController, Rpn,
    Voice2,
};

const CHANNELS: usize = MidiChannel::COUNT as usize;
const KEYS: usize = 128;

/// The distinct per-note controllers one Note Number holds. M2-104 names 22
/// registered numbers and 256 assignable ones; a sender that drives more than
/// this many on one note loses the oldest.
pub const NOTE_CONTROLS: usize = 8;

/// The notes of one Note Number that are kept as responding to its per-note
/// controllers (tutti's bound: section 7.4.5 sets none). A fifth started
/// without a Detach or an [`end`](PerNoteState::end) drops the oldest.
pub const LIVE_NOTES: usize = 4;

/// The most events one `step` returns: a Note On's (the start, every held
/// value and the attribute), or a new sensitivity's (one bend for each of a
/// channel's notes).
pub(crate) const MAX_EVENTS: usize = KEYS * LIVE_NOTES + NOTE_CONTROLS + 2;

/// A per-note controller and its value, as a note reads it (M2-104 section
/// 7.4). A controller never sent has no value: the note's own default stands.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum NoteControl {
    /// Registered Per-Note Controller #3 (section 7.4.15.2) or a Note On's
    /// Attribute Pitch 7.9 (section 7.4.15.3): the pitch the note sounds at,
    /// the Note Number being an index only. Pitch bend offsets from it.
    Pitch(Pitch),
    /// Per-Note Pitch Bend, in semitones: the wheel's position over the
    /// channel's sensitivity (section 7.4.13).
    Bend(Semitones),
    /// Polyphonic Pressure, 32 bits.
    Pressure(u32),
    /// Any other Registered Per-Note Controller (Appendix A), 32 bits.
    Registered(RegisteredPerNoteController, u32),
    /// An Assignable Per-Note Controller's index and 32-bit value.
    Assignable(u8, u32),
}

/// What a message did to the per-note state.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum NoteEvent {
    /// A note started on `address` and `note`, with the id it was minted. The
    /// values the number held follow as [`Control`](Self::Control)s, in the
    /// order they were told, then the Note On's own Attribute Pitch.
    Started {
        /// The new note.
        id: NoteId,
        /// Its Group and channel.
        address: Address,
        /// Its Note Number.
        note: NoteNumber,
    },
    /// A controller of a note took a value: sent for each live note of the
    /// number (one not detached or ended).
    Control {
        /// The note.
        id: NoteId,
        /// The controller and its value.
        control: NoteControl,
    },
    /// A note's controllers return to their defaults (Per-Note Management
    /// Reset); the note keeps sounding.
    Reset {
        /// The note.
        id: NoteId,
    },
}

/// What a Note Number was last told, by controller: a code and the wire word.
#[derive(Debug, Clone, Copy)]
struct Held {
    len: usize,
    set: [(u16, u32); NOTE_CONTROLS],
}

impl Held {
    const NONE: Held = Held {
        len: 0,
        set: [(0, 0); NOTE_CONTROLS],
    };

    const BEND: u16 = 0x200;
    const PRESSURE: u16 = 0x201;
    const ASSIGNABLE: u16 = 0x100;

    /// Records `value`, the most recent. The oldest goes if the table is full.
    fn set(&mut self, code: u16, value: u32) {
        if let Some(e) = self.set[..self.len].iter_mut().find(|e| e.0 == code) {
            e.1 = value;
            return;
        }
        if self.len == NOTE_CONTROLS {
            self.set.copy_within(1.., 0);
            self.len -= 1;
        }
        self.set[self.len] = (code, value);
        self.len += 1;
    }

    fn get(&self, code: u16) -> Option<u32> {
        self.set[..self.len]
            .iter()
            .find(|e| e.0 == code)
            .map(|e| e.1)
    }
}

/// The notes of a number that still respond, oldest first.
#[derive(Debug, Clone, Copy)]
struct Live {
    len: usize,
    ids: [NoteId; LIVE_NOTES],
}

impl Live {
    const NONE: Live = Live {
        len: 0,
        ids: [NoteId::from_raw(0); LIVE_NOTES],
    };

    fn ids(&self) -> &[NoteId] {
        &self.ids[..self.len]
    }

    /// `id` is the newest. The oldest goes if the list is full.
    fn push(&mut self, id: NoteId) {
        if self.len == LIVE_NOTES {
            self.ids.copy_within(1.., 0);
            self.len -= 1;
        }
        self.ids[self.len] = id;
        self.len += 1;
    }

    fn remove(&mut self, id: NoteId) {
        if let Some(at) = self.ids().iter().position(|&i| i == id) {
            self.ids.copy_within(at + 1..self.len, at);
            self.len -= 1;
        }
    }
}

/// The per-note state. See the module docs.
#[derive(Debug)]
pub struct PerNoteState {
    groups: usize,
    default_range: Semitones,
    /// `(group * 16 + channel) * 128 + note`.
    /// What each (Group, channel, Note Number) was told.
    held: Vec<Held>,
    /// The notes of each that respond. Parallel to `held`.
    live: Vec<Live>,
    /// Each channel's Per-Note Pitch Bend sensitivity, `group * 16 + channel`.
    ranges: Vec<Semitones>,
    next: u32,
    out: Vec<NoteEvent>,
}

impl Clone for PerNoteState {
    /// A copy with its event buffer as roomy as the original's: a derived
    /// clone would shrink it, and `step` must never grow it.
    fn clone(&self) -> Self {
        Self {
            groups: self.groups,
            default_range: self.default_range,
            held: self.held.clone(),
            live: self.live.clone(),
            ranges: self.ranges.clone(),
            next: self.next,
            out: Vec::with_capacity(self.out.capacity()),
        }
    }
}

impl PerNoteState {
    /// State for `groups` Groups (1 to 16; more are not kept), each channel's
    /// per-note bend sensitivity starting at `default_range` (section 7.4.13
    /// gives no default). Allocates; `step` never does.
    #[must_use]
    pub fn new(groups: usize, default_range: Semitones) -> Self {
        let groups = groups.clamp(1, usize::from(tutti_ump::MidiGroup::COUNT));
        Self {
            groups,
            default_range,
            held: vec![Held::NONE; groups * CHANNELS * KEYS],
            live: vec![Live::NONE; groups * CHANNELS * KEYS],
            ranges: vec![default_range; groups * CHANNELS],
            next: NoteId::MAX_CHANNEL_NOTE + 1,
            out: Vec::with_capacity(MAX_EVENTS),
        }
    }

    /// Drops everything held: the values, the live notes (detached or not), and every sensitivity back to the default. Allocates nothing.
    /// Ids already minted are not reused.
    pub fn clear(&mut self) {
        self.held.fill(Held::NONE);
        self.live.fill(Live::NONE);
        self.ranges.fill(self.default_range);
    }

    fn channel_index(&self, address: Address) -> Option<usize> {
        let group = usize::from(address.group.get());
        (group < self.groups).then(|| group * CHANNELS + usize::from(address.channel.get()))
    }

    fn slot(&self, address: Address, note: NoteNumber) -> Option<usize> {
        Some(self.channel_index(address)? * KEYS + usize::from(note.get()))
    }

    /// Reads one channel voice message of `address`, and returns what it
    /// means; the slice is valid until the next call. A Note On starts a note
    /// ([`start`](Self::start), its attribute read); the per-note controllers,
    /// Per-Note Management and RPN 0/7 are the state's; every other message
    /// (a Note Off, a Control Change, Reset All Controllers among them) returns
    /// nothing.
    pub fn step(&mut self, address: Address, message: &Voice2) -> &[NoteEvent] {
        self.out.clear();
        match *message {
            Voice2::NoteOn {
                note, attribute, ..
            } => return self.start(address, note, attribute),
            Voice2::RegisteredPerNote {
                note,
                controller,
                value,
            } => self.control(address, note, u16::from(controller.number()), value),
            Voice2::AssignablePerNote {
                note,
                controller,
                value,
            } => self.control(
                address,
                note,
                Held::ASSIGNABLE | u16::from(controller),
                value,
            ),
            Voice2::PerNotePitchBend { note, value } => {
                self.control(address, note, Held::BEND, value);
            }
            Voice2::PolyPressure { note, value } => {
                self.control(address, note, Held::PRESSURE, value);
            }
            Voice2::PerNoteManagement {
                note,
                detach,
                reset,
            } => self.management(address, note, detach, reset),
            Voice2::Registered { bank, index, value }
                if Rpn::new(bank, index) == Rpn::PER_NOTE_PITCH_BEND_SENSITIVITY =>
            {
                self.sensitivity(address, value);
            }
            _ => {}
        }
        &self.out
    }

    /// Starts a note on `note`: mints its id, makes it the number's current
    /// note, and returns [`NoteEvent::Started`] and the values the number holds
    /// (then the `attribute`'s pitch, if it has one, for this note alone).
    /// [`step`](Self::step) calls it for a Note On; an instrument that moves a
    /// sounding voice to a key (legato) calls it for the key it moves to.
    pub fn start(
        &mut self,
        address: Address,
        note: NoteNumber,
        attribute: Attribute,
    ) -> &[NoteEvent] {
        self.out.clear();
        let Some(at) = self.slot(address, note) else {
            return &self.out;
        };
        let id = NoteId::from_raw(self.next);
        // Past the last id of u32 the minting returns to the first above the
        // pairs: an id is only ever compared with the ones live.
        self.next = self
            .next
            .checked_add(1)
            .unwrap_or(NoteId::MAX_CHANNEL_NOTE + 1);
        self.live[at].push(id);
        self.out.push(NoteEvent::Started { id, address, note });
        let held = self.held[at];
        for &(code, value) in &held.set[..held.len] {
            let control = self.resolve(address, code, value);
            self.out.push(NoteEvent::Control { id, control });
        }
        if let Some(pitch) = attribute.pitch() {
            self.out.push(NoteEvent::Control {
                id,
                control: NoteControl::Pitch(pitch),
            });
        }
        &self.out
    }

    /// The controller a held code and word mean on `address`'s channel.
    fn resolve(&self, address: Address, code: u16, value: u32) -> NoteControl {
        match code {
            Held::BEND => {
                let range = self
                    .channel_index(address)
                    .map_or(self.default_range, |c| self.ranges[c]);
                NoteControl::Bend(Semitones(bend_u32_to_signed_f32(value) * range.get()))
            }
            Held::PRESSURE => NoteControl::Pressure(value),
            c if c >= Held::ASSIGNABLE => NoteControl::Assignable(c as u8, value),
            c => match RegisteredPerNoteController::from(c as u8) {
                RegisteredPerNoteController::Pitch => NoteControl::Pitch(Pitch::from_q7_25(value)),
                r => NoteControl::Registered(r, value),
            },
        }
    }

    /// A controller arrives: it becomes the number's held value and moves
    /// every live note of the number.
    fn control(&mut self, address: Address, note: NoteNumber, code: u16, value: u32) {
        let Some(at) = self.slot(address, note) else {
            return;
        };
        self.held[at].set(code, value);
        let control = self.resolve(address, code, value);
        for &id in self.live[at].ids() {
            self.out.push(NoteEvent::Control { id, control });
        }
    }

    fn management(&mut self, address: Address, note: NoteNumber, detach: bool, reset: bool) {
        let Some(at) = self.slot(address, note) else {
            return;
        };
        if detach {
            self.live[at] = Live::NONE;
        }
        if reset {
            self.held[at] = Held::NONE;
            for &id in self.live[at].ids() {
                self.out.push(NoteEvent::Reset { id });
            }
        }
    }

    fn sensitivity(&mut self, address: Address, value: u32) {
        let Some(c) = self.channel_index(address) else {
            return;
        };
        self.ranges[c] = Semitones(PitchBendSensitivity::from_rpn_bits(value).as_semitones_f32());
        for at in c * KEYS..(c + 1) * KEYS {
            if let Some(bend) = self.held[at].get(Held::BEND) {
                let control = self.resolve(address, Held::BEND, bend);
                for &id in self.live[at].ids() {
                    self.out.push(NoteEvent::Control { id, control });
                }
            }
        }
    }

    /// The note `id` has fully ended (its voice was freed, the plugin said its
    /// note ended): its number no longer moves it. A Note Off does not end a
    /// note, since its tail still responds. Looks the note up among every
    /// number's live notes, so it costs a pass over the state; allocates
    /// nothing.
    pub fn end(&mut self, id: NoteId) {
        for live in &mut self.live {
            live.remove(id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_ump::{CCNumber, MidiGroup, U7};

    const PITCH_60: u32 = 60 << 25;

    fn at(group: u8, channel: u8) -> Address {
        Address {
            group: MidiGroup::new(group).unwrap(),
            channel: MidiChannel::of(channel),
        }
    }

    fn state() -> PerNoteState {
        PerNoteState::new(2, Semitones(48.0))
    }

    fn key(n: u8) -> NoteNumber {
        NoteNumber::of(n)
    }

    fn pitch_of(s: &mut PerNoteState, a: Address, n: u8, value: u32) -> Vec<NoteEvent> {
        s.step(
            a,
            &Voice2::RegisteredPerNote {
                note: key(n),
                controller: RegisteredPerNoteController::Pitch,
                value,
            },
        )
        .to_vec()
    }

    fn bend(s: &mut PerNoteState, a: Address, n: u8, value: u32) -> Vec<NoteEvent> {
        s.step(
            a,
            &Voice2::PerNotePitchBend {
                note: key(n),
                value,
            },
        )
        .to_vec()
    }

    fn management(
        s: &mut PerNoteState,
        a: Address,
        n: u8,
        detach: bool,
        reset: bool,
    ) -> Vec<NoteEvent> {
        s.step(
            a,
            &Voice2::PerNoteManagement {
                note: key(n),
                detach,
                reset,
            },
        )
        .to_vec()
    }

    fn sensitivity(s: &mut PerNoteState, a: Address, semitones: u32) -> Vec<NoteEvent> {
        s.step(
            a,
            &Voice2::Registered {
                bank: U7::of(0),
                index: U7::of(7),
                value: semitones << 25,
            },
        )
        .to_vec()
    }

    fn start(s: &mut PerNoteState, a: Address, n: u8) -> Vec<NoteEvent> {
        s.start(a, key(n), Attribute::None).to_vec()
    }

    fn started(events: &[NoteEvent]) -> NoteId {
        match events.first() {
            Some(NoteEvent::Started { id, .. }) => *id,
            other => panic!("not a start: {other:?}"),
        }
    }

    fn pitch_event(id: NoteId, hcu: u32) -> NoteEvent {
        NoteEvent::Control {
            id,
            control: NoteControl::Pitch(Pitch::from_q7_25(hcu << 25)),
        }
    }

    fn semitones(e: &NoteEvent) -> f32 {
        match e {
            NoteEvent::Control {
                control: NoteControl::Bend(b),
                ..
            } => b.get(),
            other => panic!("not a bend: {other:?}"),
        }
    }

    /// M2-104 section 7.4.15.2 and Appendix C.3: a Pitch 7.25 told before the
    /// Note On, with no note to address, is held, and the note that starts is
    /// handed it.
    #[test]
    fn a_value_told_before_the_note_on_starts_the_note() {
        let mut s = state();
        assert_eq!(pitch_of(&mut s, at(0, 0), 0, PITCH_60), vec![]);
        let e = start(&mut s, at(0, 0), 0);
        let id = started(&e);
        assert_eq!(e.len(), 2, "{e:?}");
        assert_eq!(e[1], pitch_event(id, 60));
    }

    /// M2-104 section 7.4.5, Detach: the current note stops taking per-note
    /// values, and later ones are for future notes: the Pitch told after the
    /// Detach reaches the next note, under another id.
    #[test]
    fn a_detach_ends_the_notes_values_and_the_next_note_has_its_own() {
        let mut s = state();
        let a = at(0, 0);
        let old = started(&start(&mut s, a, 5));
        assert_eq!(management(&mut s, a, 5, true, false), vec![]);
        assert_eq!(
            pitch_of(&mut s, a, 5, PITCH_60),
            vec![],
            "the old note is deaf"
        );
        let e = start(&mut s, a, 5);
        let new = started(&e);
        assert_ne!(new, old);
        assert_eq!(e[1], pitch_event(new, 60));
        // Only the new note answers now.
        let e = bend(&mut s, a, 5, 0xC000_0000);
        assert!(
            matches!(e[..], [NoteEvent::Control { id, .. }] if id == new),
            "{e:?}"
        );
    }

    /// M2-104 section 7.4.5: only a Detach makes "previous notes on the
    /// referenced Note Number" stop responding. A Note On on a number whose
    /// earlier note has not been detached leaves both responding.
    #[test]
    fn a_number_moves_every_live_note_until_a_detach() {
        let mut s = state();
        let a = at(0, 0);
        let first = started(&start(&mut s, a, 5));
        let second = started(&start(&mut s, a, 5));
        let e = bend(&mut s, a, 5, 0xC000_0000);
        let ids: Vec<NoteId> = e
            .iter()
            .map(|e| match e {
                NoteEvent::Control { id, .. } => *id,
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(ids, vec![first, second]);
        // A Reset reaches both, too, and the Detach after it none.
        assert_eq!(
            management(&mut s, a, 5, false, true),
            vec![
                NoteEvent::Reset { id: first },
                NoteEvent::Reset { id: second }
            ]
        );
        management(&mut s, a, 5, true, false);
        assert_eq!(bend(&mut s, a, 5, 0xC000_0000), vec![]);
    }

    /// `end` is for the owner that knows a note is over: that note stops
    /// responding and the others of its number go on; a Note Off is not an
    /// end.
    #[test]
    fn an_ended_note_stops_responding_and_a_note_off_does_not_end_it() {
        let mut s = state();
        let a = at(0, 0);
        let first = started(&start(&mut s, a, 5));
        let second = started(&start(&mut s, a, 5));
        s.step(
            a,
            &Voice2::NoteOff {
                note: key(5),
                velocity: 0,
                attribute: Attribute::None,
            },
        );
        assert_eq!(bend(&mut s, a, 5, 0xC000_0000).len(), 2);
        s.end(first);
        let e = bend(&mut s, a, 5, 0xC000_0000);
        assert!(
            matches!(e[..], [NoteEvent::Control { id, .. }] if id == second),
            "{e:?}"
        );
    }

    /// The live notes of a number are bounded (`LIVE_NOTES`): past it the
    /// oldest stops responding.
    #[test]
    fn a_number_keeps_a_bounded_set_of_live_notes() {
        let mut s = state();
        let a = at(0, 0);
        let ids: Vec<NoteId> = (0..=LIVE_NOTES)
            .map(|_| started(&start(&mut s, a, 5)))
            .collect();
        let e = bend(&mut s, a, 5, 0xC000_0000);
        assert_eq!(e.len(), LIVE_NOTES);
        assert!(
            !e.iter()
                .any(|e| matches!(e, NoteEvent::Control { id, .. } if *id == ids[0]))
        );
    }

    /// M2-104 section 7.4.5: with D and S the Detach is first, so the current
    /// note is not reset (it keeps its values) and the next starts at defaults.
    #[test]
    fn detach_and_reset_together_leave_the_note_and_default_the_next() {
        let mut s = state();
        let a = at(0, 0);
        start(&mut s, a, 5);
        bend(&mut s, a, 5, 0xC000_0000);
        assert_eq!(management(&mut s, a, 5, true, true), vec![]);
        assert_eq!(start(&mut s, a, 5).len(), 1, "no value follows the start");
    }

    /// M2-104 section 7.4.5, Reset alone: the current note is reset, and the
    /// next note starts at defaults.
    #[test]
    fn a_reset_resets_the_note_and_defaults_the_next() {
        let mut s = state();
        let a = at(0, 0);
        let id = started(&start(&mut s, a, 5));
        bend(&mut s, a, 5, 0xC000_0000);
        assert_eq!(
            management(&mut s, a, 5, false, true),
            vec![NoteEvent::Reset { id }]
        );
        assert_eq!(start(&mut s, a, 5).len(), 1);
    }

    /// M2-104 section 7.4.13: the sensitivity is one channel's. Half the wheel
    /// is half of that channel's range.
    #[test]
    fn the_bend_sensitivity_is_one_channels() {
        let mut s = state();
        sensitivity(&mut s, at(0, 0), 12);
        start(&mut s, at(0, 0), 5);
        start(&mut s, at(0, 1), 5);
        let on_0 = semitones(&bend(&mut s, at(0, 0), 5, 0xC000_0000)[0]);
        let on_1 = semitones(&bend(&mut s, at(0, 1), 5, 0xC000_0000)[0]);
        assert!((on_0 - 6.0).abs() < 1e-3, "{on_0}");
        assert!((on_1 - 24.0).abs() < 1e-3, "{on_1}");
    }

    /// M2-104 section 7.4.13: a bend is a position over the sensitivity, so a
    /// new sensitivity hands the note that holds one its new size.
    #[test]
    fn a_new_sensitivity_resizes_the_bends_held() {
        let mut s = state();
        let a = at(0, 0);
        let id = started(&start(&mut s, a, 5));
        bend(&mut s, a, 5, 0xC000_0000);
        let e = sensitivity(&mut s, a, 2);
        assert!(
            matches!(e[..], [NoteEvent::Control { id: i, .. }] if i == id),
            "{e:?}"
        );
        assert!((semitones(&e[0]) - 1.0).abs() < 1e-3);
    }

    /// M2-104 section 7.4.15.3: Attribute Pitch 7.9 is that note's pitch, and
    /// "is not valid for any subsequent Notes".
    #[test]
    fn an_attribute_pitch_is_for_that_note_alone() {
        let mut s = state();
        let a = at(0, 0);
        let e = s.start(a, key(0), Attribute::Pitch(60 << 9)).to_vec();
        let id = started(&e);
        assert_eq!(e.last(), Some(&pitch_event(id, 60)));
        assert_eq!(start(&mut s, a, 0).len(), 1);
    }

    /// M2-104 section 7.4.5: a controller addresses the Note Number "on the
    /// referenced" channel: neither another key nor another channel hears it.
    #[test]
    fn a_controller_addresses_one_channel_and_key() {
        let mut s = state();
        let ids = [(0, 5), (0, 6), (1, 5)].map(|(c, n)| started(&start(&mut s, at(0, c), n)));
        let e = bend(&mut s, at(0, 0), 5, 0xC000_0000);
        assert!(
            matches!(e[..], [NoteEvent::Control { id, .. }] if id == ids[0]),
            "{e:?}"
        );
        for (c, n) in [(0, 6), (1, 5), (1, 6)] {
            assert_eq!(start(&mut s, at(0, c), n).len(), 1, "({c}, {n})");
        }
    }

    /// M2-104 section 2.1.2: each Group's channels are separate. A value, a
    /// Detach and a sensitivity told in Group 1 are not Group 0's.
    #[test]
    fn two_groups_are_independent() {
        let mut s = state();
        // The value is held in Group 1 only.
        pitch_of(&mut s, at(1, 0), 5, PITCH_60);
        assert_eq!(start(&mut s, at(0, 0), 5).len(), 1);
        assert_eq!(start(&mut s, at(1, 0), 5).len(), 2);
        // A sensitivity of one Group leaves the other's.
        sensitivity(&mut s, at(1, 0), 12);
        let on_0 = semitones(&bend(&mut s, at(0, 0), 5, 0xC000_0000)[0]);
        let on_1 = semitones(&bend(&mut s, at(1, 0), 5, 0xC000_0000)[0]);
        assert!(
            (on_0 - 24.0).abs() < 1e-3 && (on_1 - 6.0).abs() < 1e-3,
            "{on_0} {on_1}"
        );
        // A Detach in Group 1 leaves Group 0's note answering.
        let g0 = started(&start(&mut s, at(0, 0), 7));
        start(&mut s, at(1, 0), 7);
        management(&mut s, at(1, 0), 7, true, false);
        let e = bend(&mut s, at(0, 0), 7, 0xC000_0000);
        assert!(
            matches!(e[..], [NoteEvent::Control { id, .. }] if id == g0),
            "{e:?}"
        );
    }

    /// Every started note has an id of its own, above the ids a (channel,
    /// note) pair names, so two notes on one number differ and no id names a
    /// pair.
    #[test]
    fn every_started_note_is_minted_an_id_of_its_own() {
        let mut s = state();
        let mut seen = Vec::new();
        for i in 0..300u32 {
            let id = started(&start(&mut s, at(0, (i % 16) as u8), (i % 128) as u8));
            assert!(u32::from(id) > NoteId::MAX_CHANNEL_NOTE);
            assert_eq!(id.channel_note(), None);
            assert!(!seen.contains(&id));
            seen.push(id);
        }
        let a = started(&start(&mut s, at(0, 0), 5));
        let b = started(&start(&mut s, at(0, 0), 5));
        assert_ne!(a, b, "two notes on one number");
    }

    /// Reset All Controllers, a Note Off and any other message that is not
    /// per-note leave the state alone (Appendix B.2).
    #[test]
    fn messages_that_are_not_per_note_change_nothing() {
        let mut s = state();
        let a = at(0, 0);
        pitch_of(&mut s, a, 5, PITCH_60);
        for m in [
            Voice2::ControlChange {
                controller: CCNumber::RESET_ALL,
                value: 0,
            },
            Voice2::NoteOff {
                note: key(5),
                velocity: 0,
                attribute: Attribute::None,
            },
            Voice2::PitchBend(u32::MAX),
        ] {
            assert_eq!(s.step(a, &m), &[], "{m:?}");
        }
        assert_eq!(start(&mut s, a, 5).len(), 2);
    }

    /// `clear` drops the held values, the current notes (they stop being
    /// addressed) and every sensitivity.
    #[test]
    fn clear_drops_values_notes_and_sensitivities() {
        let mut s = state();
        let a = at(0, 0);
        pitch_of(&mut s, a, 5, PITCH_60);
        sensitivity(&mut s, a, 12);
        start(&mut s, a, 6);
        s.clear();
        assert_eq!(bend(&mut s, a, 6, 0xC000_0000), vec![], "no current note");
        assert_eq!(start(&mut s, a, 5).len(), 1, "the value is gone");
        let e = bend(&mut s, a, 5, 0xC000_0000);
        assert!(
            (semitones(&e[0]) - 24.0).abs() < 1e-3,
            "the default sensitivity"
        );
    }

    /// The state allocates nothing after `new`: its event buffer never outgrows
    /// its capacity, on the longest answers (every note of a channel resized).
    #[test]
    fn step_never_grows_its_buffer() {
        let mut s = state();
        let a = at(0, 0);
        let cap = s.out.capacity();
        for n in 0..128u8 {
            start(&mut s, a, n);
            bend(&mut s, a, n, 0xC000_0000);
        }
        assert_eq!(sensitivity(&mut s, a, 12).len(), 128);
        assert_eq!(s.out.capacity(), cap);
        assert_eq!(s.clone().out.capacity(), cap);
    }
}
