use super::*;
use tutti_ump::{CCNumber, MidiChannel};

#[test]
fn empty_clip_round_trips_dctpq() {
    let bytes = write_clip_file(480, &[]);
    let parsed = ParsedClipFile::from_bytes(&bytes).expect("parses");
    assert_eq!(parsed.ticks_per_quarter, 480);
    assert!(parsed.events.is_empty());
}

#[test]
fn events_round_trip_with_deltas() {
    let events = [
        ClipEvent {
            delta_ticks: 0,
            event: MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x8000),
        },
        ClipEvent {
            delta_ticks: 240,
            event: MidiEvent::note_off(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0),
        },
        ClipEvent {
            delta_ticks: 240,
            event: MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::new(1), 64, 0xFFFF),
        },
    ];
    let bytes = write_clip_file(96, &events);
    let parsed = ParsedClipFile::from_bytes(&bytes).expect("parses");
    assert_eq!(parsed.ticks_per_quarter, 96);
    assert_eq!(parsed.events.len(), 3);
    assert_eq!(parsed.events[0].delta_ticks, 0);
    assert_eq!(parsed.events[1].delta_ticks, 240);
    assert_eq!(parsed.events[2].delta_ticks, 240);
    // Payloads survive.
    assert_eq!(parsed.events, events);
}

/// `to_bytes` re-encodes exactly the parsed events, so a parse then
/// `to_bytes` is byte-stable and equals the headerless writer.
///
/// Mutation (run): have `to_bytes` call `write_clip_file(tpq, &[])` (drop
/// the events), or pass `0` for the ticks and this fails.
#[test]
fn to_bytes_re_encodes_what_from_bytes_parsed() {
    let events = [
        ClipEvent::new(
            0,
            MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x8000),
        ),
        ClipEvent::new(
            240,
            MidiEvent::note_off(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0),
        ),
    ];
    let bytes = write_clip_file(96, &events);
    let parsed = ParsedClipFile::from_bytes(&bytes).expect("parses");
    assert_eq!(parsed.to_bytes(), write_clip_file(96, &events));
    assert_eq!(parsed.to_bytes(), bytes);
}

/// Big-endian words of a clip file body, for byte-level assertions.
fn body_words(bytes: &[u8]) -> Vec<u32> {
    bytes[8..]
        .as_chunks::<4>()
        .0
        .iter()
        .map(|c| u32::from_be_bytes(*c))
        .collect()
}

#[test]
fn large_delta_writes_dcs_noop_restarts() {
    // M2-116 §3.2.2: a gap beyond the 20-bit DCS field is written as
    // `DCS(max) · NOOP` restarts, then the remainder — NOT as adjacent DCSs
    // that a reader is expected to sum. This asserts the bytes, not a
    // round-trip: writer and reader agreeing with each other is exactly the
    // thing that hid this bug.
    let big = 3_000_000; // 2×DCS_MAX + 902_850
    let bytes = write_clip_file(
        480,
        &[ClipEvent {
            delta_ticks: big,
            event: MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x8000),
        }],
    );
    let words = body_words(&bytes);

    // Skip DCS(0)+DCTPQ and DCS(0)+StartOfClip (1+1+1+4 words).
    let stream = &words[7..];
    let dcs_max_word = 0x0040_0000 | DCS_MAX; // utility, status 0x4
    let noop_word = 0x0000_0000; // utility, status 0x0

    assert_eq!(stream[0], dcs_max_word, "first chunk is a full DCS field");
    assert_eq!(stream[1], noop_word, "restart marker must be a NOOP");
    assert_eq!(stream[2], dcs_max_word, "second full chunk");
    assert_eq!(stream[3], noop_word, "second restart marker");
    assert_eq!(
        stream[4],
        0x0040_0000 | (big - 2 * DCS_MAX),
        "remainder counts from the last NOOP"
    );
    // Then the note-on itself (MT 0x4, 2 words).
    assert_eq!(stream[5] >> 28, 0x4);
}

