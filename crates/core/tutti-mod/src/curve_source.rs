//! [`CurveModulator`] — the modulators that are *also* pure functions of beat.
//!
//! # Why this is a second trait and not a flag
//!
//! [`Modulator`](crate::Modulator) is `(state, phase) -> (state, value)`:
//! state-threaded, sampled by a driver that owns the clock. [`Curve`] is
//! `&self, beat -> Option<value>`: pure, evaluated by whoever holds it, at
//! whatever rate they read. Those are not two views of one thing.
//!
//! A sample & hold shows why. Its value depends on a stepper the caller threads
//! between samples; a `&self` curve has nowhere to put that. The plugin crate's
//! `LfoCurve` handles random shapes by hashing the cycle index instead — which
//! is a *different modulator* that happens to look similar, not the same one
//! sampled differently.
//!
//! So a "deliver this route as a curve" flag would silently substitute a
//! different source for any stateful modulator. This trait makes the property
//! declarable instead: implement it only where the modulator genuinely is a
//! pure function of beat, and a route can ask for curve delivery only from a
//! kind that has said so.
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
//! Returning an absolute value here would double-count the base — a bug the
//! plugin crate's `LfoOffset` documents having already paid for once.

use std::sync::Arc;

use tutti_types::{Beat, Depth, Phase, PhaseIncrement};

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
    /// Beats per full cycle. `<= 0` freezes the curve at its phase offset,
    /// matching how a zero-frequency source behaves on the scalar path.
    fn beats_per_cycle(&self) -> f32;

    /// The raw `[-1, 1]` value at `phase` — the same waveform the scalar path
    /// samples, with no depth, polarity or range applied.
    ///
    /// Defaulted to [`Modulator::value`] with a throwaway state, which is
    /// correct precisely because an implementor of this trait is stateless.
    /// That default is the trait's honesty check: a modulator for which it is
    /// wrong is one that should not be implementing this trait.
    fn raw_at(&self, phase: Phase) -> f32 {
        let seed = <Self as Modulator>::State::default();
        self.value(seed, phase).1
    }
}

/// How an edge shapes a raw modulator value into an offset.
///
/// The scalar path carries these on [`ModEdge`](crate::ModEdge); a curve is
/// built once and evaluated later, so it has to hold them itself. Same fields,
/// same meaning — deliberately, so [`ShapedCurve`] can apply the identical
/// expression the driver does.
#[derive(Debug, Clone, Copy)]
pub struct EdgeShape {
    pub depth: Depth,
    pub polarity: Polarity,
    pub curve: CurveType,
    /// A displacement applied after phase generation. Meaningfully negative,
    /// which is why it is a [`PhaseIncrement`] and not a [`Phase`].
    pub phase_offset: PhaseIncrement,
    /// The target param's range. The offset scales by its width, matching the
    /// control-rate contract `raw * depth * (max - min)`.
    pub min: f32,
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

    pub fn with_depth(mut self, depth: impl Into<Depth>) -> Self {
        self.depth = depth.into();
        self
    }

    pub fn with_polarity(mut self, polarity: Polarity) -> Self {
        self.polarity = polarity;
        self
    }

    pub fn with_curve(mut self, curve: CurveType) -> Self {
        self.curve = curve;
        self
    }

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
        let bpc = self.modulator.beats_per_cycle();
        let phase = if bpc > 0.0 {
            Phase::wrapped(beat.get() as f32 / bpc).offset_by(self.edge.phase_offset)
        } else {
            // A frozen source still contributes: its phase-offset value, held.
            Phase::START.offset_by(self.edge.phase_offset)
        };
        Some(self.edge.offset_of(self.modulator.raw_at(phase)))
    }
}

/// An [`Lfo`](crate::Lfo) clocked by the beat — the built-in [`CurveModulator`].
///
/// Built only through [`new`](BeatLfo::new), which **refuses random shapes**.
/// `Random` and `RandomSmooth` step a stepper the caller threads between
/// samples, and a `&self` curve has nowhere to keep it. The plugin crate's
/// `LfoCurve` substitutes a beat-index hash for those — defensible there, but it
/// is a *different modulator*, and silently swapping one in is precisely what
/// this trait split exists to prevent.
///
/// So a random-shaped source is simply not curve-deliverable, and the `Option`
/// says so at construction rather than at the point of surprise.
pub struct BeatLfo {
    lfo: crate::Lfo,
    beats_per_cycle: f32,
}

impl BeatLfo {
    /// A beat-clocked LFO, or `None` if `shape` is random — see the type doc.
    pub fn new(shape: crate::LfoShape, beats_per_cycle: f32) -> Option<Self> {
        if shape.is_random() {
            return None;
        }
        Some(Self {
            lfo: crate::Lfo::new(shape),
            beats_per_cycle,
        })
    }
}

impl Modulator for BeatLfo {
    type State = <crate::Lfo as Modulator>::State;
    fn value(&self, state: Self::State, phase: Phase) -> (Self::State, f32) {
        self.lfo.value(state, phase)
    }
}

impl CurveModulator for BeatLfo {
    fn beats_per_cycle(&self) -> f32 {
        self.beats_per_cycle
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{LayerKey, LfoShape};

    fn sine(beats_per_cycle: f32) -> BeatLfo {
        BeatLfo::new(LfoShape::Sine, beats_per_cycle).expect("sine is not random")
    }

    /// A stateful shape must not be curve-deliverable — the trait split's whole
    /// purpose. The alternative is silently substituting a different modulator
    /// and calling it a delivery choice.
    #[test]
    fn a_random_shape_refuses_to_become_a_curve() {
        assert!(BeatLfo::new(LfoShape::Random, 1.0).is_none());
        assert!(BeatLfo::new(LfoShape::RandomSmooth, 1.0).is_none());
        assert!(BeatLfo::new(LfoShape::Sine, 1.0).is_some());
        assert!(BeatLfo::new(LfoShape::Triangle, 1.0).is_some());
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
            let raw = m.raw_at(Phase::wrapped(beat.get() as f32));
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
                .map(|i| ShapedCurve::new(sine(1.0), edge).value_at(Beat(i as f64 / 32.0)).unwrap())
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
