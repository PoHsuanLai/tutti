use super::*;

#[test]
fn test_parse_empty_midi() {
    let data = [
        0x4D, 0x54, 0x68, 0x64, 0x00, 0x00, 0x00, 0x06, 0x00, 0x00, 0x00, 0x01, 0x01, 0xE0, 0x4D,
        0x54, 0x72, 0x6B, 0x00, 0x00, 0x00, 0x04, 0x00, 0xFF, 0x2F, 0x00,
    ];

    let file = ParsedMidiFile::from_bytes(&data).unwrap();
    assert_eq!(file.ticks_per_beat, 480);
    assert_eq!(file.events.len(), 0);
}

/// Neither direction of the tempo relation may divide by zero.
///
/// A `Tempo(0)` meta event is malformed but wire-representable, and gave an
/// infinite BPM that became `ParsedMidiFile::tempo_bpm`. Writing a
/// non-positive BPM did the mirror: `inf` microseconds, saturating to
/// `u32::MAX` — about 71 minutes per beat — in the file. The MIDI-2 twin
/// has always guarded both.
#[test]
fn a_degenerate_tempo_is_absent_rather_than_infinite() {
    // Header (division 480) + a track holding only Set Tempo = 0.
    let data = [
        0x4D, 0x54, 0x68, 0x64, 0x00, 0x00, 0x00, 0x06, 0x00, 0x00, 0x00, 0x01, 0x01, 0xE0, 0x4D,
        0x54, 0x72, 0x6B, 0x00, 0x00, 0x00, 0x0B, 0x00, 0xFF, 0x51, 0x03, 0x00, 0x00, 0x00, 0x00,
        0xFF, 0x2F, 0x00,
    ];
    let file = ParsedMidiFile::from_bytes(&data).expect("a zero tempo is not a parse failure");
    assert!(
        file.tempo_bpm.get().is_finite(),
        "zero microseconds-per-quarter must not become an infinite BPM"
    );
    // Falls back to the documented default rather than inventing a rate.
    assert_eq!(file.tempo_bpm, Bpm(120.0));

    // And the write direction: a non-positive BPM writes the wire's own
    // "no tempo" zero rather than a saturated u32.
    let config = MidiWriteConfig {
        ticks_per_beat: 480,
        tempo_bpm: Some(Bpm(0.0)),
        time_signature: None,
    };
    let track = vec![vec![SmfTimedEvent {
        time_beats: Beat(0.0),
        channel: MidiChannel::FIRST,
        msg: SmfMessage::NoteOn {
            key: 60.into(),
            vel: 100.into(),
        },
    }]];
    let bytes = to_bytes(&track, &config).expect("encodes");
    let reparsed = ParsedMidiFile::from_bytes(&bytes).expect("round-trips");
    assert!(reparsed.tempo_bpm.get().is_finite());
}

/// Both readers of the header's division field must reject a zero.
///
/// `SmfFile::from_bytes` always did; `ParsedMidiFile::from_bytes` did not, on the same field of the same
/// file — so which entry point you called decided whether a malformed file
/// was an error or a set of infinite event times. The two bytes at offset
/// 12..14 are the division, here zeroed.
#[test]
fn either_reader_rejects_a_zero_division() {
    let mut data = [
        0x4D, 0x54, 0x68, 0x64, 0x00, 0x00, 0x00, 0x06, 0x00, 0x00, 0x00, 0x01, 0x01, 0xE0, 0x4D,
        0x54, 0x72, 0x6B, 0x00, 0x00, 0x00, 0x04, 0x00, 0xFF, 0x2F, 0x00,
    ];
    // Sanity: this fixture parses on both paths before the division is
    // zeroed, so the assertions below are about the zero and nothing else.
    assert!(ParsedMidiFile::from_bytes(&data).is_ok());
    assert!(crate::SmfFile::from_bytes(&data).is_ok());

    data[12] = 0x00;
    data[13] = 0x00;
    assert!(matches!(
        ParsedMidiFile::from_bytes(&data),
        Err(Error::MidiFileParse(_))
    ));
    assert!(matches!(
        crate::SmfFile::from_bytes(&data),
        Err(Error::MidiFileParse(_))
    ));
}

