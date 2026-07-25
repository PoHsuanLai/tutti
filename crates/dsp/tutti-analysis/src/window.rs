//! Analysis windows.
//!
//! One Hann, where there were eight copies. Seven agreed; the eighth divided
//! by `size - 1` instead of `size` — the *symmetric* Hann rather than the
//! periodic one. Only the periodic form satisfies the COLA property the
//! inverse transform's normalization assumes, so the odd copy out was both
//! inconsistent with its siblings and wrong for overlap-add. It also divided
//! by zero at `size == 1`.

/// Periodic Hann window of `size` points.
///
/// Periodic (`i / size`), not symmetric (`i / (size - 1)`): the periodic form
/// is what constant-overlap-add requires, because it tiles without the
/// duplicated endpoint that breaks the window sum.
///
/// `size == 0` yields an empty window; `size == 1` yields `[0.0]` rather than
/// dividing by zero.
pub fn hann(size: usize) -> Vec<f32> {
    (0..size)
        .map(|i| {
            let angle = 2.0 * core::f32::consts::PI * i as f32 / size as f32;
            0.5 * (1.0 - angle.cos())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn periodic_hann_starts_at_zero_and_peaks_at_the_centre() {
        let w = hann(8);
        assert_eq!(w.len(), 8);
        assert!(w[0].abs() < 1e-6, "first point is zero");
        assert!((w[4] - 1.0).abs() < 1e-6, "centre reaches unity");
        // Periodic: the last point is NOT zero, which is what distinguishes it
        // from the symmetric form and what makes it tile.
        assert!(w[7] > 0.0);
    }

    /// The distinguishing property: overlapping periodic Hann² windows at 75%
    /// sum to a constant. The symmetric variant does not, which is why the odd
    /// copy out was a real defect and not a rounding preference.
    #[test]
    fn hann_squared_overlap_adds_to_a_constant() {
        let (size, hop) = (64usize, 16usize);
        let w = hann(size);

        // Sum w² at 4x overlap across the steady-state interior.
        let frames = 16;
        let mut acc = vec![0.0f32; frames * hop + size];
        for f in 0..frames {
            for (i, &v) in w.iter().enumerate() {
                acc[f * hop + i] += v * v;
            }
        }

        // Away from the ramp-in/ramp-out edges the sum is flat.
        let interior = &acc[size..frames * hop];
        let first = interior[0];
        for (i, &v) in interior.iter().enumerate() {
            assert!(
                (v - first).abs() < 1e-4,
                "window sum not constant at interior sample {i}: {v} vs {first}"
            );
        }
    }

    #[test]
    fn degenerate_sizes_do_not_divide_by_zero() {
        assert!(hann(0).is_empty());
        assert_eq!(hann(1), vec![0.0]);
    }
}
