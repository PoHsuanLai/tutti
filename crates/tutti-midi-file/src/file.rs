//! [`MidiFile`]: one file model for both containers, lossless, with a tempo
//! map.
//!
//! `.mid` is worn by a Standard MIDI File and a MIDI 2.0 Clip File alike, so
//! which decoder runs is decided by the leading bytes ([`MidiFileKind::detect`]),
//! never by the name. Both read into the same shape: [`Track`]s of
//! `(Tick, FileEvent)`, the file's division, and the [`TempoMap`] its tempo
//! events declare. Ticks are integers, so a file read and written holds the
//! same events with no rounding; [`MidiFile::events`] is where they become
//! [`Beat`]s.
//!
//! The file layer does not translate. An SMF channel message is a MIDI 1.0
//! [`Message::Midi1`] on group 0, and a MIDI 1.0 Note On at velocity 0 stays
//! one (it is a note-off by the MIDI 1.0 rule, which [`Track::notes`] applies);
//! a Clip File's packets are as stored. Translation runs where a clip is
//! loaded for playback. A file allocates and never reaches the audio thread.

use std::io::{Read, Write};
use std::ops::{Add, AddAssign, Sub};
use std::path::Path;
use std::sync::Arc;

use tutti_midi_protocol::channel_voice;
use tutti_tempo::{Shape, TempoMap, TempoPoint};
use tutti_types::{Beat, Bpm, Seconds};
use tutti_ump::{
    Address, KeyEvent, Message, MidiChannel, MidiGroup, NoteNumber, Sysex7Packets, U7, Ump,
    Unknown, Voice2,
};

use crate::MidiFileKind;
use crate::{ConfigError, Error, OutOfOrder, Refusal, Result, clip_file, flex, smf};

mod to_clip;

pub use to_clip::{Converted, DropReason, Dropped, ToClipError};

/// A position in a file, in ticks of its division: whole numbers, so a file
/// written back lands every event on the tick it was read at.
///
/// A tick is a count, and the whole of `u64` is in range. [`MidiFile::beat_at`]
/// is the conversion to a [`Beat`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Tick(pub u64);

impl Tick {
    /// `count` quarter notes in a file of division `ticks_per_quarter`.
    ///
    /// ```
    /// use tutti_midi_file::Tick;
    ///
    /// assert_eq!(Tick::quarters(4, 480), Tick(1_920));
    /// ```
    #[inline]
    #[must_use]
    pub const fn quarters(count: u64, ticks_per_quarter: u16) -> Self {
        Self(count * ticks_per_quarter as u64)
    }

    /// This tick in quarter notes: the tick over the division
    /// (`ticks_per_quarter`, [`MidiFile::ticks_per_quarter`]).
    ///
    /// ```
    /// use tutti_midi_file::Tick;
    /// use tutti_types::Beat;
    ///
    /// assert_eq!(Tick(720).beats(480), Beat(1.5));
    /// ```
    #[inline]
    #[must_use]
    pub fn beats(self, ticks_per_quarter: u16) -> Beat {
        Beat(self.0 as f64 / f64::from(ticks_per_quarter))
    }
}

/// `ticks` later. Overflows like `u64`'s `+`.
impl Add<u64> for Tick {
    type Output = Tick;

    #[inline]
    fn add(self, ticks: u64) -> Tick {
        Tick(self.0 + ticks)
    }
}

/// `ticks` earlier. Underflows like `u64`'s `-`.
impl Sub<u64> for Tick {
    type Output = Tick;

    #[inline]
    fn sub(self, ticks: u64) -> Tick {
        Tick(self.0 - ticks)
    }
}

impl AddAssign<u64> for Tick {
    #[inline]
    fn add_assign(&mut self, ticks: u64) {
        self.0 += ticks;
    }
}

/// The format word of an SMF header (SMF 1.0 section 2.1).
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SmfFormat {
    /// Format 0: one track holding everything.
    SingleTrack,
    /// Format 1: simultaneous tracks of one timeline, the first usually
    /// carrying the tempo map.
    Parallel,
    /// Format 2: independent sequences, each with its own timeline. Their
    /// tracks are not merged and share no tempo map.
    Sequential,
}

/// What a text meta event says (SMF 1.0 section 3.3 types `01` to `09`).
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextKind {
    /// `01`: any text.
    Text,
    /// `02`: a copyright notice.
    Copyright,
    /// `03`: the track (or, in a format 0 file or the first track of a format
    /// 1 file, the sequence) name.
    TrackName,
    /// `04`: the instrument a track is for.
    InstrumentName,
    /// `05`: a lyric syllable.
    Lyric,
    /// `06`: a marker, such as a rehearsal letter or a section name.
    Marker,
    /// `07`: a cue point.
    CuePoint,
    /// `08`: a program (patch) name.
    ProgramName,
    /// `09`: a device (port) name.
    DeviceName,
}

