//! The Standard MIDI File half of [`MidiFile`], against bytes built from the
//! SMF 1.0 spec (`support`: an encoder and a decoder written from it, sharing
//! no code with the crate, so they are the second opinion on the bytes).
//!
//! **The lossless round trip is the test**: a file read and written holds the
//! same events, including SysEx and meta; running status and event spelling may
//! differ, so equality is on the model and on the independent decoder's
//! reading of the bytes, never on the bytes.
//!
//! Mutations run (each planted, seen to fail, restored), against `smf/read.rs`,
//! `smf/write.rs` and `file.rs` unless a file is named (the wire's own are in
//! `smf_wire.rs`):
//!
//! | mutation | fails |
//! |---|---|
//! | `read.rs` `event` reads an `F7` escape as a `SysEx` | `every_event_kind_survives_a_read_and_a_write` |
//! | `read.rs` `track` ignores the final End of Track (`if is_end(e)` made false) | `the_end_of_a_track_is_kept`, `every_event_kind_survives_a_read_and_a_write` |
//! | `write.rs` `append` writes each delta from tick 0 instead of the previous event | `every_event_kind_survives_a_read_and_a_write`, `a_zero_tempo_is_not_a_tempo_and_an_absurd_one_is_read_as_written` |
//! | `read.rs` `read` without the fewer-tracks-than-declared check | `a_file_missing_a_track_is_truncated`, `no_prefix_of_a_valid_file_is_accepted` |
//! | `read.rs` `steps` swallows an event error (`map_while` keeps only the `Ok`s) | `a_track_that_ends_mid_event_is_a_parse_error` |
//! | `read.rs` `division` accepts an SMPTE division as ticks | `smpte_division_is_refused` |
//! | `read.rs` `division` accepts zero | `a_zero_division_is_refused` |
//! | `tempo_map` drops the `default` point at tick 0 | `a_file_with_no_tempo_plays_at_120` (and every file with no tempo at tick 0: `TempoError::FirstNotAtZero`) |
//! | `tempo_map` keeps the first of two tempos on one tick | `the_later_of_two_tempos_on_one_tick_governs` |
//! | `tempo_map` without the sort by tick | `the_tempo_map_pools_every_track` |
//! | `Meta::bpm` reads a zero tempo as 1 BPM | `a_zero_tempo_is_not_a_tempo_and_an_absurd_one_is_read_as_written` |
//! | `events` skips the stable sort | `merged_events_are_in_time_order_ties_in_track_order` |
//! | `events` merges a format 2 file | `format_2_keeps_its_sequences_apart` |
//! | `tempo_maps` of a format 2 file built from all tracks | `format_2_keeps_its_sequences_apart` |
//! | `Track::notes` pops from the front (FIFO) | `overlapping_same_key_notes_pair_last_on_first_off` |
//! | `Track::notes` drops the channel from the key | `a_note_off_does_not_close_another_channels_note` |
//! | (the velocity-0 Note On is `Ump::key_event`'s rule, in `tutti-ump`: no mutation here reaches it) | `a_velocity_zero_note_on_closes_the_note` |
//! | `build` skips the order check | `events_out_of_order_are_refused` |
//! | `build` skips the format 0 track count | `a_format_0_file_has_one_track` |
//! | `Meta::tempo` without the clamp to 1 | `a_tempo_above_the_wire_range_writes_the_fastest_quarter` |

mod support;

use support::{Division, RawEvent, build_smf, decode_smf};
use tutti_midi_file::{
    Error, FileEvent, Meta, MidiFile, MidiFileKind, PairedNote, SmfFormat, TextKind, Tick, Track,
};
use tutti_types::{Beat, Bpm, Seconds};
use tutti_ump::{Address, Message, MidiChannel, MidiGroup, NoteNumber, U7, U14, Voice1};

const TPQ: u16 = 480;
const BEAT: u32 = TPQ as u32;

fn smf(format: u16, tracks: &[Vec<RawEvent>]) -> Vec<u8> {
    build_smf(format, Division::Metrical(TPQ), tracks)
}

fn voice(channel: u8, voice: Voice1) -> FileEvent {
    FileEvent::Message(Message::Midi1(
        Address::first(MidiChannel::new(channel).unwrap()),
        voice,
    ))
}

fn on(channel: u8, key: u8, vel: u8) -> FileEvent {
    voice(
        channel,
        Voice1::NoteOn {
            note: NoteNumber::of(key),
            velocity: U7::of(vel),
        },
    )
}