#[test]
fn reader_restarts_on_noop_and_replaces_on_bare_dcs() {
    // Hand-built conformant fixture — an independent oracle for the reader,
    // so a matching writer bug cannot mask a reader bug.
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&CLIP_FILE_MAGIC);
    let mut push = |w: u32| bytes.extend_from_slice(&w.to_be_bytes());
    push(0x0040_0000); // DCS(0)
    push(0x0030_01E0); // DCTPQ = 480
    push(0x0040_0000); // DCS(0)
    for w in [0xF020_0000, 0, 0, 0] {
        push(w); // Start of Clip
    }
    // A gap of DCS_MAX + 1000, spelled the conformant way.
    push(0x0040_0000 | DCS_MAX); // DCS(max)
    push(0x0000_0000); // NOOP → restart
    push(0x0040_0000 | 1000); // DCS(1000) counts from the NOOP
    push(0x4090_3C00); // NoteOn word 0
    push(0x8000_0000); // NoteOn word 1
    push(0x0040_0000); // DCS(0)
    for w in [0xF021_0000, 0, 0, 0] {
        push(w); // End of Clip
    }

    let parsed = ParsedClipFile::from_bytes(&bytes).expect("parses");
    assert_eq!(parsed.ticks_per_quarter, 480);
    assert_eq!(parsed.events.len(), 1);
    assert_eq!(
        parsed.events[0].delta_ticks,
        DCS_MAX + 1000,
        "NOOP banks the preceding DCS; the next DCS adds on top"
    );
}

#[test]
fn bare_consecutive_dcs_replaces_rather_than_sums() {
    // Two DCSs with no NOOP between them: §3.2.2 makes each the count since
    // the last event, so the second REPLACES the first. Summing them (our
    // old behaviour) would give 1500 and place the event late.
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&CLIP_FILE_MAGIC);
    let mut push = |w: u32| bytes.extend_from_slice(&w.to_be_bytes());
    push(0x0040_0000);
    push(0x0030_01E0); // DCTPQ = 480
    push(0x0040_0000); // DCS(0)
    for w in [0xF020_0000, 0, 0, 0] {
        push(w); // Start of Clip
    }
    push(0x0040_0000 | 500); // DCS(500)
    push(0x0040_0000 | 1000); // DCS(1000) — replaces, not adds
    push(0x4090_3C00);
    push(0x8000_0000);
    push(0x0040_0000); // DCS(0)
    for w in [0xF021_0000, 0, 0, 0] {
        push(w); // End of Clip
    }

    let parsed = ParsedClipFile::from_bytes(&bytes).expect("parses");
    assert_eq!(parsed.events.len(), 1);
    assert_eq!(parsed.events[0].delta_ticks, 1000);
}

#[test]
fn large_delta_round_trips() {
    // The round-trip must still hold — but it is now a consequence of both
    // halves being spec-correct, not of them sharing a bug.
    let events = [
        ClipEvent {
            delta_ticks: 3_000_000,
            event: MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x8000),
        },
        ClipEvent {
            delta_ticks: DCS_MAX, // exactly one full field — no restart needed
            event: MidiEvent::note_off(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0),
        },
    ];
    let bytes = write_clip_file(480, &events);
    let parsed = ParsedClipFile::from_bytes(&bytes).expect("parses");
    assert_eq!(parsed.events, events);
}

#[test]
fn exactly_max_delta_needs_no_restart() {
    // Boundary: DCS_MAX fits one field, so no NOOP should be emitted.
    let bytes = write_clip_file(
        480,
        &[ClipEvent {
            delta_ticks: DCS_MAX,
            event: MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x8000),
        }],
    );
    let stream = body_words(&bytes)[7..].to_vec();
    assert_eq!(stream[0], 0x0040_0000 | DCS_MAX);
    assert_eq!(stream[1] >> 28, 0x4, "note-on follows immediately, no NOOP");
}

#[test]
fn beats_round_trip_through_the_beat_facade() {
    // Absolute beats in → delta ticks on disk → absolute beats out.
    let bytes = write_clip_file_from_beats(
        96,
        [
            (
                Beat(0.0),
                MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x8000),
            ),
            (
                Beat(2.0),
                MidiEvent::note_off(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0),
            ),
            (
                Beat(2.5),
                MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::new(1), 64, 0xFFFF),
            ),
        ],
    );
    let clip = ParsedClipFile::from_bytes(&bytes).expect("parses");
    let timed: Vec<(Beat, MidiEvent)> = clip.iter().collect();
    assert_eq!(timed.len(), 3);
    assert!((timed[0].0 - Beat(0.0)).abs() < BeatDuration(1e-9));
    assert!((timed[1].0 - Beat(2.0)).abs() < BeatDuration(1e-9));
    assert!((timed[2].0 - Beat(2.5)).abs() < BeatDuration(1e-9));
    assert_eq!(
        timed[2].1,
        MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::new(1), 64, 0xFFFF)
    );
    assert!((clip.duration_beats() - BeatDuration(2.5)).abs() < BeatDuration(1e-9));
}