/// A meta event of a Standard MIDI File (SMF 1.0 section 3.3), minus the End
/// of Track, which is a [`Track`]'s `end`.
///
/// Text is kept as bytes: the specification does not say which encoding it is
/// in, and a file must write back what it read.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Meta {
    /// A text event.
    Text {
        /// Which of them.
        kind: TextKind,
        /// The text, as the file stores it.
        bytes: Box<[u8]>,
    },
    /// `51`: Set Tempo, microseconds per quarter note. The 24-bit wire value
    /// (a bare number: it is the file's own figure, and a zero, "no valid
    /// tempo", has to be kept to be written back). [`Meta::tempo`] and
    /// [`Meta::bpm`] cross to a [`Bpm`].
    Tempo {
        /// Microseconds per quarter note, at most `0xFF_FFFF`.
        micros_per_quarter: u32,
        /// The data past the three bytes this reads: empty unless the file
        /// has a longer event (SMF 1.0 section 3.3: programs "must ignore
        /// everything past what they know about"). Kept so the file writes
        /// back as it was.
        extra: Box<[u8]>,
    },
    /// `58`: Time Signature, as the wire states it.
    TimeSignature {
        /// The numerator.
        numerator: u8,
        /// The denominator as a power of two: 2 is a quarter, 3 an eighth.
        denominator_power: u8,
        /// MIDI clocks in a metronome click.
        clocks_per_click: u8,
        /// Notated 32nd notes in a MIDI quarter note (24 clocks): 8 usually.
        thirty_seconds_per_quarter: u8,
        /// The data past the four bytes this reads, as `extra` of
        /// [`Tempo`](Self::Tempo). A longer Time Signature is real: the
        /// compound-signature bytes "were added at the end so that the
        /// current Meta Event could be extended" (SMF 1.0 section 3.3).
        extra: Box<[u8]>,
    },
    /// `59`: Key Signature.
    KeySignature {
        /// Sharps if positive, flats if negative.
        sharps: i8,
        /// Whether the key is minor.
        minor: bool,
        /// The data past the two bytes this reads, as `extra` of
        /// [`Tempo`](Self::Tempo).
        extra: Box<[u8]>,
    },
    /// Any other meta event (sequence number `00`, channel prefix `20`, port
    /// `21`, SMPTE offset `54`, sequencer specific `7F`, an unregistered
    /// type): its type byte and its data.
    Other {
        /// The type byte.
        kind: u8,
        /// The data, without its length.
        data: Box<[u8]>,
    },
}

/// The slowest tempo the wire holds: 24 bits of microseconds per quarter.
const MAX_MICROS_PER_QUARTER: u32 = 0x00FF_FFFF;

/// Microseconds in a minute: the numerator of the tempo relation, a Set Tempo
/// being microseconds per quarter note.
const MICROS_PER_MINUTE: f64 = 60_000_000.0;

impl Meta {
    /// A Set Tempo for `bpm`, rounded to the wire's microsecond. The wire
    /// holds 1 to 16 777 215 us per quarter: a tempo above 60 million BPM
    /// writes the shortest, 1 (the wire's zero means "no valid tempo"), and
    /// one below about 3.6 BPM the longest.
    pub fn tempo(bpm: Bpm) -> Self {
        let micros = (MICROS_PER_MINUTE / bpm.get()).round() as u32;
        Self::Tempo {
            micros_per_quarter: micros.clamp(1, MAX_MICROS_PER_QUARTER),
            extra: Box::default(),
        }
    }

    /// The tempo of a Set Tempo, `None` for any other event and for a zero
    /// (no valid tempo).
    pub fn bpm(&self) -> Option<Bpm> {
        match self {
            Self::Tempo {
                micros_per_quarter, ..
            } if *micros_per_quarter != 0 => {
                Bpm::new(MICROS_PER_MINUTE / f64::from(*micros_per_quarter))
            }
            _ => None,
        }
    }

    /// A text event of `kind`.
    pub fn text(kind: TextKind, text: &str) -> Self {
        Self::Text {
            kind,
            bytes: text.as_bytes().into(),
        }
    }

    /// The text of a text event, `None` for any other event and for text that
    /// is not UTF-8 (the specification names no encoding, so the bytes are
    /// the file's: [`Meta::Text`]'s `bytes`).
    #[must_use]
    pub fn text_str(&self) -> Option<&str> {
        match self {
            Self::Text { bytes, .. } => std::str::from_utf8(bytes).ok(),
            _ => None,
        }
    }
}

/// One event of a track.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum FileEvent {
    /// A message. An SMF's channel message is [`Message::Midi1`] on group 0; a
    /// Clip File's packets are as stored.
    Message(Message),
    /// A Clip File packet no definition covers (a reserved message type, an
    /// undefined status), kept as its words so it writes back unchanged.
    Unknown(Unknown),
    /// An SMF `F0` System Exclusive event: the bytes after the `F0` and its
    /// length, normally ending in `F7`. Any length, unlike a packet.
    SysEx(Box<[u8]>),
    /// An SMF `F7` event: the continuation of a divided System Exclusive
    /// message, or an escape sequence of arbitrary bytes. Distinct from
    /// [`SysEx`](Self::SysEx) because the file distinguishes them.
    Escape(Box<[u8]>),
    /// An SMF meta event.
    Meta(Meta),
}

impl From<Message> for FileEvent {
    fn from(message: Message) -> Self {
        Self::Message(message)
    }
}

impl From<Meta> for FileEvent {
    fn from(meta: Meta) -> Self {
        Self::Meta(meta)
    }
}

impl FileEvent {
    /// The message, for an event that is one.
    pub fn message(&self) -> Option<&Message> {
        match self {
            Self::Message(m) => Some(m),
            _ => None,
        }
    }
}

