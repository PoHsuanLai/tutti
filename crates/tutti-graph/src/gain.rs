//! Edge gain (doc 013, "Edge gain: a mixer is a weighted sum"): [`EdgeGain`],
//! the spec data beside an edge, and the executor's side of a weighted lane
//! (its cell, its state across blocks, and the weighted sum kernel).
//!
//! # What a weight is
//!
//! An edge with an entry in [`GraphSpec::gains`](crate::GraphSpec::gains) is a
//! **weighted lane** of its sink's sum:
//! `acc = acc + w * a`, two roundings, never fused, in source order. A weight
//! of exactly `1.0` runs the plain add (no multiply), and an edge with no
//! entry is a plain addend. The entry is structural (adding or removing one
//! recompiles); its `base` is a control: [`Editor::set_edge_gain`] stores it
//! into a cell the executor reads once at the start of the block.
//!
//! [`Editor::set_edge_gain`]: crate::Editor::set_edge_gain
//!
//! # Per block
//!
//! Each block the executor **snapshots** every lane's cell
//! ([`SumLane::snapshot`]) before the first op, on the calling thread, so a
//! store in the middle of a block reaches no op and serial and parallel
//! cannot disagree. The sum then asks the lane for its [`Weight`]
//! ([`SumLane::weight`]):
//!
//! ```text
//! w[i] = w_prev + (w_new - w_prev) * ((i + 1) as f32 / frames as f32)
//! ```
//!
//! where `w_prev` is the weight the previous block was given (the snapshot it
//! read, not the last sample of its ramp), kept in the lane across commits
//! that keep the edge. A lane's first block starts at its value, and a
//! static weight (`w_prev == w_new`) skips the ramp. The ramp is evaluated
//! per sample from that expression: no per-lane scratch is needed, so
//! nothing allocates on the audio thread.
//!
//! The lane's state advances every block, **whether or not its addend is
//! silent** (a silent addend is skipped, its weight still moves).

use std::hash::{Hash, Hasher};
use std::sync::Arc;

use tutti_rt::PackedAtomic;
use tutti_types::Amplitude;

use crate::param::ParamRange;

/// What an edge carries beside the signal: a linear gain, `base`, and the
/// `range` a driven value will be clamped to.
///
/// Spec data beside the edge, keyed by [`EdgeId`](crate::EdgeId) in
/// [`GraphSpec::gains`](crate::GraphSpec::gains), not inside
/// [`Edge`](crate::Edge). An edge **with** an entry is a weighted lane of its
/// sink's sum; one without is plain. A weight of `1.0` is the plain sum, and a
/// negative one inverts polarity.
///
/// Compared and hashed by bits, as [`ParamRange`] is, so a spec holding one
/// stays `Eq + Hash`.
///
/// # As built (G1)
///
/// The entry has no `source` field yet: what drives a lane is the next stage
/// (doc 013, "Staging"), and no unused state ships before it. `range` is
/// carried and validated now and clamps nothing until a source drives the
/// lane: the base is a control and is never clamped, as a param's base is not.
#[derive(Clone, Copy, Debug)]
pub struct EdgeGain {
    /// The linear multiplier. Finite ([`GraphInvalid::BadEdgeGain`]); a
    /// negative value inverts polarity.
    ///
    /// [`GraphInvalid::BadEdgeGain`]: crate::GraphInvalid::BadEdgeGain
    pub base: Amplitude,
    /// What a driven value is clamped to. A NaN bound is refused.
    pub range: ParamRange,
}

impl EdgeGain {
    /// The range a new entry gets: finite, so `w * s` cannot meet infinity,
    /// and polarity is allowed (about +18 dB either way).
    pub const DEFAULT_RANGE: ParamRange = ParamRange::new(-8.0, 8.0);

    /// An entry with this `base` and the [`DEFAULT_RANGE`](Self::DEFAULT_RANGE).
    #[must_use]
    pub const fn new(base: Amplitude) -> Self {
        Self {
            base,
            range: Self::DEFAULT_RANGE,
        }
    }

    /// Whether the entry can reach the audio thread: a finite base and no NaN
    /// bound.
    #[must_use]
    pub(crate) fn is_sound(&self) -> bool {
        self.base.get().is_finite() && !self.range.min.is_nan() && !self.range.max.is_nan()
    }
}

impl Default for EdgeGain {
    /// Unity, with the default range.
    fn default() -> Self {
        Self::new(Amplitude::UNITY)
    }
}

impl PartialEq for EdgeGain {
    fn eq(&self, other: &Self) -> bool {
        self.base.get().to_bits() == other.base.get().to_bits() && self.range == other.range
    }
}

impl Eq for EdgeGain {}

impl Hash for EdgeGain {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.base.get().to_bits().hash(state);
        self.range.hash(state);
    }
}

