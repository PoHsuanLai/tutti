use super::*;
use tutti_ump::{MidiChannel, NoteNumber};

/// A note number that is in range.
fn note_number(n: u8) -> NoteNumber {
    NoteNumber::new(n).expect("a 7-bit note")
}

#[test]
fn test_basic_allocation() {
    let config = VoiceAllocatorConfig {
        max_voices: 4,
        ..Default::default()
    };
    let mut alloc = VoiceAllocator::new(config);

    // Allocate first note
    let result = alloc.allocate(
        NoteId::from_channel_note(MidiChannel::of(0), 60),
        note_number(60),
        MidiChannel::of(0),
        0.8,
    );
    assert!(matches!(
        result,
        AllocationResult::Allocated { slot_index: 0 }
    ));
    assert_eq!(alloc.active_count(), 1);

    // Allocate second note
    let result = alloc.allocate(
        NoteId::from_channel_note(MidiChannel::of(0), 64),
        note_number(64),
        MidiChannel::of(0),
        0.7,
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
        NoteId::from_channel_note(MidiChannel::of(0), 60),
        note_number(60),
        MidiChannel::of(0),
        0.8,
    );
    alloc.advance_time(100);
    alloc.allocate(
        NoteId::from_channel_note(MidiChannel::of(0), 64),
        note_number(64),
        MidiChannel::of(0),
        0.7,
    );
    alloc.advance_time(100);

    // Third note should steal the oldest (note 60)
    let result = alloc.allocate(
        NoteId::from_channel_note(MidiChannel::of(0), 67),
        note_number(67),
        MidiChannel::of(0),
        0.9,
    );
    assert!(matches!(result, AllocationResult::Stolen { slot_index: 0 }));
}

