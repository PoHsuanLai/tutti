//! The writer, checked with a decoder that is not `midly`.
//!
//! `smf.rs`'s inline write tests assert byte-exact output for one hand-picked
//! delta sequence and round-trip through `ParsedMidiFile::from_bytes`. Both are
//! useful; neither can catch an asymmetry the reader and writer *share*,
//! because both halves are the same code's opinion of itself. Reading the
//! bytes back with `support`'s independent decoder closes that.
//!
//! Mutations run against `smf.rs`, and which test each broke:
//!
//! | mutation | fails |
//! |---|---|
//! | `.round() as u32` → `as u32` in the beat→tick conversion | `a_tick_is_rounded_to_nearest_not_truncated` |
//! | `sort_by` → `sort_unstable_by` on equal keys | `parse_write_parse_is_byte_stable` (non-deterministically; see the note there) |
//! | drop the `.max(1)` on the written microseconds | `a_tempo_above_the_wire_range_writes_the_fastest_quarter` |

mod support;

use support::decode_smf;
use tutti_midi_file::smf::{self, MidiWriteConfig, SmfTimedEvent};
use tutti_types::{Beat, Bpm};
use tutti_ump::MidiChannel;

const TPQ: u16 = 480;

fn note_on(beat: f64, key: u8) -> SmfTimedEvent {
    SmfTimedEvent {
        time_beats: Beat(beat),
        channel: MidiChannel::FIRST,
        msg: midly::MidiMessage::NoteOn {
            key: key.into(),
            vel: 100.into(),
        },
    }
}

fn note_off(beat: f64, key: u8) -> SmfTimedEvent {
    SmfTimedEvent {
        time_beats: Beat(beat),
        channel: MidiChannel::FIRST,
        msg: midly::MidiMessage::NoteOff {
            key: key.into(),
            vel: 0.into(),
        },
    }
}

fn config() -> MidiWriteConfig {
    MidiWriteConfig {
        ticks_per_beat: TPQ,
        tempo_bpm: Some(Bpm::of(120.0)),
        time_signature: Some((4, 4)),
    }
}

/// **A beat position converts to the *nearest* tick, not the one below it.**
///
/// The single highest-value write test. `build_track`'s comment records the
/// defect this prevents, and the failure mode is nasty: a figure built by
/// repeated addition never lands on an exact beat — three triplets of 1/3
/// sum to `0.9999999999999999`, and beat 2 arrives as
/// `1.9999999999999998`. Truncating instead of rounding puts those onsets one
/// tick *early*, so a phrase drifts against the grid by a tick per event and
/// reads as "the import is slightly loose" rather than as a bug.
///
/// The positions are built by summation on purpose — writing `2.0` as a
/// literal makes the test pass under the mutation. The ticks are then read
/// back with `support`'s decoder, so `midly` is not asked to confirm its own
/// arithmetic.
#[test]
fn a_tick_is_rounded_to_nearest_not_truncated() {
    // Six consecutive thirds: lands on 1.0 and 2.0 only approximately.
    let mut beats = Vec::new();
    let mut b = 0.0f64;
    for _ in 0..7 {
        beats.push(b);
        b += 1.0 / 3.0;
    }
    assert!(
        beats[6] != 2.0,
        "the fixture must accumulate error, or this test proves nothing — got \
         exactly {}",
        beats[6]
    );

    let events: Vec<SmfTimedEvent> = beats.iter().map(|&b| note_on(b, 60)).collect();
    let bytes = smf::to_bytes(&[events], &config()).expect("encodes");

    let decoded = decode_smf(&bytes).expect("the writer must emit decodable SMF");
    let ticks = decoded.note_on_ticks();
    assert_eq!(ticks.len(), 7);

    // Every third of a beat at 480 tpq is 160 ticks exactly.
    let expected: Vec<u64> = (0..7).map(|i| (i as f64 * 160.0).round() as u64).collect();
    assert_eq!(
        ticks, expected,
        "positions summed from 1/3 must round to the nearest tick. Truncation \
         puts the 3rd and 6th onsets at 479 and 959 instead of 480 and 960."
    );
}

