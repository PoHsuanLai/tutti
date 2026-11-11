//! The MIDI 2.0 Clip File half of [`MidiFile`], against streams of UMP words
//! laid out by hand from M2-116 (the structure rules are about *order*, which
//! only a hand-built stream can get wrong on purpose).
//!
//! Mutations run (each planted, seen to fail, restored), against
//! `clip_file.rs` unless a file is named:
//!
//! | mutation | fails |
//! |---|---|
//! | the `ticks_per_quarter.is_some()` check of the DCTPQ arm removed | `a_second_dctpq_is_refused` |
//! | the `content` check of the DCTPQ arm removed | `a_dctpq_after_a_message_is_refused` |
//! | the `started` check before an event removed | `an_event_before_start_of_clip_is_refused` |
//! | the `started` check of the Start of Clip arm removed | `a_second_start_of_clip_is_refused` |
//! | the `taken < words.len()` check of the End of Clip arm removed | `data_after_end_of_clip_is_refused` |
//! | `MissingStartOfClip` check removed | `a_clip_without_start_of_clip_is_refused` |
//! | `end` taken as the last event's tick | `the_clip_length_is_the_delta_before_end_of_clip` |
//! | `push_delta` writes no NOOP restarts (clamps to `DCS_MAX`) | `a_gap_past_the_dcs_field_round_trips` |
//! | `write` skips `Unknown` events (`file.rs`, `to_bytes`) | `a_packet_no_definition_covers_survives` |
//! | `tempo_map` reads the Flex tempo of a track as SMF meta only (`file.rs`) | `the_tempo_map_is_the_set_tempo_messages` |
//! | `Track::notes` treats a MIDI 2.0 velocity-0 Note On as a start (`velocity >= 0`) | `a_midi2_note_on_at_velocity_zero_is_neither_start_nor_end` |
//! | `Track::notes` drops the group from the pairing key | `a_note_off_only_closes_its_own_group` |
//! | `build` accepts `Meta` in a clip (`refuses` arm removed) | `what_a_clip_cannot_hold_is_refused` |

use tutti_fixtures::midi::{note_off, note_on};
use tutti_midi_file::{
    ClipFileError, Error, FileEvent, Meta, MidiFile, MidiFileKind, SmfFormat, Tick, Track,
};
use tutti_types::{Beat, Bpm, Seconds};
use tutti_ump::{
    Address, DeltaTicks, Flex, Message, MidiChannel, MidiGroup, NoteNumber, Stream, U7, Ump,
    Utility, Voice1,
};

const TPQ: u16 = 96;
const MAGIC: &[u8; 8] = b"SMF2CLIP";

fn utility(u: Utility) -> Ump {
    Message::Utility(u).into()
}
fn dcs(ticks: u32) -> Ump {
    utility(Utility::DeltaClockstamp(DeltaTicks::masked(ticks)))
}
fn dctpq(tpq: u16) -> Ump {
    utility(Utility::TicksPerQuarterNote(tpq))
}
fn start() -> Ump {
    Message::Stream(Stream::StartOfClip).into()
}
fn end() -> Ump {
    Message::Stream(Stream::EndOfClip).into()
}
fn on(note: u8, vel: u16) -> Ump {
    note_on(MidiGroup::FIRST, MidiChannel::FIRST, note, vel)
}
fn off(note: u8) -> Ump {
    note_off(MidiGroup::FIRST, MidiChannel::FIRST, note, 0)
}
fn set_tempo(bpm: f64) -> Ump {
    Message::Flex(MidiGroup::FIRST, Flex::SetTempo((6.0e9 / bpm) as u32)).into()
}

/// The bytes of a clip made of exactly `parts`, in order.
fn clip(parts: &[Ump]) -> Vec<u8> {
    let mut out = MAGIC.to_vec();
    for ump in parts {
        for w in ump.words() {
            out.extend_from_slice(&w.to_be_bytes());
        }
    }
    out
}

/// A valid clip: DCTPQ, Start of Clip, a note a beat long, End of Clip a beat
/// after it.
fn valid() -> Vec<Ump> {
    vec![
        dcs(0),
        dctpq(TPQ),
        dcs(0),
        start(),
        dcs(0),
        on(60, 0xABCD),
        dcs(u32::from(TPQ)),
        off(60),
        dcs(u32::from(TPQ)),
        end(),
    ]
}

fn clip_error(parts: &[Ump]) -> ClipFileError {
    match MidiFile::from_bytes(&clip(parts)) {
        Err(Error::Clip(e)) => e,
        other => panic!("expected a clip error, got {other:?}"),
    }
}

