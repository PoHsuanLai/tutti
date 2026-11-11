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
//!
//! # A driven lane
//!
//! An entry with a [`source`](EdgeGain::source) is **driven**: an event
//! output (a control lane) whose ramps and segments, addressed to
//! [`EdgeGain::ADDR`], replace the base from the first one on, clamped to the
//! entry's `range`. That is the param port's step, not a copy of it: a driven
//! lane keeps a one-port [`ParamState`], and the sum asks it for the lane's
//! value at every frame ([`Weight::Frames`]), with the port's rules (the
//! declick over [`PARAM_DECLICK`](crate::PARAM_DECLICK) when the source
//! changes or goes, the base before the first event, a first block that
//! starts at its value). The state is sized when the plan is applied, so
//! nothing allocates on the audio thread.

use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use tutti_curve::{Law, Piece};

use tutti_types::{Amplitude, AtomicF32, ParamCell, UnitAddr, UnitParam};

use crate::event::Event;
use crate::param::{ParamPorts, ParamRange, ParamRangeReader, ParamRanges, ParamState};
use crate::spec::EventOut;

/// What an edge carries beside the signal: a linear gain, `base`, the
/// `range` a driven value is clamped to, and the `source` that drives it.
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
/// The base is a control and is never clamped, as a param's base is not:
/// `range` clamps only a value a `source` drives.
#[derive(Clone, Copy, Debug)]
pub struct EdgeGain {
    /// The linear multiplier. Finite ([`GraphInvalid::BadEdgeGain`]); a
    /// negative value inverts polarity.
    ///
    /// [`GraphInvalid::BadEdgeGain`]: crate::GraphInvalid::BadEdgeGain
    pub base: Amplitude,
    /// What a driven value is clamped to. A NaN bound is refused.
    pub range: ParamRange,
    /// The event output that drives the gain, replacing the base from its
    /// first event on ([`GraphSpec::connect_edge_gain`](crate::GraphSpec::connect_edge_gain)).
    /// Its node must be in the graph; `None` reads the base.
    pub source: Option<EventOut>,
}

impl EdgeGain {
    /// The address a driving lane's ramps and segments carry: an edge's gain
    /// is addressed by its edge, never by a kind and an index on a node (no
    /// `UnitParam` kind is new), so the events that drive it name this one
    /// fixed address and are read by the lane they are wired to. A control
    /// lane for an edge is built with
    /// `ControlLaneNode::for_unit(EdgeGain::ADDR, ..)`.
    pub const ADDR: UnitAddr = UnitAddr::first(UnitParam::Volume);

    /// The range a new entry gets: finite, so `w * s` cannot meet infinity,
    /// and polarity is allowed (about +18 dB either way).
    pub const DEFAULT_RANGE: ParamRange = ParamRange::new(-8.0, 8.0);

    /// An entry with this `base` and the [`DEFAULT_RANGE`](Self::DEFAULT_RANGE).
    #[must_use]
    pub const fn new(base: Amplitude) -> Self {
        Self {
            base,
            range: Self::DEFAULT_RANGE,
            source: None,
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
        self.base.get().to_bits() == other.base.get().to_bits()
            && self.range == other.range
            && self.source == other.source
    }
}

impl Eq for EdgeGain {}

impl Hash for EdgeGain {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.base.get().to_bits().hash(state);
        self.range.hash(state);
        self.source.hash(state);
    }
}

/// One lane's live controls: its base, which [`Editor::set_edge_gain`] stores
/// into and the executor snapshots once per block, and the range a driven
/// value is clamped to.
///
/// An [`Amplitude`] is a plain `f32` newtype, so the cell holds the raw `f32`
/// (an `AtomicF32`, which is what a control lane's knob is) and converts at its
/// edge: nothing outside it sees a bare float. **The base is the lane's
/// knob, and the cell is its one writer's**: [`Editor::set_edge_gain`] and a
/// route's `ModState::set_knob` store into the same cell, so a routed
/// modulation moves around the base and the base is what the gain returns to. The
/// range is the param port's own cell ([`ParamRanges`] over the one address
/// [`EdgeGain::ADDR`]), so the driven step clamps to it as a port does and a
/// control lane reads it through the same [`ParamRangeReader`].
///
/// [`Editor::set_edge_gain`]: crate::Editor::set_edge_gain
#[derive(Clone, Debug)]
pub(crate) struct GainCell {
    base: Arc<AtomicF32>,
    range: Arc<ParamRanges>,
}

