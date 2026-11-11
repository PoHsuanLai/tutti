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

use tutti_midi_protocol::{MpeZones, bend_u32_to_signed_f32, narrow_value};
use tutti_types::{Amplitude, Semitones};
use tutti_ump::{MidiChannel, NoteNumber, Rpn, Voice2};

use crate::{Controller, Effect, Holds, Phase, Stop};

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
    },
    /// Move a sounding voice to a new key without a new attack (legato).
    Retarget {
        /// The voice.
        id: VoiceId,
        /// The new key.
        key: NoteNumber,
        /// The new velocity.
        velocity: u16,
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

#[derive(Debug, Clone, Copy)]
struct Bend {
    /// The wheel, `-1..=1`.
    position: f32,
    range: Semitones,
}

impl Bend {
    fn semitones(self) -> Semitones {
        self.range * self.position
    }
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

/// The note logic. See the module docs.
#[derive(Debug)]
pub struct Voicing {
    rules: Rules,
    /// Sorted by id: voices are appended as they start.
    notes: Vec<Note>,
    sustain: [bool; CHANNELS],
    bends: [Bend; CHANNELS],
    zones: Option<MpeZones>,
    next: u64,
    strike: u64,
    held: [Held; CHANNELS],
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
            bends: self.bends,
            zones: self.zones,
            next: self.next,
            strike: self.strike,
            held: self.held,
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
            out: Vec::with_capacity(2 * rules.polyphony + CHANNELS + MAX_SPECS),
            rules,
            sustain: [false; CHANNELS],
            bends: [Bend {
                position: 0.0,
                range: rules.bend_range,
            }; CHANNELS],
            zones: None,
            next: 1,
            strike: 0,
            held: [Held::NONE; CHANNELS],
        }
    }

    /// Which channels are MPE zones (`None`: no MPE). A zone master's pitch
    /// bend adds to each of its members' own (RP-053); with no zone every
    /// channel is its own. No command is returned: the instrument sets this
    /// when it learns the layout.
    pub fn set_zones(&mut self, zones: Option<MpeZones>) {
        self.zones = zones;
    }

    /// Forgets every note, pedal, bend and held key: a fresh start. No command
    /// is returned; the instrument returns its pitches to centre. Ids are not
    /// reused.
    pub fn reset(&mut self) {
        self.forget_notes();
        self.bends = [Bend {
            position: 0.0,
            range: self.rules.bend_range,
        }; CHANNELS];
    }

    /// Forgets every note, pedal and held key, and keeps the bends: a channel's
    /// wheel and sensitivity are controller state, which outlives its notes.
    /// No command is returned; the instrument ends its voices.
    pub fn forget_notes(&mut self) {
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
        match *message {
            // A MIDI 2.0 Note On at velocity 0 is a Note On (M2-104 section
            // 7.4.2), played as quietly as there is. A MIDI 1.0 one is a Note
            // Off, and `widen` has already made it one.
            Voice2::NoteOn { note, velocity, .. } => {
                self.note_on(channel, note, velocity.max(1), patch, voices);
            }
            Voice2::NoteOff { note, velocity, .. } => {
                self.note_off(channel, note, velocity, patch, voices);
            }
            Voice2::ControlChange { controller, value } => {
                if let Some(effect) = Controller::effect(controller, narrow_value(value)) {
                    self.effect(channel, effect);
                }
            }
            Voice2::PitchBend(bend) => {
                let position = bend_u32_to_signed_f32(bend);
                self.bend(channel, |b| b.position = position);
            }
            Voice2::Registered { bank, index, value }
                if Rpn::new(bank, index) == Rpn::PITCH_BEND_SENSITIVITY =>
            {
                let range = bend_sensitivity(value);
                self.bend(channel, |b| b.range = range);
            }
            _ => {}
        }
        &self.out
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
        if self.rules.mode != Mode::Poly {
            self.play_mono(channel, key, velocity, patch, voices, false);
            return;
        }
        if self.rules.retrigger == Retrigger::Replace {
            self.release_where(|n| n.channel == channel && n.key == key);
        }
        let (specs, count) = Self::specs(patch, channel, key, velocity);
        for &spec in &specs[..count] {
            self.start_poly(channel, key, velocity, spec, voices);
        }
    }

    /// Liveness is read on a note message only: a voice the instrument no
    /// longer has (it ended, as a sampler's does at its sample's end) leaves
    /// the table, so the polyphony, the steal and a return to a held key see
    /// what sounds.
    fn reconcile(&mut self, voices: &[VoiceView]) {
        self.notes
            .retain(|n| voices.binary_search_by_key(&n.id, |v| v.id).is_ok());
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
    ) {
        self.damp_exclusive(spec, None);
        let steal = if self.notes.len() < self.rules.polyphony {
            None
        } else if let Some(v) = self.victim(voices) {
            Some(v)
        } else {
            return;
        };
        self.start(channel, key, velocity, spec, steal);
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
        let legato = self.rules.mode == Mode::Legato && (was_held || returning) && sounding;
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
                    self.notes[i].key = key;
                    self.notes[i].strike = strike;
                    self.push(Command::Retarget { id, key, velocity });
                }
                Some(i) => {
                    let id = self.notes[i].id;
                    self.start(channel, key, velocity, spec, Some(id));
                }
                None => self.start_poly(channel, key, velocity, spec, voices),
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
    ) {
        if let Some(v) = steal
            && let Ok(i) = self.notes.binary_search_by_key(&v, |n| n.id)
        {
            self.notes.remove(i);
        }
        let id = VoiceId(self.next);
        self.next += 1;
        self.notes.push(Note {
            id,
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
        if self.rules.mode != Mode::Poly {
            let was_top = self.held[ch].top().map(|k| k.0) == Some(key);
            if self.held[ch].remove(key) {
                // A held key under the sounding one changes nothing; the
                // sounding one's release returns to the most recent held key.
                if !was_top {
                    return;
                }
                if let Some((back, v)) = self.held[ch].top() {
                    self.strike += 1;
                    self.play_mono(channel, back, v, patch, voices, true);
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

    fn effect(&mut self, channel: MidiChannel, effect: Effect) {
        let ch = usize::from(channel.get());
        let on_channel = move |n: &Note| n.channel == channel;
        match effect {
            Effect::Sustain(down) => {
                self.sustain[ch] = down;
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
                self.held[ch] = Held::NONE;
                let pedal = self.sustain[ch];
                self.each_note(on_channel, |n| n.holds.key_up(pedal));
            }
            Effect::Stop(Stop::Cut) => {
                self.held[ch] = Held::NONE;
                let mut i = 0;
                while i < self.notes.len() {
                    if self.notes[i].channel == channel {
                        let id = self.notes[i].id;
                        self.push(Command::Cut(id));
                        self.notes.remove(i);
                    } else {
                        i += 1;
                    }
                }
            }
            // Reset All Controllers puts the pedals up and centres the wheel;
            // the sensitivity stays (RP-015).
            Effect::ResetControllers => {
                self.sustain[ch] = false;
                self.held[ch] = Held::NONE;
                self.each_note(on_channel, |n| {
                    let a = n.holds.sustain_up();
                    let b = n.holds.sostenuto_up();
                    a || b
                });
                self.bend(channel, |b| b.position = 0.0);
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
        let own = self.bends[channel].semitones();
        self.master_of(channel)
            .map_or(own, |m| own + self.bends[m].semitones())
    }

    /// Changes the wheel or the sensitivity of `channel`, and tells the
    /// instrument where every channel it moves ended up: its own, and, if it is
    /// a zone master, each member's.
    fn bend(&mut self, channel: MidiChannel, change: impl Fn(&mut Bend)) {
        let ch = usize::from(channel.get());
        change(&mut self.bends[ch]);
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
    use tutti_midi_protocol::MpeZoneConfig;
    use tutti_midi_protocol::widen;
    use tutti_ump::{Attribute, CCNumber, U7, Voice1};

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

    /// Mutation (run): `<` to `<=` in `start_poly` starts a fifth voice.
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
    ///
    /// Mutation (run): `MAX_SPECS` at 64 starts 40 and fails the first
    /// assertion; at 16 fails the second.
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

    /// Mutation (run): dropping the released-first class makes the 62 (held)
    /// go instead of the released 63.
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

    /// Mutation (run): `Steal::Never` read as `By(Oldest)` starts the note.
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

    /// Mutation (run): each order inverted fails its row; `Quietest` ranked by
    /// `to_bits` instead of `total_cmp` is the same for these positive levels
    /// and is not claimed here.
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

    /// Mutation (run): Replace not releasing makes the first assertion fail.
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
    ///
    /// Mutation (run): matching on the channel as well makes the other
    /// channel's voice survive.
    #[test]
    fn an_exclusive_group_ends_its_scopes_others_on_any_channel() {
        let mut s = Sim::new(EXCLUSIVE);
        s.on_with(1, 60, &Grouped { scope: 5 });
        let c = s.on_with(0, 62, &Grouped { scope: 5 });
        assert_eq!(c[0], Command::Damp(id(1)), "{c:?}");
    }

    /// The same group number in another preset is another group.
    ///
    /// Mutation (run): ignoring the scope cuts it.
    #[test]
    fn a_group_number_means_nothing_across_scopes() {
        let mut s = Sim::new(EXCLUSIVE);
        s.on_with(0, 60, &Grouped { scope: 5 });
        let c = s.on_with(0, 62, &Grouped { scope: 6 });
        assert!(!c.iter().any(|x| matches!(x, Command::Damp(_))), "{c:?}");
    }

    /// The voices one strike starts (a stereo pair, both of one class) do not
    /// end each other; the next strike ends both.
    ///
    /// Mutation (run): dropping the strike test cuts the pair's first voice
    /// when its second starts.
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
    ///
    /// Mutation (run): the `124..=127` arm removed fails every row.
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
    ///
    /// Mutation (run): the core reading a velocity 0 `NoteOn` as an off fails
    /// the second half.
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
    ///
    /// Mutation (run): choosing the prior voice without regard to channel
    /// makes the channel-1 note take the channel-0 voice.
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
    ///
    /// Mutation (run): finding the prior voice without the layer's tag makes
    /// the second layer take the first layer's fresh voice. (Dropping only
    /// the `n.strike != strike` test there is an equivalent mutant while a
    /// patch's tags are unique, which `Patch` requires.)
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
    ///
    /// Mutation (run): as for mono; and a Retarget that leaves the voice's
    /// strike alone lets the strike's own end-of-note sweep release it.
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
                    velocity: 0x8000
                },
                Command::Retarget {
                    id: id(2),
                    key: NoteNumber::of(62),
                    velocity: 0x8000
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
                    velocity: 0x8000
                },
                release(2)
            ]
        );
    }

    /// The layer a key keeps is taken over by tag, not by position: a patch
    /// whose upper key lacks layer 0 leaves layer 1's voice to layer 1.
    ///
    /// Mutation (run): matching the prior voice without `n.tag == spec.tag`
    /// takes layer 0's voice for layer 1.
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
    ///
    /// Mutation (run): a release that never looks at the stack releases the
    /// voice while 60 is still held.
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
                velocity: 0x3000
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
    ///
    /// Mutation (run): not clearing in one of the arms leaves a held key that
    /// the next release returns to.
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
    ///
    /// Mutation (run): dropping the newest instead returns to a wrong key.
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
    ///
    /// Mutation (run): `holds` without the released filter gives the first
    /// voice `key: true` while it is releasing.
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
    ///
    /// Mutation (run): no `reconcile` in `note_off` gives a Retarget (legato) or
    /// a Start stealing (mono) of the ended voice.
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
    ///
    /// Mutation (run): not setting `released` in `damp_exclusive` makes the
    /// note-off release it again and fails the first assertion; not setting
    /// `damped` damps it again on the third strike and fails the second.
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
    ///
    /// Mutation (run): no `damp_exclusive` in `play_mono` leaves channel 1's
    /// grouped voice sounding.
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
    ///
    /// Mutation (run, seen to fail): `forget_notes` also resetting `bends`
    /// loses the 2-semitone bend at the first assert.
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
    ///
    /// Mutation (run, seen to fail): reading the data as Q7.25 (`from_q7_25`)
    /// gives 12.25 for cents 50 and 12.5 for cents 64.
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
    ///
    /// Mutation (run): the master's bend written over the member's own makes
    /// the sum 2 where it is 3.
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
        let (n, o) = (s.core.notes.capacity(), s.core.out.capacity());
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
        assert_eq!((s.core.notes.capacity(), s.core.out.capacity()), (n, o));
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
    ///
    /// Mutations (run), each seen to fail: `<` to `<=` in `start_poly` (the
    /// polyphony bound); the held-stack return skipped (the top-of-stack
    /// check); `Steal::Never` taken as `By(Oldest)` (the disturbance check);
    /// the pedal not lifting in `Effect::Sustain` (the final check); the
    /// `n.strike != strike` leftover test dropped in `play_mono` (the
    /// one-voice-per-layer check).
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
                            | Command::Retarget { id, .. } => Some(id),
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
    ///
    /// Mutations (run), each seen to fail: the master's term dropped in
    /// `pitch`; `Bend` taking the sensitivity from the wrong channel; a reset
    /// that also zeroes the sensitivity.
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
