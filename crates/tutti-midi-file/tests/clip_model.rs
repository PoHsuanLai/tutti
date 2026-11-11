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
//! | the `content` check of the DCTPQ arm removed | `a_dctpq_after_a_message_is_refused`, `a_message_before_the_dctpq_is_refused` |
//! //! | the `started` check of the Start of Clip arm removed | `a_second_start_of_clip_is_refused` |
//! | `ProfileScan::accepts` refuses every packet | `set_profile_on_before_the_dctpq_is_accepted` |
//! | the header branch (`if !started`) drops the message instead of keeping it | `a_header_with_program_change_cc_and_sysex_round_trips` |
//! | `write` puts the leading Set Profile On packets after the DCTPQ | `set_profile_on_before_the_dctpq_is_accepted` |
//! | `header_rules` without the `tempo` duplicate check | `two_header_set_tempos_are_refused` |
//! | `header_rules` without the `i != 0 \|\| *tick != 0` check | `a_header_set_tempo_not_first_or_not_at_delta_zero_is_misplaced` |
//! | `header_rules` without the `signature` duplicate check | `two_header_time_signatures_are_refused` |
//! | `header_rules` `i != usize::from(tempo)` replaced by `false` | `a_header_time_signature_must_follow_the_set_tempo_immediately` |
//! | `tempo_map` ignores `track.header` | `the_header_set_tempo_is_the_tempo_from_the_start_of_the_sequence` |
//! | `MidiFile::build` skips `header_rules` | `the_constructor_holds_a_header_to_the_same_rules` |
//! | `push_delta` writes the restart's DCS as `DCS_MAX - 1`, or no NOOP | `a_long_gap_is_written_as_dcs_then_a_null` |
//! | the `taken < words.len()` of the End of Clip arm removed | `data_after_end_of_clip_is_refused` |
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

// ---- the configuration header (M2-116 sections 6 to 6.1.2) ----------------

fn sysex7(status: tutti_ump::SysexStatus, data: &[u8]) -> Ump {
    Message::Sysex7(
        MidiGroup::FIRST,
        tutti_ump::Sysex7Packet::new(status, data).unwrap(),
    )
    .into()
}

/// A Set Profile On: Universal SysEx `7E <device> 0D 22 ...` (MIDI-CI).
fn set_profile_on() -> Ump {
    sysex7(
        tutti_ump::SysexStatus::Complete,
        &[0x7E, 0x7F, 0x0D, 0x22, 1, 2],
    )
}

fn program_change() -> Ump {
    Message::Midi1(
        Address::first(MidiChannel::FIRST),
        Voice1::ProgramChange(U7::of(5)),
    )
    .into()
}

fn time_signature() -> Ump {
    Message::Flex(
        MidiGroup::FIRST,
        Flex::SetTimeSignature(tutti_ump::TimeSignature {
            numerator: 3,
            denominator: 2,
            thirty_seconds: 8,
        }),
    )
    .into()
}

/// A clip whose header after the DCTPQ is `header` (each behind a DCS of 0
/// unless it is a `dcs`), then Start of Clip, a note and End of Clip.
fn with_header(header: &[Ump]) -> Vec<Ump> {
    let mut parts = vec![dcs(0), dctpq(TPQ)];
    parts.extend_from_slice(header);
    parts.extend([
        dcs(0),
        start(),
        dcs(0),
        on(60, 1),
        dcs(10),
        off(60),
        dcs(0),
        end(),
    ]);
    parts
}

/// Section 6: Set Profile On messages come first, carry no DCS, and the DCTPQ
/// follows them. Kept in the header, written back before the DCTPQ.
#[test]
fn set_profile_on_before_the_dctpq_is_accepted() {
    let two_packets = [
        sysex7(
            tutti_ump::SysexStatus::Start,
            &[0x7E, 0x7F, 0x0D, 0x22, 1, 2],
        ),
        sysex7(tutti_ump::SysexStatus::End, &[3, 4]),
    ];
    let mut parts = vec![set_profile_on(), two_packets[0], two_packets[1]];
    parts.extend(with_header(&[]));
    let file = MidiFile::from_bytes(&clip(&parts)).unwrap();
    let header = &file.tracks()[0].header;
    assert_eq!(header.len(), 3);
    assert!(header.iter().all(|(t, _)| *t == Tick(0)));
    // The write puts them before the DCTPQ again, with no DCS between.
    let written = file.to_bytes().unwrap();
    let mut expected = MAGIC.to_vec();
    for ump in [
        set_profile_on(),
        two_packets[0],
        two_packets[1],
        dcs(0),
        dctpq(TPQ),
    ] {
        for w in ump.words() {
            expected.extend_from_slice(&w.to_be_bytes());
        }
    }
    assert!(written.starts_with(&expected));
    assert_eq!(MidiFile::from_bytes(&written).unwrap(), file);
}

