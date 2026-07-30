//! Microtuning support with pre-computed 128-note frequency table.

use tutti_core::{Cents, Semitones};

// A second copy of A440. `Note::A4_HZ` is the engine's canonical one but is
// private to `tutti-types`, and widening its visibility is a bigger change
// than this file should make on its way past. The value is fixed by
// definition, so the duplication is inert — unlike the *conversions* that used
// to sit beside it, which are now `Semitones::to_pitch_ratio` and
// `Cents::to_pitch_ratio`.
const A4_FREQ: f32 = 440.0;
const A4_NOTE: u8 = 69;

#[derive(Debug, Clone, Copy)]
pub(crate) struct ScaleDegree {
    /// 0 = unison, 1200 = octave
    pub cents: f32,
}

impl ScaleDegree {
    pub fn from_cents(cents: f32) -> Self {
        Self { cents }
    }

    pub fn from_ratio(ratio: f32) -> Self {
        Self {
            cents: 1200.0 * ratio.log2(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Tuning {
    degrees: Vec<ScaleDegree>,
    freq_table: [f32; 128],
    reference_freq: f32,
    reference_note: u8,
}

impl Tuning {
    pub fn equal_temperament() -> Self {
        Self::equal_temperament_with_reference(A4_FREQ, A4_NOTE)
    }

    pub fn equal_temperament_with_reference(reference_freq: f32, reference_note: u8) -> Self {
        let degrees: Vec<ScaleDegree> = (0..12)
            .map(|i| ScaleDegree::from_cents(i as f32 * 100.0))
            .collect();

        let mut tuning = Self {
            degrees,
            freq_table: [0.0; 128],
            reference_freq,
            reference_note,
        };
        tuning.recompute_table();
        tuning
    }

    pub fn just_intonation() -> Self {
        let ratios = [
            1.0,
            16.0 / 15.0,
            9.0 / 8.0,
            6.0 / 5.0,
            5.0 / 4.0,
            4.0 / 3.0,
            45.0 / 32.0,
            3.0 / 2.0,
            8.0 / 5.0,
            5.0 / 3.0,
            9.0 / 5.0,
            15.0 / 8.0,
        ];

        let degrees: Vec<ScaleDegree> =
            ratios.iter().map(|r| ScaleDegree::from_ratio(*r)).collect();

        let mut tuning = Self {
            degrees,
            freq_table: [0.0; 128],
            reference_freq: A4_FREQ,
            reference_note: A4_NOTE,
        };
        tuning.recompute_table();
        tuning
    }

    pub fn pythagorean() -> Self {
        let ratios = [
            1.0,
            256.0 / 243.0,
            9.0 / 8.0,
            32.0 / 27.0,
            81.0 / 64.0,
            4.0 / 3.0,
            729.0 / 512.0,
            3.0 / 2.0,
            128.0 / 81.0,
            27.0 / 16.0,
            16.0 / 9.0,
            243.0 / 128.0,
        ];

        let degrees: Vec<ScaleDegree> =
            ratios.iter().map(|r| ScaleDegree::from_ratio(*r)).collect();

        let mut tuning = Self {
            degrees,
            freq_table: [0.0; 128],
            reference_freq: A4_FREQ,
            reference_note: A4_NOTE,
        };
        tuning.recompute_table();
        tuning
    }

    pub fn meantone() -> Self {
        let mut cents = Vec::with_capacity(12);

        for i in 0..12 {
            let c = match i {
                0 => 0.0,
                1 => 76.0,
                2 => 193.0,
                3 => 310.0,
                4 => 386.0,
                5 => 503.0,
                6 => 579.0,
                7 => 697.0,
                8 => 773.0,
                9 => 890.0,
                10 => 1007.0,
                11 => 1083.0,
                _ => 0.0,
            };
            cents.push(ScaleDegree::from_cents(c));
        }

        let mut tuning = Self {
            degrees: cents,
            freq_table: [0.0; 128],
            reference_freq: A4_FREQ,
            reference_note: A4_NOTE,
        };
        tuning.recompute_table();
        tuning
    }

    /// Scale repeats every octave (1200 cents).
    pub fn from_cents(cents: &[f32]) -> Self {
        let degrees: Vec<ScaleDegree> = cents.iter().map(|c| ScaleDegree::from_cents(*c)).collect();

        let mut tuning = Self {
            degrees,
            freq_table: [0.0; 128],
            reference_freq: A4_FREQ,
            reference_note: A4_NOTE,
        };
        tuning.recompute_table();
        tuning
    }

    pub fn from_ratios(ratios: &[f32]) -> Self {
        let degrees: Vec<ScaleDegree> =
            ratios.iter().map(|r| ScaleDegree::from_ratio(*r)).collect();

        let mut tuning = Self {
            degrees,
            freq_table: [0.0; 128],
            reference_freq: A4_FREQ,
            reference_note: A4_NOTE,
        };
        tuning.recompute_table();
        tuning
    }

    fn recompute_table(&mut self) {
        let scale_size = self.degrees.len();
        if scale_size == 0 {
            for note in 0..128 {
                let semitones = Semitones(note as f32 - f32::from(self.reference_note));
                self.freq_table[note] = self.reference_freq * semitones.to_pitch_ratio();
            }
            return;
        }

        let ref_scale_pos = usize::from(self.reference_note) % scale_size;
        let ref_octave = i32::from(self.reference_note) / (scale_size as i32);

        for note in 0..128 {
            let note_scale_pos = note % scale_size;
            let note_octave = (note as i32) / (scale_size as i32);

            let note_cents = self.degrees[note_scale_pos].cents;
            let ref_cents = self.degrees[ref_scale_pos].cents;

            let octave_diff = note_octave - ref_octave;
            let cents_diff = Cents(note_cents - ref_cents + (octave_diff as f32 * 1200.0));

            self.freq_table[note] = self.reference_freq * cents_diff.to_pitch_ratio();
        }
    }

    #[cfg(test)]
    #[inline]
    pub fn note_to_freq(&self, note: u8) -> f32 {
        self.freq_table[usize::from(note)]
    }

    /// Interpolates between adjacent notes in log space for pitch bend/portamento.
    #[inline]
    pub fn fractional_note_to_freq(&self, note: f32) -> f32 {
        if note <= 0.0 {
            return self.freq_table[0];
        }
        if note >= 127.0 {
            return self.freq_table[127];
        }

        let low = note.floor() as usize;
        let high = (low + 1).min(127);
        let frac = note - low as f32;

        let low_freq = self.freq_table[low];
        let high_freq = self.freq_table[high];

        (low_freq.ln() + (high_freq.ln() - low_freq.ln()) * frac).exp()
    }

    #[cfg(test)]
    pub fn scale_size(&self) -> usize {
        self.degrees.len()
    }

    #[cfg(test)]
    pub fn set_reference(&mut self, freq: f32, note: u8) {
        self.reference_freq = freq;
        self.reference_note = note;
        self.recompute_table();
    }

    #[cfg(test)]
    pub fn reference_freq(&self) -> f32 {
        self.reference_freq
    }

    #[cfg(test)]
    pub fn reference_note(&self) -> u8 {
        self.reference_note
    }
}

impl Default for Tuning {
    fn default() -> Self {
        Self::equal_temperament()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_equal_temperament() {
        let tuning = Tuning::equal_temperament();

        // A4 should be 440 Hz
        assert!((tuning.note_to_freq(69) - 440.0).abs() < 0.01);

        // A5 should be 880 Hz (octave above)
        assert!((tuning.note_to_freq(81) - 880.0).abs() < 0.01);

        // A3 should be 220 Hz (octave below)
        assert!((tuning.note_to_freq(57) - 220.0).abs() < 0.01);

        // Middle C (C4) should be ~261.6 Hz
        assert!((tuning.note_to_freq(60) - 261.63).abs() < 0.1);
    }

    #[test]
    fn test_custom_reference() {
        let tuning = Tuning::equal_temperament_with_reference(432.0, 69);

        // A4 should be 432 Hz
        assert!((tuning.note_to_freq(69) - 432.0).abs() < 0.01);
    }

    #[test]
    fn test_just_intonation() {
        let tuning = Tuning::just_intonation();

        // Reference should still be A4 = 440 Hz
        let a4 = tuning.note_to_freq(69);
        assert!((a4 - 440.0).abs() < 0.01);

        // Perfect fifth above A4 (E5) should be 3/2 ratio
        // In just intonation from C, perfect fifth is 3/2
        // E5 is note 76, A4 is note 69
        // This is 7 semitones, so it's a perfect fifth from A to E
        let e5 = tuning.note_to_freq(76);
        let ratio = e5 / a4;
        // Should be close to 3/2 = 1.5
        assert!((ratio - 1.5).abs() < 0.02);
    }

    #[test]
    fn test_fractional_note() {
        let tuning = Tuning::equal_temperament();

        let a4 = tuning.note_to_freq(69);
        let a4_50 = tuning.fractional_note_to_freq(69.5);
        let bb4 = tuning.note_to_freq(70);

        // Fractional note should be between
        assert!(a4_50 > a4);
        assert!(a4_50 < bb4);

        // Should be roughly in the middle (geometrically)
        let expected = (a4 * bb4).sqrt();
        assert!((a4_50 - expected).abs() < 0.1);
    }

    #[test]
    fn test_from_cents() {
        // Create quarter-tone scale (24 notes per octave)
        let cents: Vec<f32> = (0..24).map(|i| i as f32 * 50.0).collect();
        let tuning = Tuning::from_cents(&cents);

        assert_eq!(tuning.scale_size(), 24);

        // Note 0 and note 24 should be an octave apart
        let freq_0 = tuning.note_to_freq(0);
        let freq_24 = tuning.note_to_freq(24);
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

    #[test]
    fn test_boundary_notes() {
        let tuning = Tuning::equal_temperament();

        // Should not panic at boundaries
        let _low = tuning.note_to_freq(0);
        let _high = tuning.note_to_freq(127);

        // Fractional boundaries
        let _low_frac = tuning.fractional_note_to_freq(-1.0);
        let _high_frac = tuning.fractional_note_to_freq(128.0);
    }

    #[test]
    fn test_pythagorean_tuning() {
        let tuning = Tuning::pythagorean();

        // Reference should still be A4 = 440 Hz
        let a4 = tuning.note_to_freq(69);
        assert!((a4 - 440.0).abs() < 0.01);

        // Perfect fifth (A4 to E5) should be pure 3/2 ratio
        let e5 = tuning.note_to_freq(76);
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
        assert!((tuning.note_to_freq(69) - 440.0).abs() < 0.01);

        // Change to A4 = 432 (alternative tuning)
        tuning.set_reference(432.0, 69);

        assert!((tuning.note_to_freq(69) - 432.0).abs() < 0.01);
        assert!((tuning.reference_freq() - 432.0).abs() < 0.01);
        assert_eq!(tuning.reference_note(), 69);

        // A5 should be 864 Hz (octave above 432)
        assert!((tuning.note_to_freq(81) - 864.0).abs() < 0.1);
    }

    #[test]
    fn test_different_reference_note() {
        // Use C4 (note 60) as reference at 256 Hz
        let tuning = Tuning::equal_temperament_with_reference(256.0, 60);

        assert!((tuning.note_to_freq(60) - 256.0).abs() < 0.01);

        // C5 should be 512 Hz
        assert!((tuning.note_to_freq(72) - 512.0).abs() < 0.1);
    }

    #[test]
    fn test_scale_size() {
        // Standard 12-TET
        let tuning_12 = Tuning::equal_temperament();
        assert_eq!(tuning_12.scale_size(), 12);

        // Quarter-tone (24 notes per octave)
        let cents: Vec<f32> = (0..24).map(|i| i as f32 * 50.0).collect();
        let tuning_24 = Tuning::from_cents(&cents);
        assert_eq!(tuning_24.scale_size(), 24);

        // Pentatonic (5 notes per octave)
        let ratios = [1.0, 9.0 / 8.0, 5.0 / 4.0, 3.0 / 2.0, 5.0 / 3.0];
        let tuning_5 = Tuning::from_ratios(&ratios);
        assert_eq!(tuning_5.scale_size(), 5);
    }

    #[test]
    fn test_octave_equivalence() {
        let tuning = Tuning::equal_temperament();

        // Any note should be exactly 2x frequency of note 12 semitones below
        for base_note in 0..116 {
            let freq_low = tuning.note_to_freq(base_note);
            let freq_high = tuning.note_to_freq(base_note + 12);
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
        let c4_just = just.note_to_freq(60);
        let e4_just = just.note_to_freq(64);
        let ratio_just = e4_just / c4_just;

        let c4_equal = equal.note_to_freq(60);
        let e4_equal = equal.note_to_freq(64);
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
        let lowest = tuning.note_to_freq(0);
        assert!(
            (lowest - 8.18).abs() < 0.1,
            "Note 0 should be ~8.18 Hz, got {}",
            lowest
        );

        // MIDI note 127 (G9) should be about 12543.85 Hz
        let highest = tuning.note_to_freq(127);
        assert!(
            (highest - 12543.85).abs() < 1.0,
            "Note 127 should be ~12543.85 Hz, got {}",
            highest
        );
    }
}
