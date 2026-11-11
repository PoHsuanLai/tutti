use super::*;
use tutti_midi_protocol::{MpeZoneConfig, MpeZones};
use tutti_ump::{MidiChannel, NoteId, NoteNumber};

#[test]
fn test_basic_allocation() {
    let config = VoiceAllocatorConfig {
        max_voices: 4,
        ..Default::default()
    };
    let mut alloc = VoiceAllocator::new(config);

    // Allocate first note
    let result = alloc.allocate(
        NoteId::from_channel_note(MidiChannel::of(0), NoteNumber::of(60)),
        NoteNumber::of(60),
        MidiChannel::of(0),
        0.8,
        tutti_ump::Attribute::None,
    );
    assert!(matches!(
        result,
        AllocationResult::Allocated { slot_index: 0 }
    ));
    assert_eq!(alloc.active_count(), 1);

    // Allocate second note
    let result = alloc.allocate(
        NoteId::from_channel_note(MidiChannel::of(0), NoteNumber::of(64)),
        NoteNumber::of(64),
        MidiChannel::of(0),
        0.7,
        tutti_ump::Attribute::None,
    );
    assert!(matches!(
        result,
        AllocationResult::Allocated { slot_index: 1 }
    ));
    assert_eq!(alloc.active_count(), 2);
}

#[test]
fn test_voice_stealing_oldest() {
    let config = VoiceAllocatorConfig {
        max_voices: 2,
        strategy: AllocationStrategy::Oldest,
        ..Default::default()
    };
    let mut alloc = VoiceAllocator::new(config);

    // Fill all voices
    alloc.allocate(
        NoteId::from_channel_note(MidiChannel::of(0), NoteNumber::of(60)),
        NoteNumber::of(60),
        MidiChannel::of(0),
        0.8,
        tutti_ump::Attribute::None,
    );
    alloc.allocate(
        NoteId::from_channel_note(MidiChannel::of(0), NoteNumber::of(64)),
        NoteNumber::of(64),
        MidiChannel::of(0),
        0.7,
        tutti_ump::Attribute::None,
    );

    // Third note should steal the oldest (note 60)
    let result = alloc.allocate(
        NoteId::from_channel_note(MidiChannel::of(0), NoteNumber::of(67)),
        NoteNumber::of(67),
        MidiChannel::of(0),
        0.9,
        tutti_ump::Attribute::None,
    );
    assert!(matches!(result, AllocationResult::Stolen { slot_index: 0 }));
}

#[test]
fn test_release() {
    let config = VoiceAllocatorConfig::default();
    let mut alloc = VoiceAllocator::new(config);

    alloc.allocate(
        NoteId::from_channel_note(MidiChannel::of(0), NoteNumber::of(60)),
        NoteNumber::of(60),
        MidiChannel::of(0),
        0.8,
        tutti_ump::Attribute::None,
    );
    assert_eq!(alloc.active_count(), 1);

    alloc.release(
        NoteId::from_channel_note(MidiChannel::of(0), NoteNumber::of(60)),
        MidiChannel::of(0),
    );
    // Voice should be releasing, not idle
    assert!(alloc.slots()[0].is_releasing());

    // After voice_finished, should be idle
    let voice_id = alloc.slots()[0].voice_id();
    alloc.voice_finished(voice_id);
    assert!(alloc.slots()[0].is_idle());
}

#[test]
fn test_sustain_pedal() {
    let config = VoiceAllocatorConfig::default();
    let mut alloc = VoiceAllocator::new(config);

    alloc.allocate(
        NoteId::from_channel_note(MidiChannel::of(0), NoteNumber::of(60)),
        NoteNumber::of(60),
        MidiChannel::of(0),
        0.8,
        tutti_ump::Attribute::None,
    );
    alloc.sustain_pedal(MidiChannel::of(0), true);
    alloc.release(
        NoteId::from_channel_note(MidiChannel::of(0), NoteNumber::of(60)),
        MidiChannel::of(0),
    );

    // Should be sustained, not releasing
    assert!(alloc.holds_of(0).is_some_and(|h| h.sustained()));
    assert!(alloc.slots()[0].is_active());

    // Pedal off should release
    alloc.sustain_pedal(MidiChannel::of(0), false);
    assert!(!alloc.holds_of(0).is_some_and(|h| h.sustained()));
    assert!(alloc.slots()[0].is_releasing());
}

