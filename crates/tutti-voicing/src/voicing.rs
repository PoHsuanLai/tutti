//! The note logic of an instrument, as one pure step.
//!
//! `Voicing::step(channel, message, patch, voices)` reads one channel voice
//! message and the voices that exist, and returns the [`Command`]s the
//! instrument must carry out. It renders no audio, owns no voice and allocates
//! nothing after [`Voicing::new`]. What differs between instruments is a value
//! of [`Rules`] and a [`Patch`]; the logic below is the same for all of them.
//!
//! The core serves one Group's sixteen channels (M2-104 section 2.1.2: a Group's
//! channels are separate from every other Group's): an instrument that plays
//! one Group reads its messages and gives `step` the channel, so no Group is
//! ever merged into these tables.
//!
//! Reading order: [`Rules`] (the declared differences), [`Command`] (what comes
//! out), then `Voicing::step`, which is the whole of it.
//!
//! # What the instrument tells the core
//!
//! The voices that exist, as a slice of [`VoiceView`] **sorted by [`VoiceId`]**
//! (ids only grow, so an instrument that appends each new voice and removes
//! each ended one keeps it sorted for free). The core looks voices up by
//! binary search and reads liveness only when a note starts: a note whose voice
//! has ended stays in the core's table until then, so a command may name a
//! voice that has since ended. The instrument ignores a command for a voice it
//! no longer has.
//!
//! # Rules that are tutti's own
//!
//! MIDI 1.0 does not say what a repeated key, a full pool, or a mono
//! instrument whose key is let go while another is still held should do. The
//! core's answers are named in [`Retrigger`], [`Steal`] and the mono rule below,
//! and are the same for every instrument that does not say otherwise.
//!
//! **Mono and legato play the last note, with a stack of the held keys** (the
//! usual "last-note priority"): releasing the sounding key while others are
//! held returns to the most recent of them (a legato instrument moves its voice
//! there, a mono one restarts), and only when no key is held does a release
//! happen, subject to the pedals. The stack of a channel holds [`HELD_KEYS`]
//! keys; past that the oldest is forgotten. A layered note keeps its layers
//! across keys: a prior voice is taken over by the new key's voice with the
//! same [`VoiceSpec::tag`].
//!
//! **Legato and glide follow M1 pp.14 to 17**, per channel: the Legato Footswitch
//! (CC 68) makes the channel [`Mode::Legato`] while it is down; a Portamento
//! Control (CC 84) names the source key of the next Note On on its channel (and
//! only that one), which then glides from it, or, in a polyphonic channel with a
//! voice sounding at the source key, moves that voice with no new attack;
//! Portamento On (CC 65) glides a note from the key struck before it. The
//! [`Glide`] on a [`Command::Start`] or [`Command::Retarget`] says from which key
//! and for how long (the channel's Portamento Time). What a channel holds
//! besides notes is its [`ChannelState`].

use tutti_midi_protocol::{MpeZones, bend_u32_to_signed_f32, narrow_value, u32_to_unit_f32};
use tutti_types::{Amplitude, Seconds, Semitones};
use tutti_ump::{
    Address, Attribute, CCNumber, MidiChannel, MidiGroup, NoteId, NoteNumber, Rpn, U7, Voice2,
};

use crate::channel::{Bend, ChannelState};
use crate::per_note::{MAX_EVENTS as NOTE_EVENTS, NOTE_CONTROLS};
use crate::{Controller, Effect, Holds, NoteControl, NoteEvent, PerNoteState, Phase, Stop};

const CHANNELS: usize = MidiChannel::COUNT as usize;

/// A channel's pitch bend sensitivity from the data of RPN 0 (MIDI 1.0 RP-003,
/// M2-115 section 4.1): two fields, the semitones in the top 7 bits and the
/// cents (0 to 99) in the 7 below, the rest unused. It is not the Q7.25 of the
/// per-note sensitivity (RPN 0/7), whose fraction is binary.
fn bend_sensitivity(value: u32) -> Semitones {
    let semitones = (value >> 25) as f32;
    let cents = ((value >> 18) & 0x7F) as f32;
    Semitones(semitones + cents / 100.0)
}

/// The most voices one note may start. A patch that asks for more is cut short.
const MAX_SPECS: usize = 32;

/// The keys a mono or legato channel remembers as held.
const HELD_KEYS: usize = 16;

/// The release velocity of a release no note-off gave one to (a pedal lifting,
/// a panic): the centre of the 16-bit range, M2-104 Appendix D.3.1's value for a
/// MIDI 1.0 note-off.
const NEUTRAL_RELEASE: u16 = 0x8000;

/// A voice, named in the order the core started them. Ids only grow and are 64
/// bits wide, so two voices never share one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct VoiceId(pub u64);

/// How many voices sound for a channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Any number, up to the polyphony.
    Poly,
    /// One note for each channel: a new key takes over the voices the channel
    /// has.
    Mono,
    /// One note for each channel: a new key while one is held moves the voices
    /// to the new key without a new attack.
    Legato,
}

/// What a key struck again does while an earlier strike of it sounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Retrigger {
    /// Releases the earlier strike's voices at once, whatever holds them.
    Replace,
    /// Leaves them sounding.
    Layer,
}

/// How a voice is chosen to give way when every slot is taken. A voice already
/// released always goes before any held one; among released voices the quietest
/// goes first. A strike never takes a voice it started itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Order {
    /// The one started longest ago.
    Oldest,
    /// The one started last.
    Newest,
    /// The one with the lowest envelope level.
    Quietest,
    /// The highest key.
    HighestNote,
    /// The lowest key.
    LowestNote,
}

/// What happens to a note when no slot is free.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Steal {
    /// The note is dropped; nothing sounding is disturbed.
    Never,
    /// A sounding voice gives way, by [`Order`].
    By(Order),
}

/// The declared differences between instruments.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rules {
    /// Poly, mono or legato.
    pub mode: Mode,
    /// A key struck again.
    pub retrigger: Retrigger,
    /// The most voices sounding at once.
    pub polyphony: usize,
    /// Who gives way when that is reached.
    pub steal: Steal,
    /// Whether a voice with a group number ends the others of its group and
    /// scope (see [`VoiceSpec`]).
    pub exclusive_groups: bool,
    /// Whether CC 66 holds the notes sounding when it goes down.
    pub sostenuto: bool,
    /// What a full pitch bend spans on a channel until RPN 0 says otherwise.
    pub bend_range: Semitones,
    /// What a full Per-Note Pitch Bend spans on a channel until RPN 0/7 says
    /// otherwise (M2-104 section 7.4.13 gives no default).
    pub per_note_bend_range: Semitones,
}

/// One voice a note becomes: what [`Patch::voices`] hands the core.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VoiceSpec {
    /// Its exclusive group, `0` for none. A voice of a group, when it starts,
    /// ends every other voice of the same group and scope that a different
    /// strike started, on any channel. The voices one strike starts (a stereo
    /// pair) never end each other.
    pub group: u16,
    /// What a group is scoped to: a plain id the patch chooses (a SoundFont
    /// gives its preset's number). Groups of different scopes are unrelated.
    pub scope: u32,
    /// The patch's own name for the voice (a region number), given back in
    /// [`Command::Start`]. It is also how a mono or legato channel matches a
    /// prior voice to the same layer of the next key: equal tags are the same
    /// layer.
    pub tag: u32,
}

/// A glide a note starts or moves with: the voice's pitch starts at `from`'s
/// and reaches the note's own in `time`, at a constant rate in cents (a straight
/// line in pitch), as the channel's Portamento Time says (General MIDI 2 section
/// 3.3.3). It comes from Portamento On (CC 65) or a Portamento Control (CC 84,
/// M1 p.16: the key "the subsequent note is gliding from").
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Glide {
    /// The key the glide leaves. Its pitch is the key's, with the channel's
    /// bend on it as on the note's own ("Pitch bend offsets the pitch used by
    /// both the Portamento Control starting note and the target Note-On").
    pub from: NoteNumber,
    /// How long it takes to arrive.
    pub time: Seconds,
}

/// Where a Note On's glide would come from, resolved from the channel before the
/// note is placed.
#[derive(Debug, Clone, Copy)]
struct GlideSource {
    /// The Portamento Control's key, which the note consumes.
    ptc: Option<NoteNumber>,
    /// The key last struck on the channel, if Portamento is on.
    previous: Option<NoteNumber>,
}

impl GlideSource {
    /// The key a glide leaves for a note that starts: the Portamento Control's,
    /// else the previous key.
    fn start(self) -> Option<NoteNumber> {
        self.ptc.or(self.previous)
    }
}

/// A patch says which voices a note starts.
pub trait Patch {
    /// Calls `spec` once for each voice a strike of `key` at `velocity` on
    /// `channel` starts. A patch that starts one voice calls it once.
    fn voices(
        &self,
        channel: MidiChannel,
        key: NoteNumber,
        velocity: u16,
        spec: &mut dyn FnMut(VoiceSpec),
    );
}

/// What the instrument tells the core about a voice that exists.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VoiceView {
    /// The voice.
    pub id: VoiceId,
    /// Its envelope level now.
    pub level: Amplitude,
}

/// What the instrument must do, in order.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Command {
    /// Start a voice. With `steal`, in the slot that voice leaves, which it
    /// cuts; without, in a free slot.
    Start {
        /// The new voice.
        id: VoiceId,
        /// Its channel.
        channel: MidiChannel,
        /// Its key.
        key: NoteNumber,
        /// The strike's velocity, 16 bits, at least 1.
        velocity: u16,
        /// What the patch said.
        spec: VoiceSpec,
        /// The voice it takes the slot of.
        steal: Option<VoiceId>,
        /// The glide it starts with, if the channel has Portamento on or a
        /// Portamento Control pending.
        glide: Option<Glide>,
    },
    /// Move a sounding voice to a new key without a new attack (legato, or the
    /// voice a Portamento Control names in a polyphonic channel).
    Retarget {
        /// The voice.
        id: VoiceId,
        /// The new key.
        key: NoteNumber,
        /// The new velocity.
        velocity: u16,
        /// The glide to the new key, if there is one: the voice's pitch
        /// starts at the glide's key (an immediate jump there, M1 p.16) and
        /// reaches the new key's in the glide's time; without one it moves at
        /// once.
        glide: Option<Glide>,
    },
    /// Let a voice go into its release.
    Release {
        /// The voice.
        id: VoiceId,
        /// The release velocity, 16 bits: the note-off's, or the neutral value
        /// (0x8000) when a pedal or a panic let it go.
        velocity: u16,
    },
    /// Silence a voice at once and free its slot.
    Cut(VoiceId),
    /// Let a voice go into a short release of the instrument's own choosing,
    /// whatever its envelope's release is and whatever holds it: the "rapid
    /// termination" of an exclusive class (SF2 2.04 section 8.1.2, generator
    /// 57). The voice still exists until it fades; the core counts it as
    /// released (so it is the first to give way), and a later note-off, pedal
    /// or panic has nothing left to release in it. Only a voice of an
    /// exclusive group is ever damped, so an instrument without
    /// [`Rules::exclusive_groups`] never sees this command.
    Damp(VoiceId),
    /// A per-note controller of a voice's Note Number took a value (M2-104
    /// section 7.4): sent when the message arrives, for each voice of that note
    /// that has not been detached, and when a voice starts, for each value the
    /// note already holds (a controller is persistent state, section 7.4.15.2).
    Control {
        /// The voice.
        id: VoiceId,
        /// The controller and its value.
        control: NoteControl,
    },
    /// A voice's per-note controllers return to their defaults (Per-Note
    /// Management Reset, section 7.4.5), whatever it keeps sounding.
    ResetControls(VoiceId),
    /// A channel's pitch moved: its notes sound `bend` away from their keys.
    /// Sent for each channel a bend, its sensitivity or a reset reaches.
    Retune {
        /// The channel.
        channel: MidiChannel,
        /// The wheel's position times the sensitivity, plus (in an MPE zone's
        /// member) the master's.
        bend: Semitones,
    },
}

#[derive(Debug, Clone, Copy)]
struct Note {
    id: VoiceId,
    /// The per-note id of the strike that started it (or last moved it): what
    /// [`PerNoteState`] addresses its controllers by.
    note: NoteId,
    /// The strike that started it (or last moved it).
    strike: u64,
    channel: MidiChannel,
    key: NoteNumber,
    group: u16,
    scope: u32,
    tag: u32,
    holds: Holds,
    released: bool,
    /// Already told to fade quickly: told only once.
    damped: bool,
    /// What the note-off said, for the release a pedal later lets through.
    off_velocity: u16,
}

/// A channel's held keys, most recent last. Fixed capacity.
#[derive(Debug, Clone, Copy)]
struct Held {
    len: usize,
    keys: [(NoteNumber, u16); HELD_KEYS],
}

impl Held {
    const NONE: Held = Held {
        len: 0,
        keys: [(NoteNumber::MAX, 0); HELD_KEYS],
    };

    /// `key` is the most recent. The oldest goes if the stack is full.
    fn push(&mut self, key: NoteNumber, velocity: u16) {
        self.remove(key);
        if self.len == HELD_KEYS {
            self.keys.copy_within(1.., 0);
            self.len -= 1;
        }
        self.keys[self.len] = (key, velocity);
        self.len += 1;
    }

    /// Takes `key` out; whether it was there.
    fn remove(&mut self, key: NoteNumber) -> bool {
        let Some(at) = self.keys[..self.len].iter().position(|k| k.0 == key) else {
            return false;
        };
        self.keys.copy_within(at + 1..self.len, at);
        self.len -= 1;
        true
    }

    fn top(&self) -> Option<(NoteNumber, u16)> {
        self.len.checked_sub(1).map(|i| self.keys[i])
    }
}

/// Whether `channel` is `master` itself or a member of the zone `master`
/// manages (with no zones, only itself).
fn in_zone(zones: Option<MpeZones>, master: MidiChannel, channel: MidiChannel) -> bool {
    channel == master
        || zones
            .and_then(|z| z.zone_of(channel).copied())
            .is_some_and(|zone| zone.is_member_channel(channel) && zone.master_channel() == master)
}

/// The note logic. See the module docs.
#[derive(Debug)]
pub struct Voicing {
    rules: Rules,
    /// Sorted by id: voices are appended as they start.
    notes: Vec<Note>,
    sustain: [bool; CHANNELS],
    /// What each channel has been told that outlives its notes.
    channels: [ChannelState; CHANNELS],
    zones: Option<MpeZones>,
    next: u64,
    strike: u64,
    held: [Held; CHANNELS],
    /// The per-note controllers (section 7.4), for this Group's channels.
    per_note: PerNoteState,
    /// What the per-note state said of the message being read.
    events: Vec<NoteEvent>,
    /// Per-note ids whose voices left the table in this step.
    retired: Vec<NoteId>,
    /// The id of the note being started, which the voices it starts take.
    strike_note: NoteId,
    out: Vec<Command>,
}