#[test]
fn beat_facade_orders_unsorted_input() {
    // Out-of-order input is sorted by beat; deltas never go negative.
    let bytes = write_clip_file_from_beats(
        480,
        [
            (
                Beat(4.0),
                MidiEvent::note_off(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0),
            ),
            (
                Beat(0.0),
                MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x8000),
            ),
        ],
    );
    let clip = ParsedClipFile::from_bytes(&bytes).expect("parses");
    let beats: Vec<Beat> = clip.iter().map(|(b, _)| b).collect();
    assert_eq!(beats.len(), 2);
    assert!(beats[0] < beats[1]);
    assert!((beats[0] - Beat(0.0)).abs() < BeatDuration(1e-9));
    assert!((beats[1] - Beat(4.0)).abs() < BeatDuration(1e-9));
}

#[test]
fn header_round_trips_tempo_and_time_signature() {
    // M2-116 §7.1.1/§7.1.2: the sequence opens with Set Tempo then Set Time
    // Signature. A clip carries its own tempo map; an importer that can't
    // read it back places every note at the wrong wall-clock time.
    let header = ClipHeader {
        tempo_bpm: Bpm(174.0),
        time_signature: (7, 8),
    };
    let bytes = write_clip_file_with_header(
        480,
        header,
        &[ClipEvent::new(
            0,
            MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x8000),
        )],
    );

    let clip = ParsedClipFile::from_bytes(&bytes).expect("parses");
    assert!(
        !clip
            .tempo_bpm()
            .expect("tempo present")
            .differs_from(Bpm(174.0), 0.05),
        "tempo survives the round trip"
    );
    assert_eq!(clip.time_signature(), Some((7, 8)));

    // Ordering: tempo comes before the time signature, both before the note.
    let kinds: Vec<_> = clip
        .events
        .iter()
        .map(|ce| {
            (
                crate::ump::flex_tempo_bpm(&ce.event).is_some(),
                crate::ump::flex_time_signature(&ce.event).is_some(),
            )
        })
        .collect();
    assert_eq!(kinds[0], (true, false), "Set Tempo first");
    assert_eq!(kinds[1], (false, true), "Set Time Signature second");

    // The note still lands at beat 0 — the header messages share its
    // zero clockstamp rather than pushing it later.
    let timed: Vec<(Beat, MidiEvent)> = clip.iter().collect();
    assert!(timed.iter().all(|(b, _)| *b == Beat(0.0)));

    // A clip written without a header simply declares none.
    let bare = ParsedClipFile::from_bytes(&write_clip_file(480, &[])).expect("parses");
    assert_eq!(bare.tempo_bpm(), None);
    assert_eq!(bare.time_signature(), None);
}

#[test]
fn errors_distinguish_failure_modes() {
    // Not a clip file at all.
    assert_eq!(
        ParsedClipFile::from_bytes(b"NOTACLIP\0\0\0\0"),
        Err(ClipFileError::BadMagic)
    );
    assert_eq!(
        ParsedClipFile::from_bytes(b"short"),
        Err(ClipFileError::BadMagic)
    );
    // Right magic, but the body isn't word-aligned.
    assert_eq!(
        ParsedClipFile::from_bytes(b"SMF2CLIP\x00\x00\x00"),
        Err(ClipFileError::Unaligned)
    );
    // Magic + aligned body, but no DCTPQ before EOF.
    assert_eq!(
        ParsedClipFile::from_bytes(b"SMF2CLIP"),
        Err(ClipFileError::MissingDctpq)
    );
}

