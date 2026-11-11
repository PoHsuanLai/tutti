//! `MidiFile::to_clip`: a Standard MIDI File as a MIDI 2.0 Clip File, each
//! mapping checked against figures written out from the specifications (SMF
//! 1.0 section 3.3 for the meta events, M2-116 section 3.3, 7.1.1, 7.1.2 and
//! Appendix B for what replaces them, M2-104 section 7.5 for the Flex fields).
//!
//! Mutations run (each planted in `src/file/to_clip.rs` unless a file is
//! named, seen to fail, restored):
//!
//! | mutation | fails |
//! |---|---|
//! | `TEN_NS_PER_MICRO` = 10 | `a_set_tempo_is_ten_ns_units_per_quarter` |
//! | `key_signature` `rem_euclid(7)` as `% 7` | `a_key_signature_is_the_sharps_and_the_tonic` (flat keys) |
//! | `key_signature` ignores `minor` | `a_key_signature_is_the_sharps_and_the_tonic` |
//! | `TextKind::Copyright` mapped to `Lyrics` | `copyright_and_lyrics_are_flex_text_over_several_packets` |
//! | the text's packets cut to the first (`take(1)`) | `copyright_and_lyrics_are_flex_text_over_several_packets` |
//! | the SysEx body's `0xF0` prefix left off | `a_complete_sysex_is_sysex7_packets` |
//! | the merge sort ties by reversed track | `format_1_merges_by_tick_ties_in_track_order` |
//! | a format 2 file let through | `format_2_is_refused` |
//! | a clip returned as it is | `a_clip_is_not_converted_again` |
//! | every event ranked `Other` (tick 0 not ordered) | `the_first_tempo_and_signature_lead_the_clip` |
//! | the default tempo always added | `a_tempo_at_tick_zero_is_not_defaulted` |
//! | the default signature never added | `a_file_with_no_tempo_or_signature_gets_the_smf_defaults` |
//! | the end taken as the shortest track's (`min`) | `the_division_and_the_end_carry_over` |
//! | the division passed as 96 | `a_file_with_no_tempo_or_signature_gets_the_smf_defaults`, `the_division_and_the_end_carry_over` |
//! | `0x20` not a `ReplacedByAddressing` | `what_has_no_successor_is_reported_with_its_tick` |
//! | the `track == 0` test of the name dropped | `the_first_track_names_the_sequence_and_the_others_are_reported` |
//! | the UTF-8 check as `from_utf8_lossy` | `text_that_flex_cannot_carry_is_reported` |
//! | the whole-note refusal removed | `what_has_no_successor_is_reported_with_its_tick` |
//! | `flex_text_packets` without its NUL check (`tutti-ump`) | `a_nul_is_refused` (there) |
//!
//! The round trip (`the_clip_round_trips_through_its_bytes`) is a property of
//! the codec the conversion feeds; it is pinned by the clip codec's own
//! mutations in `clip_model.rs`, and here it checks that the Flex and SysEx7
//! packets the conversion makes survive them.

use tutti_midi_file::{
    DropReason, Error, FileEvent, Meta, MidiFile, SmfFormat, TextKind, Tick, ToClipError, Track,
};
use tutti_types::{Beat, Seconds};
use tutti_ump::{
    Address, Flex, FlexAddress, FlexTextKind, Form, KeySignature, Letter, Message, Metronome,
    MidiChannel, MidiGroup, NoteNumber, SharpsFlats, SysexStatus, TimeSignature, U7, Voice1,
};

const TPQ: u16 = 480;

fn note(channel: u8, key: u8) -> FileEvent {
    FileEvent::Message(Message::Midi1(
        Address::first(MidiChannel::new(channel).unwrap()),
        Voice1::NoteOn {
            note: NoteNumber::of(key),
            velocity: U7::of(100),
        },
    ))
}

fn meta(m: Meta) -> FileEvent {
    FileEvent::Meta(m)
}

fn text(kind: TextKind, s: &str) -> FileEvent {
    meta(Meta::text(kind, s))
}

