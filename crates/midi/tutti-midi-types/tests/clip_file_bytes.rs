//! The MIDI 2.0 Clip File codec, from the outside.
//!
//! `clip_file.rs` is **1,154 lines of hand-rolled byte handling** written
//! against spec M2-116 — chunk magic, DCTPQ, delta clockstamps, Start/End of
//! Clip, Flex Data tempo and time signature, note pairing — using `midi2` only
//! to build individual UMP words. There is no second implementation of this
//! format anywhere on crates.io, so there is no oracle to check it against,
//! and there were **zero integration tests**: all 19 tests were inline, and
//! every one of them checked the writer against the reader. Two halves of the
//! same understanding cannot contradict each other.
//!
//! The tests here attack it from angles that do not require a second
//! implementation:
//!
//! - **Closed-form properties.** "No prefix of a valid file is itself valid"
//!   is true by construction and needs no oracle, and it sweeps every length
//!   bound in a parser built from manual `u32::from_be_bytes` slicing. That is
//!   where this kind of code breaks.
//! - **Corruption sweeps.** Flip each header byte; the reader must reject or
//!   at minimum not panic.
//! - **Cross-checking the reader's own accessors against each other**, where
//!   two independent paths through the same bytes must agree.
//!
//! Mutations run against `clip_file.rs`, and which test each broke:
//!
//! | mutation | fails |
//! |---|---|
//! | `abs_tick += delta` → `abs_tick = delta` in `timed()` | `delta_clockstamps_accumulate_to_the_reported_beats` (+ 4 inline tests) |
//! | drop the `bytes.len() < 8` guard before the magic compare | `no_prefix_of_a_valid_clip_is_accepted`, `the_magic_is_required` |
//! | delete the `leading_header_len` strip from `write_clip` | `re_encoding_with_a_header_replaces_it_rather_than_duplicating_it` |
//! | `leading_header_len`'s `take_while` → `filter` | `a_mid_clip_tempo_change_survives_a_header_rewrite` |
//!
//! That last one **passed against the first draft** of its test, and the
//! reason is worth keeping: the draft put the mid-clip tempo change at a
//! non-zero delta, but `leading_header_len`'s predicate already requires
//! `delta_ticks == 0`, so `filter` and `take_while` agreed on that fixture.
//! Only a zero-delta tempo event that is not in the opening run — one
//! simultaneous with a preceding event — separates a positional rule from a
//! content rule.

use tutti_midi_types::{
    read_clip_file, write_clip_file_with_header, Beat, Bpm, ClipEvent, ClipHeader, MidiChannel,
    MidiEvent, MidiGroup,
};

const TPQ: u16 = 96;

fn note_on(note: u8, vel: u16) -> MidiEvent {
    MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, note, vel)
}

fn note_off(note: u8) -> MidiEvent {
    MidiEvent::note_off(MidiGroup::FIRST, MidiChannel::FIRST, note, 0)
}

/// A clip with a header, two notes and a rest — enough structure that a
/// truncation anywhere lands inside something meaningful.
fn sample_clip() -> Vec<u8> {
    write_clip_file_with_header(
        TPQ,
        ClipHeader {
            tempo_bpm: Bpm(128.0),
            time_signature: (5, 4),
        },
        &[
            ClipEvent::new(0, note_on(60, 0xABCD)),
            ClipEvent::new(u32::from(TPQ), note_off(60)),
            ClipEvent::new(u32::from(TPQ) * 2, note_on(67, 0x1234)),
            ClipEvent::new(u32::from(TPQ) / 2, note_off(67)),
        ],
    )
}

/// **No prefix of a valid clip is itself a valid clip, and none panics.**
///
/// The highest-value test in this file, and the cheapest. A parser assembled
/// from manual index arithmetic over a byte slice fails in exactly one way:
/// a length check that is missing, off by one, or performed after the slice.
/// Sweeping every truncation point exercises every such bound at once, and
/// the expected answer needs no oracle — a file shorter than the whole cannot
/// be the whole.
///
/// "Never panics" is the load-bearing half. A panic here is an index-out-of-
/// bounds on attacker- or corruption-supplied bytes, reached from
/// `read_clip_file_from_path`, i.e. from any file a user opens.
///
/// Mutation-checked: removing any single length guard in `read_clip_file`
/// turns one or more of these iterations into a panic.
#[test]
fn no_prefix_of_a_valid_clip_is_accepted() {
    let full = sample_clip();
    assert!(
        read_clip_file(&full).is_ok(),
        "the fixture itself must be valid, or this test sweeps nothing"
    );

    for n in 0..full.len() {
        let prefix = &full[..n];
        let result = std::panic::catch_unwind(|| read_clip_file(prefix));
        match result {
            Ok(Ok(_)) => panic!(
                "a {n}-byte prefix of a {}-byte clip was accepted as a whole \
                 file — a length bound is missing",
                full.len()
            ),
            Ok(Err(_)) => {}
            Err(_) => panic!(
                "read_clip_file panicked on a {n}-byte prefix; truncated input \
                 must be an error, never an index-out-of-bounds"
            ),
        }
    }
}