fn off(channel: u8, key: u8, vel: u8) -> FileEvent {
    voice(
        channel,
        Voice1::NoteOff {
            note: NoteNumber::of(key),
            velocity: U7::of(vel),
        },
    )
}

fn read(bytes: &[u8]) -> MidiFile {
    MidiFile::from_bytes(bytes).expect("reads")
}

/// A file one of every event the SMF has, in a track.
fn everything() -> Vec<RawEvent> {
    let meta = |delta, kind: u8, data: &[u8]| {
        let mut v = vec![0xFF, kind, data.len() as u8];
        v.extend_from_slice(data);
        RawEvent::new(delta, v)
    };
    let mut escape = vec![0xF7, 3];
    escape.extend_from_slice(&[0x01, 0x02, 0x03]);
    vec![
        meta(0, 0x00, &[0, 5]),                 // sequence number
        meta(0, 0x01, b"text"),                 // text
        meta(0, 0x02, b"(c) nobody"),           // copyright
        RawEvent::track_name(0, "Lead"),        // track name
        meta(0, 0x04, b"flute"),                // instrument name
        meta(0, 0x05, b"la"),                   // lyric
        meta(0, 0x06, b"Verse"),                // marker
        meta(0, 0x07, b"door slams"),           // cue point
        meta(0, 0x08, b"patch"),                // program name
        meta(0, 0x09, b"port a"),               // device name
        meta(0, 0x20, &[3]),                    // channel prefix
        meta(0, 0x21, &[1]),                    // port
        RawEvent::tempo(0, 500_000),            // 120 BPM
        meta(0, 0x54, &[0x21, 30, 15, 10, 50]), // SMPTE offset (25 fps, 1:30:15:10.50)
        RawEvent::time_signature(0, 6, 3),      // 6/8
        meta(0, 0x59, &[0xFD, 1]),              // three flats, minor
        meta(0, 0x7F, &[0x7D, 1, 2]),           // sequencer specific
        meta(0, 0x60, &[9, 9]),                 // an unregistered type
        RawEvent::note_on(0, 0, 60, 100),
        // Running status: the same status, omitted.
        RawEvent::running_status(0, [62, 90]),
        RawEvent::new(10, [0xA0, 60, 33]),    // poly pressure
        RawEvent::new(0, [0xB1, 7, 99]),      // controller
        RawEvent::new(0, [0xC2, 40]),         // program change
        RawEvent::new(0, [0xD3, 50]),         // channel pressure
        RawEvent::new(0, [0xE4, 0x01, 0x40]), // pitch bend
        RawEvent::sysex(10, &[0x7E, 0x7F, 0x09, 0x01]),
        RawEvent::sysex(0, &[0x11; 300]), // a length needing two VLQ bytes
        RawEvent::new(0, escape),         // an F7 escape
        RawEvent::note_off(BEAT, 0, 60, 64),
        RawEvent::note_off(0, 0, 62, 0),
        meta(0, 0x06, b"end"),
    ]
}