fn packet(m: Message) -> FileEvent {
    FileEvent::Message(m)
}

#[test]
fn a_clip_reads_at_absolute_ticks() {
    let file = MidiFile::from_bytes(&clip(&valid())).unwrap();
    assert_eq!(file.kind(), MidiFileKind::ClipFile);
    assert_eq!(file.smf_format(), None);
    assert_eq!(file.ticks_per_quarter(), TPQ);
    let events = &file.tracks()[0].events;
    assert_eq!(events.len(), 2, "structure is not an event");
    assert_eq!(events[0].0, Tick(0));
    assert_eq!(events[1].0, Tick(96));
    assert_eq!(
        events[0].1,
        FileEvent::Message(tutti_ump::decode(on(60, 0xABCD)).unwrap())
    );
    assert_eq!(file.beat_at(events[1].0), Beat(1.0));
    // The clip's length is the delta before End of Clip.
    assert_eq!(file.end_beat(), Beat(2.0));
    // And a clip's notes keep all 16 bits.
    let notes: Vec<_> = file.tracks()[0].notes().collect();
    assert_eq!(notes.len(), 1);
    assert_eq!((notes[0].start, notes[0].end), (Tick(0), Tick(96)));
    assert_eq!(notes[0].velocity, 0xABCD);
}

/// **The clip's length is the Delta Clockstamp before End of Clip**, so a clip
/// that ends a bar after its last note is a bar long, and says so after a
/// round trip.
#[test]
fn the_clip_length_is_the_delta_before_end_of_clip() {
    let mut parts = valid();
    let n = parts.len();
    parts[n - 2] = dcs(4 * u32::from(TPQ));
    let file = MidiFile::from_bytes(&clip(&parts)).unwrap();
    assert_eq!(file.tracks()[0].end, Tick(5 * u64::from(TPQ)));
    let again = MidiFile::from_bytes(&file.to_bytes().unwrap()).unwrap();
    assert_eq!(again, file);
}

/// The lossless round trip: every packet, the Flex messages, a packet no
/// definition covers, and a gap past the 20-bit DCS field (restarted by NOOPs,
/// M2-116 section 3.2.2) all come back.
#[test]
fn a_gap_past_the_dcs_field_round_trips() {
    let gap: u32 = 3 * 0x000F_FFFF + 17; // three restarts and a remainder
    let parts = [
        dcs(0),
        dctpq(480),
        dcs(0),
        start(),
        dcs(0),
        set_tempo(120.0),
        dcs(0),
        on(60, 1),
        // A DCS is 20 bits: this gap is NOOP restarts, then a count.
        dcs(0x000F_FFFF),
        utility(Utility::Noop),
        dcs(0x000F_FFFF),
        utility(Utility::Noop),
        dcs(0x000F_FFFF),
        utility(Utility::Noop),
        dcs(17),
        off(60),
        dcs(0),
        end(),
    ];
    let file = MidiFile::from_bytes(&clip(&parts)).unwrap();
    assert_eq!(file.tracks()[0].events[2].0, Tick(u64::from(gap)));
    let written = file.to_bytes().unwrap();
    assert_eq!(MidiFile::from_bytes(&written).unwrap(), file);
}

#[test]
fn a_packet_no_definition_covers_survives() {
    // Message type 0x6 is reserved: a 32-bit packet nothing decodes.
    let reserved = Ump([0x6001_2345, 0, 0, 0]);
    let mut parts = valid();
    parts.insert(6, reserved);
    parts.insert(6, dcs(0));
    let file = MidiFile::from_bytes(&clip(&parts)).unwrap();
    let kept = file.tracks()[0]
        .events
        .iter()
        .any(|(_, e)| matches!(e, FileEvent::Unknown(u) if u.0 == reserved));
    assert!(kept, "{:?}", file.tracks()[0].events);
    assert_eq!(
        MidiFile::from_bytes(&file.to_bytes().unwrap()).unwrap(),
        file
    );
}