/// One lane's live base: the cell [`Editor::set_edge_gain`] stores into and
/// the executor snapshots once per block.
///
/// An [`Amplitude`] is a plain `f32` newtype but not `bytemuck::Pod` (that
/// trait is not ours to implement for it), so the word holds the raw `f32` and
/// the cell converts at its edge: nothing outside it sees a bare float.
///
/// [`Editor::set_edge_gain`]: crate::Editor::set_edge_gain
#[derive(Clone, Debug)]
pub(crate) struct GainCell(Arc<PackedAtomic<f32>>);

impl GainCell {
    /// A cell holding `base`. Control side: allocates.
    pub(crate) fn new(base: Amplitude) -> Self {
        Self(Arc::new(PackedAtomic::new(base.get())))
    }

    /// Store `base`. Wait-free.
    pub(crate) fn store(&self, base: Amplitude) {
        self.0.store(base.get());
    }

    /// The base now. Wait-free.
    #[inline]
    pub(crate) fn load(&self) -> Amplitude {
        Amplitude::new(self.0.load())
    }
}

/// What one addend is multiplied by in one block.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Weight {
    /// Exactly `1.0`, static: the plain add, no multiply.
    Unit,
    /// A static weight other than `1.0`: `w * a`, no ramp.
    Static(Amplitude),
    /// Moving across the block from the weight the previous block was given
    /// to this one's.
    Ramp {
        /// The previous block's weight.
        from: Amplitude,
        /// This block's.
        to: Amplitude,
    },
}

/// A weighted lane's state, kept in the executor by [`EdgeId`](crate::EdgeId)
/// and carried across every commit that keeps the edge.
///
/// A plain addend of a weighted op (an edge with no entry, or the previous
/// result of a chain) has a lane with no cell: weight [`Weight::Unit`] and
/// no state.
#[derive(Clone, Debug)]
pub(crate) struct SumLane {
    /// The live base; `None` for a plain addend.
    cell: Option<GainCell>,
    /// The base as this block's snapshot read it.
    target: Amplitude,
    /// The weight the previous block was given; `None` before the lane's
    /// first block (which has no ramp).
    prev: Option<Amplitude>,
}

impl SumLane {
    /// An addend with no entry: weight `1.0`, no state.
    pub(crate) fn plain() -> Self {
        Self {
            cell: None,
            target: Amplitude::UNITY,
            prev: None,
        }
    }

    /// A lane reading `cell`. `carried` is the weight the same edge's lane
    /// was last given, when a commit keeps it; `None` starts at the value.
    pub(crate) fn weighted(cell: GainCell, carried: Option<Amplitude>) -> Self {
        Self {
            target: cell.load(),
            cell: Some(cell),
            prev: carried,
        }
    }

    /// The weight the last block was given, if the lane has run.
    pub(crate) fn carried(&self) -> Option<Amplitude> {
        self.prev
    }

    /// Read the cell, once, for this block. Called before the first op, so a
    /// store while the block runs reaches no op of it.
    #[inline]
    pub(crate) fn snapshot(&mut self) {
        if let Some(cell) = &self.cell {
            self.target = cell.load();
        }
    }

    /// This block's weight, and the lane's state moved on to the next block.
    /// Reads the snapshot, never the cell.
    #[inline]
    pub(crate) fn weight(&mut self) -> Weight {
        if self.cell.is_none() {
            return Weight::Unit;
        }
        let to = self.target;
        let from = self.prev.unwrap_or(to);
        self.prev = Some(to);
        if from.get() != to.get() {
            Weight::Ramp { from, to }
        } else if to.get() == 1.0 {
            Weight::Unit
        } else {
            Weight::Static(to)
        }
    }
}

/// `w[i]` of a ramp across `frames`: the doc's expression, spelled once.
/// The reference interpreter writes its own.
#[inline(always)]
fn ramp_at(from: f32, to: f32, i: usize, frames: f32) -> f32 {
    from + (to - from) * ((i + 1) as f32 / frames)
}

/// The first live addend: `out = +0.0 + w * a`, or `out = +0.0 + w * out`
/// when `a` is `None` (the in-place form, `out` already holds the addend).
///
/// The `+0.0 +` is the fold's start and turns a `-0.0` product into `+0.0`.
#[inline]
pub(crate) fn first(out: &mut [f32], a: Option<&[f32]>, w: Weight) {
    let frames = out.len() as f32;
    match (a, w) {
        (Some(a), Weight::Unit) => {
            for (o, &x) in out.iter_mut().zip(a) {
                *o = 0.0 + x;
            }
        }
        (None, Weight::Unit) => {
            for o in out.iter_mut() {
                *o += 0.0;
            }
        }
        (Some(a), Weight::Static(w)) => {
            let w = w.get();
            for (o, &x) in out.iter_mut().zip(a) {
                *o = 0.0 + w * x;
            }
        }
        (None, Weight::Static(w)) => {
            let w = w.get();
            for o in out.iter_mut() {
                *o = 0.0 + w * *o;
            }
        }
        (Some(a), Weight::Ramp { from, to }) => {
            let (from, to) = (from.get(), to.get());
            for (i, (o, &x)) in out.iter_mut().zip(a).enumerate() {
                *o = 0.0 + ramp_at(from, to, i, frames) * x;
            }
        }
        (None, Weight::Ramp { from, to }) => {
            let (from, to) = (from.get(), to.get());
            for (i, o) in out.iter_mut().enumerate() {
                *o = 0.0 + ramp_at(from, to, i, frames) * *o;
            }
        }
    }
}