fn flex(f: Flex) -> FileEvent {
    FileEvent::Message(Message::Flex(MidiGroup::FIRST, f))
}

fn track(events: Vec<(u64, FileEvent)>) -> Track {
    Track::new(events.into_iter().map(|(t, e)| (Tick(t), e)).collect())
}

fn format0(events: Vec<(u64, FileEvent)>) -> MidiFile {
    MidiFile::smf(SmfFormat::SingleTrack, TPQ, vec![track(events)]).unwrap()
}

fn events_of(file: &MidiFile) -> &[(Tick, FileEvent)] {
    &file.tracks()[0].events
}

const DEFAULT_TEMPO: u32 = 50_000_000;

fn signature_events(n: u8, d: u8, bb: u8, cc: u8) -> [FileEvent; 2] {
    [
        flex(Flex::SetTimeSignature(TimeSignature {
            numerator: n,
            denominator: d,
            thirty_seconds: bb,
        })),
        flex(Flex::SetMetronome(Metronome {
            clocks_per_click: cc,
            accent_1: 0,
            accent_2: 0,
            accent_3: 0,
            subdivision_1: 0,
            subdivision_2: 0,
        })),
    ]
}

/// SMF 1.0: 500000 us per quarter is 120 bpm; M2-104 section 7.5.3: the field
/// is 10 ns units per quarter, so 0.5 s is 50,000,000.
#[test]
fn a_set_tempo_is_ten_ns_units_per_quarter() {
    let smf = format0(vec![
        (
            0,
            meta(Meta::Tempo {
                micros_per_quarter: 500_000,
            }),
        ),
        (0, note(0, 60)),
    ]);
    let clip = smf.to_clip().unwrap().clip;
    assert_eq!(
        events_of(&clip)[0],
        (Tick(0), flex(Flex::SetTempo(50_000_000)))
    );
    // The widest the wire holds still fits the field: 0xFFFFFF us is 1_677_721_500.
    let slow = format0(vec![(
        0,
        meta(Meta::Tempo {
            micros_per_quarter: 0xFF_FFFF,
        }),
    )]);
    let clip = slow.to_clip().unwrap().clip;
    assert_eq!(events_of(&clip)[0].1, flex(Flex::SetTempo(1_677_721_500)));
}

/// SMF 1.0: `58 04 06 03 18 08` is 6/8, 24 MIDI clocks a click, 8 32nds per
/// quarter. M2-104 sections 7.5.4 and 7.5.5: the signature carries numerator,
/// denominator power and the 32nds; the metronome the clocks per click, and
/// 0 for the accents and subdivisions SMF has no field for.
#[test]
fn a_time_signature_is_a_signature_and_a_metronome() {
    let smf = format0(vec![
        (
            0,
            meta(Meta::Tempo {
                micros_per_quarter: 500_000,
            }),
        ),
        (
            0,
            meta(Meta::TimeSignature {
                numerator: 6,
                denominator_power: 3,
                clocks_per_click: 36,
                thirty_seconds_per_quarter: 8,
            }),
        ),
    ]);
    let events = smf.to_clip().unwrap().clip;
    let [_, a, b] = events_of(&events) else {
        panic!("{:?}", events_of(&events));
    };
    let [sig, metronome] = signature_events(6, 3, 8, 36);
    assert_eq!((&a.1, &b.1), (&sig, &metronome));
}

/// M2-104 section 7.5.7: sharps or flats, and the tonic's letter. 3 sharps
/// minor is F sharp minor (the relative minor of A major), so the tonic is F;
/// 0 major is C, 2 flats major is B flat, 3 flats minor is C minor, 7 sharps
/// major is C sharp.
#[test]
fn a_key_signature_is_the_sharps_and_the_tonic() {
    for (sharps, minor, tonic) in [
        (3, true, Letter::F),
        (0, false, Letter::C),
        (-2, false, Letter::B),
        (-3, true, Letter::C),
        (7, false, Letter::C),
        (0, true, Letter::A),
    ] {
        let smf = format0(vec![(0, meta(Meta::KeySignature { sharps, minor }))]);
        let clip = smf.to_clip().unwrap().clip;
        let key = events_of(&clip)
            .iter()
            .find_map(|(_, e)| match e {
                FileEvent::Message(Message::Flex(_, Flex::SetKeySignature(a, k))) => Some((*a, *k)),
                _ => None,
            })
            .unwrap();
        assert_eq!(
            key,
            (
                FlexAddress::Group,
                KeySignature {
                    sharps_flats: SharpsFlats::new(sharps),
                    tonic: Some(tonic),
                }
            ),
            "{sharps} {minor}"
        );
    }
}