/// Anything else before the DCTPQ is not allowed: a note, or a SysEx that is
/// not a Set Profile On.
#[test]
fn a_message_before_the_dctpq_is_refused() {
    let mut note_first = vec![dcs(0), on(60, 1)];
    note_first.extend(with_header(&[]));
    assert_eq!(clip_error(&note_first), ClipFileError::DctpqNotFirst);
    let other_sysex = sysex7(tutti_ump::SysexStatus::Complete, &[0x7E, 0x7F, 0x06, 0x01]);
    let mut parts = vec![other_sysex];
    parts.extend(with_header(&[]));
    assert_eq!(clip_error(&parts), ClipFileError::DctpqNotFirst);
}

/// Section 6: the header may hold other messages that configure a receiver,
/// each behind its own DCS. They are in the model, in order with their delta
/// times, are not events of the sequence, and write back.
#[test]
fn a_header_with_program_change_cc_and_sysex_round_trips() {
    let control = Ump::from(Message::Midi2(
        Address::first(MidiChannel::FIRST),
        tutti_ump::Voice2::ControlChange {
            controller: tutti_ump::CCNumber::of(7),
            value: 0x8000_0000,
        },
    ));
    let sysex = sysex7(tutti_ump::SysexStatus::Complete, &[0x7D, 1, 2, 3]);
    let file = MidiFile::from_bytes(&clip(&with_header(&[
        dcs(0),
        program_change(),
        dcs(10),
        control,
        dcs(5),
        sysex,
    ])))
    .unwrap();
    let track = &file.tracks()[0];
    let ticks: Vec<u64> = track.header.iter().map(|(t, _)| t.0).collect();
    assert_eq!(ticks, [0, 10, 15]);
    assert_eq!(
        track.header[2].1,
        FileEvent::Message(tutti_ump::decode(sysex).unwrap())
    );
    // Not on the sequence's timeline, not sounding at bar 1.
    assert_eq!(track.events.len(), 2);
    assert_eq!(file.events().unwrap().count(), 2);
    assert_eq!(
        MidiFile::from_bytes(&file.to_bytes().unwrap()).unwrap(),
        file
    );
}

#[test]
fn two_header_set_tempos_are_refused() {
    let parts = with_header(&[dcs(0), set_tempo(100.0), dcs(0), set_tempo(90.0)]);
    assert_eq!(clip_error(&parts), ClipFileError::DuplicateHeaderTempo);
}

/// Section 6.1.1: the Set Tempo "shall" be the first message after the DCTPQ,
/// at a delta of 0 (or sharing the DCTPQ's DCS, which is also 0).
#[test]
fn a_header_set_tempo_not_first_or_not_at_delta_zero_is_misplaced() {
    let after_other = with_header(&[dcs(0), program_change(), dcs(0), set_tempo(100.0)]);
    assert_eq!(
        clip_error(&after_other),
        ClipFileError::MisplacedHeaderTempo
    );
    let late = with_header(&[dcs(7), set_tempo(100.0)]);
    assert_eq!(clip_error(&late), ClipFileError::MisplacedHeaderTempo);
    // Sharing the DCTPQ's DCS (no DCS of its own) is first at delta 0.
    let sharing = with_header(&[set_tempo(100.0)]);
    assert!(MidiFile::from_bytes(&clip(&sharing)).is_ok());
}

#[test]
fn two_header_time_signatures_are_refused() {
    let parts = with_header(&[dcs(0), time_signature(), dcs(0), time_signature()]);
    assert_eq!(
        clip_error(&parts),
        ClipFileError::DuplicateHeaderTimeSignature
    );
}