#[test]
fn test_mono_mode() {
    let config = VoiceAllocatorConfig {
        max_voices: 4,
        mode: VoiceMode::Mono,
        ..Default::default()
    };
    let mut alloc = VoiceAllocator::new(config);

    let result1 = alloc.allocate(
        NoteId::from_channel_note(MidiChannel::of(0), NoteNumber::of(60)),
        NoteNumber::of(60),
        MidiChannel::of(0),
        0.8,
        tutti_ump::Attribute::None,
    );
    assert!(matches!(
        result1,
        AllocationResult::Allocated { slot_index: 0 }
    ));
    assert_eq!(alloc.active_count(), 1);
    assert_eq!(alloc.slots()[0].note(), NoteNumber::of(60));

    // Second note should retrigger in slot 0 (mono = always slot 0)
    let result2 = alloc.allocate(
        NoteId::from_channel_note(MidiChannel::of(0), NoteNumber::of(64)),
        NoteNumber::of(64),
        MidiChannel::of(0),
        0.7,
        tutti_ump::Attribute::None,
    );
    assert!(matches!(
        result2,
        AllocationResult::Allocated { slot_index: 0 }
    ));
    assert_eq!(alloc.active_count(), 1);
    assert_eq!(alloc.slots()[0].note(), NoteNumber::of(64));

    // Note mapping should be updated
    assert!(
        alloc
            .id_to_slot
            .get(NoteId::from_channel_note(
                MidiChannel::of(0),
                NoteNumber::of(60)
            ))
            .is_none()
    );
    assert_eq!(
        alloc
            .id_to_slot
            .get(NoteId::from_channel_note(
                MidiChannel::of(0),
                NoteNumber::of(64)
            ))
            .copied(),
        Some(0)
    );
}

#[test]
fn test_legato_mode() {
    let config = VoiceAllocatorConfig {
        max_voices: 4,
        mode: VoiceMode::Legato,
        ..Default::default()
    };
    let mut alloc = VoiceAllocator::new(config);

    // First note triggers normally
    let result1 = alloc.allocate(
        NoteId::from_channel_note(MidiChannel::of(0), NoteNumber::of(60)),
        NoteNumber::of(60),
        MidiChannel::of(0),
        0.8,
        tutti_ump::Attribute::None,
    );
    assert!(matches!(result1, AllocationResult::Allocated { .. }));

    // Second note should be legato (no retrigger)
    let result2 = alloc.allocate(
        NoteId::from_channel_note(MidiChannel::of(0), NoteNumber::of(64)),
        NoteNumber::of(64),
        MidiChannel::of(0),
        0.7,
        tutti_ump::Attribute::None,
    );
    assert!(matches!(result2, AllocationResult::LegatoRetrigger { .. }));

    // Should still have only 1 active voice
    assert_eq!(alloc.active_count(), 1);
    assert_eq!(alloc.slots()[0].note(), NoteNumber::of(64));
}

#[test]
fn test_no_steal() {
    let config = VoiceAllocatorConfig {
        max_voices: 2,
        strategy: AllocationStrategy::NoSteal,
        ..Default::default()
    };
    let mut alloc = VoiceAllocator::new(config);

    alloc.allocate(
        NoteId::from_channel_note(MidiChannel::of(0), NoteNumber::of(60)),
        NoteNumber::of(60),
        MidiChannel::of(0),
        0.8,
        tutti_ump::Attribute::None,
    );
    alloc.allocate(
        NoteId::from_channel_note(MidiChannel::of(0), NoteNumber::of(64)),
        NoteNumber::of(64),
        MidiChannel::of(0),
        0.7,
        tutti_ump::Attribute::None,
    );

    // Third note should fail
    let result = alloc.allocate(
        NoteId::from_channel_note(MidiChannel::of(0), NoteNumber::of(67)),
        NoteNumber::of(67),
        MidiChannel::of(0),
        0.9,
        tutti_ump::Attribute::None,
    );
    assert!(matches!(result, AllocationResult::Unavailable));
}