/// **Every event kind of the SMF is read, and survives a write.**
///
/// What the first read must say (the figures are the bytes above), then
/// that writing it and reading that back is the same file, and that an
/// independent decoder reads the same events in the original and the rewrite
/// (running status made explicit by it).
#[test]
fn every_event_kind_survives_a_read_and_a_write() {
    for format in [0u16, 1, 2] {
        let tracks = if format == 0 {
            vec![everything()]
        } else {
            vec![everything(), vec![RawEvent::note_on(0, 1, 70, 80)]]
        };
        let bytes = smf(format, &tracks);
        let file = read(&bytes);
        assert_eq!(file.kind(), MidiFileKind::StandardMidiFile);
        assert_eq!(file.ticks_per_quarter(), TPQ);

        let events = &file.tracks()[0].events;
        let at = |i: usize| &events[i];
        assert_eq!(
            at(0).1,
            FileEvent::Meta(Meta::Other {
                kind: 0,
                data: vec![0, 5].into()
            })
        );
        assert_eq!(at(1).1, FileEvent::Meta(Meta::text(TextKind::Text, "text")));
        assert_eq!(file.tracks()[0].name(), Some("Lead"));
        assert_eq!(at(5).1, FileEvent::Meta(Meta::text(TextKind::Lyric, "la")));
        assert_eq!(
            at(6).1,
            FileEvent::Meta(Meta::text(TextKind::Marker, "Verse"))
        );
        assert_eq!(
            at(12).1,
            FileEvent::Meta(Meta::Tempo {
                micros_per_quarter: 500_000,
                extra: Box::default(),
            })
        );
        assert_eq!(
            at(13).1,
            FileEvent::Meta(Meta::Other {
                kind: 0x54,
                data: vec![0x21, 30, 15, 10, 50].into()
            })
        );
        assert_eq!(
            at(14).1,
            FileEvent::Meta(Meta::TimeSignature {
                numerator: 6,
                denominator_power: 3,
                clocks_per_click: 24,
                thirty_seconds_per_quarter: 8,
                extra: Box::default(),
            })
        );
        assert_eq!(
            at(15).1,
            FileEvent::Meta(Meta::KeySignature {
                sharps: -3,
                minor: true,
                extra: Box::default(),
            })
        );
        assert_eq!(at(18).1, on(0, 60, 100));
        // Running status read as the message it stands for.
        assert_eq!(at(19).1, on(0, 62, 90));
        assert_eq!(
            at(20),
            &(
                Tick(10),
                voice(
                    0,
                    Voice1::PolyPressure {
                        note: NoteNumber::of(60),
                        pressure: U7::of(33)
                    }
                )
            )
        );
        assert_eq!(
            at(24).1,
            voice(4, Voice1::PitchBend(U14::of(0x40 << 7 | 0x01)))
        );
        assert_eq!(
            at(25).1,
            FileEvent::SysEx(vec![0x7E, 0x7F, 0x09, 0x01, 0xF7].into())
        );
        let FileEvent::SysEx(long) = &at(26).1 else {
            panic!("a SysEx");
        };
        assert_eq!(long.len(), 301);
        assert_eq!(at(27).1, FileEvent::Escape(vec![1, 2, 3].into()));
        assert_eq!(at(28).0, Tick(20 + u64::from(BEAT)));
        assert_eq!(
            file.smf_format(),
            Some(match format {
                0 => SmfFormat::SingleTrack,
                1 => SmfFormat::Parallel,
                _ => SmfFormat::Sequential,
            })
        );

        let written = file.to_bytes().expect("writes");
        assert_eq!(read(&written), file, "format {format}");
        // The second opinion: the same events, read by a decoder that shares
        // nothing with the crate.
        assert_eq!(
            decode_smf(&written)
                .expect("a foreign reader accepts it")
                .tracks,
            decode_smf(&bytes).expect("and the original").tracks,
            "format {format}"
        );
    }
}

/// The track's length is its End of Track, which may sit after the last event.
///
/// Mutation: see the table (`end` = last event tick).
#[test]
fn the_end_of_a_track_is_kept() {
    // `build_smf` appends an End of Track at delta 0; this one is a bar late.
    let mut bytes = smf(0, &[vec![RawEvent::note_on(0, 0, 60, 100)]]);
    let n = bytes.len();
    // The last event is `00 FF 2F 00`; its delta becomes 4 beats (0x8F 0x00 is
    // 1920 as a VLQ: 15*128 + 0).
    bytes[n - 4] = 0x8F;
    bytes.insert(n - 3, 0x00);
    // MTrk length is the four bytes at offset 18..22 (14 header + "MTrk").
    let len = u32::from_be_bytes(bytes[18..22].try_into().unwrap()) + 1;
    bytes[18..22].copy_from_slice(&len.to_be_bytes());

    let file = read(&bytes);
    assert_eq!(file.tracks()[0].end, Tick(4 * u64::from(BEAT)));
    assert_eq!(file.end_beat(), Beat(4.0));
    assert_eq!(read(&file.to_bytes().unwrap()).tracks()[0].end, Tick(1920));
}

/// A delta on a non-note event advances the clock: the note after a SysEx a
/// beat in is on beat 2.
#[test]
fn a_sysex_between_notes_does_not_shift_the_beat_grid() {
    let file = read(&smf(
        0,
        &[vec![
            RawEvent::note_on(0, 0, 60, 100),
            RawEvent::note_off(BEAT / 2, 0, 60, 0),
            RawEvent::sysex(BEAT / 2, &[0x7D, 0x01]),
            RawEvent::note_on(BEAT, 0, 62, 100),
        ]],
    ));
    let ticks: Vec<u64> = file.tracks()[0].events.iter().map(|(t, _)| t.0).collect();
    assert_eq!(ticks, [0, 240, 480, 960]);
    assert_eq!(file.beat_at(Tick(960)), Beat(2.0));
}