/// Section 6.1.2: the Set Time Signature follows the Set Tempo immediately (or
/// is first when there is no Set Tempo).
#[test]
fn a_header_time_signature_must_follow_the_set_tempo_immediately() {
    let behind_other = with_header(&[
        dcs(0),
        set_tempo(100.0),
        dcs(0),
        program_change(),
        dcs(0),
        time_signature(),
    ]);
    assert_eq!(
        clip_error(&behind_other),
        ClipFileError::MisplacedHeaderTimeSignature
    );
    let before_tempo = with_header(&[dcs(0), time_signature(), dcs(0), set_tempo(100.0)]);
    // The tempo is then not first.
    assert_eq!(
        clip_error(&before_tempo),
        ClipFileError::MisplacedHeaderTempo
    );
    let after_other_no_tempo = with_header(&[dcs(0), program_change(), dcs(0), time_signature()]);
    assert_eq!(
        clip_error(&after_other_no_tempo),
        ClipFileError::MisplacedHeaderTimeSignature
    );
    for ok in [
        with_header(&[dcs(0), set_tempo(100.0), dcs(0), time_signature()]),
        with_header(&[dcs(0), time_signature()]),
    ] {
        assert!(MidiFile::from_bytes(&clip(&ok)).is_ok());
    }
}

/// **The header's Set Tempo is the tempo from the start of the sequence**: the
/// header's clock runs before Start of Clip, so its tempo is at sequence tick
/// 0, and a Set Tempo in the sequence is an ordinary change after it.
#[test]
fn the_header_set_tempo_is_the_tempo_from_the_start_of_the_sequence() {
    let q = u32::from(TPQ);
    let mut parts = vec![
        dcs(0),
        dctpq(TPQ),
        dcs(0),
        set_tempo(100.0),
        dcs(0),
        start(),
    ];
    parts.extend([dcs(q), set_tempo(50.0), dcs(q), on(60, 1), dcs(0), end()]);
    let file = MidiFile::from_bytes(&clip(&parts)).unwrap();
    let map = file.tempo_map();
    assert_eq!(map.tempo_at(Beat(0.5)), Bpm::of(100.0));
    assert_eq!(map.tempo_at(Beat(1.5)), Bpm::of(50.0));
    // One beat at 100 BPM (0.6 s) and one at 50 (1.2 s).
    let close = |a: Seconds, b: f64| assert!((a.0 - b).abs() < 1e-9, "{a:?} vs {b}");
    close(file.seconds_at(Beat(2.0)), 1.8);
    assert_eq!(file.tracks()[0].header.len(), 1);
}

/// The constructor holds a header to the rules the reader does, and an SMF has
/// none.
#[test]
fn the_constructor_holds_a_header_to_the_same_rules() {
    let tempo = |bpm| (Tick(0), packet(tutti_ump::decode(set_tempo(bpm)).unwrap()));
    let twice = Track {
        header: vec![tempo(100.0), tempo(90.0)],
        ..Track::default()
    };
    assert!(matches!(
        MidiFile::clip(TPQ, twice.clone()),
        Err(Error::Clip(ClipFileError::DuplicateHeaderTempo))
    ));
    let once = Track {
        header: vec![tempo(100.0)],
        ..Track::default()
    };
    let file = MidiFile::clip(TPQ, once.clone()).unwrap();
    assert_eq!(
        MidiFile::from_bytes(&file.to_bytes().unwrap()).unwrap(),
        file
    );
    assert!(matches!(
        MidiFile::smf(SmfFormat::SingleTrack, TPQ, vec![once]),
        Err(Error::InvalidConfig(_))
    ));
}

/// Section 3.2.2: a gap past 1,048,575 ticks is written as a DCS of 1,048,575
/// followed by a Null message, once per restart, then the remainder's DCS.
#[test]
fn a_long_gap_is_written_as_dcs_then_a_null() {
    let gap: u32 = 2 * 0x000F_FFFF + 3;
    let track = track(vec![
        (0, packet(tutti_ump::decode(on(60, 1)).unwrap())),
        (u64::from(gap), packet(tutti_ump::decode(off(60)).unwrap())),
    ]);
    let bytes = MidiFile::clip(TPQ, track).unwrap().to_bytes().unwrap();
    let words: Vec<u32> = bytes[8..]
        .chunks(4)
        .map(|c| u32::from_be_bytes(c.try_into().unwrap()))
        .collect();
    let (max, noop) = (dcs(0x000F_FFFF).words()[0], 0u32);
    let expect = [max, noop, max, noop, dcs(3).words()[0]];
    assert!(
        words.windows(5).any(|w| w == expect),
        "no DCS, Null, DCS, Null, DCS run in {words:08x?}"
    );
}