/// The Flex text of each packet, with its place in the message.
fn flex_texts(file: &MidiFile) -> Vec<(FlexTextKind, Form, Vec<u8>)> {
    events_of(file)
        .iter()
        .filter_map(|(_, e)| match e {
            FileEvent::Message(Message::Flex(
                _,
                Flex::Text {
                    kind, form, bytes, ..
                },
            )) => Some((*kind, *form, bytes.to_vec())),
            _ => None,
        })
        .collect()
}

/// M2-116 Appendix B: Copyright Notice and Lyrics. M2-104 section 7.5.9: 12
/// bytes a packet, Start, Continue, End, zero padding.
#[test]
fn copyright_and_lyrics_are_flex_text_over_several_packets() {
    let notice = "(c) 2026 Someone Else"; // 21 bytes: 12 + 9
    let smf = format0(vec![
        (0, text(TextKind::Copyright, notice)),
        (10, text(TextKind::Lyric, "la")),
    ]);
    let clip = smf.to_clip().unwrap().clip;
    let got = flex_texts(&clip);
    let mut padded = b"Someone Else".to_vec();
    padded.truncate(12);
    assert_eq!(
        got,
        [
            (
                FlexTextKind::Copyright,
                Form::Start,
                b"(c) 2026 Som".to_vec()
            ),
            (
                FlexTextKind::Copyright,
                Form::End,
                b"eone Else\0\0\0".to_vec()
            ),
            (
                FlexTextKind::Lyrics,
                Form::Complete,
                b"la\0\0\0\0\0\0\0\0\0\0".to_vec()
            ),
        ]
    );
    let ticks: Vec<u64> = events_of(&clip)
        .iter()
        .filter(|(_, e)| matches!(e, FileEvent::Message(Message::Flex(_, Flex::Text { .. }))))
        .map(|(t, _)| t.0)
        .collect();
    assert_eq!(ticks, [0, 0, 10]);
}

/// M2-104 section 7.7: seven bytes are a Start of six and an End of one.
#[test]
fn a_complete_sysex_is_sysex7_packets() {
    let smf = format0(vec![(
        5,
        FileEvent::SysEx(vec![0x7E, 1, 2, 3, 4, 5, 6, 0xF7].into()),
    )]);
    let converted = smf.to_clip().unwrap();
    assert!(converted.dropped.is_empty());
    let packets: Vec<_> = events_of(&converted.clip)
        .iter()
        .filter_map(|(t, e)| match e {
            FileEvent::Message(Message::Sysex7(_, p)) => Some((t.0, p.status(), p.data().to_vec())),
            _ => None,
        })
        .collect();
    assert_eq!(
        packets,
        [
            (5, SysexStatus::Start, vec![0x7E, 1, 2, 3, 4, 5]),
            (5, SysexStatus::End, vec![6]),
        ]
    );
}

#[test]
fn a_sysex_that_is_not_one_complete_message_is_reported() {
    let divided = FileEvent::SysEx(vec![0x7E, 1, 2].into()); // no F7: continued by an F7 event
    let eight_bit = FileEvent::SysEx(vec![0x7E, 0x80, 0xF7].into());
    let smf = format0(vec![(0, divided.clone()), (1, eight_bit.clone())]);
    let converted = smf.to_clip().unwrap();
    let report: Vec<_> = converted
        .dropped
        .iter()
        .map(|d| (d.tick.0, d.event.clone(), d.reason))
        .collect();
    assert_eq!(
        report,
        [
            (0, divided, DropReason::SysexNotCompleteSevenBit),
            (1, eight_bit, DropReason::SysexNotCompleteSevenBit),
        ]
    );
}