/// Format 2 keeps its sequences apart: there is no merged event list, and each
/// has the tempo map of its own events.
#[test]
fn format_2_keeps_its_sequences_apart() {
    let file = read(&smf(
        2,
        &[
            vec![
                RawEvent::tempo(0, 500_000),
                RawEvent::note_on(0, 0, 60, 100),
            ],
            vec![
                RawEvent::tempo(0, 1_000_000),
                RawEvent::note_on(0, 0, 62, 100),
            ],
        ],
    ));
    assert_eq!(file.smf_format(), Some(SmfFormat::Sequential));
    assert!(file.events().is_none(), "no common timeline to merge on");
    assert_eq!(file.tracks().len(), 2);
    let tempo = |i| file.tempo_map_of(i).unwrap().tempo_at(Beat(0.0));
    assert_eq!(tempo(0), Bpm::of(120.0));
    assert_eq!(tempo(1), Bpm::of(60.0));
    assert!(file.tempo_map_of(2).is_none());
    assert_eq!(file.tempo_map().tempo_at(Beat(0.0)), Bpm::of(120.0));
    // The same two tracks as format 1 share one map and one timeline.
    let parallel = read(&smf(
        1,
        &[
            vec![
                RawEvent::tempo(0, 500_000),
                RawEvent::note_on(0, 0, 60, 100),
            ],
            vec![RawEvent::note_on(0, 0, 62, 100)],
        ],
    ));
    assert_eq!(parallel.events().unwrap().count(), 3);
    assert_eq!(
        parallel.tempo_map_of(1).unwrap().tempo_at(Beat(0.0)),
        Bpm::of(120.0)
    );
}

/// Format 1: the tracks stay apart and named, and the merged view is sorted by
/// tick with ties in track order.
#[test]
fn merged_events_are_in_time_order_ties_in_track_order() {
    let file = read(&smf(
        1,
        &[
            vec![
                RawEvent::track_name(0, "Conductor"),
                RawEvent::note_on(0, 0, 60, 100),
                RawEvent::note_off(4 * BEAT, 0, 60, 0),
            ],
            vec![
                RawEvent::track_name(0, "Bass"),
                RawEvent::note_on(2 * BEAT, 1, 67, 100),
                RawEvent::note_off(BEAT, 1, 67, 0),
                // On the tick of track 0's Note Off: track 0 first.
                RawEvent::note_on(BEAT, 1, 69, 100),
            ],
        ],
    ));
    assert_eq!(file.tracks()[0].name(), Some("Conductor"));
    assert_eq!(file.tracks()[1].name(), Some("Bass"));
    let merged: Vec<(Beat, &FileEvent)> = file.events().unwrap().collect();
    let beats: Vec<f64> = merged.iter().map(|(b, _)| b.get()).collect();
    assert!(beats.windows(2).all(|w| w[0] <= w[1]), "{beats:?}");
    // The two events at beat 4: track 0's off, then track 1's on.
    let at4: Vec<&FileEvent> = merged
        .iter()
        .filter(|(b, _)| *b == Beat(4.0))
        .map(|(_, e)| *e)
        .collect();
    let (a, b) = (off(0, 60, 0), on(1, 69, 100));
    assert_eq!(at4, [&a, &b]);
}

/// **A tempo map built from the file: 120 BPM, then 60, beat to seconds exact.**
///
/// Four beats at 0.5 s then four at 1 s: beat 4 is 2 s, beat 8 is 6 s, and a
/// beat inside the second stretch is its own tempo's.
#[test]
fn the_tempo_map_follows_the_file() {
    let file = read(&smf(
        1,
        &[vec![
            RawEvent::tempo(0, 500_000),
            RawEvent::tempo(4 * BEAT, 1_000_000),
            RawEvent::note_on(0, 0, 60, 100),
            RawEvent::note_off(4 * BEAT, 0, 60, 0), // at beat 8
        ]],
    ));
    let close = |a: Seconds, b: f64| assert!((a.0 - b).abs() < 1e-12, "{a:?} vs {b}");
    close(file.seconds_at(Beat(0.0)), 0.0);
    close(file.seconds_at(Beat(1.0)), 0.5);
    close(file.seconds_at(Beat(4.0)), 2.0);
    close(file.seconds_at(Beat(5.5)), 3.5);
    close(file.seconds_at(file.end_beat()), 6.0);
    assert_eq!(file.tempo_map().tempo_at(Beat(3.99)), Bpm::of(120.0));
    assert_eq!(file.tempo_map().tempo_at(Beat(4.0)), Bpm::of(60.0));
    assert_eq!(file.beat_at(Tick(480)), Beat(1.0));
    // And it comes back out: the tempo events are events of the track.
    assert_eq!(
        read(&file.to_bytes().unwrap()).seconds_at(Beat(8.0)),
        file.seconds_at(Beat(8.0))
    );
}