#[test]
fn test_voice_stealing_quietest() {
    let config = VoiceAllocatorConfig {
        max_voices: 2,
        strategy: AllocationStrategy::Quietest,
        ..Default::default()
    };
    let mut alloc = VoiceAllocator::new(config);

    // Allocate two voices
    let result1 = alloc.allocate(
        NoteId::from_channel_note(MidiChannel::of(0), NoteNumber::of(60)),
        NoteNumber::of(60),
        MidiChannel::of(0),
        0.8,
        tutti_ump::Attribute::None,
    );
    let slot_1 = match result1 {
        AllocationResult::Allocated { slot_index } => slot_index,
        _ => panic!("Expected allocation"),
    };

    let result2 = alloc.allocate(
        NoteId::from_channel_note(MidiChannel::of(0), NoteNumber::of(64)),
        NoteNumber::of(64),
        MidiChannel::of(0),
        0.7,
        tutti_ump::Attribute::None,
    );
    let slot_2 = match result2 {
        AllocationResult::Allocated { slot_index } => slot_index,
        _ => panic!("Expected allocation"),
    };

    // Set envelope levels - slot 0 is quieter
    alloc.update_envelope_level(slot_1, 0.2);
    alloc.update_envelope_level(slot_2, 0.8);

    // Third note should steal the quietest (slot 0, note 60)
    let result = alloc.allocate(
        NoteId::from_channel_note(MidiChannel::of(0), NoteNumber::of(67)),
        NoteNumber::of(67),
        MidiChannel::of(0),
        0.9,
        tutti_ump::Attribute::None,
    );
    match result {
        AllocationResult::Stolen { slot_index } => {
            assert_eq!(slot_index, 0);
        }
        _ => panic!("Expected stealing, got {:?}", result),
    }
}

#[test]
fn test_voice_stealing_highest_note() {
    let config = VoiceAllocatorConfig {
        max_voices: 2,
        strategy: AllocationStrategy::HighestNote,
        ..Default::default()
    };
    let mut alloc = VoiceAllocator::new(config);

    // Allocate C4 (60) and G4 (67)
    alloc.allocate(
        NoteId::from_channel_note(MidiChannel::of(0), NoteNumber::of(60)),
        NoteNumber::of(60),
        MidiChannel::of(0),
        0.8,
        tutti_ump::Attribute::None,
    ); // slot 0
    alloc.allocate(
        NoteId::from_channel_note(MidiChannel::of(0), NoteNumber::of(67)),
        NoteNumber::of(67),
        MidiChannel::of(0),
        0.8,
        tutti_ump::Attribute::None,
    ); // slot 1 - higher note

    // Third note should steal the highest (G4 = 67)
    let result = alloc.allocate(
        NoteId::from_channel_note(MidiChannel::of(0), NoteNumber::of(72)),
        NoteNumber::of(72),
        MidiChannel::of(0),
        0.9,
        tutti_ump::Attribute::None,
    );
    match result {
        AllocationResult::Stolen { slot_index } => {
            assert_eq!(slot_index, 1, "Should steal slot with highest note (67)");
        }
        _ => panic!("Expected stealing"),
    }
}

#[test]
fn test_voice_stealing_lowest_note() {
    let config = VoiceAllocatorConfig {
        max_voices: 2,
        strategy: AllocationStrategy::LowestNote,
        ..Default::default()
    };
    let mut alloc = VoiceAllocator::new(config);

    // Allocate C4 (60) and G4 (67)
    alloc.allocate(
        NoteId::from_channel_note(MidiChannel::of(0), NoteNumber::of(60)),
        NoteNumber::of(60),
        MidiChannel::of(0),
        0.8,
        tutti_ump::Attribute::None,
    ); // slot 0 - lower note
    alloc.allocate(
        NoteId::from_channel_note(MidiChannel::of(0), NoteNumber::of(67)),
        NoteNumber::of(67),
        MidiChannel::of(0),
        0.8,
        tutti_ump::Attribute::None,
    ); // slot 1

    // Third note should steal the lowest (C4 = 60)
    let result = alloc.allocate(
        NoteId::from_channel_note(MidiChannel::of(0), NoteNumber::of(72)),
        NoteNumber::of(72),
        MidiChannel::of(0),
        0.9,
        tutti_ump::Attribute::None,
    );
    match result {
        AllocationResult::Stolen { slot_index } => {
            assert_eq!(slot_index, 0, "Should steal slot with lowest note (60)");
        }
        _ => panic!("Expected stealing"),
    }
}

#[test]
fn test_voice_stealing_newest() {
    let config = VoiceAllocatorConfig {
        max_voices: 2,
        strategy: AllocationStrategy::Newest,
        ..Default::default()
    };
    let mut alloc = VoiceAllocator::new(config);

    alloc.allocate(
        NoteId::from_channel_note(MidiChannel::of(0), NoteNumber::of(60)),
        NoteNumber::of(60),
        MidiChannel::of(0),
        0.8,
        tutti_ump::Attribute::None,
    ); // slot 0 - older
    alloc.allocate(
        NoteId::from_channel_note(MidiChannel::of(0), NoteNumber::of(64)),
        NoteNumber::of(64),
        MidiChannel::of(0),
        0.7,
        tutti_ump::Attribute::None,
    ); // slot 1 - newer

    // Third note should steal the newest (note 64 in slot 1)
    let result = alloc.allocate(
        NoteId::from_channel_note(MidiChannel::of(0), NoteNumber::of(67)),
        NoteNumber::of(67),
        MidiChannel::of(0),
        0.9,
        tutti_ump::Attribute::None,
    );
    match result {
        AllocationResult::Stolen { slot_index } => {
            assert_eq!(slot_index, 1, "Should steal newest voice");
        }
        _ => panic!("Expected stealing"),
    }
}

