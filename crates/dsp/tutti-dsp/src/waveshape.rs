//! Memoryless waveshaping curves ([`ShapeKind`]): `Tanh`, `Atan`, `Softsign`,
//! `HardClip`, `Crush` and `SoftCrush`, each one line of arithmetic.

use tutti_types::Drive;

/// Selects which waveshaping curve a [`DistortionNode`] applies.
///
/// Discriminants are stable — they persist in saved projects via
/// `EffectKind::Distortion`. Append new kinds at the end.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum ShapeKind {
    /// `tanh` saturation — smooth, tube-like.
    #[default]
    Tanh,
    /// `atan` saturation — slightly harder knee than tanh.
    Atan,
    /// `softsign` (`x / (1 + |x|)`) — gentle.
    Softsign,
    /// Hard clip to ±1.
    HardClip,
    /// Bitcrush-style staircase quantization.
    Crush,
    /// Smoothed staircase quantization.
    SoftCrush,
}

/// Runs `$body` with `$k` bound to `$kind` as a **constant**, one arm per
/// kind, so each arm's loop is compiled for its own curve: the `match` in
/// [`ShapeKind::curve`] folds away instead of running per sample.
macro_rules! with_kind {
    ($kind:expr, |$k:ident| $body:expr) => {
        match $kind {
            ShapeKind::Tanh => {
                const $k: ShapeKind = ShapeKind::Tanh;
                $body
            }
            ShapeKind::Atan => {
                const $k: ShapeKind = ShapeKind::Atan;
                $body
            }
            ShapeKind::Softsign => {
                const $k: ShapeKind = ShapeKind::Softsign;
                $body
            }
            ShapeKind::HardClip => {
                const $k: ShapeKind = ShapeKind::HardClip;
                $body
            }
            ShapeKind::Crush => {
                const $k: ShapeKind = ShapeKind::Crush;
                $body
            }
            ShapeKind::SoftCrush => {
                const $k: ShapeKind = ShapeKind::SoftCrush;
                $body
            }
        }
    };
}

impl ShapeKind {
    /// Shapes one sample: `x` through this curve at `drive`.
    ///
    /// `drive` is input gain into the curve, floored at 0 — the same
    /// normalization [`DistortionNode`] applies to its param. For the two
    /// staircases it is instead the number of **levels per unit**, floored at 1
    /// (fewer than one level per unit is not a staircase).
    ///
    /// Memoryless and total: every curve maps every finite `x` to a finite
    /// output, and none keeps state between calls.
    #[inline]
    #[must_use]
    pub fn apply(self, x: f32, drive: Drive) -> f32 {
        Shaper::build(self, drive.get().max(0.0)).shape(x)
    }

    /// The curve at a hardness already normalized by [`Shaper::build`].
    ///
    /// The operation order within each arm is fixed: the pinned values in the
    /// tests depend on it bit for bit.
    #[inline(always)]
    #[must_use]
    pub fn curve(self, hardness: f32, x: f32) -> f32 {
        use core::f32::consts::PI;
        match self {
            Self::Tanh => (x * hardness).tanh(),
            // Rescaled to saturate at ±1 while keeping unit slope at the origin.
            Self::Atan => (x * (hardness * PI * 0.5)).atan() * (2.0 / PI),
            Self::Softsign => {
                let y = x * hardness;
                y / (1.0 + y.abs())
            }
            Self::HardClip => (x * hardness).clamp(-1.0, 1.0),
            Self::Crush => (x * hardness).round() / hardness,
            Self::SoftCrush => {
                let y = x * hardness;
                let step = y.floor();
                (step + smooth9(y - step)) / hardness
            }
        }
    }

    /// `input` through the curve at one `hardness`, into `output`
    /// (`output.len()` frames of `input` are read).
    ///
    /// The loop is compiled for the kind, so the curve inlines.
    #[inline]
    pub fn shape_block(self, hardness: f32, input: &[f32], output: &mut [f32]) {
        with_kind!(self, |K| {
            for (o, &x) in output.iter_mut().zip(input) {
                *o = K.curve(hardness, x);
            }
        })
    }

    /// `input` through the curve at a hardness per frame, into `output`.
    #[inline]
    pub fn shape_block_per_frame(self, hardness: &[f32], input: &[f32], output: &mut [f32]) {
        with_kind!(self, |K| {
            for ((o, &x), &h) in output.iter_mut().zip(input).zip(hardness) {
                *o = K.curve(h, x);
            }
        })
    }

    /// `input` through the curve with each frame's hardness held in `inout`,
    /// which the shaped sample replaces.
    #[inline]
    pub fn shape_block_in_place(self, input: &[f32], inout: &mut [f32]) {
        with_kind!(self, |K| {
            for (o, &x) in inout.iter_mut().zip(input) {
                *o = K.curve(*o, x);
            }
        })
    }
}

