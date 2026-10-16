//! [`MidiFile`]: either container, sniffed by magic, read as one timed
//! MIDI 2.0 event stream.
//!
//! SMF inputs come from `support`, the spec-written encoder (see its header),
//! so a tick is the tick the test wrote, not one this crate's writer chose.
//! The decode tests (`smf_bytes_decode_to_tracks` through
//! `a_truncated_smf_is_a_parse_error`) moved here from `bevy-tutti`'s
//! `midi/file/tests.rs` with the decode itself (design 015, PR 1).

mod support;

use support::{build_smf, Division, RawEvent};
use tutti_core::{Beat, Bpm};
use tutti_midi_file::{
    encode_midi_file, write_clip_file_from_beats, write_clip_file_with_header, ClipEvent,
    ClipHeader, Error, MidiFile, MidiFileKind, MidiWriteConfig, SmfMessage, SmfTimedEvent,
};
use tutti_midi_types::{MidiChannel, MidiEvent, MidiGroup};

/// 480 ticks per quarter, the value `MidiWriteConfig` defaults to.
const TPQ: u16 = 480;
const BEAT: u32 = TPQ as u32;

fn metrical(tracks: &[Vec<RawEvent>]) -> Vec<u8> {
    build_smf(1, Division::Metrical(TPQ), tracks)
}

/// A one-note SMF's bytes, through this crate's own writer.
fn smf_bytes() -> Vec<u8> {
    encode_midi_file(
        &[vec![
            SmfTimedEvent {
                time_beats: Beat(0.0),
                channel: 0,
                msg: SmfMessage::NoteOn {
                    key: 60.into(),
                    vel: 100.into(),
                },
            },
            SmfTimedEvent {
                time_beats: Beat(1.0),
                channel: 0,
                msg: SmfMessage::NoteOff {
                    key: 60.into(),
                    vel: 0.into(),
                },
            },
        ]],
        &MidiWriteConfig::default(),
    )
    .expect("encodes")
}

// ── the timed stream ────────────────────────────────────────────────────────

/// **An SMF and a Clip File of the same notes yield the same
/// `(Beat, MidiEvent)` list, the SMF's velocity widened.**
///
/// The SMF holds two tracks (so the merge is exercised), a note on channel 3,
/// a note ended by a velocity-0 note-on (the MIDI 1.0 idiom) and one by a
/// note-off. The clip holds the same notes as native MIDI 2.0 events, its
/// velocities the Min-Center-Max figures the spec fixes for 7-bit 127 and 64:
/// `0xFFFF` and `0x8000`.
///
/// Mutation (run): `timed`'s SMF arm returning the parsed event without
/// `normalize` (MIDI 1.0 Channel Voice, 7-bit) → the lists differ → fails.
/// Mutation (run): the SMF arm dropping the second track's channel-3 events
/// (a `filter` on channel 0 before the conversion) → fails.
#[test]
fn an_smf_and_a_clip_file_of_the_same_notes_yield_the_same_events() {
    let smf = metrical(&[
        vec![
            RawEvent::note_on(0, 0, 60, 127),
            // The 1.0 idiom: a velocity-0 note-on ends the note.
            RawEvent::note_on(BEAT, 0, 60, 0),
        ],
        vec![
            RawEvent::note_on(BEAT / 2, 3, 64, 64),
            RawEvent::note_off(BEAT, 3, 64, 0),
        ],
    ]);
    let (g, c0, c3) = (MidiGroup::FIRST, MidiChannel::FIRST, MidiChannel::new(3));
    let clip = write_clip_file_from_beats(
        TPQ,
        [
            (Beat(0.0), MidiEvent::note_on(g, c0, 60, 0xFFFF)),
            (Beat(0.5), MidiEvent::note_on(g, c3, 64, 0x8000)),
            (Beat(1.0), MidiEvent::note_off(g, c0, 60, 0)),
            (Beat(1.5), MidiEvent::note_off(g, c3, 64, 0)),
        ],
    );

    let smf = MidiFile::from_bytes(&smf).expect("the SMF decodes");
    let clip = MidiFile::from_bytes(&clip).expect("the clip decodes");
    assert_eq!(smf.kind(), MidiFileKind::StandardMidiFile);
    assert_eq!(clip.kind(), MidiFileKind::ClipFile);

    let from_smf: Vec<(Beat, MidiEvent)> = smf.timed().collect();
    let from_clip: Vec<(Beat, MidiEvent)> = clip.timed().collect();
    assert_eq!(from_smf.len(), 4, "every channel message of both tracks");
    assert_eq!(from_smf, from_clip);
    assert_eq!(
        from_smf[0].1.velocity_u16(),
        Some(0xFFFF),
        "7-bit 127 widens to full-scale 16-bit"
    );
}

/// **A note-on at tick 480 of a 480-ppq file lands at `Beat(1.0)`.**
///
/// Mutation (run): `smf.rs`'s `parse_track` adding an event's delta after
/// pushing it (the tick of the *previous* event) → the note lands at
/// `Beat(0.0)` → fails.
#[test]
fn a_note_on_at_tick_480_of_a_480_ppq_file_lands_at_beat_one() {
    let file = MidiFile::from_bytes(&metrical(&[vec![
        RawEvent::note_on(BEAT, 0, 60, 100),
        RawEvent::note_off(BEAT, 0, 60, 0),
    ]]))
    .expect("decodes");
    let timed: Vec<(Beat, MidiEvent)> = file.timed().collect();
    assert_eq!(timed[0].0, Beat(1.0));
    assert_eq!(timed[0].1.note(), Some(60));
    assert_eq!(timed[1].0, Beat(2.0));
    assert_eq!(file.end(), Beat(2.0), "the last event, the note-off");
}

