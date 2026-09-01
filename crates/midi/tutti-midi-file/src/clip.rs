//! MIDI 2.0 **Clip File** (M2-116) file I/O — the path-level half of the codec
//! whose byte-level half lives in [`tutti_midi_types`].
//!
//! This is to clip files what [`crate::smf`] is to Standard MIDI Files:
//! `read_clip_file_from_path` / `write_clip_file_to_path`, plus [`MidiFileKind`]
//! for deciding *which* of the two a path holds before committing to a parser.
//!
//! Extension is not a reliable discriminator — `.mid` and `.midi` are used for
//! both formats in the wild, and M2-116 §5 registers `.midi2` without retiring
//! them. Both formats are self-identifying by magic, so [`MidiFileKind::sniff`]
//! reads the leading bytes and answers from the content.

use std::path::Path;

use tutti_midi_types::{read_clip_file, ClipEvent, ClipHeader, ParsedClipFile, CLIP_FILE_MAGIC};

use crate::{Error, Result};

/// The 4-byte Standard MIDI File header chunk id (SMF spec §2.1). A clip file's
/// magic is `"SMF2CLIP"`, so the two never collide on their first four bytes.
const SMF_MAGIC: [u8; 4] = *b"MThd";

/// Which MIDI file format a byte stream holds, decided by magic rather than by
/// file extension.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MidiFileKind {
    /// A Standard MIDI File — leading `MThd`. Parse with [`crate::smf::tracks`].
    StandardMidiFile,
    /// A MIDI 2.0 Clip File — leading `SMF2CLIP`. Parse with [`read_clip_file`].
    ClipFile,
}

impl MidiFileKind {
    /// Identify a byte stream by its magic. `None` if it is neither format
    /// (including a stream too short to tell).
    ///
    /// Cheap enough to call on the first bytes of a file: it inspects at most
    /// the leading 8 bytes and never parses the body.
    pub fn sniff(bytes: &[u8]) -> Option<Self> {
        if bytes.len() >= 8 && bytes[..8] == CLIP_FILE_MAGIC {
            Some(Self::ClipFile)
        } else if bytes.len() >= 4 && bytes[..4] == SMF_MAGIC {
            Some(Self::StandardMidiFile)
        } else {
            None
        }
    }

    /// Identify the file at `path` by reading only its header, not the whole
    /// file. `Ok(None)` means "readable, but neither format" — distinct from
    /// `Err`, which means the file could not be read at all.
    pub fn sniff_path(path: impl AsRef<Path>) -> Result<Option<Self>> {
        use std::io::Read;

        let mut header = [0u8; 8];
        let mut file = std::fs::File::open(path.as_ref())?;
        // A short file is not an error — it is simply not one of these formats.
        let read = match file.read(&mut header) {
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => 0,
            Err(e) => return Err(e.into()),
        };
        Ok(Self::sniff(&header[..read]))
    }
}

/// Read and parse a MIDI 2.0 Clip File from `path`.
///
/// The clip-file counterpart of [`crate::smf::tracks_from_path`]. Parse errors
/// keep their [`ClipFileError`](tutti_midi_types::ClipFileError) wording, so
/// "not a clip file" stays distinguishable from "malformed clip file" in the
/// message — check [`MidiFileKind::sniff_path`] first if you need to branch on
/// that programmatically.
pub fn read_clip_file_from_path(path: impl AsRef<Path>) -> Result<ParsedClipFile> {
    let path = path.as_ref();
    let bytes = std::fs::read(path)?;
    read_clip_file(&bytes).map_err(|e| Error::MidiFileParse(format!("{}: {e}", path.display())))
}

/// Write events to a MIDI 2.0 Clip File at `path`.
///
/// `header` is optional only in the type system — M2-116 §7.1.1/§7.1.2 recommend
/// every clip declare its tempo and time signature, and an importer that reads a
/// clip without them has to guess. Pass `Some` unless the musical context is
/// genuinely unknown.
pub fn write_clip_file_to_path(
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
mod tests {
    use super::*;
    use tutti_midi_types::MidiEvent;
    use tutti_midi_types::{Bpm, MidiChannel, MidiGroup};

    #[test]
    fn round_trips_a_clip_through_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("round_trip.midi2");
        let events = [
            ClipEvent::new(
                0,
                MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0xABCD),
            ),
            ClipEvent::new(
                96,
                MidiEvent::note_off(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0),
            ),
        ];
        write_clip_file_to_path(
            &path,
            96,
            Some(ClipHeader {
                tempo_bpm: Bpm(128.0),
                time_signature: (5, 4),
            }),
            &events,
        )
        .unwrap();

        let clip = read_clip_file_from_path(&path).unwrap();
        assert_eq!(clip.time_signature(), Some((5, 4)));
        assert!(!clip.tempo_bpm().unwrap().differs_from(Bpm(128.0), 0.05));

        // The header's Set Tempo / Set Time Signature are real events in the
        // stream, so `events` carries them ahead of the notes — compare the
        // paired notes rather than the raw event list.
        let notes = clip.notes();
        assert_eq!(notes.len(), 1);
        assert_eq!(
            (notes[0].note, notes[0].duration_beats),
            (60, tutti_core::BeatDuration(1.0))
        );
        // The whole point of the format: velocity survives at 16 bits.
        assert_eq!(notes[0].velocity, 0xABCD);
    }

    #[test]
    fn sniff_tells_the_two_midi_formats_apart() {
        let clip = tutti_midi_types::write_clip_file(96, &[]);
        assert_eq!(MidiFileKind::sniff(&clip), Some(MidiFileKind::ClipFile));

        let smf = crate::smf::encode_midi_file(&[vec![]], &crate::smf::MidiWriteConfig::default())
            .unwrap();
        assert_eq!(
            MidiFileKind::sniff(&smf),
            Some(MidiFileKind::StandardMidiFile)
        );

        assert_eq!(MidiFileKind::sniff(b"not midi at all"), None);
        // Too short to carry either magic — not a format, not a panic.
        assert_eq!(MidiFileKind::sniff(b"SMF2"), None);
        assert_eq!(MidiFileKind::sniff(b""), None);
    }

    /// `sniff_path` answers from the file's magic, and distinguishes the three
    /// outcomes a caller must tell apart: recognised, readable-but-unrecognised,
    /// and unreadable.
    ///
    /// The extension case is the trap this closes — a clip file named `.mid`.
    /// The extension says SMF; the magic says otherwise, and the magic wins.
    #[test]
    fn sniff_path_reads_the_magic_not_the_extension() {
        let dir = tempfile::tempdir().unwrap();

        // A clip file under an SMF extension still sniffs as a clip file, and
        // the SMF reader rejects it rather than mis-parsing it.
        let misnamed = dir.path().join("misnamed.mid");
        write_clip_file_to_path(&misnamed, 96, None, &[]).unwrap();
        assert_eq!(
            MidiFileKind::sniff_path(&misnamed).unwrap(),
            Some(MidiFileKind::ClipFile)
        );
        assert!(crate::smf::tracks_from_path(&misnamed).is_err());
        assert!(read_clip_file_from_path(&misnamed).is_ok());

        // Readable but unrecognised — `Ok(None)`, not an error.
        let junk = dir.path().join("not_midi.bin");
        std::fs::write(&junk, b"just some bytes").unwrap();
        assert_eq!(MidiFileKind::sniff_path(&junk).unwrap(), None);

        // Missing file — an error, not `Ok(None)`.
        assert!(MidiFileKind::sniff_path(dir.path().join("absent.midi2")).is_err());
    }
}
