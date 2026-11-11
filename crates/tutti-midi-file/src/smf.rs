//! Standard MIDI File bytes: the wire half under [`MidiFile`](crate::MidiFile).
//!
//! [`read()`] is a `winnow` parser (private to `read.rs`, in no signature) and
//! [`write()`] a fold of pure functions, the reader's inverse. Both parse onto
//! and write from this crate's own types ([`FileEvent`](crate::FileEvent),
//! [`Meta`](crate::Meta)).
//!
//! # Everything here is 7-bit
//!
//! An SMF cannot express MIDI 2.0 resolution: a channel message is a
//! [`Voice1`](tutti_ump::Voice1) on group 0, and the MIDI 2.0 side of the
//! crate (the Clip File) is a *different format*, not a wider encoding of
//! this one.
//!
//! # Truncation
//!
//! A chunk that runs past the end of the file, or fewer track chunks than the
//! header declares, is [`Error::Truncated`](crate::Error::Truncated); a track
//! that ends mid-event, or a track count that disagrees with the header the
//! other way, is [`Error::MidiFileParse`](crate::Error::MidiFileParse).

mod read;
mod write;

pub(crate) use read::read;
pub(crate) use write::write;

use crate::file::{SmfFormat, Track};

/// What an SMF holds, read: the header's format and division and the tracks.
pub(crate) struct SmfParts {
    pub(crate) format: SmfFormat,
    pub(crate) ticks_per_quarter: u16,
    pub(crate) tracks: Vec<Track>,
}