/// A track: events in order, each at an absolute [`Tick`], and where the track
/// ends.
///
/// An SMF's track is one `MTrk` chunk and a Clip File's one sequence. Its name
/// is its Track Name meta event ([`name`](Self::name)), which stays where the
/// file put it.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Track {
    /// A Clip File's configuration header (M2-116 section 6): the messages
    /// between the file header and Start of Clip that configure a receiver
    /// (Set Profile On, Bank Select, Program Change, volume, System
    /// Exclusive, and at most one Set Tempo and one Set Time Signature), in
    /// order. They are *not* events of the sequence and do not sound at its
    /// start. Their ticks are the header's own clock, counted from the DCTPQ;
    /// the leading Set Profile On messages are at 0. Empty for an SMF.
    pub header: Vec<(Tick, FileEvent)>,
    /// Where the header ends, on the header's clock: the tick of Start of Clip,
    /// which is the last header message's tick plus the Delta Clockstamp before
    /// Start of Clip. M2-116 section 6.1: "the cumulative values of all Delta
    /// Clockstamp up to and including the Delta Clockstamp for the Start of
    /// Clip Message determines the total length of time allocated to the Clip
    /// Configuration Header", which a receiver may need to configure itself.
    /// Not before the last header message. `Tick(0)` for an SMF, and for a
    /// header that asks for no time after its last message.
    pub header_end: Tick,
    /// The events, in file order. Their ticks do not decrease.
    pub events: Vec<(Tick, FileEvent)>,
    /// Where the track ends: an SMF's End of Track, a Clip File's End of
    /// Clip. Not before the last event.
    pub end: Tick,
}

impl Track {
    /// An empty track, ending at tick 0. Events are added with the builder
    /// ([`note`](Self::note), [`tempo`](Self::tempo), [`meta`](Self::meta),
    /// [`push`](Self::push)), by [`collect`](Iterator::collect)ing or
    /// [`extend`](Extend::extend)ing `(Tick, FileEvent)` pairs, or by filling
    /// the public fields of a track a file layer reads.
    ///
    /// ```
    /// use tutti_midi_file::{Tick, Track};
    /// use tutti_types::Bpm;
    /// use tutti_ump::{MidiChannel, NoteNumber, U7};
    ///
    /// let track = Track::new()
    ///     .tempo(Tick(0), Bpm::of(90.0))
    ///     .note(Tick(0), 480, MidiChannel::FIRST, NoteNumber::of(60), U7::of(100))
    ///     .note(Tick(480), 480, MidiChannel::FIRST, NoteNumber::of(64), U7::of(100));
    /// assert_eq!(track.end, Tick(960));
    /// assert_eq!(track.events.len(), 5);
    /// ```
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Places `event` at `tick`, after the events already at that tick, and
    /// moves the track's end to `tick` if it is past it.
    fn place(&mut self, tick: Tick, event: FileEvent) {
        let at = self.events.partition_point(|&(t, _)| t <= tick);
        self.events.insert(at, (tick, event));
        self.end = self.end.max(tick);
    }

    /// A note: a Note On at `tick` and its Note Off `len` ticks on, on `channel`
    /// (a bare [`MidiChannel`] is in Group 1, as an SMF holds). The Note Off
    /// carries the MIDI release velocity of 64, "no release velocity".
    ///
    /// Events are placed in tick order, after the events already at their
    /// tick, so notes may be added in any order, overlap, or start together
    /// (a chord). At one tick the events are in the order they were added: a
    /// Note Off lands before the Note On of a note that starts where it ends
    /// only when its note was added first.
    #[must_use]
    pub fn note(
        mut self,
        tick: Tick,
        len: u64,
        channel: impl Into<Address>,
        note: NoteNumber,
        velocity: U7,
    ) -> Self {
        let address = channel.into();
        self.place(tick, Message::note_on(address, note, velocity).into());
        self.place(
            tick + len,
            Message::note_off(address, note, U7::of(64)).into(),
        );
        self
    }

    /// A Set Tempo of `bpm` at `tick` ([`Meta::tempo`]), placed in tick order
    /// as [`note`](Self::note) is.
    #[must_use]
    pub fn tempo(mut self, tick: Tick, bpm: Bpm) -> Self {
        self.place(tick, Meta::tempo(bpm).into());
        self
    }

    /// A meta event at `tick`, placed in tick order as [`note`](Self::note) is.
    #[must_use]
    pub fn meta(mut self, tick: Tick, meta: Meta) -> Self {
        self.place(tick, meta.into());
        self
    }

    /// Appends `event` at `tick`: a message ([`Message::note_on`] and its
    /// kin), a [`Meta`], or any [`FileEvent`]. Where [`note`](Self::note)
    /// places an event by its tick, this one is the strict door: the track's
    /// events do not go back in time. The track's end moves to `tick` if it is
    /// past it.
    ///
    /// # Errors
    ///
    /// [`OutOfOrder`] if `tick` is before the last event's, naming both; the
    /// event is not added.
    pub fn push(
        mut self,
        tick: Tick,
        event: impl Into<FileEvent>,
    ) -> std::result::Result<Self, OutOfOrder> {
        if let Some(&(last, _)) = self.events.last()
            && tick < last
        {
            return Err(OutOfOrder { last, at: tick });
        }
        self.events.push((tick, event.into()));
        self.end = self.end.max(tick);
        Ok(self)
    }