#[test]
fn test_release() {
    let config = VoiceAllocatorConfig::default();
    let mut alloc = VoiceAllocator::new(config);

    alloc.allocate(
        NoteId::from_channel_note(MidiChannel::of(0), 60),
        note_number(60),
        MidiChannel::of(0),
        0.8,
    );
    assert_eq!(alloc.active_count(), 1);

    alloc.release(
        NoteId::from_channel_note(MidiChannel::of(0), 60),
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
        NoteId::from_channel_note(MidiChannel::of(0), 60),
        note_number(60),
        MidiChannel::of(0),
        0.8,
    );
    alloc.sustain_pedal(MidiChannel::of(0), true);
    alloc.release(
        NoteId::from_channel_note(MidiChannel::of(0), 60),
        MidiChannel::of(0),
    );

    // Should be sustained, not releasing
    assert!(alloc.slots()[0].is_sustained());
    assert!(alloc.slots()[0].is_active());

    // Pedal off should release
    alloc.sustain_pedal(MidiChannel::of(0), false);
    assert!(!alloc.slots()[0].is_sustained());
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
        NoteId::from_channel_note(MidiChannel::of(0), 60),
        note_number(60),
        MidiChannel::of(0),
        0.8,
    );
    assert!(matches!(
        result1,
        AllocationResult::Allocated { slot_index: 0 }
    ));
    assert_eq!(alloc.active_count(), 1);
    assert_eq!(alloc.slots()[0].note(), note_number(60));

    // Second note should retrigger in slot 0 (mono = always slot 0)
    let result2 = alloc.allocate(
        NoteId::from_channel_note(MidiChannel::of(0), 64),
        note_number(64),
        MidiChannel::of(0),
        0.7,
    );
    assert!(matches!(
        result2,
        AllocationResult::Allocated { slot_index: 0 }
    ));
    assert_eq!(alloc.active_count(), 1);
    assert_eq!(alloc.slots()[0].note(), note_number(64));

    // Note mapping should be updated
    assert!(
        alloc
            .id_to_slot
            .get(NoteId::from_channel_note(MidiChannel::of(0), 60))
            .is_none()
    );
    assert_eq!(
        alloc
            .id_to_slot
            .get(NoteId::from_channel_note(MidiChannel::of(0), 64))
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
        NoteId::from_channel_note(MidiChannel::of(0), 60),
        note_number(60),
        MidiChannel::of(0),
        0.8,
    );
    assert!(matches!(result1, AllocationResult::Allocated { .. }));

    // Second note should be legato (no retrigger)
    let result2 = alloc.allocate(
        NoteId::from_channel_note(MidiChannel::of(0), 64),
        note_number(64),
        MidiChannel::of(0),
        0.7,
    );
    assert!(matches!(result2, AllocationResult::LegatoRetrigger { .. }));

    // Should still have only 1 active voice
    assert_eq!(alloc.active_count(), 1);
    assert_eq!(alloc.slots()[0].note(), note_number(64));
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
        NoteId::from_channel_note(MidiChannel::of(0), 60),
        note_number(60),
        MidiChannel::of(0),
        0.8,
    );
    alloc.allocate(
        NoteId::from_channel_note(MidiChannel::of(0), 64),
        note_number(64),
        MidiChannel::of(0),
        0.7,
    );

    // Third note should fail
    let result = alloc.allocate(
        NoteId::from_channel_note(MidiChannel::of(0), 67),
        note_number(67),
        MidiChannel::of(0),
        0.9,
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
        NoteId::from_channel_note(MidiChannel::of(0), 60),
        note_number(60),
        MidiChannel::of(0),
        0.8,
    );
    let slot_1 = match result1 {
        AllocationResult::Allocated { slot_index } => slot_index,
        _ => panic!("Expected allocation"),
    };

    let result2 = alloc.allocate(
        NoteId::from_channel_note(MidiChannel::of(0), 64),
        note_number(64),
        MidiChannel::of(0),
        0.7,
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
        NoteId::from_channel_note(MidiChannel::of(0), 67),
        note_number(67),
        MidiChannel::of(0),
        0.9,
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
        NoteId::from_channel_note(MidiChannel::of(0), 60),
        note_number(60),
        MidiChannel::of(0),
        0.8,
    ); // slot 0
    alloc.allocate(
        NoteId::from_channel_note(MidiChannel::of(0), 67),
        note_number(67),
        MidiChannel::of(0),
        0.8,
    ); // slot 1 - higher note

    // Third note should steal the highest (G4 = 67)
    let result = alloc.allocate(
        NoteId::from_channel_note(MidiChannel::of(0), 72),
        note_number(72),
        MidiChannel::of(0),
        0.9,
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
        NoteId::from_channel_note(MidiChannel::of(0), 60),
        note_number(60),
        MidiChannel::of(0),
        0.8,
    ); // slot 0 - lower note
    alloc.allocate(
        NoteId::from_channel_note(MidiChannel::of(0), 67),
        note_number(67),
        MidiChannel::of(0),
        0.8,
    ); // slot 1

    // Third note should steal the lowest (C4 = 60)
    let result = alloc.allocate(
        NoteId::from_channel_note(MidiChannel::of(0), 72),
        note_number(72),
        MidiChannel::of(0),
        0.9,
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
        NoteId::from_channel_note(MidiChannel::of(0), 60),
        note_number(60),
        MidiChannel::of(0),
        0.8,
    ); // slot 0 - older
    alloc.advance_time(100);
    alloc.allocate(
        NoteId::from_channel_note(MidiChannel::of(0), 64),
        note_number(64),
        MidiChannel::of(0),
        0.7,
    ); // slot 1 - newer
    alloc.advance_time(100);

    // Third note should steal the newest (note 64 in slot 1)
    let result = alloc.allocate(
        NoteId::from_channel_note(MidiChannel::of(0), 67),
        note_number(67),
        MidiChannel::of(0),
        0.9,
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
        NoteId::from_channel_note(MidiChannel::of(0), 60),
        note_number(60),
        MidiChannel::of(0),
        0.8,
    );
    alloc.sostenuto_pedal(MidiChannel::of(0), true);

    // Release key - should be held by sostenuto
    alloc.release(
        NoteId::from_channel_note(MidiChannel::of(0), 60),
        MidiChannel::of(0),
    );
    assert!(alloc.slots()[0].is_active());
    assert!(alloc.slots()[0].is_sostenuto_held());

    // New note played AFTER sostenuto down should NOT be held
    alloc.allocate(
        NoteId::from_channel_note(MidiChannel::of(0), 64),
        note_number(64),
        MidiChannel::of(0),
        0.7,
    );
    assert!(
        !alloc.slots()[1].is_sostenuto_held(),
        "New notes should not be sostenuto-held"
    );
    alloc.release(
        NoteId::from_channel_note(MidiChannel::of(0), 64),
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
    assert!(!alloc.slots()[0].is_sostenuto_held());
    assert!(alloc.slots()[0].is_releasing());
}

