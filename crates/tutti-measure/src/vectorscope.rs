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

/// Where the stride stands between blocks.
///
/// Carries how many frames to skip before the next point, so a stream fed in
/// blocks keeps one stride phase instead of restarting it per block. Keep the
/// same `decimate` for a run; a different one only changes the stride from the
/// next point on.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VectorscopeState {
    skip: usize,
}

impl VectorscopeState {
    /// The start of a stream: the first frame is kept.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

/// Appends every `decimate`-th frame of the next block to `out` as a
/// [`ScopePoint`]. `1` keeps every frame.
///
/// Writes into the caller's buffer, so a UI feeding blocks reuses one `Vec`
/// (clear it between draws). It grows `out` and so allocates when it is full:
/// not for the audio thread.
pub fn step_vectorscope(
    state: &mut VectorscopeState,
    planes: StereoPlanes<'_>,
    decimate: NonZeroUsize,
    out: &mut Vec<ScopePoint>,
) {
    let frames = planes.frames();
    let (left, right) = (planes.left(), planes.right());
    let mut at = state.skip;
    while at < frames {
        out.push(ScopePoint::from_lr(left[at], right[at]));
        at += decimate.get();
    }
    state.skip = at - frames;
}

/// Every `decimate`-th frame of one whole pair as a [`ScopePoint`], starting
/// at the first: [`step_vectorscope`] over a fresh state. Allocates the result.
pub fn vectorscope_points(planes: StereoPlanes<'_>, decimate: NonZeroUsize) -> Vec<ScopePoint> {
    let mut out = Vec::with_capacity(planes.frames().div_ceil(decimate.get()));
    step_vectorscope(&mut VectorscopeState::new(), planes, decimate, &mut out);
    out
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

    /// Feeding blocks of any size, through one reused buffer, gives the batch
    /// points: the stride phase carries over block edges.
    #[test]
    fn blocks_of_any_size_keep_one_stride_phase() {
        let l: Vec<f32> = (0..103).map(|i| i as f32).collect();
        let r: Vec<f32> = (0..103).map(|i| -(i as f32) * 0.5).collect();
        for decimate in [1usize, 3, 4, 7, 200] {
            let d = NonZeroUsize::new(decimate).unwrap();
            let batch = scope(&l, &r, d);
            for block in [1usize, 2, 5, 10, 64, 103] {
                let mut state = VectorscopeState::new();
                let mut out = Vec::new();
                for (lb, rb) in l.chunks(block).zip(r.chunks(block)) {
                    step_vectorscope(&mut state, StereoPlanes::new(lb, rb).unwrap(), d, &mut out);
                }
                assert_eq!(out, batch, "decimate {decimate}, block {block}");
            }
        }
    }
}