#[test]
fn test_sostenuto_pedal() {
    let config = VoiceAllocatorConfig::default();
    let mut alloc = VoiceAllocator::new(config);

    // Play note, then press sostenuto
    alloc.allocate(
        NoteId::from_channel_note(MidiChannel::of(0), NoteNumber::of(60)),
        NoteNumber::of(60),
        MidiChannel::of(0),
        0.8,
        tutti_ump::Attribute::None,
    );
    alloc.sostenuto_pedal(MidiChannel::of(0), true);

    // Release key - should be held by sostenuto
    alloc.release(
        NoteId::from_channel_note(MidiChannel::of(0), NoteNumber::of(60)),
        MidiChannel::of(0),
    );
    assert!(alloc.slots()[0].is_active());
    assert!(alloc.holds_of(0).is_some_and(|h| h.sostenuto()));

    // New note played AFTER sostenuto down should NOT be held
    alloc.allocate(
        NoteId::from_channel_note(MidiChannel::of(0), NoteNumber::of(64)),
        NoteNumber::of(64),
        MidiChannel::of(0),
        0.7,
        tutti_ump::Attribute::None,
    );
    assert!(
        !alloc.holds_of(1).is_some_and(|h| h.sostenuto()),
        "New notes should not be sostenuto-held"
    );
    alloc.release(
        NoteId::from_channel_note(MidiChannel::of(0), NoteNumber::of(64)),
        MidiChannel::of(0),
    );
    assert!(
        alloc.slots()[1].is_releasing(),
        "New note should release normally"
    );

    // Original note still held
    assert!(alloc.slots()[0].is_active());

    // Sostenuto off should release the held note (key was released)
    alloc.sostenuto_pedal(MidiChannel::of(0), false);
    assert!(!alloc.holds_of(0).is_some_and(|h| h.sostenuto()));
    assert!(alloc.slots()[0].is_releasing());
}

#[test]
fn test_sostenuto_key_still_held() {
    let config = VoiceAllocatorConfig::default();
    let mut alloc = VoiceAllocator::new(config);

    // Play note, then press sostenuto
    alloc.allocate(
        NoteId::from_channel_note(MidiChannel::of(0), NoteNumber::of(60)),
        NoteNumber::of(60),
        MidiChannel::of(0),
        0.8,
        tutti_ump::Attribute::None,
    );
    alloc.sostenuto_pedal(MidiChannel::of(0), true);

    // DON'T release key — sostenuto off should NOT release
    alloc.sostenuto_pedal(MidiChannel::of(0), false);
    assert!(alloc.slots()[0].is_active());
    assert!(!alloc.holds_of(0).is_some_and(|h| h.sostenuto()));
    assert!(alloc.holds_of(0).is_some_and(|h| h.key()));
}

#[test]
fn test_all_notes_off() {
    let config = VoiceAllocatorConfig::default();
    let mut alloc = VoiceAllocator::new(config);

    // Play notes on channel 0 and 1
    alloc.allocate(
        NoteId::from_channel_note(MidiChannel::of(0), NoteNumber::of(60)),
        NoteNumber::of(60),
        MidiChannel::of(0),
        0.8,
        tutti_ump::Attribute::None,
    );
    alloc.allocate(
        NoteId::from_channel_note(MidiChannel::of(0), NoteNumber::of(64)),
        NoteNumber::of(64),
        MidiChannel::of(0),
        0.7,
        tutti_ump::Attribute::None,
    );
    alloc.allocate(
        NoteId::from_channel_note(MidiChannel::of(1), NoteNumber::of(67)),
        NoteNumber::of(67),
        MidiChannel::of(1),
        0.9,
        tutti_ump::Attribute::None,
    );

    assert_eq!(alloc.active_count(), 3);

    // All notes off on channel 0 only
    alloc.all_notes_off(MidiChannel::of(0));

    // Channel 0 notes should be releasing
    assert!(alloc.slots()[0].is_releasing());
    assert!(alloc.slots()[1].is_releasing());
    // Channel 1 note still active
    assert!(alloc.slots()[2].is_active());
}