// ---- M2-116 conformance of the reader and the writer (doc 017 L1) -----------

/// The packets of a written clip after the file header, in order.
fn packets_of(bytes: &[u8]) -> Vec<Ump> {
    let words: Vec<u32> = bytes[8..]
        .chunks(4)
        .map(|c| u32::from_be_bytes(c.try_into().unwrap()))
        .collect();
    tutti_ump::split_stream(&words).collect()
}

fn is_dcs(ump: Ump) -> bool {
    matches!(
        tutti_ump::decode(ump),
        Ok(Message::Utility(Utility::DeltaClockstamp(_)))
    )
}

/// A MIDI-CI message `7E 7F 0D <sub_id2> ...` as one System Exclusive packet.
fn ci(sub_id2: u8, status: tutti_ump::SysexStatus) -> Ump {
    sysex7(status, &[0x7E, 0x7F, 0x0D, sub_id2, 0, 0])
}

/// Section 3.2.1 and section 6: "The DCTPQ shall have a preceding Delta
/// Clockstamp (DCS) with Number of Ticks Since Last Event set to 0x00000."
#[test]
fn the_dctpq_has_a_zero_delta_clockstamp_before_it() {
    let tail = [dcs(0), start(), dcs(0), end()];
    let with = |before: &[Ump]| -> Vec<Ump> {
        let mut parts = before.to_vec();
        parts.push(dctpq(TPQ));
        parts.extend_from_slice(&tail);
        parts
    };
    assert!(MidiFile::from_bytes(&clip(&with(&[dcs(0)]))).is_ok());
    assert_eq!(clip_error(&with(&[])), ClipFileError::DctpqDelta);
    assert_eq!(clip_error(&with(&[dcs(5)])), ClipFileError::DctpqDelta);
    // It follows the Set Profile On messages, and its DCS is the one after them.
    assert!(MidiFile::from_bytes(&clip(&with(&[set_profile_on(), dcs(0)]))).is_ok());
    assert_eq!(
        clip_error(&with(&[set_profile_on(), dcs(2)])),
        ClipFileError::DctpqDelta
    );
}

/// Section 6: "Set Profile On messages shall not have a prepended Delta
/// Clockstamp (DCS)."
#[test]
fn a_set_profile_on_has_no_delta_clockstamp_before_it() {
    let after = [dcs(0), dctpq(TPQ), dcs(0), start(), dcs(0), end()];
    let with = |before: &[Ump]| -> Vec<Ump> {
        let mut parts = before.to_vec();
        parts.extend_from_slice(&after);
        parts
    };
    assert!(MidiFile::from_bytes(&clip(&with(&[set_profile_on()]))).is_ok());
    assert_eq!(
        clip_error(&with(&[dcs(0), set_profile_on()])),
        ClipFileError::ProfileOnDelta
    );
    // Nor before a later packet of one message.
    let two = [
        sysex7(
            tutti_ump::SysexStatus::Start,
            &[0x7E, 0x7F, 0x0D, 0x22, 1, 2],
        ),
        sysex7(tutti_ump::SysexStatus::End, &[3, 4]),
    ];
    assert!(MidiFile::from_bytes(&clip(&with(&two))).is_ok());
    assert_eq!(
        clip_error(&with(&[two[0], dcs(0), two[1]])),
        ClipFileError::ProfileOnDelta
    );
}

