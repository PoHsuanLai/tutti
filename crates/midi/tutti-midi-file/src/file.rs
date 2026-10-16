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
use tutti_core::{Beat, Bpm};
use tutti_midi_types::{normalize, read_clip_file, MidiEvent, ParsedClipFile};

use crate::smf::{tracks, ParsedMidiFile, SmfTrack};
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
    Smf {
        /// Its tracks, notes paired, as [`smf::tracks`](crate::smf::tracks)
        /// reads them.
        tracks: Vec<SmfTrack>,
        /// The same file as one merged event stream, which
        /// [`timed`](MidiFile::timed) reads: the paired notes alone would
        /// drop every controller, bend and program change.
        stream: ParsedMidiFile,
    },
    /// A MIDI 2.0 Clip File: one stream, keeping 16-bit velocities.
    Clip(Box<ParsedClipFile>),
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
            Some(MidiFileKind::StandardMidiFile) => Ok(Self::Smf {
                tracks: tracks(bytes)?,
                stream: ParsedMidiFile::parse(bytes)?,
            }),
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
            Self::Smf { .. } => MidiFileKind::StandardMidiFile,
            Self::Clip(_) => MidiFileKind::ClipFile,
        }
    }

    /// Every event as `(position, event)`, positions in quarter notes from the
    /// start of the file, in file order (an SMF's tracks merged by position,
    /// ties kept in track order).
    ///
    /// Each event is [`normalize`]d to MIDI 2.0 Channel Voice, so the two
    /// containers speak one vocabulary: an SMF's 7-bit velocity is widened to
    /// 16 bits by the spec's Min-Center-Max scaler, and a velocity-0 note-on
    /// is a note-off. An SMF and a Clip File of the same notes therefore yield
    /// the same list.
    ///
    /// What each container contributes: an SMF its channel-voice messages
    /// (its meta events are not MIDI messages: the tempo is
    /// [`tempo`](Self::tempo), a track's name is in `tracks`); a Clip File every
    /// UMP in its sequence, Flex Data included. The tempo map is not applied.
    pub fn timed(&self) -> Box<dyn Iterator<Item = (Beat, MidiEvent)> + '_> {
        match self {
            Self::Smf { stream, .. } => {
                // Reused across events: midly writes the message's wire bytes
                // here, and `from_midi1_bytes` reads them back as a UMP.
                let mut wire = Vec::with_capacity(3);
                Box::new(stream.events.iter().filter_map(move |e| {
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

    /// The tempo the file declares, `None` if it declares none.
    ///
    /// An SMF always has one: its first Set Tempo, or the 120 BPM the SMF
    /// specification assumes without one. A Clip File has its first Flex Data
    /// Set Tempo or `None`, and adopting a default for it is the caller's
    /// decision ([`ParsedClipFile::tempo_bpm`] says why).
    pub fn tempo(&self) -> Option<Bpm> {
        match self {
            Self::Smf { stream, .. } => Some(stream.tempo_bpm),
            Self::Clip(clip) => clip.tempo_bpm(),
        }
    }

    /// Where the last event lands: the position of the final event
    /// [`timed`](Self::timed) yields (`Beat(0.0)` for an empty file). A
    /// note-off is an event too, so this is where the last message lands,
    /// not necessarily where sound stops.
    pub fn end(&self) -> Beat {
        let length = match self {
            Self::Smf { stream, .. } => stream.duration_beats,
            Self::Clip(clip) => clip.duration_beats(),
        };
        Beat(0.0) + length
    }
}