    /// The track's events as the packets a clip plays, at their beats for a
    /// file of division `ticks_per_quarter`, in the track's order. The rule is
    /// [`MidiFile::timeline`]'s.
    pub fn timeline(&self, ticks_per_quarter: u16) -> impl Iterator<Item = (Beat, Ump)> + '_ {
        self.events.iter().flat_map(move |(tick, event)| {
            let beat = tick.beats(ticks_per_quarter);
            packets(event).map(move |ump| (beat, ump))
        })
    }

    /// The track's name: its first Track Name event, if it is UTF-8 text.
    pub fn name(&self) -> Option<&str> {
        self.events.iter().find_map(|(_, event)| match event {
            FileEvent::Meta(Meta::Text {
                kind: TextKind::TrackName,
                bytes,
            }) => std::str::from_utf8(bytes).ok(),
            _ => None,
        })
    }

    /// The track's notes, a Note On paired with its Note Off, in order of
    /// onset and then `(group, channel, note)`.
    ///
    /// Pairing is keyed on `(group, channel, note)`: a Note Off only closes a
    /// Note On on the same group and channel, overlapping identical notes close
    /// last-on first-off, and a note still open at the track's end is dropped
    /// (its length is unknowable). A MIDI 1.0 Note On at velocity 0 is a Note
    /// Off, as the MIDI 1.0 specification says; a MIDI 2.0 one is neither a
    /// start nor an end.
    ///
    /// This walks the whole track, allocates the list and sorts it on every
    /// call: keep the result rather than calling it again.
    #[must_use]
    pub fn notes(&self) -> Vec<PairedNote> {
        use std::collections::BTreeMap;

        let mut held: BTreeMap<(MidiGroup, MidiChannel, NoteNumber), Vec<(Tick, u16)>> =
            BTreeMap::new();
        let mut out: Vec<PairedNote> = Vec::new();

        for (tick, event) in &self.events {
            let Some(message) = event.message() else {
                continue;
            };
            let ump = Ump::from(*message);
            // `key_event` reads the status, group, channel and note bits of a
            // MIDI 1.0 or 2.0 channel-voice packet and counts a MIDI 1.0 Note
            // On at velocity 0 as an off, as `Ump::is_note_off` does.
            let Some(key_event @ (KeyEvent::On(key) | KeyEvent::Off(key))) = ump.key_event() else {
                continue;
            };
            let Address { group, channel } = key.address();
            let note = key.note();
            let map_key = (group, channel, note);
            if matches!(key_event, KeyEvent::Off(_)) {
                if let Some((start, velocity)) = held.get_mut(&map_key).and_then(Vec::pop) {
                    out.push(PairedNote {
                        group,
                        channel,
                        note,
                        velocity,
                        start,
                        end: (*tick).max(start),
                    });
                }
            } else {
                // A MIDI 1.0 Note On widens (`channel_voice`), a MIDI 2.0 one
                // keeps its 16 bits; a MIDI 2.0 Note On at velocity 0 is
                // neither a start nor an end.
                if let Some((_, Voice2::NoteOn { velocity, .. })) = channel_voice(ump)
                    && velocity > 0
                {
                    held.entry(map_key).or_default().push((*tick, velocity));
                }
            }
        }

        // Onset order, then key, so simultaneous notes have a stable order
        // rather than one that depends on when their Note Offs arrived.
        out.sort_by_key(|n| (n.start, n.group, n.channel, n.note));
        out
    }
}

/// A track of the events, in tick order: a stable sort, so events at one tick
/// stay in the order given. Ends at the last of them.
impl FromIterator<(Tick, FileEvent)> for Track {
    fn from_iter<I: IntoIterator<Item = (Tick, FileEvent)>>(events: I) -> Self {
        let mut track = Track::new();
        track.extend(events);
        track
    }
}

/// Adds the events, then puts the track in tick order (a stable sort, so
/// events at one tick stay in the order added) and moves its end to the last
/// event's if that is past it.
impl Extend<(Tick, FileEvent)> for Track {
    fn extend<I: IntoIterator<Item = (Tick, FileEvent)>>(&mut self, events: I) {
        self.events.extend(events);
        self.events.sort_by_key(|&(tick, _)| tick);
        if let Some(&(last, _)) = self.events.last() {
            self.end = self.end.max(last);
        }
    }
}

/// A note paired from its Note On and Note Off ([`Track::notes`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PairedNote {
    /// The UMP group (0 for an SMF). Part of the pairing key: groups are
    /// independent 16-channel spaces.
    pub group: MidiGroup,
    /// The channel.
    pub channel: MidiChannel,
    /// The key.
    pub note: NoteNumber,
    /// The Note On velocity at MIDI 2.0 width: a MIDI 1.0 velocity is widened
    /// by the specification's scaling, so this is lossless in both directions.
    /// A bare `u16` because 16 bits is the whole raw of a MIDI 2.0 velocity.
    pub velocity: u16,
    /// Where the note starts.
    pub start: Tick,
    /// Where it ends; not before `start`.
    pub end: Tick,
}

/// The container a file was read from, and with it the rules it is written by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Container {
    Smf(SmfFormat),
    Clip,
}

/// A MIDI file of either container, read losslessly.
///
/// Fields are private and the tempo map is derived from the tracks (their
/// Set Tempo events), so the two never disagree; [`tracks`](Self::tracks) and
/// the accessors read it, and [`smf`](Self::smf) and [`clip`](Self::clip)
/// build one and check what a container can hold.
///
/// Format 2 SMFs keep their sequences apart: [`events`](Self::events) merges
/// the tracks of a format 0 or 1 file and of a Clip File, which share one
/// timeline, and has none for a format 2 file, whose tracks are independent
/// ([`tempo_map_of`](Self::tempo_map_of) is each one's own).
///
/// ```
/// use tutti_midi_file::{MidiFile, SmfFormat, Tick, Track};
/// use tutti_types::{Beat, Bpm, Seconds};
/// use tutti_ump::{MidiChannel, NoteNumber, U7};
///
/// let track = Track::new()
///     .tempo(Tick(0), Bpm::of(120.0))
///     .note(Tick(0), 480, MidiChannel::FIRST, NoteNumber::of(60), U7::of(100));
/// let file = MidiFile::smf(SmfFormat::SingleTrack, 480, vec![track])?;
///
/// let again = MidiFile::from_bytes(&file.to_bytes()?)?;
/// assert_eq!(again, file);
/// assert_eq!(again.seconds_at(Beat(1.0)), Seconds(0.5));
/// # Ok::<(), tutti_midi_file::Error>(())
/// ```
#[derive(Clone, Debug)]
pub struct MidiFile {
    container: Container,
    ticks_per_quarter: u16,
    tracks: Vec<Track>,
    /// One map for a file of one timeline, one per track for a format 2 file.
    tempo_maps: Vec<Arc<TempoMap>>,
}

