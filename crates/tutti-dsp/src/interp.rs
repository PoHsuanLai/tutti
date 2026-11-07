//! Interpolation kernels shared by the delay line and the sampler's readers.

/// Catmull-Rom cubic Hermite interpolation across four consecutive taps.
///
/// `y1` is the sample at the integer position, `y0`/`y2`/`y3` its neighbours
/// (`y0` one behind, `y2`/`y3` ahead); `t` is the fractional offset in
/// `[0, 1)` between `y1` and `y2`.
///
/// The operation order is part of the contract: the sampler's readers and the
/// delay line render bit for bit what this expression gives, so a
/// re-association changes their output. Per-sample, so `#[inline]` lets the
/// sampler's loops fuse it across the crate boundary.
///
/// ```
/// use tutti_dsp::cubic_hermite;
///
/// // Along a line the cubic is the line.
/// assert_eq!(cubic_hermite(0.0, 1.0, 2.0, 3.0, 0.5), 1.5);
/// ```
#[inline]
pub fn cubic_hermite(y0: f32, y1: f32, y2: f32, y3: f32, t: f32) -> f32 {
    let c0 = y1;
    let c1 = 0.5 * (y2 - y0);
    let c2 = y0 - 2.5 * y1 + 2.0 * y2 - 0.5 * y3;
    let c3 = 0.5 * (y3 - y0) + 1.5 * (y1 - y2);
    ((c3 * t + c2) * t + c1) * t + c0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The curve passes through its inner taps at `t = 0` and `t = 1`. For
    /// the taps -1, 0, 1, 8 the coefficients are `c1 = 1`, `c2 = -3`,
    /// `c3 = 3`, so the curve is `3t^3 - 3t^2 + t` and its midpoint is 0.125.
    ///
    /// Mutations (each fails): `c1` without the `0.5`; `c0 = y2`; `y0` and
    /// `y3` swapped in `c3`.
    #[test]
    fn passes_through_its_inner_taps_and_follows_the_cubic() {
        let (y0, y1, y2, y3) = (-1.0, 0.0, 1.0, 8.0);
        assert_eq!(cubic_hermite(y0, y1, y2, y3, 0.0), y1);
        assert_eq!(cubic_hermite(y0, y1, y2, y3, 1.0), y2);
        assert_eq!(cubic_hermite(y0, y1, y2, y3, 0.5), 0.125);
    }
}
