//! `tutti-midi-file`'s reader against bytes built from the SMF spec.
//!
//! This crate had 13 tests, all inline, and **no integration tests and no
//! input files of any kind**. What coverage there was pointed at degenerate
//! headers (a zero division, a zero tempo) and at byte-exact encoder output.
//! The *hand-rolled* layer — everything this crate adds on top of `midly` —
//! was reached only through its own writer, so any asymmetry shared by both
//! halves was invisible.
//!
//! That layer is where the risk is. `midly` does chunk framing, varints and
//! running status; `smf.rs` does tick↔`Beat` conversion, the zero guards,
//! SMPTE rejection, cross-track merge and stable sort, note pairing keyed
//! `(channel, key)` with LIFO overlap and velocity-0-as-off, track naming, and
//! format selection. None of that is `midly`'s.
//!
//! Inputs come from `support`, an independent encoder/decoder written from the
//! spec — see its header for why that rather than an oracle crate or a
//! committed corpus.
//!
//! Mutations run against `smf.rs`, and which test each broke:
//!
//! | mutation | fails |
//! |---|---|
//! | move `tick +=` inside the `if let TrackEventKind::Midi` arm | `a_sysex_between_notes_does_not_shift_the_beat_grid` |
//! | `stack.pop()` → `stack.remove(0)` (FIFO pairing) | `overlapping_same_key_notes_pair_lifo` |
//! | drop the `channel` from the pairing key | `a_note_off_does_not_close_another_channels_note` |
//! | `Timing::Timecode(..) =>` guess a tpb instead of erroring | `smpte_division_is_rejected_at_both_entry_points` |
//! | delete the `ticks_per_beat == 0` guard | `a_zero_division_is_rejected_at_both_entry_points` |

mod support;

use support::{build_smf, Division, RawEvent};
use tutti_midi_file::smf::{self, ParsedMidiFile};

/// 480 ticks per quarter, the value `MidiWriteConfig` defaults to.
const TPQ: u16 = 480;

fn metrical(tracks: &[Vec<RawEvent>]) -> Vec<u8> {
    build_smf(1, Division::Metrical(TPQ), tracks)
}

/// **A delta on a non-MIDI event must still advance the clock.**
///
/// The highest-value test here. `parse_track` and `pair_notes` both accumulate
/// the running tick *before* filtering for channel messages. Move that
/// accumulation inside the filter — an easy and natural-looking edit — and
/// every onset after any meta or sysex event slides early by that event's
/// delta. It is silent, it is cumulative, and no existing test could see it,
/// because the inline tests only ever build tracks of pure note events.
///
/// The note lands one beat after a sysex that itself sits one beat in, so the
/// correct answer is beat 2 and the mutated one is beat 1.
#[test]
fn a_sysex_between_notes_does_not_shift_the_beat_grid() {
    let data = metrical(&[vec![
        RawEvent::note_on(0, 0, 60, 100),
        RawEvent::note_off(u32::from(TPQ) / 2, 0, 60, 0),
        // A full beat of silence, spent on a sysex rather than on a note.
        RawEvent::sysex(u32::from(TPQ) / 2, &[0x7E, 0x7F, 0x09, 0x01]),
        RawEvent::note_on(u32::from(TPQ), 0, 62, 100),
        RawEvent::note_off(u32::from(TPQ) / 2, 0, 62, 0),
    ]]);

    let parsed = ParsedMidiFile::parse(&data).expect("a well-formed file parses");
    let onsets: Vec<f64> = parsed
        .events
        .iter()
        .filter(|e| matches!(e.msg, midly::MidiMessage::NoteOn { vel, .. } if vel.as_int() > 0))
        .map(|e| e.time_beats.get())
        .collect();
    assert_eq!(
        onsets.len(),
        2,
        "both note-ons should survive the sysex in between"
    );
    assert!(
        (onsets[1] - 2.0).abs() < 1e-9,
        "the second note sits a beat after a sysex a beat in, so beat 2 — got \
         {}. A beat of 1.0 means a non-MIDI event's delta was not counted.",
        onsets[1]
    );

    // The same property through the other entry point, which keeps its own
    // running tick in `pair_notes`.
    let tracks = smf::tracks(&data).expect("a well-formed file parses");
    let note = tracks[0]
        .notes
        .iter()
        .find(|n| n.key == 62)
        .expect("the post-sysex note should be paired");
    assert!(
        (note.start_beats.get() - 2.0).abs() < 1e-9,
        "pair_notes must count the sysex delta too — got {}",
        note.start_beats.get()
    );
}