#[test]
fn test_write_and_read_roundtrip() {
    let events = vec![
        SmfTimedEvent {
            time_beats: Beat(0.0),
            channel: MidiChannel::FIRST,
            msg: SmfMessage::NoteOn {
                key: 60.into(),
                vel: 100.into(),
            },
        },
        SmfTimedEvent {
            time_beats: Beat(1.0),
            channel: MidiChannel::FIRST,
            msg: SmfMessage::NoteOff {
                key: 60.into(),
                vel: 0.into(),
            },
        },
    ];

    let config = MidiWriteConfig {
        ticks_per_beat: 480,
        tempo_bpm: Some(Bpm(120.0)),
        time_signature: Some((4, 2)),
    };

    let data = to_bytes(&[events], &config).unwrap();
    let parsed = ParsedMidiFile::from_bytes(&data).unwrap();

    assert_eq!(parsed.ticks_per_beat, 480);
    assert!(!parsed.tempo_bpm.differs_from(Bpm(120.0), 0.1));
    assert_eq!(parsed.events.len(), 2);
    assert!((parsed.events[0].time_beats - Beat(0.0)).abs() < BeatDuration(0.001));
    assert!((parsed.events[1].time_beats - Beat(1.0)).abs() < BeatDuration(0.001));
}

/// Beat->tick rounds to nearest, asserted on the tick itself rather than
/// through a write->read round trip.
///
/// The round-trip tests above cannot see this. They use integer beats,
/// which are exact on any grid, and they compare beats back with a
/// `0.001`-beat tolerance — nearly half a tick at 480 tpb, so a one-tick
/// error passes. This asserts the integer the encoder actually computes.
#[test]
fn beat_to_tick_rounds_to_nearest() {
    let at = |beats: f64| SmfTimedEvent {
        time_beats: Beat(beats),
        channel: MidiChannel::FIRST,
        msg: SmfMessage::NoteOn {
            key: 60.into(),
            vel: 100.into(),
        },
    };
    let config = MidiWriteConfig {
        ticks_per_beat: 480,
        ..Default::default()
    };
    // `build_track` emits deltas, so a single event's delta is its
    // absolute tick.
    let tick_of = |beats: f64| {
        let track = build_track(&[at(beats)], &config, false);
        track[0].delta.as_int()
    };

    // Off-grid, below the midpoint: 0.5008 beats is 240.384 ticks.
    assert_eq!(tick_of(0.500_8), 240, "below midpoint rounds down");
    // Off-grid, above the midpoint: 0.5015 beats is 240.72 ticks.
    assert_eq!(tick_of(0.501_5), 241, "above midpoint rounds up");

    // The regression this replaced truncation for. A beat reached by
    // repeated addition of a triplet lands just under its exact value;
    // truncation wrote it a whole tick early.
    let mut accumulated = 0.0_f64;
    for _ in 0..6 {
        accumulated += 1.0 / 3.0;
    }
    assert!(accumulated < 2.0, "the float lands just under two beats");
    assert_eq!(
        tick_of(accumulated),
        960,
        "an accumulated beat 2 writes tick 960, not 959"
    );

    // Exact multiples are unaffected in either direction.
    assert_eq!(tick_of(0.0), 0);
    assert_eq!(tick_of(1.0), 480);
    assert_eq!(tick_of(0.25), 120);
}

/// The delta-time varint on the wire, byte for byte.
///
/// Asserts the encoded bytes directly rather than parsing them back, so a
/// rounding change shows up as a differing file and not merely as a
/// differing in-memory value. 481 ticks is deliberately over the 127-tick
/// single-byte varint ceiling, making this a check of the multi-byte
/// encoding too: 481 = 0b11_1100001 -> `0x83 0x61`.
#[test]
fn delta_time_varint_golden_bytes() {
    // 1.0015 beats is 480.72 ticks at 480 tpb: rounds to 481, truncates
    // to 480 — a one-byte difference in the file.
    let events = vec![SmfTimedEvent {
        time_beats: Beat(1.001_5),
        channel: MidiChannel::FIRST,
        msg: SmfMessage::NoteOn {
            key: 60.into(),
            vel: 100.into(),
        },
    }];
    let data = to_bytes(
        &[events],
        &MidiWriteConfig {
            ticks_per_beat: 480,
            tempo_bpm: None,
            time_signature: None,
        },
    )
    .unwrap();

    // Header: "MThd", length 6, format 0, 1 track, division 480 (0x01E0).
    assert_eq!(
        &data[..14],
        &[
            0x4D, 0x54, 0x68, 0x64, 0x00, 0x00, 0x00, 0x06, 0x00, 0x00, 0x00, 0x01, 0x01, 0xE0
        ],
        "header"
    );
    // Track chunk: "MTrk", then the length, then the events.
    assert_eq!(&data[14..18], b"MTrk", "track magic");
    // First event: delta 481 as the varint `0x83 0x61`, then NoteOn on
    // channel 0 (0x90), key 60 (0x3C), velocity 100 (0x64).
    assert_eq!(
        &data[22..27],
        &[0x83, 0x61, 0x90, 0x3C, 0x64],
        "delta varint 481 then NoteOn — truncation would write 0x83 0x60"
    );
}

#[test]
fn test_write_empty_tracks_error() {
    let result = to_bytes(&[], &MidiWriteConfig::default());
    assert!(result.is_err());
}

