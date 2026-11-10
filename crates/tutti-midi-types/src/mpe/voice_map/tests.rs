use super::*;

fn note(n: u8) -> NoteNumber {
    NoteNumber::of(n)
}

#[test]
fn rotation_mints_distinct_ids_for_same_pitch() {
    // Two note-ons at the same pitch on one channel get independent ids —
    // the point of Note Number Rotation.
    let mut rot = NoteRotationAllocator::new();
    let a = rot.note_on(note(60));
    let b = rot.note_on(note(60));
    assert_ne!(a, b, "same-pitch notes must mint distinct ids");
    // The most recent is what a by-number per-note message resolves to.
    assert_eq!(rot.resolve(note(60)), Some(b));
}

#[test]
fn rotation_note_off_frees_and_returns_id() {
    let mut rot = NoteRotationAllocator::new();
    let a = rot.note_on(note(64));
    assert_eq!(rot.resolve(note(64)), Some(a));
    assert_eq!(rot.note_off(note(64)), Some(a));
    assert_eq!(rot.resolve(note(64)), None);
    assert_eq!(rot.note_off(note(64)), None); // idempotent
}

/// A minted id never names a `(channel, note)` pair, including across the
/// counter's wrap.
///
/// Mutation: start `next` at 1 (or let the wrap go to 0): `channel_note()` is
/// `Some` and this fails.
#[test]
fn rotation_minted_ids_never_name_a_pair() {
    let mut rot = NoteRotationAllocator::new();
    for _ in 0..300 {
        let id = rot.note_on(note(60));
        assert!(id.raw() > NoteId::MAX_CHANNEL_NOTE);
        assert_eq!(id.channel_note(), None);
    }
    // Force the wrap: the last value, then back above the pair range.
    rot.next = u32::MAX;
    assert_eq!(rot.note_on(note(60)).raw(), u32::MAX);
    let after = rot.note_on(note(60));
    assert_eq!(after.raw(), NoteId::MAX_CHANNEL_NOTE + 1);
    assert_eq!(after.channel_note(), None);
}

#[test]
fn test_voice_stealing_cleans_up_old_mapping() {
    // 2 member channels (1, 2) for lower zone
    let config = MpeZoneConfig::lower(2);
    let mut map = MpeChannelVoiceMap::new(config);

    // Fill all channels
    let ch_a = map.assign_note(note(60)).unwrap();
    let ch_b = map.assign_note(note(64)).unwrap();
    assert_ne!(ch_a, ch_b);

    // All channels occupied — assigning note 67 steals
    let ch_c = map.assign_note(note(67)).unwrap();

    // The stolen note's mapping must be cleaned up
    let stolen_note = if ch_c == ch_a { note(60) } else { note(64) };
    assert!(
        map.channel_for_note(stolen_note).is_none(),
        "Old note mapping must be cleaned up after voice stealing"
    );
    assert_eq!(map.channel_for_note(note(67)), Some(ch_c));

    // Bidirectional consistency: channel → note and note → channel match
    assert_eq!(map.note_for_channel(ch_c), Some(note(67)));
}

#[test]
fn test_voice_stealing_takes_oldest_note() {
    // 3 members (channels 1,2,3). Assign in a known order so "oldest" is
    // unambiguous, then force a steal and confirm the FIRST note goes.
    let config = MpeZoneConfig::lower(3);
    let mut map = MpeChannelVoiceMap::new(config);

    let ch_oldest = map.assign_note(note(60)).unwrap(); // assigned first
    map.assign_note(note(64)).unwrap();
    map.assign_note(note(67)).unwrap();

    // All 3 occupied — assigning a 4th steals the oldest (note 60).
    let ch_new = map.assign_note(note(72)).unwrap();

    assert_eq!(
        ch_new, ch_oldest,
        "steal must reuse the channel of the oldest note"
    );
    assert!(
        map.channel_for_note(note(60)).is_none(),
        "oldest note (60) must be evicted"
    );
    assert!(
        map.channel_for_note(note(64)).is_some(),
        "64 must still sound"
    );
    assert_eq!(map.channel_for_note(note(72)), Some(ch_new));

    // A second steal must take note 64 (now the oldest survivor), not 67.
    map.assign_note(note(76)).unwrap();
    assert!(
        map.channel_for_note(note(64)).is_none(),
        "64 is now oldest, evict it"
    );
    assert!(
        map.channel_for_note(note(67)).is_some(),
        "67 is newer, must survive"
    );
}

#[test]
fn test_note_for_channel() {
    let config = MpeZoneConfig::lower(3);
    let mut map = MpeChannelVoiceMap::new(config);

    // No notes assigned → None for all channels
    assert!(map.note_for_channel(MidiChannel::of(1)).is_none());
    assert!(map.note_for_channel(MidiChannel::of(2)).is_none());

    // Assign note 60
    let ch = map.assign_note(note(60)).unwrap();
    assert_eq!(map.note_for_channel(ch), Some(note(60)));
}

#[test]
fn test_clear_resets_all_mappings() {
    let config = MpeZoneConfig::lower(3);
    let mut map = MpeChannelVoiceMap::new(config);

    map.assign_note(note(60));
    map.assign_note(note(64));
    map.assign_note(note(67));

    map.clear();

    assert!(map.channel_for_note(note(60)).is_none());
    assert!(map.channel_for_note(note(64)).is_none());
    assert!(map.channel_for_note(note(67)).is_none());

    // Should be able to assign fresh after clear
    let ch = map.assign_note(note(72));
    assert!(ch.is_some());
}

// NOTE: `MpeChannelVoiceMap::handles_channel` is a one-line forward to
// `MpeZoneConfig::handles_channel`, whose master/member layout is pinned per
// zone shape by `zone::tests::zone_layout_places_master_and_members_per_rp053`.

#[test]
fn test_channel_reuse_after_release() {
    let config = MpeZoneConfig::lower(2);
    let mut map = MpeChannelVoiceMap::new(config);

    let ch1 = map.assign_note(note(60)).unwrap();
    let _ch2 = map.assign_note(note(64)).unwrap();

    // Release first note
    map.release_note(note(60));

    // New note should reuse the freed channel
    let ch3 = map.assign_note(note(67)).unwrap();
    assert_eq!(ch3, ch1, "Should reuse freed channel");
}