/// Two files are equal when their container, division and tracks are; the
/// tempo maps are derived from the tracks.
impl PartialEq for MidiFile {
    fn eq(&self, other: &Self) -> bool {
        self.container == other.container
            && self.ticks_per_quarter == other.ticks_per_quarter
            && self.tracks == other.tracks
    }
}

impl Eq for MidiFile {}

impl MidiFile {
    /// The extensions a MIDI file of either container is found under.
    ///
    /// `.mid` and `.midi` are the Standard MIDI File's; `.mid2` and `.midi2`
    /// are in use for Clip Files. A reader handles all four the same way,
    /// detecting rather than trusting any of them.
    pub const EXTENSIONS: &'static [&'static str] = &["mid", "midi", "mid2", "midi2"];

    /// An SMF of `format` and division `ticks_per_quarter` over `tracks`.
    ///
    /// # Errors
    ///
    /// [`Error::Config`] if there is no track, a format 0 file does not have
    /// exactly one, the division is zero or over 15 bits, a track's events
    /// are out of order or past its end, two events are further apart than
    /// the wire's 28-bit delta, a message is not a group 0 MIDI 1.0 channel
    /// message, a Set Tempo is over 24 bits (the [`ConfigError`] names which
    /// and where); [`Error::Tempo`] if the tempo events do not make a
    /// [`TempoMap`].
    #[must_use = "an Err means no file was built"]
    pub fn smf(format: SmfFormat, ticks_per_quarter: u16, tracks: Vec<Track>) -> Result<Self> {
        Self::build(Container::Smf(format), ticks_per_quarter, tracks)
    }

    /// A Clip File of division `ticks_per_quarter` over one sequence and its
    /// [`header`](Track::header).
    ///
    /// # Errors
    ///
    /// [`Error::Config`] if the division is zero, the events or the
    /// header's are out of order or the events past the track's end, the header
    /// ends before its last message, two are further apart than a `u32`, or an
    /// event is not a packet (`SysEx`, `Escape` and `Meta` are SMF's), is a
    /// MIDI-CI Property Exchange message (sections 6.4 and 7.2) or is the
    /// file's own structure (a Delta Clockstamp, the DCTPQ, a Null, Start or End
    /// of Clip);
    /// [`Error::Clip`] if the header breaks a rule of sections 6.1 to 6.1.2
    /// (a second Set Tempo or Set Time Signature, one misplaced or not at delta
    /// 0);
    /// [`Error::Tempo`] if the Set Tempo messages do not make a
    /// [`TempoMap`].
    pub fn clip(ticks_per_quarter: u16, track: Track) -> Result<Self> {
        Self::build(Container::Clip, ticks_per_quarter, vec![track])
    }

    fn build(container: Container, ticks_per_quarter: u16, tracks: Vec<Track>) -> Result<Self> {
        let invalid = |why: ConfigError| Err(Error::Config(why));
        if ticks_per_quarter == 0
            || (matches!(container, Container::Smf(_)) && ticks_per_quarter > 0x7FFF)
        {
            return invalid(ConfigError::Division { ticks_per_quarter });
        }
        if tracks.is_empty() {
            return invalid(ConfigError::NoTracks);
        }
        if matches!(container, Container::Smf(SmfFormat::SingleTrack)) && tracks.len() != 1 {
            return invalid(ConfigError::SingleTrack {
                tracks: tracks.len(),
            });
        }
        if tracks.len() > usize::from(u16::MAX) {
            return invalid(ConfigError::TooManyTracks {
                tracks: tracks.len(),
            });
        }
        // The widest gap the wire holds: 28 bits in an SMF, 32 in a Clip File.
        let max_gap = match container {
            Container::Smf(_) => 0x0FFF_FFFF,
            Container::Clip => u64::from(u32::MAX),
        };
        for (i, track) in tracks.iter().enumerate() {
            if !track.header.is_empty() || track.header_end != Tick(0) {
                if container != Container::Clip {
                    return invalid(ConfigError::HeaderInSmf { track: i });
                }
                let mut at = Tick(0);
                let mut packets = Vec::with_capacity(track.header.len());
                for (tick, event) in &track.header {
                    if *tick < at {
                        return invalid(ConfigError::HeaderOutOfOrder {
                            track: i,
                            at: *tick,
                        });
                    }
                    if tick.0 - at.0 > max_gap {
                        return invalid(ConfigError::HeaderGap {
                            track: i,
                            at: *tick,
                        });
                    }
                    at = *tick;
                    if let Some(refusal) = refuses(container, event) {
                        return invalid(ConfigError::Refused {
                            track: i,
                            in_header: true,
                            refusal,
                        });
                    }
                    match event {
                        FileEvent::Message(m) => packets.push((tick.0, Ump::from(*m))),
                        FileEvent::Unknown(u) => packets.push((tick.0, u.0)),
                        _ => {}
                    }
                }
                clip_file::check_header(&packets)?;
                if track.header_end < at {
                    return invalid(ConfigError::HeaderEndBeforeLast {
                        track: i,
                        end: track.header_end,
                    });
                }
                if track.header_end.0 - at.0 > max_gap {
                    return invalid(ConfigError::HeaderEndTooFar {
                        track: i,
                        end: track.header_end,
                    });
                }
            }
            let mut at = Tick(0);
            for (tick, event) in &track.events {
                if *tick < at {
                    return invalid(ConfigError::OutOfOrder {
                        track: i,
                        at: *tick,
                    });
                }
                if tick.0 - at.0 > max_gap {
                    return invalid(ConfigError::Gap {
                        track: i,
                        at: *tick,
                    });
                }
                at = *tick;
                if let Some(refusal) = refuses(container, event) {
                    return invalid(ConfigError::Refused {
                        track: i,
                        in_header: false,
                        refusal,
                    });
                }
            }
            if track.end < at {
                return invalid(ConfigError::EndBeforeLast {
                    track: i,
                    end: track.end,
                });
            }
            if track.end.0 - at.0 > max_gap {
                return invalid(ConfigError::EndTooFar {
                    track: i,
                    end: track.end,
                });
            }
        }
        let tempo_maps = match container {
            Container::Smf(SmfFormat::Sequential) => tracks
                .iter()
                .map(|t| tempo_map(container, ticks_per_quarter, std::slice::from_ref(t)))
                .collect::<Result<_>>()?,
            _ => vec![tempo_map(container, ticks_per_quarter, &tracks)?],
        };
        Ok(Self {
            container,
            ticks_per_quarter,
            tracks,
            tempo_maps,
        })
    }

    /// Decodes a complete MIDI file from its bytes, detecting the container
    /// from its magic: `MThd` for an SMF, `SMF2CLIP` for a Clip File.
    ///
    /// # Errors
    ///
    /// - [`Error::UnknownFormat`] when the magic matches neither container (no
    ///   decoder runs).
    /// - [`Error::Truncated`] when an SMF is cut off: a chunk that runs past
    ///   the end, or fewer tracks than the header declares.
    /// - [`Error::MidiUnsupportedTiming`] for an SMF in SMPTE division. A
    ///   timecode file has no beat grid to put a tempo map on, and importing
    ///   one means choosing a tempo, which is the caller's to say.
    /// - [`Error::MidiFileParse`] for any other malformed SMF, a zero division
    ///   included.
    /// - [`Error::Clip`] for a malformed Clip File, naming the rule it broke.
    /// - [`Error::Tempo`] if the tempo events do not make a [`TempoMap`].
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        match MidiFileKind::detect(bytes) {
            Some(MidiFileKind::StandardMidiFile) => {
                let parts = smf::read(bytes)?;
                Self::build(
                    Container::Smf(parts.format),
                    parts.ticks_per_quarter,
                    parts.tracks,
                )
            }
            Some(MidiFileKind::ClipFile) => {
                let clip = clip_file::parse(bytes)?;
                let events = |packets: Vec<(u64, Ump)>| -> Vec<(Tick, FileEvent)> {
                    packets
                        .into_iter()
                        .map(|(tick, ump)| {
                            let event = match ump.decode() {
                                Ok(message) => FileEvent::Message(message),
                                Err(unknown) => FileEvent::Unknown(unknown),
                            };
                            (Tick(tick), event)
                        })
                        .collect()
                };
                let track = Track {
                    header: events(clip.header),
                    events: events(clip.events),
                    header_end: Tick(clip.header_end),
                    end: Tick(clip.end),
                };
                Self::build(Container::Clip, clip.ticks_per_quarter, vec![track])
            }
            // Its own error rather than a parse failure: "this is not a MIDI
            // file" is a different thing to fix than "this MIDI file is
            // malformed".
            None => Err(Error::UnknownFormat),
        }
    }

    /// Reads and decodes the MIDI file at `path`. See [`from_bytes`](Self::from_bytes).
    ///
    /// # Errors
    ///
    /// [`Error::Io`] if the file cannot be read; otherwise as
    /// [`from_bytes`](Self::from_bytes).
    pub fn read(path: impl AsRef<Path>) -> Result<Self> {
        Self::from_bytes(&std::fs::read(path.as_ref())?)
    }

    /// The file as bytes, in its own container: every event written back at
    /// its tick. Running status and the spelling of a Delta Clockstamp may
    /// differ from the file it was read from; a read of the result is equal to
    /// this file.
    ///
    /// # Errors
    ///
    /// [`ConfigError::TooLarge`] (as [`Error::Config`]) if the encoder
    /// refuses (an SMF past 65535 tracks or an event of 256 MB).
    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        match self.container {
            Container::Smf(format) => smf::write(format, self.ticks_per_quarter, &self.tracks),
            Container::Clip => {
                let track = &self.tracks[0];
                let packets = |events: &[(Tick, FileEvent)]| -> Vec<(u64, Ump)> {
                    events
                        .iter()
                        .filter_map(|(tick, event)| match event {
                            FileEvent::Message(m) => Some((tick.0, Ump::from(*m))),
                            FileEvent::Unknown(u) => Some((tick.0, u.0)),
                            // Refused at construction: not a packet.
                            _ => None,
                        })
                        .collect()
                };
                Ok(clip_file::write(
                    self.ticks_per_quarter,
                    &packets(&track.header),
                    track.header_end.0,
                    &packets(&track.events),
                    track.end.0,
                ))
            }
        }
    }

    /// Reads and decodes a MIDI file from `reader`, to its end. See
    /// [`from_bytes`](Self::from_bytes).
    ///
    /// # Errors
    ///
    /// [`Error::Io`] if the reader fails; otherwise as
    /// [`from_bytes`](Self::from_bytes).
    pub fn read_from(mut reader: impl Read) -> Result<Self> {
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes)?;
        Self::from_bytes(&bytes)
    }

    /// Writes [`to_bytes`](Self::to_bytes) to `writer`.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] if the write fails; otherwise as
    /// [`to_bytes`](Self::to_bytes).
    pub fn write_to(&self, mut writer: impl Write) -> Result<()> {
        writer.write_all(&self.to_bytes()?)?;
        Ok(())
    }

    /// Writes [`to_bytes`](Self::to_bytes) to `path`.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] if the write fails; otherwise as [`to_bytes`](Self::to_bytes).
    pub fn write(&self, path: impl AsRef<Path>) -> Result<()> {
        std::fs::write(path, self.to_bytes()?)?;
        Ok(())
    }

    /// Which container this file is.
    pub fn kind(&self) -> MidiFileKind {
        match self.container {
            Container::Smf(_) => MidiFileKind::StandardMidiFile,
            Container::Clip => MidiFileKind::ClipFile,
        }
    }

    /// An SMF's format, `None` for a Clip File.
    pub fn smf_format(&self) -> Option<SmfFormat> {
        match self.container {
            Container::Smf(format) => Some(format),
            Container::Clip => None,
        }
    }

    /// The division: ticks per quarter note.
    pub fn ticks_per_quarter(&self) -> u16 {
        self.ticks_per_quarter
    }

    /// The tracks, as the file has them (a Clip File's is one).
    pub fn tracks(&self) -> &[Track] {
        &self.tracks
    }

    /// The tempo map of the file's timeline, built from its Set Tempo events
    /// (an SMF's meta events across all tracks, a Clip File's Flex Data Set
    /// Tempo messages), steps at the tick each lands on. A file that sets none
    /// at the start plays at 120 BPM until the first, the MIDI default. Two
    /// at one tick: the later governs, as a sequencer applying them in order
    /// would.
    ///
    /// For a format 2 file, which has a timeline per sequence, this is the
    /// first sequence's; [`tempo_map_of`](Self::tempo_map_of) names one.
    ///
    /// It is shared: `file.tempo_map().clone()` is a reference count, and
    /// `TransportSettings::set_tempo_map` (`tutti-transport`) takes it as it
    /// is.
    pub fn tempo_map(&self) -> &Arc<TempoMap> {
        &self.tempo_maps[0]
    }

    /// The tempo map `track` is read on: the file's for a file of one timeline,
    /// the track's own for a format 2 file. `None` past the last track.
    pub fn tempo_map_of(&self, track: usize) -> Option<&Arc<TempoMap>> {
        if track >= self.tracks.len() {
            return None;
        }
        Some(self.tempo_maps.get(track).unwrap_or(&self.tempo_maps[0]))
    }

    /// `tick` in quarter notes: the tick over the division.
    pub fn beat_at(&self, tick: Tick) -> Beat {
        tick.beats(self.ticks_per_quarter)
    }

    /// Seconds from beat 0 to `at` over the [`tempo_map`](Self::tempo_map),
    /// each stretch between two changes at its own tempo (a ramp at its own
    /// slope: a file's changes are all steps). What an offline render of the
    /// file lasts: `seconds_at(end_beat())` is the span a bounce of the file renders (`Bounce::until(end_beat())`).
    pub fn seconds_at(&self, at: Beat) -> Seconds {
        self.tempo_map().seconds_at(Beat(at.get().max(0.0)))
    }

    /// Where the file ends: its longest track's end (an SMF's End of Track, a
    /// Clip File's End of Clip), at least where the last event lands.
    pub fn end_beat(&self) -> Beat {
        let end = self.tracks.iter().map(|t| t.end).max().unwrap_or_default();
        self.beat_at(end)
    }

    /// Every event of the file's one timeline (a Clip File's
    /// [`header`](Track::header) is not on it) as `(position, event)`, in
    /// order: the tracks merged by tick, ties in track order and then in the
    /// track's own order. `None` for a format 2 file, whose sequences are
    /// independent and have no common timeline to merge on: read each
    /// [`Track`] with [`beat_at`](Self::beat_at).
    pub fn events(&self) -> Option<impl Iterator<Item = (Beat, &FileEvent)>> {
        if self.container == Container::Smf(SmfFormat::Sequential) {
            return None;
        }
        let mut all: Vec<(Tick, &FileEvent)> = self
            .tracks
            .iter()
            .flat_map(|t| t.events.iter().map(|(tick, event)| (*tick, event)))
            .collect();
        // Stable: equal ticks keep track order, then the track's own.
        all.sort_by_key(|&(tick, _)| tick);
        Some(
            all.into_iter()
                .map(move |(tick, event)| (self.beat_at(tick), event)),
        )
    }
}

