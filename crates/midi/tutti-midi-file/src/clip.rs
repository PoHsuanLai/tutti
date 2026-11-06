//! MIDI 2.0 **Clip File** (M2-116) file I/O — the path-level half of the codec
//! whose byte-level half lives in [`tutti_midi_types`].
//!
//! [`read()`] and [`write()`] move a clip to and from disk;
//! [`MidiFile::read`](crate::MidiFile::read) reads either container. [`MidiFileKind`] decides *which* of the two a
//! path holds before committing to a parser.
//!
//! Extension is not a reliable discriminator — `.mid` and `.midi` are used for
//! both formats in the wild, and M2-116 §5 registers `.midi2` without retiring
//! them. Both formats are self-identifying by magic, so [`MidiFileKind::detect`]
//! reads the leading bytes and answers from the content.

use std::path::Path;

use tutti_midi_types::{CLIP_FILE_MAGIC, ClipEvent, ClipHeader, ParsedClipFile};

use crate::{Error, Result};

/// The 4-byte Standard MIDI File header chunk id (SMF spec §2.1). A clip file's
/// magic is `"SMF2CLIP"`, so the two never collide on their first four bytes.
const SMF_MAGIC: [u8; 4] = *b"MThd";

/// Which MIDI file format a byte stream holds, decided by magic rather than by
/// file extension.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum MidiFileKind {
    /// A Standard MIDI File — leading `MThd`. Read with [`SmfFile::from_bytes`](crate::SmfFile::from_bytes).
    StandardMidiFile,
    /// A MIDI 2.0 Clip File — leading `SMF2CLIP`. Read with [`ParsedClipFile::from_bytes`](tutti_midi_types::ParsedClipFile::from_bytes).
    ClipFile,
}

impl MidiFileKind {
    /// Identifies a byte stream by its magic. `None` if it is neither format
    /// (including a stream too short to tell).
    ///
    /// Cheap enough to call on the first bytes of a file: it inspects at most
    /// the leading 8 bytes and never parses the body.
    pub fn detect(bytes: &[u8]) -> Option<Self> {
        if bytes.len() >= 8 && bytes[..8] == CLIP_FILE_MAGIC {
            Some(Self::ClipFile)
        } else if bytes.len() >= 4 && bytes[..4] == SMF_MAGIC {
            Some(Self::StandardMidiFile)
        } else {
            None
        }
    }

    /// Identifies the file at `path` by reading only its header, not the whole
    /// file. `Ok(None)` means "readable, but neither format".
    ///
    /// # Errors
    ///
    /// [`Error::Io`] if the file cannot be opened or read. A
    /// file shorter than a header is not an error; it returns `Ok(None)`.
    pub fn detect_path(path: impl AsRef<Path>) -> Result<Option<Self>> {
        use std::io::Read;

        let mut header = [0u8; 8];
        let mut file = std::fs::File::open(path.as_ref())?;
        // A short file is not an error — it is simply not one of these formats.
        let read = match file.read(&mut header) {
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => 0,
            Err(e) => return Err(e.into()),
        };
        Ok(Self::detect(&header[..read]))
    }
}

/// Reads and parses a MIDI 2.0 Clip File from `path`.
///
/// Parse errors keep their [`ClipFileError`](tutti_midi_types::ClipFileError)
/// wording, prefixed with the path. Use [`MidiFileKind::detect_path`] first to
/// branch on the container programmatically, or
/// [`MidiFile::read`](crate::MidiFile::read) to accept either.
///
/// # Errors
///
/// [`Error::Io`] if the file cannot be read, or
/// [`Error::MidiFileParse`] (prefixed with the
/// path) if the bytes are not a valid Clip File.
pub fn read(path: impl AsRef<Path>) -> Result<ParsedClipFile> {
    let path = path.as_ref();
    let bytes = std::fs::read(path)?;
    ParsedClipFile::from_bytes(&bytes)
        .map_err(|e| Error::MidiFileParse(format!("{}: {e}", path.display())))
}

/// Writes events to a MIDI 2.0 Clip File at `path`.
///
/// `header` is optional only in the type system — M2-116 §7.1.1/§7.1.2 recommend
/// every clip declare its tempo and time signature, and an importer that reads a
/// clip without them has to guess. Pass `Some` unless the musical context is
/// genuinely unknown.
///
/// # Errors
///
/// [`Error::Io`] if the file cannot be written.
pub fn write(
    path: impl AsRef<Path>,
    ticks_per_quarter: u16,
    header: Option<ClipHeader>,
    events: &[ClipEvent],
) -> Result<()> {
    let bytes = match header {
        Some(h) => tutti_midi_types::write_clip_file_with_header(ticks_per_quarter, h, events),
        None => tutti_midi_types::write_clip_file(ticks_per_quarter, events),
    };
    std::fs::write(path.as_ref(), bytes)?;
    Ok(())
}

#[cfg(test)]
mod tests;