/// **The tempo map is the Set Tempo messages** (Flex Data, M2-116 section
/// 7.1.1): 120 BPM then 60 from beat 4, beat to seconds exact, and a clip with
/// none plays at the default 120.
#[test]
fn the_tempo_map_is_the_set_tempo_messages() {
    let q = u32::from(TPQ);
    let file = MidiFile::from_bytes(&clip(&[
        dcs(0),
        dctpq(TPQ),
        dcs(0),
        start(),
        dcs(0),
        set_tempo(120.0),
        dcs(4 * q),
        set_tempo(60.0),
        dcs(4 * q),
        end(),
    ]))
    .unwrap();
    let close = |a: Seconds, b: f64| assert!((a.0 - b).abs() < 1e-9, "{a:?} vs {b}");
    close(file.seconds_at(Beat(4.0)), 2.0);
    close(file.seconds_at(Beat(8.0)), 6.0);
    assert_eq!(file.tempo_map().tempo_at(Beat(5.0)), Bpm::of(60.0));
    // The messages are events, kept.
    assert_eq!(file.tracks()[0].events.len(), 2);

    let bare = MidiFile::from_bytes(&clip(&valid())).unwrap();
    assert_eq!(bare.tempo_map().constant_tempo(), Some(Bpm::of(120.0)));
}

// ---- structure (M2-116 sections 6 and 7): one error variant per rule ------

#[test]
fn a_dctpq_after_a_message_is_refused() {
    // Start of Clip, then the DCTPQ: the DCTPQ is not first.
    let parts = [dcs(0), start(), dcs(0), dctpq(TPQ), dcs(0), end()];
    assert_eq!(clip_error(&parts), ClipFileError::DctpqNotFirst);
}

#[test]
fn a_second_dctpq_is_refused() {
    let mut parts = valid();
    parts.splice(2..2, [dcs(0), dctpq(TPQ)]);
    assert_eq!(clip_error(&parts), ClipFileError::DuplicateDctpq);
}

#[test]
fn an_event_before_start_of_clip_is_refused() {
    let parts = [
        dcs(0),
        dctpq(TPQ),
        dcs(0),
        on(60, 1),
        dcs(0),
        start(),
        dcs(0),
        end(),
    ];
    assert_eq!(clip_error(&parts), ClipFileError::EventBeforeStartOfClip);
}

#[test]
fn a_second_start_of_clip_is_refused() {
    let mut parts = valid();
    parts.splice(4..4, [dcs(0), start()]);
    assert_eq!(clip_error(&parts), ClipFileError::DuplicateStartOfClip);
}

#[test]
fn data_after_end_of_clip_is_refused() {
    let mut parts = valid();
    parts.extend([dcs(0), on(61, 1)]);
    assert_eq!(clip_error(&parts), ClipFileError::TrailingData);
}

#[test]
fn a_clip_without_a_dctpq_is_refused() {
    let parts = [dcs(0), start(), dcs(0), end()];
    assert_eq!(clip_error(&parts), ClipFileError::MissingDctpq);
}

#[test]
fn a_zero_dctpq_is_refused() {
    let mut parts = valid();
    parts[1] = dctpq(0);
    assert_eq!(clip_error(&parts), ClipFileError::ZeroDctpq);
}

#[test]
fn a_clip_without_start_of_clip_is_refused() {
    let parts = [dcs(0), dctpq(TPQ), dcs(0), end()];
    assert_eq!(clip_error(&parts), ClipFileError::MissingStartOfClip);
}

#[test]
fn a_clip_without_end_of_clip_is_refused() {
    let mut parts = valid();
    parts.truncate(parts.len() - 2);
    assert_eq!(clip_error(&parts), ClipFileError::MissingEndOfClip);
}

/// Timing and padding are not structure: a Delta Clockstamp, a Null and JR
/// messages may come before the DCTPQ and Start of Clip.
#[test]
fn clockstamps_and_jr_messages_do_not_count_as_content() {
    let mut parts = vec![utility(Utility::Noop), utility(Utility::JrClock(7)), dcs(5)];
    parts.extend(valid());
    assert!(MidiFile::from_bytes(&clip(&parts)).is_ok());
}

#[test]
fn the_magic_alignment_and_a_cut_packet_are_refused() {
    let good = clip(&valid());
    assert!(matches!(
        MidiFile::from_bytes(b"SMF2CLI"),
        Err(Error::UnknownFormat)
    ));
    let mut odd = good.clone();
    odd.push(0);
    assert!(matches!(
        MidiFile::from_bytes(&odd),
        Err(Error::Clip(ClipFileError::Unaligned))
    ));
    // End of Clip is a 128-bit message: cut it to one word.
    let cut = &good[..good.len() - 12];
    assert!(matches!(
        MidiFile::from_bytes(cut),
        Err(Error::Clip(ClipFileError::Truncated))
    ));
}