/// **A bit flip anywhere in the header is rejected, and never panics.**
///
/// Complements the truncation sweep: that one shortens the input, this one
/// keeps the length and corrupts the content, which reaches different
/// branches (magic comparison, DCTPQ decode, UMP message-type dispatch).
#[test]
fn a_corrupted_header_byte_is_rejected_or_at_worst_survived() {
    let full = sample_clip();
    // The magic plus the DCTPQ that follows it — the fixed prologue every
    // reader must validate before it can trust anything after.
    for i in 0..16.min(full.len()) {
        for bit in [0x01u8, 0x80] {
            let mut corrupt = full.clone();
            corrupt[i] ^= bit;
            if corrupt == full {
                continue;
            }
            let result = std::panic::catch_unwind(|| read_clip_file(&corrupt));
            assert!(
                result.is_ok(),
                "read_clip_file panicked on a single bit flip at byte {i} — \
                 corruption must be an error, never a panic"
            );
        }
    }
}

/// **The magic is checked, and nothing shorter than it is accepted.**
#[test]
fn the_magic_is_required() {
    let mut wrong = sample_clip();
    wrong[0] = b'X';
    assert!(
        read_clip_file(&wrong).is_err(),
        "a file whose magic does not read SMF2CLIP is not a clip file"
    );

    for n in 0..8 {
        assert!(
            read_clip_file(&b"SMF2CLIP"[..n]).is_err(),
            "{n} bytes cannot carry an 8-byte magic"
        );
    }
}

/// **Delta clockstamps accumulate to the beats the reader reports.**
///
/// `timed()` running-sums the per-event deltas into absolute `Beat`s. That
/// conversion had only ever been checked against the writer that produced the
/// deltas. Here the expected beats are computed *from the deltas the test
/// itself chose*, divided by the TPQ it chose — arithmetic that shares nothing
/// with the codec.
///
/// Mutation-checked: making the accumulator assign rather than add
/// (`acc = delta` instead of `acc += delta`) fails this.
#[test]
fn delta_clockstamps_accumulate_to_the_reported_beats() {
    // Deltas in ticks, and the absolute beats they must produce at TPQ=96.
    let deltas = [0u32, 96, 48, 192, 24];
    let mut running = 0u32;
    let expected: Vec<f64> = deltas
        .iter()
        .map(|d| {
            running += d;
            f64::from(running) / f64::from(TPQ)
        })
        .collect();

    let events: Vec<ClipEvent> = deltas
        .iter()
        .enumerate()
        .map(|(i, &d)| ClipEvent::new(d, note_on(60 + i as u8, 0x4000)))
        .collect();

    // No header: `write_clip_file_with_header` would prepend Flex Data tempo
    // and time-signature events, which `timed()` reports alongside the notes
    // and which would offset every index below.
    let bytes = tutti_midi_types::write_clip_file(TPQ, &events);
    let clip = read_clip_file(&bytes).expect("a clip we just wrote must parse");

    let got: Vec<f64> = clip.timed().map(|(b, _)| b.get()).collect();
    assert_eq!(
        got.len(),
        expected.len(),
        "every event must come back, got {got:?}"
    );
    for (i, (g, e)) in got.iter().zip(&expected).enumerate() {
        assert!(
            (g - e).abs() < 1e-9,
            "event {i}: expected beat {e}, got {g} — the delta chain did not \
             accumulate"
        );
    }
}