/// The map pools every track's Set Tempo: track 0 sets 60 at beat 4, track 1
/// sets 100 BPM at beat 0, so beat 0 plays at 100.
#[test]
fn the_tempo_map_pools_every_track() {
    let file = read(&smf(
        1,
        &[
            vec![
                RawEvent::tempo(4 * BEAT, 1_000_000),
                RawEvent::note_on(0, 0, 60, 100),
            ],
            vec![
                RawEvent::tempo(0, 600_000),
                RawEvent::note_on(0, 1, 62, 100),
            ],
        ],
    ));
    let points = file.tempo_map().points();
    assert_eq!(points.len(), 2);
    assert_eq!(
        (points[0].beat, points[0].tempo),
        (Beat(0.0), Bpm::of(100.0))
    );
    assert_eq!(
        (points[1].beat, points[1].tempo),
        (Beat(4.0), Bpm::of(60.0))
    );
}

/// No Set Tempo at tick 0: 120 BPM, the MIDI default, governs until the first;
/// a Set Tempo later does not govern the beats before it.
#[test]
fn a_file_with_no_tempo_plays_at_120() {
    let none = read(&smf(0, &[vec![RawEvent::note_on(0, 0, 60, 100)]]));
    assert_eq!(none.tempo_map().constant_tempo(), Some(Bpm::of(120.0)));
    assert_eq!(none.seconds_at(Beat(2.0)), Seconds(1.0));

    let later = read(&smf(0, &[vec![RawEvent::tempo(2 * BEAT, 1_000_000)]]));
    assert_eq!(later.tempo_map().tempo_at(Beat(0.0)), Bpm::of(120.0));
    assert_eq!(later.tempo_map().tempo_at(Beat(2.0)), Bpm::of(60.0));
}

/// Two tempos on one tick: the later in the file governs (applied in order).
#[test]
fn the_later_of_two_tempos_on_one_tick_governs() {
    let file = read(&smf(
        1,
        &[
            vec![RawEvent::tempo(0, 500_000)],
            vec![RawEvent::tempo(0, 1_000_000)],
        ],
    ));
    assert_eq!(file.tempo_map().points().len(), 1);
    assert_eq!(file.tempo_map().tempo_at(Beat(0.0)), Bpm::of(60.0));
}

/// A zero microseconds-per-quarter is "no valid tempo": kept as an event,
/// absent from the map. Any other value is a tempo, however absurd (1 us is 60
/// million BPM, 0xFFFFFF us about 3.58 BPM).
#[test]
fn a_zero_tempo_is_not_a_tempo_and_an_absurd_one_is_read_as_written() {
    let file = read(&smf(
        0,
        &[vec![
            RawEvent::tempo(0, 0),
            RawEvent::tempo(BEAT, 1),
            RawEvent::tempo(BEAT, 0xFF_FFFF),
        ]],
    ));
    assert_eq!(
        file.tracks()[0].events[0].1,
        FileEvent::Meta(Meta::Tempo {
            micros_per_quarter: 0,
            extra: Box::default(),
        })
    );
    let points = file.tempo_map().points();
    assert_eq!(points.len(), 3);
    assert_eq!(points[0].tempo, Bpm::of(120.0));
    assert_eq!(points[1].tempo, Bpm::of(60_000_000.0));
    assert!((points[2].tempo.get() - 60_000_000.0 / 16_777_215.0).abs() < 1e-12);
    // The zero is written back.
    assert_eq!(read(&file.to_bytes().unwrap()), file);
}