#[test]
fn test_all_sound_off() {
    let config = VoiceAllocatorConfig::default();
    let mut alloc = VoiceAllocator::new(config);

    // Play notes
    alloc.allocate(
        NoteId::from_channel_note(MidiChannel::of(0), NoteNumber::of(60)),
        NoteNumber::of(60),
        MidiChannel::of(0),
        0.8,
        tutti_ump::Attribute::None,
    );
    alloc.allocate(
        NoteId::from_channel_note(MidiChannel::of(0), NoteNumber::of(64)),
        NoteNumber::of(64),
        MidiChannel::of(0),
        0.7,
        tutti_ump::Attribute::None,
    );

    // All sound off - immediate silence
    alloc.all_sound_off(MidiChannel::of(0));

    // Should be immediately idle (no release phase)
    assert!(alloc.slots()[0].is_idle());
    assert!(alloc.slots()[1].is_idle());
    assert_eq!(alloc.active_count(), 0);
}

#[test]
fn test_reset() {
    let config = VoiceAllocatorConfig::default();
    let mut alloc = VoiceAllocator::new(config);

    // Play notes with pedals
    alloc.allocate(
        NoteId::from_channel_note(MidiChannel::of(0), NoteNumber::of(60)),
        NoteNumber::of(60),
        MidiChannel::of(0),
        0.8,
        tutti_ump::Attribute::None,
    );
    alloc.allocate(
        NoteId::from_channel_note(MidiChannel::of(0), NoteNumber::of(64)),
        NoteNumber::of(64),
        MidiChannel::of(0),
        0.7,
        tutti_ump::Attribute::None,
    );
    alloc.sustain_pedal(MidiChannel::of(0), true);
    alloc.sostenuto_pedal(MidiChannel::of(0), true);

    assert_eq!(alloc.active_count(), 2);

    // Reset everything
    alloc.reset();

    assert_eq!(alloc.active_count(), 0);
    // Pedals should be cleared
    alloc.allocate(
        NoteId::from_channel_note(MidiChannel::of(0), NoteNumber::of(60)),
        NoteNumber::of(60),
        MidiChannel::of(0),
        0.8,
        tutti_ump::Attribute::None,
    );
    alloc.release(
        NoteId::from_channel_note(MidiChannel::of(0), NoteNumber::of(60)),
        MidiChannel::of(0),
    );
    // Without sustain, should go to releasing
    assert!(alloc.slots()[0].is_releasing());
}

#[test]
fn test_retrigger_same_note() {
    let config = VoiceAllocatorConfig::default();
    let mut alloc = VoiceAllocator::new(config);

    // Play same note twice
    alloc.allocate(
        NoteId::from_channel_note(MidiChannel::of(0), NoteNumber::of(60)),
        NoteNumber::of(60),
        MidiChannel::of(0),
        0.8,
        tutti_ump::Attribute::None,
    );
    assert!(alloc.slots()[0].is_active());

    // Same note again should release old and allocate new
    let result = alloc.allocate(
        NoteId::from_channel_note(MidiChannel::of(0), NoteNumber::of(60)),
        NoteNumber::of(60),
        MidiChannel::of(0),
        0.9,
        tutti_ump::Attribute::None,
    );
    match result {
        AllocationResult::Allocated { slot_index } => {
            // Old voice should be releasing, new one allocated
            assert!(slot_index == 0 || slot_index == 1);
        }
        _ => panic!("Expected new allocation"),
    }
}

#[test]
fn test_prefer_stealing_releasing_voices() {
    let config = VoiceAllocatorConfig {
        max_voices: 2,
        strategy: AllocationStrategy::Oldest,
        ..Default::default()
    };
    let mut alloc = VoiceAllocator::new(config);

    // Allocate and release one voice
    alloc.allocate(
        NoteId::from_channel_note(MidiChannel::of(0), NoteNumber::of(60)),
        NoteNumber::of(60),
        MidiChannel::of(0),
        0.8,
        tutti_ump::Attribute::None,
    );
    alloc.allocate(
        NoteId::from_channel_note(MidiChannel::of(0), NoteNumber::of(64)),
        NoteNumber::of(64),
        MidiChannel::of(0),
        0.7,
        tutti_ump::Attribute::None,
    );
    alloc.release(
        NoteId::from_channel_note(MidiChannel::of(0), NoteNumber::of(60)),
        MidiChannel::of(0),
    ); // Now slot 0 is releasing

    // New note should prefer the releasing voice over active
    let result = alloc.allocate(
        NoteId::from_channel_note(MidiChannel::of(0), NoteNumber::of(67)),
        NoteNumber::of(67),
        MidiChannel::of(0),
        0.9,
        tutti_ump::Attribute::None,
    );
    match result {
        AllocationResult::Stolen { slot_index } => {
            assert_eq!(slot_index, 0, "Should prefer stealing releasing voice");
        }
        _ => panic!("Expected stealing"),
    }
}