impl MidiFile {
    /// The file's one timeline as the packets a clip plays, `(Beat, Ump)` in
    /// order ([`events`](Self::events)' order), for
    /// `Midi::play_clip(target, file.timeline()?)`. `None` for a format 2
    /// file, which has no one timeline: read each [`Track`] with
    /// [`Track::timeline`].
    ///
    /// The rule, one event at a time:
    ///
    /// - A [`Message`](FileEvent::Message) passes as its packet, as it is: an
    ///   SMF's channel message is MIDI 1.0 on Group 1, and a Clip File's
    ///   packets (Flex Data included) are as stored.
    /// - A [`SysEx`](FileEvent::SysEx) goes out as its SysEx7 packets, all at
    ///   the event's beat. A divided message (an `F0` event not ending in `F7`,
    ///   continued by [`Escape`](FileEvent::Escape) events) is not
    ///   reassembled and is dropped, as is one with a byte of 128 or more.
    /// - [`Meta`](FileEvent::Meta) events are not messages: the tempo is the
    ///   file's [`tempo_map`](Self::tempo_map), and the rest (text, time and
    ///   key signature, ...) are dropped. So are an
    ///   [`Escape`](FileEvent::Escape) and an [`Unknown`](FileEvent::Unknown)
    ///   packet.
    ///
    /// The Clip File's [`header`](Track::header) is not on the timeline.
    ///
    /// ```
    /// use tutti_midi_file::{MidiFile, SmfFormat, Tick, Track};
    /// use tutti_types::Beat;
    /// use tutti_ump::{MidiChannel, NoteNumber, U7, Ump};
    ///
    /// let track = Track::new().note(Tick(480), 480, MidiChannel::FIRST, NoteNumber::of(60), U7::of(100));
    /// let file = MidiFile::smf(SmfFormat::SingleTrack, 480, vec![track])?;
    /// let clip: Vec<(Beat, Ump)> = file.timeline().unwrap().collect();
    /// assert_eq!(clip.len(), 2);
    /// assert_eq!(clip[0].0, Beat(1.0));
    /// assert_eq!(clip[1].0, Beat(2.0));
    /// # Ok::<(), tutti_midi_file::Error>(())
    /// ```
    pub fn timeline(&self) -> Option<impl Iterator<Item = (Beat, Ump)> + '_> {
        let events = self.events()?;
        Some(events.flat_map(|(beat, event)| packets(event).map(move |ump| (beat, ump))))
    }
}

