use super::*;
use crate::MidiFile;

#[test]
fn detect_tells_the_two_midi_formats_apart() {
    let clip = MidiFile::clip(96, crate::Track::default())
        .unwrap()
        .to_bytes()
        .unwrap();
    assert_eq!(MidiFileKind::detect(&clip), Some(MidiFileKind::ClipFile));

    let smf = MidiFile::smf(
        crate::SmfFormat::SingleTrack,
        480,
        vec![crate::Track::default()],
    )
    .unwrap()
    .to_bytes()
    .unwrap();
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
    MidiFile::clip(96, crate::Track::default())
        .unwrap()
        .write(&misnamed)
        .unwrap();
    assert_eq!(
        MidiFileKind::detect_path(&misnamed).unwrap(),
        Some(MidiFileKind::ClipFile)
    );
    assert_eq!(
        MidiFile::read(&misnamed).unwrap().kind(),
        MidiFileKind::ClipFile
    );

    // Readable but unrecognised — `Ok(None)`, not an error.
    let junk = dir.path().join("not_midi.bin");
    std::fs::write(&junk, b"just some bytes").unwrap();
    assert_eq!(MidiFileKind::detect_path(&junk).unwrap(), None);

    // Missing file — an error, not `Ok(None)`.
    assert!(MidiFileKind::detect_path(dir.path().join("absent.midi2")).is_err());
}