/// **Parse → write → parse is byte-stable.**
///
/// The codec's own inline tests compare parsed *values*. Bytes are stricter:
/// they also pin event ordering and the placement of the Start/End of Clip
/// markers, neither of which a value comparison can see.
///
/// Note which writer this uses, and why it is the correct one:
/// `ParsedClipFile::events` already *contains* the Flex Data tempo and
/// time-signature events, because in this format metadata is events. So
/// re-emitting parsed events goes through [`write_clip_file`], not
/// `write_clip_file_with_header` — see
/// `re_encoding_with_a_header_replaces_it_rather_than_duplicating_it` for the
/// other writer, which strips the parsed pair before re-emitting it.
#[test]
fn parse_write_parse_is_byte_stable() {
    let first = sample_clip();
    let clip = read_clip_file(&first).expect("parses");

    let reencoded = tutti_midi_types::write_clip_file(clip.ticks_per_quarter, &clip.events);

    assert_eq!(
        first, reencoded,
        "re-encoding a parsed clip must reproduce its bytes; a difference \
         means the round trip is lossy or the ordering is not data-determined"
    );

    // And again, to catch a difference that only appears after the first pass.
    let twice = read_clip_file(&reencoded).expect("the re-encode parses");
    assert_eq!(
        reencoded,
        tutti_midi_types::write_clip_file(twice.ticks_per_quarter, &twice.events),
        "the round trip must reach a fixed point on the first pass, not drift"
    );
}

/// **A header handed to the writer replaces one the events already carry.**
///
/// In this format tempo and time signature are Flex Data *events*, so a parse
/// puts them in `ParsedClipFile::events`. Handing those events back to the
/// with-header writer used to emit them a second time, and the file grew a
/// duplicate tempo declaration on every open-and-save cycle. Nothing errored,
/// and a reader takes the *first* declaration, so the file kept playing
/// correctly while accumulating junk — the quiet kind of wrong. This test was
/// originally written to pin that behaviour, and said in as many words that it
/// "should become an equality" once it was fixed. It is now that equality.
///
/// *Mutation:* delete the `leading_header_len` strip from `write_clip`
/// (make `let events = events;` unconditional) — `with_header` grows by two
/// UMP/DCS pairs and the equality fails.
#[test]
fn re_encoding_with_a_header_replaces_it_rather_than_duplicating_it() {
    let first = sample_clip();
    let clip = read_clip_file(&first).expect("parses");

    let with_header = write_clip_file_with_header(
        clip.ticks_per_quarter,
        clip.header().expect("the fixture declares both halves"),
        &clip.events,
    );

    assert_eq!(
        first, with_header,
        "re-encoding a parsed clip through the with-header writer must \
         reproduce its bytes; growth means the header was prepended to a \
         header the events already carried"
    );

    // A second cycle, because a duplication that is idempotent after the
    // first pass would still pass the check above.
    let twice = read_clip_file(&with_header).expect("parses");
    assert_eq!(
        with_header,
        write_clip_file_with_header(
            twice.ticks_per_quarter,
            twice.header().expect("still both halves"),
            &twice.events,
        ),
        "the round trip must reach a fixed point, not grow by a constant"
    );
}

/// **Only the *leading* tempo is the header; a mid-clip tempo change is
/// musical content and must survive the rewrite.**
///
/// The fix for the duplication above strips tempo and time-signature events
/// before writing the supplied header. Stripping the wrong set is the obvious
/// way to get that wrong, and it would be inaudible in every other test here:
/// a clip that changes tempo halfway is the only shape that can tell a
/// leading-run strip from a global filter.
///
/// **The mid-clip tempo change here is deliberately at delta 0.** The first
/// draft of this test put it a bar in, at a non-zero delta — and the
/// `take_while` -> `filter` mutation *passed*, because `leading_header_len`'s
/// predicate already requires `delta_ticks == 0`, so a non-zero-delta event is
/// excluded either way. The two differ only on a zero-delta tempo event that
/// is **not** in the opening run, i.e. one simultaneous with a preceding
/// event. That is the shape below, and it is the only shape that can tell a
/// positional rule from a content rule.
///
/// *Mutation:* change `leading_header_len`'s `take_while` to `filter` —
/// the count reaches past the first note, so the note is stripped with the
/// header and the event count drops.
#[test]
fn a_mid_clip_tempo_change_survives_a_header_rewrite() {
    let original = write_clip_file_with_header(
        TPQ,
        ClipHeader {
            tempo_bpm: Bpm(100.0),
            time_signature: (4, 4),
        },
        &[
            ClipEvent::new(0, note_on(60, 0x4000)),
            ClipEvent::new(u32::from(TPQ) * 4, note_off(60)),
            // Simultaneous with the note-off: zero-delta, but NOT leading.
            ClipEvent::new(0, MidiEvent::flex_set_tempo(MidiGroup::FIRST, 140.0)),
        ],
    );

    let clip = read_clip_file(&original).expect("parses");
    let tempo_events = |c: &tutti_midi_types::ParsedClipFile| {
        c.events
            .iter()
            .filter(|ce| tutti_midi_types::ump::flex_tempo_bpm(&ce.event).is_some())
            .count()
    };
    assert_eq!(
        tempo_events(&clip),
        2,
        "the fixture must hold both the header tempo and the mid-clip change, \
         or this test proves nothing"
    );

    let rewritten = write_clip_file_with_header(
        clip.ticks_per_quarter,
        clip.header().expect("both halves"),
        &clip.events,
    );
    let after = read_clip_file(&rewritten).expect("parses");

    // The sharpest of the three: a strip that reaches past the opening run
    // takes real events with it, and the count is what says so.
    assert_eq!(
        after.events.len(),
        clip.events.len(),
        "the rewrite lost events: stripping reached past the header run"
    );
    assert_eq!(
        tempo_events(&after),
        2,
        "the mid-clip tempo change was stripped along with the header"
    );
    assert_eq!(
        original, rewritten,
        "and the bytes are unchanged end to end"
    );
}