/// Writing a tempo above the wire's range writes the fastest quarter, 1 us,
/// not the wire's "no valid tempo" zero.
#[test]
fn a_tempo_above_the_wire_range_writes_the_fastest_quarter() {
    assert_eq!(
        Meta::tempo(Bpm::of(1.0e9)),
        Meta::Tempo {
            micros_per_quarter: 1,
            extra: Box::default(),
        }
    );
    assert_eq!(
        Meta::tempo(Bpm::of(0.001)),
        Meta::Tempo {
            micros_per_quarter: 0xFF_FFFF,
            extra: Box::default(),
        }
    );
    // 174 BPM is not a whole microsecond: the wire quantises it.
    let Meta::Tempo {
        micros_per_quarter, ..
    } = Meta::tempo(Bpm::of(174.0))
    else {
        panic!("a tempo");
    };
    assert_eq!(micros_per_quarter, 344_828);
    assert!(
        !Meta::tempo(Bpm::of(174.0))
            .bpm()
            .unwrap()
            .differs_from(Bpm::of(174.0), 0.001)
    );
}

/// An SMPTE division has no beat grid and is refused with its own error,
/// whichever way in.
#[test]
fn smpte_division_is_refused() {
    let bytes = build_smf(
        0,
        Division::Smpte {
            fps: -25,
            ticks_per_frame: 40,
        },
        &[vec![RawEvent::note_on(0, 0, 60, 100)]],
    );
    assert!(matches!(
        MidiFile::from_bytes(&bytes),
        Err(Error::MidiUnsupportedTiming)
    ));
}

#[test]
fn a_zero_division_is_refused() {
    let bytes = build_smf(0, Division::Metrical(0), &[vec![]]);
    assert!(matches!(
        MidiFile::from_bytes(&bytes),
        Err(Error::MidiFileParse(_))
    ));
}

// ---- truncation ----------------------------------------------------------

/// **No strict prefix of a valid file is accepted, and none panics.**
///
/// A file shorter than the whole cannot be the whole, so the answer needs no
/// oracle; a lenient reader would hand back a shorter file for most of them.
#[test]
fn no_prefix_of_a_valid_file_is_accepted() {
    let bytes = smf(
        1,
        &[
            everything(),
            vec![
                RawEvent::note_on(0, 1, 70, 80),
                RawEvent::note_off(BEAT, 1, 70, 0),
            ],
        ],
    );
    assert!(MidiFile::from_bytes(&bytes).is_ok());
    for n in 0..bytes.len() {
        assert!(
            MidiFile::from_bytes(&bytes[..n]).is_err(),
            "the first {n} of {} bytes were accepted",
            bytes.len()
        );
    }
}

/// A chunk whose length runs past the end of the file is `Truncated`.
#[test]
fn a_chunk_cut_short_is_truncated() {
    let bytes = smf(0, &[everything()]);
    let cut = &bytes[..bytes.len() - 5];
    assert!(matches!(MidiFile::from_bytes(cut), Err(Error::Truncated)));
    // Cut inside the 8-byte chunk header of the track.
    assert!(matches!(
        MidiFile::from_bytes(&bytes[..14 + 5]),
        Err(Error::Truncated)
    ));
}

/// Fewer track chunks than the header declares is `Truncated`: here the second
/// of two is missing whole, and the cut is on a chunk boundary.
#[test]
fn a_file_missing_a_track_is_truncated() {
    let two = smf(
        1,
        &[
            vec![RawEvent::note_on(0, 0, 60, 100)],
            vec![RawEvent::note_on(0, 1, 62, 100)],
        ],
    );
    let one = smf(1, &[vec![RawEvent::note_on(0, 0, 60, 100)]]);
    let cut = &two[..one.len()];
    assert!(matches!(MidiFile::from_bytes(cut), Err(Error::Truncated)));
}

/// A track whose chunk length is right but which ends mid-event is malformed:
/// a lenient reader would keep the events before it and say nothing.
#[test]
fn a_track_that_ends_mid_event_is_a_parse_error() {
    let mut bytes = build_smf(0, Division::Metrical(TPQ), &[vec![]]);
    // Replace the track body with `00 90 3C` (a Note On missing its velocity).
    bytes.truncate(14);
    bytes.extend_from_slice(b"MTrk");
    bytes.extend_from_slice(&3u32.to_be_bytes());
    bytes.extend_from_slice(&[0x00, 0x90, 0x3C]);
    assert!(matches!(
        MidiFile::from_bytes(&bytes),
        Err(Error::MidiFileParse(_))
    ));
}

