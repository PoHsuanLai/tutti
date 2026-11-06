use super::*;
use crate::MidiFile;
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
    write(
        &path,
        96,
        Some(ClipHeader {
            tempo_bpm: Bpm(128.0),
            time_signature: (5, 4),
        }),
        &events,
    )
    .unwrap();

    let clip = read(&path).unwrap();
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
fn detect_tells_the_two_midi_formats_apart() {
    let clip = tutti_midi_types::write_clip_file(96, &[]);
    assert_eq!(MidiFileKind::detect(&clip), Some(MidiFileKind::ClipFile));

    let smf = crate::smf::to_bytes(&[vec![]], &crate::smf::MidiWriteConfig::default()).unwrap();
    assert_eq!(
        MidiFileKind::detect(&smf),
        Some(MidiFileKind::StandardMidiFile)
    );

    assert_eq!(MidiFileKind::detect(b"not midi at all"), None);
    // Too short to carry either magic — not a format, not a panic.
    assert_eq!(MidiFileKind::detect(b"SMF2"), None);
    assert_eq!(MidiFileKind::detect(b""), None);
}

/// `detect_path` answers from the file's magic, and distinguishes the three
/// outcomes a caller must tell apart: recognised, readable-but-unrecognised,
/// and unreadable.
///
/// The extension case is the trap this closes — a clip file named `.mid`.
/// The extension says SMF; the magic says otherwise, and the magic wins.
#[test]
fn detect_path_reads_the_magic_not_the_extension() {
    let dir = tempfile::tempdir().unwrap();

    // A clip file under an SMF extension still detects as a clip file, and
    // the SMF reader rejects it rather than mis-parsing it.
    let misnamed = dir.path().join("misnamed.mid");
    write(&misnamed, 96, None, &[]).unwrap();
    assert_eq!(
        MidiFileKind::detect_path(&misnamed).unwrap(),
        Some(MidiFileKind::ClipFile)
    );
    assert!(crate::SmfFile::read(&misnamed).is_err());
    assert!(read(&misnamed).is_ok());
    assert!(matches!(MidiFile::read(&misnamed), Ok(MidiFile::Clip(_))));

    // Readable but unrecognised — `Ok(None)`, not an error.
    let junk = dir.path().join("not_midi.bin");
    std::fs::write(&junk, b"just some bytes").unwrap();
    assert_eq!(MidiFileKind::detect_path(&junk).unwrap(), None);

    // Missing file — an error, not `Ok(None)`.
    assert!(MidiFileKind::detect_path(dir.path().join("absent.midi2")).is_err());
}
