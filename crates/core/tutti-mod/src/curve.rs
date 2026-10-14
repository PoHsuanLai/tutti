//! [`Curve`] — a value as a pure function of a musical position.
//!
//! The rate-agnostic `beat -> value` interface shared by automation and
//! modulation: an automation envelope, an LFO, a constant, and the summing
//! [`crate::LayeredCurve`] are all `Curve`s. It lives here rather than in
//! `tutti-nodes` because modulation depends on it and `tutti-nodes` depends on
//! this crate. The one foreign impl,
//! [`AutomationEnvelope`], is orphan-legal here since `audio_automation` is a
//! direct dependency.

use audio_automation::{AutomationEnvelope, CurveType};
use tutti_types::Beat;

/// The piece of a [`Curve`] a beat falls in, between two breakpoints: what a
/// producer of curve-segment events ([`Curve::segment_at`]) sends once per
/// piece instead of sampling [`Curve::value_at`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum CurveSegment {
    /// The value is `value` from here until `until` (`None`: for good) —
    /// before a curve's first point, after its last, a single point.
    Hold {
        /// The value held.
        value: f32,
        /// Where the curve next moves, if it does.
        until: Option<Beat>,
    },
    /// The value moves from `from` at `start` to `to` at `end` along
    /// `shape`, as [`CurveType::interpolate`] reads it: at beat `b` the
    /// value is `shape.interpolate(from, to, (b - start) / (end - start))`.
    /// `start < end`.
    Move {
        /// Where the move starts.
        start: Beat,
        /// Where it ends (the next piece starts there).
        end: Beat,
        /// The value at `start`.
        from: f32,
        /// The value at `end`.
        to: f32,
        /// How it gets there.
        shape: CurveType,
    },
}

/// A value as a pure function of a musical position (`beat -> value`).
///
/// An automation envelope (`audio_automation::AutomationEnvelope`), a shaped
/// LFO ([`ShapedCurve`](crate::ShapedCurve)), a constant, and the summing
/// [`LayeredCurve`](crate::LayeredCurve) are all curves. The stored form — breakpoints, a constant, an LFO shape, an expression — is
/// the implementor's business. Callers supply an already-resolved, already
/// loop-wrapped [`Beat`]; the curve holds no clock and consults no loop range.
/// That keeps it evaluable from a live transport, an offline render, or a
/// per-sample port signal alike — the curve is the `a` in `y = a(w(b(s)))`,
/// composed with the clock at the call site rather than owning one.
///
/// Returns `None` where the curve has no value (disabled / empty), so callers
/// keep the empty-vs-zero distinction the playback consumers rely on: an
/// automation lane substitutes `0.0`, a plugin parameter source leaves the
/// plugin at its last value.
pub trait Curve: Send + Sync {
    /// Evaluates the curve at `beat`, or returns `None` if it has no value
    /// there.
    fn value_at(&self, beat: Beat) -> Option<f32>;

    /// Returns a copy of this curve that **no later write reaches**, for a
    /// render that runs beside the live graph (a graph fork: an offline
    /// export).
    ///
    /// `None`, the default, says the curve is immutable once shared — an
    /// envelope, an LFO shape, anything whose `value_at` reads only what it was
    /// built with — so sharing the `Arc` already is the copy. A curve that reads
    /// state something writes after construction (a modulation target a router
    /// writes every frame) returns `Some` of a frozen copy, and one that wraps
    /// another curve forwards the question to it.
    fn frozen(&self) -> Option<std::sync::Arc<dyn Curve>> {
        None
    }

    /// The piece of the curve `beat` falls in, for a producer that sends a
    /// segment per piece rather than sampling every frame (an automation
    /// lane's event output). It must agree with [`value_at`](Self::value_at)
    /// over the piece: `Hold` is `value_at` for every beat until `until`,
    /// `Move` is it for every beat in `[start, end)`.
    ///
    /// `None`, the default, says the curve cannot say (an LFO, a sum of
    /// layers, a curve with no value at `beat`): the producer samples
    /// `value_at` instead. Evaluated on the audio thread, so it must not
    /// allocate.
    fn segment_at(&self, beat: Beat) -> Option<CurveSegment> {
        let _ = beat;
        None
    }
}

/// `T` is the envelope's target *label* — never the evaluated value, which is
/// always `f32` — so the impl ignores it beyond the thread bounds a `Curve`
/// trait object needs.
impl<T: Send + Sync> Curve for AutomationEnvelope<T> {
    fn value_at(&self, beat: Beat) -> Option<f32> {
        self.get_value_at(beat.get())
    }

