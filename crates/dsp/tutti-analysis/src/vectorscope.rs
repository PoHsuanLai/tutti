//! Vectorscope (goniometer) data: a stereo pair as a stream of points.
//!
//! The scope draws each frame at `(side, mid)`, the left/right plane rotated
//! by 45 degrees so mono falls on the vertical axis, a polarity-inverted pair on
//! the horizontal, and a signal in one channel on a diagonal. The rotation is
//! orthonormal, so a point's distance from the origin is the frame's own level
//! `sqrt(l^2 + r^2)`.
//!
//! This is the data only: drawing, persistence and fading are the display's.

use core::num::NonZeroUsize;

use tutti_types::StereoPlanes;

/// One frame on the scope.
///
/// Signed sample values, not unit quantities: they are audio, like the
/// `&[f32]` they come from.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct ScopePoint {
    /// `(l - r) / sqrt(2)`: the horizontal axis.
    pub side: f32,
    /// `(l + r) / sqrt(2)`: the vertical axis.
    pub mid: f32,
}

impl ScopePoint {
    /// The frame at left `l` and right `r`.
    #[inline]
    #[must_use]
    pub fn from_lr(l: f32, r: f32) -> Self {
        Self {
            side: (l - r) * core::f32::consts::FRAC_1_SQRT_2,
            mid: (l + r) * core::f32::consts::FRAC_1_SQRT_2,
        }
    }
}

/// Every `decimate`-th frame of the pair as a [`ScopePoint`], starting at the
/// first. `1` keeps every frame.
///
/// Stateless: a caller feeding blocks passes each pair with the same stride and
/// accepts a stride phase that restarts per block. Allocates the result.
pub fn vectorscope_points(planes: StereoPlanes<'_>, decimate: NonZeroUsize) -> Vec<ScopePoint> {
    planes
        .left()
        .iter()
        .zip(planes.right())
        .step_by(decimate.get())
        .map(|(&l, &r)| ScopePoint::from_lr(l, r))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const ONE: NonZeroUsize = NonZeroUsize::MIN;

    fn sine(len: usize) -> Vec<f32> {
        (0..len)
            .map(|i| (core::f32::consts::TAU * 440.0 * i as f32 / 48_000.0).sin())
            .collect()
    }

    fn scope(l: &[f32], r: &[f32], decimate: NonZeroUsize) -> Vec<ScopePoint> {
        vectorscope_points(StereoPlanes::new(l, r).unwrap(), decimate)
    }

    /// Mono lies on the mid axis, with the mono level times sqrt 2 as its
    /// height.
    ///
    /// Mutation checked: `l + r` for `l - r` in `side` puts the points on the
    /// diagonal and fails; dropping the `1/sqrt 2` scale fails the height.
    #[test]
    fn mono_lies_on_the_mid_axis() {
        let x = sine(480);
        for (p, &s) in scope(&x, &x, ONE).iter().zip(&x) {
            assert_eq!(p.side, 0.0);
            assert!((p.mid - s * core::f32::consts::SQRT_2).abs() < 1e-6);
        }
    }

    /// An inverted right channel lies on the side axis, and a signal in one
    /// channel on a diagonal of slope +-1.
    ///
    /// Mutation checked: `l - r` for `r - l` in `side` flips the sign and fails
    /// the left-only diagonal.
    #[test]
    fn inverted_pairs_lie_on_the_side_axis_and_one_channel_on_a_diagonal() {
        let x = sine(480);
        let inverted: Vec<f32> = x.iter().map(|v| -v).collect();
        for p in scope(&x, &inverted, ONE) {
            assert_eq!(p.mid, 0.0);
        }
        let silent = vec![0.0f32; x.len()];
        // Left only: side == mid. Right only: side == -mid.
        for p in scope(&x, &silent, ONE) {
            assert_eq!(p.side, p.mid);
        }
        for p in scope(&silent, &x, ONE) {
            assert_eq!(p.side, -p.mid);
        }
    }

    /// A panned sine sits at the angle its gains fix: `side / mid` is
    /// `(gl - gr) / (gl + gr)` on every frame, and the radius is the frame's
    /// level, which the rotation preserves.
    ///
    /// Mutation checked: swapping the two channels in `from_lr` negates the
    /// ratio and fails; using a `0.5` scale in place of `1/sqrt 2` fails the
    /// radius.
    #[test]
    fn a_panned_sine_sits_at_its_gain_angle_with_its_level_as_radius() {
        let x = sine(480);
        let (gl, gr) = (0.8f32, 0.2f32);
        let left: Vec<f32> = x.iter().map(|v| v * gl).collect();
        let right: Vec<f32> = x.iter().map(|v| v * gr).collect();
        let expected = (gl - gr) / (gl + gr);
        for ((p, &l), &r) in scope(&left, &right, ONE).iter().zip(&left).zip(&right) {
            assert!((p.side * (gl + gr) - p.mid * (gl - gr)).abs() < 1e-6);
            if p.mid.abs() > 0.1 {
                assert!((p.side / p.mid - expected).abs() < 1e-4);
            }
            let radius = (p.side * p.side + p.mid * p.mid).sqrt();
            assert!((radius - (l * l + r * r).sqrt()).abs() < 1e-6);
        }
    }

    /// Decimation keeps frames `0, n, 2n, ...` and an empty pair gives no
    /// points.
    ///
    /// Mutation checked: starting at frame 1 (`skip(1)`), or `step_by(n + 1)`,
    /// fails the indices.
    #[test]
    fn decimation_keeps_every_nth_frame_from_the_first() {
        let l: Vec<f32> = (0..10).map(|i| i as f32).collect();
        let r = vec![0.0f32; 10];
        let kept = scope(&l, &r, NonZeroUsize::new(4).unwrap());
        let mids: Vec<f32> = kept.iter().map(|p| p.mid).collect();
        let want: Vec<f32> = [0.0f32, 4.0, 8.0]
            .iter()
            .map(|v| v * core::f32::consts::FRAC_1_SQRT_2)
            .collect();
        assert_eq!(mids, want);
        assert!(scope(&[], &[], ONE).is_empty());
    }
}
