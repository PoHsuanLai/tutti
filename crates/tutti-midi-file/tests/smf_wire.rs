//! The SMF byte layer: variable-length quantities, running status, framing
//! and the writer's byte output, against bytes built from the SMF 1.0 spec
//! (`support`, which shares no code with the crate).

mod support;

use support::{Division, RawEvent, build_smf, write_vlq};
use tutti_midi_file::{Error, FileEvent, Meta, MidiFile, SmfFormat, TextKind, Tick, Track};
use tutti_ump::{Message, Voice1};

const TPQ: u16 = 96;

fn file(events: Vec<RawEvent>) -> Vec<u8> {
    build_smf(0, Division::Metrical(TPQ), &[events])
}

/// A format 0 file whose one track body is exactly `body` (no End of Track
/// added), for bytes the builder cannot spell.
fn raw_track(body: &[u8]) -> Vec<u8> {
    let mut bytes = build_smf(0, Division::Metrical(TPQ), &[vec![]]);
    bytes.truncate(14);
    bytes.extend_from_slice(b"MTrk");
    bytes.extend_from_slice(&(body.len() as u32).to_be_bytes());
    bytes.extend_from_slice(body);
    bytes
}

fn ticks(bytes: &[u8]) -> Vec<u64> {
    let file = MidiFile::from_bytes(bytes).expect("reads");
    file.tracks()[0].events.iter().map(|(t, _)| t.0).collect()
}

fn voices(bytes: &[u8]) -> Vec<Voice1> {
    let file = MidiFile::from_bytes(bytes).expect("reads");
    file.tracks()[0]
        .events
        .iter()
        .filter_map(|(_, e)| match e {
            FileEvent::Message(Message::Midi1(_, v)) => Some(*v),
            _ => None,
        })
        .collect()
}

// ---- variable-length quantities (SMF 1.0 section 1.1) ----------------------

/// The spec's table of quantities and their bytes: 0 is `00`, `7F` is `7F`,
/// `80` is `81 00`, `0FFFFFFF` is `FF FF FF 7F`.
const QUANTITIES: [(u32, &[u8]); 8] = [
    (0x0000_0000, &[0x00]),
    (0x0000_007F, &[0x7F]),
    (0x0000_0080, &[0x81, 0x00]),
    (0x0000_3FFF, &[0xFF, 0x7F]),
    (0x0000_4000, &[0x81, 0x80, 0x00]),
    (0x001F_FFFF, &[0xFF, 0xFF, 0x7F]),
    (0x0020_0000, &[0x81, 0x80, 0x80, 0x00]),
    (0x0FFF_FFFF, &[0xFF, 0xFF, 0xFF, 0x7F]),
];

/// A delta of each quantity reads to its value, spelled by the spec's bytes
/// and not by the support encoder.
#[test]
fn a_quantity_reads_to_its_value() {
    for (value, bytes) in QUANTITIES {
        // A delta, then a Note On: `delta 90 3C 40`.
        let body = [bytes, &[0x90, 0x3C, 0x40]].concat();
        assert_eq!(ticks(&raw_track(&body)), [u64::from(value)], "{value:#x}");
    }
}

/// A delta of each quantity is written as the spec's bytes: the shortest
/// spelling, no leading zero groups.
#[test]
fn a_quantity_writes_to_its_bytes() {
    for (value, bytes) in QUANTITIES {
        let written = MidiFile::smf(
            SmfFormat::SingleTrack,
            TPQ,
            vec![Track::new(vec![(
                Tick(u64::from(value)),
                FileEvent::Meta(Meta::Text {
                    kind: TextKind::Text,
                    bytes: Box::default(),
                }),
            )])],
        )
        .unwrap()
        .to_bytes()
        .unwrap();
        // 14-byte header, 8-byte track header, then the delta.
        assert_eq!(written.get(22..22 + bytes.len()), Some(bytes), "{value:#x}");
    }
}

