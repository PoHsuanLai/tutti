use super::*;
use tutti_fixtures::midi::note_on;
use tutti_graph::UmpAt;
use tutti_ump::{KeyEvent, MidiChannel, MidiGroup};

/// An event at `offset`, which may lie past the 64-frame block the placement
/// test uses: it is built against a longer one.
fn at(note: u8, offset: u32) -> UmpAt {
    (
        Offset::new(offset as usize, Samples(1024)).unwrap(),
        // 7-bit velocity 100, widened (M2-115).
        note_on(
            MidiGroup::FIRST,
            MidiChannel::FIRST,
            note,
            tutti_fixtures::midi::wide_velocity(100),
        ),
    )
}

/// Sorted by offset, arrival order kept at one offset; a late offset
/// placed on the block's last frame.
#[test]
fn sorts_stably_and_places_late_offsets_last() {
    let mut v = [at(1, 10), at(0, 5), at(2, 10), at(4, 900), at(3, 0)];
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