    /// The envelope's breakpoints, each point's `curve` the shape of the
    /// piece it starts. `None` where it has no value (disabled, empty), and
    /// for a piece its constraints would bend: a `step_size` quantises every
    /// value, and a `min`/`max` that cuts into a piece's endpoints clamps
    /// its middle, neither of which a shape carries. (A clamp the endpoints
    /// stay inside never bites: an `audio-automation` shape that overshoots
    /// its endpoints has no segment form anyway.)
    fn segment_at(&self, beat: Beat) -> Option<CurveSegment> {
        let points = &self.points;
        if !self.enabled || points.is_empty() || self.step_size.is_some() {
            return None;
        }
        let inside =
            |v: f32| self.min_value.is_none_or(|m| v >= m) && self.max_value.is_none_or(|m| v <= m);
        let b = beat.get();
        // The first point after `beat`: the piece is the one before it.
        let next = points.partition_point(|p| p.time <= b);
        // Before the first point or from the last on: that point's value,
        // clamped as `value_at` clamps it.
        let hold = |until: Option<f64>| {
            self.value_at(beat).map(|value| CurveSegment::Hold {
                value,
                until: until.map(Beat),
            })
        };
        if next == 0 {
            return hold(Some(points[0].time));
        }
        if next == points.len() {
            return hold(None);
        }
        let (p, q) = (&points[next - 1], &points[next]);
        if !(inside(p.value) && inside(q.value)) || p.time >= q.time {
            return None;
        }
        Some(CurveSegment::Move {
            start: Beat(p.time),
            end: Beat(q.time),
            from: p.value,
            to: q.value,
            shape: p.curve,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use audio_automation::AutomationPoint;

    fn envelope() -> AutomationEnvelope<()> {
        let mut e = AutomationEnvelope::new(());
        e.add_point(AutomationPoint::with_curve(
            1.0,
            0.2,
            CurveType::Exponential,
        ));
        e.add_point(AutomationPoint::with_curve(2.0, 0.8, CurveType::Linear));
        e.add_point(AutomationPoint::new(3.0, 0.4));
        e
    }

    /// An envelope's pieces are its breakpoints: a hold before the first
    /// and from the last, a move between, each agreeing with `value_at`
    /// over its span; a breakpoint starts the piece after it.
    ///
    /// Mutation (run): `partition_point(|p| p.time < b)` (a breakpoint ends
    /// the piece before it) → beat 2.0 reads the exponential piece → fails.
    #[test]
    fn an_envelopes_pieces_are_its_breakpoints() {
        let e = envelope();
        assert_eq!(
            e.segment_at(Beat(0.5)),
            Some(CurveSegment::Hold {
                value: 0.2,
                until: Some(Beat(1.0))
            })
        );
        let exp = CurveSegment::Move {
            start: Beat(1.0),
            end: Beat(2.0),
            from: 0.2,
            to: 0.8,
            shape: CurveType::Exponential,
        };
        assert_eq!(e.segment_at(Beat(1.0)), Some(exp));
        assert_eq!(e.segment_at(Beat(1.7)), Some(exp));
        assert!(matches!(
            e.segment_at(Beat(2.0)),
            Some(CurveSegment::Move {
                shape: CurveType::Linear,
                ..
            })
        ));
        assert_eq!(
            e.segment_at(Beat(9.0)),
            Some(CurveSegment::Hold {
                value: 0.4,
                until: None
            })
        );
    }

    /// A piece the envelope's constraints would bend is no segment: a step
    /// size, or a clamp cutting into its endpoints. A clamp both endpoints
    /// stay inside leaves it one; a disabled envelope has none.
    ///
    /// Mutation (run): drop the endpoint check (`inside`) → the clamped
    /// piece is a segment → fails.
    #[test]
    fn a_bent_piece_is_no_segment() {
        assert!(envelope().with_step(0.1).segment_at(Beat(1.5)).is_none());
        assert!(envelope().with_max(0.5).segment_at(Beat(1.5)).is_none());
        assert!(envelope()
            .with_range(0.0, 1.0)
            .segment_at(Beat(1.5))
            .is_some());
        let mut off = envelope();
        off.enabled = false;
        assert!(off.segment_at(Beat(1.5)).is_none());
    }
}