impl Clone for Voicing {
    /// A copy with its tables as roomy as the original's: a derived clone
    /// would shrink them to their length, and `step` must never grow them.
    fn clone(&self) -> Self {
        let mut notes = Vec::with_capacity(self.notes.capacity());
        notes.extend_from_slice(&self.notes);
        Self {
            rules: self.rules,
            notes,
            sustain: self.sustain,
            channels: self.channels,
            zones: self.zones,
            next: self.next,
            strike: self.strike,
            held: self.held,
            per_note: self.per_note.clone(),
            events: Vec::with_capacity(self.events.capacity()),
            retired: Vec::with_capacity(self.retired.capacity()),
            strike_note: self.strike_note,
            out: Vec::with_capacity(self.out.capacity()),
        }
    }
}

impl Voicing {
    /// A core for `rules`. Allocates its tables; `step` never does.
    #[must_use]
    pub fn new(rules: Rules) -> Self {
        Self {
            notes: Vec::with_capacity(rules.polyphony + 1),
            out: Vec::with_capacity(
                2 * rules.polyphony + CHANNELS + MAX_SPECS * (2 + NOTE_CONTROLS),
            ),
            rules,
            sustain: [false; CHANNELS],
            channels: [ChannelState::new(rules.bend_range); CHANNELS],
            zones: None,
            next: 1,
            strike: 0,
            held: [Held::NONE; CHANNELS],
            per_note: PerNoteState::new(1, rules.per_note_bend_range),
            events: Vec::with_capacity(NOTE_EVENTS),
            retired: Vec::with_capacity(2 * rules.polyphony + MAX_SPECS + 2),
            strike_note: NoteId::from_raw(0),
        }
    }

    /// Which channels are MPE zones (`None`: no MPE). A zone master's pitch
    /// bend adds to each of its members' own (RP-053); with no zone every
    /// channel is its own. No command is returned: the instrument sets this
    /// when it learns the layout.
    pub fn set_zones(&mut self, zones: Option<MpeZones>) {
        self.zones = zones;
    }

    /// Forgets every note, pedal, controller, bend, held key and per-note
    /// controller: a fresh start. No command is returned; the instrument returns
    /// its pitches to centre. Ids are not reused.
    pub fn reset(&mut self) {
        self.forget_notes();
        self.per_note.clear();
        self.channels = [ChannelState::new(self.rules.bend_range); CHANNELS];
    }

    /// Forgets every note, pedal and held key, and keeps the channels'
    /// controllers and the per-note controllers: a channel's wheel, sensitivity
    /// and modulation, and what a Note Number was told, are controller state,
    /// which outlives its notes.
    /// No command is returned; the instrument ends its voices.
    pub fn forget_notes(&mut self) {
        for n in &self.notes {
            self.per_note.end(n.note);
        }
        self.notes.clear();
        self.sustain = [false; CHANNELS];
        self.held = [Held::NONE; CHANNELS];
    }

    /// Where `channel`'s pitch stands now: its wheel over its sensitivity, and
    /// its zone master's if it is a member. What the last
    /// [`Command::Retune`] for it said.
    #[must_use]
    pub fn channel_pitch(&self, channel: MidiChannel) -> Semitones {
        self.pitch(usize::from(channel.get()))
    }

    /// Every channel's [`ChannelState`], by channel number.
    #[must_use]
    pub fn channels(&self) -> &[ChannelState; MidiChannel::COUNT as usize] {
        &self.channels
    }

    /// What `channel` has been told apart from its notes: its controllers,
    /// pressure, pitch wheel and tuning, and portamento switches. The
    /// instrument reads the ones it renders (modulation, volume, pan,
    /// expression) from it when it makes a block.
    #[must_use]
    pub fn channel(&self, channel: MidiChannel) -> &ChannelState {
        &self.channels[usize::from(channel.get())]
    }

    /// What holds `id` open, if it is a voice the core knows and has not
    /// released (a released note is held by nothing; its [`phase`](Self::phase)
    /// says so).
    #[must_use]
    pub fn holds(&self, id: VoiceId) -> Option<Holds> {
        self.note(id).filter(|n| !n.released).map(|n| n.holds)
    }

    /// What holds `id` open or whether it is released, if the core knows it.
    #[must_use]
    pub fn phase(&self, id: VoiceId) -> Option<Phase> {
        self.note(id).map(|n| match (n.released, n.holds.key()) {
            (true, _) => Phase::Releasing,
            (false, true) => Phase::Key,
            (false, false) => Phase::Pedal,
        })
    }

    fn note(&self, id: VoiceId) -> Option<&Note> {
        self.notes
            .binary_search_by_key(&id, |n| n.id)
            .ok()
            .map(|i| &self.notes[i])
    }

    fn push(&mut self, c: Command) {
        debug_assert!(self.out.len() < self.out.capacity(), "command buffer full");
        if self.out.len() < self.out.capacity() {
            self.out.push(c);
        }
    }

    /// Reads one message on `channel`, with the voices that exist (sorted by
    /// id), and returns what to do. The slice is valid until the next call.
    pub fn step<P: Patch + ?Sized>(
        &mut self,
        channel: MidiChannel,
        message: &Voice2,
        patch: &P,
        voices: &[VoiceView],
    ) -> &[Command] {
        self.out.clear();
        self.events.clear();
        self.retired.clear();
        match *message {
            // A MIDI 2.0 Note On at velocity 0 is a Note On (M2-104 section
            // 7.4.2), played as quietly as there is. A MIDI 1.0 one is a Note
            // Off, and `widen` has already made it one.
            Voice2::NoteOn {
                note,
                velocity,
                attribute,
            } => {
                self.begin(channel, note, attribute);
                self.note_on(channel, note, velocity.max(1), patch, voices);
            }
            Voice2::NoteOff { note, velocity, .. } => {
                self.note_off(channel, note, velocity, patch, voices);
            }
            Voice2::ControlChange { controller, value } => {
                let value = narrow_value(value);
                match Controller::effect(controller, value) {
                    Some(effect) => self.effect(channel, effect),
                    None => self.controller(channel, controller, value),
                }
            }
            Voice2::ChannelPressure(pressure) => {
                let pressure = u32_to_unit_f32(pressure);
                self.channels[usize::from(channel.get())].set_pressure(pressure);
            }
            Voice2::PitchBend(bend) => {
                let position = bend_u32_to_signed_f32(bend);
                self.bend(channel, |b| b.position = position);
            }
            // The channel's registered parameters: the pitch bend sensitivity
            // (RPN 0), the Channel Fine and Coarse Tuning (RP-022: RPN 1 and 2) and
            // the Modulation Depth Range (CA-026: RPN 5). The data is the MIDI
            // 1.0 pair zero-extended (M2-115 section 4.1): `MSB << 7 | LSB` in
            // the top 14 bits.
            Voice2::Registered { bank, index, value } => match Rpn::new(bank, index) {
                Rpn::PITCH_BEND_SENSITIVITY => {
                    let range = bend_sensitivity(value);
                    self.bend(channel, |b| b.range = range);
                }
                Rpn::CHANNEL_FINE_TUNING => {
                    // 14 bits centred at 0x2000, a semitone over the half range.
                    let fine = Semitones((value >> 18) as f32 / 8192.0 - 1.0);
                    self.bend(channel, |b| b.fine = fine);
                }
                Rpn::CHANNEL_COARSE_TUNING => {
                    // The MSB, centred at 64, in semitones.
                    let coarse = Semitones((value >> 25) as f32 - 64.0);
                    self.bend(channel, |b| b.coarse = coarse);
                }
                Rpn::MODULATION_DEPTH_RANGE => {
                    self.channels[usize::from(channel.get())].set_modulation_range(value);
                }
                _ => {
                    let address = Self::address(channel);
                    self.events
                        .extend_from_slice(self.per_note.step(address, message));
                }
            },
            // The per-note controllers, Per-Note Management and RPN 0/7 are
            // `PerNoteState`'s; this core carries what it says to the voices.
            Voice2::RegisteredPerNote { .. }
            | Voice2::AssignablePerNote { .. }
            | Voice2::PerNotePitchBend { .. }
            | Voice2::PolyPressure { .. }
            | Voice2::PerNoteManagement { .. } => {
                let address = Self::address(channel);
                self.events
                    .extend_from_slice(self.per_note.step(address, message));
            }
            _ => {}
        }
        self.deliver();
        self.settle();
        &self.out
    }

    /// A Control Change the instrument does not own: the channel's 14-bit
    /// controllers and Portamento Time ([`ChannelState`]), and the three that
    /// change how notes are placed: Portamento On/Off (CC 65, "recognized as
    /// OFF" below 64, GM2 section 3.3.8), the Legato Footswitch (CC 68, M1
    /// p.14: 40 to 7F is Legato) and Portamento Control (CC 84, M1 p.16). Any
    /// other controller is the instrument's own.
    fn controller(&mut self, channel: MidiChannel, controller: CCNumber, value: U7) {
        let ch = usize::from(channel.get());
        match controller {
            CCNumber::PORTAMENTO_SWITCH => self.channels[ch].portamento = Controller::pedal(value),
            CCNumber::LEGATO => {
                let down = Controller::pedal(value);
                self.channels[ch].legato = down;
                // The keys a legato channel held are only a stack while the
                // channel is monophonic.
                if !down && self.rules.mode == Mode::Poly {
                    self.held[ch] = Held::NONE;
                }
            }
            // "A single Portamento Control message only affects the next Note-On
            // received on the matching channel (i.e. it is reset after the
            // Note-On). Receiving a Portamento Control message does not affect
            // the pitch of any currently sustaining or releasing notes."
            CCNumber::PORTAMENTO_CONTROL => {
                self.channels[ch].glide_from = NoteNumber::new(value.get());
            }
            _ => self.channels[ch].control(controller, value),
        }
    }

    /// How many voices sound for `channel`: the rules' mode, or Legato while
    /// the Legato Footswitch is down ("turn monophonic legato response on and
    /// off", M1 p.14; "When turned off the voice assignment mode [...] returns
    /// to the state it was in").
    fn mode(&self, channel: MidiChannel) -> Mode {
        if self.channels[usize::from(channel.get())].legato {
            Mode::Legato
        } else {
            self.rules.mode
        }
    }

    /// Where a Note On's glide comes from, and the channel's key last struck.
    /// The Portamento Control is consumed; Portamento On starts from the key
    /// struck before.
    fn glide_source(&mut self, channel: MidiChannel, key: NoteNumber) -> GlideSource {
        let state = &mut self.channels[usize::from(channel.get())];
        let source = GlideSource {
            ptc: state.glide_from.take(),
            previous: state.last_key.filter(|_| state.portamento),
        };
        state.last_key = Some(key);
        source
    }

    /// The glide a note on `key` makes from `from`, if the keys differ.
    fn glide(
        &self,
        channel: MidiChannel,
        from: Option<NoteNumber>,
        key: NoteNumber,
    ) -> Option<Glide> {
        let from = from.filter(|&f| f != key)?;
        let time = self.channels[usize::from(channel.get())].glide_time(from, key);
        Some(Glide { from, time })
    }

    fn address(channel: MidiChannel) -> Address {
        Address {
            group: MidiGroup::FIRST,
            channel,
        }
    }

    /// Starts a note in the per-note state: its id is the one the voices this
    /// message starts take, and what it holds is delivered after them.
    fn begin(&mut self, channel: MidiChannel, key: NoteNumber, attribute: Attribute) {
        let events = self.per_note.start(Self::address(channel), key, attribute);
        if let Some(NoteEvent::Started { id, .. }) = events.first() {
            self.strike_note = *id;
        }
        self.events.extend_from_slice(events);
    }

    /// Turns what the per-note state said into commands for the voices of
    /// each note it names.
    fn deliver(&mut self) {
        for e in 0..self.events.len() {
            let (note, control) = match self.events[e] {
                NoteEvent::Started { .. } => continue,
                NoteEvent::Control { id, control, .. } => (id, Some(control)),
                NoteEvent::Reset { id } => (id, None),
            };
            for i in 0..self.notes.len() {
                if self.notes[i].note == note {
                    let id = self.notes[i].id;
                    self.push(match control {
                        Some(control) => Command::Control { id, control },
                        None => Command::ResetControls(id),
                    });
                }
            }
        }
    }

    /// The voices `patch` makes of a strike.
    fn specs<P: Patch + ?Sized>(
        patch: &P,
        channel: MidiChannel,
        key: NoteNumber,
        velocity: u16,
    ) -> ([VoiceSpec; MAX_SPECS], usize) {
        let mut specs = [VoiceSpec {
            group: 0,
            scope: 0,
            tag: 0,
        }; MAX_SPECS];
        let mut count = 0;
        patch.voices(channel, key, velocity, &mut |s| {
            if count < MAX_SPECS {
                specs[count] = s;
                count += 1;
            }
        });
        (specs, count)
    }

    fn note_on<P: Patch + ?Sized>(
        &mut self,
        channel: MidiChannel,
        key: NoteNumber,
        velocity: u16,
        patch: &P,
        voices: &[VoiceView],
    ) {
        self.strike += 1;
        self.reconcile(voices);
        let source = self.glide_source(channel, key);
        if self.mode(channel) != Mode::Poly {
            self.play_mono(channel, key, velocity, patch, voices, source, false);
            return;
        }
        // A Portamento Control names a key that may sound: "If there is a
        // currently sounding voice whose note number is coincident with the
        // source note number, the voice's pitch will glide to the new Note-On's
        // pitch according to the portamento time without re-attacking. Then, no
        // new voice should be assigned." (M1 p.16)
        if let Some(from) = source.ptc
            && self.glide_voices(channel, from, key, velocity)
        {
            return;
        }
        if self.rules.retrigger == Retrigger::Replace {
            self.release_where(|n| n.channel == channel && n.key == key);
        }
        let (specs, count) = Self::specs(patch, channel, key, velocity);
        for &spec in &specs[..count] {
            self.start_poly(channel, key, velocity, spec, voices, source.start());
        }
    }

