use super::*;
use tutti_midi_protocol::TuningFrequency;
use tutti_ump::NoteNumber;

#[test]
fn test_equal_temperament() {
    let tuning = Tuning::equal_temperament();

    // A4 should be 440 Hz
    assert!((tuning.note_to_freq(NoteNumber::of(69)).get() - 440.0).abs() < 0.01);

    // A5 should be 880 Hz (octave above)
    assert!((tuning.note_to_freq(NoteNumber::of(81)).get() - 880.0).abs() < 0.01);

    // A3 should be 220 Hz (octave below)
    assert!((tuning.note_to_freq(NoteNumber::of(57)).get() - 220.0).abs() < 0.01);

    // Middle C (C4) should be ~261.6 Hz
    assert!((tuning.note_to_freq(NoteNumber::of(60)).get() - 261.63).abs() < 0.1);
}

#[test]
fn test_custom_reference() {
    let tuning = Tuning::equal_temperament_with_reference(Hz::new(432.0), NoteNumber::of(69));

    // A4 should be 432 Hz
    assert!((tuning.note_to_freq(NoteNumber::of(69)).get() - 432.0).abs() < 0.01);
}

#[test]
fn test_just_intonation() {
    let tuning = Tuning::just_intonation();

    // Reference should still be A4 = 440 Hz
    let a4 = tuning.note_to_freq(NoteNumber::of(69)).get();
    assert!((a4 - 440.0).abs() < 0.01);

    // Perfect fifth above A4 (E5) should be 3/2 ratio
    // In just intonation from C, perfect fifth is 3/2
    // E5 is note 76, A4 is note 69
    // This is 7 semitones, so it's a perfect fifth from A to E
    let e5 = tuning.note_to_freq(NoteNumber::of(76)).get();
    let ratio = e5 / a4;
    // Should be close to 3/2 = 1.5
    assert!((ratio - 1.5).abs() < 0.02);
}

/// The table is frequency data: equal temperament is each key at its own
/// number, A4 (note 69) is `45 00 00`, and just intonation's major third over C
/// (5/4, 386.3 cents) is that many cents above note 64's equal-tempered pitch
/// less 400.
#[test]
fn the_table_is_frequency_data() {
    let equal = Tuning::equal_temperament().table();
    for k in 0..128u8 {
        let key = NoteNumber::of(k);
        assert_eq!(equal.get(key), TuningFrequency::equal(key), "key {k}");
    }
    let just = Tuning::just_intonation().table();
    // Major third over C: 5/4 is 386.31 cents.
    let c = just.get(NoteNumber::of(60)).pitch().unwrap().get();
    let e = just.get(NoteNumber::of(64)).pitch().unwrap().get();
    assert!(
        ((e - c) * 100.0 - 386.3137).abs() < 0.01,
        "{}",
        (e - c) * 100.0
    );
    let a = just.get(NoteNumber::of(69)).pitch().unwrap().get();
    assert!((a - 69.0).abs() < 1e-4, "{a}");
}

#[test]
fn test_from_cents() {
    // Create quarter-tone scale (24 notes per octave)
    let cents: Vec<f32> = (0..24).map(|i| i as f32 * 50.0).collect();
    let tuning = Tuning::from_cents(&cents);

    assert_eq!(tuning.scale_size(), 24);

    // Note 0 and note 24 should be an octave apart
    let freq_0 = tuning.note_to_freq(NoteNumber::of(0)).get();
    let freq_24 = tuning.note_to_freq(NoteNumber::of(24)).get();
    let ratio = freq_24 / freq_0;
    assert!((ratio - 2.0).abs() < 0.01);
}

#[test]
fn test_from_ratios() {
    // Simple Pythagorean pentatonic
    let ratios = [1.0, 9.0 / 8.0, 81.0 / 64.0, 3.0 / 2.0, 27.0 / 16.0];
    let tuning = Tuning::from_ratios(&ratios);

    assert_eq!(tuning.scale_size(), 5);
}

/// The MIDI range's ends.
///
/// The mapping is exponential in the note number, so an off-by-one in the
/// octave arithmetic shows up here first: note 0 → 127 must span exactly
/// 127 semitones.
#[test]
fn test_boundary_notes() {
    let tuning = Tuning::equal_temperament();

    let low = tuning.note_to_freq(NoteNumber::of(0)).get();
    let high = tuning.note_to_freq(NoteNumber::of(127)).get();
    assert!(low.is_finite() && low > 0.0, "note 0 gave {low} Hz");
    assert!(high.is_finite(), "note 127 gave {high} Hz");
    assert!(
        high > low,
        "note 127 ({high} Hz) must be above note 0 ({low} Hz)"
    );

    // 12-TET spans 127 semitones from note 0 to note 127.
    let want = 2.0f32.powf(127.0 / 12.0);
    assert!(
        (high / low - want).abs() / want < 1e-3,
        "note 0 -> 127 spans {}x, expected {want}x",
        high / low
    );
}