/// The ninth-order smoothstep: `0 → 0`, `1 → 1`, with the first four
/// derivatives zero at both ends, so each [`ShapeKind::SoftCrush`] step is a
/// smooth S rather than a jump.
#[inline]
fn smooth9(x: f32) -> f32 {
    let x2 = x * x;
    ((((70.0 * x - 315.0) * x + 540.0) * x - 420.0) * x + 126.0) * x2 * x2 * x
}

/// A curve at a fixed hardness: the kind plus its drive, already floored the
/// way the kind needs. Rebuilt only when drive moves, which is a field swap —
/// the curves carry no state.
#[derive(Clone, Copy, Debug)]
pub struct Shaper {
    kind: ShapeKind,
    hardness: f32,
}

impl Shaper {
    /// The shaper for `kind` at `drive`, floored the way the kind needs.
    #[must_use]
    pub fn build(kind: ShapeKind, drive: f32) -> Self {
        let hardness = match kind {
            ShapeKind::Crush | ShapeKind::SoftCrush => drive.max(1.0),
            _ => drive,
        };
        Self { kind, hardness }
    }

    /// The hardness the curve runs at.
    #[inline]
    #[must_use]
    pub fn hardness(&self) -> f32 {
        self.hardness
    }

    /// One sample through the curve.
    #[must_use]
    pub fn shape(&self, x: f32) -> f32 {
        self.kind.curve(self.hardness, x)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The curves, pinned.
    ///
    /// The formulas follow the common waveshaper definitions; these values
    /// pin them.
    ///
    /// The four curves built from `+ * / round floor clamp abs` are pinned
    /// **exactly**: IEEE-754 rounds each of those correctly, so the result is
    /// the same on every platform. `tanh` and `atan` are libm
    /// quality-of-implementation and differ in the last ulp between C runtimes
    /// (the same reason `tutti-core`'s click digest is gated off MSVC), so those
    /// two are pinned against the f64 closed form with a tolerance instead.
    ///
    /// Mutation: dropping `Crush`'s `max(1.0)` floor fails the drive-0.5 row
    /// (round(0.6·0.5) is 0, not 1); dropping Atan's `2/π`
    /// rescale fails its row by ~0.36; swapping Softsign's `abs` for the signed
    /// value fails the −3 row (−3/−2 = 1.5).
    #[test]
    fn curves_are_pinned() {
        let exact = [
            (ShapeKind::HardClip, 0.75, 2.0, 1.0),
            (ShapeKind::HardClip, -0.2, 2.0, -0.4),
            (ShapeKind::HardClip, -3.0, 1.0, -1.0),
            (ShapeKind::Softsign, 1.0, 1.0, 0.5),
            (ShapeKind::Softsign, -3.0, 1.0, -0.75),
            (ShapeKind::Crush, 0.3, 4.0, 0.25),
            // Drive below one level per unit floors to one.
            (ShapeKind::Crush, 0.6, 0.5, 1.0),
            (ShapeKind::Crush, 0.3, 0.5, 0.0),
            // SoftCrush passes through each step's midpoint and each integer.
            (ShapeKind::SoftCrush, 1.5, 1.0, 1.5),
            (ShapeKind::SoftCrush, 1.0, 1.0, 1.0),
            (ShapeKind::SoftCrush, 0.25, 2.0, 0.25),
        ];
        for (kind, x, drive, want) in exact {
            let got = kind.apply(x, Drive(drive));
            assert_eq!(
                got.to_bits(),
                f32::to_bits(want),
                "{kind:?}({x}, drive {drive}) = {got}, want exactly {want}"
            );
        }

        // Off the midpoint the S-curve is a rational: smooth9(1/4) is
        // 12826 / 262144 exactly, so at hardness 1 a quarter step lands there.
        let quarter = ShapeKind::SoftCrush.apply(0.25, Drive(1.0));
        assert!((f64::from(quarter) - 12826.0 / 262144.0).abs() < 1e-7);

        let half_pi = std::f64::consts::FRAC_PI_2;
        let closed = [
            (ShapeKind::Tanh, 0.5f32, 2.0f32, 1.0f64.tanh()),
            (ShapeKind::Tanh, -1.0, 3.0, (-3.0f64).tanh()),
            (ShapeKind::Atan, 1.0, 1.0, half_pi.atan() / half_pi),
            (
                ShapeKind::Atan,
                -0.5,
                4.0,
                (-half_pi * 2.0).atan() / half_pi,
            ),
        ];
        for (kind, x, drive, want) in closed {
            let got = f64::from(kind.apply(x, Drive(drive)));
            assert!(
                (got - want).abs() < 1e-6,
                "{kind:?}({x}, drive {drive}) = {got}, want {want}"
            );
        }
    }
}