/// The packets of one event, by [`MidiFile::timeline`]'s rule.
enum EventPackets<'a> {
    None,
    One(Option<Ump>),
    Sysex(Sysex7Packets<'a>),
}

impl Iterator for EventPackets<'_> {
    type Item = Ump;

    fn next(&mut self) -> Option<Ump> {
        match self {
            Self::None => None,
            Self::One(ump) => ump.take(),
            Self::Sysex(packets) => packets.next(),
        }
    }
}

fn packets(event: &FileEvent) -> EventPackets<'_> {
    match event {
        FileEvent::Message(message) => EventPackets::One(Some(Ump::from(*message))),
        FileEvent::SysEx(data) => match data.split_last() {
            Some((0xF7, payload)) => Sysex7Packets::checked(MidiGroup::FIRST, payload)
                .map_or(EventPackets::None, EventPackets::Sysex),
            _ => EventPackets::None,
        },
        FileEvent::Unknown(_) | FileEvent::Escape(_) | FileEvent::Meta(_) => EventPackets::None,
    }
}

impl TryFrom<&[u8]> for MidiFile {
    type Error = Error;

    /// [`MidiFile::from_bytes`].
    fn try_from(bytes: &[u8]) -> Result<Self> {
        Self::from_bytes(bytes)
    }
}

