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
//! - **A note has one sounding pitch, and it is handed out whole**
//!   ([`NoteControl::Pitch`]). Section 7.4.15 lists what sets a note's pitch:
//!   the Note Number is the default; Registered Per-Note Controller #3 and the
//!   Attribute Pitch override it, the attribute for its one note ("Pitch Bend
//!   and Per-Note Pitch Bend act as offsets from the Attribute #3: Pitch 7.9",
//!   and likewise "act as offsets from the pitch set by Registered Per-Note
//!   Controller #3: Pitch 7.25"). So the sounding pitch is the told pitch (the
//!   attribute, else the number's Pitch 7.25, else the key) plus the Per-Note
//!   Pitch Bend, the wheel's position over the channel's RPN 0/7 sensitivity
//!   (section 7.4.13: the range "from the current sounding pitch of a Note").
//!   It is sent when a note starts (after the values the number holds, and
//!   only if it is not the key's: a note told nothing has the instrument's
//!   default) and whenever the told pitch, the bend or the
//!   sensitivity changes, whichever order they arrive in, and saturates at the
//!   ends of Q7.25's range. The channel's own Pitch Bend and Channel Tuning
//!   are not in it: they are the channel's, and the instrument applies them.
//!   A consumer with one tuning per note takes it as it is.
//! - Reset All Controllers does not reach it (Appendix B.2) but for Polyphonic
//!   Pressure, which RP-015 resets and Appendix B.2 does not list among the new
//!   per-note controllers it leaves: [`reset_pressure`](PerNoteState::reset_pressure)
//!   is the caller's to call. [`step`](PerNoteState::step) reads no Control Change.
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
/// value and the sounding pitch), or a new sensitivity's (one pitch for each
/// of a channel's notes).
pub(crate) const MAX_EVENTS: usize = KEYS * LIVE_NOTES + NOTE_CONTROLS + 2;

/// A per-note controller and its value, as a note reads it (M2-104 section
/// 7.4). A controller never sent has no value: the note's own default stands.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum NoteControl {
    /// The pitch the note sounds at (see the module docs): its told pitch
    /// (Registered Per-Note Controller #3, a Note On's Attribute Pitch 7.9, else
    /// its key's own) offset by the Per-Note Pitch Bend. Absolute, and without
    /// the channel's Pitch Bend. A note that hears no `Pitch` sounds at its
    /// key (a note started with nothing told and no bend is sent none); after
    /// a [`Reset`](NoteEvent::Reset) it is the key's again.
    Pitch(Pitch),
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
    /// order they were told, then the note's sounding pitch if it is not its
    /// key's (the attribute's, the number's Pitch 7.25, and the held bend).
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
        /// The Note Number the note was started on: the key a [`NoteControl::Pitch`]
        /// is measured from.
        note: NoteNumber,
        /// The controller and its value.
        control: NoteControl,
    },
    /// A note's controllers return to their defaults (Per-Note Management
    /// Reset); the note keeps sounding. Its sounding pitch is its key's again
    /// and no [`NoteControl::Pitch`] follows: the consumer that took a pitch
    /// returns to the key with the rest.
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

    /// Registered Per-Note Controller #3's code, its number.
    const PITCH: u16 = RegisteredPerNoteController::Pitch.number() as u16;
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

/// The notes of a number that still respond, oldest first, each with the
/// pitch it was told (`None`: its key's).
#[derive(Debug, Clone, Copy)]
struct Live {
    len: usize,
    ids: [NoteId; LIVE_NOTES],
    told: [Option<Pitch>; LIVE_NOTES],
}

impl Live {
    const NONE: Live = Live {
        len: 0,
        ids: [NoteId::from_raw(0); LIVE_NOTES],
        told: [None; LIVE_NOTES],
    };

    /// The live notes with the pitch each was told, oldest first.
    fn notes(&self) -> impl Iterator<Item = (NoteId, Option<Pitch>)> + use<> {
        let live = *self;
        (0..live.len).map(move |i| (live.ids[i], live.told[i]))
    }

    fn ids(&self) -> &[NoteId] {
        &self.ids[..self.len]
    }

    /// `id` is the newest. The oldest goes if the list is full.
    fn push(&mut self, id: NoteId, told: Option<Pitch>) {
        if self.len == LIVE_NOTES {
            self.ids.copy_within(1.., 0);
            self.told.copy_within(1.., 0);
            self.len -= 1;
        }
        self.ids[self.len] = id;
        self.told[self.len] = told;
        self.len += 1;
    }