#[test]
fn test_sostenuto_key_still_held() {
    let config = VoiceAllocatorConfig::default();
    let mut alloc = VoiceAllocator::new(config);

    // Play note, then press sostenuto
    alloc.allocate(
        NoteId::from_channel_note(MidiChannel::of(0), 60),
        note_number(60),
        MidiChannel::of(0),
        0.8,
    );
    alloc.sostenuto_pedal(MidiChannel::of(0), true);

    // DON'T release key — sostenuto off should NOT release
    alloc.sostenuto_pedal(MidiChannel::of(0), false);
    assert!(alloc.slots()[0].is_active());
    assert!(!alloc.slots()[0].is_sostenuto_held());
    assert!(alloc.slots()[0].is_key_held());
}

#[test]
fn test_all_notes_off() {
    let config = VoiceAllocatorConfig::default();
    let mut alloc = VoiceAllocator::new(config);

    // Play notes on channel 0 and 1
    alloc.allocate(
        NoteId::from_channel_note(MidiChannel::of(0), 60),
        note_number(60),
        MidiChannel::of(0),
        0.8,
    );
    alloc.allocate(
        NoteId::from_channel_note(MidiChannel::of(0), 64),
        note_number(64),
        MidiChannel::of(0),
        0.7,
    );
    alloc.allocate(
        NoteId::from_channel_note(MidiChannel::of(1), 67),
        note_number(67),
        MidiChannel::of(1),
        0.9,
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
        NoteId::from_channel_note(MidiChannel::of(0), 60),
        note_number(60),
        MidiChannel::of(0),
        0.8,
    );
    alloc.allocate(
        NoteId::from_channel_note(MidiChannel::of(0), 64),
        note_number(64),
        MidiChannel::of(0),
        0.7,
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
        NoteId::from_channel_note(MidiChannel::of(0), 60),
        note_number(60),
        MidiChannel::of(0),
        0.8,
    );
    alloc.allocate(
        NoteId::from_channel_note(MidiChannel::of(0), 64),
        note_number(64),
        MidiChannel::of(0),
        0.7,
    );
    alloc.sustain_pedal(MidiChannel::of(0), true);
    alloc.sostenuto_pedal(MidiChannel::of(0), true);

    assert_eq!(alloc.active_count(), 2);

    // Reset everything
    alloc.reset();

    assert_eq!(alloc.active_count(), 0);
    // Pedals should be cleared
    alloc.allocate(
        NoteId::from_channel_note(MidiChannel::of(0), 60),
        note_number(60),
        MidiChannel::of(0),
        0.8,
    );
    alloc.release(
        NoteId::from_channel_note(MidiChannel::of(0), 60),
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
        NoteId::from_channel_note(MidiChannel::of(0), 60),
        note_number(60),
        MidiChannel::of(0),
        0.8,
    );
    assert!(alloc.slots()[0].is_active());

    // Same note again should release old and allocate new
    let result = alloc.allocate(
        NoteId::from_channel_note(MidiChannel::of(0), 60),
        note_number(60),
        MidiChannel::of(0),
        0.9,
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
        NoteId::from_channel_note(MidiChannel::of(0), 60),
        note_number(60),
        MidiChannel::of(0),
        0.8,
    );
    alloc.advance_time(100);
    alloc.allocate(
        NoteId::from_channel_note(MidiChannel::of(0), 64),
        note_number(64),
        MidiChannel::of(0),
        0.7,
    );
    alloc.advance_time(100);
    alloc.release(
        NoteId::from_channel_note(MidiChannel::of(0), 60),
        MidiChannel::of(0),
    ); // Now slot 0 is releasing

    // New note should prefer the releasing voice over active
    let result = alloc.allocate(
        NoteId::from_channel_note(MidiChannel::of(0), 67),
        note_number(67),
        MidiChannel::of(0),
        0.9,
    );
    match result {
        AllocationResult::Stolen { slot_index } => {
            assert_eq!(slot_index, 0, "Should prefer stealing releasing voice");
        }
        _ => panic!("Expected stealing"),
    }
}
