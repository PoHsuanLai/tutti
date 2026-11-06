//! Microtuning: an arbitrary scale resolved eagerly into a 128-entry frequency
//! table.
//!
//! The table is the point. A note-on happens on the audio thread, so mapping a
//! note number to [`Hz`] must be an array index — every scale, whether equal,
//! just, meantone or user-supplied, is flattened to the same table at
//! construction and costs the same to play.

use tutti_types::{Cents, Hz, Semitones};

// A second copy of A440. `Note::A4_HZ` is the engine's canonical one but is
// private to `tutti-types`, and widening its visibility is a bigger change
// than this file should make on its way past. The value is fixed by
// definition, so the duplication is inert — unlike the *conversions* that used
// to sit beside it, which are now `Semitones::to_pitch_ratio` and
// `Cents::to_pitch_ratio`.
const A4_FREQ: Hz = Hz(440.0);
const A4_NOTE: u8 = 69;

#[derive(Debug, Clone, Copy)]
pub(crate) struct ScaleDegree {
    /// 0 = unison, 1200 = octave
    pub cents: Cents,
}

impl ScaleDegree {
    pub fn from_cents(cents: impl Into<Cents>) -> Self {
        Self {
            cents: cents.into(),
        }
    }

    pub fn from_ratio(ratio: f32) -> Self {
        Self {
            cents: Cents::from_pitch_ratio(ratio),
        }
    }
}

/// A note-number-to-frequency map, resolved once into a 128-entry table.
///
/// The scale is a list of degrees in [`Cents`] that repeats every octave; the
/// constructors below build it, and the table is recomputed eagerly so lookup
/// on the audio thread is an array index with no arithmetic. That makes a
/// `Tuning` cheap to *use* and comparatively expensive to *build* — construct
/// it off the audio thread.
///
/// # Scale size decides where the repeat lands
///
/// A scale of `n` degrees maps note number `k` to degree `k % n` in octave
/// `k / n`, so a non-12 scale repeats every `n` note numbers rather than every
/// 12. A 24-degree quarter-tone scale therefore spans an octave across 24 keys,
/// and the keyboard covers correspondingly less range.
#[derive(Debug, Clone)]
pub struct Tuning {
    degrees: Vec<ScaleDegree>,
    freq_table: [Hz; 128],
    reference_freq: Hz,
    reference_note: u8,
}

impl Tuning {
    /// 12-tone equal temperament with A4 (note 69) at 440 [`Hz`] — the default,
    /// and the tuning every ratio-based interval below is measured against.
    pub fn equal_temperament() -> Self {
        Self::equal_temperament_with_reference(A4_FREQ, A4_NOTE)
    }

    /// 12-tone equal temperament pinned to an arbitrary reference.
    ///
    /// `reference_note` is the MIDI note number that sounds at
    /// `reference_freq`; everything else follows from equal-tempered steps
    /// either side. Use it for concert-pitch shifts (A4 = 432 Hz) or to pin the
    /// scale to a note other than A.
    pub fn equal_temperament_with_reference(
        reference_freq: impl Into<Hz>,
        reference_note: u8,
    ) -> Self {
        let degrees: Vec<ScaleDegree> = (0..12)
            .map(|i| ScaleDegree::from_cents(i as f32 * 100.0))
            .collect();

        let mut tuning = Self {
            degrees,
            freq_table: [Hz(0.0); 128],
            reference_freq: reference_freq.into(),
            reference_note,
        };
        tuning.recompute_table();
        tuning
    }

