//! [`CurveModulator`] — the modulators that are *also* pure functions of beat.
//!
//! # Why this is a second trait and not a flag
//!
//! [`Modulator`](crate::Modulator) is `(state, phase) -> (state, value)`:
//! state-threaded, sampled by a driver that owns the clock. [`Curve`] is
//! `&self, beat -> Option<value>`: pure, evaluated by whoever holds it, at
//! whatever rate they read. Those are not two views of one thing.
//!
//! So a "deliver this route as a curve" flag would be a lie for any modulator
//! whose value depends on more than its position: a `Curve` has nowhere to put
//! threaded state, so something else would have to be substituted silently.
//! This trait makes the property declarable instead — implement it only where a
//! value at a position really is computable from that position.
//!
//! # What that does *not* mean
//!
//! Not "stateless only". [`BeatLfo`]'s stepped shapes look stateful on the
//! scalar path — an xorshift advanced once per phase wrap — yet they implement
//! this trait, because the same randomness re-keyed on the **cycle index** is
//! addressable: index `n` is computable without having produced `n-1`.
//!
//! That reformulation is strictly better than the sequence it replaces. A
//! stepper that only ever advances cannot be evaluated at an arbitrary beat and
//! does not replay a bar identically after a transport seek; a hash of the cycle
//! index does both. It is a different sequence of numbers — the same shape, not
//! the same samples — and that is the honest cost.
//!
//! The line this trait draws is therefore *position-derivable vs not*, not
//! *stateless vs stateful*. What genuinely cannot cross it is a modulator whose
//! history the beat cannot reconstruct — an envelope follower tracking live
//! audio, say.
//!
//! # Why the offset is shaped here
//!
//! Both paths must produce the *same value* — a route that switches delivery
//! must not change what the listener hears. The scalar path computes
//! `shape(raw, depth, polarity, curve) * (max - min)` in
//! [`ModPreFrame::run`](crate::ModPreFrame::run). [`ShapedCurve`] applies the
//! identical expression to the same [`shape`] function, so the two agree by
//! construction rather than by two implementations staying in sync.
//!
//! It is an **offset** curve, swinging around zero: the owning
//! [`LayeredCurve`](crate::LayeredCurve) adds the base and applies the clamp.
//! Returning an absolute value here double-counts the base, which reads as a
//! param sitting at twice its authored value rather than as an error anywhere.

use std::sync::Arc;

use tutti_types::{Beat, BeatDuration, Depth, Phase, PhaseIncrement};

use crate::curve::Curve;
use crate::shape::{shape, Polarity};
use crate::{CurveType, Modulator};

/// A modulator that is additionally a pure function of musical position.
///
/// Implement it only when the modulator's value at a beat depends on *nothing
/// but that beat*. A stateful modulator must not implement it — see the module
/// doc; the whole point of the split is that the compiler can refuse those.
///
/// [`beats_per_cycle`](Self::beats_per_cycle) is how the implementor maps beat
/// to phase. It is on the trait rather than derived from a rate because a curve
/// has no driver threading time for it: the beat *is* its clock.
pub trait CurveModulator: Modulator + Send + Sync + 'static {
    /// Beats per full cycle — a span, so a larger value is *slower*. A
    /// non-positive span freezes the curve at its phase offset, matching the
    /// scalar path via [`Beat::cycles_of`].
    fn beats_per_cycle(&self) -> BeatDuration;

    /// The raw `[-1, 1]` value at `cycles` — the same waveform the scalar path
    /// samples, with no depth, polarity or range applied.
    ///
    /// `cycles` is the **un-wrapped** position: `beat / beats_per_cycle`. Its
    /// fractional part is the phase, and its integer part is the cycle index.
    /// A wrapped [`Phase`] would be enough for a periodic shape but throws away
    /// exactly what a per-cycle value needs to be addressable — which cycle it
    /// is. Handing over the whole position is what lets a stepped shape be a
    /// pure function of beat at all.
    ///
    /// The default wraps to a [`Phase`] and samples [`Modulator::value`] with a
    /// throwaway state, which is correct only when the modulator's value
    /// depends on nothing but its phase. A modulator that threads real state
    /// must override this with a position-derived formulation — see
    /// [`BeatLfo`] for the two random shapes.
    fn raw_at(&self, cycles: f32) -> f32 {
        let seed = <Self as Modulator>::State::default();
        self.value(seed, Phase::wrapped(cycles)).1
    }
}