/// **Overlapping notes on one key close innermost-first.**
///
/// `pair_notes`'s doc claims LIFO. Nothing checked it, and FIFO is the more
/// obvious thing to write. The two velocities make the pairing observable:
/// under LIFO the *second* onset takes the *first* offset, so the short
/// duration belongs to velocity 111.
#[test]
fn overlapping_same_key_notes_pair_lifo() {
    let q = u32::from(TPQ);
    let data = metrical(&[vec![
        RawEvent::note_on(0, 0, 60, 100), // onset A at beat 0
        RawEvent::note_on(q, 0, 60, 111), // onset B at beat 1
        RawEvent::note_off(q, 0, 60, 0),  // beat 2 — closes B under LIFO
        RawEvent::note_off(q, 0, 60, 0),  // beat 3 — closes A
    ]]);

    let tracks = smf::tracks(&data).expect("parses");
    let notes = &tracks[0].notes;
    assert_eq!(notes.len(), 2);

    let b = notes
        .iter()
        .find(|n| n.velocity == 111)
        .expect("the inner note");
    let a = notes
        .iter()
        .find(|n| n.velocity == 100)
        .expect("the outer note");
    assert!(
        (b.duration_beats.get() - 1.0).abs() < 1e-9,
        "LIFO: the later onset (vel 111) takes the earlier offset, so 1 beat — \
         got {}. 2 beats means FIFO.",
        b.duration_beats.get()
    );
    assert!(
        (a.duration_beats.get() - 3.0).abs() < 1e-9,
        "and the outer note spans to the last offset — got {}",
        a.duration_beats.get()
    );
}

/// **Pairing is per channel.** A NoteOff on channel 1 must not close a NoteOn
/// of the same key on channel 0 — the norm for General MIDI Type-0 files,
/// where every instrument shares one track.
///
/// The onsets are *staggered* and the offsets arrive in the same order as the
/// onsets, which matters: an earlier draft of this test opened both notes at
/// beat 0 and closed them innermost-first, and a `held` map keyed on the key
/// alone produced **identical** durations, because LIFO popping happened to
/// hand each offset back to the right onset. It passed under the mutation it
/// was written to catch. With the offsets in onset order, merging the channels
/// pairs them crosswise and the two answers separate: correct is (ch0: 0→2,
/// ch1: 1→3); channel-blind is (ch0: 1→2, ch1: 0→3).
#[test]
fn a_note_off_does_not_close_another_channels_note() {
    let q = u32::from(TPQ);
    let data = metrical(&[vec![
        RawEvent::note_on(0, 0, 60, 100), // ch0 onset at beat 0
        RawEvent::note_on(q, 1, 60, 100), // ch1 onset at beat 1
        RawEvent::note_off(q, 0, 60, 0),  // beat 2 — ch0's own offset
        RawEvent::note_off(q, 1, 60, 0),  // beat 3 — ch1's own offset
    ]]);

    let notes = &smf::tracks(&data).expect("parses")[0].notes;
    assert_eq!(notes.len(), 2, "one note per channel");
    let ch0 = notes.iter().find(|n| n.channel == 0).expect("channel 0");
    let ch1 = notes.iter().find(|n| n.channel == 1).expect("channel 1");

    assert!(
        (ch0.start_beats.get() - 0.0).abs() < 1e-9 && (ch0.duration_beats.get() - 2.0).abs() < 1e-9,
        "channel 0 runs beat 0→2; got start {} dur {}. A start of 1 means the \
         offsets were paired without regard to channel.",
        ch0.start_beats.get(),
        ch0.duration_beats.get()
    );
    assert!(
        (ch1.start_beats.get() - 1.0).abs() < 1e-9 && (ch1.duration_beats.get() - 2.0).abs() < 1e-9,
        "channel 1 runs beat 1→3; got start {} dur {}",
        ch1.start_beats.get(),
        ch1.duration_beats.get()
    );
}

