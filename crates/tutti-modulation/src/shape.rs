//! Value vocabulary + the pure shaping math.
//!
//! [`Polarity`] and [`LfoShape`] are the small enums a route needs;
//! [`shape`] is the pure function a [`ControlProgram`](crate::ControlProgram)
//! shapes every offset with. Its response curve is `tutti-curve`'s
//! [`Law::eval`], the one shape of a [`Law`] the graph, the lanes
//! and the editor share.

use tutti_curve::Law;

use tutti_types::{Depth, Phase};

// =========================================================================
// Value vocabulary
// =========================================================================

/// How a shaped modulation signal maps around its base.
///
/// Owned here rather than re-using a host's own polarity type: this crate sits
/// below any host and cannot see one. A host with its own spelling writes the
/// boundary `From` on its side.
#[cfg_attr(feature = "bevy", derive(bevy_reflect::Reflect))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Polarity {
    /// Preserve sign, curve the magnitude: `x ∈ [-1, 1] → [-1, 1]` for a
    /// curve inside `[0, 1]` (see [`shape`]). `Back` and `Elastic` dip below 0 (to about
    /// −0.10 and −0.373), so there a small `|x|` pushes the other way from
    /// its sign.
    #[default]
    Bipolar,
    /// Remap one-directional: `x ∈ [-1, 1] → [0, 1]` for a curve inside
    /// `[0, 1]`. Under `Back` or `Elastic` the low part of the input pushes
    /// below base (to about −0.10 and −0.373 of depth).
    Unipolar,
}

/// LFO waveform. Owned here (the single source of truth); `tutti_nodes`
/// re-exports it.
///
/// The first five are pure functions of [`Phase`] and evaluate through
/// [`LfoShape::evaluate_periodic`]. The last two are *stepped* — their value
/// depends on the cycle index, so a caller must branch on
/// [`LfoShape::is_random`] first, or evaluate any shape at an unwrapped
/// position through [`Wave::raw_at`](crate::Wave::raw_at).
#[cfg_attr(feature = "bevy", derive(bevy_reflect::Reflect))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LfoShape {
    /// Sine over the cycle: `sin(phase · 2π)`.
    #[default]
    Sine,
    /// Linear rise to `+1` at quarter phase, fall through `-1` at three
    /// quarters, back to zero.
    Triangle,
    /// `+1` for the first half of the cycle, `-1` for the second. Discontinuous
    /// at the halfway point and at the wrap.
    Square,
    /// Rising ramp: `-1` at the cycle start climbing to `+1` at its end.
    Sawtooth,
    /// Falling ramp — [`Sawtooth`](Self::Sawtooth) reversed: `+1` down to `-1`.
    SawtoothDown,
    /// Stepped noise: one value per cycle, held across it, a hash of the cycle
    /// index and a seed, so it needs the unwrapped position, not just the
    /// phase ([`is_random`](Self::is_random) is true; [`Wave::raw_at`]
    /// evaluates it).
    ///
    /// [`Wave::raw_at`]: crate::Wave::raw_at
    Random,
    /// [`Random`](Self::Random)'s steps, linearly interpolated across the cycle
    /// instead of held: from the previous cycle's step to this one's.
    RandomSmooth,
}

impl LfoShape {
    /// True for shapes whose output depends on the cycle index (and a seed),
    /// not just the phase. Callers must branch on this before
    /// [`LfoShape::evaluate_periodic`], or evaluate through
    /// [`Wave::raw_at`](crate::Wave::raw_at), which takes every shape.
    #[inline]
    #[must_use]
    pub fn is_random(&self) -> bool {
        matches!(self, Self::Random | Self::RandomSmooth)
    }

    /// Evaluate a purely phase-deterministic shape at [`Phase`] → `[-1, 1]`.
    ///
    /// Callers must first check [`LfoShape::is_random`]; random shapes need the
    /// cycle index and are not handled here (they debug-assert).
    #[inline]
    pub fn evaluate_periodic(&self, phase_t: Phase) -> f32 {
        let phase = phase_t.get();
        match self {
            // The one arm with a named conversion: `to_radians` is documented
            // as being for exactly this call, and `phase * TAU` was it spelled
            // out. The arms below stay on the raw cycle position — quarters and
            // halves of a cycle are not angles, so unwrapping once at the top
            // is right for them.
            Self::Sine => phase_t.to_radians().sin(),
            Self::Triangle => {
                let p = phase * 4.0;
                if p < 1.0 {
                    p
                } else if p < 3.0 {
                    2.0 - p
                } else {
                    p - 4.0
                }
            }
            Self::Square => {
                if phase < 0.5 {
                    1.0
                } else {
                    -1.0
                }
            }
            Self::Sawtooth => phase * 2.0 - 1.0,
            Self::SawtoothDown => 1.0 - phase * 2.0,
            Self::Random | Self::RandomSmooth => {
                debug_assert!(false, "evaluate_periodic called on random shape");
                0.0
            }
        }
    }

    /// Every shape, in declaration order — for populating a picker or
    /// exhaustively exercising the roster in a test.
    pub fn all() -> &'static [Self] {
        &[
            Self::Sine,
            Self::Triangle,
            Self::Square,
            Self::Sawtooth,
            Self::SawtoothDown,
            Self::Random,
            Self::RandomSmooth,
        ]
    }

    /// Display label for a UI (`"Saw Down"`, `"Random (Smooth)"`) — not the
    /// variant's Rust name, and not stable enough to parse or persist.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Sine => "Sine",
            Self::Triangle => "Triangle",
            Self::Square => "Square",
            Self::Sawtooth => "Sawtooth",
            Self::SawtoothDown => "Saw Down",
            Self::Random => "Random",
            Self::RandomSmooth => "Random (Smooth)",
        }
    }
}

impl core::fmt::Display for LfoShape {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.name())
    }
}

// =========================================================================
// Pure shaping / folding math — ONE copy, shared by every adapter.
// =========================================================================

/// Map a raw modulation signal `x ∈ [-1, 1]` to a shaped additive offset via
/// `depth · polarity · curve`. `Bipolar` keeps the sign and curves the
/// magnitude; `Unipolar` remaps `[-1, 1] → [0, 1]` (the source only pushes one
/// direction from base) then curves. `depth` scales the result.
///
/// The curve is `curve.eval(u)` on the normalized `u`, so
/// every [`Law`] shapes exactly as the editor draws it. Every law runs from 0
/// at no input to 1 at full input, and none leaves `[0, 1]` on the way, so the
/// offset never exceeds `depth`. `Hold` is 0 until the input is exactly full.
///
/// The return is a bare `f32`, not a [`Depth`]: it is an offset in the
/// *target's* units — Hz for a cutoff, linear gain for a fader — which the
/// caller has already scaled by the target's range. Only the input amount is a
/// `Depth`.
#[inline]
pub fn shape(x: f32, depth: Depth, polarity: Polarity, curve: Law) -> f32 {
    let depth = depth.get();
    match polarity {
        // Bipolar: preserve sign, curve the magnitude, restore sign.
        Polarity::Bipolar => {
            let mag = curve.eval(x.abs());
            depth * mag * x.signum()
        }
        // Unipolar: remap [-1, 1] → [0, 1], then curve.
        Polarity::Unipolar => {
            let u = (x.clamp(-1.0, 1.0) + 1.0) * 0.5;
            depth * curve.eval(u)
        }
    }
}

#[cfg(test)]
mod tests;
