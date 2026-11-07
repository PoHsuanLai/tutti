//! The note logic of an instrument, as one pure step.
//!
//! `Voicing::step(channel, message, patch, voices)` reads one channel voice
//! message and the voices that exist, and returns the [`Command`]s the
//! instrument must carry out. It renders no audio, owns no voice and allocates
//! nothing after [`Voicing::new`]. What differs between instruments is a value
//! of [`Rules`] and a [`Patch`]; the logic below is the same for all of them.
//!
//! Reading order: [`Rules`] (the declared differences), [`Command`] (what comes
//! out), then `Voicing::step`, which is the whole of it.

use tutti_midi_protocol::narrow_value;
use tutti_types::{Amplitude, Frame, Samples, Semitones};
use tutti_ump::{MidiChannel, NoteNumber, Pitch, Voice2};

use crate::{Controller, Effect, Holds, Phase, Stop};

const CHANNELS: usize = MidiChannel::COUNT as usize;

/// The registered parameter that sets a channel's pitch bend sensitivity: bank
/// and index (MIDI 1.0, Registered Parameter Numbers, RPN 0).
const BEND_SENSITIVITY: (u8, u8) = (0, 0);

/// The most voices one note may start. A patch that asks for more is cut short.
const MAX_SPECS: usize = 32;

/// A voice, named in the order the core started them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct VoiceId(pub u32);

/// How many voices sound for a channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Any number, up to the polyphony.
    Poly,
    /// One for each channel: a new note takes over the voice the channel has.
    Mono,
    /// One for each channel: a new note while one is held moves the voice to
    /// the new key without a new attack.
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
/// goes first.
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
    /// Whether a voice with a group number ends the others of its group on its
    /// channel.
    pub exclusive_groups: bool,
    /// Whether CC 66 holds the notes sounding when it goes down.
    pub sostenuto: bool,
    /// What a full pitch bend spans on a channel until RPN 0 says otherwise.
    pub bend_range: Semitones,
}

/// One voice a note becomes: what [`Patch::voices`] hands the core.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VoiceSpec {
    /// Its exclusive group, `0` for none.
    pub group: u16,
    /// The patch's own name for it (a region number), given back in
    /// [`Command::Start`].
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
        /// The strike's velocity, 16 bits.
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
    Release(VoiceId),
    /// Silence a voice at once and free its slot.
    Cut(VoiceId),
    /// A channel's pitch moved: its notes sound `bend` away from their keys.
    /// Sent for each channel a bend, its sensitivity or a reset reaches.
    Retune {
        /// The channel.
        channel: MidiChannel,
        /// The wheel's position times the sensitivity.
        bend: Semitones,
    },
}

#[derive(Debug, Clone, Copy)]
struct Note {
    id: VoiceId,
    channel: MidiChannel,
    key: NoteNumber,
    group: u16,
    started: Frame,
    holds: Holds,
    released: bool,
}

#[derive(Debug, Clone, Copy)]
struct Bend {
    /// The wheel, `-1..=1`.
    position: f32,
    range: Semitones,
}