/// A declared tick unit of zero is not a tick unit.
///
/// It parsed, and then every beat in the file was infinity: `timed()`
/// divides the running tick count by it, and so does `duration_beats`. The
/// SMF reader has always rejected the same condition — `tracks()` returns
/// "zero ticks-per-beat" — so this is that guard's MIDI-2 twin, which was
/// never written. Presence was checked; the value was not.
#[test]
fn a_zero_tick_unit_is_rejected_rather_than_dividing_by_it() {
    let ev = MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x8000);
    let bytes = write_clip_file(0, &[ClipEvent::new(240, ev)]);
    assert_eq!(
        ParsedClipFile::from_bytes(&bytes),
        Err(ClipFileError::ZeroDctpq)
    );

    // The neighbouring value still parses, so the guard rejects zero rather
    // than anything that merely looks small.
    let bytes = write_clip_file(1, &[ClipEvent::new(240, ev)]);
    let parsed = ParsedClipFile::from_bytes(&bytes).expect("tpq=1 is a valid tick unit");
    assert!(parsed.iter().all(|(beat, _)| beat.get().is_finite()));
}

#[test]
fn structural_shalls_are_enforced() {
    // M2-116 §7 requires both brackets, and §7.3 forbids anything after End
    // of Clip. These were all accepted silently, which defeats the point of
    // ClipFileError distinguishing "not a clip file" from "malformed".
    let mut base = Vec::new();
    base.extend_from_slice(&CLIP_FILE_MAGIC);
    for w in [0x0040_0000u32, 0x0030_01E0] {
        base.extend_from_slice(&w.to_be_bytes()); // DCS(0) + DCTPQ
    }
    let push = |bytes: &mut Vec<u8>, words: &[u32]| {
        for w in words {
            bytes.extend_from_slice(&w.to_be_bytes());
        }
    };

    // DCTPQ but no Start of Clip.
    let mut no_start = base.clone();
    push(&mut no_start, &[0x0040_0000, 0xF021_0000, 0, 0, 0]);
    assert_eq!(
        ParsedClipFile::from_bytes(&no_start),
        Err(ClipFileError::MissingStartOfClip)
    );

    // Start but no End — a file truncated mid-clip.
    let mut no_end = base.clone();
    push(&mut no_end, &[0x0040_0000, 0xF020_0000, 0, 0, 0]);
    assert_eq!(
        ParsedClipFile::from_bytes(&no_end),
        Err(ClipFileError::MissingEndOfClip)
    );

    // Data after End of Clip: §7.3 forbids it, and there is no such thing
    // as a multi-clip clip file (that is the MIDI Container File's job).
    let mut trailing = base.clone();
    push(
        &mut trailing,
        &[
            0x0040_0000,
            0xF020_0000,
            0,
            0,
            0, // Start of Clip
            0x0040_0000,
            0xF021_0000,
            0,
            0,
            0, // End of Clip
            0x0040_0000,
            0x4090_3C00,
            0x8000_0000, // …then a stray note-on
        ],
    );
    assert_eq!(
        ParsedClipFile::from_bytes(&trailing),
        Err(ClipFileError::TrailingData)
    );

    // A well-formed file with both brackets still parses.
    assert!(ParsedClipFile::from_bytes(&write_clip_file(480, &[])).is_ok());
}

// --- Note pairing ---

/// Round-trip a beat-positioned event list and pair it back into notes.
fn notes_from(events: impl IntoIterator<Item = (Beat, MidiEvent)>) -> Vec<ClipNote> {
    let bytes = write_clip_file_from_beats(96, events);
    ParsedClipFile::from_bytes(&bytes).unwrap().notes()
}

#[test]
fn pairs_notes_and_keeps_full_velocity() {
    let notes = notes_from([
        (
            Beat(0.0),
            MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0xABCD),
        ),
        (
            Beat(2.5),
            MidiEvent::note_off(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0),
        ),
    ]);

    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0].note, 60);
    assert_eq!(notes[0].start_beats, Beat(0.0));
    assert_eq!(notes[0].duration_beats, BeatDuration(2.5));
    // The reason pairing lives here: a caller pairing on `velocity_u7`
    // would have silently narrowed this to 7 bits.
    assert_eq!(notes[0].velocity, 0xABCD);
}

