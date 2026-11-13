#![doc = include_str!("../README.md")]
//! MIDI **file** codec for the Tutti audio engine: Standard MIDI File (SMF,
//! `.mid`) and MIDI 2.0 Clip File (M2-116, `.midi2`) read into one model and
//! written back from it, losslessly. Re-exported by `tutti` as
//! `tutti::midi_file` (feature `midi`).
//!
//! ## The model
//!
//! A [`MidiFile`] is its container (an SMF's format 0, 1 or 2, or a Clip File),
//! its division, its [`Track`]s and the `TempoMap` its tempo events declare. A
//! track is `(Tick, FileEvent)` in order and where it ends. Ticks are integers,
//! so a write is exact; [`MidiFile::events`] gives the merged timeline as
//! [`Beat`](tutti_types::Beat)s, [`MidiFile::timeline`] the same as packets for a clip, and [`Track::notes`] pairs Note Ons with Note
//! Offs.
//!
//! - **A [`FileEvent`]** is a `Message` (an SMF's channel message is MIDI 1.0 on
//!   group 0; a Clip File's packet is as stored), a `SysEx` or `Escape` of any
//!   length, or a `Meta` (text, tempo, time and key signature, the rest as bytes).
//!   The file layer does not translate: a MIDI 1.0 Note On at velocity 0 stays one.
//! - **Lossless.** A read, a write and a read give the same file: SysEx and meta
//!   included. Running status and a Delta Clockstamp's spelling may differ.
//! - **Truncation is an error** ([`Error::Truncated`]), not a shorter file.
//! - **Format 2** keeps its sequences apart: no merged `events()`, a tempo map
//!   per track ([`MidiFile::tempo_map_of`]).
//! - **Tempo** is `MidiFile::tempo_map()`, steps at the tick each Set Tempo (an
//!   SMF's meta event, a Clip File's Flex Data message) lands on; 120 BPM until
//!   the first if none is at the start. `seconds_at` integrates it.
//! - **Metrical timing only.** An SMPTE division is refused with
//!   [`Error::MidiUnsupportedTiming`]: it has no beat grid to put a tempo map on.
//!
//! - **To a Clip File.** [`MidiFile::to_clip`] converts a format 0 or 1 SMF: its meta
//!   events become the Flex Data messages of M2-116 Appendix B, and what has no UMP
//!   form comes back in [`Converted::dropped`], never lost silently.
//!
//! [`MidiFileKind::detect`] tells the two containers apart by magic, not by
//! extension. Nothing here touches an OS MIDI API; ports are in
//! `tutti-midi-hardware`.
//!
//! ## Quick start
//!
//! Build a file, write it, read it back; the tempo map answers in seconds:
//!
//! ```rust
//! use tutti_midi_file::{Meta, MidiFile, SmfFormat, Tick, Track};
//! use tutti_types::{Beat, Bpm, Seconds};
//! use tutti_ump::{MidiChannel, NoteNumber, U7};
//!
//! // 120 BPM, then 60 from beat 4, at 480 ticks per quarter.
//! let track = Track::new()
//!     .meta(Tick(0), Meta::tempo(Bpm::of(120.0)))
//!     .note(Tick(0), 1_440, MidiChannel::FIRST, NoteNumber::of(60), U7::of(100))
//!     .meta(Tick::quarters(4, 480), Meta::tempo(Bpm::of(60.0)));
//! let file = MidiFile::smf(SmfFormat::SingleTrack, 480, vec![track])?;
//!
//! let read = MidiFile::from_bytes(&file.to_bytes()?)?;
//! assert_eq!(read, file);
//! assert_eq!(read.seconds_at(Beat(8.0)), Seconds(6.0));
//! let notes = read.tracks()[0].notes();
//! assert_eq!((notes[0].start, notes[0].end), (Tick(0), Tick(1_440)));
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! A clip plays a file's messages as they are: `file.timeline()` is `(Beat, Ump)`
//! pairs (channel messages as their packets, SysEx as SysEx7 packets, meta events
//! dropped: the tempo is [`MidiFile::tempo_map`]), which the session's
//! `Midi::play_file` plays as a clip (or a caller turns into `tutti-midi-runtime`'s
//! `TimedMidiEvent`s for `play_clip`). A format 2 file has no one timeline and is
//! refused with `NoCommonTimeline`.
//!
//! ### Tempo is the wire's
//!
//! SMF stores tempo as microseconds per quarter note, an integer: `174.0` BPM is
//! not representable and reads back as `174.0002958...`. That is the format.
//!
//! ### Reading from a path
//!
//! ```rust,no_run
//! use tutti_midi_file::{MidiFile, MidiFileKind};
//!
//! // Which container? By magic bytes, not by extension: a `.mid` extension on a
//! // Clip File is a real thing that happens.
//! if MidiFileKind::detect_path("song.mid")?.is_some() {
//!     let file = MidiFile::read("song.mid")?;
//!     for track in file.tracks() {
//!         println!("{:?}: {} notes", track.name(), track.notes().len());
//!     }
//! }
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
#![forbid(unsafe_code)]
#![cfg_attr(docsrs, feature(doc_cfg))]

