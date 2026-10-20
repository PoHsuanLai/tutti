//! [`MidiFile`]: a MIDI file of either container, sniffed by magic, read as
//! one timed event stream.
//!
//! `.mid` is worn by a Standard MIDI File and a MIDI 2.0 Clip File alike, so
//! which decoder runs is decided by the leading bytes ([`MidiFileKind::sniff`]),
//! never by the name. The two containers keep their own shapes (an SMF's named
//! tracks with 7-bit velocities, a Clip File's one stream with 16-bit ones);
//! [`MidiFile::timed`] is where they meet, as the engine's MIDI 2.0
//! [`MidiEvent`]s in [`Beat`]s.

use std::path::Path;

use midly::live::LiveEvent;
use midly::Smf;
use tutti_core::{Beat, Bpm};
use tutti_midi_types::ump::flex_tempo_bpm;
use tutti_midi_types::{normalize, read_clip_file, MidiEvent, ParsedClipFile};

use crate::smf::{tracks_of, ParsedMidiFile, SmfTrack};
use crate::{Error, MidiFileKind, Result};

/// A decoded MIDI file, in whichever of the two containers it turned out to be.
///
/// Two variants because the formats differ in what they hold: an SMF is a
/// list of named tracks with 7-bit velocities, a Clip File is one stream that
/// keeps 16 bits. Collapsing them here would discard whichever half the other
/// lacks; [`timed`](Self::timed) is the one view both have.
///
/// ```
/// use tutti_core::Beat;
/// use tutti_midi_file::{encode_midi_file, MidiFile, MidiWriteConfig, SmfMessage, SmfTimedEvent};
///
/// let bytes = encode_midi_file(
///     &[vec![SmfTimedEvent {
///         time_beats: Beat(2.0),
///         channel: 0,
///         msg: SmfMessage::NoteOn { key: 60.into(), vel: 127.into() },
///     }]],
///     &MidiWriteConfig::default(),
/// )?;
///
/// let file = MidiFile::from_bytes(&bytes)?;
/// let (beat, note) = file.timed().next().expect("one note");
/// assert_eq!(beat, Beat(2.0));
/// // Widened to MIDI 2.0: full-scale 7-bit velocity is full-scale 16-bit.
/// assert_eq!(note.velocity_u16(), Some(0xFFFF));
/// assert_eq!(file.end(), Beat(2.0));
/// # Ok::<(), tutti_midi_file::Error>(())
/// ```
#[derive(Debug, Clone)]
pub enum MidiFile {
    /// A Standard MIDI File.
    Smf(SmfFile),
    /// A MIDI 2.0 Clip File: one stream, keeping 16-bit velocities.
    Clip(Box<ParsedClipFile>),
}

/// A Standard MIDI File read two ways from one parse: its tracks with notes
/// paired, and the merged event stream [`MidiFile::timed`] reads.
///
/// Both halves come from the same `midly` parse and cannot be set apart, so
/// they never disagree: the fields are private, read through
/// [`tracks`](Self::tracks) and [`stream`](Self::stream).
#[derive(Debug, Clone)]
pub struct SmfFile {
    tracks: Vec<SmfTrack>,
    stream: ParsedMidiFile,
}

impl SmfFile {
    /// Parses SMF bytes once into both views.
    fn parse(bytes: &[u8]) -> Result<Self> {
        let smf = Smf::parse(bytes)?;
        Ok(Self {
            tracks: tracks_of(&smf)?,
            stream: ParsedMidiFile::from_smf(&smf)?,
        })
    }

    /// The tracks, notes paired, as [`smf::tracks`](crate::smf::tracks)
    /// reads them.
    pub fn tracks(&self) -> &[SmfTrack] {
        &self.tracks
    }

    /// The file as one merged event stream: every channel-voice message, its
    /// tempo at beat 0 and its tempo changes. The paired notes alone would
    /// drop every controller, bend and program change.
    pub fn stream(&self) -> &ParsedMidiFile {
        &self.stream
    }
}