// ---- notes ---------------------------------------------------------------

fn notes_of(tracks: &[Vec<RawEvent>]) -> Vec<PairedNote> {
    read(&smf(0, tracks)).tracks()[0].notes().collect()
}

#[test]
fn a_note_pairs_its_on_and_off_with_16_bit_velocity() {
    let notes = notes_of(&[vec![
        RawEvent::note_on(0, 2, 60, 100),
        RawEvent::note_off(BEAT + BEAT / 2, 2, 60, 0),
    ]]);
    assert_eq!(notes.len(), 1);
    let n = notes[0];
    assert_eq!(
        (n.group, n.channel),
        (MidiGroup::FIRST, MidiChannel::new(2).unwrap())
    );
    assert_eq!(n.note, NoteNumber::of(60));
    assert_eq!((n.start, n.end), (Tick(0), Tick(720)));
    // The 7-bit velocity widened by the specification's scaling (M2-104 D.3).
    assert!(
        n.velocity > 100 << 9 && n.velocity < 101 << 9,
        "{}",
        n.velocity
    );
}

#[test]
fn overlapping_same_key_notes_pair_last_on_first_off() {
    let notes = notes_of(&[vec![
        RawEvent::note_on(0, 0, 60, 100),
        RawEvent::note_on(BEAT, 0, 60, 90),
        RawEvent::note_off(BEAT, 0, 60, 0),
        RawEvent::note_off(BEAT, 0, 60, 0),
    ]]);
    // The first Note Off (tick 960) closes the second On (tick 480).
    let spans: Vec<(u64, u64)> = notes.iter().map(|n| (n.start.0, n.end.0)).collect();
    assert_eq!(spans, [(0, 1440), (480, 960)]);
}

#[test]
fn a_note_off_does_not_close_another_channels_note() {
    let notes = notes_of(&[vec![
        RawEvent::note_on(0, 0, 60, 100),
        RawEvent::note_off(BEAT, 1, 60, 0),
        RawEvent::note_off(BEAT, 0, 60, 0),
    ]]);
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0].end, Tick(960));
}

#[test]
fn a_velocity_zero_note_on_closes_the_note() {
    let notes = notes_of(&[vec![
        RawEvent::note_on(0, 0, 60, 100),
        RawEvent::note_on(BEAT, 0, 60, 0),
        // Open at the end of the track: dropped.
        RawEvent::note_on(0, 0, 64, 100),
    ]]);
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0].end, Tick(480));
}

/// The file layer does not translate: a velocity-0 Note On is read as the Note
/// On it is, and written back as one.
#[test]
fn a_velocity_zero_note_on_stays_a_note_on_in_the_file() {
    let file = read(&smf(0, &[vec![RawEvent::note_on(0, 0, 60, 0)]]));
    assert_eq!(file.tracks()[0].events[0].1, on(0, 60, 0));
    assert_eq!(read(&file.to_bytes().unwrap()), file);
}

// ---- building a file -----------------------------------------------------

fn track(events: Vec<(u64, FileEvent)>) -> Track {
    Track::new(events.into_iter().map(|(t, e)| (Tick(t), e)).collect())
}

/// The writer's own framing is read by the independent decoder: chunk
/// lengths, VLQ deltas, the division, the mandatory End of Track.
#[test]
fn the_writer_is_read_by_a_foreign_decoder() {
    let file = MidiFile::smf(
        SmfFormat::Parallel,
        TPQ,
        vec![
            track(vec![(0, FileEvent::Meta(Meta::tempo(Bpm::of(90.0))))]),
            track(vec![
                (0, on(0, 60, 100)),
                (4 * u64::from(BEAT), off(0, 60, 0)),
            ]),
        ],
    )
    .unwrap();
    let bytes = file.to_bytes().unwrap();
    let decoded = decode_smf(&bytes).expect("a foreign decoder accepts the framing");
    assert_eq!(decoded.format, 1);
    assert_eq!(decoded.ticks_per_quarter(), Some(TPQ));
    assert_eq!(decoded.note_on_ticks(), [0]);
    assert_eq!(decoded.first_tempo_us(), Some(666_667));
    assert!(decoded.tracks.iter().all(|t| {
        t.last()
            .is_some_and(|e| e.status == 0xFF && e.data.first() == Some(&0x2F))
    }));
    // Ticks past a 2-byte VLQ delta survive: tick 1920 is `8F 00`.
    assert_eq!(decoded.tracks[1][1].tick, 1920);
}