/// The merge rule of `events`, on the clip: by tick, ties in track order then
/// the track's own order.
#[test]
fn format_1_merges_by_tick_ties_in_track_order() {
    let a = (note(0, 60), note(0, 61)); // track 0, both at 100
    let b = note(1, 62); // track 1 at 100
    let c = note(1, 63); // track 1 at 50
    let smf = MidiFile::smf(
        SmfFormat::Parallel,
        TPQ,
        vec![
            track(vec![(100, a.0.clone()), (100, a.1.clone())]),
            track(vec![(50, c.clone()), (100, b.clone())]),
        ],
    )
    .unwrap();
    let clip = smf.to_clip().unwrap().clip;
    let notes: Vec<(u64, FileEvent)> = events_of(&clip)
        .iter()
        .filter(|(_, e)| matches!(e, FileEvent::Message(Message::Midi1(..))))
        .map(|(t, e)| (t.0, e.clone()))
        .collect();
    assert_eq!(notes, [(50, c), (100, a.0), (100, a.1), (100, b)]);
    // The end is the longest track's.
    assert_eq!(clip.tracks()[0].end, Tick(100));
}

#[test]
fn format_2_is_refused() {
    let smf = MidiFile::smf(
        SmfFormat::Sequential,
        TPQ,
        vec![track(vec![(0, note(0, 60))]), track(vec![(0, note(0, 61))])],
    )
    .unwrap();
    assert!(matches!(
        smf.to_clip(),
        Err(Error::ToClip(ToClipError::Sequential))
    ));
}

#[test]
fn a_clip_is_not_converted_again() {
    let clip = format0(vec![(0, note(0, 60))]).to_clip().unwrap().clip;
    assert!(matches!(
        clip.to_clip(),
        Err(Error::ToClip(ToClipError::AlreadyClip))
    ));
}

/// M2-116 sections 7.1.1 and 7.1.2: the first Set Tempo, then the first Set
/// Time Signature, lead the sequence at its start, whatever order the SMF had.
#[test]
fn the_first_tempo_and_signature_lead_the_clip() {
    let smf = MidiFile::smf(
        SmfFormat::Parallel,
        TPQ,
        vec![
            track(vec![(0, note(0, 60))]),
            track(vec![
                (
                    0,
                    meta(Meta::TimeSignature {
                        numerator: 3,
                        denominator_power: 2,
                        clocks_per_click: 24,
                        thirty_seconds_per_quarter: 8,
                    }),
                ),
                (
                    0,
                    meta(Meta::Tempo {
                        micros_per_quarter: 1_000_000,
                    }),
                ),
            ]),
        ],
    )
    .unwrap();
    let clip = smf.to_clip().unwrap().clip;
    let [sig, metronome] = signature_events(3, 2, 8, 24);
    let want = [
        flex(Flex::SetTempo(100_000_000)),
        sig,
        metronome,
        note(0, 60),
    ];
    let got: Vec<_> = events_of(&clip).iter().map(|(_, e)| e.clone()).collect();
    assert_eq!(got, want);
}

/// SMF 1.0 section 3.3: with no Set Tempo 120 bpm is assumed, and with no Time
/// Signature 4/4 (24 clocks, 8 32nds). The clip says it (section 7.1.1).
#[test]
fn a_file_with_no_tempo_or_signature_gets_the_smf_defaults() {
    let smf = format0(vec![
        (0, note(0, 60)),
        (
            960,
            meta(Meta::Tempo {
                micros_per_quarter: 250_000,
            }),
        ),
    ]);
    let clip = smf.to_clip().unwrap().clip;
    let [sig, metronome] = signature_events(4, 2, 8, 24);
    let got: Vec<_> = events_of(&clip)
        .iter()
        .map(|(t, e)| (t.0, e.clone()))
        .collect();
    assert_eq!(
        got,
        [
            (0, flex(Flex::SetTempo(DEFAULT_TEMPO))),
            (0, sig),
            (0, metronome),
            (0, note(0, 60)),
            // The later tempo stays where it was: not moved to a clock grid.
            (960, flex(Flex::SetTempo(25_000_000))),
        ]
    );
    // Two beats at the default 120 bpm (1 s), then two at 240 bpm (0.5 s).
    assert_eq!(clip.seconds_at(Beat(4.0)), Seconds(1.5));
}