/// **No strict prefix of a valid clip is itself a clip, and none panics.**
#[test]
fn no_prefix_of_a_valid_clip_is_accepted() {
    let bytes = clip(&valid());
    assert!(MidiFile::from_bytes(&bytes).is_ok());
    for n in 0..bytes.len() {
        assert!(
            MidiFile::from_bytes(&bytes[..n]).is_err(),
            "the first {n} of {} bytes were accepted",
            bytes.len()
        );
    }
}

// ---- building a clip ------------------------------------------------------

fn track(events: Vec<(u64, FileEvent)>) -> Track {
    Track::new(events.into_iter().map(|(t, e)| (Tick(t), e)).collect())
}

#[test]
fn what_a_clip_cannot_hold_is_refused() {
    let one = |event| MidiFile::clip(TPQ, track(vec![(0, event)]));
    assert!(matches!(
        one(FileEvent::SysEx(vec![1].into())),
        Err(Error::InvalidConfig(_))
    ));
    assert!(matches!(
        one(FileEvent::Meta(Meta::tempo(Bpm::of(120.0)))),
        Err(Error::InvalidConfig(_))
    ));
    // The file's own structure is not an event.
    for structure in [
        Message::Stream(Stream::StartOfClip),
        Message::Stream(Stream::EndOfClip),
        Message::Utility(Utility::Noop),
        Message::Utility(Utility::TicksPerQuarterNote(1)),
    ] {
        assert!(matches!(
            one(packet(structure)),
            Err(Error::InvalidConfig(_))
        ));
    }
    // A gap past a u32.
    let far = MidiFile::clip(
        TPQ,
        track(vec![(
            u64::from(u32::MAX) + 1,
            packet(tutti_ump::decode(on(60, 1)).unwrap()),
        )]),
    );
    assert!(matches!(far, Err(Error::InvalidConfig(_))));
    assert!(matches!(
        MidiFile::clip(0, Track::default()),
        Err(Error::InvalidConfig(_))
    ));
}

// ---- notes ----------------------------------------------------------------

fn notes_of(events: Vec<(u64, Ump)>) -> Vec<tutti_midi_file::PairedNote> {
    let events = events
        .into_iter()
        .map(|(t, u)| (t, packet(tutti_ump::decode(u).unwrap())))
        .collect();
    MidiFile::clip(TPQ, track(events)).unwrap().tracks()[0]
        .notes()
        .collect()
}

#[test]
fn a_note_off_only_closes_its_own_group() {
    let g1 = MidiGroup::new(1).unwrap();
    let notes = notes_of(vec![
        (0, on(60, 100)),
        (10, note_off(g1, MidiChannel::FIRST, 60, 0)),
        (20, off(60)),
    ]);
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0].end, Tick(20));
}

/// A MIDI 2.0 Note On at velocity 0 is a note (M2-104 section 7.4.2), and
/// neither starts nor ends one here.
#[test]
fn a_midi2_note_on_at_velocity_zero_is_neither_start_nor_end() {
    let notes = notes_of(vec![(0, on(60, 100)), (10, on(60, 0)), (20, off(60))]);
    assert_eq!(notes.len(), 1);
    assert_eq!((notes[0].start, notes[0].end), (Tick(0), Tick(20)));
}

/// An SMF and a Clip File holding the same MIDI 1.0 messages pair the same
/// notes: the file layer keeps the vocabulary one.
#[test]
fn an_smf_and_a_clip_of_the_same_messages_pair_the_same_notes() {
    let message = |on: bool| {
        let (note, velocity) = (NoteNumber::of(64), U7::of(90));
        Message::Midi1(
            Address::first(MidiChannel::FIRST),
            if on {
                Voice1::NoteOn { note, velocity }
            } else {
                Voice1::NoteOff { note, velocity }
            },
        )
    };
    let events = || vec![(0, packet(message(true))), (96, packet(message(false)))];
    let smf = MidiFile::smf(SmfFormat::SingleTrack, TPQ, vec![track(events())]).unwrap();
    let clip = MidiFile::clip(TPQ, track(events())).unwrap();
    let read = |f: &MidiFile| MidiFile::from_bytes(&f.to_bytes().unwrap()).unwrap();
    let (a, b) = (read(&smf), read(&clip));
    assert_eq!(
        a.tracks()[0].notes().collect::<Vec<_>>(),
        b.tracks()[0].notes().collect::<Vec<_>>()
    );
    assert_eq!(a.tracks()[0].events[0], b.tracks()[0].events[0]);
}