    /// Moves the sounding voices of `from` on `channel` to `key`, gliding, with
    /// no new attack. Whether there were any.
    fn glide_voices(
        &mut self,
        channel: MidiChannel,
        from: NoteNumber,
        key: NoteNumber,
        velocity: u16,
    ) -> bool {
        let glide = self.glide(channel, Some(from), key);
        let strike = self.strike;
        let mut moved = false;
        for i in 0..self.notes.len() {
            let n = &mut self.notes[i];
            if n.channel == channel && n.key == from && !n.released {
                n.key = key;
                n.strike = strike;
                let id = n.id;
                let old = std::mem::replace(&mut n.note, self.strike_note);
                self.retire(old);
                self.push(Command::Retarget {
                    id,
                    key,
                    velocity,
                    glide,
                });
                moved = true;
            }
        }
        moved
    }

    /// Liveness is read on a note message only: a voice the instrument no
    /// longer has (it ended, as a sampler's does at its sample's end) leaves
    /// the table, so the polyphony, the steal and a return to a held key see
    /// what sounds.
    fn reconcile(&mut self, voices: &[VoiceView]) {
        let retired = &mut self.retired;
        self.notes.retain(|n| {
            let alive = voices.binary_search_by_key(&n.id, |v| v.id).is_ok();
            if !alive && retired.len() < retired.capacity() && !retired.contains(&n.note) {
                retired.push(n.note);
            }
            alive
        });
    }

    /// A voice left the table: its per-note id is ended once no voice of the
    /// same strike remains ([`settle`](Self::settle)).
    fn retire(&mut self, note: NoteId) {
        if self.retired.len() < self.retired.capacity() && !self.retired.contains(&note) {
            self.retired.push(note);
        }
    }

    /// Ends in the per-note state every note whose voices have all left the
    /// table, so its number stops moving it.
    fn settle(&mut self) {
        for i in 0..self.retired.len() {
            let note = self.retired[i];
            if !self.notes.iter().any(|n| n.note == note) {
                self.per_note.end(note);
            }
        }
    }

    /// A voice of an exclusive class ends the sounding voices of that class in
    /// its scope (SF2 2.04 section 8.1.2, generator 57), on any channel, never
    /// one of its own strike. The spec says "rapidly terminated", not cut: the
    /// voices are told to [`Command::Damp`], once, and count as released from
    /// then on. `except` is a (channel, tag) taken over in place instead
    /// (mono), which is not damped.
    fn damp_exclusive(&mut self, spec: VoiceSpec, except: Option<(MidiChannel, u32)>) {
        if !self.rules.exclusive_groups || spec.group == 0 {
            return;
        }
        let strike = self.strike;
        for i in 0..self.notes.len() {
            let n = &mut self.notes[i];
            if n.group == spec.group
                && n.scope == spec.scope
                && n.strike != strike
                && !n.damped
                && except != Some((n.channel, n.tag))
            {
                n.damped = true;
                n.released = true;
                let id = n.id;
                self.push(Command::Damp(id));
            }
        }
    }

    fn start_poly(
        &mut self,
        channel: MidiChannel,
        key: NoteNumber,
        velocity: u16,
        spec: VoiceSpec,
        voices: &[VoiceView],
        from: Option<NoteNumber>,
    ) {
        self.damp_exclusive(spec, None);
        let steal = if self.notes.len() < self.rules.polyphony {
            None
        } else if let Some(v) = self.victim(voices) {
            Some(v)
        } else {
            return;
        };
        self.start(channel, key, velocity, spec, steal, from);
    }

    /// Mono and legato: the channel has one note, and a new key takes over the
    /// voices of the note it had, layer by layer (equal tags), sounding or
    /// released (a released tail is cut, as on a monosynth). `returning` is the
    /// return to a key still held after the sounding one was let go: the held
    /// stack is not pushed again.
    fn play_mono<P: Patch + ?Sized>(
        &mut self,
        channel: MidiChannel,
        key: NoteNumber,
        velocity: u16,
        patch: &P,
        voices: &[VoiceView],
        source: GlideSource,
        returning: bool,
    ) {
        let ch = usize::from(channel.get());
        let was_held = self.held[ch].top().is_some();
        if !returning {
            self.held[ch].push(key, velocity);
        }
        let sounding = self
            .notes
            .iter()
            .any(|n| n.channel == channel && !n.released);
        let legato = self.mode(channel) == Mode::Legato && (was_held || returning) && sounding;
        let portamento = self.channels[ch].portamento;
        let (specs, count) = Self::specs(patch, channel, key, velocity);
        let strike = self.strike;
        for &spec in &specs[..count] {
            self.damp_exclusive(spec, Some((channel, spec.tag)));
            // The prior voice of this layer, a sounding one before a tail.
            let prior = |notes: &[Note], released: bool| {
                notes.iter().position(|n| {
                    n.channel == channel
                        && n.tag == spec.tag
                        && n.strike != strike
                        && n.released == released
                })
            };
            let at = prior(&self.notes, false).or_else(|| prior(&self.notes, true));
            match at {
                Some(i) if legato && !self.notes[i].released => {
                    let id = self.notes[i].id;
                    // The voice leaves the Portamento Control's key (M1 p.16:
                    // "an immediate pitch jump to the key specified by the
                    // portamento Control message, and then a glide"), else, with
                    // Portamento on, the key it sounded.
                    let from = source.ptc.or(portamento.then_some(self.notes[i].key));
                    let glide = self.glide(channel, from, key);
                    self.notes[i].key = key;
                    self.notes[i].strike = strike;
                    let moved = std::mem::replace(&mut self.notes[i].note, self.strike_note);
                    self.retire(moved);
                    self.push(Command::Retarget {
                        id,
                        key,
                        velocity,
                        glide,
                    });
                }
                Some(i) => {
                    let id = self.notes[i].id;
                    self.start(channel, key, velocity, spec, Some(id), source.start());
                }
                None => self.start_poly(channel, key, velocity, spec, voices, source.start()),
            }
        }
        // Layers the new key lacks go.
        self.release_where(|n| n.channel == channel && n.strike != strike);
    }

    fn start(
        &mut self,
        channel: MidiChannel,
        key: NoteNumber,
        velocity: u16,
        spec: VoiceSpec,
        steal: Option<VoiceId>,
        from: Option<NoteNumber>,
    ) {
        let glide = self.glide(channel, from, key);
        if let Some(v) = steal
            && let Ok(i) = self.notes.binary_search_by_key(&v, |n| n.id)
        {
            let gone = self.notes.remove(i).note;
            self.retire(gone);
        }
        let id = VoiceId(self.next);
        self.next += 1;
        self.notes.push(Note {
            id,
            note: self.strike_note,
            strike: self.strike,
            channel,
            key,
            group: spec.group,
            scope: spec.scope,
            tag: spec.tag,
            holds: Holds::struck(),
            released: false,
            damped: false,
            off_velocity: NEUTRAL_RELEASE,
        });
        self.push(Command::Start {
            id,
            channel,
            key,
            velocity,
            spec,
            steal,
            glide,
        });
    }

    /// Who gives way, in one pass over the notes (which are in id order, so the
    /// first of equals is the oldest): released voices first, the quietest of
    /// them; then by [`Order`]. A strike's own voices are not candidates.
    fn victim(&self, voices: &[VoiceView]) -> Option<VoiceId> {
        let Steal::By(order) = self.rules.steal else {
            return None;
        };
        let mut seen = voices.iter().peekable();
        let mut best: Option<(&Note, f32)> = None;
        for n in &self.notes {
            while seen.next_if(|v| v.id < n.id).is_some() {}
            let level = seen
                .peek()
                .filter(|v| v.id == n.id)
                .map_or(0.0, |v| v.level.get());
            if n.strike == self.strike {
                continue;
            }
            let better = best.is_none_or(|(b, bl)| match (n.released, b.released) {
                (true, false) => true,
                (false, true) => false,
                (true, true) => level.total_cmp(&bl).is_lt(),
                (false, false) => match order {
                    Order::Oldest => false,
                    Order::Newest => true,
                    Order::Quietest => level.total_cmp(&bl).is_lt(),
                    Order::HighestNote => n.key > b.key,
                    Order::LowestNote => n.key < b.key,
                },
            });
            if better {
                best = Some((n, level));
            }
        }
        best.map(|(n, _)| n.id)
    }

    fn note_off<P: Patch + ?Sized>(
        &mut self,
        channel: MidiChannel,
        key: NoteNumber,
        velocity: u16,
        patch: &P,
        voices: &[VoiceView],
    ) {
        let ch = usize::from(channel.get());
        self.reconcile(voices);
        if self.mode(channel) != Mode::Poly {
            let was_top = self.held[ch].top().map(|k| k.0) == Some(key);
            if self.held[ch].remove(key) {
                // A held key under the sounding one changes nothing; the
                // sounding one's release returns to the most recent held key.
                if !was_top {
                    return;
                }
                if let Some((back, v)) = self.held[ch].top() {
                    self.strike += 1;
                    self.begin(channel, back, Attribute::None);
                    // The return glides from the key just let go, with Portamento
                    // on; it is a return, not a Portamento Control's note.
                    let source = GlideSource {
                        ptc: None,
                        previous: self.channels[ch].portamento.then_some(key),
                    };
                    self.channels[ch].last_key = Some(back);
                    self.play_mono(channel, back, v, patch, voices, source, true);
                    return;
                }
            }
        }
        let pedal = self.sustain[ch];
        self.each_note(
            |n| n.channel == channel && n.key == key,
            |n| {
                n.off_velocity = velocity;
                n.holds.key_up(pedal)
            },
        );
    }

    /// A pedal or a panic on `channel`. On a zone's master it is the whole
    /// zone's: M1-100 section 2.3.1, "certain MIDI messages (for example,
    /// Damper Pedal) can be expected to affect all Sounding Notes across the
    /// Manager Channel and all Member Channels", and they "should be sent only
    /// on a Zone's Manager Channel". On a member, or on a channel outside any
    /// zone, it is that channel's alone.
    fn effect(&mut self, channel: MidiChannel, effect: Effect) {
        let zones = self.zones;
        let covers = move |c: MidiChannel| in_zone(zones, channel, c);
        let on_channel = move |n: &Note| covers(n.channel);
        match effect {
            Effect::Sustain(down) => {
                self.each_channel(covers, |s, ch| s.sustain[ch] = down);
                if !down {
                    self.each_note(on_channel, |n| n.holds.sustain_up());
                }
            }
            Effect::Sostenuto(down) if self.rules.sostenuto => {
                if down {
                    self.each_note(on_channel, |n| {
                        n.holds.sostenuto_down();
                        false
                    });
                } else {
                    self.each_note(on_channel, |n| n.holds.sostenuto_up());
                }
            }
            Effect::Sostenuto(_) => {}
            // All Notes Off releases, and a pedal still holds what it holds.
            Effect::Stop(Stop::Release) => {
                self.each_channel(covers, |s, ch| s.held[ch] = Held::NONE);
                let pedals = self.sustain;
                self.each_note(on_channel, |n| {
                    n.holds.key_up(pedals[usize::from(n.channel.get())])
                });
            }
            Effect::Stop(Stop::Cut) => {
                self.each_channel(covers, |s, ch| s.held[ch] = Held::NONE);
                let mut i = 0;
                while i < self.notes.len() {
                    if covers(self.notes[i].channel) {
                        let id = self.notes[i].id;
                        self.push(Command::Cut(id));
                        let gone = self.notes.remove(i).note;
                        self.retire(gone);
                    } else {
                        i += 1;
                    }
                }
            }
            // Reset All Controllers (RP-015) puts the pedals up, centres the
            // wheel and returns the controllers it names ([`ChannelState`]); the
            // sensitivity, the tuning and the parameters stay. "Reset
            // polyphonic pressure for all notes to 0": the per-note state
            // sets each held Polyphonic Pressure of the channel to 0 (M2-104
            // Appendix B.2 leaves the new per-note controllers alone, and Poly
            // Pressure is not one of them). The parameter numbers go null in
            // the translator, which keeps the selection.
            Effect::ResetControllers => {
                self.each_channel(covers, |s, ch| {
                    s.sustain[ch] = false;
                    s.held[ch] = Held::NONE;
                    s.channels[ch].reset_controllers();
                    if let Some(c) = MidiChannel::new(ch as u8) {
                        let events = s.per_note.reset_pressure(Self::address(c));
                        s.events.extend_from_slice(events);
                    }
                });
                self.each_note(on_channel, |n| {
                    let a = n.holds.sustain_up();
                    let b = n.holds.sostenuto_up();
                    a || b
                });
                self.bend(channel, |b| b.position = 0.0);
            }
        }
    }

    /// Runs `f` for the index of each channel `covers` names.
    fn each_channel(
        &mut self,
        covers: impl Fn(MidiChannel) -> bool,
        mut f: impl FnMut(&mut Self, usize),
    ) {
        for ch in 0..CHANNELS {
            if MidiChannel::new(ch as u8).is_some_and(&covers) {
                f(self, ch);
            }
        }
    }

    /// The master of the zone `channel` is a member of, if any.
    fn master_of(&self, channel: usize) -> Option<usize> {
        let channel = MidiChannel::new(channel as u8)?;
        let zone = self.zones?.zone_of(channel).copied()?;
        zone.is_member_channel(channel)
            .then(|| usize::from(zone.master_channel().get()))
    }

    /// Where `channel`'s pitch stands: its own wheel, and its master's.
    fn pitch(&self, channel: usize) -> Semitones {
        let own = self.channels[channel].bend.semitones();
        self.master_of(channel)
            .map_or(own, |m| own + self.channels[m].bend.semitones())
    }

    /// Changes the wheel or the sensitivity of `channel`, and tells the
    /// instrument where every channel it moves ended up: its own, and, if it is
    /// a zone master, each member's.
    fn bend(&mut self, channel: MidiChannel, change: impl Fn(&mut Bend)) {
        let ch = usize::from(channel.get());
        change(&mut self.channels[ch].bend);
        for i in 0..CHANNELS {
            if (i == ch || self.master_of(i) == Some(ch))
                && let Some(channel) = MidiChannel::new(i as u8)
            {
                let bend = self.pitch(i);
                self.push(Command::Retune { channel, bend });
            }
        }
    }

    fn release_where(&mut self, which: impl Fn(&Note) -> bool) {
        self.each_note(which, |_| true);
    }

