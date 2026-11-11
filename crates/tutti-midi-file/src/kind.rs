//! [`MidiFileKind`]: which of the two containers a byte stream holds, decided
//! by magic rather than by name.
//!
//! Extension is not a reliable discriminator: `.mid` and `.midi` are used for
//! both formats in the wild, and M2-116 section 5 registers `.midi2` without
//! retiring them. Both formats are self-identifying by magic, so
//! [`MidiFileKind::detect`] reads the leading bytes and answers from the
//! content.

use std::path::Path;

use crate::Result;
use crate::clip_file::CLIP_FILE_MAGIC;

/// The 4-byte Standard MIDI File header chunk id (SMF spec §2.1). A clip file's
/// magic is `"SMF2CLIP"`, so the two never collide on their first four bytes.
const SMF_MAGIC: [u8; 4] = *b"MThd";

/// Which MIDI file format a byte stream holds, decided by magic rather than by
/// file extension.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum MidiFileKind {
    /// A Standard MIDI File — leading `MThd`. Read with [`MidiFile::from_bytes`](crate::MidiFile::from_bytes).
    StandardMidiFile,
    /// A MIDI 2.0 Clip File — leading `SMF2CLIP`. Read with [`MidiFile::from_bytes`](crate::MidiFile::from_bytes).
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
    /// [`Error::Io`](crate::Error::Io) if the file cannot be opened or read. A
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

#[cfg(test)]
mod tests;
