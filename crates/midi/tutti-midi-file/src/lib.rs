#![doc = include_str!("../README.md")]

pub mod clip;
pub mod smf;

pub use clip::{read_clip_file_from_path, write_clip_file_to_path, MidiFileKind};
pub use smf::{
    encode_midi_file, write_midi_file, MidiWriteOptions, ParsedMidiFile, SmfMessage, SmfNote,
    SmfTimedEvent, SmfTrack,
};

use thiserror::Error;

/// What can go wrong reading or writing a MIDI file.
#[derive(Error, Debug)]
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
}

impl From<midly::Error> for Error {
    fn from(e: midly::Error) -> Self {
        Self::MidiFileParse(e.to_string())
    }
}

/// `Result` with this crate's [`enum@Error`] as the failure type.
pub type Result<T> = std::result::Result<T, Error>;

/// The MIDI 2.0 Clip File byte codec, re-exported from `tutti-midi-types`.
///
/// [`clip`] is the *file-level* (path) half; these are the byte-level types it
/// reads and writes.
pub use tutti_midi_types::{
    read_clip_file, write_clip_file, write_clip_file_from_beats, write_clip_file_with_header,
    ClipEvent, ClipFileError, ClipHeader, ClipNote, ParsedClipFile, CLIP_FILE_MAGIC,
};

/// `midly` itself, for a consumer that needs the raw SMF vocabulary.
pub use midly;