#[test]
fn a_note_off_only_closes_its_own_channel_and_group() {
    // Same note number in three different (group, channel) spaces, closed
    // in a deliberately scrambled order.
    let notes = notes_from([
        (
            Beat(0.0),
            MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x1000),
        ),
        (
            Beat(0.0),
            MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::new(1), 60, 0x2000),
        ),
        (
            Beat(0.0),
            MidiEvent::note_on(MidiGroup::new(1), MidiChannel::FIRST, 60, 0x3000),
        ),
        (
            Beat(1.0),
            MidiEvent::note_off(MidiGroup::new(1), MidiChannel::FIRST, 60, 0),
        ),
        (
            Beat(2.0),
            MidiEvent::note_off(MidiGroup::FIRST, MidiChannel::new(1), 60, 0),
        ),
        (
            Beat(3.0),
            MidiEvent::note_off(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0),
        ),
    ]);

    assert_eq!(notes.len(), 3);
    // Sorted by (group, channel) at equal onset, so the order is stable.
    assert_eq!(
        notes
            .iter()
            .map(|n| (n.group, n.channel))
            .collect::<Vec<_>>(),
        [
            (MidiGroup::new(0), MidiChannel::new(0)),
            (MidiGroup::new(0), MidiChannel::new(1)),
            (MidiGroup::new(1), MidiChannel::new(0)),
        ]
    );
    // Each closed against its own Note Off, not the nearest one.
    assert_eq!(notes[0].duration_beats, BeatDuration(3.0));
    assert_eq!(notes[1].duration_beats, BeatDuration(2.0));
    assert_eq!(notes[2].duration_beats, BeatDuration(1.0));
    assert_eq!(
        notes.iter().map(|n| n.velocity).collect::<Vec<_>>(),
        [0x1000, 0x2000, 0x3000]
    );
}

#[test]
fn overlapping_identical_notes_close_in_lifo_order() {
    let notes = notes_from([
        (
            Beat(0.0),
            MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x1000),
        ),
        (
            Beat(1.0),
            MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x2000),
        ),
        (
            Beat(2.0),
            MidiEvent::note_off(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0),
        ),
        (
            Beat(4.0),
            MidiEvent::note_off(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0),
        ),
    ]);

    assert_eq!(notes.len(), 2);
    // The first Note Off closes the *most recent* Note On.
    assert_eq!(
        (notes[0].start_beats, notes[0].duration_beats),
        (Beat(0.0), BeatDuration(4.0))
    );
    assert_eq!(
        (notes[1].start_beats, notes[1].duration_beats),
        (Beat(1.0), BeatDuration(1.0))
    );
}

#[test]
fn a_midi1_velocity_zero_note_on_closes_a_note() {
    // MIDI 1.0's running-status idiom: NoteOn with velocity 0 is a NoteOff.
    // If it were treated as an onset instead, this would pair as two open
    // notes and yield nothing. Built from MIDI 1.0 bytes so these are real
    // Channel Voice 1 packets — `note_on_7bit` would widen to CV2 and never
    // reach this path.
    let on = MidiEvent::from_midi1_bytes(0, &[0x90, 60, 100]).expect("midi1 note-on");
    let off = MidiEvent::from_midi1_bytes(0, &[0x90, 60, 0]).expect("midi1 note-on vel 0");
    let notes = notes_from([(Beat(0.0), on), (Beat(1.5), off)]);

    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0].duration_beats, BeatDuration(1.5));
    // A MIDI 1.0 velocity upscales by Min-Center-Max rather than shifting.
    assert_eq!(
        notes[0].velocity,
        crate::convert::midi1_velocity_to_midi2(100)
    );
}

#[test]
fn notes_left_open_at_end_of_clip_are_dropped() {
    let notes = notes_from([
        (
            Beat(0.0),
            MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x8000),
        ),
        (
            Beat(1.0),
            MidiEvent::note_off(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0),
        ),
        // Never closed — its duration is unknowable, so it is not a note.
        (
            Beat(2.0),
            MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 64, 0x8000),
        ),
    ]);

    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0].note, 60);
}

#[test]
fn non_note_events_do_not_disturb_pairing() {
    let notes = notes_from([
        (
            Beat(0.0),
            MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x8000),
        ),
        (
            Beat(0.5),
            MidiEvent::cc(
                MidiGroup::FIRST,
                MidiChannel::FIRST,
                CCNumber::BRIGHTNESS,
                0x4000,
            ),
        ),
        (
            Beat(1.0),
            MidiEvent::note_off(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0),
        ),
    ]);

    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0].duration_beats, BeatDuration(1.0));
}