impl GainCell {
    /// A cell holding `base` and `range`. Control side: allocates.
    pub(crate) fn new(base: Amplitude, range: ParamRange) -> Self {
        let ranges = ParamRanges::new(ParamPorts::of(&[EdgeGain::ADDR]));
        ranges.store(EdgeGain::ADDR, range);
        Self {
            base: Arc::new(AtomicF32::new(base.get())),
            range: ranges,
        }
    }

    /// Store `base`. Wait-free.
    pub(crate) fn store(&self, base: Amplitude) {
        self.base.store(base.get(), Ordering::Release);
    }

    /// Store `range`. Wait-free.
    pub(crate) fn store_range(&self, range: ParamRange) {
        self.range.store(EdgeGain::ADDR, range);
    }

    /// The base now. Wait-free.
    #[inline]
    pub(crate) fn load(&self) -> Amplitude {
        Amplitude::new(self.base.load(Ordering::Acquire))
    }

    /// The base as a width-erased cell: the very cell the executor reads and
    /// [`store`](Self::store) writes, handed to a control lane as its knob (a
    /// native param's knob is the node's own cell, and an edge's is its
    /// gain). Clones the `Arc`: control side.
    pub(crate) fn knob(&self) -> ParamCell {
        Arc::clone(&self.base).into()
    }

    /// A read-only view of the range cell, for a control lane.
    pub(crate) fn range_reader(&self) -> ParamRangeReader {
        self.range
            .reader(EdgeGain::ADDR)
            .expect("the cell holds the gain's own address")
    }

    /// The range cells, for a driven lane's state.
    fn ranges(&self) -> Arc<ParamRanges> {
        Arc::clone(&self.range)
    }
}

/// What one addend is multiplied by in one block.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Weight<'a> {
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
    /// A driven lane: the weight at each frame, already clamped to the
    /// lane's range (the param port's step).
    Frames(&'a [f32]),
}

/// A weighted lane's state, kept in the executor by [`EdgeId`](crate::EdgeId)
/// and carried across every commit that keeps the edge.
///
/// A plain addend of a weighted op (an edge with no entry, or the previous
/// result of a chain) has a lane with no cell: weight [`Weight::Unit`] and
/// no state.
#[derive(Debug)]
pub(crate) struct SumLane {
    /// The live controls; `None` for a plain addend.
    cell: Option<GainCell>,
    /// The base as this block's snapshot read it.
    target: Amplitude,
    /// The weight the previous block was given; `None` before the lane's
    /// first block (which has no ramp).
    prev: Option<Amplitude>,
    /// The param port's state, for a lane a source drives (or one still
    /// fading to its base after the source went): sized with the plan.
    drive: Option<ParamState>,
}

impl SumLane {
    /// An addend with no entry: weight `1.0`, no state.
    pub(crate) fn plain() -> Self {
        Self {
            cell: None,
            target: Amplitude::UNITY,
            prev: None,
            drive: None,
        }
    }

    /// A lane reading `cell`. `carried` is what the same edge's lane held
    /// when a commit keeps it (`Carried::default()` starts at the value).
    /// `driven` says whether this plan's lane has a source; `max` is the
    /// prepared block, which sizes the driven state's values.
    pub(crate) fn weighted(cell: GainCell, carried: Carried, driven: bool, max: usize) -> Self {
        let Carried { prev, drive } = carried;
        // A driven lane keeps its state across the commit; an undriven one
        // keeps it only while it fades to its base. A lane that has run
        // before resumes (a source connected now is a connection to a
        // running port); a new one is on its first block.
        let drive = match drive {
            Some(state) if (driven || state.busy()) && state.fits(max) => Some(state),
            _ if driven => Some(ParamState::lane(max, cell.ranges(), prev.is_some())),
            _ => None,
        };
        Self {
            target: cell.load(),
            cell: Some(cell),
            prev,
            drive,
        }
    }