#[test]
fn a_tempo_at_tick_zero_is_not_defaulted() {
    let smf = format0(vec![(
        0,
        meta(Meta::Tempo {
            micros_per_quarter: 1_000_000,
        }),
    )]);
    let clip = smf.to_clip().unwrap().clip;
    let tempos: Vec<_> = events_of(&clip)
        .iter()
        .filter_map(|(_, e)| match e {
            FileEvent::Message(Message::Flex(_, Flex::SetTempo(t))) => Some(*t),
            _ => None,
        })
        .collect();
    assert_eq!(tempos, [100_000_000]);
}

#[test]
fn the_division_and_the_end_carry_over() {
    let smf = MidiFile::smf(
        SmfFormat::Parallel,
        96,
        vec![
            Track {
                header: vec![],
                events: vec![(Tick(10), note(0, 60))],
                end: Tick(400),
            },
            Track {
                header: vec![],
                events: vec![(Tick(10), note(1, 60))],
                end: Tick(250),
            },
        ],
    )
    .unwrap();
    let clip = smf.to_clip().unwrap().clip;
    assert_eq!(clip.ticks_per_quarter(), 96);
    assert_eq!(clip.tracks()[0].end, Tick(400));
    assert!(clip.tracks()[0].header.is_empty());
}

/// Each event with no UMP form, with its tick and reason, in clip order.
#[test]
fn what_has_no_successor_is_reported_with_its_tick() {
    let other = |kind: u8| {
        meta(Meta::Other {
            kind,
            data: vec![1, 2].into(),
        })
    };
    let escape = FileEvent::Escape(vec![0xF3, 1].into());
    let whole_note = meta(Meta::TimeSignature {
        numerator: 4,
        denominator_power: 0,
        clocks_per_click: 24,
        thirty_seconds_per_quarter: 8,
    });
    let events = vec![
        (0, other(0x00)), // sequence number
        (0, other(0x20)), // channel prefix
        (5, other(0x21)), // port prefix
        (7, text(TextKind::Marker, "A")),
        (7, text(TextKind::CuePoint, "door")),
        (8, text(TextKind::InstrumentName, "piano")),
        (8, text(TextKind::ProgramName, "p")),
        (9, text(TextKind::DeviceName, "d")),
        (10, other(0x54)), // SMPTE offset
        (11, other(0x7F)), // sequencer specific
        (12, escape.clone()),
        (13, whole_note.clone()),
        (
            14,
            meta(Meta::KeySignature {
                sharps: 8,
                minor: false,
            }),
        ),
        (15, other(0x30)), // unregistered
    ];
    let smf = format0(events.clone());
    let converted = smf.to_clip().unwrap();
    let reasons: Vec<(u64, DropReason)> = converted
        .dropped
        .iter()
        .map(|d| (d.tick.0, d.reason))
        .collect();
    use DropReason::*;
    assert_eq!(
        reasons,
        [
            (0, NoSuccessor),
            (0, ReplacedByAddressing),
            (5, ReplacedByAddressing),
            (7, NoSuccessor),
            (7, NoSuccessor),
            (8, NoSuccessor),
            (8, NoSuccessor),
            (9, NoSuccessor),
            (10, NoSuccessor),
            (11, NoSuccessor),
            (12, NoSuccessor),
            (13, WholeNoteDenominator),
            (14, KeySignatureOutOfRange),
            (15, NoSuccessor),
        ]
    );
    // The report holds the event itself.
    assert_eq!(converted.dropped[10].event, escape);
    assert_eq!(converted.dropped[11].event, whole_note);
    // The dropped signature was at tick 13, so the clip's only one is the
    // default 4/4 at tick 0 (the file declared none at its start).
    let signatures: Vec<_> = events_of(&converted.clip)
        .iter()
        .filter(|(_, e)| {
            matches!(
                e,
                FileEvent::Message(Message::Flex(_, Flex::SetTimeSignature(_)))
            )
        })
        .map(|(t, _)| t.0)
        .collect();
    assert_eq!(signatures, [0]);
}