    /// 5-limit just intonation: twelve degrees built from whole-number ratios
    /// (16/15, 9/8, 6/5, 5/4, 4/3, …), rooted on C and pinned at A4 = 440 [`Hz`].
    ///
    /// Intervals in the home key beat perfectly — the major third is exactly
    /// 5/4 rather than equal temperament's 1.2599, about 14 cents flatter (the
    /// syntonic comma) — at the cost of intervals in distant keys, which is the
    /// point of the tuning and the reason it is not the default.
    ///
    /// Degree 0 lands on note numbers divisible by 12, so the scale is rooted
    /// on C even though the *pitch* reference is A4.
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
            freq_table: [Hz(0.0); 128],
            reference_freq: A4_FREQ,
            reference_note: A4_NOTE,
        };
        tuning.recompute_table();
        tuning
    }

    /// Pythagorean tuning: twelve degrees stacked from pure 3/2 fifths, rooted
    /// on C and pinned at A4 = 440 [`Hz`].
    ///
    /// Every fifth is exactly 3/2, which makes the thirds correspondingly wide
    /// (81/64 rather than 5/4) and leaves the wolf interval where the stack
    /// closes.
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
            freq_table: [Hz(0.0); 128],
            reference_freq: A4_FREQ,
            reference_note: A4_NOTE,
        };
        tuning.recompute_table();
        tuning
    }

    /// Quarter-comma meantone, twelve degrees given to the nearest whole
    /// [`Cents`] (0, 76, 193, 310, 386, …), rooted on C and pinned at
    /// A4 = 440 [`Hz`].
    ///
    /// The historical compromise between [`just_intonation`](Self::just_intonation)
    /// and [`equal_temperament`](Self::equal_temperament): thirds are near-pure
    /// (386 cents against just's 386 and equal's 400) and the error is pushed
    /// into the fifths.
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
            freq_table: [Hz(0.0); 128],
            reference_freq: A4_FREQ,
            reference_note: A4_NOTE,
        };
        tuning.recompute_table();
        tuning
    }

    /// An arbitrary scale from a list of degrees in cents above the root, e.g.
    /// `[0.0, 50.0, 100.0, …]` for quarter tones.
    ///
    /// The scale repeats every octave (1200 cents), and its *length* — not 12 —
    /// is the number of note numbers one repeat spans. Pinned at
    /// A4 = 440 [`Hz`]. An empty slice falls back to 12-tone equal temperament.
    #[must_use]
    pub fn from_cents(cents: &[f32]) -> Self {
        let degrees: Vec<ScaleDegree> = cents.iter().map(|c| ScaleDegree::from_cents(*c)).collect();

        let mut tuning = Self {
            degrees,
            freq_table: [Hz(0.0); 128],
            reference_freq: A4_FREQ,
            reference_note: A4_NOTE,
        };
        tuning.recompute_table();
        tuning
    }

    /// An arbitrary scale from frequency ratios above the root, e.g.
    /// `[1.0, 9.0/8.0, 3.0/2.0]`. Each is converted to [`Cents`], so this is
    /// [`from_cents`](Self::from_cents) with the multiplication spelled the way
    /// historical tunings are written. Same octave-repeat and reference rules.
    #[must_use]
    pub fn from_ratios(ratios: &[f32]) -> Self {
        let degrees: Vec<ScaleDegree> =
            ratios.iter().map(|r| ScaleDegree::from_ratio(*r)).collect();

        let mut tuning = Self {
            degrees,
            freq_table: [Hz(0.0); 128],
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
            let cents_diff =
                note_cents - ref_cents + Semitones::OCTAVE.to_cents() * octave_diff as f32;

            self.freq_table[note] = self.reference_freq * cents_diff.to_pitch_ratio();
        }
    }

    /// The frequency in [`Hz`] of a whole MIDI note.
    #[cfg(test)]
    #[inline]
    pub fn note_to_freq(&self, note: u8) -> Hz {
        self.freq_table[usize::from(note)]
    }

    /// The frequency in [`Hz`] for a fractional MIDI note number.
    ///
    /// Interpolates geometrically (in log-frequency) between the two adjacent
    /// table entries, so 69.5 is the geometric mean of notes 69 and 70 rather
    /// than the arithmetic one. That is what lets an absolute per-note tuning
    /// override land between keys.
    ///
    /// Note numbers outside 0..=127 clamp to the end entries rather than
    /// extrapolating — this runs on the audio thread and never panics on a
    /// wild input.
    ///
    /// The *interpolation* is always geometric even when the scale is not
    /// equal-tempered, so a fractional note in an unequal scale is not
    /// generally where that scale would put it.
    #[inline]
    pub fn fractional_note_to_freq(&self, note: f32) -> Hz {
        if note <= 0.0 {
            return self.freq_table[0];
        }
        if note >= 127.0 {
            return self.freq_table[127];
        }

        let low = note.floor() as usize;
        let high = (low + 1).min(127);
        let frac = note - low as f32;

        let low_freq = self.freq_table[low].get();
        let high_freq = self.freq_table[high].get();

        Hz((low_freq.ln() + (high_freq.ln() - low_freq.ln()) * frac).exp())
    }

    /// Number of degrees in the scale.
    #[cfg(test)]
    pub fn scale_size(&self) -> usize {
        self.degrees.len()
    }

    /// Sets the reference frequency and note, then rebuilds the table.
    #[cfg(test)]
    pub fn set_reference(&mut self, freq: impl Into<Hz>, note: u8) {
        self.reference_freq = freq.into();
        self.reference_note = note;
        self.recompute_table();
    }

    /// The reference frequency.
    #[cfg(test)]
    pub fn reference_freq(&self) -> Hz {
        self.reference_freq
    }

    /// The MIDI note the reference frequency is pinned to.
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
mod tests;