/// **A velocity-0 Note On is a Note Off.** Universally used by hardware that
/// exploits running status, and therefore common in real files.
#[test]
fn a_velocity_zero_note_on_closes_the_note() {
    let q = u32::from(TPQ);
    let data = metrical(&[vec![
        RawEvent::note_on(0, 0, 64, 90),
        RawEvent::note_on(q, 0, 64, 0), // the "off"
    ]]);

    let notes = &smf::tracks(&data).expect("parses")[0].notes;
    assert_eq!(
        notes.len(),
        1,
        "a velocity-0 note-on must close the note, not open a second one"
    );
    assert!((notes[0].duration_beats.get() - 1.0).abs() < 1e-9);
    assert_eq!(notes[0].velocity, 90, "the onset velocity is the note's");
}

/// **Running status parses.** A contract test on `midly` rather than on tutti
/// code — said plainly, because its value is catching a `midly` upgrade that
/// changes behaviour underneath `tracks()`, not proving something this crate
/// does. Without it, a file written by any hardware sequencer would be read
/// short and nothing here would notice.
#[test]
fn running_status_notes_survive_the_reader() {
    let q = u32::from(TPQ);
    let data = metrical(&[vec![
        RawEvent::note_on(0, 0, 60, 100),
        // Status omitted: still Note On, channel 0.
        RawEvent::running_status(q, vec![62u8, 100]),
        RawEvent::running_status(q, vec![60u8, 0]), // vel 0 → off for 60
        RawEvent::running_status(q, vec![62u8, 0]), // → off for 62
    ]]);

    let notes = &smf::tracks(&data).expect("parses")[0].notes;
    assert_eq!(
        notes.len(),
        2,
        "both notes must survive; a reader that dropped status-less events \
         would report {} of them",
        notes.len()
    );
    assert!(notes.iter().any(|n| n.key == 60));
    assert!(notes.iter().any(|n| n.key == 62));
}

/// **SMPTE division is refused by both entry points.**
///
/// A timecode division is not a tempo-relative grid, so every `Beat` this
/// crate produces would be meaningless. `ParsedMidiFile::parse` and `tracks`
/// each carry their own `match` on the timing, so each needs its own check —
/// an inline test covered neither at this level.
#[test]
fn smpte_division_is_rejected_at_both_entry_points() {
    let data = build_smf(
        0,
        Division::Smpte {
            fps: -25,
            ticks_per_frame: 40,
        },
        &[vec![RawEvent::note_on(0, 0, 60, 100)]],
    );

    assert!(
        matches!(
            ParsedMidiFile::parse(&data),
            Err(tutti_midi_file::Error::MidiUnsupportedTiming)
        ),
        "parse must refuse a timecode division rather than invent a tpb"
    );
    assert!(
        matches!(
            smf::tracks(&data),
            Err(tutti_midi_file::Error::MidiUnsupportedTiming)
        ),
        "tracks must refuse it too — it has its own timing match"
    );
}

/// **A zero division is refused by both entry points.** It is the divisor for
/// every beat in the file, so letting one through returns a parse in which
/// every event sits at infinity.
#[test]
fn a_zero_division_is_rejected_at_both_entry_points() {
    let data = build_smf(
        0,
        Division::Metrical(0),
        &[vec![RawEvent::note_on(0, 0, 60, 100)]],
    );

    assert!(
        ParsedMidiFile::parse(&data).is_err(),
        "parse must refuse a zero ticks-per-quarter"
    );
    assert!(
        smf::tracks(&data).is_err(),
        "tracks must refuse it too — it has its own guard"
    );
}

