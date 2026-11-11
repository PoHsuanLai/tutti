//! The mixer strip's stereo balance law.

use tutti_types::{Amplitude, Pan};

/// The gains `(left, right)` of a stereo **balance** at position `pan`.
///
/// Attenuates the channel you pan away from and leaves the other at unity:
/// `left = min(1, 1 - p)`, `right = min(1, 1 + p)` for `p` in `-1..=1`, so
/// `(1, 1)` at centre, `(1, 0)` hard left and `(0, 1)` hard right. A `pan`
/// outside the range is clamped to it, and a NaN reads as unity on both sides.
///
/// This is balance, not panning. A panner places a mono signal with an
/// equal-power law that reads -3 dB on each side at centre; a balance
/// rebalances a pair that is already stereo, so centre must be exactly unity.
/// It is not VBAP's power-normalised law either: nothing is boosted and the
/// pair's power is not held constant.
#[inline]
#[must_use]
pub fn balance_gains(pan: Pan) -> (Amplitude, Amplitude) {
    let p = pan.get().clamp(-1.0, 1.0);
    (Amplitude((1.0 - p).min(1.0)), Amplitude((1.0 + p).min(1.0)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(p: f32) -> (f32, f32) {
        let (l, r) = balance_gains(Pan(p));
        (l.get(), r.get())
    }

    /// Unity at centre, one side silent at each extreme, the far side falling
    /// linearly and the near side held at unity.
    #[test]
    fn balance_is_unity_at_centre_and_silences_the_far_side_at_the_extremes() {
        assert_eq!(at(0.0), (1.0, 1.0));
        assert_eq!(at(-1.0), (1.0, 0.0));
        assert_eq!(at(1.0), (0.0, 1.0));
        assert_eq!(at(-0.5), (1.0, 0.5));
        assert_eq!(at(0.25), (0.75, 1.0));
    }

    /// Nothing is boosted, and the far side is monotone in the position.
    #[test]
    fn balance_never_boosts_and_is_monotone() {
        let mut prev = f32::INFINITY;
        for i in 0..=40 {
            let p = -1.0 + i as f32 * 0.05;
            let (l, r) = at(p);
            assert!(l <= 1.0 && r <= 1.0 && l >= 0.0 && r >= 0.0);
            assert!(l <= prev, "left falls as the pan moves right");
            prev = l;
        }
    }

    /// A pan past the range holds the extreme; a NaN is unity on both sides.
    #[test]
    fn out_of_range_and_nan_are_held() {
        assert_eq!(at(3.0), (0.0, 1.0));
        assert_eq!(at(-3.0), (1.0, 0.0));
        assert_eq!(at(f32::NAN), (1.0, 1.0));
    }
}