/// **Parse → write → parse is byte-stable.**
///
/// Any nondeterminism in the merge order or in meta-event placement shows up
/// here and nowhere else: the existing round-trip test compares *parsed
/// values*, which are equal under any ordering of simultaneous events.
///
/// Honest about its strength: an unstable sort over equal keys is only
/// *observably* unstable when the implementation happens to reorder, so this
/// catches the mutation probabilistically rather than always. It is still
/// worth having — it is the only thing in the crate that would catch a
/// deliberate change to tie-breaking — and saying so beats implying a
/// guarantee it does not give.
#[test]
fn parse_write_parse_is_byte_stable() {
    // Deliberately dense in simultaneous events: four notes share beat 1, so
    // any tie-break instability has somewhere to show.
    let mut events = vec![note_on(0.0, 60), note_off(0.5, 60)];
    for key in [62u8, 64, 65, 67] {
        events.push(note_on(1.0, key));
        events.push(note_off(2.0, key));
    }

    let first = smf::to_bytes(&[events], &config()).expect("encodes");
    let parsed = smf::ParsedMidiFile::from_bytes(&first).expect("parses");
    let reencoded = smf::to_bytes(
        std::slice::from_ref(&parsed.events),
        &MidiWriteConfig {
            ticks_per_beat: parsed.ticks_per_beat,
            tempo_bpm: Some(parsed.tempo_bpm),
            time_signature: Some((4, 4)),
        },
    )
    .expect("re-encodes");

    assert_eq!(
        first, reencoded,
        "a parse and re-encode must reproduce the same bytes; a difference \
         means the event order is not determined by the data alone"
    );
}

/// **A tempo outside the wire's range writes the nearest value it holds**, not
/// the wire zero ("no valid tempo") and not a wrapped 24-bit field.
///
/// A `Bpm` is never zero or negative, so the wire zero can only come from a
/// tempo above 60 million BPM (the quotient casts to 0), and a tempo below
/// about 3.6 BPM overflows the 24-bit field, which masks. Reading the Set Tempo
/// payload back with the independent decoder closes the loop.
///
/// Mutation (run): drop the `.clamp(1, ..)` in `build_track` for a plain
/// `as u32`: the fast case reads back 0 and fails. Mutation (run): clamp only
/// the lower bound: the slow case reads back a wrapped value and fails.
#[test]
fn a_tempo_above_the_wire_range_writes_the_fastest_quarter() {
    for (bpm, want_us) in [
        (1.0e9, 1),
        (6.0e7, 1),
        (120.0, 500_000),
        (1.0, 0xFF_FFFF),
        (f64::MIN_POSITIVE, 0xFF_FFFF),
    ] {
        let bytes = smf::to_bytes(
            &[vec![note_on(0.0, 60), note_off(1.0, 60)]],
            &MidiWriteConfig {
                ticks_per_beat: TPQ,
                tempo_bpm: Some(Bpm::of(bpm)),
                time_signature: None,
            },
        )
        .expect("encodes");

        let decoded = decode_smf(&bytes).expect("decodable");
        assert_eq!(decoded.first_tempo_us(), Some(want_us), "{bpm} BPM");

        // And the reader's half: a usable tempo, never absent or infinite.
        let parsed = smf::ParsedMidiFile::from_bytes(&bytes).expect("parses");
        assert!(
            parsed.tempo_bpm.get().is_finite() && parsed.tempo_bpm.get() > 0.0,
            "got {}",
            parsed.tempo_bpm.get()
        );
    }
}

/// The writer's own output must be readable by an independent decoder at all
/// — chunk lengths, VLQ deltas and the mandatory End of Track.
///
/// Cheap, and it is the assumption every other test in this file rests on.
#[test]
fn the_writers_framing_is_decodable_by_a_foreign_reader() {
    let bytes =
        smf::to_bytes(&[vec![note_on(0.0, 60), note_off(4.0, 60)]], &config()).expect("encodes");

    let decoded = decode_smf(&bytes).expect("an independent decoder must accept the framing");
    assert_eq!(
        decoded.ticks_per_quarter(),
        Some(TPQ),
        "the header must declare the tpq it was configured with"
    );
    assert!(
        decoded.tracks.iter().all(|t| t
            .last()
            .is_some_and(|e| e.status == 0xFF && e.data.first() == Some(&0x2F))),
        "every track must end with the mandatory End of Track meta event"
    );
}

/// An empty track list is refused rather than producing a headerless file.
#[test]
fn an_empty_track_list_is_refused() {
    let err = smf::to_bytes(&[], &config()).expect_err("a file with no tracks is not a file");
    assert!(
        matches!(err, tutti_midi_file::Error::InvalidConfig(_)),
        "expected InvalidConfig, got {err:?}"
    );
}