#[test]
fn test_pythagorean_tuning() {
    let tuning = Tuning::pythagorean();

    // Reference should still be A4 = 440 Hz
    let a4 = tuning.note_to_freq(NoteNumber::of(69)).get();
    assert!((a4 - 440.0).abs() < 0.01);

    // Perfect fifth (A4 to E5) should be pure 3/2 ratio
    let e5 = tuning.note_to_freq(NoteNumber::of(76)).get();
    let ratio = e5 / a4;
    assert!(
        (ratio - 1.5).abs() < 0.01,
        "Pythagorean fifth should be 3/2, got {}",
        ratio
    );
}

#[test]
fn test_set_reference_dynamically() {
    let mut tuning = Tuning::equal_temperament();

    // Default A4 = 440
    assert!((tuning.note_to_freq(NoteNumber::of(69)).get() - 440.0).abs() < 0.01);

    // Change to A4 = 432 (alternative tuning)
    tuning.set_reference(Hz::new(432.0), NoteNumber::of(69));

    assert!((tuning.note_to_freq(NoteNumber::of(69)).get() - 432.0).abs() < 0.01);
    assert!((tuning.reference_freq().get() - 432.0).abs() < 0.01);
    assert_eq!(tuning.reference_note(), NoteNumber::of(69));

    // A5 should be 864 Hz (octave above 432)
    assert!((tuning.note_to_freq(NoteNumber::of(81)).get() - 864.0).abs() < 0.1);
}

#[test]
fn test_different_reference_note() {
    // Use C4 (note 60) as reference at 256 Hz
    let tuning = Tuning::equal_temperament_with_reference(Hz::new(256.0), NoteNumber::of(60));

    assert!((tuning.note_to_freq(NoteNumber::of(60)).get() - 256.0).abs() < 0.01);

    // C5 should be 512 Hz
    assert!((tuning.note_to_freq(NoteNumber::of(72)).get() - 512.0).abs() < 0.1);
}

#[test]
fn test_octave_equivalence() {
    let tuning = Tuning::equal_temperament();

    // Any note should be exactly 2x frequency of note 12 semitones below
    for base_note in 0..116u8 {
        let freq_low = tuning.note_to_freq(NoteNumber::of(base_note)).get();
        let freq_high = tuning.note_to_freq(NoteNumber::of(base_note + 12)).get();
        let ratio = freq_high / freq_low;
        assert!(
            (ratio - 2.0).abs() < 0.001,
            "Octave ratio should be 2.0, got {} for notes {} and {}",
            ratio,
            base_note,
            base_note + 12
        );
    }
}

#[test]
fn test_just_vs_equal_temperament_difference() {
    let just = Tuning::just_intonation();
    let equal = Tuning::equal_temperament();

    // Just intonation intervals are relative to C, not A
    // C4 = note 60, E4 = note 64 (major third)
    let c4_just = just.note_to_freq(NoteNumber::of(60)).get();
    let e4_just = just.note_to_freq(NoteNumber::of(64)).get();
    let ratio_just = e4_just / c4_just;

    let c4_equal = equal.note_to_freq(NoteNumber::of(60)).get();
    let e4_equal = equal.note_to_freq(NoteNumber::of(64)).get();
    let ratio_equal = e4_equal / c4_equal;

    // Just intonation major third from C is 5/4 = 1.25
    assert!(
        (ratio_just - 1.25).abs() < 0.01,
        "Just major third (C to E) should be 5/4 = 1.25, got {}",
        ratio_just
    );

    // Equal temperament major third is 2^(4/12) ≈ 1.2599
    assert!(
        (ratio_equal - 1.2599).abs() < 0.01,
        "Equal major third should be ~1.26, got {}",
        ratio_equal
    );

    // They should be different - this is the "syntonic comma"
    assert!(
        (ratio_just - ratio_equal).abs() > 0.005,
        "Just and equal thirds should differ by ~14 cents"
    );
}

#[test]
fn test_very_low_and_high_frequencies() {
    let tuning = Tuning::equal_temperament();

    // MIDI note 0 (C-1) should be about 8.18 Hz
    let lowest = tuning.note_to_freq(NoteNumber::of(0)).get();
    assert!(
        (lowest - 8.18).abs() < 0.1,
        "Note 0 should be ~8.18 Hz, got {}",
        lowest
    );

    // MIDI note 127 (G9) should be about 12543.85 Hz
    let highest = tuning.note_to_freq(NoteNumber::of(127)).get();
    assert!(
        (highest - 12543.85).abs() < 1.0,
        "Note 127 should be ~12543.85 Hz, got {}",
        highest
    );
}
