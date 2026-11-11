#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]
#![cfg_attr(docsrs, feature(doc_cfg))]

mod clip_file;
mod file;
mod flex;
mod kind;
mod smf;

pub use clip_file::ClipFileError;
pub use file::{
    Converted, DropReason, Dropped, FileEvent, Meta, MidiFile, PairedNote, SmfFormat, TextKind,
    Tick, ToClipError, Track,
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
    /// container cannot hold. The message says which.
    #[error("Invalid config: {0}")]
    InvalidConfig(String),

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

    /// The leading bytes match neither container's magic, so no decoder ran
    /// ([`MidiFile::from_bytes`]). Distinct from a parse failure: "this is not
    /// a MIDI file" is a different thing to fix than "this MIDI file is
    /// malformed".
    #[error("not a Standard MIDI File or a MIDI 2.0 Clip File")]
    UnknownFormat,
}

/// `Result` with this crate's [`enum@Error`] as the failure type.
pub type Result<T> = std::result::Result<T, Error>;