    fn remove(&mut self, id: NoteId) {
        if let Some(at) = self.ids().iter().position(|&i| i == id) {
            self.ids.copy_within(at + 1..self.len, at);
            self.told.copy_within(at + 1..self.len, at);
            self.len -= 1;
        }
    }

    /// Every live note is told `pitch`.
    fn tell_all(&mut self, pitch: Option<Pitch>) {
        self.told[..self.len].fill(pitch);
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
    pub fn step(&mut self, address: Address, message: Voice2) -> &[NoteEvent] {
        self.out.clear();
        match message {
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
        let held = self.held[at];
        // The attribute overrides what the number holds, for this note alone.
        let told = attribute
            .pitch()
            .or_else(|| held.get(Held::PITCH).map(Pitch::from_q7_25));
        self.live[at].push(id, told);
        self.out.push(NoteEvent::Started { id, address, note });
        for &(code, value) in &held.set[..held.len] {
            if code != Held::PITCH && code != Held::BEND {
                let control = self.resolve(code, value);
                self.out.push(NoteEvent::Control { id, note, control });
            }
        }
        // A note told nothing sounds at its key, which is the instrument's
        // default: no event.
        let sounding = self.sounding(at, address, told);
        if sounding.q7_25() != ((at % KEYS) as u32) << 25 {
            let control = NoteControl::Pitch(sounding);
            self.out.push(NoteEvent::Control { id, note, control });
        }
        &self.out
    }

    /// The sounding pitch of a note of slot `at` that was told `told`: that
    /// pitch (the key's if none) offset by the number's held bend, scaled by
    /// the channel's sensitivity.
    fn sounding(&self, at: usize, address: Address, told: Option<Pitch>) -> Pitch {
        let told = told.unwrap_or_else(|| Pitch::from_q7_25(((at % KEYS) as u32) << 25));
        let range = self
            .channel_index(address)
            .map_or(self.default_range, |c| self.ranges[c]);
        let bend = self.held[at]
            .get(Held::BEND)
            .map_or(0.0, |v| f64::from(bend_u32_to_signed_f32(v) * range.get()));
        let q = f64::from(told.q7_25()) + bend * f64::from(1u32 << 25);
        Pitch::from_q7_25(q.round().clamp(0.0, f64::from(u32::MAX)) as u32)
    }

    /// The controller a held code and word, other than the pitch and its bend,
    /// mean.
    fn resolve(&self, code: u16, value: u32) -> NoteControl {
        match code {
            Held::PRESSURE => NoteControl::Pressure(value),
            c if c >= Held::ASSIGNABLE => NoteControl::Assignable(c as u8, value),
            c => NoteControl::Registered(RegisteredPerNoteController::from(c as u8), value),
        }
    }

    /// A controller arrives: it becomes the number's held value and moves
    /// every live note of the number.
    fn control(&mut self, address: Address, note: NoteNumber, code: u16, value: u32) {
        let Some(at) = self.slot(address, note) else {
            return;
        };
        self.held[at].set(code, value);
        if code == Held::PITCH {
            self.live[at].tell_all(Some(Pitch::from_q7_25(value)));
        }
        if code == Held::PITCH || code == Held::BEND {
            self.sound_all(at, address);
            return;
        }
        let control = self.resolve(code, value);
        for &id in self.live[at].ids() {
            self.out.push(NoteEvent::Control { id, note, control });
        }
    }

    /// Each live note of slot `at` is told its sounding pitch.
    fn sound_all(&mut self, at: usize, address: Address) {
        let note = NoteNumber::masked((at % KEYS) as u8);
        for (id, told) in self.live[at].notes() {
            let control = NoteControl::Pitch(self.sounding(at, address, told));
            self.out.push(NoteEvent::Control { id, note, control });
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
            self.live[at].tell_all(None);
            for &id in self.live[at].ids() {
                self.out.push(NoteEvent::Reset { id });
            }
        }
    }

    fn sensitivity(&mut self, address: Address, value: u32) {
        let Some(c) = self.channel_index(address) else {
            return;
        };
        self.ranges[c] = PitchBendSensitivity::from_rpn_bits(value).semitones();
        for at in c * KEYS..(c + 1) * KEYS {
            if self.held[at].get(Held::BEND).is_some() {
                self.sound_all(at, address);
            }
        }
    }

    /// Reset All Controllers' "Reset polyphonic pressure for all notes to 0"
    /// (RP-015) on `address`'s channel: every Note Number whose held Polyphonic
    /// Pressure is not 0 takes 0, and its live notes are told. The other
    /// per-note controllers are left (M2-104 Appendix B.2). The slice is valid
    /// until the next call.
    pub fn reset_pressure(&mut self, address: Address) -> &[NoteEvent] {
        self.out.clear();
        let Some(c) = self.channel_index(address) else {
            return &self.out;
        };
        for key in 0..KEYS {
            let at = c * KEYS + key;
            if self.held[at].get(Held::PRESSURE).is_some_and(|v| v != 0) {
                self.control(address, NoteNumber::masked(key as u8), Held::PRESSURE, 0);
            }
        }
        &self.out
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
            Voice2::RegisteredPerNote {
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
            Voice2::PerNotePitchBend {
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
            Voice2::PerNoteManagement {
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
            Voice2::Registered {
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

    fn pitch_event(id: NoteId, note: u8, hcu: u32) -> NoteEvent {
        NoteEvent::Control {
            id,
            note: key(note),
            control: NoteControl::Pitch(Pitch::from_q7_25(hcu << 25)),
        }
    }

    /// The sounding pitch of a `Pitch` event, as a fractional note number.
    fn sounding(e: &NoteEvent) -> f32 {
        match e {
            NoteEvent::Control {
                control: NoteControl::Pitch(p),
                ..
            } => p.note_number(),
            other => panic!("not a pitch: {other:?}"),
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
        assert_eq!(e[1], pitch_event(id, 0, 60));
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
        assert_eq!(e[1], pitch_event(new, 5, 60));
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
            Voice2::NoteOff {
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
        assert_eq!(start(&mut s, a, 5).len(), 1, "the next note is at its key");
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
        let on_0 = sounding(&bend(&mut s, at(0, 0), 5, 0xC000_0000)[0]);
        let on_1 = sounding(&bend(&mut s, at(0, 1), 5, 0xC000_0000)[0]);
        assert!((on_0 - 11.0).abs() < 1e-3, "{on_0}");
        assert!((on_1 - 29.0).abs() < 1e-3, "{on_1}");
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
        assert!((sounding(&e[0]) - 6.0).abs() < 1e-3, "key 5 plus 1");
    }

    /// M2-104 section 7.4.15.3: Attribute Pitch 7.9 is that note's pitch, and
    /// "is not valid for any subsequent Notes".
    #[test]
    fn an_attribute_pitch_is_for_that_note_alone() {
        let mut s = state();
        let a = at(0, 0);
        let e = s.start(a, key(0), Attribute::Pitch(60 << 9)).to_vec();
        let id = started(&e);
        assert_eq!(e.last(), Some(&pitch_event(id, 0, 60)));
        assert_eq!(start(&mut s, a, 0).len(), 1, "the key's");
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
        assert_eq!(sounding(&start(&mut s, at(1, 0), 5)[1]), 60.0);
        // A sensitivity of one Group leaves the other's.
        sensitivity(&mut s, at(1, 0), 12);
        let on_0 = sounding(&bend(&mut s, at(0, 0), 5, 0xC000_0000)[0]);
        let on_1 = sounding(&bend(&mut s, at(1, 0), 5, 0xC000_0000)[0]);
        assert!(
            (on_0 - 29.0).abs() < 1e-3 && (on_1 - 66.0).abs() < 1e-3,
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
            assert_eq!(s.step(a, m), &[], "{m:?}");
        }
        assert_eq!(sounding(&start(&mut s, a, 5)[1]), 60.0);
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
            (sounding(&e[0]) - 29.0).abs() < 1e-3,
            "the default sensitivity"
        );
    }

    /// M2-104 section 7.4.15.2: Per-Note Pitch Bend "acts as an offset from the
    /// pitch set by Registered Per-Note Controller #3". A pitch told to a live
    /// note and then a bend sounds as their sum.
    #[test]
    fn a_pitch_then_a_bend_sounds_as_the_sum() {
        let mut s = state();
        let a = at(0, 0);
        let id = started(&start(&mut s, a, 5));
        let e = pitch_of(&mut s, a, 5, PITCH_60 + (1 << 24));
        assert_eq!(e.len(), 1);
        assert!((sounding(&e[0]) - 60.5).abs() < 1e-4);
        // Half the wheel up over 48 semitones: +24.
        let e = bend(&mut s, a, 5, 0xC000_0000);
        assert!(matches!(e[..], [NoteEvent::Control { id: i, .. }] if i == id));
        assert!((sounding(&e[0]) - 84.5).abs() < 1e-4, "{e:?}");
    }

    /// The same two messages in the other order sound the same: the sum does
    /// not depend on which arrived last.
    #[test]
    fn a_bend_then_a_pitch_sounds_as_the_sum_too() {
        let mut s = state();
        let a = at(0, 0);
        start(&mut s, a, 5);
        let e = bend(&mut s, a, 5, 0xC000_0000);
        assert!((sounding(&e[0]) - 29.0).abs() < 1e-4, "key 5 plus 24");
        let e = pitch_of(&mut s, a, 5, PITCH_60 + (1 << 24));
        assert!((sounding(&e[0]) - 84.5).abs() < 1e-4, "{e:?}");
        // And a note that starts after both is handed the sum at once.
        let e = start(&mut s, a, 5);
        assert!((sounding(&e[1]) - 84.5).abs() < 1e-4, "{e:?}");
    }

    /// M2-104 section 7.4.15.3: Per-Note Pitch Bend offsets from the Attribute
    /// Pitch, and the attribute wins over the number's held Pitch 7.25.
    #[test]
    fn an_attribute_pitch_plus_a_later_bend() {
        let mut s = state();
        let a = at(0, 0);
        pitch_of(&mut s, a, 5, PITCH_60);
        let e = s.start(a, key(5), Attribute::Pitch(70 << 9)).to_vec();
        assert!((sounding(e.last().unwrap()) - 70.0).abs() < 1e-4, "{e:?}");
        let e = bend(&mut s, a, 5, 0xC000_0000);
        assert!((sounding(&e[0]) - 94.0).abs() < 1e-4, "{e:?}");
    }

    /// M2-104 section 7.4.13: the bend is a position over the sensitivity, so a
    /// new sensitivity re-sends the rescaled sum of the told pitch and the bend.
    #[test]
    fn a_new_sensitivity_resends_the_rescaled_sum() {
        let mut s = state();
        let a = at(0, 0);
        start(&mut s, a, 5);
        pitch_of(&mut s, a, 5, PITCH_60);
        bend(&mut s, a, 5, 0xC000_0000);
        let e = sensitivity(&mut s, a, 4);
        assert_eq!(e.len(), 1);
        assert!(
            (sounding(&e[0]) - 62.0).abs() < 1e-4,
            "60 plus half of 4: {e:?}"
        );
    }

    /// M2-104 section 7.4.5, Reset: the note's controllers return to their
    /// defaults, so its pitch is its key's: the next note and the one reset
    /// (by the consumer, on the `Reset` event) are at the key; a bend told after
    /// the Reset is an offset from the key.
    #[test]
    fn a_reset_returns_the_sounding_pitch_to_the_key() {
        let mut s = state();
        let a = at(0, 0);
        let id = started(&start(&mut s, a, 5));
        pitch_of(&mut s, a, 5, PITCH_60);
        bend(&mut s, a, 5, 0xC000_0000);
        assert_eq!(
            management(&mut s, a, 5, false, true),
            vec![NoteEvent::Reset { id }]
        );
        let e = bend(&mut s, a, 5, 0xC000_0000);
        assert!(
            (sounding(&e[0]) - 29.0).abs() < 1e-4,
            "key 5 plus 24: {e:?}"
        );
    }

    /// M2-104 section 7.4.5, Detach: the note keeps what it has, so a pitch, a
    /// bend and a sensitivity after it send that note nothing.
    #[test]
    fn a_detached_note_hears_no_pitch_bend_or_sensitivity() {
        let mut s = state();
        let a = at(0, 0);
        start(&mut s, a, 5);
        management(&mut s, a, 5, true, false);
        assert_eq!(pitch_of(&mut s, a, 5, PITCH_60), vec![]);
        assert_eq!(bend(&mut s, a, 5, 0xC000_0000), vec![]);
        assert_eq!(sensitivity(&mut s, a, 12), vec![]);
    }

    /// The sum saturates at the ends of the pitch range (Q7.25 holds 0 to
    /// just under 128): a full bend down from key 5 over 48 semitones is 0.
    #[test]
    fn a_sum_past_the_range_saturates() {
        let mut s = state();
        let a = at(0, 0);
        start(&mut s, a, 5);
        let e = bend(&mut s, a, 5, 0);
        assert_eq!(sounding(&e[0]), 0.0);
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