    /// For each unreleased note `which` picks, `f` says whether its holds are
    /// gone; if so it is released and the instrument is told.
    fn each_note(&mut self, which: impl Fn(&Note) -> bool, mut f: impl FnMut(&mut Note) -> bool) {
        for i in 0..self.notes.len() {
            let n = &mut self.notes[i];
            if !n.released && which(n) && f(n) {
                n.released = true;
                let (id, velocity) = (n.id, n.off_velocity);
                self.push(Command::Release { id, velocity });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::LIVE_NOTES;
    use tutti_midi_protocol::MpeZoneConfig;
    use tutti_midi_protocol::widen;
    use tutti_ump::{Attribute, CCNumber, Pitch, RegisteredPerNoteController, U7, Voice1};

    /// One voice per note, no group.
    struct One;
    impl Patch for One {
        fn voices(&self, _: MidiChannel, _: NoteNumber, _: u16, spec: &mut dyn FnMut(VoiceSpec)) {
            spec(VoiceSpec {
                group: 0,
                scope: 0,
                tag: 0,
            });
        }
    }

    /// Two layers per note, both in `group` of `scope` (a stereo pair when the
    /// group is not 0).
    struct Pair {
        group: u16,
        scope: u32,
    }
    impl Patch for Pair {
        fn voices(&self, _: MidiChannel, _: NoteNumber, _: u16, spec: &mut dyn FnMut(VoiceSpec)) {
            for tag in 0..2 {
                spec(VoiceSpec {
                    group: self.group,
                    scope: self.scope,
                    tag,
                });
            }
        }
    }

    /// One voice in `group` of `scope`.
    struct Grouped {
        scope: u32,
    }
    impl Patch for Grouped {
        fn voices(&self, _: MidiChannel, _: NoteNumber, _: u16, spec: &mut dyn FnMut(VoiceSpec)) {
            spec(VoiceSpec {
                group: 7,
                scope: self.scope,
                tag: 0,
            });
        }
    }

    const POLY: Rules = Rules {
        mode: Mode::Poly,
        retrigger: Retrigger::Replace,
        polyphony: 4,
        steal: Steal::By(Order::Oldest),
        exclusive_groups: false,
        sostenuto: true,
        bend_range: Semitones(2.0),
        per_note_bend_range: Semitones(48.0),
    };
    const MONO: Rules = Rules {
        mode: Mode::Mono,
        polyphony: 8,
        ..POLY
    };
    const LEGATO: Rules = Rules {
        mode: Mode::Legato,
        polyphony: 8,
        ..POLY
    };

    /// The instrument's side: the voices it has, in id order, carrying out the
    /// commands.
    struct Sim {
        core: Voicing,
        voices: Vec<VoiceView>,
    }
    fn id(n: u64) -> VoiceId {
        VoiceId(n)
    }
    fn release(n: u64) -> Command {
        Command::Release {
            id: id(n),
            velocity: NEUTRAL_RELEASE,
        }
    }

    impl Sim {
        fn new(rules: Rules) -> Self {
            Self {
                core: Voicing::new(rules),
                voices: vec![],
            }
        }
        fn send<P: Patch>(&mut self, c: u8, m: Voice2, p: &P) -> Vec<Command> {
            let cmds = self
                .core
                .step(MidiChannel::of(c), &m, p, &self.voices)
                .to_vec();
            for cmd in &cmds {
                match *cmd {
                    Command::Start { id, steal, .. } => {
                        self.voices.retain(|v| Some(v.id) != steal);
                        self.voices.push(VoiceView {
                            id,
                            level: Amplitude(1.0),
                        });
                    }
                    Command::Cut(id) => self.voices.retain(|v| v.id != id),
                    _ => {}
                }
            }
            cmds
        }
        fn on(&mut self, c: u8, k: u8) -> Vec<Command> {
            self.on_with(c, k, &One)
        }
        fn on_with<P: Patch>(&mut self, c: u8, k: u8, p: &P) -> Vec<Command> {
            self.send(
                c,
                Voice2::NoteOn {
                    note: NoteNumber::of(k),
                    velocity: 0x8000,
                    attribute: Attribute::None,
                },
                p,
            )
        }
        fn off(&mut self, c: u8, k: u8) -> Vec<Command> {
            self.off_with(c, k, &One)
        }
        fn off_with<P: Patch>(&mut self, c: u8, k: u8, p: &P) -> Vec<Command> {
            self.send(
                c,
                Voice2::NoteOff {
                    note: NoteNumber::of(k),
                    velocity: NEUTRAL_RELEASE,
                    attribute: Attribute::None,
                },
                p,
            )
        }
        fn cc(&mut self, c: u8, n: u8, v: u8) -> Vec<Command> {
            self.send(
                c,
                Voice2::ControlChange {
                    controller: CCNumber::of(n),
                    value: u32::from(v) << 25,
                },
                &One,
            )
        }
        fn end(&mut self, id: VoiceId) {
            self.voices.retain(|v| v.id != id);
        }
    }

    // ---- allocation ---------------------------------------------------

    #[test]
    fn the_oldest_gives_way_when_full() {
        let mut s = Sim::new(POLY);
        for k in 60..64 {
            s.on(0, k);
        }
        let c = s.on(0, 64);
        assert_eq!(c.len(), 1);
        assert!(matches!(c[0], Command::Start { steal: Some(v), .. } if v == id(1)));
    }

    /// A patch that starts `.0` voices for every note.
    struct Layers(u32);
    impl Patch for Layers {
        fn voices(&self, _: MidiChannel, _: NoteNumber, _: u16, spec: &mut dyn FnMut(VoiceSpec)) {
            for tag in 0..self.0 {
                spec(VoiceSpec {
                    group: 0,
                    scope: 0,
                    tag,
                });
            }
        }
    }

    /// A strike starts at most `MAX_SPECS` (32) voices, and a patch that asks
    /// for more is cut short, in the order it named them. The figure is tutti's
    /// (the SoundFont 2.04 specification sets no limit on the layers of a note;
    /// the largest note of the TimGM6mb font is 6, measured). A patch of 32
    /// layers is whole.
    #[test]
    fn a_strike_starts_at_most_the_cap_and_a_patch_within_it_is_whole() {
        let rules = Rules {
            polyphony: 64,
            ..POLY
        };
        let tags = |c: &[Command]| -> Vec<u32> {
            c.iter()
                .filter_map(|c| match c {
                    Command::Start { spec, .. } => Some(spec.tag),
                    _ => None,
                })
                .collect()
        };
        let mut s = Sim::new(rules);
        let started = tags(&s.on_with(0, 60, &Layers(40)));
        assert_eq!(started, (0..32).collect::<Vec<_>>());
        let mut s = Sim::new(rules);
        let started = tags(&s.on_with(0, 60, &Layers(32)));
        assert_eq!(started.len(), 32);
    }

    #[test]
    fn a_released_voice_gives_way_before_a_held_one() {
        let mut s = Sim::new(POLY);
        for k in 60..64 {
            s.on(0, k);
        }
        s.off(0, 63);
        let c = s.on(0, 64);
        assert!(
            matches!(c[0], Command::Start { steal: Some(v), .. } if v == id(4)),
            "{c:?}"
        );
    }

    #[test]
    fn never_drops_the_note_and_disturbs_nothing() {
        let mut s = Sim::new(Rules {
            steal: Steal::Never,
            ..POLY
        });
        for k in 60..64 {
            s.on(0, k);
        }
        assert!(s.on(0, 64).is_empty());
        assert_eq!(s.voices.len(), 4);
    }

    #[test]
    fn each_order_picks_its_voice() {
        for (order, want) in [
            (Order::Oldest, 1),
            (Order::Newest, 4),
            (Order::HighestNote, 4),
            (Order::LowestNote, 1),
        ] {
            let mut s = Sim::new(Rules {
                steal: Steal::By(order),
                ..POLY
            });
            for k in 60..64 {
                s.on(0, k);
            }
            let c = s.on(0, 70);
            assert!(
                matches!(c[0], Command::Start { steal: Some(v), .. } if v == id(want)),
                "{order:?} {c:?}"
            );
        }
        // Quietest: the voice with the lowest level; among released ones too.
        let mut s = Sim::new(Rules {
            steal: Steal::By(Order::Quietest),
            ..POLY
        });
        for k in 60..64 {
            s.on(0, k);
        }
        s.voices[2].level = Amplitude(0.1);
        let c = s.on(0, 70);
        assert!(
            matches!(c[0], Command::Start { steal: Some(v), .. } if v == id(3)),
            "{c:?}"
        );
        let mut s = Sim::new(POLY);
        for k in 60..64 {
            s.on(0, k);
        }
        s.off(0, 60);
        s.off(0, 62);
        s.voices[2].level = Amplitude(0.1);
        let c = s.on(0, 70);
        assert!(
            matches!(c[0], Command::Start { steal: Some(v), .. } if v == id(3)),
            "the quieter of the released ones: {c:?}"
        );
    }

    /// A strike never takes a voice it started itself: at a full pool a pair
    /// whose first voice took the last slot drops its second.
    #[test]
    fn a_strike_does_not_steal_its_own_voices() {
        let mut s = Sim::new(Rules {
            polyphony: 3,
            ..POLY
        });
        s.on(0, 60);
        s.on(0, 62);
        let c = s.on_with(0, 64, &Pair { group: 0, scope: 0 });
        let starts: Vec<_> = c
            .iter()
            .filter_map(|x| match x {
                Command::Start { steal, .. } => Some(*steal),
                _ => None,
            })
            .collect();
        assert_eq!(starts, [None, Some(id(1))], "{c:?}");
        assert!(
            s.voices.iter().any(|v| v.id == id(4)),
            "the first layer lives"
        );
    }

    // ---- repeated keys and layers --------------------------------------

    #[test]
    fn replace_releases_the_earlier_strike_and_layer_keeps_it() {
        let mut s = Sim::new(POLY);
        s.on(0, 60);
        let c = s.on(0, 60);
        assert_eq!(c[0], release(1));
        assert!(matches!(c[1], Command::Start { steal: None, .. }));
        let mut s = Sim::new(Rules {
            retrigger: Retrigger::Layer,
            ..POLY
        });
        s.on(0, 60);
        let c = s.on(0, 60);
        assert_eq!(c.len(), 1);
        assert!(matches!(c[0], Command::Start { .. }));
        // A note-off then ends both layers.
        assert_eq!(s.off(0, 60), [release(1), release(2)]);
    }

    #[test]
    fn a_note_is_as_many_voices_as_the_patch_says() {
        let mut s = Sim::new(POLY);
        let c = s.on_with(0, 60, &Pair { group: 0, scope: 0 });
        assert_eq!(c.len(), 2);
        assert_eq!(s.off(0, 60).len(), 2);
    }

    // ---- exclusive groups (SF2 2.04 section 8.1.2, generator 57) --------

    const EXCLUSIVE: Rules = Rules {
        exclusive_groups: true,
        polyphony: 8,
        ..POLY
    };

    /// The scope of an exclusive class is the whole preset (8.1.2, generator
    /// 57, Read): a note of the class ends the sounding notes of that class
    /// in the same preset, whichever channel plays them. Two channels playing
    /// one preset share it.
    #[test]
    fn an_exclusive_group_ends_its_scopes_others_on_any_channel() {
        let mut s = Sim::new(EXCLUSIVE);
        s.on_with(1, 60, &Grouped { scope: 5 });
        let c = s.on_with(0, 62, &Grouped { scope: 5 });
        assert_eq!(c[0], Command::Damp(id(1)), "{c:?}");
    }

    /// The same group number in another preset is another group.
    #[test]
    fn a_group_number_means_nothing_across_scopes() {
        let mut s = Sim::new(EXCLUSIVE);
        s.on_with(0, 60, &Grouped { scope: 5 });
        let c = s.on_with(0, 62, &Grouped { scope: 6 });
        assert!(!c.iter().any(|x| matches!(x, Command::Damp(_))), "{c:?}");
    }

    /// The voices one strike starts (a stereo pair, both of one class) do not
    /// end each other; the next strike ends both.
    #[test]
    fn the_voices_of_one_strike_do_not_end_each_other() {
        let mut s = Sim::new(EXCLUSIVE);
        let pair = Pair { group: 7, scope: 5 };
        let c = s.on_with(0, 60, &pair);
        assert!(!c.iter().any(|x| matches!(x, Command::Damp(_))), "{c:?}");
        assert_eq!(s.voices.len(), 2);
        let c = s.on_with(0, 62, &pair);
        let damps: Vec<_> = c.iter().filter(|x| matches!(x, Command::Damp(_))).collect();
        assert_eq!(damps.len(), 2, "{c:?}");
        // A damp is a release, not a cut: all four voices still exist.
        assert_eq!(s.voices.len(), 4);
        // Told once: a third strike damps only the second's pair.
        let c = s.on_with(0, 64, &pair);
        let damps: Vec<_> = c.iter().filter(|x| matches!(x, Command::Damp(_))).collect();
        assert_eq!(damps.len(), 2, "{c:?}");
    }

    // ---- pedals, panics, reset ------------------------------------------

    /// MIDI 1.0 Detailed Specification, Damper Pedal (Est: the text is not in
    /// the repo): down from 64; per channel.
    #[test]
    fn the_pedal_holds_until_it_comes_up() {
        let mut s = Sim::new(POLY);
        s.cc(0, 64, 127);
        s.on(0, 60);
        assert!(s.off(0, 60).is_empty());
        assert_eq!(s.core.phase(id(1)), Some(Phase::Pedal));
        assert_eq!(s.cc(0, 64, 0), [release(1)]);
        // Another channel's pedal holds nothing.
        let mut s = Sim::new(POLY);
        s.cc(1, 64, 127);
        s.on(0, 60);
        assert_eq!(s.off(0, 60), [release(1)]);
    }

    /// All Notes Off respects the pedal; All Sound Off does not.
    #[test]
    fn all_notes_off_respects_the_pedal_and_all_sound_off_does_not() {
        let mut s = Sim::new(POLY);
        s.cc(0, 64, 127);
        s.on(0, 60);
        s.on(0, 62);
        s.off(0, 62);
        assert!(s.cc(0, 123, 0).is_empty(), "held by the pedal");
        assert_eq!(s.cc(0, 120, 0), [Command::Cut(id(1)), Command::Cut(id(2))]);
        let mut s = Sim::new(POLY);
        s.on(0, 60);
        assert_eq!(s.cc(0, 123, 0), [release(1)]);
    }

    /// The mode messages, CC 124 to 127, also do All Notes Off (MIDI 1.0
    /// Detailed Specification, Channel Mode Messages; Est, the text is not in
    /// the repo); the mode they name is `Rules`'.
    #[test]
    fn the_mode_messages_also_release_what_is_held() {
        for n in 124..=127 {
            let mut s = Sim::new(POLY);
            s.on(0, 60);
            assert_eq!(s.cc(0, n, 0), [release(1)], "CC {n}");
            let mut s = Sim::new(POLY);
            s.cc(0, 64, 127);
            s.on(0, 60);
            assert!(s.cc(0, n, 0).is_empty(), "CC {n} with the pedal down");
        }
    }

    /// RP-015 (Est): Reset All Controllers puts the pedal up, so a note it held goes.
    #[test]
    fn reset_all_controllers_lifts_the_pedal() {
        let mut s = Sim::new(POLY);
        s.cc(0, 64, 127);
        s.on(0, 60);
        s.off(0, 60);
        assert_eq!(
            s.cc(0, 121, 0),
            [
                release(1),
                Command::Retune {
                    channel: MidiChannel::of(0),
                    bend: Semitones(0.0)
                }
            ]
        );
        s.on(0, 62);
        assert_eq!(s.off(0, 62), [release(2)], "the pedal is up now");
    }

    #[test]
    fn sostenuto_holds_what_sounds_when_it_goes_down() {
        let mut s = Sim::new(POLY);
        s.on(0, 60);
        s.cc(0, 66, 127);
        s.on(0, 62);
        assert!(s.off(0, 60).is_empty());
        assert_eq!(s.off(0, 62), [release(2)], "struck after the pedal");
        assert_eq!(s.cc(0, 66, 0), [release(1)]);
        let mut s = Sim::new(Rules {
            sostenuto: false,
            ..POLY
        });
        s.on(0, 60);
        s.cc(0, 66, 127);
        assert_eq!(s.off(0, 60), [release(1)]);
    }

    /// The release carries the note-off's velocity, also when a pedal lets it
    /// through later; a panic or a lifted pedal with none to carry sends the
    /// neutral one.
    #[test]
    fn a_release_carries_its_velocity() {
        let mut s = Sim::new(POLY);
        s.on(0, 60);
        let off = |s: &mut Sim, k, velocity| {
            s.send(
                0,
                Voice2::NoteOff {
                    note: NoteNumber::of(k),
                    velocity,
                    attribute: Attribute::None,
                },
                &One,
            )
        };
        assert_eq!(
            off(&mut s, 60, 0x1234),
            [Command::Release {
                id: id(1),
                velocity: 0x1234
            }]
        );
        let mut s = Sim::new(POLY);
        s.cc(0, 64, 127);
        s.on(0, 60);
        off(&mut s, 60, 0x4321);
        assert_eq!(
            s.cc(0, 64, 0),
            [Command::Release {
                id: id(1),
                velocity: 0x4321
            }]
        );
        let mut s = Sim::new(POLY);
        s.on(0, 60);
        assert_eq!(s.cc(0, 123, 0), [release(1)]);
    }

    // ---- velocity zero ---------------------------------------------------

    /// A MIDI 1.0 Note On at velocity 0 is a Note Off (MIDI 1.0 Detailed
    /// Specification; `widen` makes it one); a MIDI 2.0 Note On at velocity 0
    /// is a Note On (M2-104 section 7.4.2), played as quietly as there is.
    #[test]
    fn velocity_zero_is_an_off_in_midi1_and_an_on_in_midi2() {
        let mut s = Sim::new(POLY);
        s.on(0, 60);
        let one = widen(Voice1::NoteOn {
            note: NoteNumber::of(60),
            velocity: U7::of(0),
        });
        assert!(matches!(one, Voice2::NoteOff { .. }));
        assert_eq!(s.send(0, one, &One), [release(1)]);
        let two = Voice2::NoteOn {
            note: NoteNumber::of(62),
            velocity: 0,
            attribute: Attribute::None,
        };
        let c = s.send(0, two, &One);
        assert!(
            matches!(c[..], [Command::Start { velocity: 1, .. }]),
            "{c:?}"
        );
    }

    // ---- mono and legato ---------------------------------------------------

    /// One voice a channel: a second channel's note does not take the first's.
    #[test]
    fn mono_is_one_voice_for_each_channel() {
        let mut s = Sim::new(MONO);
        s.on(0, 60);
        let other = s.on(1, 64);
        assert!(
            matches!(other[..], [Command::Start { steal: None, .. }]),
            "{other:?}"
        );
        let again = s.on(0, 62);
        assert!(
            matches!(again[..], [Command::Start { steal: Some(v), .. }] if v == id(1)),
            "taken over in place, no release before: {again:?}"
        );
        // Channel 0's old key is gone from its stack: its note-off is nothing.
        assert!(s.off(0, 60).is_empty());
        assert_eq!(s.off(1, 64), [release(2)]);
    }

    /// A released tail is the channel's voice too: the next note cuts it.
    #[test]
    fn mono_takes_over_a_released_tail() {
        let mut s = Sim::new(MONO);
        s.on(0, 60);
        s.off(0, 60);
        let c = s.on(0, 62);
        assert!(
            matches!(c[..], [Command::Start { steal: Some(v), .. }] if v == id(1)),
            "{c:?}"
        );
    }

    fn starts(c: &[Command]) -> Vec<(u64, Option<u64>, u32)> {
        c.iter()
            .filter_map(|x| match x {
                Command::Start {
                    id, steal, spec, ..
                } => Some((id.0, steal.map(|v| v.0), spec.tag)),
                _ => None,
            })
            .collect()
    }

    /// A two-layer mono note: the first key starts both layers; the second
    /// takes each over by its tag, in place, with no stray release.
    #[test]
    fn two_layer_mono_takes_over_layer_by_layer() {
        let p = Pair { group: 0, scope: 0 };
        let mut s = Sim::new(MONO);
        let first = s.on_with(0, 60, &p);
        assert_eq!(starts(&first), [(1, None, 0), (2, None, 1)], "{first:?}");
        assert_eq!(first.len(), 2);
        let second = s.on_with(0, 62, &p);
        assert_eq!(
            starts(&second),
            [(3, Some(1), 0), (4, Some(2), 1)],
            "{second:?}"
        );
        assert_eq!(
            second.len(),
            2,
            "no release of a voice about to be taken over"
        );
        assert_eq!(s.voices.len(), 2);
    }

    /// A two-layer legato note: the first key starts both layers; the second
    /// moves each by its tag, once.
    #[test]
    fn two_layer_legato_moves_each_layer_once() {
        let p = Pair { group: 0, scope: 0 };
        let mut s = Sim::new(LEGATO);
        let first = s.on_with(0, 60, &p);
        assert_eq!(starts(&first), [(1, None, 0), (2, None, 1)], "{first:?}");
        assert_eq!(first.len(), 2, "no retarget of the fresh voices");
        let second = s.on_with(0, 62, &p);
        assert_eq!(
            second,
            [
                Command::Retarget {
                    id: id(1),
                    key: NoteNumber::of(62),
                    velocity: 0x8000,
                    glide: None,
                },
                Command::Retarget {
                    id: id(2),
                    key: NoteNumber::of(62),
                    velocity: 0x8000,
                    glide: None,
                },
            ]
        );
    }

    /// A layer the new key lacks is let go, and one it adds starts.
    #[test]
    fn mono_layers_follow_the_key() {
        struct Lower;
        impl Patch for Lower {
            fn voices(
                &self,
                _: MidiChannel,
                k: NoteNumber,
                _: u16,
                spec: &mut dyn FnMut(VoiceSpec),
            ) {
                spec(VoiceSpec {
                    group: 0,
                    scope: 0,
                    tag: 0,
                });
                if k.get() < 61 {
                    spec(VoiceSpec {
                        group: 0,
                        scope: 0,
                        tag: 1,
                    });
                }
            }
        }
        let mut s = Sim::new(LEGATO);
        let a = s.on_with(0, 60, &Lower);
        assert_eq!(a.len(), 2);
        let b = s.on_with(0, 62, &Lower);
        assert_eq!(
            b,
            [
                Command::Retarget {
                    id: id(1),
                    key: NoteNumber::of(62),
                    velocity: 0x8000,
                    glide: None,
                },
                release(2)
            ]
        );
    }

    /// The layer a key keeps is taken over by tag, not by position: a patch
    /// whose upper key lacks layer 0 leaves layer 1's voice to layer 1.
    #[test]
    fn a_layer_is_taken_over_by_its_tag() {
        struct Upper;
        impl Patch for Upper {
            fn voices(
                &self,
                _: MidiChannel,
                k: NoteNumber,
                _: u16,
                spec: &mut dyn FnMut(VoiceSpec),
            ) {
                if k.get() < 61 {
                    spec(VoiceSpec {
                        group: 0,
                        scope: 0,
                        tag: 0,
                    });
                }
                spec(VoiceSpec {
                    group: 0,
                    scope: 0,
                    tag: 1,
                });
            }
        }
        let mut s = Sim::new(MONO);
        s.on_with(0, 60, &Upper);
        let c = s.on_with(0, 62, &Upper);
        assert_eq!(starts(&c), [(3, Some(2), 1)], "{c:?}");
        assert!(c.contains(&release(1)), "{c:?}");
    }

    /// Last-note priority with a stack of held keys (tutti's rule; MIDI 1.0
    /// does not say): letting go the sounding key returns to the most recent
    /// held one, a mono instrument restarting there.
    #[test]
    fn mono_returns_to_the_most_recent_held_key() {
        let mut s = Sim::new(MONO);
        s.on(0, 60);
        s.on(0, 64);
        let back = s.off(0, 64);
        assert_eq!(back.len(), 1, "{back:?}");
        assert!(
            matches!(back[0], Command::Start { key: k, steal: Some(v), .. } if k == NoteNumber::of(60) && v == id(2)),
            "{back:?}"
        );
        // 60 is the last held: letting it go releases.
        assert_eq!(s.off(0, 60), [release(3)]);
    }

    /// Legato returns by moving the voice, with the held key's own velocity.
    #[test]
    fn legato_returns_without_an_attack() {
        let mut s = Sim::new(LEGATO);
        s.send(
            0,
            Voice2::NoteOn {
                note: NoteNumber::of(60),
                velocity: 0x3000,
                attribute: Attribute::None,
            },
            &One,
        );
        s.on(0, 64);
        assert_eq!(
            s.off(0, 64),
            [Command::Retarget {
                id: id(1),
                key: NoteNumber::of(60),
                velocity: 0x3000,
                glide: None,
            }]
        );
        assert_eq!(s.off(0, 60), [release(1)]);
    }

    /// A key held under the sounding one is let go with no sound change, and
    /// the sounding one's release then releases.
    #[test]
    fn a_held_key_under_the_top_is_let_go_silently() {
        let mut s = Sim::new(LEGATO);
        s.on(0, 60);
        s.on(0, 64);
        assert!(s.off(0, 60).is_empty());
        assert_eq!(s.off(0, 64), [release(1)]);
    }

    /// With the pedal down the last release holds.
    #[test]
    fn mono_obeys_the_pedal_when_the_last_key_goes() {
        let mut s = Sim::new(MONO);
        s.cc(0, 64, 127);
        s.on(0, 60);
        assert!(s.off(0, 60).is_empty());
        assert_eq!(s.cc(0, 64, 0), [release(1)]);
    }

    /// CC 120, 121 and 123 to 127 forget the held keys, as does `reset`.
    #[test]
    fn the_panics_and_reset_forget_the_held_keys() {
        for n in [120, 121, 123, 124, 125, 126, 127] {
            let mut s = Sim::new(MONO);
            s.on(0, 60);
            s.on(0, 64);
            s.cc(0, n, 0);
            s.on(0, 67);
            // 60 and 64 are forgotten: 67 alone, so its release releases.
            let c = s.off(0, 67);
            assert!(matches!(c[..], [Command::Release { .. }]), "CC {n}: {c:?}");
        }
        let mut s = Sim::new(MONO);
        s.on(0, 60);
        s.on(0, 64);
        s.core.reset();
        s.voices.clear();
        s.on(0, 67);
        assert!(matches!(s.off(0, 67)[..], [Command::Release { .. }]));
    }

    /// The stack holds `HELD_KEYS`; past that the oldest is forgotten.
    #[test]
    fn the_held_stack_forgets_its_oldest() {
        let mut s = Sim::new(MONO);
        for k in 40..40 + HELD_KEYS as u8 + 1 {
            s.on(0, k);
        }
        // Letting go down the stack returns each time, until the oldest kept.
        let top = 40 + HELD_KEYS as u8;
        for k in (41 + 1..=top).rev() {
            let c = s.off(0, k);
            assert!(
                matches!(c[..], [Command::Start { key: back, .. }] if back == NoteNumber::of(k - 1)),
                "{k}: {c:?}"
            );
        }
        // 41 is the oldest kept (40 was dropped): releasing it releases.
        assert!(matches!(s.off(0, 41)[..], [Command::Release { .. }]));
    }

    /// A note the core released (Replace) is held by nothing: `holds` agrees
    /// with `phase`.
    #[test]
    fn a_replaced_note_is_held_by_nothing() {
        let mut s = Sim::new(POLY);
        s.on(0, 60);
        s.on(0, 60);
        assert_eq!(s.core.phase(id(1)), Some(Phase::Releasing));
        assert_eq!(s.core.holds(id(1)), None);
        assert!(s.core.holds(id(2)).is_some_and(|h| h.key()));
    }

    /// A voice the instrument ended (a sampler's, at its sample's end) is not
    /// returned to: the return starts a fresh voice, in both mono modes.
    #[test]
    fn a_return_never_names_a_voice_the_instrument_ended() {
        // The sounding voice: legato moved voice 1, mono started voice 2.
        for (rules, sounding) in [(LEGATO, 1), (MONO, 2)] {
            let mut s = Sim::new(rules);
            s.on(0, 60);
            s.on(0, 64);
            s.end(id(sounding));
            let c = s.off(0, 64);
            assert!(
                matches!(c[..], [Command::Start { key: k, steal: None, .. }] if k == NoteNumber::of(60)),
                "{:?}: {c:?}",
                rules.mode
            );
        }
    }

    /// The "rapid termination" of an exclusive class is a release of the
    /// instrument's own choosing (SF2 2.04 section 8.1.2, generator 57), told
    /// once: a damped voice counts as released, so a later note-off, pedal or
    /// All Notes Off has nothing to release in it, a panic still cuts it, and
    /// it is the first to give way when the pool is full.
    #[test]
    fn a_damped_voice_counts_as_released_and_is_told_once() {
        let mut s = Sim::new(Rules {
            polyphony: 2,
            ..EXCLUSIVE
        });
        s.on_with(0, 60, &Grouped { scope: 5 });
        let c = s.on_with(0, 62, &Grouped { scope: 5 });
        assert_eq!(c[0], Command::Damp(id(1)), "{c:?}");
        assert!(
            s.off(0, 60).is_empty(),
            "a damped voice has nothing left to release"
        );
        let c = s.on_with(0, 64, &Grouped { scope: 5 });
        assert_eq!(
            c.iter().filter(|x| matches!(x, Command::Damp(_))).count(),
            1,
            "only the second voice is damped now: {c:?}"
        );
        // Full pool (2): the third strike took the first voice's slot (a
        // released one goes first), so the second is damped and the third is
        // held; a fourth takes the second, never the held third.
        let c = s.on_with(0, 65, &One);
        assert!(
            c.iter().any(
                |x| matches!(x, Command::Start { steal: Some(v), .. } if *v == id(1) || *v == id(2))
            ),
            "a released voice gives way: {c:?}"
        );
        assert!(
            !c.iter()
                .any(|x| matches!(x, Command::Start { steal: Some(v), .. } if *v == id(3))),
            "the held voice stays: {c:?}"
        );
    }

    /// A mono note of an exclusive class ends the others of its class even when
    /// it takes its own channel's voice over in place.
    #[test]
    fn a_mono_takeover_still_ends_its_exclusive_class() {
        let mut s = Sim::new(Rules {
            exclusive_groups: true,
            ..MONO
        });
        s.on(0, 60);
        s.on_with(1, 62, &Grouped { scope: 5 });
        let c = s.on_with(0, 64, &Grouped { scope: 5 });
        assert!(c.contains(&Command::Damp(id(2))), "{c:?}");
        assert!(
            c.iter()
                .any(|x| matches!(x, Command::Start { steal: Some(v), .. } if *v == id(1))),
            "the takeover itself is not cut: {c:?}"
        );
    }

    /// The layout of `lower` and `upper` member counts (0: no such zone), the
    /// tests' shorthand for the protocol's [`MpeZones`].
    fn zones(lower: u8, upper: u8) -> Option<MpeZones> {
        MpeZones::new(MpeZoneConfig::lower(lower), MpeZoneConfig::upper(upper))
    }

    // ---- bends -------------------------------------------------------------

    fn bend(v: u32) -> Voice2 {
        Voice2::PitchBend(v)
    }
    fn retunes(c: &[Command]) -> Vec<(u8, f32)> {
        c.iter()
            .filter_map(|x| match x {
                Command::Retune { channel, bend } => Some((channel.get(), bend.0)),
                _ => None,
            })
            .collect()
    }
    fn rpn0(range_semitones: f32) -> Voice2 {
        Voice2::Registered {
            bank: U7::of(0),
            index: U7::of(0),
            value: (range_semitones * (1u32 << 25) as f32) as u32,
        }
    }

    /// `forget_notes` ends the notes, pedals and held keys and leaves the bends
    /// where the channels put them; `reset` returns those too.
    #[test]
    fn forgetting_the_notes_keeps_the_bends() {
        let mut s = Sim::new(POLY);
        s.send(2, bend(u32::MAX), &One);
        s.on(2, 60);
        s.core.forget_notes();
        assert_eq!(s.core.channel_pitch(MidiChannel::of(2)), Semitones(2.0));
        assert!(s.core.holds(id(1)).is_none());
        s.core.reset();
        assert_eq!(s.core.channel_pitch(MidiChannel::of(2)), Semitones(0.0));
    }

    /// RPN 0 is two fields, semitones then cents (RP-003, M2-115 section 4.1):
    /// 12 semitones 50 cents is a range of 12.5, a full bend up retunes by it,
    /// and the 18 low bits are unused. The fraction is decimal, not Q7.25's
    /// binary one: cents 64 is 0.64, not 0.5.
    #[test]
    fn rpn0_cents_are_hundredths_of_a_semitone() {
        let rpn = |semitones: u32, cents: u32, low: u32| Voice2::Registered {
            bank: U7::of(0),
            index: U7::of(0),
            value: semitones << 25 | cents << 18 | low,
        };
        for (semitones, cents, low, want) in [
            (12, 50, 0, 12.5f32),
            (12, 64, 0, 12.64),
            (2, 0, 0, 2.0),
            (0, 99, 0x3_FFFF, 0.99),
        ] {
            let mut s = Sim::new(POLY);
            s.send(1, rpn(semitones, cents, low), &One);
            let up = s.send(1, bend(u32::MAX), &One);
            assert!((retunes(&up)[0].1 - want).abs() < 1e-5, "{up:?} {want}");
            assert_eq!(
                s.core.channel_pitch(MidiChannel::of(1)),
                Semitones(retunes(&up)[0].1)
            );
        }
    }

    /// MIDI 1.0 Pitch Bend (Est): the wheel is a channel's, and full travel
    /// spans the sensitivity, 2 semitones until RPN 0 says otherwise.
    #[test]
    fn a_bend_is_the_channels_and_spans_its_sensitivity() {
        let mut s = Sim::new(POLY);
        let up = s.send(3, bend(u32::MAX), &One);
        assert_eq!(retunes(&up).len(), 1);
        assert_eq!(retunes(&up)[0].0, 3);
        assert!((retunes(&up)[0].1 - 2.0).abs() < 1e-4, "{up:?}");
        let down = s.send(3, bend(0), &One);
        assert!((retunes(&down)[0].1 + 2.0).abs() < 1e-6);
        assert_eq!(retunes(&s.send(3, bend(1 << 31), &One)), [(3, 0.0)]);
    }

    #[test]
    fn rpn_0_changes_the_range_and_the_pitch_with_it() {
        let mut s = Sim::new(POLY);
        s.send(0, bend(u32::MAX), &One);
        let c = s.send(0, rpn0(12.0), &One);
        assert!((retunes(&c)[0].1 - 12.0).abs() < 1e-3, "{c:?}");
        let other = s.send(1, bend(u32::MAX), &One);
        assert!(
            (retunes(&other)[0].1 - 2.0).abs() < 1e-4,
            "channel 1 keeps its range"
        );
    }

    /// Reset All Controllers centres the wheel and leaves the sensitivity
    /// (RP-015, Est).
    #[test]
    fn reset_centres_the_wheel_and_keeps_the_range() {
        let mut s = Sim::new(POLY);
        s.send(0, rpn0(12.0), &One);
        s.send(0, bend(u32::MAX), &One);
        assert_eq!(retunes(&s.cc(0, 121, 0)), [(0, 0.0)]);
        let again = s.send(0, bend(u32::MAX), &One);
        assert!((retunes(&again)[0].1 - 12.0).abs() < 1e-3);
    }

    /// Under MPE (RP-053, Est: the text is not in the repo; the repo's
    /// `MpeZoneConfig` documents the same) a zone master's bend adds to each
    /// member's own, within the zone, and does not replace it.
    #[test]
    fn a_zone_masters_bend_adds_to_each_members_own() {
        let mut s = Sim::new(POLY);
        s.core.set_zones(zones(3, 0));
        // Member 2 bends its own 1 semitone (half travel at range 2).
        let c = s.send(2, bend(0xC000_0000), &One);
        assert_eq!(retunes(&c), [(2, 1.0)]);
        // The master bends 2: member 2 sounds 3, members 1 and 3 sound 2.
        let c = s.send(0, bend(u32::MAX), &One);
        let r = retunes(&c);
        assert_eq!(r.len(), 4, "{r:?}");
        let at = |ch: u8| r.iter().find(|x| x.0 == ch).unwrap().1;
        assert!((at(0) - 2.0).abs() < 1e-4 && (at(1) - 2.0).abs() < 1e-4);
        assert!((at(2) - 3.0).abs() < 1e-4 && (at(3) - 2.0).abs() < 1e-4);
        // Beyond the zone nothing moves.
        assert!(!r.iter().any(|x| x.0 == 4));
    }

    /// The master's sensitivity is its own: RPN 0 on the master retunes the
    /// zone, a member's moves only that member.
    #[test]
    fn rpn_0_on_a_master_retunes_the_zone_and_on_a_member_only_itself() {
        let mut s = Sim::new(POLY);
        s.core.set_zones(zones(2, 0));
        s.send(0, bend(u32::MAX), &One);
        let c = s.send(0, rpn0(12.0), &One);
        assert_eq!(retunes(&c).len(), 3);
        assert!(retunes(&c).iter().all(|x| (x.1 - 12.0).abs() < 1e-3));
        let m = s.send(1, rpn0(48.0), &One);
        assert_eq!(retunes(&m).len(), 1);
    }

    /// The upper zone counts down from channel 14 to its master 15.
    #[test]
    fn the_upper_zone_counts_down() {
        let mut s = Sim::new(POLY);
        s.core.set_zones(zones(0, 2));
        let c = s.send(15, bend(u32::MAX), &One);
        let chs: Vec<_> = retunes(&c).iter().map(|x| x.0).collect();
        assert_eq!(chs, [13, 14, 15]);
    }

    /// A master's reset centres the zone, a member's only itself.
    #[test]
    fn a_masters_reset_centres_the_zone() {
        let mut s = Sim::new(POLY);
        s.core.set_zones(zones(2, 0));
        s.send(0, bend(u32::MAX), &One);
        s.send(1, bend(u32::MAX), &One);
        assert_eq!(
            retunes(&s.cc(2, 121, 0)),
            [(2, 2.0)],
            "a member's reset leaves the master's bend"
        );
        let r = retunes(&s.cc(0, 121, 0));
        assert_eq!(
            r,
            [(0, 0.0), (1, 2.0), (2, 0.0)],
            "the member's own wheel stays"
        );
    }

    /// `reset` returns the wheels and the sensitivities to the rules' own.
    #[test]
    fn reset_forgets_the_bends() {
        let mut s = Sim::new(POLY);
        s.send(0, rpn0(12.0), &One);
        s.send(0, bend(u32::MAX), &One);
        s.core.reset();
        let c = s.send(0, bend(u32::MAX), &One);
        assert!((retunes(&c)[0].1 - 2.0).abs() < 1e-4);
    }

    // ---- zone-wide messages (M1-100 section 2.3.1) ---------------------------

    /// M1-100 section 2.3.1: "certain MIDI messages (for example, Damper
    /// Pedal) can be expected to affect all Sounding Notes across the Manager
    /// Channel and all Member Channels", sent "only on a Zone's Manager
    /// Channel". The pedal on the master holds the notes of every member, the
    /// ones sounding and the ones that start after it, and its release lets
    /// them go.
    #[test]
    fn a_zone_masters_pedal_holds_every_members_notes() {
        let mut s = Sim::new(POLY);
        s.core.set_zones(zones(3, 0));
        s.on(1, 60);
        s.on(2, 64);
        s.cc(0, 64, 127);
        s.on(3, 67);
        assert!(s.off(1, 60).is_empty());
        assert!(s.off(2, 64).is_empty());
        assert!(s.off(3, 67).is_empty());
        assert_eq!(s.cc(0, 64, 0), [release(1), release(2), release(3)]);
    }

    /// The master's pedal is the zone's and no wider: a channel outside the
    /// zone is not held, and a member's own pedal is its own.
    #[test]
    fn a_zone_masters_pedal_stops_at_the_zone() {
        let mut s = Sim::new(POLY);
        s.core.set_zones(zones(2, 0));
        s.on(5, 60);
        s.cc(0, 64, 127);
        assert_eq!(s.off(5, 60), [release(1)]);
        s.cc(0, 64, 0);
        s.on(1, 62);
        s.cc(2, 64, 127);
        assert_eq!(s.off(1, 62), [release(2)], "a member's pedal is its own");
    }

    /// M1-100 section 2.3.1 as above, for All Notes Off (Table 5, CC 123:
    /// Optional on the Manager Channel): on the master it releases every
    /// member's notes; on a member, that member's alone.
    #[test]
    fn all_notes_off_on_a_zone_master_releases_every_member() {
        let mut s = Sim::new(POLY);
        s.core.set_zones(zones(3, 0));
        s.on(1, 60);
        s.on(2, 64);
        s.on(3, 67);
        assert_eq!(s.cc(2, 123, 0), [release(2)]);
        assert_eq!(s.cc(0, 123, 0), [release(1), release(3)]);
    }

    /// Reset All Controllers (Table 5, CC 121: Optional on both) on the master
    /// puts the zone's pedal up, members' held notes included.
    #[test]
    fn reset_all_controllers_on_a_zone_master_lifts_the_zones_pedal() {
        let mut s = Sim::new(POLY);
        s.core.set_zones(zones(2, 0));
        s.on(1, 60);
        s.cc(0, 64, 127);
        s.off(1, 60);
        assert_eq!(
            s.cc(0, 121, 0)
                .iter()
                .filter(|c| matches!(c, Command::Release { .. }))
                .count(),
            1
        );
    }

    // ---- liveness and cost -----------------------------------------------

    /// Liveness is read when a note starts: a voice that ended is forgotten
    /// then, its slot is free for the new note, and a command that named it
    /// in between was for a voice the instrument no longer has.
    #[test]
    fn a_voice_that_ended_is_forgotten_when_the_next_note_starts() {
        let mut s = Sim::new(POLY);
        for k in 60..64 {
            s.on(0, k);
        }
        s.off(0, 60);
        s.end(id(1));
        // A pedal lifting still names nothing it should not: 1 is already released.
        let c = s.on(0, 64);
        assert!(matches!(c[0], Command::Start { steal: None, .. }), "{c:?}");
        assert!(s.off(0, 60).is_empty(), "nothing left of the ended voice");
    }

    /// The core allocates nothing after `new`: its tables never outgrow their
    /// capacity.
    #[test]
    fn step_never_grows_its_tables() {
        let mut s = Sim::new(POLY);
        let (n, o, r) = (
            s.core.notes.capacity(),
            s.core.out.capacity(),
            s.core.retired.capacity(),
        );
        for i in 0..200u8 {
            s.on(i % 2, 40 + i % 30);
            if i % 3 == 0 {
                s.off(i % 2, 40 + (i + 5) % 30);
            }
            if i % 7 == 0 {
                s.cc(i % 2, 123, 0);
            }
            if i % 11 == 0 {
                s.cc(i % 2, 64, 127 * (i % 2));
            }
        }
        assert_eq!(
            (
                s.core.notes.capacity(),
                s.core.out.capacity(),
                s.core.retired.capacity()
            ),
            (n, o, r)
        );
    }

    // ---- per-note controllers (M2-104 section 7.4) ----------------------

    /// 60.0 as a Registered Per-Note Controller #3 word (Q7.25).
    const PITCH_60: u32 = 60 << 25;

    fn pnc(s: &mut Sim, c: u8, k: u8, controller: RegisteredPerNoteController, value: u32) {
        let cmds = s.send(
            c,
            Voice2::RegisteredPerNote {
                note: NoteNumber::of(k),
                controller,
                value,
            },
            &One,
        );
        assert!(cmds.iter().all(|c| matches!(c, Command::Control { .. })));
    }

    fn per_note_bend(s: &mut Sim, c: u8, k: u8, value: u32) -> Vec<Command> {
        s.send(
            c,
            Voice2::PerNotePitchBend {
                note: NoteNumber::of(k),
                value,
            },
            &One,
        )
    }

    fn management(s: &mut Sim, c: u8, k: u8, detach: bool, reset: bool) -> Vec<Command> {
        s.send(
            c,
            Voice2::PerNoteManagement {
                note: NoteNumber::of(k),
                detach,
                reset,
            },
            &One,
        )
    }

    /// M2-104 section 7.4.15.2 and Appendix C.3 (Method 1): "Per-Note
    /// Management; PNCC#3 Set Pitch 60.0; Note On (Pitch sounds as 60.0)". A
    /// Pitch 7.25 that arrives before its Note On, with no voice to address,
    /// is the state the note starts with: the Note On is followed by the
    /// controller, for the voice it started.
    #[test]
    fn a_pitch_sent_before_its_note_on_is_what_the_note_starts_with() {
        let mut s = Sim::new(POLY);
        management(&mut s, 0, 0, true, true);
        pnc(&mut s, 0, 0, RegisteredPerNoteController::Pitch, PITCH_60);
        let c = s.on(0, 0);
        assert!(matches!(c[0], Command::Start { .. }), "{c:?}");
        assert_eq!(
            c[1..],
            [Command::Control {
                id: id(1),
                control: NoteControl::Pitch(Pitch::from_q7_25(PITCH_60)),
            }]
        );
    }

    fn told(c: &[Command]) -> Vec<VoiceId> {
        c.iter()
            .filter_map(|c| match c {
                Command::Control { id, .. } => Some(*id),
                _ => None,
            })
            .collect()
    }

    /// M2-104 section 7.4.15.2: a value is for the note, and a note a patch
    /// makes of several voices is one note: every voice of the strike takes it.
    #[test]
    fn every_voice_of_a_strike_takes_the_per_note_value() {
        let mut s = Sim::new(POLY);
        pnc(&mut s, 0, 5, RegisteredPerNoteController::Pitch, PITCH_60);
        let c = s.on_with(0, 5, &Pair { group: 0, scope: 0 });
        assert_eq!(told(&c), vec![id(1), id(2)], "{c:?}");
    }

    /// M2-104 section 7.4.5: only a Detach makes "previous notes on the
    /// referenced Note Number" stop responding, so until then the tail of the
    /// first strike (released by the second on the same key) follows a value
    /// with the second; after the Detach neither does.
    #[test]
    fn a_tail_follows_a_per_note_value_until_a_detach() {
        let mut s = Sim::new(POLY);
        s.on(0, 5);
        s.on(0, 5);
        assert!(s.core.phase(id(1)).is_some_and(|p| p == Phase::Releasing));
        let c = per_note_bend(&mut s, 0, 5, 0xC000_0000);
        assert_eq!(told(&c), vec![id(1), id(2)], "{c:?}");
        management(&mut s, 0, 5, true, false);
        let c = per_note_bend(&mut s, 0, 5, 0x4000_0000);
        assert_eq!(told(&c), vec![], "{c:?}");
    }

    /// A voice the instrument freed has ended its note, and the number's room
    /// for responding notes (`LIVE_NOTES`) is not spent on it: a tail still
    /// sounding keeps following the number's values after more strikes than
    /// the bound, as long as the strikes' voices were freed.
    #[test]
    fn freed_voices_leave_room_for_a_tail_that_still_sounds() {
        let mut s = Sim::new(POLY);
        s.on(0, 5);
        for n in 2..=(LIVE_NOTES as u64 + 2) {
            s.on(0, 5);
            s.end(id(n));
        }
        s.on(0, 5);
        let c = per_note_bend(&mut s, 0, 5, 0xC000_0000);
        let told = told(&c);
        assert!(told.contains(&id(1)), "{told:?}");
    }

    /// `reset` is a fresh start, and what a Note Number was told goes with the
    /// rest (it clears the per-note state in place); `forget_notes` keeps it,
    /// as it keeps the bends.
    #[test]
    fn reset_forgets_the_per_note_values_and_forgetting_the_notes_keeps_them() {
        let mut s = Sim::new(POLY);
        pnc(&mut s, 0, 5, RegisteredPerNoteController::Pitch, PITCH_60);
        s.core.forget_notes();
        let c = s.on(0, 5);
        assert!(matches!(c.last(), Some(Command::Control { .. })), "{c:?}");
        s.core.reset();
        s.voices.clear();
        let c = s.on(0, 5);
        assert!(
            !c.iter().any(|c| matches!(c, Command::Control { .. })),
            "{c:?}"
        );
    }

    /// M2-104 section 7.4.5, Detach: "all currently playing notes and previous
    /// notes on the referenced Note Number shall no longer respond to any
    /// Per-Note Controllers", and later changes "apply to future notes only".
    /// Detach, then Pitch, then Note On on the same number: the Pitch reaches
    /// the new voice (2) and never the detached one (1).
    #[test]
    fn after_a_detach_the_next_pitch_is_for_the_next_note_alone() {
        let mut s = Sim::new(POLY);
        s.on(0, 5);
        assert_eq!(management(&mut s, 0, 5, true, false), vec![]);
        // No voice responds: the value is only stored.
        pnc(&mut s, 0, 5, RegisteredPerNoteController::Pitch, PITCH_60);
        let c = s.on(0, 5);
        let pitch = NoteControl::Pitch(Pitch::from_q7_25(PITCH_60));
        assert_eq!(
            c.last(),
            Some(&Command::Control {
                id: id(2),
                control: pitch
            })
        );
        // Later changes reach voice 2 and not voice 1.
        let c = per_note_bend(&mut s, 0, 5, 0xC000_0000);
        assert!(
            matches!(c[..], [Command::Control { id: v, .. }] if v == id(2)),
            "{c:?}"
        );
    }

    /// M2-104 section 7.4.5: with D=1 and S=1 the device "first process the
    /// Detach function, and then perform the Reset function": the playing note
    /// keeps its values (it is told nothing), and the defaults apply to the
    /// next note, which starts with no per-note controller.
    #[test]
    fn detach_and_reset_together_freeze_the_note_and_default_the_next() {
        let mut s = Sim::new(POLY);
        s.on(0, 5);
        per_note_bend(&mut s, 0, 5, 0xC000_0000);
        assert_eq!(management(&mut s, 0, 5, true, true), vec![]);
        let c = s.on(0, 5);
        assert!(
            c.iter().any(|c| matches!(c, Command::Start { .. }))
                && !c.iter().any(|c| matches!(c, Command::Control { .. })),
            "no controller follows the start: {c:?}"
        );
    }

    /// M2-104 section 7.4.5, Reset: "all Per-Note Controllers on the referenced
    /// Note Number should be reset to their default values". Without Detach the
    /// playing note is reset too, and the next note starts at the defaults.
    #[test]
    fn a_reset_returns_the_playing_note_and_the_next_one_to_defaults() {
        let mut s = Sim::new(POLY);
        pnc(&mut s, 0, 5, RegisteredPerNoteController::Volume, 1 << 30);
        s.on(0, 5);
        assert_eq!(
            management(&mut s, 0, 5, false, true),
            vec![Command::ResetControls(id(1))]
        );
        let c = s.on(0, 5);
        assert!(
            !c.iter().any(|c| matches!(c, Command::Control { .. })),
            "{c:?}"
        );
    }

    /// M2-104 Appendix B.2: Reset All Controllers does not reset per-note
    /// controllers.
    #[test]
    fn reset_all_controllers_leaves_per_note_controllers() {
        let mut s = Sim::new(POLY);
        pnc(&mut s, 0, 5, RegisteredPerNoteController::Pitch, PITCH_60);
        s.cc(0, 121, 0);
        let c = s.on(0, 5);
        assert!(matches!(c.last(), Some(Command::Control { .. })), "{c:?}");
    }

    // ---- channel controllers, glide and legato -----------------------------

    fn glide_of(c: &[Command]) -> Option<Glide> {
        c.iter().find_map(|x| match x {
            Command::Start { glide, .. } | Command::Retarget { glide, .. } => *glide,
            _ => None,
        })
    }

    fn registered(index: u8, value: u32) -> Voice2 {
        Voice2::Registered {
            bank: U7::of(0),
            index: U7::of(index),
            value,
        }
    }

    /// M1 p.14, Legato Footswitch: "40-7F Legato" turns "monophonic legato
    /// response" on: "if a new Note-On is received before the Note-Off for the
    /// currently sounding note, pitch is changed without re-attacking"; "When
    /// turned off the voice assignment mode (polyphonic or monophonic) returns to
    /// the state it was in", and it is per channel.
    #[test]
    fn the_legato_footswitch_makes_a_channel_legato_until_it_is_up() {
        let mut s = Sim::new(POLY);
        s.cc(0, 68, 0x40);
        s.on(0, 60);
        let c = s.on(0, 62);
        assert!(
            matches!(c[..], [Command::Retarget { id: v, .. }] if v == id(1)),
            "{c:?}"
        );
        // Another channel is still polyphonic.
        s.on(1, 60);
        assert!(matches!(s.on(1, 62)[..], [Command::Start { .. }]));
        // Up: poly again, and the first note's key sounds as the voice's.
        s.cc(0, 68, 0x3F);
        assert!(matches!(s.on(0, 64)[..], [Command::Start { .. }]));
    }

    /// M1 p.16, Example 2: `B0 54 3C` (PTC from #60), `90 40 40` (Note-On #64)
    /// is "#64 with glide from #60"; "A single Portamento Control message only
    /// affects the next Note-On received on the matching channel (i.e. it is
    /// reset after the Note-On)". The glide is at the portamento time's rate,
    /// "ignoring portamento on/off".
    #[test]
    fn a_portamento_control_glides_the_next_note_only() {
        let mut s = Sim::new(POLY);
        assert_eq!(s.cc(0, 84, 60), []);
        let c = s.on(0, 64);
        let glide = glide_of(&c).expect("the note glides");
        assert_eq!(glide.from, NoteNumber::of(60));
        // 400 cents at CC 5 = 0, the fastest rate of GM2 section 3.3.3's
        // figure (1,000 cent/msec): 0.4 ms.
        assert!((glide.time.get() - 0.0004).abs() < 1e-6, "{glide:?}");
        assert_eq!(glide_of(&s.on(0, 65)), None);
        // Another channel's note is not the one it is for.
        s.cc(0, 84, 60);
        assert_eq!(glide_of(&s.on(1, 64)), None);
    }

    /// M1 p.16, Example 1: #60 on; `B0 54 3C` "no change in current note";
    /// `90 40 40` "re-tune from #60 to #64" (and "no new voice should be
    /// assigned"); `80 3C 40` "no change"; `80 40 40` "#64 off". The note is
    /// "turned off by a note that matches the Note-On key number; not the key
    /// number stated in the Portamento Control message".
    #[test]
    fn a_portamento_control_from_a_sounding_key_retunes_that_voice() {
        let mut s = Sim::new(POLY);
        s.on(0, 60);
        assert_eq!(s.cc(0, 84, 60), []);
        let c = s.on(0, 64);
        assert!(
            matches!(&c[..], [Command::Retarget { id: v, key, glide: Some(g), .. }]
                if *v == id(1) && *key == NoteNumber::of(64) && g.from == NoteNumber::of(60)),
            "{c:?}"
        );
        assert_eq!(s.off(0, 60), []);
        assert_eq!(s.off(0, 64), [release(1)]);
    }

    /// M1 p.16: "if in Mono mode or if Legato Footswitch is currently on, a new
    /// overlapped note event would result in an immediate pitch jump to the key
    /// specified by the portamento Control message, and then a glide": the move
    /// leaves the Portamento Control's key, not the sounding one.
    #[test]
    fn a_legato_move_with_a_portamento_control_leaves_that_key() {
        let mut s = Sim::new(LEGATO);
        s.on(0, 60);
        s.cc(0, 84, 55);
        let c = s.on(0, 64);
        assert!(
            matches!(&c[..], [Command::Retarget { glide: Some(g), .. }] if g.from == NoteNumber::of(55)),
            "{c:?}"
        );
    }

    /// GM2 section 3.3.8: Portamento On/Off "Default Value: 0 (OFF)", "values
    /// between 0 and 63 are recognized as OFF; values between 64 and 127 are
    /// recognized as ON". With it on a note glides from the key struck before it,
    /// in the time GM2 section 3.3.3's rate gives the interval: at CC 5 = 127
    /// (0.01 cent/msec, the figure's slowest) a semitone takes 10 s; and the
    /// default (0) is the fastest, 1,000 cent/msec.
    #[test]
    fn portamento_on_glides_from_the_previous_key_at_the_time_cc_5_sets() {
        let mut s = Sim::new(POLY);
        s.on(0, 60);
        assert_eq!(glide_of(&s.on(0, 61)), None, "off by default");
        s.cc(0, 65, 63);
        assert_eq!(glide_of(&s.on(0, 62)), None, "63 is still off");
        s.cc(0, 65, 64);
        s.cc(0, 5, 127);
        let g = glide_of(&s.on(0, 63)).expect("on");
        assert_eq!(g.from, NoteNumber::of(62));
        assert!((g.time.get() - 10.0).abs() < 1e-3, "{g:?}");
        s.cc(0, 5, 0);
        let g = glide_of(&s.on(0, 75)).expect("on");
        assert_eq!(g.from, NoteNumber::of(63));
        assert!((g.time.get() - 0.0012).abs() < 1e-6, "{g:?}");
    }

    /// RP-022: RPN 01 is Channel Fine Tuning and RPN 02 Channel Coarse Tuning;
    /// the channel's notes sound that far from their keys (M1 RPN table: fine is
    /// 14 bits centred at 2000H over 100 cents, coarse an MSB of semitones centred
    /// at 64), and a bend adds to it.
    #[test]
    fn channel_tuning_retunes_the_channel() {
        let mut s = Sim::new(POLY);
        // Coarse: MSB 65 is one semitone up.
        assert_eq!(
            retunes(&s.send(2, registered(2, 65 << 25), &One)),
            [(2, 1.0)]
        );
        // Fine: 0x3000 is half way between 0x2000 and 0x4000, 50 cents up.
        let r = retunes(&s.send(2, registered(1, 0x3000 << 18), &One));
        assert!((r[0].1 - 1.5).abs() < 1e-4, "{r:?}");
        // Full bend up at range 2 adds 2.
        let r = retunes(&s.send(2, bend(u32::MAX), &One));
        assert!((r[0].1 - 3.5).abs() < 1e-4, "{r:?}");
        // Another channel is untouched.
        assert!(s.core.channel_pitch(MidiChannel::of(3)).0 == 0.0);
    }

    /// RP-015: "Set Expression (#11) to 127; Set Modulation (#1) to 0; Set
    /// Pedals (#64, #65, #66, #67) to 0; [...] Reset channel pressure to 0.
    /// [...] Do NOT reset Volume (#7); Do NOT reset Pan (#10); [...] Do NOT
    /// reset registered or non-registered parameters" (the Modulation Depth
    /// Range, the tuning and the sensitivity), and it is the channel's own.
    #[test]
    fn reset_all_controllers_follows_rp_015_on_its_channel() {
        let mut s = Sim::new(POLY);
        for c in [0, 1] {
            s.cc(c, 1, 100);
            s.cc(c, 7, 50);
            s.cc(c, 10, 20);
            s.cc(c, 11, 30);
            s.cc(c, 65, 127);
            s.send(c, Voice2::ChannelPressure(u32::MAX), &One);
            s.send(c, registered(5, 1 << 25), &One);
            s.send(c, registered(2, 66 << 25), &One);
        }
        s.cc(0, 121, 0);
        let ch = s.core.channel(MidiChannel::of(0));
        let position = |cc: u8| f32::from(cc) * 128.0 / 16383.0;
        assert_eq!(ch.expression(), position(127));
        assert_eq!(ch.modulation(), 0.0);
        assert_eq!(ch.pressure(), 0.0);
        assert!(!ch.portamento());
        assert_eq!(ch.volume(), position(50));
        assert_eq!(ch.pan(), {
            let mut probe = Sim::new(POLY);
            probe.cc(0, 10, 20);
            probe.core.channel(MidiChannel::of(0)).pan()
        });
        assert_eq!(ch.modulation_range(), tutti_types::Cents(100.0));
        assert_eq!(s.core.channel_pitch(MidiChannel::of(0)).0, 2.0);
        // Channel 1 heard none of it.
        let other = s.core.channel(MidiChannel::of(1));
        assert_eq!(other.modulation(), position(100));
        assert_eq!(other.expression(), position(30));
        assert_eq!(other.pressure(), 1.0);
        assert!(other.portamento());
    }

    /// RP-015: "Reset polyphonic pressure for all notes to 0". The Registered
    /// and Assignable Per-Note Controllers and Per-Note Pitch Bend are the ones
    /// M2-104 Appendix B.2 says are NOT reset.
    #[test]
    fn reset_all_controllers_zeroes_poly_pressure_and_leaves_the_other_per_note_values() {
        let mut s = Sim::new(POLY);
        s.send(
            0,
            Voice2::PolyPressure {
                note: NoteNumber::of(60),
                value: 1 << 31,
            },
            &One,
        );
        pnc(&mut s, 0, 60, RegisteredPerNoteController::Volume, 1 << 30);
        s.on(0, 60);
        let c = s.cc(0, 121, 0);
        assert!(
            c.iter().any(|x| matches!(
                x,
                Command::Control {
                    control: NoteControl::Pressure(0),
                    ..
                }
            )),
            "{c:?}"
        );
        assert!(
            !c.iter().any(|x| matches!(
                x,
                Command::Control {
                    control: NoteControl::Registered(..),
                    ..
                }
            )),
            "{c:?}"
        );
    }

    /// CA-026: RPN 5 scales Control Change 1; GM2 section 3.3.2 "The depth of
    /// change at the maximum value conforms to the value that is set by
    /// Modulation Depth Range", section 3.4.4: default "+/-50 cents", and "MSB =
    /// 01H, LSB = 00H" is one semitone.
    #[test]
    fn the_modulation_depth_range_scales_the_wheel() {
        let mut s = Sim::new(POLY);
        s.cc(0, 1, 127);
        s.cc(0, 33, 127);
        let depth = |s: &Sim| s.core.channel(MidiChannel::of(0)).modulation_depth().get();
        assert!((depth(&s) - 50.0).abs() < 1e-3);
        s.send(0, registered(5, 1 << 25), &One);
        assert!((depth(&s) - 100.0).abs() < 1e-3);
    }

    /// M1 Channel Pressure is per channel: the value is the channel's own.
    #[test]
    fn channel_pressure_is_per_channel() {
        let mut s = Sim::new(POLY);
        s.send(3, Voice2::ChannelPressure(1 << 31), &One);
        assert!((s.core.channel(MidiChannel::of(3)).pressure() - 0.5).abs() < 1e-3);
        assert_eq!(s.core.channel(MidiChannel::of(0)).pressure(), 0.0);
    }

    // ---- random streams ------------------------------------------------

    struct Rng(u64);
    impl Rng {
        fn below(&mut self, n: u64) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0 % n
        }
    }

    /// Random streams of notes (one and two layers), pedals, panics and
    /// resets, in every mode and strategy, against what must hold after every
    /// message:
    /// - never more voices than the polyphony;
    /// - `Steal::Never` disturbs no sounding voice when it drops a note;
    /// - a mono or legato channel has at most one unreleased voice for each
    ///   layer, all on the key at the top of its held stack when it has one;
    /// - after every key and pedal is let go, no note is unreleased;
    /// - every command names a voice the instrument has.
    #[test]
    fn random_streams_keep_their_invariants() {
        for stream in 0..400u64 {
            let mut rng =
                Rng(0x9E37_79B9_7F4A_7C15 ^ (stream + 1).wrapping_mul(0xD1B5_4A32_D192_ED03));
            let mode = [Mode::Poly, Mode::Mono, Mode::Legato][(stream % 3) as usize];
            let steal = [
                Steal::By(Order::Oldest),
                Steal::By(Order::Newest),
                Steal::By(Order::Quietest),
                Steal::By(Order::HighestNote),
                Steal::By(Order::LowestNote),
                Steal::Never,
            ][((stream / 3) % 6) as usize];
            let rules = Rules {
                mode,
                steal,
                polyphony: 6,
                retrigger: if stream % 2 == 0 {
                    Retrigger::Replace
                } else {
                    Retrigger::Layer
                },
                ..POLY
            };
            let layers = stream % 4 >= 2;
            let mut s = Sim::new(rules);
            // What the keys are, in the order they went down.
            let mut order: [Vec<u8>; 2] = [vec![], vec![]];
            let mut pedal = [false; 2];
            let mut sost = [false; 2];
            let pair = Pair { group: 0, scope: 0 };
            for _ in 0..80 {
                let c = rng.below(2) as u8;
                let cu = usize::from(c);
                // More distinct keys than the held stack holds.
                let k = 40 + rng.below(24) as u8;
                let before: Vec<VoiceId> = s.voices.iter().map(|v| v.id).collect();
                let full = s.voices.len() >= 6;
                let op = rng.below(16);
                let note_msg = op <= 8;
                let cmds = match op {
                    0..=4 => {
                        order[cu].retain(|&x| x != k);
                        order[cu].push(k);
                        if layers {
                            s.on_with(c, k, &pair)
                        } else {
                            s.on(c, k)
                        }
                    }
                    5..=8 => {
                        order[cu].retain(|&x| x != k);
                        if layers {
                            s.off_with(c, k, &pair)
                        } else {
                            s.off(c, k)
                        }
                    }
                    9 => {
                        pedal[cu] = !pedal[cu];
                        s.cc(c, 64, if pedal[cu] { 127 } else { 0 })
                    }
                    10 => {
                        order[cu].clear();
                        s.cc(c, [120, 123, 124, 125, 126, 127][rng.below(6) as usize], 0)
                    }
                    11 => {
                        order[cu].clear();
                        pedal[cu] = false;
                        sost[cu] = false;
                        s.cc(c, 121, 0)
                    }
                    12 => {
                        sost[cu] = !sost[cu];
                        s.cc(c, 66, if sost[cu] { 127 } else { 0 })
                    }
                    _ => {
                        // The instrument ends a voice by itself (a sample ran
                        // out): the core is told at the next note message.
                        if !s.voices.is_empty() {
                            let at = rng.below(s.voices.len() as u64) as usize;
                            let gone = s.voices[at].id;
                            s.end(gone);
                        }
                        vec![]
                    }
                };
                assert!(
                    s.voices.len() <= 6,
                    "stream {stream}: {} voices",
                    s.voices.len()
                );
                if note_msg {
                    // Every command names a voice the instrument has, or one
                    // this very message starts.
                    for cmd in &cmds {
                        let named = match *cmd {
                            Command::Release { id, .. }
                            | Command::Cut(id)
                            | Command::Damp(id)
                            | Command::Retarget { id, .. }
                            | Command::Control { id, .. }
                            | Command::ResetControls(id) => Some(id),
                            Command::Start { steal, .. } => steal,
                            Command::Retune { .. } => None,
                        };
                        if let Some(v) = named {
                            assert!(
                                before.contains(&v) || starts_in(&cmds, v),
                                "stream {stream}: {cmd:?} names a voice the instrument lacks"
                            );
                        }
                    }
                    // And the core keeps no voice the instrument lacks.
                    assert!(
                        s.core
                            .notes
                            .iter()
                            .all(|n| s.voices.iter().any(|v| v.id == n.id)),
                        "stream {stream}: the core holds a voice the instrument ended"
                    );
                }
                if steal == Steal::Never && full && mode == Mode::Poly && op <= 4 {
                    assert!(
                        before.iter().all(|b| s.voices.iter().any(|v| v.id == *b)),
                        "stream {stream}: Never disturbed a voice: {cmds:?}"
                    );
                }
                if mode != Mode::Poly {
                    for chan in 0..2u8 {
                        // The held stack is the keys that are down, the
                        // newest HELD_KEYS of them, in order.
                        let held = &s.core.held[usize::from(chan)];
                        let keys: Vec<u8> =
                            held.keys[..held.len].iter().map(|k| k.0.get()).collect();
                        let o = &order[usize::from(chan)];
                        assert_eq!(
                            keys,
                            o[o.len().saturating_sub(HELD_KEYS)..],
                            "stream {stream}: channel {chan}'s held stack"
                        );
                        let unreleased: Vec<&Note> = s
                            .core
                            .notes
                            .iter()
                            .filter(|n| n.channel.get() == chan && !n.released)
                            .collect();
                        let layers_n = if layers { 2 } else { 1 };
                        for tag in 0..layers_n {
                            assert!(
                                unreleased.iter().filter(|n| n.tag == tag).count() <= 1,
                                "stream {stream}: two unreleased voices of layer {tag} on channel {chan}"
                            );
                        }
                        if let Some((top, _)) = held.top() {
                            assert!(
                                unreleased.iter().all(|n| n.key == top),
                                "stream {stream}: channel {chan} sounds {:?}, the stack's top is {top:?}",
                                unreleased.iter().map(|n| n.key).collect::<Vec<_>>()
                            );
                        }
                    }
                }
            }
            // Let everything go.
            for c in 0..2u8 {
                if pedal[usize::from(c)] {
                    s.cc(c, 64, 0);
                }
                if sost[usize::from(c)] {
                    s.cc(c, 66, 0);
                }
                // A mono channel that returned to a held key needs the keys
                // let go again to reach the last.
                for _ in 0..2 {
                    for k in 40..64u8 {
                        if layers {
                            s.off_with(c, k, &pair);
                        } else {
                            s.off(c, k);
                        }
                    }
                }
            }
            assert!(
                s.core.notes.iter().all(|n| n.released),
                "stream {stream}: a note is still unreleased after every key and pedal went: {:?}",
                s.core.notes.iter().filter(|n| !n.released).count()
            );
        }
    }

    /// Random streams of pitch bends, RPN 0 and resets, with and without MPE
    /// zones, against a model that keeps the wheel and the sensitivity of each
    /// channel and sums a member's with its master's: every `Retune` says the
    /// model's figure, and a bend always retunes its own channel.
    #[test]
    fn random_bend_streams_match_the_model() {
        for stream in 0..200u64 {
            let mut rng =
                Rng(0xA5A5_1234_5678_9ABC ^ (stream + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15));
            let (lo, up) = [(0u8, 0u8), (3, 0), (0, 4), (5, 6)][(stream % 4) as usize];
            let zones = zones(lo, up);
            let mut s = Sim::new(POLY);
            s.core.set_zones(zones);
            let mut pos = [0.0f64; 16];
            let mut range = [2.0f64; 16];
            let master = |c: usize| -> Option<usize> {
                let (lo, up) = (usize::from(lo), usize::from(up));
                if (1..=lo).contains(&c) {
                    Some(0)
                } else if c < 15 && c >= 15 - up {
                    Some(15)
                } else {
                    None
                }
            };
            for _ in 0..40 {
                let c = rng.below(16) as u8;
                let cu = usize::from(c);
                let (cmds, bent) = match rng.below(4) {
                    0 | 1 => {
                        let raw = (rng.below(1 << 32)) as u32;
                        pos[cu] = (f64::from(raw) - 2147483648.0) / 2147483648.0;
                        (s.send(c, bend(raw), &One), true)
                    }
                    2 => {
                        let r = rng.below(24) as f32;
                        range[cu] = f64::from(r);
                        (s.send(c, rpn0(r), &One), true)
                    }
                    _ => {
                        pos[cu] = 0.0;
                        (s.cc(c, 121, 0), true)
                    }
                };
                for (ch, got) in retunes(&cmds) {
                    let chu = usize::from(ch);
                    let want =
                        pos[chu] * range[chu] + master(chu).map_or(0.0, |m| pos[m] * range[m]);
                    assert!(
                        (f64::from(got) - want).abs() < 1e-3,
                        "stream {stream}: channel {ch} retuned to {got}, the model says {want}"
                    );
                }
                if bent {
                    assert!(
                        retunes(&cmds).iter().any(|r| r.0 == c),
                        "stream {stream}: a message on channel {c} did not retune it"
                    );
                }
            }
        }
    }

    fn starts_in(cmds: &[Command], v: VoiceId) -> bool {
        cmds.iter()
            .any(|c| matches!(c, Command::Start { id, .. } if *id == v))
    }
}