    /// What the lane holds for the next plan's lane of the same edge.
    pub(crate) fn carry(&mut self) -> Carried {
        Carried {
            prev: self.prev,
            drive: self.drive.take(),
        }
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
    /// Reads the snapshot, never the cell. `source` is the lane's driving
    /// event output and its events this block, when the plan has one.
    ///
    /// A lane with a driven state runs the param port's step first
    /// ([`ParamState::port`]): when it delivers frames the weight is those
    /// frames (and the lane's `prev` is the last of them, so a lane that
    /// stops being driven carries on from where it was); otherwise the base,
    /// as for an undriven lane.
    #[inline]
    pub(crate) fn weight<'a>(
        &'a mut self,
        frames: usize,
        source: Option<(EventOut, &[Event])>,
    ) -> Weight<'a> {
        if self.cell.is_none() {
            return Weight::Unit;
        }
        let to = self.target;
        if let Some(drive) = &mut self.drive {
            drive.begin();
            drive.port(0, EdgeGain::ADDR, frames, source, Some(to.get()));
            if let Some(f) = drive.frames(0, frames) {
                self.prev = f.last().map(|&last| Amplitude::new(last));
                return Weight::Frames(f);
            }
        }
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

/// What a lane hands the next plan's lane of the same edge.
#[derive(Debug, Default)]
pub(crate) struct Carried {
    /// The weight the lane was last given.
    pub(crate) prev: Option<Amplitude>,
    /// Its driven state, if it had one.
    pub(crate) drive: Option<ParamState>,
}

/// `w[i]` of a ramp across `frames`: a linear [`Piece`] at `t = (i + 1) / frames`.
#[inline(always)]
pub(crate) fn ramp_at(from: f32, to: f32, i: usize, frames: f32) -> f32 {
    Piece::new(from, to, Law::Linear).value((i + 1) as f32 / frames)
}

/// The first live addend: `out = +0.0 + w * a`, or `out = +0.0 + w * out`
/// when `a` is `None` (the in-place form, `out` already holds the addend).
///
/// The `+0.0 +` is the fold's start and turns a `-0.0` product into `+0.0`.
#[inline]
pub(crate) fn first(out: &mut [f32], a: Option<&[f32]>, w: Weight<'_>) {
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
        (Some(a), Weight::Frames(w)) => {
            for ((o, &x), &w) in out.iter_mut().zip(a).zip(w) {
                *o = 0.0 + w * x;
            }
        }
        (None, Weight::Frames(w)) => {
            for (o, &w) in out.iter_mut().zip(w) {
                *o = 0.0 + w * *o;
            }
        }
    }
}

/// A later live addend: `out = out + w * a`, a multiply and an add, two
/// roundings (Rust never contracts them into a fused multiply-add).
#[inline]
pub(crate) fn add(out: &mut [f32], a: &[f32], w: Weight<'_>) {
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
        Weight::Frames(w) => {
            for ((o, &x), &w) in out.iter_mut().zip(a).zip(w) {
                *o += w * x;
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
        let cell = GainCell::new(amp(base), EdgeGain::DEFAULT_RANGE);
        (
            cell.clone(),
            SumLane::weighted(cell, Carried::default(), false, 8),
        )
    }

    /// The lane reads the snapshot, never the cell: a store after the
    /// snapshot (a store in the middle of the block) reaches no op.
    #[test]
    fn a_store_after_the_snapshot_waits_for_the_next_block() {
        let (cell, mut lane) = lane(0.5);
        lane.snapshot();
        cell.store(amp(2.0));
        assert_eq!(lane.weight(8, None), Weight::Static(amp(0.5)));
        lane.snapshot();
        assert_eq!(
            lane.weight(8, None),
            Weight::Ramp {
                from: amp(0.5),
                to: amp(2.0)
            }
        );
    }

    /// First block: no ramp; unity is the plain add; a static weight is
    /// static; a move ramps from the weight the previous block was given.
    #[test]
    fn first_block_has_no_ramp_and_a_static_weight_skips_it() {
        let (cell, mut lane) = lane(0.25);
        lane.snapshot();
        assert_eq!(lane.weight(8, None), Weight::Static(amp(0.25)));
        lane.snapshot();
        assert_eq!(lane.weight(8, None), Weight::Static(amp(0.25)));
        cell.store(amp(1.0));
        lane.snapshot();
        assert_eq!(
            lane.weight(8, None),
            Weight::Ramp {
                from: amp(0.25),
                to: amp(1.0)
            }
        );
        lane.snapshot();
        assert_eq!(lane.weight(8, None), Weight::Unit);
    }

    #[test]
    fn a_plain_lane_is_unity_and_keeps_no_state() {
        let mut lane = SumLane::plain();
        lane.snapshot();
        assert_eq!(lane.weight(8, None), Weight::Unit);
        assert_eq!(lane.carry().prev, None);
    }

    #[test]
    fn the_cell_returns_what_was_stored() {
        let cell = GainCell::new(amp(-1.5), EdgeGain::DEFAULT_RANGE);
        assert_eq!(cell.load(), amp(-1.5));
        cell.store(amp(0.125));
        assert_eq!(cell.load(), amp(0.125));
    }

    /// The kernel's ramp is the doc's expression, sample for sample.
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