/// SMF 1.0 section 3.3: in a format 0 track or the first track of format 1 a
/// Track Name is the sequence's name (M2-116 Appendix B: Composition Name);
/// any other is a track's, which a clip has none of.
#[test]
fn the_first_track_names_the_sequence_and_the_others_are_reported() {
    let smf = MidiFile::smf(
        SmfFormat::Parallel,
        TPQ,
        vec![
            track(vec![(0, text(TextKind::TrackName, "Sonata"))]),
            track(vec![(0, text(TextKind::TrackName, "Violin"))]),
        ],
    )
    .unwrap();
    let converted = smf.to_clip().unwrap();
    assert_eq!(
        flex_texts(&converted.clip),
        [(
            FlexTextKind::CompositionName,
            Form::Complete,
            b"Sonata\0\0\0\0\0\0".to_vec()
        )]
    );
    assert_eq!(converted.dropped.len(), 1);
    assert_eq!(converted.dropped[0].track, 1);
    assert_eq!(converted.dropped[0].reason, DropReason::TrackName);
    // Format 0: its one track's name is the sequence's.
    let zero = format0(vec![(0, text(TextKind::TrackName, "Solo"))]);
    assert_eq!(
        flex_texts(&zero.to_clip().unwrap().clip)[0].0,
        FlexTextKind::CompositionName
    );
}

/// Flex text is UTF-8 and zero-padded (M2-104 section 7.5.9); a generic text
/// event is Unknown Metadata Text.
#[test]
fn text_that_flex_cannot_carry_is_reported() {
    let latin1 = FileEvent::Meta(Meta::Text {
        kind: TextKind::Lyric,
        bytes: vec![b'c', b'a', b'f', 0xE9].into(),
    });
    let nul = FileEvent::Meta(Meta::Text {
        kind: TextKind::Copyright,
        bytes: b"a\0b".to_vec().into(),
    });
    let smf = format0(vec![
        (0, latin1),
        (0, nul),
        (0, text(TextKind::Text, "remark")),
    ]);
    let converted = smf.to_clip().unwrap();
    let reasons: Vec<_> = converted.dropped.iter().map(|d| d.reason).collect();
    assert_eq!(reasons, [DropReason::TextNotUtf8, DropReason::TextHasNul]);
    assert_eq!(
        flex_texts(&converted.clip),
        [(
            FlexTextKind::UnknownMetadata,
            Form::Complete,
            b"remark\0\0\0\0\0\0".to_vec()
        )]
    );
}

/// The clip written and read back is the same clip.
#[test]
fn the_clip_round_trips_through_its_bytes() {
    let smf = MidiFile::smf(
        SmfFormat::Parallel,
        TPQ,
        vec![
            track(vec![
                (
                    0,
                    meta(Meta::Tempo {
                        micros_per_quarter: 500_000,
                    }),
                ),
                (
                    0,
                    text(TextKind::Copyright, "a rather long copyright notice"),
                ),
                (0, note(0, 60)),
                (480, note(0, 62)),
                (
                    960,
                    FileEvent::SysEx(vec![0x7E, 1, 2, 3, 4, 5, 6, 7, 0xF7].into()),
                ),
            ]),
            track(vec![
                (
                    0,
                    meta(Meta::KeySignature {
                        sharps: -3,
                        minor: true,
                    }),
                ),
                (480, note(1, 64)),
            ]),
        ],
    )
    .unwrap();
    let clip = smf.to_clip().unwrap().clip;
    let again = MidiFile::from_bytes(&clip.to_bytes().unwrap()).unwrap();
    assert_eq!(again, clip);
}
