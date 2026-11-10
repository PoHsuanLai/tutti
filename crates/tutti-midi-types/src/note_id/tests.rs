use super::*;
use tutti_ump::{MidiChannel, NoteNumber};

#[test]
fn same_note_number_distinct_ids_do_not_collide() {
    // The whole point: two live notes on note 60 must stay independent.
    let a = NoteId::from_channel_note(MidiChannel::of(0), NoteNumber::of(60));
    let b = NoteId::from_raw(0xDEAD_0000); // rotation-minted, same sounding pitch
    assert_ne!(a, b);

    let mut map: PerNoteMap<i32, 8> = PerNoteMap::new();
    assert!(map.insert(a, 1));
    assert!(map.insert(b, 2));
    assert_eq!(map.get(a), Some(&1));
    assert_eq!(map.get(b), Some(&2));
    assert_eq!(map.remove(a), Some(1));
    assert_eq!(map.get(b), Some(&2));
}

#[test]
fn full_map_rejects_new_ids() {
    let mut map: PerNoteMap<u8, 2> = PerNoteMap::new();
    assert!(map.insert(NoteId::from_raw(1), 1));
    assert!(map.insert(NoteId::from_raw(2), 2));
    assert!(!map.insert(NoteId::from_raw(3), 3));
    // existing key still writable when full
    assert!(map.insert(NoteId::from_raw(1), 9));
    assert_eq!(map.get(NoteId::from_raw(1)), Some(&9));
}