// ---- random streams -------------------------------------------------------

struct Rng(u64);
impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % n
    }
}

/// What the allocator holds for each live slot: `(voice, key, gated)`.
fn table(alloc: &VoiceAllocator) -> Vec<(VoiceId, u8, bool)> {
    (0..alloc.slots().len())
        .filter(|&i| !alloc.slots()[i].is_idle())
        .map(|i| {
            let s = &alloc.slots()[i];
            let gated = alloc
                .core_phase(i)
                .is_some_and(|p| p != tutti_voicing::Phase::Releasing);
            (s.voice_id(), s.note().get(), gated)
        })
        .collect()
}

/// Random streams of notes, pedals, panics, resets, bends and envelope levels,
/// in every mode and strategy, checking after every message that:
/// - (a) no more voices sound than the polyphony;
/// - (c) every live slot is a voice the core knows, and the reverse: a slot
///   the core released is not gated;
/// - (d) the id map holds no stale entry: every mapped id names a live slot of
///   that id, and every unreleased live slot is mapped;
/// - (e) on a full poly allocator the voice taken is the one the strategy
///   names under the slots' real envelope levels (a releasing voice first,
///   the quietest of those; otherwise by strategy, the oldest among equals);
/// - (f) in mono and legato at most one voice a channel is gated, and it plays
///   the most recent held key;
/// - (g) `NoSteal` at a full allocator drops the note and changes nothing;
/// - (b) after every key and pedal is let go, nothing is gated.
///
/// Mutations (run), each seen to fail: `<` to `<=` in the core's `start_poly`
/// (a); `Steal::Never` read as `By(Oldest)` (g); `total_cmp(..).is_lt()` to
/// `is_gt()` for the quietest (e); the core's mono return skipped (f); the
/// pedal-up not releasing (b); `unmap` removed from the `Release` arm (d).
#[test]
fn random_streams_keep_their_invariants() {
    use AllocationStrategy as S;
    let strategies = [
        S::Oldest,
        S::Newest,
        S::Quietest,
        S::HighestNote,
        S::LowestNote,
        S::NoSteal,
    ];
    let modes = [
        VoiceMode::Poly,
        VoiceMode::Poly,
        VoiceMode::Mono,
        VoiceMode::Legato,
    ];
    for stream in 0..600u64 {
        let mut rng = Rng(0x2545_F491_4F6C_DD1D ^ (stream + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15));
        let strategy = strategies[(stream % 6) as usize];
        let mode = modes[((stream / 6) % 4) as usize];
        let max = 4;
        let mut alloc = VoiceAllocator::new(VoiceAllocatorConfig {
            max_voices: max,
            strategy,
            mode,
            ..Default::default()
        });
        if stream % 5 == 0 {
            alloc.set_zones(MpeZones::new(
                MpeZoneConfig::lower(3),
                MpeZoneConfig::upper(2),
            ));
        }
        // The test's own model of the held keys of each channel.
        let mut held: [Vec<u8>; 2] = [vec![], vec![]];
        let mut pedal = [false; 2];
        let mut sostenuto = [false; 2];
        for _ in 0..80 {
            let c = rng.below(2) as u8;
            let ch = MidiChannel::of(c);
            let k = 60 + rng.below(8) as u8;
            // Real envelope levels, set before the message as the render would.
            for i in 0..max {
                if !alloc.slots()[i].is_idle() {
                    let level = rng.below(100) as f32 / 100.0;
                    alloc.update_envelope_level(i, level);
                }
            }
            let op = rng.below(14);
            match op {
                0..=4 => {
                    let id = NoteId::from_channel_note(ch, NoteNumber::of(k));
                    let full = alloc.active_count() == max;
                    let before = table(&alloc);
                    let expect = (mode == VoiceMode::Poly && full)
                        .then(|| expected_victim(&alloc, strategy, id, k, c));
                    let r =
                        alloc.allocate(id, NoteNumber::of(k), ch, 0.5, tutti_ump::Attribute::None);
                    held[usize::from(c)].retain(|&x| x != k);
                    if !matches!(r, AllocationResult::Unavailable) || mode != VoiceMode::Poly {
                        held[usize::from(c)].push(k);
                    }
                    // (g) a same-key Replace frees a slot first, so only a note
                    // on another key at a full table must be dropped.
                    if strategy == S::NoSteal
                        && mode == VoiceMode::Poly
                        && full
                        && !before.iter().any(|b| b.1 == k)
                    {
                        assert!(
                            matches!(r, AllocationResult::Unavailable),
                            "stream {stream}: {r:?}"
                        );
                        assert_eq!(
                            table(&alloc),
                            before,
                            "stream {stream}: NoSteal disturbed a voice"
                        );
                    }
                    // (e)
                    if let (Some(want), Some(got), true) = (
                        expect.flatten(),
                        r.slot_index(),
                        matches!(r, AllocationResult::Stolen { .. }),
                    ) {
                        assert_eq!(
                            got, want,
                            "stream {stream}: {strategy:?} took slot {got}, wanted {want}"
                        );
                    }
                }
                5..=8 => {
                    held[usize::from(c)].retain(|&x| x != k);
                    alloc.release(NoteId::from_channel_note(ch, NoteNumber::of(k)), ch);
                }
                9 => {
                    pedal[usize::from(c)] = !pedal[usize::from(c)];
                    alloc.sustain_pedal(ch, pedal[usize::from(c)]);
                }
                10 => {
                    sostenuto[usize::from(c)] = !sostenuto[usize::from(c)];
                    alloc.sostenuto_pedal(ch, sostenuto[usize::from(c)]);
                }
                11 => {
                    held[usize::from(c)].clear();
                    if rng.below(2) == 0 {
                        alloc.all_notes_off(ch);
                    } else {
                        alloc.all_sound_off(ch);
                    }
                }
                12 => {
                    held[usize::from(c)].clear();
                    pedal[usize::from(c)] = false;
                    sostenuto[usize::from(c)] = false;
                    alloc.reset_controllers(ch);
                }
                _ => {
                    // The render finishes a releasing voice.
                    if let Some((v, _, false)) = table(&alloc).into_iter().find(|t| !t.2) {
                        alloc.voice_finished(v);
                    }
                    alloc.message(ch, &Voice2::PitchBend(rng.below(1 << 32) as u32));
                }
            }
            // (a)
            assert!(alloc.active_count() <= max, "stream {stream}");
            // (c) and (d)
            for i in 0..max {
                let s = &alloc.slots()[i];
                if s.is_idle() {
                    assert!(alloc.core_phase(i).is_none());
                    continue;
                }
                assert!(
                    alloc.core_knows(s.voice_id()),
                    "stream {stream}: slot {i} is a voice the core lost"
                );
                let mapped = alloc.id_to_slot.get(s.id()).copied() == Some(i);
                assert_eq!(
                    mapped,
                    alloc.core_phase(i) != Some(tutti_voicing::Phase::Releasing),
                    "stream {stream}: slot {i} id map out of step with the core"
                );
            }
            // (f)
            if mode != VoiceMode::Poly {
                for chan in 0..2u8 {
                    let gated: Vec<_> = (0..max)
                        .filter(|&i| {
                            !alloc.slots()[i].is_idle()
                                && alloc.slots()[i].channel().get() == chan
                                && alloc.core_phase(i) != Some(tutti_voicing::Phase::Releasing)
                        })
                        .collect();
                    assert!(
                        gated.len() <= 1,
                        "stream {stream}: {gated:?} gated on channel {chan}"
                    );
                    if let (Some(&top), Some(&i)) = (held[usize::from(chan)].last(), gated.first())
                    {
                        assert_eq!(
                            alloc.slots()[i].note().get(),
                            top,
                            "stream {stream}: not the latest key"
                        );
                    }
                }
            }
        }
        // (b)
        for c in 0..2u8 {
            let ch = MidiChannel::of(c);
            alloc.sustain_pedal(ch, false);
            alloc.sostenuto_pedal(ch, false);
            for _ in 0..2 {
                for k in 60..68u8 {
                    alloc.release(NoteId::from_channel_note(ch, NoteNumber::of(k)), ch);
                }
            }
        }
        assert!(
            table(&alloc).iter().all(|t| !t.2),
            "stream {stream}: gated after every key and pedal went: {:?}",
            table(&alloc)
        );
    }
}