#[test]
fn tracks_pairs_note_on_off_with_duration() {
    // One note: on at beat 0, off at beat 1, on a named track.
    let events = vec![
        SmfTimedEvent {
            time_beats: Beat(0.0),
            channel: MidiChannel::FIRST,
            msg: SmfMessage::NoteOn {
                key: 60.into(),
                vel: 100.into(),
            },
        },
        SmfTimedEvent {
            time_beats: Beat(1.0),
            channel: MidiChannel::FIRST,
            msg: SmfMessage::NoteOff {
                key: 60.into(),
                vel: 0.into(),
            },
        },
    ];
    let data = to_bytes(
        &[events],
        &MidiWriteConfig {
            ticks_per_beat: 480,
            ..Default::default()
        },
    )
    .unwrap();

    let parsed = crate::SmfFile::from_bytes(&data).unwrap().into_tracks();
    assert_eq!(parsed.len(), 1);
    assert_eq!(parsed[0].notes.len(), 1);
    let n = parsed[0].notes[0];
    assert_eq!(n.key, 60);
    assert_eq!(n.velocity, 100);
    assert!((n.start_beats - Beat(0.0)).abs() < BeatDuration(1e-6));
    assert!((n.duration_beats - BeatDuration(1.0)).abs() < BeatDuration(1e-3));
}

#[test]
fn tracks_pair_notes_per_channel() {
    // Same key on two channels, overlapping. Channel 0's note runs 0..4;
    // channel 1's runs 1..2. Pairing on key alone would let channel 1's
    // NoteOff at beat 2 close channel 0's note (LIFO), yielding durations
    // 1 (ch1: 1..2 closed by the ch0 off at 4 → no, LIFO gives 1..2→1)
    // and 2 — i.e. the wrong note gets the wrong length.
    let note = |time_beats: f64, channel: u8, on: bool| SmfTimedEvent {
        time_beats: Beat(time_beats),
        channel: MidiChannel::of(channel),
        msg: if on {
            SmfMessage::NoteOn {
                key: 60.into(),
                vel: 100.into(),
            }
        } else {
            SmfMessage::NoteOff {
                key: 60.into(),
                vel: 0.into(),
            }
        },
    };
    let events = vec![
        note(0.0, 0, true),
        note(1.0, 1, true),
        note(2.0, 1, false),
        note(4.0, 0, false),
    ];
    let data = to_bytes(
        &[events],
        &MidiWriteConfig {
            ticks_per_beat: 480,
            ..Default::default()
        },
    )
    .unwrap();

    let parsed = crate::SmfFile::from_bytes(&data).unwrap().into_tracks();
    assert_eq!(parsed[0].notes.len(), 2);

    let ch0 = parsed[0]
        .notes
        .iter()
        .find(|n| n.channel == MidiChannel::of(0))
        .expect("channel 0 note");
    let ch1 = parsed[0]
        .notes
        .iter()
        .find(|n| n.channel == MidiChannel::of(1))
        .expect("channel 1 note");

    assert!(
        (ch0.start_beats - Beat(0.0)).abs() < BeatDuration(1e-3)
            && (ch0.duration_beats - BeatDuration(4.0)).abs() < BeatDuration(1e-3),
        "ch0 note should be closed by its own NoteOff at beat 4, got {ch0:?}"
    );
    assert!(
        (ch1.start_beats - Beat(1.0)).abs() < BeatDuration(1e-3)
            && (ch1.duration_beats - BeatDuration(1.0)).abs() < BeatDuration(1e-3),
        "ch1 note should be closed by its own NoteOff at beat 2, got {ch1:?}"
    );
}

#[test]
fn tracks_treats_velocity_zero_note_on_as_off() {
    let events = vec![
        SmfTimedEvent {
            time_beats: Beat(0.0),
            channel: MidiChannel::FIRST,
            msg: SmfMessage::NoteOn {
                key: 64.into(),
                vel: 80.into(),
            },
        },
        SmfTimedEvent {
            time_beats: Beat(2.0),
            channel: MidiChannel::FIRST,
            // Running-status note-off: NoteOn with velocity 0.
            msg: SmfMessage::NoteOn {
                key: 64.into(),
                vel: 0.into(),
            },
        },
    ];
    let data = to_bytes(
        &[events],
        &MidiWriteConfig {
            ticks_per_beat: 480,
            ..Default::default()
        },
    )
    .unwrap();

    let parsed = crate::SmfFile::from_bytes(&data).unwrap().into_tracks();
    assert_eq!(
        parsed[0].notes.len(),
        1,
        "vel-0 NoteOn should close the note"
    );
    assert!((parsed[0].notes[0].duration_beats - BeatDuration(2.0)).abs() < BeatDuration(1e-3));
}