/// A later live addend: `out = out + w * a`, a multiply and an add, two
/// roundings (Rust never contracts them into a fused multiply-add).
#[inline]
pub(crate) fn add(out: &mut [f32], a: &[f32], w: Weight) {
    let frames = out.len() as f32;
    match w {
        Weight::Unit => {
            for (o, &x) in out.iter_mut().zip(a) {
                *o += x;
            }
        }
        Weight::Static(w) => {
            let w = w.get();
            for (o, &x) in out.iter_mut().zip(a) {
                *o += w * x;
            }
        }
        Weight::Ramp { from, to } => {
            let (from, to) = (from.get(), to.get());
            for (i, (o, &x)) in out.iter_mut().zip(a).enumerate() {
                *o += ramp_at(from, to, i, frames) * x;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn amp(v: f32) -> Amplitude {
        Amplitude::new(v)
    }

    fn lane(base: f32) -> (GainCell, SumLane) {
        let cell = GainCell::new(amp(base));
        (cell.clone(), SumLane::weighted(cell, None))
    }

    /// The lane reads the snapshot, never the cell: a store after the
    /// snapshot (a store in the middle of the block) reaches no op.
    ///
    /// Mutation: `weight` reads `cell.load()` in place of `self.target` ->
    /// the store lands in this block and the first assertion fails.
    #[test]
    fn a_store_after_the_snapshot_waits_for_the_next_block() {
        let (cell, mut lane) = lane(0.5);
        lane.snapshot();
        cell.store(amp(2.0));
        assert_eq!(lane.weight(), Weight::Static(amp(0.5)));
        lane.snapshot();
        assert_eq!(
            lane.weight(),
            Weight::Ramp {
                from: amp(0.5),
                to: amp(2.0)
            }
        );
    }

    /// First block: no ramp; unity is the plain add; a static weight is
    /// static; a move ramps from the weight the previous block was given.
    ///
    /// Mutation: `prev.unwrap_or(to)` -> `prev.unwrap_or(Amplitude::SILENT)`
    /// (a first block ramps from zero) fails the first assertion.
    #[test]
    fn first_block_has_no_ramp_and_a_static_weight_skips_it() {
        let (cell, mut lane) = lane(0.25);
        lane.snapshot();
        assert_eq!(lane.weight(), Weight::Static(amp(0.25)));
        lane.snapshot();
        assert_eq!(lane.weight(), Weight::Static(amp(0.25)));
        cell.store(amp(1.0));
        lane.snapshot();
        assert_eq!(
            lane.weight(),
            Weight::Ramp {
                from: amp(0.25),
                to: amp(1.0)
            }
        );
        lane.snapshot();
        assert_eq!(lane.weight(), Weight::Unit);
    }

    #[test]
    fn a_plain_lane_is_unity_and_keeps_no_state() {
        let mut lane = SumLane::plain();
        lane.snapshot();
        assert_eq!(lane.weight(), Weight::Unit);
        assert_eq!(lane.carried(), None);
    }

    #[test]
    fn the_cell_returns_what_was_stored() {
        let cell = GainCell::new(amp(-1.5));
        assert_eq!(cell.load(), amp(-1.5));
        cell.store(amp(0.125));
        assert_eq!(cell.load(), amp(0.125));
    }

    /// The kernel's ramp is the doc's expression, sample for sample.
    ///
    /// Mutation: `ramp_at` uses `i as f32 / frames` (starts at the old
    /// weight, never reaches the new) fails.
    #[test]
    fn the_ramp_is_the_documented_expression() {
        let frames = 8usize;
        let a = vec![1.0f32; frames];
        let mut out = vec![0.0f32; frames];
        first(
            &mut out,
            Some(&a),
            Weight::Ramp {
                from: amp(0.0),
                to: amp(1.0),
            },
        );
        for (i, &o) in out.iter().enumerate() {
            let want = 0.0 + (0.0 + (1.0f32 - 0.0) * ((i + 1) as f32 / frames as f32)) * 1.0;
            assert_eq!(o.to_bits(), want.to_bits(), "frame {i}");
        }
    }
}