/// **Format 1: tracks stay separate and keep their names.**
///
/// `tracks()` is the per-track view an importer uses to make one clip per
/// track. Flattening would be invisible to `ParsedMidiFile`, which merges by
/// design.
#[test]
fn format_1_multitrack_keeps_tracks_and_names_separate() {
    let q = u32::from(TPQ);
    let data = build_smf(
        1,
        Division::Metrical(TPQ),
        &[
            // Track 0: conductor — name and tempo, no notes.
            vec![
                RawEvent::track_name(0, "Conductor"),
                RawEvent::tempo(0, 500_000), // 120 BPM
                RawEvent::time_signature(0, 3, 2),
            ],
            vec![
                RawEvent::track_name(0, "Bass"),
                RawEvent::note_on(0, 0, 36, 100),
                RawEvent::note_off(q, 0, 36, 0),
            ],
            vec![
                RawEvent::track_name(0, "Lead"),
                RawEvent::note_on(0, 1, 72, 80),
                RawEvent::note_off(q * 2, 1, 72, 0),
            ],
        ],
    );

    let tracks = smf::tracks(&data).expect("parses");
    assert_eq!(tracks.len(), 3, "three chunks, three tracks");
    assert_eq!(tracks[0].name.as_deref(), Some("Conductor"));
    assert_eq!(tracks[1].name.as_deref(), Some("Bass"));
    assert_eq!(tracks[2].name.as_deref(), Some("Lead"));
    assert!(
        tracks[0].notes.is_empty(),
        "the conductor track has no notes"
    );
    assert_eq!(tracks[1].notes.len(), 1);
    assert_eq!(tracks[2].notes.len(), 1);
    assert_eq!(tracks[1].notes[0].key, 36);
    assert_eq!(tracks[2].notes[0].key, 72);

    // And the merged view finds the tempo from track 0 while carrying both
    // tracks' notes — the cross-track merge `ParsedMidiFile` exists for.
    let parsed = ParsedMidiFile::parse(&data).expect("parses");
    assert!((parsed.tempo_bpm.get() - 120.0).abs() < 1e-6);
    assert_eq!(
        parsed.events.len(),
        4,
        "two note-ons and two note-offs, merged across tracks"
    );
}

/// **The merged stream is sorted by time, across tracks.**
///
/// `sort_by_time` is what makes `get_events_in_range`'s binary search valid.
/// An unsorted merge would make that search return arbitrary slices, and
/// nothing would fail loudly.
#[test]
fn merged_events_are_sorted_across_tracks() {
    let q = u32::from(TPQ);
    let data = build_smf(
        1,
        Division::Metrical(TPQ),
        &[
            // Deliberately interleaved: track 0's events straddle track 1's.
            vec![
                RawEvent::note_on(0, 0, 60, 100),
                RawEvent::note_off(q * 4, 0, 60, 0),
            ],
            vec![
                RawEvent::note_on(q * 2, 1, 67, 100),
                RawEvent::note_off(q, 1, 67, 0),
            ],
        ],
    );

    let parsed = ParsedMidiFile::parse(&data).expect("parses");
    let times: Vec<f64> = parsed.events.iter().map(|e| e.time_beats.get()).collect();
    assert!(
        times.windows(2).all(|w| w[0] <= w[1]),
        "merged events must be in time order, got {times:?}"
    );
    assert_eq!(times.len(), 4);

    // And the range query tiles correctly over that order.
    let in_first_bar = parsed.get_events_in_range(tutti_core::Beat(0.0), tutti_core::Beat(3.0));
    assert_eq!(
        in_first_bar.len(),
        2,
        "beats [0,3) hold the two onsets at 0 and 2, and neither offset"
    );
}