#[test]
fn a_format_0_file_has_one_track() {
    let two = vec![Track::default(), Track::default()];
    assert!(matches!(
        MidiFile::smf(SmfFormat::SingleTrack, TPQ, two),
        Err(Error::InvalidConfig(_))
    ));
}

#[test]
fn an_empty_track_list_and_a_bad_division_are_refused() {
    assert!(matches!(
        MidiFile::smf(SmfFormat::Parallel, TPQ, vec![]),
        Err(Error::InvalidConfig(_))
    ));
    for tpq in [0u16, 0x8000] {
        assert!(matches!(
            MidiFile::smf(SmfFormat::Parallel, tpq, vec![Track::default()]),
            Err(Error::InvalidConfig(_))
        ));
    }
}

#[test]
fn events_out_of_order_are_refused() {
    let out_of_order = Track {
        events: vec![(Tick(10), on(0, 60, 1)), (Tick(5), off(0, 60, 1))],
        end: Tick(10),
        ..Track::default()
    };
    assert!(matches!(
        MidiFile::smf(SmfFormat::SingleTrack, TPQ, vec![out_of_order]),
        Err(Error::InvalidConfig(_))
    ));
    // An end before the last event.
    let early_end = Track {
        events: vec![(Tick(10), on(0, 60, 1))],
        end: Tick(5),
        ..Track::default()
    };
    assert!(matches!(
        MidiFile::smf(SmfFormat::SingleTrack, TPQ, vec![early_end]),
        Err(Error::InvalidConfig(_))
    ));
}

/// What an SMF cannot hold is refused rather than dropped on write: a packet,
/// another group, a tempo over 24 bits, a gap over 28 bits.
#[test]
fn what_an_smf_cannot_hold_is_refused() {
    let one = |event| MidiFile::smf(SmfFormat::SingleTrack, TPQ, vec![track(vec![(0, event)])]);
    let group_1 = FileEvent::Message(Message::Midi1(
        Address {
            group: MidiGroup::new(1).unwrap(),
            channel: MidiChannel::FIRST,
        },
        Voice1::ProgramChange(U7::of(1)),
    ));
    assert!(matches!(one(group_1), Err(Error::InvalidConfig(_))));
    let midi2 = FileEvent::Message(Message::Midi2(
        Address::first(MidiChannel::FIRST),
        tutti_ump::Voice2::ChannelPressure(0),
    ));
    assert!(matches!(one(midi2), Err(Error::InvalidConfig(_))));
    let slow = FileEvent::Meta(Meta::Tempo {
        micros_per_quarter: 0x100_0000,
        extra: Box::default(),
    });
    assert!(matches!(one(slow), Err(Error::InvalidConfig(_))));
    let gap = MidiFile::smf(
        SmfFormat::SingleTrack,
        TPQ,
        vec![track(vec![(0x1000_0000, on(0, 60, 1))])],
    );
    assert!(matches!(gap, Err(Error::InvalidConfig(_))));
}

/// The longest gap the wire holds, 28 bits, round-trips.
#[test]
fn the_widest_delta_round_trips() {
    let file = MidiFile::smf(
        SmfFormat::SingleTrack,
        TPQ,
        vec![track(vec![(0x0FFF_FFFF, on(0, 60, 1))])],
    )
    .unwrap();
    assert_eq!(read(&file.to_bytes().unwrap()), file);
}

/// A file written to a path reads back from it, and a path that is not there
/// is an I/O error.
#[test]
fn a_file_reads_and_writes_a_path() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("x.mid");
    let file = MidiFile::smf(
        SmfFormat::SingleTrack,
        TPQ,
        vec![track(vec![(0, on(0, 60, 1))])],
    )
    .unwrap();
    file.write(&path).unwrap();
    assert_eq!(MidiFile::read(&path).unwrap(), file);
    assert!(matches!(
        MidiFile::read(dir.path().join("absent.mid")),
        Err(Error::Io(_))
    ));
}

#[test]
fn bytes_of_neither_container_are_named_as_such() {
    assert!(matches!(
        MidiFile::from_bytes(b"RIFF....WAVE"),
        Err(Error::UnknownFormat)
    ));
    assert!(matches!(
        MidiFile::from_bytes(b""),
        Err(Error::UnknownFormat)
    ));
}