/// `support`'s encoder agrees with the table (a check on the oracle).
#[test]
fn the_oracle_encoder_agrees_with_the_table() {
    for (value, bytes) in QUANTITIES {
        let mut out = Vec::new();
        write_vlq(&mut out, value);
        assert_eq!(out, bytes);
    }
}

/// A quantity over four bytes is not a quantity: refused, as a parse error
/// and not a panic or a wrapped value.
#[test]
fn a_five_byte_quantity_is_refused() {
    let body = [0x81, 0x80, 0x80, 0x80, 0x00, 0x90, 0x3C, 0x40];
    assert!(matches!(
        MidiFile::from_bytes(&raw_track(&body)),
        Err(Error::MidiFileParse(_))
    ));
    // And one cut off mid-quantity.
    assert!(matches!(
        MidiFile::from_bytes(&raw_track(&[0x81, 0x80])),
        Err(Error::MidiFileParse(_))
    ));
}

// ---- running status (SMF 1.0 section 3) ------------------------------------

/// A channel event without its status byte repeats the last one's.
#[test]
fn running_status_repeats_the_last_channel_status() {
    let bytes = file(vec![
        RawEvent::note_on(0, 2, 60, 100),
        RawEvent::running_status(10, vec![62, 90]),
        RawEvent::running_status(10, vec![64, 80]),
    ]);
    let notes: Vec<_> = voices(&bytes)
        .into_iter()
        .filter_map(|v| match v {
            Voice1::NoteOn { note, velocity } => Some((note.get(), velocity.get())),
            _ => None,
        })
        .collect();
    assert_eq!(notes, [(60, 100), (62, 90), (64, 80)]);
    assert_eq!(ticks(&bytes), [0, 10, 20]);
}

/// A meta event cancels running status: a data byte after one has none.
/// (The bytes are chosen so that if the status were `FF`, they would read as
/// a valid meta event: type `3E`, length 0.)
#[test]
fn a_meta_event_cancels_running_status() {
    let bytes = file(vec![
        RawEvent::note_on(0, 0, 60, 100),
        RawEvent::track_name(0, "x"),
        RawEvent::running_status(0, vec![0x3E, 0x00]),
    ]);
    assert!(matches!(
        MidiFile::from_bytes(&bytes),
        Err(Error::MidiFileParse(_))
    ));
}

/// A sysex event cancels running status the same way (under a surviving
/// `F0` status the byte `00` would be a valid empty sysex).
#[test]
fn a_sysex_cancels_running_status() {
    let bytes = file(vec![
        RawEvent::note_on(0, 0, 60, 100),
        RawEvent::sysex(0, &[0x7E, 0x00]),
        RawEvent::running_status(0, vec![0x00]),
    ]);
    assert!(matches!(
        MidiFile::from_bytes(&bytes),
        Err(Error::MidiFileParse(_))
    ));
}

/// A first event with no status and none running is malformed.
#[test]
fn a_first_event_needs_a_status() {
    assert!(matches!(
        MidiFile::from_bytes(&raw_track(&[0x00, 0x3C, 0x40])),
        Err(Error::MidiFileParse(_))
    ));
}

/// A status then a different channel status re-establishes: the status after
/// a Program Change (one data byte) is the Program Change's.
#[test]
fn running_status_follows_the_data_length_of_its_status() {
    let bytes = file(vec![
        RawEvent::new(0, vec![0xC1, 5]),
        RawEvent::running_status(0, vec![6]),
    ]);
    assert_eq!(voices(&bytes).len(), 2);
}

/// A real-time or system common status has no place in a file.
#[test]
fn system_statuses_are_refused() {
    for status in [0xF1u8, 0xF2, 0xF8, 0xFE] {
        assert!(
            matches!(
                MidiFile::from_bytes(&raw_track(&[0x00, status, 0x00])),
                Err(Error::MidiFileParse(_))
            ),
            "{status:#x}"
        );
    }
}

/// A data byte with its top bit set inside a channel message is malformed.
#[test]
fn a_high_data_byte_is_malformed() {
    assert!(matches!(
        MidiFile::from_bytes(&raw_track(&[0x00, 0x90, 0x3C, 0x80])),
        Err(Error::MidiFileParse(_))
    ));
}