mod clip_file;
mod config;
mod file;
mod kind;
mod smf;

pub use clip_file::ClipFileError;
pub use config::{ConfigError, OutOfOrder, Refusal};
pub use file::{
    Converted, DropReason, Dropped, FileEvent, Meta, MidiFile, NoCommonTimeline, PairedNote,
    SmfFormat, TextKind, Tick, ToClipError, Track,
};
pub use kind::MidiFileKind;

use thiserror::Error;

/// What can go wrong reading or writing a MIDI file.
#[derive(Error, Debug)]
#[non_exhaustive]
pub enum Error {
    /// The file could not be read or written at all — missing, unreadable, or
    /// the write failed.
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    /// The bytes are not a well-formed SMF: a malformed event (a data byte
    /// with its top bit set, a running status with none active, a
    /// variable-length quantity over four bytes, an event cut off inside its
    /// chunk), a track count that disagrees with the header, a zero
    /// ticks-per-beat division. The message says which.
    #[error("MIDI parse error: {0}")]
    MidiFileParse(String),

    /// The file is cut off: a chunk header short of its eight bytes, a chunk
    /// whose declared length runs past the end of the file, or fewer track
    /// chunks than the header declares. What was read is not returned: a
    /// shorter file is not the file.
    #[error("the MIDI file is truncated")]
    Truncated,

    /// SMPTE/timecode division. The codec reads metrical timing only: beats
    /// come from the file's division, and a timecode file has no beat grid to
    /// read them from or put a tempo map on.
    #[error("Unsupported MIDI timing format")]
    MidiUnsupportedTiming,

    /// The tracks cannot make a file of the container asked for: an empty
    /// track list, a zero division, events out of order, an event the
    /// container cannot hold. The [`ConfigError`] says which and where.
    #[error("invalid tracks: {0}")]
    Config(#[from] ConfigError),

    /// An event pushed before the last of its track
    /// ([`Track::push`]).
    #[error(transparent)]
    OutOfOrder(#[from] OutOfOrder),

    /// The file's tempo events do not make a tempo map.
    #[error("the file's tempo events: {0}")]
    Tempo(#[from] tutti_tempo::TempoError),

    /// A Clip File that did not parse, read from bytes by
    /// [`MidiFile::from_bytes`].
    ///
    /// Kept apart from [`Self::MidiFileParse`]: the clip decoder's errors name
    /// which structural rule the file broke, and flattening them to a string
    /// would lose which container was being read.
    #[error(transparent)]
    Clip(#[from] ClipFileError),

    /// The file cannot be converted to a Clip File ([`MidiFile::to_clip`]).
    #[error(transparent)]
    ToClip(#[from] ToClipError),

    /// A format 2 file has no common timeline ([`MidiFile::timeline`]).
    #[error(transparent)]
    NoCommonTimeline(#[from] NoCommonTimeline),

    /// The leading bytes match neither container's magic, so no decoder ran
    /// ([`MidiFile::from_bytes`]). Distinct from a parse failure: "this is not
    /// a MIDI file" is a different thing to fix than "this MIDI file is
    /// malformed".
    #[error("not a Standard MIDI File or a MIDI 2.0 Clip File")]
    UnknownFormat,
}

/// `Result` with this crate's [`enum@Error`] as the failure type.
pub type Result<T> = std::result::Result<T, Error>;
