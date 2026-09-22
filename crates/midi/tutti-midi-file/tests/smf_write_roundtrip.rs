//! The writer, checked with a decoder that is not `midly`.
//!
//! `smf.rs`'s inline write tests assert byte-exact output for one hand-picked
//! delta sequence and round-trip through `ParsedMidiFile::parse`. Both are
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
//! | write `Bpm(0.0)` as a saturated `u32` | `a_nonpositive_bpm_writes_the_wire_zero` |

mod support;

use support::decode_smf;
use tutti_core::{Beat, Bpm};
use tutti_midi_file::smf::{self, MidiWriteConfig, SmfTimedEvent};

const TPQ: u16 = 480;

fn note_on(beat: f64, key: u8) -> SmfTimedEvent {
    SmfTimedEvent {
        time_beats: Beat(beat),
        channel: 0,
        msg: midly::MidiMessage::NoteOn {
            key: key.into(),
            vel: 100.into(),
        },
    }
}

fn note_off(beat: f64, key: u8) -> SmfTimedEvent {
    SmfTimedEvent {
        time_beats: Beat(beat),
        channel: 0,
        msg: midly::MidiMessage::NoteOff {
            key: key.into(),
            vel: 0.into(),
        },
    }
}

fn config() -> MidiWriteConfig {
    MidiWriteConfig {
        ticks_per_beat: TPQ,
        tempo_bpm: Some(Bpm(120.0)),
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
    let bytes = smf::encode_midi_file(&[events], &config()).expect("encodes");

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

    let first = smf::encode_midi_file(&[events], &config()).expect("encodes");
    let parsed = smf::ParsedMidiFile::parse(&first).expect("parses");
    let reencoded = smf::encode_midi_file(
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

/// **A non-positive BPM writes the wire zero rather than a saturated tempo.**
///
/// The read side of this guard is already tested
/// (`a_degenerate_tempo_is_absent_rather_than_infinite`); the write side was
/// asserted only through `midly`. Reading the Set Tempo payload back with the
/// independent decoder closes the loop: `Bpm(0.0)` must produce
/// microseconds-per-quarter of 0, which the reader then reports as *absent*,
/// not `u32::MAX`, which it would report as an absurd-but-finite tempo.
#[test]
fn a_nonpositive_bpm_writes_the_wire_zero() {
    let bytes = smf::encode_midi_file(
        &[vec![note_on(0.0, 60), note_off(1.0, 60)]],
        &MidiWriteConfig {
            ticks_per_beat: TPQ,
            tempo_bpm: Some(Bpm(0.0)),
            time_signature: None,
        },
    )
    .expect("encodes");

    let decoded = decode_smf(&bytes).expect("decodable");
    assert_eq!(
        decoded.first_tempo_us(),
        Some(0),
        "a zero BPM must be written as a zero microseconds-per-quarter, not \
         saturated — a saturated value reads back as a real (tiny) tempo"
    );

    // And the reader's half: absent, not infinite, not 0 BPM.
    let parsed = smf::ParsedMidiFile::parse(&bytes).expect("parses");
    assert!(
        parsed.tempo_bpm.get().is_finite() && parsed.tempo_bpm.get() > 0.0,
        "a file declaring a degenerate tempo must fall back to a usable one, \
         got {}",
        parsed.tempo_bpm.get()
    );
}

/// The writer's own output must be readable by an independent decoder at all
/// — chunk lengths, VLQ deltas and the mandatory End of Track.
///
/// Cheap, and it is the assumption every other test in this file rests on.
#[test]
fn the_writers_framing_is_decodable_by_a_foreign_reader() {
    let bytes = smf::encode_midi_file(&[vec![note_on(0.0, 60), note_off(4.0, 60)]], &config())
        .expect("encodes");

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
    let err =
        smf::encode_midi_file(&[], &config()).expect_err("a file with no tracks is not a file");
    assert!(
        matches!(err, tutti_midi_file::Error::InvalidConfig(_)),
        "expected InvalidConfig, got {err:?}"
    );
}