// ---- framing ---------------------------------------------------------------

/// A chunk of an unknown type between tracks is skipped (section 2).
#[test]
fn an_alien_chunk_is_skipped() {
    let mut bytes = file(vec![RawEvent::note_on(0, 0, 60, 100)]);
    bytes.extend_from_slice(b"XFIH");
    bytes.extend_from_slice(&3u32.to_be_bytes());
    bytes.extend_from_slice(&[1, 2, 3]);
    assert_eq!(voices(&bytes).len(), 1);
}

/// A track chunk past the count the header declares disagrees with it.
#[test]
fn more_tracks_than_declared_is_malformed() {
    let two = build_smf(1, Division::Metrical(TPQ), &[vec![], vec![]]);
    let mut bytes = two.clone();
    // The header declares two (bytes 10..12); say one.
    bytes[10..12].copy_from_slice(&1u16.to_be_bytes());
    assert!(matches!(
        MidiFile::from_bytes(&bytes),
        Err(Error::MidiFileParse(_))
    ));
    assert!(MidiFile::from_bytes(&two).is_ok());
}

/// A header chunk longer than six bytes is read for its first six.
#[test]
fn a_longer_header_is_read() {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"MThd");
    bytes.extend_from_slice(&8u32.to_be_bytes());
    bytes.extend_from_slice(&[0, 0, 0, 1, 0, TPQ as u8, 0xAA, 0xBB]);
    bytes.extend_from_slice(b"MTrk");
    bytes.extend_from_slice(&4u32.to_be_bytes());
    bytes.extend_from_slice(&[0x00, 0xFF, 0x2F, 0x00]);
    assert_eq!(
        MidiFile::from_bytes(&bytes).unwrap().ticks_per_quarter(),
        TPQ
    );
}

// ---- meta events bigger than expected (SMF 1.0 section 3.3) ----------------

/// SMF 1.0 section 3.3: programs "shouldn't be surprised if it's bigger than
/// they expected. If so, they must ignore everything past what they know
/// about." A Set Tempo, a Time Signature and a Key Signature with extra data
/// are read as themselves, from the bytes they define.
#[test]
fn a_meta_event_longer_than_defined_is_read_for_what_it_defines() {
    let bytes = raw_track(&[
        0x00, 0xFF, 0x51, 0x05, 0x07, 0xA1, 0x20, 0xAA, 0xBB, // tempo + 2
        0x00, 0xFF, 0x58, 0x06, 0x06, 0x03, 0x18, 0x08, 0x03, 0x04, // 6/8 + 2
        0x00, 0xFF, 0x59, 0x03, 0xFD, 0x01, 0x99, // 3 flats minor + 1
        0x00, 0xFF, 0x2F, 0x00,
    ]);
    let file = MidiFile::from_bytes(&bytes).unwrap();
    let metas: Vec<&Meta> = file.tracks()[0]
        .events
        .iter()
        .filter_map(|(_, e)| match e {
            FileEvent::Meta(m) => Some(m),
            _ => None,
        })
        .collect();
    assert_eq!(
        metas,
        [
            &Meta::Tempo {
                micros_per_quarter: 500_000,
                extra: Box::new([0xAA, 0xBB]),
            },
            &Meta::TimeSignature {
                numerator: 6,
                denominator_power: 3,
                clocks_per_click: 24,
                thirty_seconds_per_quarter: 8,
                extra: Box::new([3, 4]),
            },
            &Meta::KeySignature {
                sharps: -3,
                minor: true,
                extra: Box::new([0x99]),
            },
        ]
    );
}

