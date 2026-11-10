#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]
#![cfg_attr(docsrs, feature(doc_cfg))]

mod clip;
mod clip_file;
mod file;
mod flex;
mod smf;

pub use clip::{MidiFileKind, read_clip, write_clip};
pub use file::{MidiFile, SmfFile};
pub use smf::{
    MidiWriteConfig, ParsedMidiFile, SmfNote, SmfTimedEvent, SmfTrack, smf_to_bytes, write_smf,
};

use thiserror::Error;

/// What can go wrong reading or writing a MIDI file.
#[derive(Error, Debug)]
#[non_exhaustive]
pub enum Error {
    /// The file could not be read or written at all — missing, unreadable, or
    /// the write failed.
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    /// The bytes are not a well-formed file of the expected format. Covers
    /// `midly`'s own parse failures, a zero ticks-per-beat division, and a
    /// clip-file parse error forwarded with its path.
    #[error("MIDI parse error: {0}")]
    MidiFileParse(String),

    /// SMPTE/timecode division. The codec reads metrical timing only — beats
    /// come from the file's division, and a timecode file has no beat grid to
    /// read them from.
    #[error("Unsupported MIDI timing format")]
    MidiUnsupportedTiming,

    /// The write options or track set cannot produce a file — an empty track
    /// list, a zero ticks-per-beat.
    #[error("Invalid config: {0}")]
    InvalidConfig(String),

    /// A Clip File that did not parse, read from bytes by
    /// [`MidiFile::from_bytes`].
    ///
    /// Kept apart from [`Self::MidiFileParse`]: the clip decoder's errors name
    /// which structural rule the file broke, and flattening them to a string
    /// would lose which container was being read.
    #[error(transparent)]
    Clip(#[from] ClipFileError),

    /// The leading bytes match neither container's magic, so no decoder ran
    /// ([`MidiFile::from_bytes`]). Distinct from a parse failure: "this is not
    /// a MIDI file" is a different thing to fix than "this MIDI file is
    /// malformed".
    #[error("not a Standard MIDI File or a MIDI 2.0 Clip File")]
    UnknownFormat,
}

impl From<midly::Error> for Error {
    fn from(e: midly::Error) -> Self {
        Self::MidiFileParse(e.to_string())
    }
}

/// `Result` with this crate's [`enum@Error`] as the failure type.
pub type Result<T> = std::result::Result<T, Error>;

/// The MIDI 2.0 Clip File byte codec.
///
/// [`clip`] is the *file-level* (path) half; these are the byte-level types it
/// reads and writes.
pub use clip_file::{
    CLIP_FILE_MAGIC, ClipEvent, ClipFileError, ClipHeader, ClipNote, ParsedClipFile,
    write_clip_file, write_clip_file_from_beats, write_clip_file_with_header,
};
