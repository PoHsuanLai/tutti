use super::*;

#[test]
fn channel_note_roundtrip() {
    let id = NoteId::from_channel_note(MidiChannel::of(9), 60);
    assert_eq!(id.channel(), MidiChannel::of(9));
    assert_eq!(id.note_number(), 60);
}

#[test]
fn same_note_number_distinct_ids_do_not_collide() {
    // The whole point: two live notes on note 60 must stay independent.
    let a = NoteId::from_channel_note(MidiChannel::of(0), 60);
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

/// An over-wide note number is masked into its 7 bits, never carried into
/// the channel field.
///
/// The doc on [`NoteId::from_channel_note`] promises a `note` ≥ 128 "can
/// never alias a different pair or read back changed". Without the mask the
/// 8th bit lands in the channel nibble, so note 128 on channel 0 would be
/// indistinguishable from note 0 on channel 1 — two live notes silently
/// sharing one id.
#[test]
fn an_over_wide_note_is_masked_rather_than_aliased() {
    for note in [128u8, 129, 200, 255] {
        let id = NoteId::from_channel_note(MidiChannel::of(0), note);
        assert_eq!(
            id.channel(),
            MidiChannel::of(0),
            "note {note} moved channel"
        );
        assert_eq!(
            id.note_number(),
            note & 0x7f,
            "note {note} read back changed"
        );
        assert_ne!(
            id,
            NoteId::from_channel_note(MidiChannel::of(1), note & 0x7f),
            "note {note} aliased onto the next channel"
        );
    }
}

#[test]
fn ord_makes_it_a_btree_key() {
    use std::collections::BTreeMap;
    let mut m = BTreeMap::new();
    m.insert(NoteId::from_channel_note(MidiChannel::of(0), 64), "b");
    m.insert(NoteId::from_channel_note(MidiChannel::of(0), 60), "a");
    // BTreeMap requires Ord; keys come back sorted.
    let notes: Vec<u8> = m.keys().map(|k| k.note_number()).collect();
    assert_eq!(notes, [60, 64]);
}