/// **`tempo` is the declared one, and an SMF always declares one.**
///
/// 600 000 µs per quarter is exactly 100 BPM, and so is the clip header's
/// Flex Data tempo. An SMF with no Set Tempo reads as the specification's
/// 120; a clip with none reads `None`, its default being the caller's call.
///
/// Mutation (run): `tempo`'s clip arm returning `Some(Bpm(120.0))` when the
/// clip declares none → fails.
#[test]
fn tempo_reads_the_declared_tempo_from_either_container() {
    let smf = metrical(&[vec![
        RawEvent::tempo(0, 600_000),
        RawEvent::note_on(0, 0, 60, 100),
    ]]);
    let tempo = MidiFile::from_bytes(&smf).unwrap().tempo().unwrap();
    assert!(!tempo.differs_from(Bpm(100.0), 1e-9), "got {tempo:?}");

    let clip = write_clip_file_with_header(
        TPQ,
        ClipHeader {
            tempo_bpm: Bpm(100.0),
            time_signature: (4, 4),
        },
        &[],
    );
    let tempo = MidiFile::from_bytes(&clip).unwrap().tempo().unwrap();
    assert!(!tempo.differs_from(Bpm(100.0), 1e-6), "got {tempo:?}");

    let bare_smf = metrical(&[vec![RawEvent::note_on(0, 0, 60, 100)]]);
    assert_eq!(
        MidiFile::from_bytes(&bare_smf).unwrap().tempo(),
        Some(Bpm(120.0))
    );
    let bare_clip = tutti_midi_file::write_clip_file(
        TPQ,
        &[ClipEvent::new(
            0,
            MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x8000),
        )],
    );
    assert_eq!(MidiFile::from_bytes(&bare_clip).unwrap().tempo(), None);
}

/// `read` is `from_bytes` over a file's bytes, and a missing file is an IO
/// error, not an unknown format.
#[test]
fn read_decodes_a_path_and_reports_a_missing_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("song.mid");
    std::fs::write(&path, smf_bytes()).unwrap();
    assert_eq!(
        MidiFile::read(&path).unwrap().kind(),
        MidiFileKind::StandardMidiFile
    );
    let err = MidiFile::read(dir.path().join("absent.mid")).expect_err("no file");
    assert!(matches!(err, Error::Io(_)), "got {err:?}");
}

// ── decoding: moved from bevy-tutti with the decode ─────────────────────────

/// **An SMF decodes to its tracks.**
#[test]
fn smf_bytes_decode_to_tracks() {
    let file = MidiFile::from_bytes(&smf_bytes()).expect("decodes");
    match file {
        MidiFile::Smf { tracks, .. } => {
            assert_eq!(tracks.len(), 1, "one track in, one track out");
            assert!(
                !tracks[0].notes.is_empty(),
                "the encoded note must survive the round trip"
            );
        }
        MidiFile::Clip(_) => panic!("an SMF must not decode as a clip file"),
    }
}

/// **The container is decided by magic, not by extension.**
///
/// This is why one type covers both formats. `.mid` is worn by a Standard
/// MIDI File and a MIDI 2.0 Clip File alike, and a reader that trusted the
/// name would decode half its inputs with the wrong decoder.
#[test]
fn a_clip_file_is_recognised_by_its_magic() {
    let bytes = tutti_midi_types::write_clip_file(960, &[]);
    let file = MidiFile::from_bytes(&bytes).expect("decodes");
    assert!(
        matches!(file, MidiFile::Clip(_)),
        "a clip file's SMF2CLIP magic must select the clip decoder, whatever \
         extension the file happened to have"
    );
}

/// **Neither format is its own error, not a parse failure.**
///
/// "This is not a MIDI file" and "this MIDI file is malformed" are different
/// things for a user to fix, so they must not collapse into one message.
#[test]
fn a_file_of_neither_format_is_named_as_such() {
    let err = MidiFile::from_bytes(b"not a midi file at all").expect_err("must fail");
    assert!(
        matches!(err, Error::UnknownFormat),
        "expected UnknownFormat, got {err:?}"
    );
}

/// **A truncated file of a known format is a *parse* error, not UnknownFormat.**
///
/// The complement of the test above: the magic is right, so the container is
/// known — what failed is the decode. Distinguishing the two is the whole point
/// of sniffing separately from parsing.
#[test]
fn a_truncated_smf_is_a_parse_error() {
    let mut bytes = smf_bytes();
    bytes.truncate(10); // keeps `MThd`, loses the rest
    let err = MidiFile::from_bytes(&bytes).expect_err("must fail");
    assert!(
        !matches!(err, Error::UnknownFormat),
        "a truncated SMF still has MThd, so it is a decode failure rather than \
         an unrecognised container; got {err:?}"
    );
}