/// The file model is lossless: the bytes past what a program knows are written
/// back (SMF 1.0 section 3.3: a program "must not add anything of their own"
/// to a meta event, and so must not drop what is there).
#[test]
fn a_longer_meta_event_is_written_back_as_it_was() {
    let bytes = raw_track(&[
        0x00, 0xFF, 0x51, 0x05, 0x07, 0xA1, 0x20, 0xAA, 0xBB, //
        0x00, 0xFF, 0x58, 0x06, 0x06, 0x03, 0x18, 0x08, 0x03, 0x04, //
        0x00, 0xFF, 0x59, 0x03, 0xFD, 0x01, 0x99, //
        0x00, 0xFF, 0x2F, 0x00,
    ]);
    let back = MidiFile::from_bytes(&bytes).unwrap().to_bytes().unwrap();
    assert_eq!(back, bytes);
}

/// SMF 1.0 section 3.3: "An end-of-track event must occur as the last event in
/// the track." One with events after it is refused, not read as the track's
/// end (which would drop what follows).
#[test]
fn an_end_of_track_that_is_not_last_is_refused() {
    let bytes = raw_track(&[
        0x00, 0xFF, 0x2F, 0x00, //
        0x00, 0x90, 60, 100, //
        0x00, 0xFF, 0x2F, 0x00,
    ]);
    assert!(matches!(
        MidiFile::from_bytes(&bytes),
        Err(Error::MidiFileParse(_))
    ));
}

// ---- the writer ------------------------------------------------------------

fn note(key: u8) -> FileEvent {
    use tutti_ump::{Address, MidiChannel, NoteNumber, U7};
    FileEvent::Message(Message::Midi1(
        Address::first(MidiChannel::of(0)),
        Voice1::NoteOn {
            note: NoteNumber::of(key),
            velocity: U7::of(100),
        },
    ))
}

fn track_body(events: Vec<(u64, FileEvent)>) -> Vec<u8> {
    let track = Track::new(events.into_iter().map(|(t, e)| (Tick(t), e)).collect());
    let bytes = MidiFile::smf(SmfFormat::SingleTrack, TPQ, vec![track])
        .unwrap()
        .to_bytes()
        .unwrap();
    bytes.get(22..).unwrap().to_vec()
}

/// Two notes of one status write the status once: `00 90 3C 64`, `00 3E 64`,
/// then End of Track.
#[test]
fn the_writer_elides_a_repeated_status() {
    assert_eq!(
        track_body(vec![(0, note(60)), (0, note(62))]),
        [0x00, 0x90, 60, 100, 0x00, 62, 100, 0x00, 0xFF, 0x2F, 0x00]
    );
}

/// After a meta event the status is stated again.
#[test]
fn the_writer_restates_a_status_after_a_meta_event() {
    let meta = FileEvent::Meta(Meta::Text {
        kind: TextKind::Marker,
        bytes: Box::new(*b"m"),
    });
    assert_eq!(
        track_body(vec![(0, note(60)), (0, meta), (0, note(62))]),
        [
            0x00, 0x90, 60, 100, // note
            0x00, 0xFF, 0x06, 0x01, b'm', // marker
            0x00, 0x90, 62, 100, // note, status restated
            0x00, 0xFF, 0x2F, 0x00,
        ]
    );
}

/// Everything the SMF holds survives a write and a read, running status
/// included: equality is on the model.
#[test]
fn a_written_file_reads_back_equal() {
    let events = vec![
        (
            0,
            FileEvent::Meta(Meta::Tempo {
                micros_per_quarter: 500_000,
                extra: Box::default(),
            }),
        ),
        (0, note(60)),
        (5, note(62)),
        (200, FileEvent::SysEx(Box::new([0x7E, 0x7F]))),
        (200, FileEvent::Escape(Box::new([1, 2]))),
        (300, note(64)),
        (0x0FFF_FFFF, note(65)),
    ];
    let track = Track::new(events.into_iter().map(|(t, e)| (Tick(t), e)).collect());
    let file = MidiFile::smf(SmfFormat::Parallel, TPQ, vec![track.clone(), track]).unwrap();
    let back = MidiFile::from_bytes(&file.to_bytes().unwrap()).unwrap();
    assert_eq!(back.tracks(), file.tracks());
    assert_eq!(back.ticks_per_quarter(), TPQ);
}