/// Map an integer index to a stable pseudo-random value in `[-1, 1]`
/// (splitmix64 finalizer).
///
/// Addressable rather than sequential: the value for index `n` is computable
/// without having produced `n-1`. That is the property that lets a stepped
/// random shape be a [`Curve`] — and it is *stronger* than the threaded
/// xorshift the scalar path uses, which only ever advances and so cannot replay
/// a bar identically after a transport seek.
#[inline]
pub fn hash_bipolar(n: i64) -> f32 {
    let mut z = (n as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^= z >> 31;
    // Top 24 bits → [0, 1) → [-1, 1].
    ((z >> 40) as f32 / (1u32 << 24) as f32) * 2.0 - 1.0
}

/// How an edge shapes a raw modulator value into an offset.
///
/// The scalar path carries these on [`ModEdge`](crate::ModEdge); a curve is
/// built once and evaluated later, so it has to hold them itself. Same fields,
/// same meaning — deliberately, so [`ShapedCurve`] can apply the identical
/// expression the driver does.
#[derive(Debug, Clone, Copy)]
pub struct EdgeShape {
    /// Bipolar routing depth. Negative inverts the source rather than
    /// attenuating it.
    pub depth: Depth,
    /// Whether the offset swings both ways around zero or only one.
    pub polarity: Polarity,
    /// Response curve applied to the shaped magnitude.
    pub curve: CurveType,
    /// A displacement applied after phase generation. Meaningfully negative,
    /// which is why it is a [`PhaseIncrement`] and not a [`Phase`].
    pub phase_offset: PhaseIncrement,
    /// Lower bound of the target param's range. The offset scales by the
    /// range's width, matching the control-rate contract
    /// `raw * depth * (max - min)`.
    pub min: f32,
    /// Upper bound of the target param's range. See [`min`](Self::min).
    pub max: f32,
}

impl EdgeShape {
    /// A full-depth bipolar linear edge over `[min, max]`.
    pub fn new(min: f32, max: f32) -> Self {
        Self {
            depth: Depth::FULL,
            polarity: Polarity::Bipolar,
            curve: CurveType::Linear,
            phase_offset: PhaseIncrement(0.0),
            min,
            max,
        }
    }

    /// Set the routing [`Depth`]. Negative inverts the source.
    pub fn with_depth(mut self, depth: impl Into<Depth>) -> Self {
        self.depth = depth.into();
        self
    }

    /// Set the [`Polarity`] — whether the offset swings both ways around zero
    /// or pushes one direction only.
    pub fn with_polarity(mut self, polarity: Polarity) -> Self {
        self.polarity = polarity;
        self
    }

    /// Set the response [`CurveType`] applied to the shaped magnitude.
    pub fn with_curve(mut self, curve: CurveType) -> Self {
        self.curve = curve;
        self
    }

    /// Set the phase displacement applied after phase generation. A
    /// [`PhaseIncrement`], so it may be negative.
    pub fn with_phase_offset(mut self, offset: impl Into<PhaseIncrement>) -> Self {
        self.phase_offset = offset.into();
        self
    }

    /// The offset a raw `[-1, 1]` value becomes under this edge.
    ///
    /// The one expression both delivery paths use. `ModPreFrame::run` computes
    /// it inline for the scalar case; keeping the curve case on this method is
    /// what makes "same route, either delivery, same value" true by
    /// construction.
    #[inline]
    pub fn offset_of(&self, raw: f32) -> f32 {
        shape(raw, self.depth, self.polarity, self.curve) * (self.max - self.min)
    }
}

/// A [`CurveModulator`] plus an [`EdgeShape`], as an offset [`Curve`].
///
/// This is what a route installs when it wants sub-block delivery: the sink
/// evaluates it at each block beat instead of receiving one collapsed scalar per
/// frame.
pub struct ShapedCurve<M: CurveModulator> {
    modulator: M,
    edge: EdgeShape,
}

impl<M: CurveModulator> ShapedCurve<M> {
    /// Pair a [`CurveModulator`] with the [`EdgeShape`] that turns its raw
    /// `[-1, 1]` output into an offset in the target's units.
    pub fn new(modulator: M, edge: EdgeShape) -> Self {
        Self { modulator, edge }
    }

    /// Erase to the `Arc<dyn Curve>` a layered sink installs.
    pub fn erased(modulator: M, edge: EdgeShape) -> Arc<dyn Curve> {
        Arc::new(Self::new(modulator, edge))
    }
}

impl<M: CurveModulator> Curve for ShapedCurve<M> {
    /// The offset at `beat` — never an absolute value. See the module doc.
    fn value_at(&self, beat: Beat) -> Option<f32> {
        // The offset is added here rather than inside the modulator because it
        // displaces the *position*, which is what shifts a per-cycle boundary
        // too. A frozen span still contributes: its phase-offset value, held.
        let cycles = beat
            .cycles_of(self.modulator.beats_per_cycle())
            .unwrap_or(0.0) as f32
            + self.edge.phase_offset.get();
        Some(self.edge.offset_of(self.modulator.raw_at(cycles)))
    }
}

/// An [`Lfo`](crate::Lfo) clocked by the beat — the built-in [`CurveModulator`].
///
/// **Every** LFO shape is curve-deliverable, including the stepped ones. The
/// scalar path drives `Random`/`RandomSmooth` from a threaded xorshift, which
/// only advances — so it cannot be evaluated at an arbitrary beat, and it does
/// not replay a bar identically after a seek. Keying the same randomness on the
/// *cycle index* instead (a splitmix64 hash of the index) removes both
/// limitations at once: the value becomes addressable, and therefore
/// reproducible.
///
/// The two formulations are different sequences of numbers — the same shape,
/// not the same samples. That is the honest cost, and it buys a property the
/// scalar path never had.
pub struct BeatLfo {
    lfo: crate::Lfo,
    beats_per_cycle: BeatDuration,
}

impl BeatLfo {
    /// A beat-clocked LFO of any shape, one cycle per `beats_per_cycle`.
    pub fn new(shape: crate::LfoShape, beats_per_cycle: impl Into<BeatDuration>) -> Self {
        Self {
            lfo: crate::Lfo::new(shape),
            beats_per_cycle: beats_per_cycle.into(),
        }
    }
}

impl Modulator for BeatLfo {
    type State = <crate::Lfo as Modulator>::State;
    fn value(&self, state: Self::State, phase: Phase) -> (Self::State, f32) {
        self.lfo.value(state, phase)
    }
}

impl CurveModulator for BeatLfo {
    fn beats_per_cycle(&self) -> BeatDuration {
        self.beats_per_cycle
    }

    /// Position-derived for every shape — periodic ones from the phase, stepped
    /// ones from the cycle index.
    fn raw_at(&self, cycles: f32) -> f32 {
        // `floor`, not `as i64`: truncation rounds toward zero, so cycles -0.5
        // and +0.5 would collide on index 0 and a source stepped before the
        // origin would repeat its neighbour's value.
        let index = cycles.floor();
        match self.lfo.shape {
            // One value per cycle, held across it.
            crate::LfoShape::Random => hash_bipolar(index as i64),
            // The same steps, interpolated across the phase — the ramp this
            // shape is named for. Hashing both ends reconstructs the
            // `previous`/`current` pair the scalar path threads, without
            // needing the history that produced it.
            crate::LfoShape::RandomSmooth => {
                let prev = hash_bipolar(index as i64 - 1);
                let cur = hash_bipolar(index as i64);
                prev + (cur - prev) * (cycles - index)
            }
            // Purely phase-determined: the default is already exact.
            _ => {
                let seed = <crate::Lfo as Modulator>::State::default();
                self.lfo.value(seed, Phase::wrapped(cycles)).1
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{LayerKey, LfoShape};

    fn sine(beats_per_cycle: f64) -> BeatLfo {
        BeatLfo::new(LfoShape::Sine, BeatDuration(beats_per_cycle))
    }

    /// The scalar path (`SourceRate`/`ModPreFrame`) and the curve path
    /// (`BeatLfo::beats_per_cycle`) must read the SAME rate off the same
    /// number — one param driven by both must not run at two speeds.
    ///
    /// Pinned because the two units are reciprocals that coincide at `1.0`: a
    /// `1.0 / frequency` slipped into either adapter agrees with every example
    /// written at `1.0` and disagrees at `2.0`.
    ///
    /// Two details are load-bearing. It drives the real `Sourced::tick_phase`
    /// rather than re-deriving the phase inline — hand-copying the driver's
    /// arithmetic compares a copy against a copy and cannot catch the paths
    /// diverging, which is the whole job. And the non-power-of-two spans are
    /// deliberate: `[0.5, 1, 2, 4]` are all exact in f32, so they agree even
    /// while both paths narrow.
    #[test]
    fn curve_and_scalar_paths_read_the_rate_identically() {
        use crate::{ErasedModulator, SourceRate, Sourced};
        use tutti_types::Seconds;

        for beats_per_cycle in [0.5f64, 1.0, 2.0, 4.0, 3.0, 0.75, 1.0 / 3.0] {
            for beat in [0.0f64, 0.25, 0.5, 1.0, 2.0, 3.5, 97.25] {
                // Scalar path: the driver's own source, sampled at this beat.
                // `dt` is irrelevant to a synced clock — it re-derives.
                let mut sourced = Sourced::new(
                    crate::Lfo::new(LfoShape::Sine),
                    SourceRate::beat_synced(BeatDuration(beats_per_cycle), 0.0),
                );
                let scalar = sourced.sample(Beat(beat), Seconds(0.0));

                // Curve path: the real `ShapedCurve` beat → cycles mapping. The
                // edge spans a unit range (`-0.5..0.5`) so its depth scaling is
                // 1x and only the RATE is under test, not the depth.
                let curve = ShapedCurve::new(sine(beats_per_cycle), EdgeShape::new(-0.5, 0.5));
                let curved = curve.value_at(Beat(beat)).unwrap();

                assert!(
                    (scalar - curved).abs() < 1e-4,
                    "span {beats_per_cycle} beat {beat}: scalar {scalar} vs curve {curved}"
                );
            }
        }
    }

    /// A larger span is a *slower* source. Read as `Hz`, doubling it would
    /// double the rate instead — the reciprocal this field's unit exists to
    /// rule out.
    #[test]
    fn a_longer_span_completes_fewer_cycles() {
        let one_beat = ShapedCurve::new(sine(1.0), EdgeShape::new(-0.5, 0.5));
        let two_beat = ShapedCurve::new(sine(2.0), EdgeShape::new(-0.5, 0.5));

        // The 2-beat span reaches at beat 2 whatever the 1-beat span reached at
        // beat 1 — it is half as fast, so it takes twice as long. Sampled at the
        // quarter-cycle peak, where a sine is unambiguous (at the half-cycle it
        // is zero, the same value it holds at a full cycle).
        let peak = one_beat.value_at(Beat(0.25)).unwrap();
        assert!(peak > 0.4, "quarter of a cycle should be near the peak");
        assert!((two_beat.value_at(Beat(0.5)).unwrap() - peak).abs() < 1e-6);

        // And at the same beat the slower one is strictly behind.
        assert!(two_beat.value_at(Beat(0.25)).unwrap() < peak);
    }

    /// A non-positive span freezes rather than dividing by zero or running
    /// backwards. One guard, in `Beat::cycles_of` — every path routes through
    /// it, so the negative case cannot be answered two ways.
    #[test]
    fn a_non_positive_span_freezes_at_the_phase_offset() {
        for span in [0.0f64, -2.0] {
            let curve = ShapedCurve::new(sine(span), EdgeShape::new(-0.5, 0.5));
            let held = curve.value_at(Beat(0.0)).unwrap();
            for beat in [1.0f64, 7.5, 100.0] {
                assert_eq!(
                    curve.value_at(Beat(beat)).unwrap(),
                    held,
                    "span {span} should hold across beats"
                );
            }
        }
    }

    /// `Random` holds one value per cycle and jumps at the boundary — a
    /// beat-synced sample & hold, not a ramp.
    #[test]
    fn random_holds_within_a_cycle_and_steps_between() {
        let lfo = BeatLfo::new(LfoShape::Random, 1.0);

        let within: Vec<f32> = [0.1, 0.4, 0.9].iter().map(|c| lfo.raw_at(*c)).collect();
        assert!(
            within.windows(2).all(|w| (w[0] - w[1]).abs() < 1e-6),
            "a held value must not move inside its cycle: {within:?}"
        );

        let steps: Vec<f32> = (0..6).map(|c| lfo.raw_at(c as f32 + 0.5)).collect();
        let distinct = steps
            .iter()
            .filter(|v| (*v - steps[0]).abs() > 1e-6)
            .count();
        assert!(
            distinct >= 4,
            "cycles should differ from each other: {steps:?}"
        );
        assert!(
            steps.iter().all(|v| (-1.0..=1.0).contains(v)),
            "values stay in [-1, 1]: {steps:?}"
        );
    }

    /// `RandomSmooth` ramps between the same steps. The failure this guards is
    /// one value per cycle under a name that promises interpolation — which is
    /// `Random`'s behaviour wearing the wrong label, and defeats the point of
    /// sub-block delivery, which exists to be smooth.
    #[test]
    fn random_smooth_ramps_instead_of_stepping() {
        let smooth = BeatLfo::new(LfoShape::RandomSmooth, 1.0);

        let across: Vec<f32> = (0..8)
            .map(|i| smooth.raw_at(3.0 + i as f32 / 8.0))
            .collect();
        let moved = across
            .iter()
            .filter(|v| (*v - across[0]).abs() > 1e-6)
            .count();
        assert!(
            moved >= 6,
            "a smooth shape must move within a cycle, not hold: {across:?}"
        );

        // It is a *linear* ramp between the cycle's endpoints: the midpoint of
        // the cycle is the mean of its ends.
        let start = smooth.raw_at(3.0);
        let end = smooth.raw_at(4.0);
        let mid = smooth.raw_at(3.5);
        assert!(
            (mid - (start + end) / 2.0).abs() < 1e-5,
            "midpoint {mid} should be the mean of {start} and {end}"
        );

        // And it is continuous across the boundary — the defining difference
        // from `Random`, which jumps there.
        let before = smooth.raw_at(4.0 - 1e-4);
        assert!(
            (before - end).abs() < 1e-3,
            "no jump at the cycle boundary: {before} vs {end}"
        );
    }

    /// The property the scalar path cannot offer: the same beat always gives the
    /// same value, so a bar replays identically after a transport seek. The
    /// threaded xorshift only ever advances, so it cannot.
    #[test]
    fn a_stepped_curve_replays_identically_after_a_seek() {
        for shape in [LfoShape::Random, LfoShape::RandomSmooth] {
            let lfo = BeatLfo::new(shape, 1.0);
            let first: Vec<f32> = (0..16).map(|i| lfo.raw_at(i as f32 / 4.0)).collect();
            // Wander far away, then come back — as a seek would.
            for i in 0..32 {
                let _ = lfo.raw_at(100.0 + i as f32);
            }
            let replay: Vec<f32> = (0..16).map(|i| lfo.raw_at(i as f32 / 4.0)).collect();
            assert_eq!(first, replay, "{shape:?} must be reproducible at a beat");
        }
    }

    /// Negative positions must not collide with positive ones. `as i64`
    /// truncates toward zero, so -0.5 and +0.5 would share index 0 and a source
    /// stepped before the origin would repeat its neighbour.
    #[test]
    fn cycles_before_the_origin_get_their_own_values() {
        let lfo = BeatLfo::new(LfoShape::Random, 1.0);
        assert!(
            (lfo.raw_at(-0.5) - lfo.raw_at(0.5)).abs() > 1e-6,
            "cycle -1 and cycle 0 must not share a value"
        );
        // Still held within the negative cycle.
        assert!((lfo.raw_at(-0.9) - lfo.raw_at(-0.1)).abs() < 1e-6);
    }

    /// The load-bearing property: a curve layer and the scalar the driver would
    /// have produced are the *same number*. If these ever diverge, switching a
    /// route's delivery changes what the user hears.
    #[test]
    fn a_curve_offset_matches_the_scalar_path_exactly() {
        let edge = EdgeShape::new(0.0, 10.0).with_depth(Depth(0.7));
        let m = sine(1.0);
        let curve = ShapedCurve::new(sine(1.0), edge);

        for i in 0..16 {
            let beat = Beat(i as f64 / 16.0);
            // The scalar path, spelled exactly as `ModPreFrame::run` does.
            let raw = m.raw_at(beat.get() as f32);
            let scalar = shape(raw, edge.depth, edge.polarity, edge.curve) * (edge.max - edge.min);

            let from_curve = curve.value_at(beat).expect("a curve always contributes");
            assert!(
                (scalar - from_curve).abs() < 1e-6,
                "beat {beat:?}: scalar {scalar} vs curve {from_curve}"
            );
        }
    }

    /// An offset swings around zero — the sink adds the base. Returning an
    /// absolute value here would double-count it.
    #[test]
    fn the_curve_is_an_offset_not_an_absolute_value() {
        let curve = ShapedCurve::new(sine(1.0), EdgeShape::new(100.0, 200.0));
        let mut saw_negative = false;
        let mut saw_positive = false;
        for i in 0..16 {
            let v = curve.value_at(Beat(i as f64 / 16.0)).unwrap();
            saw_negative |= v < -1e-3;
            saw_positive |= v > 1e-3;
            assert!(
                v.abs() <= 100.0 + 1e-3,
                "an offset must stay within the range width, got {v}"
            );
        }
        assert!(
            saw_negative && saw_positive,
            "a bipolar offset must swing both ways around zero"
        );
    }

    /// The payoff, through a real accumulator: a curve layer moves *between*
    /// frames, a scalar layer holds until the driver next writes it.
    ///
    /// This is what sub-block delivery buys, and the reason the two paths are
    /// worth distinguishing at all. Both are read through the same
    /// [`LayeredCurve`] with the same base and range — only the layer kind
    /// differs.
    #[test]
    fn a_curve_layer_traces_between_frames_where_a_scalar_holds() {
        use crate::LayeredCurve;

        let edge = EdgeShape::new(0.0, 10.0);
        let curve = ShapedCurve::erased(sine(1.0), edge);

        let mut layered: LayeredCurve<f32> = LayeredCurve::new(5.0, 0.0, 10.0);
        layered.set_layer(LayerKey(1), curve);

        // Sample within one frame's worth of beats, as a per-block reader would.
        let traced: Vec<f32> = (0..8)
            .map(|i| layered.value_at(Beat(i as f64 / 8.0)).unwrap())
            .collect();
        let distinct = traced
            .iter()
            .filter(|v| (*v - traced[0]).abs() > 1e-4)
            .count();
        assert!(
            distinct >= 6,
            "a curve layer should vary across the block, got {traced:?}"
        );

        // The same accumulator with a scalar layer: one value, whatever beat.
        let mut scalar: LayeredCurve<f32> = LayeredCurve::new(5.0, 0.0, 10.0);
        scalar.set_scalar_layer(LayerKey(1), 2.0);
        for i in 0..8 {
            let v = scalar.value_at(Beat(i as f64 / 8.0)).unwrap();
            assert!(
                (v - 7.0).abs() < 1e-6,
                "a scalar layer is beat-independent, got {v} at beat {i}/8"
            );
        }
    }

    /// `beats_per_cycle <= 0` freezes rather than dividing by zero.
    #[test]
    fn a_frozen_curve_holds_one_value() {
        let curve = ShapedCurve::new(sine(0.0), EdgeShape::new(0.0, 1.0));
        let first = curve.value_at(Beat(0.0)).unwrap();
        for i in 0..8 {
            let v = curve.value_at(Beat(i as f64 * 3.7)).unwrap();
            assert!(
                (v - first).abs() < 1e-6 && v.is_finite(),
                "a frozen curve must hold {first}, got {v}"
            );
        }
    }

    /// Depth scales the offset, and the range width sets its scale — the two
    /// factors the scalar contract multiplies.
    #[test]
    fn depth_and_span_scale_the_offset() {
        let peak = |edge: EdgeShape| {
            (0..32)
                .map(|i| {
                    ShapedCurve::new(sine(1.0), edge)
                        .value_at(Beat(i as f64 / 32.0))
                        .unwrap()
                })
                .fold(0.0_f32, |a, b| a.max(b.abs()))
        };

        let full = peak(EdgeShape::new(0.0, 10.0));
        let half = peak(EdgeShape::new(0.0, 10.0).with_depth(Depth(0.5)));
        assert!(
            (full / 2.0 - half).abs() < 1e-3,
            "half depth should halve the swing: {full} vs {half}"
        );

        let wide = peak(EdgeShape::new(0.0, 20.0));
        assert!(
            (wide / 2.0 - full).abs() < 1e-3,
            "double the span should double the swing: {wide} vs {full}"
        );
    }
}