impl MidiFile {
    /// The extensions a MIDI file of either container is found under.
    ///
    /// `.mid` and `.midi` are the Standard MIDI File's; `.mid2` and `.midi2`
    /// are in use for Clip Files. A reader handles all four the same way,
    /// sniffing rather than trusting any of them.
    pub const EXTENSIONS: &'static [&'static str] = &["mid", "midi", "mid2", "midi2"];

    /// Decodes a complete MIDI file from its bytes, sniffing the container
    /// from its magic: `MThd` for an SMF, `SMF2CLIP` for a Clip File.
    ///
    /// # Errors
    ///
    /// [`Error::UnknownFormat`] when the magic matches neither container (no
    /// decoder runs). Otherwise the matching decoder's error: an SMF's as
    /// [`smf::tracks`](crate::smf::tracks) returns it, a Clip File's as
    /// [`Error::Clip`].
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        match MidiFileKind::sniff(bytes) {
            Some(MidiFileKind::StandardMidiFile) => Ok(Self::Smf(SmfFile::parse(bytes)?)),
            Some(MidiFileKind::ClipFile) => Ok(Self::Clip(Box::new(read_clip_file(bytes)?))),
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

    /// Which container this file is.
    pub fn kind(&self) -> MidiFileKind {
        match self {
            Self::Smf(_) => MidiFileKind::StandardMidiFile,
            Self::Clip(_) => MidiFileKind::ClipFile,
        }
    }

    /// Every event as `(position, event)`, positions in quarter notes from the
    /// start of the file, in file order (an SMF's tracks merged by position,
    /// ties kept in track order).
    ///
    /// Each event is [`normalize`]d to MIDI 2.0 Channel Voice, so the two
    /// containers speak one vocabulary: an SMF's 7-bit values are widened by
    /// the spec's Min-Center-Max scaler, and a velocity-0 note-on is a
    /// note-off. An SMF and a Clip File of the same channel-voice messages
    /// therefore yield the same list.
    ///
    /// What each container contributes differs past that: an SMF its
    /// channel-voice messages only (its meta events are not MIDI messages: the
    /// tempo is [`tempo`](Self::tempo) and [`tempo_map`](Self::tempo_map), a
    /// track's name is in its tracks); a Clip File every UMP in its sequence,
    /// Flex Data (a Set Tempo, a time signature) included. The tempo map is not
    /// applied.
    pub fn timed(&self) -> Box<dyn Iterator<Item = (Beat, MidiEvent)> + '_> {
        match self {
            Self::Smf(smf) => {
                // Reused across events: midly writes the message's wire bytes
                // here, and `from_midi1_bytes` reads them back as a UMP.
                let mut wire = Vec::with_capacity(3);
                Box::new(smf.stream.events.iter().filter_map(move |e| {
                    wire.clear();
                    LiveEvent::Midi {
                        channel: e.channel.into(),
                        message: e.msg,
                    }
                    .write(&mut wire)
                    .ok()?;
                    let event = MidiEvent::from_midi1_bytes(0, &wire)?;
                    Some((e.time_beats, normalize(&event)))
                }))
            }
            Self::Clip(clip) => Box::new(clip.timed().map(|(beat, e)| (beat, normalize(&e)))),
        }
    }

    /// The tempo at beat 0, `None` if the file does not say.
    ///
    /// An SMF always says: a Set Tempo at tick 0 (the first in track order if
    /// several tracks set one there), otherwise the 120 BPM its specification
    /// assumes until a Set Tempo changes it. A Set Tempo later in the file does
    /// not govern the beats before it. A Clip File says only with a Flex Data
    /// Set Tempo at beat 0; adopting a default for one without is the caller's
    /// decision ([`ParsedClipFile::tempo_bpm`] says why).
    ///
    /// A file whose tempo changes needs [`tempo_map`](Self::tempo_map): this
    /// is the first tempo, not the file's.
    pub fn tempo(&self) -> Option<Bpm> {
        match self {
            Self::Smf(smf) => Some(smf.stream.tempo_bpm),
            Self::Clip(_) => self
                .tempo_map()
                .first()
                .filter(|&&(at, _)| at == Beat(0.0))
                .map(|&(_, tempo)| tempo),
        }
    }

    /// Every tempo change the file declares as `(position, tempo)`, in time
    /// order: an SMF's Set Tempo meta events across all tracks (ties in track
    /// order), a Clip File's Flex Data Set Tempo messages. Empty for a file
    /// that declares none; an SMF then plays at 120 BPM throughout.
    pub fn tempo_map(&self) -> Vec<(Beat, Bpm)> {
        match self {
            Self::Smf(smf) => smf.stream.tempo_changes.clone(),
            Self::Clip(clip) => clip
                .timed()
                .filter_map(|(at, e)| flex_tempo_bpm(&e).map(|bpm| (at, Bpm(bpm))))
                .collect(),
        }
    }

    /// Seconds from beat 0 to `at`, integrated over the
    /// [`tempo_map`](Self::tempo_map): each stretch between two changes at
    /// its own tempo. Before the first change the file plays at
    /// [`tempo`](Self::tempo), or the 120 BPM an SMF's specification assumes
    /// when it declares none (a Clip File without a tempo at beat 0 gets the
    /// same). What an offline render of the file lasts:
    /// `seconds_at(end())` is `RenderConfig::duration_seconds`.
    ///
    /// `f64`, not `Seconds`: `Seconds` is `f32`, which cannot carry an
    /// hour-long file to the frame (and `duration_seconds` is `f64`).
    pub fn seconds_at(&self, at: Beat) -> f64 {
        let mut tempo = self.tempo().unwrap_or(Bpm(120.0));
        let mut from = 0.0_f64;
        let mut seconds = 0.0_f64;
        let to = at.0.max(0.0);
        for (change, bpm) in self.tempo_map() {
            let change = change.0;
            if change >= to {
                break;
            }
            if change > from {
                seconds += (change - from) * 60.0 / tempo.0;
                from = change;
            }
            tempo = bpm;
        }
        seconds + (to - from) * 60.0 / tempo.0
    }

    /// Where the last event lands: the position of the final event
    /// [`timed`](Self::timed) yields (`Beat(0.0)` for an empty file). A
    /// note-off is an event too, so this is where the last message lands,
    /// not necessarily where sound stops.
    pub fn end(&self) -> Beat {
        let length = match self {
            Self::Smf(smf) => smf.stream.duration_beats,
            Self::Clip(clip) => clip.duration_beats(),
        };
        Beat(0.0) + length
    }
}