/// Section 6.1.2: the header's Set Time Signature "shall use the Delta
/// Clockstamp which precedes the DCTPQ message or shall have a preceding Delta
/// Clockstamp with Number of Ticks Since Last Event set to 0x00000", as the Set
/// Tempo does (section 6.1.1).
#[test]
fn the_header_time_signature_is_at_delta_zero() {
    for header in [
        vec![dcs(5), time_signature()],
        vec![dcs(0), set_tempo(100.0), dcs(5), time_signature()],
    ] {
        assert_eq!(
            clip_error(&with_header(&header)),
            ClipFileError::MisplacedHeaderTimeSignature,
            "{header:?}"
        );
    }
    // The constructor holds the model to the same rule.
    let late = Track {
        header: vec![(
            Tick(5),
            packet(tutti_ump::decode(time_signature()).unwrap()),
        )],
        header_end: Tick(5),
        ..Track::default()
    };
    assert!(matches!(
        MidiFile::clip(TPQ, late),
        Err(Error::Clip(ClipFileError::MisplacedHeaderTimeSignature))
    ));
}

/// Section 6.4: "Property Data for PE Resources shall not be included in a MIDI
/// Clip File." Section 7.2: the sequence data holds "any MIDI messages in UMP
/// Format, except Property Exchange messages". Property Exchange is MIDI-CI
/// sub-ID#2 `0x30` to `0x3F` (M2-101 Table 5), so its neighbours are fine.
#[test]
fn property_exchange_is_refused_everywhere() {
    use tutti_ump::SysexStatus::{Complete, Start};
    for sub_id2 in [0x30, 0x34, 0x3F] {
        for status in [Complete, Start] {
            let pe = ci(sub_id2, status);
            // In the sequence data...
            let in_sequence = [
                dcs(0),
                dctpq(TPQ),
                dcs(0),
                start(),
                dcs(0),
                pe,
                dcs(0),
                end(),
            ];
            assert_eq!(
                clip_error(&in_sequence),
                ClipFileError::PropertyExchange,
                "{sub_id2:#x}"
            );
            // ...and in the header.
            assert_eq!(
                clip_error(&with_header(&[dcs(0), pe])),
                ClipFileError::PropertyExchange
            );
            // The writer's gate refuses it too.
            let track = Track::new(vec![(Tick(0), packet(tutti_ump::decode(pe).unwrap()))]);
            assert!(
                matches!(MidiFile::clip(TPQ, track), Err(Error::InvalidConfig(_))),
                "{sub_id2:#x}"
            );
            let header = Track {
                header: vec![(Tick(0), packet(tutti_ump::decode(pe).unwrap()))],
                ..Track::default()
            };
            assert!(matches!(
                MidiFile::clip(TPQ, header),
                Err(Error::InvalidConfig(_))
            ));
        }
    }
    // Profile Configuration (0x2F) and Process Inquiry (0x40) are not it.
    for sub_id2 in [0x22, 0x2F, 0x40] {
        let other = ci(sub_id2, Complete);
        let parts = [
            dcs(0),
            dctpq(TPQ),
            dcs(0),
            start(),
            dcs(0),
            other,
            dcs(0),
            end(),
        ];
        assert!(MidiFile::from_bytes(&clip(&parts)).is_ok(), "{sub_id2:#x}");
    }
}

/// Section 6.1: "the cumulative values of all Delta Clockstamp up to and
/// including the Delta Clockstamp for the Start of Clip Message determines the
/// total length of time allocated to the Clip Configuration Header." The last
/// gap is kept, read and written.
#[test]
fn the_header_keeps_its_duration() {
    // A header message at 4, then 26 more ticks to Start of Clip: it ends at 30.
    let parts = [
        dcs(0),
        dctpq(TPQ),
        dcs(4),
        program_change(),
        dcs(26),
        start(),
        dcs(0),
        end(),
    ];
    let file = MidiFile::from_bytes(&clip(&parts)).unwrap();
    let track = &file.tracks()[0];
    assert_eq!(track.header.len(), 1);
    assert_eq!(track.header[0].0, Tick(4));
    assert_eq!(track.header_end, Tick(30));
    // Written back with the same Delta Clockstamp before Start of Clip.
    let bytes = file.to_bytes().unwrap();
    let written = packets_of(&bytes);
    let at = written.iter().position(|&u| u == start()).unwrap();
    assert_eq!(written[at - 1], dcs(26));
    assert_eq!(MidiFile::from_bytes(&bytes).unwrap(), file);
    // A header with no message has its duration too.
    let empty = [dcs(0), dctpq(TPQ), dcs(12), start(), dcs(0), end()];
    let file = MidiFile::from_bytes(&clip(&empty)).unwrap();
    assert_eq!(file.tracks()[0].header_end, Tick(12));
    assert_eq!(
        MidiFile::from_bytes(&file.to_bytes().unwrap()).unwrap(),
        file
    );
    // None, as in `valid`, is zero.
    let file = MidiFile::from_bytes(&clip(&valid())).unwrap();
    assert_eq!(file.tracks()[0].header_end, Tick(0));
}

