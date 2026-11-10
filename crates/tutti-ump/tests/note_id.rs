//! `NoteId`: `channel * 128 + note`, the plugin formats' wire value, and the
//! ids an allocator mints above it.

use std::collections::BTreeMap;

use tutti_ump::{MidiChannel, NoteId, NoteNumber};

fn pair(channel: u8, note: u8) -> (MidiChannel, NoteNumber) {
    (MidiChannel::of(channel), NoteNumber::of(note))
}

/// Every one of the 16 x 128 pairs reads back as itself.
///
/// Mutation: pack the channel with `* 127`, or read it back with `% 127`:
/// the first pair that collides or reads back changed fails this.
#[test]
fn every_pair_round_trips() {
    for channel in 0..16 {
        for note in 0..128 {
            let (c, n) = pair(channel, note);
            let id = NoteId::from_channel_note(c, n);
            assert_eq!(id.channel_note(), Some((c, n)), "{channel}/{note}");
        }
    }
}

/// The id is `channel * 128 + note`, the value a CLAP or VST3 `note_id` carries,
/// and the pairs fill `0 ..= MAX_CHANNEL_NOTE` with no gap and no repeat.
///
/// Mutation: `<< 8` in place of `* 128` (the old encoding): the raw check fails.
#[test]
fn the_id_is_the_plugin_wire_value() {
    assert_eq!(NoteId::MAX_CHANNEL_NOTE, 2047);
    let mut seen = vec![false; 2048];
    for channel in 0..16u8 {
        for note in 0..128u8 {
            let (c, n) = pair(channel, note);
            let raw = NoteId::from_channel_note(c, n).raw();
            assert_eq!(raw, u32::from(channel) * 128 + u32::from(note));
            assert_eq!(u32::from(NoteId::from_raw(raw)), raw);
            assert!(!std::mem::replace(&mut seen[raw as usize], true));
        }
    }
    assert!(seen.into_iter().all(|s| s));
}

/// An id above the largest pair names no pair, so a minted id is never read as
/// one.
///
/// Mutation: test `>=` against `MAX_CHANNEL_NOTE`, or drop the check in
/// `channel_note`: the first assertion (2047 is still a pair) or the rest fail.
#[test]
fn a_raw_above_the_max_names_no_pair() {
    assert_eq!(
        NoteId::from_raw(NoteId::MAX_CHANNEL_NOTE).channel_note(),
        Some(pair(15, 127))
    );
    for raw in [NoteId::MAX_CHANNEL_NOTE + 1, 0xDEAD_0000, u32::MAX] {
        assert_eq!(NoteId::from_raw(raw).channel_note(), None, "{raw}");
    }
}

/// `Ord` makes an id a `BTreeMap` key, in raw order.
///
/// Mutation: derive no `Ord` and the file fails to compile; compare by note
/// descending by hand and the order assertion fails.
#[test]
fn ord_makes_it_a_btree_key() {
    let (c, a) = pair(0, 60);
    let (_, b) = pair(0, 64);
    let mut m = BTreeMap::new();
    m.insert(NoteId::from_channel_note(c, b), "b");
    m.insert(NoteId::from_channel_note(c, a), "a");
    assert_eq!(m.into_values().collect::<Vec<_>>(), ["a", "b"]);
}