/// Why `container` cannot hold `event`, `None` if it can.
fn refuses(container: Container, event: &FileEvent) -> Option<Refusal> {
    match (container, event) {
        (Container::Smf(_), FileEvent::Message(Message::Midi1(address, _))) => {
            (address.group != MidiGroup::FIRST).then_some(Refusal::SmfGroup)
        }
        (Container::Smf(_), FileEvent::Message(_) | FileEvent::Unknown(_)) => {
            Some(Refusal::SmfPacket)
        }
        (
            Container::Smf(_),
            FileEvent::Meta(Meta::Tempo {
                micros_per_quarter, ..
            }),
        ) if *micros_per_quarter > MAX_MICROS_PER_QUARTER => Some(Refusal::TempoOver24Bits {
            micros_per_quarter: *micros_per_quarter,
        }),
        (Container::Smf(_), _) => None,
        (Container::Clip, FileEvent::SysEx(_) | FileEvent::Escape(_) | FileEvent::Meta(_)) => {
            Some(Refusal::ClipSmfEvent)
        }
        (Container::Clip, FileEvent::Message(Message::Utility(_))) => Some(Refusal::ClipUtility),
        (
            Container::Clip,
            FileEvent::Message(Message::Stream(
                tutti_ump::Stream::StartOfClip | tutti_ump::Stream::EndOfClip,
            )),
        ) => Some(Refusal::ClipStructure),
        (Container::Clip, FileEvent::Message(message)) if clip_file::property_exchange(message) => {
            Some(Refusal::ClipPropertyExchange)
        }
        (Container::Clip, _) => None,
    }
}

/// The tempo map of one timeline: `tracks`' Set Tempo events as steps.
fn tempo_map(
    container: Container,
    ticks_per_quarter: u16,
    tracks: &[Track],
) -> Result<Arc<TempoMap>> {
    // The MIDI default, which a file that sets none at its start plays at.
    let default = Bpm::of(120.0);
    let mut changes: Vec<(Tick, Bpm)> = Vec::new();
    for track in tracks {
        // A Clip File's header Set Tempo counts from the start of the
        // sequence: the header runs before it, so its tempo is the one at tick
        // 0 (a Set Tempo at tick 0 of the sequence, later in the stream,
        // overrides it).
        for (_, event) in &track.header {
            if let (Container::Clip, FileEvent::Message(message)) = (container, event)
                && let Some(bpm) = flex::tempo(message)
            {
                changes.push((Tick(0), bpm));
            }
        }
        for (tick, event) in &track.events {
            let bpm = match (container, event) {
                (Container::Smf(_), FileEvent::Meta(meta)) => meta.bpm(),
                (Container::Clip, FileEvent::Message(message)) => flex::tempo(message),
                _ => None,
            };
            if let Some(bpm) = bpm {
                changes.push((*tick, bpm));
            }
        }
    }
    // Stable: two tracks setting a tempo on one tick keep track order, and the
    // later one governs.
    changes.sort_by_key(|&(tick, _)| tick);
    let mut points: Vec<(Tick, Bpm)> = Vec::with_capacity(changes.len() + 1);
    if changes.first().is_none_or(|&(tick, _)| tick != Tick(0)) {
        points.push((Tick(0), default));
    }
    for change in changes {
        match points.last_mut() {
            Some(last) if last.0 == change.0 => *last = change,
            _ => points.push(change),
        }
    }
    Ok(Arc::new(TempoMap::new(points.into_iter().map(
        |(tick, tempo)| TempoPoint {
            beat: tick.beats(ticks_per_quarter),
            tempo,
            shape: Shape::Step,
        },
    ))?))
}
