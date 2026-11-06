use super::*;
use tutti_ump::{MidiChannel, MidiGroup};

fn at(note: u8, offset: u32) -> MidiEvent {
    MidiEvent::note_on_7bit(MidiGroup::FIRST, MidiChannel::FIRST, note, 100)
        .with_frame_offset(offset)
}

/// Sorted by offset, arrival order kept at one offset; a late offset
/// placed on the block's last frame.
///
/// Mutation (run): `>=` for `>` in the sort (unstable at ties) → 2
/// before 1 → fails. Mutation (run): no clamp in `offset_of` → `None`,
/// placed on frame 0 → fails.
#[test]
fn sorts_stably_and_places_late_offsets_last() {
    let mut v = [at(1, 10), at(0, 5), at(2, 10), at(4, 900), at(3, 0)];
    in_offset_order(&mut v);
    let got: Vec<(u8, u32)> = v
        .iter()
        .map(|e| (e.note().unwrap(), e.frame_offset))
        .collect();
    assert_eq!(got, vec![(3, 0), (0, 5), (1, 10), (2, 10), (4, 900)]);
    assert_eq!(offset_of(&v[4], Samples(64)).get(), 63);
}