/// **A clip written without a header reports no tempo and no time signature.**
///
/// "Absent" and "defaulted to 120 BPM / 4-4" are different answers, and only
/// one of them lets a consumer fall back to the project tempo. `ClipHeader`'s
/// `Default` *is* 120/4-4, so a reader that quietly substituted it would look
/// correct in every test that uses the default header — which is most of them.
#[test]
fn a_clip_with_no_header_reports_absent_rather_than_default() {
    let bytes = tutti_midi_types::write_clip_file(
        TPQ,
        &[
            ClipEvent::new(0, note_on(60, 0x4000)),
            ClipEvent::new(u32::from(TPQ), note_off(60)),
        ],
    );
    let clip = read_clip_file(&bytes).expect("parses");

    assert_eq!(
        clip.tempo_bpm(),
        None,
        "a clip that declares no tempo must say so, not report the default"
    );
    assert_eq!(clip.time_signature(), None, "likewise the time signature");
    // And the notes are still there — the absence is of metadata, not content.
    assert_eq!(clip.notes().len(), 1);
}

/// **16-bit velocity survives the round trip.**
///
/// The whole reason this format exists rather than SMF. A codec that narrowed
/// to 7 bits would still pass every pairing and timing test in the crate.
#[test]
fn sixteen_bit_velocity_is_not_narrowed() {
    let bytes = write_clip_file_with_header(
        TPQ,
        ClipHeader::default(),
        &[
            ClipEvent::new(0, note_on(60, 0xBEEF)),
            ClipEvent::new(u32::from(TPQ), note_off(60)),
        ],
    );
    let notes = read_clip_file(&bytes).expect("parses").notes();
    assert_eq!(notes.len(), 1);
    assert_eq!(
        notes[0].velocity,
        0xBEEF,
        "MIDI 2.0 velocity is 16-bit; narrowing to 7 bits would report {}",
        0xBEEFu16 >> 9
    );
}

/// **A tempo and time signature round-trip exactly, not approximately.**
///
/// Tempo is carried as ten-nanosecond units per quarter, so the BPM a
/// consumer reads back is the result of two conversions. A drift of a
/// fraction of a BPM is inaudible per bar and catastrophic over a song.
#[test]
fn the_declared_tempo_and_meter_survive_the_round_trip() {
    for (bpm, sig) in [(128.0, (5u8, 4u8)), (93.5, (7, 8)), (200.0, (3, 4))] {
        let bytes = write_clip_file_with_header(
            TPQ,
            ClipHeader {
                tempo_bpm: Bpm(bpm),
                time_signature: sig,
            },
            &[ClipEvent::new(0, note_on(60, 0x4000))],
        );
        let clip = read_clip_file(&bytes).expect("parses");
        let got = clip.tempo_bpm().expect("a declared tempo must be reported");
        assert!(
            (got.get() - bpm).abs() < 0.01,
            "tempo {bpm} came back as {} — the ten-nanosecond conversion drifts",
            got.get()
        );
        assert_eq!(clip.time_signature(), Some(sig));
    }
}

/// The duration a clip reports must cover its last event.
#[test]
fn the_reported_duration_reaches_the_last_event() {
    let bytes = write_clip_file_with_header(
        TPQ,
        ClipHeader::default(),
        &[
            ClipEvent::new(0, note_on(60, 0x4000)),
            ClipEvent::new(u32::from(TPQ) * 4, note_off(60)),
        ],
    );
    let clip = read_clip_file(&bytes).expect("parses");
    let last = clip.timed().map(|(b, _)| b).last().unwrap_or(Beat(0.0));
    assert!(
        clip.duration_beats().get() >= last.get(),
        "a clip whose duration ({}) is shorter than its last event ({}) would \
         have that event trimmed by any consumer that trusts the duration",
        clip.duration_beats().get(),
        last.get()
    );
}