/// The slot the strategy names on a full poly allocator (the new note's own
/// key replaces a same-key voice first, so the table is read as it stands
/// after that release).
fn expected_victim(
    alloc: &VoiceAllocator,
    strategy: AllocationStrategy,
    _id: NoteId,
    k: u8,
    c: u8,
) -> Option<usize> {
    use AllocationStrategy as S;
    let live: Vec<usize> = (0..alloc.slots().len())
        .filter(|&i| !alloc.slots()[i].is_idle())
        .collect();
    let released: Vec<usize> = live
        .iter()
        .copied()
        .filter(|&i| {
            // The same key on the same channel is released first (Replace).
            alloc.core_phase(i) == Some(tutti_voicing::Phase::Releasing)
                || (alloc.slots()[i].note().get() == k && alloc.slots()[i].channel().get() == c)
        })
        .collect();
    let by = |set: &[usize], f: &dyn Fn(usize) -> f64| -> Option<usize> {
        // The oldest among equals: the lowest voice id first.
        let mut sorted = set.to_vec();
        sorted.sort_by_key(|&i| alloc.slots()[i].voice_id());
        sorted
            .into_iter()
            .fold(None, |best: Option<usize>, i| match best {
                Some(b) if f(i) >= f(b) => Some(b),
                _ => Some(i),
            })
    };
    let level = |i: usize| f64::from(alloc.slots()[i].envelope_level());
    if !released.is_empty() {
        return by(&released, &level);
    }
    let id = |i: usize| alloc.slots()[i].voice_id().0 as f64;
    let key = |i: usize| f64::from(alloc.slots()[i].note().get());
    match strategy {
        S::Oldest => by(&live, &id),
        S::Newest => by(&live, &|i| -id(i)),
        S::Quietest => by(&live, &level),
        S::HighestNote => by(&live, &|i| -key(i)),
        S::LowestNote => by(&live, &key),
        S::NoSteal => None,
    }
}

