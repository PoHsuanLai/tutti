use super::*;
use crate::test_support::at;

use tutti_fixtures::midi;
use tutti_graph::UmpAt;
use tutti_midi_protocol::widen_velocity;
use tutti_ump::{KeyEvent, MidiChannel, U7};

/// A note at `offset`, which may lie past the 64-frame block the placement test
/// uses.
fn note_at(note: u8, offset: usize) -> UmpAt {
    at(
        offset,
        // 7-bit velocity 100, widened (M2-115).
        midi::note_on(MidiChannel::FIRST, note, widen_velocity(U7::of(100)).get()),
    )
}

/// Sorted by offset, arrival order kept at one offset; a late offset
/// placed on the block's last frame.
#[test]
fn sorts_stably_and_places_late_offsets_last() {
    let mut v = [
        note_at(1, 10),
        note_at(0, 5),
        note_at(2, 10),
        note_at(4, 900),
        note_at(3, 0),
    ];
    in_offset_order(&mut v);
    let got: Vec<(u8, u32)> = v
        .iter()
        .map(|e| {
            (
                match e.1.key_event() {
                    Some(KeyEvent::On(key) | KeyEvent::Off(key)) => key.note().get(),
                    other => panic!("a note, got {other:?}"),
                },
                e.0.get(),
            )
        })
        .collect();
    assert_eq!(got, vec![(3, 0), (0, 5), (1, 10), (2, 10), (4, 900)]);
    assert_eq!(offset_of(&v[4], Samples(64)).get(), 63);
}
