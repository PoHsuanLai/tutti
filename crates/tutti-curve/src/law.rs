/// How a piece moves from its start value to its end value: a function `g` of
/// its progress `t` in `[0, 1]`, with `g(0) = 0` and `g(1) = 1`; the value is
/// `from + (to - from) · g(t)`.
///
/// A **closed set**, so a segment stays a `Copy` value a few words long on the
/// audio thread rather than an `Arc<dyn Curve>`. Every law is monotone between
/// its endpoints, so a piece never leaves `[from, to]`.
///
/// A parameter that is not finite, or out of the range a variant states,
/// evaluates as [`Linear`](Self::Linear): the audio thread never computes a
/// NaN from a law.
///
/// **Platform figures.** `Linear`, `Hold`, and the power laws at exponents 0.5
/// and 1–5 evaluate with IEEE-exact operations (multiplication, `sqrt`), so
/// they give the same bits on every target. Other exponents (`powf`) and
/// [`Exp`](Self::Exp) (`exp_m1`) go through the platform's libm and may differ
/// in the last ulp between targets; within one target they are deterministic,
/// so a parallel render still equals the serial one.
///
/// ```
/// use tutti_curve::Law;
/// assert_eq!(Law::Linear.eval(0.25), 0.25);
/// assert_eq!(Law::PowerIn(2.0).eval(0.5), 0.25);
/// assert_eq!(Law::PowerOut(2.0).eval(0.5), 0.75);
/// assert_eq!(Law::Hold.eval(0.99), 0.0);
/// assert_eq!(Law::PowerIn(f32::NAN).eval(0.5), 0.5); // as linear
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum Law {
    /// `t`. The default.
    #[default]
    Linear,
    /// `0` until the end, then `1`: the start value held, and a step onto the
    /// end value on the piece's last frame.
    Hold,
    /// `t^p`, `p > 0`: an ease-in. `p > 1` starts slow (`2` is a quadratic),
    /// `p < 1` starts fast (`0.5` is a square root).
    PowerIn(f32),
    /// `1 - (1 - t)^p`, `p > 0`: the ease-out mirror of
    /// [`PowerIn`](Self::PowerIn).
    PowerOut(f32),
    /// [`PowerIn`](Self::PowerIn) over the first half and
    /// [`PowerOut`](Self::PowerOut) over the second, `p > 0`: an S-curve (`2`
    /// is the quadratic S-curve).
    PowerInOut(f32),
    /// `(e^(k·t) - 1) / (e^k - 1)`: a true exponential with curvature `k`
    /// (`|k|` past 64 is clamped). `k > 0` starts slow, `k < 0` starts fast;
    /// `k` near 0 is linear. `k = ln(to / from)` makes the value geometric (a
    /// frequency sweep linear in pitch).
    Exp(f32),
}

/// The largest `|k|` [`Law::Exp`] evaluates: `e^64` is far inside `f32`, so the
/// ratio never overflows to `inf / inf`.
const EXP_CURVATURE_MAX: f32 = 64.0;

impl Law {
    /// `g(t)`, `t` clamped to `[0, 1]` (a NaN `t` is 0). Allocation-free: what
    /// a param port calls once per frame.
    #[inline]
    #[must_use]
    pub fn eval(self, t: f32) -> f32 {
        let t = if t >= 0.0 { t.min(1.0) } else { 0.0 };
        match self {
            Self::Linear => t,
            Self::Hold => {
                if t < 1.0 {
                    0.0
                } else {
                    1.0
                }
            }
            Self::PowerIn(p) if valid_exponent(p) => pow(t, p),
            Self::PowerOut(p) if valid_exponent(p) => 1.0 - pow(1.0 - t, p),
            Self::PowerInOut(p) if valid_exponent(p) => {
                // `2^(p-1) · t^p` is `(2t)^p / 2`, which keeps the integer
                // exponents on multiplications.
                if t < 0.5 {
                    pow(2.0 * t, p) / 2.0
                } else {
                    1.0 - pow(2.0 - 2.0 * t, p) / 2.0
                }
            }
            Self::Exp(k) if k.is_finite() => {
                let k = k.clamp(-EXP_CURVATURE_MAX, EXP_CURVATURE_MAX);
                if k.abs() < 1e-4 {
                    t
                } else {
                    (k * t).exp_m1() / k.exp_m1()
                }
            }
            // An invalid parameter: linear (see the type docs).
            Self::PowerIn(_) | Self::PowerOut(_) | Self::PowerInOut(_) | Self::Exp(_) => t,
        }
    }
}

fn valid_exponent(p: f32) -> bool {
    p.is_finite() && p > 0.0
}

/// `x^p` for `x` in `[0, 1]`: the integer exponents up to 5 and the square
/// root by IEEE-exact operations, anything else through `powf`.
#[inline]
fn pow(x: f32, p: f32) -> f32 {
    if p == 1.0 {
        x
    } else if p == 2.0 {
        x * x
    } else if p == 3.0 {
        x * x * x
    } else if p == 4.0 {
        let x2 = x * x;
        x2 * x2
    } else if p == 5.0 {
        let x2 = x * x;
        x2 * x2 * x
    } else if p == 0.5 {
        x.sqrt()
    } else {
        x.powf(p)
    }
}

#[cfg(test)]
mod tests;