/// A voice that ended on its own (its envelope or sample ran out) is not
/// returned to: the key left held starts a fresh voice (a `Gate::Start`).
///
/// Mutation (run): the core not reading the voices on a note-off leaves the
/// slot idle, the gate retargeting a voice that is gone.
#[test]
fn a_return_after_the_voice_ended_starts_a_fresh_one() {
    for mode in [VoiceMode::Mono, VoiceMode::Legato] {
        let mut alloc = VoiceAllocator::new(VoiceAllocatorConfig {
            max_voices: 4,
            mode,
            ..Default::default()
        });
        let ch = MidiChannel::of(0);
        for k in [60, 64] {
            alloc.allocate(
                NoteId::from_channel_note(ch, NoteNumber::of(k)),
                NoteNumber::of(k),
                ch,
                0.5,
                tutti_ump::Attribute::None,
            );
        }
        let _ = alloc.take_gates();
        let live = alloc
            .slots()
            .iter()
            .find(|s| !s.is_idle())
            .map(|s| s.voice_id())
            .unwrap();
        alloc.voice_finished(live);
        alloc.release(NoteId::from_channel_note(ch, NoteNumber::of(64)), ch);
        let gates = alloc.take_gates();
        assert!(
            gates.iter().any(
                |g| matches!(g, Gate::Start { key, legato: false, .. } if *key == NoteNumber::of(60))
            ),
            "{mode:?}: {gates:?}"
        );
    }
}

/// A velocity reaches the note logic as the nearest 16-bit word, not the one
/// below: 0.5 is 32767.5 words, which rounds to 32768 (M2-104's centre word).
/// A mono return to a held key plays that word back, so it is observable.
///
/// Mutation (run, seen to fail): `allocate` truncating (`(v * 65535.0) as u16`)
/// gives 32767 and the returned velocity is 32767 / 65535.
#[test]
fn a_velocity_is_quantised_to_the_nearest_word() {
    let mut alloc = VoiceAllocator::new(VoiceAllocatorConfig {
        max_voices: 4,
        mode: VoiceMode::Mono,
        ..Default::default()
    });
    let ch = MidiChannel::of(0);
    let id = |n| NoteId::from_channel_note(ch, NoteNumber::of(n));
    alloc.allocate(
        id(60),
        NoteNumber::of(60),
        ch,
        0.5,
        tutti_ump::Attribute::None,
    );
    alloc.allocate(
        id(64),
        NoteNumber::of(64),
        ch,
        0.9,
        tutti_ump::Attribute::None,
    );
    alloc.take_gates();
    alloc.release(id(64), ch);
    let gates = alloc.take_gates();
    let back = gates.iter().find_map(|g| match g {
        Gate::Start { key, velocity, .. } if *key == NoteNumber::of(60) => Some(*velocity),
        _ => None,
    });
    assert_eq!(back, Some(32768.0 / 65535.0), "{gates:?}");
}
