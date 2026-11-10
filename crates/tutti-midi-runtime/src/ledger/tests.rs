use super::*;
use tutti_fixtures::midi::note_off;

/// **The off the ledger writes is the MIDI 2.0 Note Off at velocity 0 that
/// `note_off` builds**, on the key's group, channel and note.
///
/// Mutation (run): `NoteKey::off` on channel 0 → fails.
#[test]
fn an_owed_off_is_the_note_off_the_wire_types_build() {
    let key = NoteKey {
        group: MidiGroup::of(3),
        channel: MidiChannel::of(9),
        note: NoteNumber::of(61),
    };
    let off = key.off(Offset::ZERO);
    let want = note_off(key.group, key.channel, 61, 0);
    assert_eq!(off, Event::midi(Offset::ZERO, want.0));
}

/// **A table entry is 12 bytes for the clip's tag and 6 for none**, so
/// 2,048 keys hold 24 KiB and a relay's 128 hold under 1 KiB.
///
/// Mutation (run): a `u64` in `Entry` → 16 bytes, fails.
#[test]
fn the_table_costs_a_few_bytes_a_key() {
    assert_eq!(size_of::<Entry<std::num::NonZeroU32>>(), 12);
    assert_eq!(size_of::<Entry<()>>(), 6);
    assert_eq!(NoteLedger::<()>::new(128).heap_bytes(), 768);
}
