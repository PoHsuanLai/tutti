//! MIDI **file** codecs: Standard MIDI File ([`smf`]) and MIDI 2.0 Clip File
//! ([`clip`], M2-116).
//!
//! Reading a `.mid` and talking to a MIDI port are different jobs, and this
//! crate is the boundary that keeps them apart: nothing here touches an OS MIDI
//! API, so a consumer that only reads files never links CoreMIDI or the ALSA
//! sequencer. That is a dependency edge rather than a feature flag — it cannot
//! be got wrong by forgetting `default-features = false`. Ports are
//! `tutti-midi-hardware`'s, which deliberately does not re-export these codecs.
//!
//! # What the two codecs cover
//!
//! - [`smf`] — SMF 1.0 (`.mid`): parse to beat-positioned events
//!   ([`ParsedMidiFile`]) or per-track paired notes ([`smf::tracks`]), and write
//!   ([`encode_midi_file`] / [`write_midi_file`]). Metrical timing only; the
//!   tempo map is reported, not applied.
//! - [`clip`] — MIDI 2.0 Clip File (`.midi2`): the *file-level* (path) half.
//!   The byte-level codec is `tutti-midi-types`' and is re-exported below, so a
//!   clip written there round-trips through a path here with one import.
//!
//! [`MidiFileKind::sniff`] tells the two apart by magic bytes, not extension.
//!
//! Quick start, the tempo-quantisation caveat, and the two constraints worth
//! knowing are in the crate README, included below.
#![doc = include_str!("../README.md")]

pub mod clip;
pub mod smf;

pub use clip::{read_clip_file_from_path, write_clip_file_to_path, MidiFileKind};
pub use smf::{
    encode_midi_file, write_midi_file, MidiWriteConfig, ParsedMidiFile, SmfMessage, SmfNote,
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