#[test]
fn the_constructor_holds_the_header_duration_to_the_header() {
    let message = (
        Tick(8),
        packet(tutti_ump::decode(program_change()).unwrap()),
    );
    let before = Track {
        header: vec![message.clone()],
        header_end: Tick(7),
        ..Track::default()
    };
    assert!(matches!(
        MidiFile::clip(TPQ, before),
        Err(Error::InvalidConfig(_))
    ));
    let at = Track {
        header: vec![message],
        header_end: Tick(8),
        ..Track::default()
    };
    assert!(MidiFile::clip(TPQ, at.clone()).is_ok());
    // An SMF has no configuration header, so no duration.
    let smf = Track {
        header_end: Tick(1),
        ..Track::default()
    };
    assert!(matches!(
        MidiFile::smf(SmfFormat::SingleTrack, TPQ, vec![smf]),
        Err(Error::InvalidConfig(_))
    ));
}

fn text_packets(kind: tutti_ump::FlexTextKind, text: &str) -> Vec<(Tick, FileEvent)> {
    tutti_ump::flex_text_packets(MidiGroup::FIRST, tutti_ump::FlexAddress::Group, kind, text)
        .unwrap()
        .map(|ump| (Tick(10), packet(tutti_ump::decode(ump).unwrap())))
        .collect()
}

/// M2-104 sections 7.5.10 and 7.5.12: "When placed in a MIDI Clip File, all parts
/// of the Lyric Data Message shall be placed at the same time, sharing a single
/// Delta Clock Stamp", and M2-116 section 3.2.2: a Delta Clockstamp "declares the
/// time of all following messages which occur before the next Delta Clockstamp
/// message". The parts of a Lyric or Ruby message are written behind one DCS;
/// the parts of any other message, and separate messages, keep their own.
#[test]
fn the_parts_of_a_lyric_or_ruby_message_share_one_delta_clockstamp() {
    use tutti_ump::FlexTextKind::{Composer, Lyrics, Ruby};
    // 20 bytes: a Start and an End packet (12 bytes each).
    let syllable = "abcdefghijklmnopqrst";
    for (kind, shared) in [(Lyrics, true), (Ruby, true), (Composer, false)] {
        let events = text_packets(kind, syllable);
        assert_eq!(events.len(), 2);
        let bytes = MidiFile::clip(TPQ, Track::new(events.clone()))
            .unwrap()
            .to_bytes()
            .unwrap();
        let written = packets_of(&bytes);
        let first = written
            .iter()
            .position(|&u| u == Ump::from(*events[0].1.message().unwrap()))
            .unwrap();
        assert_eq!(
            is_dcs(written[first + 1]),
            !shared,
            "{kind:?}: a DCS between the parts"
        );
        assert!(
            is_dcs(written[first - 1]),
            "{kind:?}: a DCS before the first"
        );
        // Either way it reads back as the same events at the same ticks.
        let back = MidiFile::from_bytes(&bytes).unwrap();
        assert_eq!(back.tracks()[0].events, events, "{kind:?}");
    }
    // Two separate Lyric messages at one tick are two messages: each its own DCS.
    let mut two = text_packets(Lyrics, "one");
    two.extend(text_packets(Lyrics, "two"));
    let bytes = MidiFile::clip(TPQ, Track::new(two.clone()))
        .unwrap()
        .to_bytes()
        .unwrap();
    let written = packets_of(&bytes);
    let first = written
        .iter()
        .position(|&u| u == Ump::from(*two[0].1.message().unwrap()))
        .unwrap();
    assert!(is_dcs(written[first + 1]));
    assert_eq!(
        MidiFile::from_bytes(&bytes).unwrap().tracks()[0].events,
        two
    );
}