/// The note logic. See the module docs.
#[derive(Debug)]
pub struct Voicing {
    rules: Rules,
    notes: Vec<Note>,
    sustain: [bool; CHANNELS],
    bends: [Bend; CHANNELS],
    /// Whether channels 0 and 15 are zone masters, whose bend reaches every
    /// channel (MPE).
    zone_masters: bool,
    clock: Frame,
    next: u32,
    legato_from: [Option<NoteNumber>; CHANNELS],
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
            zone_masters: self.zone_masters,
            clock: self.clock,
            next: self.next,
            legato_from: self.legato_from,
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
            zone_masters: false,
            clock: Frame::ZERO,
            next: 1,
            legato_from: [None; CHANNELS],
        }
    }

    /// Whether channels 0 and 15 are zone masters (MPE): their pitch bend, its
    /// sensitivity and its reset reach every channel.
    pub fn set_zone_masters(&mut self, on: bool) {
        self.zone_masters = on;
    }

    /// Time passes: `frames` older. Age is how "oldest" is measured.
    pub fn advance(&mut self, frames: Samples) {
        self.clock += frames;
    }

    /// Forgets every note, pedal, bend and the legato notes: a fresh start. No
    /// command is returned; the instrument returns its pitches to centre.
    pub fn reset(&mut self) {
        self.notes.clear();
        self.sustain = [false; CHANNELS];
        self.bends = [Bend {
            position: 0.0,
            range: self.rules.bend_range,
        }; CHANNELS];
        self.legato_from = [None; CHANNELS];
    }

    /// What holds `id` open, if it is a voice the core knows.
    #[must_use]
    pub fn holds(&self, id: VoiceId) -> Option<Holds> {
        self.note(id).map(|n| n.holds)
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
        self.notes.iter().find(|n| n.id == id)
    }

    fn push(&mut self, c: Command) {
        debug_assert!(self.out.len() < self.out.capacity(), "command buffer full");
        if self.out.len() < self.out.capacity() {
            self.out.push(c);
        }
    }

    /// Reads one message on `channel`, with the voices that exist, and returns
    /// what to do. The slice is valid until the next call.
    pub fn step<P: Patch + ?Sized>(
        &mut self,
        channel: MidiChannel,
        message: &Voice2,
        patch: &P,
        voices: &[VoiceView],
    ) -> &[Command] {
        self.out.clear();
        self.notes.retain(|n| voices.iter().any(|v| v.id == n.id));
        match *message {
            Voice2::NoteOn { note, velocity, .. } if velocity != 0 => {
                self.note_on(channel, note, velocity, patch, voices);
            }
            Voice2::NoteOn { note, .. } | Voice2::NoteOff { note, .. } => {
                self.note_off(channel, note);
            }
            Voice2::ControlChange { controller, value } => {
                if let Some(effect) = Controller::effect(controller, narrow_value(value)) {
                    self.effect(channel, effect);
                }
            }
            Voice2::PitchBend(bend) => {
                let position =
                    ((f64::from(bend) - f64::from(1u32 << 31)) / f64::from(1u32 << 31)) as f32;
                self.bend(channel, |b| b.position = position);
            }
            Voice2::Registered { bank, index, value }
                if (bank.get(), index.get()) == BEND_SENSITIVITY =>
            {
                let range = Semitones(Pitch::from_q7_25(value).note_number());
                self.bend(channel, |b| b.range = range);
            }
            _ => {}
        }
        &self.out
    }

    fn note_on<P: Patch + ?Sized>(
        &mut self,
        channel: MidiChannel,
        key: NoteNumber,
        velocity: u16,
        patch: &P,
        voices: &[VoiceView],
    ) {
        if self.rules.retrigger == Retrigger::Replace && self.rules.mode == Mode::Poly {
            self.release_where(|n| n.channel == channel && n.key == key);
        }
        let mut specs = [VoiceSpec { group: 0, tag: 0 }; MAX_SPECS];
        let mut count = 0;
        patch.voices(channel, key, velocity, &mut |s| {
            if count < MAX_SPECS {
                specs[count] = s;
                count += 1;
            }
        });
        for &spec in &specs[..count] {
            match self.rules.mode {
                Mode::Poly => self.start_poly(channel, key, velocity, spec, voices),
                Mode::Mono | Mode::Legato => self.start_mono(channel, key, velocity, spec, voices),
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
        if self.rules.exclusive_groups && spec.group != 0 {
            let mut i = 0;
            while i < self.notes.len() {
                let n = self.notes[i];
                if n.group == spec.group && n.channel == channel {
                    self.push(Command::Cut(n.id));
                    self.notes.remove(i);
                } else {
                    i += 1;
                }
            }
        }
        let steal = if self.notes.len() < self.rules.polyphony {
            None
        } else if let Some(v) = self.victim(voices) {
            Some(v)
        } else {
            return;
        };
        self.start(channel, key, velocity, spec, steal);
    }

    /// Mono and legato: the channel has one voice, and a new note takes it over,
    /// sounding or released (a released tail is cut, as on a monosynth).
    fn start_mono(
        &mut self,
        channel: MidiChannel,
        key: NoteNumber,
        velocity: u16,
        spec: VoiceSpec,
        voices: &[VoiceView],
    ) {
        let ch = usize::from(channel.get());
        let own = self
            .notes
            .iter()
            .filter(|n| n.channel == channel)
            .max_by_key(|n| n.id)
            .map(|n| (n.id, n.released));
        if let (Mode::Legato, Some((id, false)), Some(_)) =
            (self.rules.mode, own, self.legato_from[ch])
        {
            if let Some(n) = self.notes.iter_mut().find(|n| n.id == id) {
                n.key = key;
            }
            self.legato_from[ch] = Some(key);
            self.push(Command::Retarget { id, key, velocity });
            return;
        }
        self.legato_from[ch] = Some(key);
        match own {
            Some((id, released)) => {
                if !released {
                    self.release_where(|n| n.id == id);
                }
                self.start(channel, key, velocity, spec, Some(id));
            }
            None => self.start_poly(channel, key, velocity, spec, voices),
        }
    }

    fn start(
        &mut self,
        channel: MidiChannel,
        key: NoteNumber,
        velocity: u16,
        spec: VoiceSpec,
        steal: Option<VoiceId>,
    ) {
        if let Some(v) = steal {
            self.notes.retain(|n| n.id != v);
        }
        let id = VoiceId(self.next);
        self.next += 1;
        self.notes.push(Note {
            id,
            channel,
            key,
            group: spec.group,
            started: self.clock,
            holds: Holds::struck(),
            released: false,
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

    /// Who gives way: released voices first, the quietest of them; then by
    /// [`Order`]. The first of equals in `voices`' order wins.
    fn victim(&self, voices: &[VoiceView]) -> Option<VoiceId> {
        let Steal::By(order) = self.rules.steal else {
            return None;
        };
        let level = |id| {
            voices
                .iter()
                .find(|v| v.id == id)
                .map_or(Amplitude(0.0), |v| v.level)
        };
        voices
            .iter()
            .filter_map(|v| self.note(v.id))
            .chain(
                self.notes
                    .iter()
                    .filter(|n| voices.iter().all(|v| v.id != n.id)),
            )
            .min_by_key(|n| {
                let l = u64::from(level(n.id).get().to_bits());
                if n.released {
                    (0, l)
                } else {
                    (
                        1,
                        match order {
                            Order::Oldest => n.started.get(),
                            Order::Newest => u64::MAX - n.started.get(),
                            Order::Quietest => l,
                            Order::HighestNote => u64::MAX - u64::from(n.key.get()),
                            Order::LowestNote => u64::from(n.key.get()),
                        },
                    )
                }
            })
            .map(|n| n.id)
    }

    fn note_off(&mut self, channel: MidiChannel, key: NoteNumber) {
        let ch = usize::from(channel.get());
        if self.rules.mode == Mode::Legato && self.legato_from[ch] == Some(key) {
            self.legato_from[ch] = None;
        }
        let pedal = self.sustain[ch];
        self.each_note(
            |n| n.channel == channel && n.key == key,
            |n| n.holds.key_up(pedal),
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
                let pedal = self.sustain[ch];
                self.each_note(on_channel, |n| n.holds.key_up(pedal));
            }
            Effect::Stop(Stop::Cut) => {
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
                self.each_note(on_channel, |n| {
                    let a = n.holds.sustain_up();
                    let b = n.holds.sostenuto_up();
                    a || b
                });
                self.bend(channel, |b| b.position = 0.0);
            }
        }
    }

    /// Changes the wheel or the sensitivity of `channel`, or of every channel
    /// when the zone masters' do, and tells the instrument where each ended up.
    fn bend(&mut self, channel: MidiChannel, change: impl Fn(&mut Bend)) {
        let ch = usize::from(channel.get());
        let all = self.zone_masters && matches!(ch, 0 | CHANNELS_LAST);
        let reach = if all { 0..CHANNELS } else { ch..ch + 1 };
        for i in reach {
            change(&mut self.bends[i]);
            let bend = self.bends[i].range * self.bends[i].position;
            if let Some(channel) = MidiChannel::new(i as u8) {
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
                let id = n.id;
                self.push(Command::Release(id));
            }
        }
    }
}

/// The upper zone master's channel.
const CHANNELS_LAST: usize = CHANNELS - 1;

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_ump::{Attribute, CCNumber, U7};

    /// One voice per note, no group.
    struct One;
    impl Patch for One {
        fn voices(&self, _: MidiChannel, _: NoteNumber, _: u16, spec: &mut dyn FnMut(VoiceSpec)) {
            spec(VoiceSpec { group: 0, tag: 0 });
        }
    }
    /// Two voices per note, the second in group 7.
    struct Two;
    impl Patch for Two {
        fn voices(&self, _: MidiChannel, _: NoteNumber, _: u16, spec: &mut dyn FnMut(VoiceSpec)) {
            spec(VoiceSpec { group: 0, tag: 0 });
            spec(VoiceSpec { group: 7, tag: 1 });
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

    /// The instrument's side: slots, in order, carrying out the commands.
    struct Sim {
        core: Voicing,
        slots: Vec<VoiceView>,
        log: Vec<Command>,
    }
    fn ch(n: u8) -> MidiChannel {
        MidiChannel::of(n)
    }
    fn key(n: u8) -> NoteNumber {
        NoteNumber::new(n).unwrap()
    }

    impl Sim {
        fn new(rules: Rules) -> Self {
            Self {
                core: Voicing::new(rules),
                slots: vec![],
                log: vec![],
            }
        }
        fn send<P: Patch>(&mut self, c: u8, m: Voice2, p: &P) -> Vec<Command> {
            let cmds = self.core.step(ch(c), &m, p, &self.slots).to_vec();
            for cmd in &cmds {
                match *cmd {
                    Command::Start { id, steal, .. } => {
                        let v = VoiceView {
                            id,
                            level: Amplitude(1.0),
                        };
                        match steal.and_then(|s| self.slots.iter().position(|x| x.id == s)) {
                            Some(i) => self.slots[i] = v,
                            None => self.slots.push(v),
                        }
                    }
                    Command::Cut(id) => self.slots.retain(|x| x.id != id),
                    _ => {}
                }
            }
            self.log.extend(&cmds);
            cmds
        }
        fn on(&mut self, c: u8, k: u8) -> Vec<Command> {
            self.on_with(c, k, &One)
        }
        fn on_with<P: Patch>(&mut self, c: u8, k: u8, p: &P) -> Vec<Command> {
            self.send(
                c,
                Voice2::NoteOn {
                    note: key(k),
                    velocity: 0x8000,
                    attribute: Attribute::None,
                },
                p,
            )
        }
        fn off(&mut self, c: u8, k: u8) -> Vec<Command> {
            self.send(
                c,
                Voice2::NoteOff {
                    note: key(k),
                    velocity: 0,
                    attribute: Attribute::None,
                },
                &One,
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
            self.slots.retain(|x| x.id != id);
        }
    }
    fn id(n: u32) -> VoiceId {
        VoiceId(n)
    }

    /// Mutation (run): `<` to `<=` in `start_poly` starts a fifth voice.
    #[test]
    fn the_oldest_gives_way_when_full() {
        let mut s = Sim::new(POLY);
        for k in 60..64 {
            s.on(0, k);
        }
        s.core.advance(Samples(10));
        let c = s.on(0, 64);
        assert_eq!(c.len(), 1);
        assert!(matches!(c[0], Command::Start { steal: Some(v), .. } if v == id(1)));
    }

    /// Mutation (run): dropping the released-first class makes the 62 (held,
    /// older than nothing) go instead of the released 63.
    #[test]
    fn a_released_voice_gives_way_before_a_held_one() {
        let mut s = Sim::new(POLY);
        for k in 60..64 {
            s.on(0, k);
            s.core.advance(Samples(10));
        }
        s.off(0, 63);
        let c = s.on(0, 64);
        assert!(
            matches!(c[0], Command::Start { steal: Some(v), .. } if v == id(4)),
            "{c:?}"
        );
    }

    #[test]
    fn never_drops_the_note() {
        let mut s = Sim::new(Rules {
            steal: Steal::Never,
            ..POLY
        });
        for k in 60..64 {
            s.on(0, k);
        }
        assert!(s.on(0, 64).is_empty());
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
                s.core.advance(Samples(10));
            }
            let c = s.on(0, 70);
            assert!(
                matches!(c[0], Command::Start { steal: Some(v), .. } if v == id(want)),
                "{order:?} {c:?}"
            );
        }
        // Quietest: the voice with the lowest level.
        let mut s = Sim::new(Rules {
            steal: Steal::By(Order::Quietest),
            ..POLY
        });
        for k in 60..64 {
            s.on(0, k);
        }
        s.slots[2].level = Amplitude(0.1);
        let c = s.on(0, 70);
        assert!(
            matches!(c[0], Command::Start { steal: Some(v), .. } if v == id(3)),
            "{c:?}"
        );
    }

    /// Mutation (run): Replace not releasing makes the first assertion fail.
    #[test]
    fn replace_releases_the_earlier_strike_and_layer_keeps_it() {
        let mut s = Sim::new(POLY);
        s.on(0, 60);
        let c = s.on(0, 60);
        assert_eq!(c[0], Command::Release(id(1)));
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
        let c = s.off(0, 60);
        assert_eq!(c, [Command::Release(id(1)), Command::Release(id(2))]);
    }

    #[test]
    fn a_note_is_as_many_voices_as_the_patch_says() {
        let mut s = Sim::new(POLY);
        let c = s.on_with(0, 60, &Two);
        assert_eq!(c.len(), 2);
        assert_eq!(s.off(0, 60).len(), 2);
    }

    /// SF2 2.04 exclusive class (generator 57, Est): a voice of a group cuts
    /// the others of that group on its channel, not another channel's.
    #[test]
    fn an_exclusive_group_cuts_its_own_channel_only() {
        let mut s = Sim::new(Rules {
            exclusive_groups: true,
            polyphony: 8,
            ..POLY
        });
        s.on_with(0, 60, &Two);
        s.on_with(1, 60, &Two);
        let c = s.on_with(0, 62, &Two);
        assert!(c.contains(&Command::Cut(id(2))), "{c:?}");
        assert!(
            !c.iter()
                .any(|x| matches!(x, Command::Cut(v) if *v == id(4)))
        );
        let mut s = Sim::new(Rules {
            polyphony: 8,
            ..POLY
        });
        s.on_with(0, 60, &Two);
        assert!(
            !s.on_with(0, 62, &Two)
                .iter()
                .any(|x| matches!(x, Command::Cut(_)))
        );
    }

    #[test]
    fn the_pedal_holds_until_it_comes_up() {
        let mut s = Sim::new(POLY);
        s.cc(0, 64, 127);
        s.on(0, 60);
        assert!(s.off(0, 60).is_empty());
        assert_eq!(s.core.phase(id(1)), Some(Phase::Pedal));
        assert_eq!(s.cc(0, 64, 0), [Command::Release(id(1))]);
        // Another channel's pedal holds nothing.
        let mut s = Sim::new(POLY);
        s.cc(1, 64, 127);
        s.on(0, 60);
        assert_eq!(s.off(0, 60), [Command::Release(id(1))]);
    }

    /// MIDI 1.0 CC 123 (Est): released, and a pedal still holds.
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
        assert_eq!(s.cc(0, 123, 0), [Command::Release(id(1))]);
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
                Command::Release(id(1)),
                Command::Retune {
                    channel: ch(0),
                    bend: Semitones(0.0)
                }
            ]
        );
        s.on(0, 62);
        assert_eq!(
            s.off(0, 62),
            [Command::Release(id(2))],
            "the pedal is up now"
        );
    }

    #[test]
    fn sostenuto_holds_what_sounds_when_it_goes_down() {
        let mut s = Sim::new(POLY);
        s.on(0, 60);
        s.cc(0, 66, 127);
        s.on(0, 62);
        assert!(s.off(0, 60).is_empty());
        assert_eq!(
            s.off(0, 62),
            [Command::Release(id(2))],
            "struck after the pedal"
        );
        assert_eq!(s.cc(0, 66, 0), [Command::Release(id(1))]);
        let mut s = Sim::new(Rules {
            sostenuto: false,
            ..POLY
        });
        s.on(0, 60);
        s.cc(0, 66, 127);
        assert_eq!(s.off(0, 60), [Command::Release(id(1))]);
    }

    #[test]
    fn mono_releases_the_sounding_note_and_takes_its_slot() {
        let mut s = Sim::new(Rules {
            mode: Mode::Mono,
            ..POLY
        });
        s.on(0, 60);
        let c = s.on(0, 62);
        assert_eq!(c[0], Command::Release(id(1)));
        assert!(matches!(c[1], Command::Start { steal: Some(v), .. } if v == id(1)));
    }

    #[test]
    fn legato_retargets_a_sounding_voice_and_starts_the_first_note_anew() {
        let mut s = Sim::new(Rules {
            mode: Mode::Legato,
            ..POLY
        });
        assert!(matches!(s.on(0, 60)[0], Command::Start { .. }));
        assert_eq!(
            s.on(0, 62),
            [Command::Retarget {
                id: id(1),
                key: key(62),
                velocity: 0x8000
            }]
        );
        // Everything released, the next note starts anew.
        s.off(0, 62);
        assert!(matches!(s.on(0, 64).last(), Some(Command::Start { .. })));
    }

    /// A voice that has run out (the instrument stops listing it) is forgotten:
    /// its slot is free, and nothing is commanded for it again.
    #[test]
    fn a_voice_that_ended_frees_its_slot() {
        let mut s = Sim::new(POLY);
        for k in 60..64 {
            s.on(0, k);
        }
        s.off(0, 60);
        s.end(id(1));
        let c = s.on(0, 64);
        assert!(matches!(c[0], Command::Start { steal: None, .. }), "{c:?}");
        assert!(s.off(0, 60).is_empty(), "nothing left of the ended voice");
    }

    /// The core allocates nothing after `new`: its tables never outgrow their capacity.
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
            s.core.advance(Samples(5));
        }
        assert_eq!((s.core.notes.capacity(), s.core.out.capacity()), (n, o));
    }

    fn bend(_c: u8, v: u32) -> Voice2 {
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
            bank: U7::new(0).unwrap(),
            index: U7::new(0).unwrap(),
            value: (range_semitones * (1u32 << 25) as f32) as u32,
        }
    }

    /// MIDI 1.0 Pitch Bend (Est): the wheel is a channel's, and full travel
    /// spans the sensitivity, 2 semitones until RPN 0 says otherwise.
    ///
    /// Mutation (run): dropping the `ch..ch + 1` reach (always all channels)
    /// makes the second assertion fail.
    #[test]
    fn a_bend_is_the_channels_and_spans_its_sensitivity() {
        let mut s = Sim::new(POLY);
        let up = s.send(3, bend(3, u32::MAX), &One);
        assert_eq!(retunes(&up).len(), 1);
        assert_eq!(retunes(&up)[0].0, 3);
        assert!((retunes(&up)[0].1 - 2.0).abs() < 1e-4, "{up:?}");
        let down = s.send(3, bend(3, 0), &One);
        assert!((retunes(&down)[0].1 + 2.0).abs() < 1e-6);
        let centre = s.send(3, bend(3, 1 << 31), &One);
        assert_eq!(retunes(&centre), [(3, 0.0)]);
    }

    /// RPN 0 sets the sensitivity of its channel only, and the wheel stays
    /// where it was, so the pitch moves with it.
    #[test]
    fn rpn_0_changes_the_range_and_the_pitch_with_it() {
        let mut s = Sim::new(POLY);
        s.send(0, bend(0, u32::MAX), &One);
        let c = s.send(0, rpn0(12.0), &One);
        assert!((retunes(&c)[0].1 - 12.0).abs() < 1e-3, "{c:?}");
        let other = s.send(1, bend(1, u32::MAX), &One);
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
        s.send(0, bend(0, u32::MAX), &One);
        let c = s.cc(0, 121, 0);
        assert_eq!(retunes(&c), [(0, 0.0)]);
        let again = s.send(0, bend(0, u32::MAX), &One);
        assert!((retunes(&again)[0].1 - 12.0).abs() < 1e-3);
    }

    /// Under MPE the zone masters' bend, sensitivity and reset reach every
    /// channel; a member's reaches its own.
    #[test]
    fn a_zone_master_reaches_every_channel_and_a_member_its_own() {
        let mut s = Sim::new(POLY);
        s.core.set_zone_masters(true);
        assert_eq!(retunes(&s.send(0, bend(0, u32::MAX), &One)).len(), 16);
        assert_eq!(retunes(&s.send(15, bend(15, 0), &One)).len(), 16);
        assert_eq!(retunes(&s.send(5, bend(5, u32::MAX), &One)).len(), 1);
        s.core.set_zone_masters(false);
        assert_eq!(retunes(&s.send(0, bend(0, u32::MAX), &One)).len(), 1);
    }

    /// `reset` returns the wheels and the sensitivities to the rules' own.
    #[test]
    fn reset_forgets_the_bends() {
        let mut s = Sim::new(POLY);
        s.send(0, rpn0(12.0), &One);
        s.send(0, bend(0, u32::MAX), &One);
        s.core.reset();
        let c = s.send(0, bend(0, u32::MAX), &One);
        assert!((retunes(&c)[0].1 - 2.0).abs() < 1e-4);
    }

    /// One voice a channel: a second channel's note does not take the first's.
    ///
    /// Mutation (run): taking the first voice of the slice, whatever its
    /// channel, makes the channel-1 note cut the channel-0 voice.
    #[test]
    fn mono_is_one_voice_for_each_channel() {
        let mut s = Sim::new(Rules {
            mode: Mode::Mono,
            ..POLY
        });
        s.on(0, 60);
        let other = s.on(1, 64);
        assert!(
            matches!(other[..], [Command::Start { steal: None, .. }]),
            "{other:?}"
        );
        let again = s.on(0, 62);
        assert_eq!(again[0], Command::Release(id(1)));
        assert!(matches!(again[1], Command::Start { steal: Some(v), .. } if v == id(1)));
        // And a note-off of channel 0's old key leaves channel 1 alone.
        assert!(s.off(0, 60).is_empty());
        assert_eq!(s.off(1, 64), [Command::Release(id(2))]);
    }

    /// A released tail is the channel's voice too: the next note cuts it.
    #[test]
    fn mono_takes_over_a_released_tail() {
        let mut s = Sim::new(Rules {
            mode: Mode::Mono,
            ..POLY
        });
        s.on(0, 60);
        s.off(0, 60);
        let c = s.on(0, 62);
        assert!(
            matches!(c[..], [Command::Start { steal: Some(v), .. }] if v == id(1)),
            "{c:?}"
        );
    }

    /// Legato is remembered for each channel.
    #[test]
    fn legato_is_for_each_channel() {
        let mut s = Sim::new(Rules {
            mode: Mode::Legato,
            ..POLY
        });
        s.on(0, 60);
        assert!(
            matches!(s.on(1, 64)[0], Command::Start { .. }),
            "channel 1 has no voice to move"
        );
        assert!(matches!(s.on(0, 62)[0], Command::Retarget { .. }));
    }
}
