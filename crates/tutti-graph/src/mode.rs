//! Bypass, mute and mix as graph state (doc 013, "Bypass and mute are graph
//! state"): [`NodeMode`], a node's [`Mix`] (its blend with the aligned input),
//! the cell they are stored in, and the state the executor keeps to apply
//! them.
//!
//! # What a mode is
//!
//! A node is [`Active`](NodeMode::Active) (it runs), [`Muted`](NodeMode::Muted)
//! (it runs and its audio outputs are silence) or
//! [`Bypassed`](NodeMode::Bypassed) (its audio outputs are its inputs, delayed
//! by its declared latency, and it is not called). The mode is **the
//! executor's**: the node and the host of a plugin know nothing of it.
//! [`Editor::set_mode`](crate::Editor::set_mode) stores into a cell the unit
//! holds and marks nothing for commit, as a param range is; the executor
//! loads the cell once per block, before the first op, so every op of the
//! block on any thread sees one mode.
//!
//! # The blend
//!
//! What a node op writes is `y * w_y + x * w_x` per sample, `y` being the
//! node's own output and `x` the aligned input (the input delayed by the
//! declared latency `L`: the **bypass line**, or the input itself when `L` is
//! zero). A mode is a pair of steady weights: Active `(1, 0)`, Bypassed
//! `(0, 1)`, Muted `(0, 0)`. A change of mode moves the weights there over
//! [`MODE_DECLICK`] frames along
//! [`CrossfadeCurve::EqualAmplitude`](crate::CrossfadeCurve), from the weights
//! the unit has now (a flip mid-fade never jumps):
//!
//! ```text
//! w(k) = w_from + (w_to - w_from) * g(k),   g = EqualAmplitude.gains(k, 256).0
//! ```
//!
//! which for a toggle from a steady mode is exactly `(g_out, g_in)` of
//! [`CrossfadeCurve::gains`](crate::CrossfadeCurve::gains), the one function
//! the executor and the reference share. A term whose weight is zero at both
//! ends of the fade is left out of the sum (`0 * NaN` is `NaN`), and a steady
//! mode copies (Bypassed), leaves (Active) or zero-fills (Muted), never
//! multiplies, so a steady bypass is the input bit for bit.
//!
//! # The mix
//!
//! A node's [`Mix`] `m` is a blend of the node's output `y` with the aligned
//! input `x̂`: `out = y * m + x̂ * (1 - m)` per channel through the bypass's
//! mapping. It is the weight pair of an [`Active`](NodeMode::Active) node:
//! `(m, 1 - m)`, so [`Mix::WET`] (all of the node's output) *is* `(1, 0)` and
//! [`Mix::DRY`] (all of the aligned input) *is* `(0, 1)`, Bypassed's.
//!
//! - **The mode wins.** Bypassed and Muted ignore the mix (their weights are
//!   `(0, 1)` and `(0, 0)` whatever it is), and a toggle to Active fades to
//!   the mix's weights, not to `(1, 0)`.
//! - **WET pays nothing.** A steady `Mix::WET` is the plain op: the aligned
//!   input is not kept beside the output, and nothing is blended (a node
//!   with latency feeds its bypass line, as it does at any mix). The output
//!   is the node's, bit for bit.
//! - **DRY runs the node.** A steady `Mix::DRY` writes `x̂` (a copy, bit for
//!   bit what Bypassed writes in steady state), but the node keeps being
//!   called: its state stays warm, and a move off DRY needs no reset and no
//!   warm-up. Only Bypassed parks a node.
//! - **A move ramps over one block**, as an edge's gain does: the cell is read
//!   once per block, before the first op, and
//!   `m[i] = m_prev + (m_new - m_prev) * ((i + 1) / frames)` where `m_prev`
//!   is what the previous block was given (the first block starts at its
//!   value). A fade between modes runs against these per-frame weights, so a
//!   mix that moves during one does not jump.
//!
//! # A driven mix
//!
//! A control lane can drive a node's mix: `connect_param` at
//! `ParamIn { node, NodeMix::ADDR }` (a reserved address no node declares).
//! The compiler lists it as the **last** entry of the node op's `params`, at
//! the sentinel port [`ParamPortOp::MIX_PORT`](crate::ParamPortOp::MIX_PORT),
//! aligns its source to the node's arrival as any param's, and counts it in the
//! cycle check, the claims and the verifier like any read of the op. The mode
//! state keeps the lane's state, a one-port [`ParamState`] built when a plan
//! first drives the unit's mix (`ModeState::set_driven`, at apply: the only
//! allocation) and kept for the unit's life.
//!
//! A driven unit never takes the plain op. Each block, before anything reads
//! the mix, the lane runs the param port's step on its source's events
//! (`ModeState::drive_step`), with the mix cell's snapshot as the base: when it
//! delivers frames they **replace** the block's ramp (`m[i]` is the lane's
//! value at frame `i`, clamped to the port's range, itself narrowed to
//! `0..=1`) and the next block ramps from the last of them; otherwise the base
//! ramp above applies (a source that goes declicks to the base over
//! [`PARAM_DECLICK`](crate::PARAM_DECLICK), as a driven edge gain does). The
//! mode still wins, and the lane runs while the node is parked. The cell is the
//! lane's knob ([`Editor::mix_cell`](crate::Editor::mix_cell)), so
//! [`Editor::set_mix`](crate::Editor::set_mix) and a route are one writer.
//!
//! # Un-bypass
//!
//! A bypassed unit is not called, so its state is stale. The toggle that
//! leaves Bypassed calls [`Node::reset`](crate::Node::reset) before the first
//! call, keeps the aligned input audible for `L` frames (the node's output is
//! its warm-up, discarded), and then fades from the input to the output.
//! (Toggling to Muted resets too and skips the warm-up: nothing of the node
//! is heard.) A node that kept running (a toggle back while still fading
//! out, or from Muted) is not reset.
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use tutti_types::{AtomicF32, Mix, ParamCell, Samples, UnitAddr, UnitParam};

use crate::NodeKey;
use crate::arena::Arena;
use crate::event::Event;
use crate::fade::CrossfadeCurve;
use crate::gain::ramp_at;
use crate::held::Held;
use crate::kernels::AudioRing;
use crate::node::Shape;
use crate::param::{ParamPorts, ParamRange, ParamRangeReader, ParamRanges, ParamState};
use crate::pass::Pass;
use crate::spec::EventOut;

/// How long a change of mode crossfades: 256 frames, [`PARAM_DECLICK`](crate::PARAM_DECLICK)'s
/// value, along [`CrossfadeCurve::EqualAmplitude`] (the aligned input and the
/// node's output are phase-coherent, the case that curve is for).
pub const MODE_DECLICK: Samples = Samples(256);

/// What the graph does with a node (doc 013, "Bypass and mute are graph
/// state"): a state the graph holds for any node, a plugin included. See the
/// module docs of `src/mode.rs` for what each mode makes of the node's outputs.
///
/// - **`Active`**: the node runs.
/// - **`Muted`**: the node keeps running (it consumes its events, keeps its
///   state and advances its tail) and its audio outputs are silence, flagged
///   silent, so everything downstream takes its silence skip. Its event
///   outputs are untouched.
/// - **`Bypassed`**: its audio outputs are its inputs delayed by its
///   declared latency (output `c` is input `c`; an output past the input
///   width, and every output of a node with no audio input, is silence), its
///   event outputs are empty, and it is not called.
///
/// Set with [`Editor::set_mode`](crate::Editor::set_mode); absent from
/// [`GraphSpec::modes`](crate::GraphSpec::modes) is `Active`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum NodeMode {
    /// The node runs.
    #[default]
    Active,
    /// The node's audio inputs pass to its outputs, delayed by its declared
    /// latency, and it is not called.
    Bypassed,
    /// The node runs and its audio outputs are silence.
    Muted,
}

impl NodeMode {
    fn to_bits(self) -> u8 {
        match self {
            Self::Active => 0,
            Self::Bypassed => 1,
            Self::Muted => 2,
        }
    }

    fn from_bits(bits: u8) -> Self {
        match bits {
            1 => Self::Bypassed,
            2 => Self::Muted,
            _ => Self::Active,
        }
    }

    /// The weights a steady mode holds, at mix `m` (only Active reads it).
    fn weights(self, m: f32) -> Weights {
        match self {
            Self::Active => Weights {
                node: m,
                input: 1.0 - m,
            },
            Self::Bypassed => Weights::BYPASSED,
            Self::Muted => Weights::MUTED,
        }
    }
}

/// Why [`Editor::set_mode`](crate::Editor::set_mode) refused.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ModeError {
    /// The spec has no node at the key.
    NoSuchNode {
        /// The key.
        node: NodeKey,
    },
    /// The node has no pass ([`PassError`](crate::PassError)): a bypass
    /// would have to guess where its input goes.
    Unplaced {
        /// The key.
        node: NodeKey,
        /// What it would need to declare.
        error: crate::PassError,
    },
}

impl std::fmt::Display for ModeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoSuchNode { node } => write!(f, "no node {} to set the mode of", node.0),
            Self::Unplaced { node, error } => {
                write!(f, "node {} cannot be bypassed: {error}", node.0)
            }
        }
    }
}

impl std::error::Error for ModeError {}

/// A node's [`Mix`] as the spec holds it (doc 013, "Bypass and mute are graph
/// state", M4): the blend of the node's output with its aligned input,
/// `out = y * m + x̂ * (1 - m)`. [`Mix::WET`] (all of the node's output) is
/// the canonical absent entry of [`GraphSpec::mixes`](crate::GraphSpec::mixes).
///
/// Compared and hashed by bits, as [`ParamRange`] is, so a
/// spec holding one stays `Eq + Hash`.
#[derive(Clone, Copy, Debug)]
pub struct NodeMix {
    /// The blend position: finite and in `0..=1`
    /// ([`GraphInvalid::BadMix`](crate::GraphInvalid::BadMix)).
    pub mix: Mix,
}

impl NodeMix {
    /// The address a driving lane's ramps and segments carry, and the
    /// [`ParamIn`](crate::ParamIn) key of the mix in
    /// [`GraphSpec::params`](crate::GraphSpec::connect_param): a node's mix
    /// is addressed by its node, never by a kind and an index it declares
    /// (index 255 of [`UnitParam::Wet`] is reserved:
    /// [`ParamPortsError::Reserved`](crate::ParamPortsError::Reserved)), so
    /// the events that drive it name this one fixed address. A control lane
    /// for a mix is built with `ControlLaneNode::for_unit(NodeMix::ADDR, ..)`.
    pub const ADDR: UnitAddr = UnitAddr::new(UnitParam::Wet, u8::MAX);

    /// The range a mix's port gets without an entry: the whole blend.
    pub const DEFAULT_RANGE: ParamRange = ParamRange::new(0.0, 1.0);

    /// `range` as the lane clamps to it: inside `0..=1` whatever was asked (a
    /// mix off the ends is not a blend), so a range a host sets wider than
    /// the blend is narrowed here, once, and the lane's step needs no second
    /// clamp.
    #[must_use]
    pub fn lane_range(range: ParamRange) -> ParamRange {
        let (lo, hi) = range.ordered();
        ParamRange::new(lo.clamp(0.0, 1.0), hi.clamp(0.0, 1.0))
    }

    /// Whether `mix` can be one: finite and in `0..=1` (what
    /// [`Mix::new_clamped`] makes).
    #[must_use]
    pub(crate) fn is_sound(mix: Mix) -> bool {
        (0.0..=1.0).contains(&mix.get())
    }
}

impl PartialEq for NodeMix {
    fn eq(&self, other: &Self) -> bool {
        self.mix.get().to_bits() == other.mix.get().to_bits()
    }
}

impl Eq for NodeMix {}

impl std::hash::Hash for NodeMix {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.mix.get().to_bits().hash(state);
    }
}

/// Why [`Editor::set_mix`](crate::Editor::set_mix) refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum MixError {
    /// The spec has no node at the key.
    NoSuchNode {
        /// The key.
        node: NodeKey,
    },
    /// The mix is not in `0..=1` (or is NaN): a blend position off the ends of
    /// the blend, which is [`Mix::new_clamped`]'s to make, not the graph's.
    OutOfRange,
    /// A mix short of [`Mix::WET`] on a node with no pass
    /// ([`PassError`](crate::PassError)): its dry side would be a guess.
    Unplaced {
        /// The key.
        node: NodeKey,
        /// What it would need to declare.
        error: crate::PassError,
    },
}

impl std::fmt::Display for MixError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoSuchNode { node } => write!(f, "no node {} to set the mix of", node.0),
            Self::OutOfRange => write!(f, "a mix must be in 0..=1"),
            Self::Unplaced { node, error } => {
                write!(f, "node {} cannot be mixed with its input: {error}", node.0)
            }
        }
    }
}

impl std::error::Error for MixError {}

/// A unit's mode and mix, shared between the editor (the one writer) and the
/// unit (the executor reads each once per block). An `AtomicU8` and the
/// mix's raw `f32` (`Mix` is not `Pod`; the cell converts at its edge): a
/// mode and a mix are controls, not rewires. **The mix cell is the mix's one
/// writer's**: [`Editor::set_mix`](crate::Editor::set_mix) stores into it.
#[derive(Clone, Debug)]
pub(crate) struct ModeCell {
    mode: Arc<AtomicU8>,
    mix: Arc<AtomicF32>,
    /// The range a driven mix is clamped to: the param port's own cell over
    /// the one address [`NodeMix::ADDR`], so the lane's step clamps to it as a
    /// port does and a control lane reads it through the same
    /// [`ParamRangeReader`].
    range: Arc<ParamRanges>,
}

impl ModeCell {
    /// A cell holding `mode` and `mix`. Control side: allocates.
    pub(crate) fn new(mode: NodeMode, mix: Mix) -> Self {
        let range = ParamRanges::new(ParamPorts::MIX);
        range.store(NodeMix::ADDR, NodeMix::DEFAULT_RANGE);
        Self {
            mode: Arc::new(AtomicU8::new(mode.to_bits())),
            mix: Arc::new(AtomicF32::new(mix.get())),
            range,
        }
    }

    /// Store the range a driven mix is clamped to (narrowed to `0..=1`).
    /// Wait-free.
    pub(crate) fn store_range(&self, range: ParamRange) {
        self.range.store(NodeMix::ADDR, NodeMix::lane_range(range));
    }

    /// The mix as a width-erased cell: the very cell the executor reads and
    /// [`store_mix`](Self::store_mix) writes, handed to a control lane as its
    /// knob, so [`Editor::set_mix`](crate::Editor::set_mix) and a route are
    /// one writer. Clones the `Arc`: control side.
    pub(crate) fn knob(&self) -> ParamCell {
        Arc::clone(&self.mix).into()
    }

    /// A read-only view of the range cell, for a control lane.
    pub(crate) fn range_reader(&self) -> ParamRangeReader {
        self.range
            .reader(NodeMix::ADDR)
            .expect("the cell holds the mix's own address")
    }

    /// The range cells, for a driven lane's state.
    fn ranges(&self) -> Arc<ParamRanges> {
        Arc::clone(&self.range)
    }

    /// Store `mode`. Wait-free.
    pub(crate) fn store(&self, mode: NodeMode) {
        self.mode.store(mode.to_bits(), Ordering::Release);
    }

    /// Store `mix`. Wait-free.
    pub(crate) fn store_mix(&self, mix: Mix) {
        self.mix.store(mix.get(), Ordering::Release);
    }

    /// The mode now. Wait-free.
    #[inline]
    pub(crate) fn load(&self) -> NodeMode {
        NodeMode::from_bits(self.mode.load(Ordering::Acquire))
    }

    /// The mix now. Wait-free.
    #[inline]
    pub(crate) fn load_mix(&self) -> Mix {
        Mix::new(self.mix.load(Ordering::Acquire))
    }
}

/// How much of the node's output and of the aligned input a node op writes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Weights {
    /// The node's own output (`y`).
    pub(crate) node: f32,
    /// The aligned input (`x`).
    pub(crate) input: f32,
}

impl Weights {
    pub(crate) const BYPASSED: Self = Self {
        node: 0.0,
        input: 1.0,
    };
    pub(crate) const MUTED: Self = Self {
        node: 0.0,
        input: 0.0,
    };

    /// These weights moved `g` of the way to `to`. At `g` one of the two ends
    /// of a toggle from a steady mode this is exactly `1 - g` and `g` (the
    /// `gains` of [`CrossfadeCurve`]): `1 + (0 - 1) * g` and `0 + (1 - 0) * g`.
    fn toward(self, to: Self, g: f32) -> Self {
        Self {
            node: self.node + (to.node - self.node) * g,
            input: self.input + (to.input - self.input) * g,
        }
    }
}

/// A fade in progress: from the weights the unit had when it began, to its
/// mode's, `k` frames of [`MODE_DECLICK`] done.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Ramp {
    from: Weights,
    k: usize,
}

/// What a block makes of one unit, in order: `warm` frames of the aligned
/// input alone, then the frames of `ramp`, then `steady`, all against the
/// block's `mix`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Segments {
    pub(crate) warm: usize,
    pub(crate) ramp: Option<RampRun>,
    pub(crate) steady: NodeMode,
    pub(crate) mix: MixRun,
}

/// The part of a fade a block runs: from the weights the fade began at, toward
/// the weights of `heading` at each frame's mix.
#[derive(Clone, Copy, Debug)]
pub(crate) struct RampRun {
    from: Weights,
    heading: NodeMode,
    /// The fade's frame the block's first ramp frame is.
    k: usize,
    len: usize,
}

/// A block's mix: the previous block's to this one's, across `frames`
/// ([`ramp_at`], as an edge's gain ramps), or, when a control lane drives it
/// and delivers this block, the lane's value at each frame (`driven`, the
/// values handed beside it: a `&[f32]` of `frames`, which every method that
/// reads a frame takes as `lane`).
#[derive(Clone, Copy, Debug)]
pub(crate) struct MixRun {
    from: f32,
    to: f32,
    frames: usize,
    driven: bool,
}

/// What a block's mix is, for the flags of the output.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MixState {
    /// Steady `Mix::WET`.
    Wet,
    /// Steady `Mix::DRY`.
    Dry,
    /// Steady, off the ends.
    Between,
    /// Moving across the block.
    Moving,
}

impl MixRun {
    /// Whether the whole block is steady [`Mix::WET`]: nothing to blend.
    pub(crate) fn is_wet(&self) -> bool {
        !self.driven
            && self.from.to_bits() == 1.0f32.to_bits()
            && self.to.to_bits() == 1.0f32.to_bits()
    }

    /// Which of the four things a block's mix is.
    pub(crate) fn state(&self) -> MixState {
        if self.is_wet() {
            MixState::Wet
        } else if !self.is_static() {
            MixState::Moving
        } else if self.to == 0.0 {
            MixState::Dry
        } else {
            MixState::Between
        }
    }

    /// Whether the mix does not move across the block (a driven block moves
    /// at every frame, whatever its ends are).
    fn is_static(&self) -> bool {
        !self.driven && self.from.to_bits() == self.to.to_bits()
    }

    /// The mix at frame `i` of the block; `lane` is the driving lane's values
    /// (read only when the block is driven).
    #[inline]
    fn at(&self, i: usize, lane: &[f32]) -> f32 {
        if self.driven {
            lane[i]
        } else if self.is_static() {
            self.to
        } else {
            ramp_at(self.from, self.to, i, self.frames as f32)
        }
    }

    /// The mix of the block's last frame (a ramp's last sample, which
    /// `from + (to - from) * 1.0` need not make exactly `to`).
    fn last(&self, lane: &[f32]) -> f32 {
        self.at(self.frames - 1, lane)
    }
}

impl Segments {
    /// Whether the whole block is one steady mode.
    pub(crate) fn is_steady(&self) -> bool {
        self.warm == 0 && self.ramp.is_none()
    }

    /// Blend one output channel in place: `out` holds the node's output `y`,
    /// `x` the aligned input (`None`: silence, a channel with no input to
    /// pass).
    ///
    /// A term whose weight is zero at both ends of the fade is left out, so a
    /// NaN in a term that is off is never heard; the steady modes copy,
    /// leave or fill, never multiply. A steady Active block at a mix off the
    /// ends is `y * m + x * (1 - m)` (a channel with no `x` is `y * m`); at
    /// `Mix::WET` it leaves `out` alone and at `Mix::DRY` it copies `x`.
    /// `lane` is the driving lane's value at each frame of a driven block
    /// (empty otherwise).
    pub(crate) fn blend(&self, out: &mut [f32], x: Option<&[f32]>, lane: &[f32]) {
        let put = |out: &mut [f32], range: std::ops::Range<usize>| match x {
            Some(x) => out[range.clone()].copy_from_slice(&x[range]),
            None => out[range].fill(0.0),
        };
        put(out, 0..self.warm);
        let mut at = self.warm;
        if let Some(r) = self.ramp {
            let input = x.is_some();
            for j in 0..r.len {
                let i = at + j;
                let to = r.heading.weights(self.mix.at(i, lane));
                let w = r.from.toward(to, gain(r.k + j));
                let node_on = r.from.node != 0.0 || to.node != 0.0;
                let input_on = input && (r.from.input != 0.0 || to.input != 0.0);
                out[i] = match (node_on, input_on) {
                    (true, true) => out[i] * w.node + x.map_or(0.0, |x| x[i]) * w.input,
                    (true, false) => out[i] * w.node,
                    (false, true) => x.map_or(0.0, |x| x[i]) * w.input,
                    (false, false) => 0.0,
                };
            }
            at += r.len;
        }
        match self.steady {
            NodeMode::Active => self.blend_active(out, x, at, lane),
            NodeMode::Muted => out[at..].fill(0.0),
            NodeMode::Bypassed => put(out, at..out.len()),
        }
    }

    /// The steady Active frames from `at` on, against the block's mix.
    fn blend_active(&self, out: &mut [f32], x: Option<&[f32]>, at: usize, lane: &[f32]) {
        let m = self.mix;
        if m.is_wet() {
            return;
        }
        if m.is_static() {
            let w = m.to;
            match x {
                _ if w == 1.0 => {}
                Some(x) if w == 0.0 => out[at..].copy_from_slice(&x[at..]),
                None if w == 0.0 => out[at..].fill(0.0),
                Some(x) => {
                    let wx = 1.0 - w;
                    for (o, &x) in out[at..].iter_mut().zip(&x[at..]) {
                        *o = *o * w + x * wx;
                    }
                }
                None => {
                    for o in &mut out[at..] {
                        *o *= w;
                    }
                }
            }
            return;
        }
        for i in at..out.len() {
            let w = m.at(i, lane);
            out[i] = match x {
                Some(x) => out[i] * w + x[i] * (1.0 - w),
                None => out[i] * w,
            };
        }
    }
}

/// The incoming gain of frame `k` of the fade.
fn gain(k: usize) -> f32 {
    CrossfadeCurve::EqualAmplitude
        .gains(k, MODE_DECLICK.get())
        .0
}

/// The aligned-input path of a latent node: one ring of its declared latency
/// per channel the node passes (`min(inputs, outputs)`), fed every block with
/// the node's input. Built on the control side, never grown.
pub(crate) struct BypassLine {
    rings: Vec<AudioRing>,
}

impl BypassLine {
    /// The line `shape` needs: none for a node with no latency, or with no
    /// channel to pass. Control side: allocates.
    pub(crate) fn for_shape(shape: &Shape) -> Option<Self> {
        let latency = shape.latency.samples();
        let main = Pass::of_or_nothing(shape).inputs();
        (!latency.is_zero() && main > 0).then(|| Self {
            rings: (0..main).map(|_| AudioRing::new(latency)).collect(),
        })
    }
}

/// A unit's mode as the executor keeps it. Lives in the unit's record in the
/// store, as its crossfade does, and is cut by a hard edit as one is.
pub(crate) struct ModeState {
    cell: ModeCell,
    /// The mode this block reads (the cell, once per block).
    seen: NodeMode,
    /// Whether this block's op is the plain one (`snapshot` decides, once).
    plain: bool,
    /// The mix the previous block was given, and this block's snapshot of
    /// the cell: the ends of this block's ramp.
    mix_from: f32,
    mix_to: f32,
    /// The mode the weights are heading for.
    heading: NodeMode,
    /// The weights as the last frame left them.
    weights: Weights,
    ramp: Option<Ramp>,
    /// Frames of the aligned input left to hear before the fade in.
    warm: usize,
    /// The node has not run since it was parked: un-bypassing resets it.
    stale: bool,
    /// A node with nothing to pass and nothing to silence: no outputs at
    /// all (a meter, a tap). Never parked, never muted.
    inert: bool,
    /// The declared latency, in frames.
    latency: usize,
    /// Which input channel feeds which output channel when the node is not
    /// called ([`Pass`]).
    pass: Pass,
    line: Option<BypassLine>,
    /// The aligned input of a block that needs it away from the node's own
    /// slots: one slot per passed channel.
    scratch: Option<Arena>,
    /// The fold of that aligned input onto the outputs, one slot per output
    /// channel: only for a pass that folds.
    folded: Option<Arena>,
    /// What each event input has been sent and not ended, to close when the
    /// node is bypassed.
    held_in: Vec<Held>,
    /// The mix's driven lane: the param port's step over one port, built when
    /// a plan first drives this unit's mix ([`set_driven`](Self::set_driven))
    /// and kept for the unit's life, so no commit frees one on the audio
    /// thread (an idle one costs `max_block` floats and no work).
    drive: Option<ParamState>,
    /// Whether this block's lane delivered frames: they replace the ramp.
    lane_on: bool,
    /// Whether the unit has run a block: a lane built for a unit that has is
    /// a connection to a running port (it declicks), not a first block.
    ran: bool,
}

impl ModeState {
    /// The state of a unit about to be placed: steady in `cell`'s mode, with
    /// no fade (a fork, a new unit). Control side: allocates its line, its
    /// scratch and its tables.
    pub(crate) fn new(cell: ModeCell, shape: &Shape, max_block: usize) -> Self {
        let mode = cell.load();
        let mix = cell.load_mix().get();
        let pass = Pass::of_or_nothing(shape);
        let main = pass.inputs();
        let folded = pass.folds().then(|| Arena::new(pass.outputs(), max_block));
        Self {
            cell,
            seen: mode,
            plain: false,
            mix_from: mix,
            mix_to: mix,
            heading: mode,
            weights: mode.weights(mix),
            ramp: None,
            warm: 0,
            // A unit that begins bypassed has not run: un-bypassing resets it.
            stale: mode == NodeMode::Bypassed,
            inert: shape.audio_out().count() == 0 && shape.event_out == 0,
            latency: shape.latency.samples().get(),
            pass,
            line: BypassLine::for_shape(shape),
            scratch: (main > 0).then(|| Arena::new(main, max_block)),
            folded,
            held_in: (0..shape.event_in).map(|_| Held::new()).collect(),
            drive: None,
            lane_on: false,
            ran: false,
        }
    }

    /// Say whether the plan now applied drives this unit's mix, building the
    /// lane's state the first time it does (sized to `max` frames: the only
    /// allocation, at apply). A lane that is no longer driven keeps its state,
    /// which fades to the base and then idles.
    pub(crate) fn set_driven(&mut self, driven: bool, max: usize) {
        if driven && self.drive.is_none() {
            self.drive = Some(ParamState::lane(max, self.cell.ranges(), self.ran));
        }
        debug_assert!(
            self.drive.as_ref().is_none_or(|d| d.fits(max)),
            "a kept unit's lane serves the executor's block"
        );
    }

    /// The mix's lane for a block: the param port's step on the driving
    /// source's events (`None`: nothing drives it), run before anything reads
    /// the block's mix. When it delivers frames they are the block's mix at
    /// each frame ([`lane`](Self::lane)), clamped to the port's range, and
    /// the next block ramps from the last of them; otherwise the base, the
    /// cell's snapshot, ramps as it does undriven.
    #[inline]
    pub(crate) fn drive_step(&mut self, frames: usize, source: Option<(EventOut, &[Event])>) {
        let Some(drive) = &mut self.drive else {
            return;
        };
        drive.begin();
        drive.port(0, NodeMix::ADDR, frames, source, Some(self.mix_to));
        if let Some(f) = drive.frames(0, frames) {
            self.lane_on = true;
            self.mix_to = f[frames - 1];
        }
    }

    /// The driving lane's value at each frame of this block: empty unless the
    /// lane delivered ([`drive_step`](Self::drive_step)).
    pub(crate) fn lane(&self, frames: usize) -> &[f32] {
        self.drive
            .as_ref()
            .filter(|_| self.lane_on)
            .and_then(|d| d.frames(0, frames))
            .unwrap_or(&[])
    }

    /// Read the cell, and decide which op the block runs: once per block,
    /// before the first op, so every op of the block sees one mode. (The
    /// state moves only inside the unit's own op, after that decision.)
    #[inline]
    pub(crate) fn snapshot(&mut self) {
        self.seen = self.cell.load();
        self.ran = true;
        self.lane_on = false;
        // The mix advances every block, parked or silent or not, so a move
        // made while the node was out has arrived when it is heard.
        self.mix_from = self.mix_to;
        self.mix_to = self.cell.load_mix().get();
        self.plain = self.inert
            || (self.seen == NodeMode::Active
                && self.heading == NodeMode::Active
                && self.mix_run(0).is_wet()
                && self.ramp.is_none()
                && self.warm == 0
                && self.line.is_none()
                && self.held_in.is_empty()
                && self.drive.as_ref().is_none_or(|d| !d.busy()));
    }

    /// The block's mix: the previous block's to this one's, across `frames`.
    fn mix_run(&self, frames: usize) -> MixRun {
        MixRun {
            from: self.mix_from,
            to: self.mix_to,
            frames,
            driven: self.lane_on,
        }
    }

    /// Whether this block's op is the plain one: steady Active at `Mix::WET`
    /// with nothing to feed or track, or nothing a mode could act on.
    #[inline]
    pub(crate) fn is_plain(&self) -> bool {
        self.plain
    }

    /// Replace the line, after a change of declared latency: the line of a
    /// commit that rebuilt it. Returns the old one, to free on the control
    /// side.
    pub(crate) fn relined(
        &mut self,
        latency: usize,
        line: Option<BypassLine>,
    ) -> Option<BypassLine> {
        self.latency = latency;
        std::mem::replace(&mut self.line, line)
    }

    /// The mode this block reads.
    pub(crate) fn seen(&self) -> NodeMode {
        self.seen
    }

    /// The mode the unit is in: the one its weights head for.
    pub(crate) fn heading(&self) -> NodeMode {
        self.heading
    }

    /// Whether the node is stale: parked and not yet reset.
    pub(crate) fn is_stale(&self) -> bool {
        self.stale
    }

    /// Mark the node stale: it was parked this block.
    pub(crate) fn park(&mut self) {
        self.stale = true;
    }

    /// Move to `want`. Returns whether the node must be reset before it is
    /// called (it was parked, and will be called again).
    pub(crate) fn toggle(&mut self, want: NodeMode) -> bool {
        self.warm = 0;
        let reset = self.stale;
        if reset {
            self.stale = false;
            if want == NodeMode::Active {
                self.warm = self.latency;
            }
        }
        self.ramp = (self.weights != want.weights(self.mix_to)).then_some(Ramp {
            from: self.weights,
            k: 0,
        });
        self.heading = want;
        reset
    }

    /// Whether the node is parked this block: Bypassed, its fade done, its
    /// event inputs all closed.
    pub(crate) fn is_parked(&self) -> bool {
        self.heading == NodeMode::Bypassed
            && self.ramp.is_none()
            && self.warm == 0
            && !self.holds_input()
    }

    /// Whether the node's event inputs are discarded (and what they held
    /// closed): from the toggle to Bypassed on.
    pub(crate) fn discards_events(&self) -> bool {
        self.heading == NodeMode::Bypassed && !self.inert
    }

    /// Whether an event input still holds a key or a pedal the node has not
    /// been sent the close of.
    pub(crate) fn holds_input(&self) -> bool {
        self.held_in.iter().any(|h| !h.is_empty())
    }

    /// Whether this block needs the aligned input beside the node's output.
    pub(crate) fn needs_input(&self) -> bool {
        self.warm > 0
            || self.heading == NodeMode::Bypassed
            || (self.heading == NodeMode::Active && !self.mix_run(0).is_wet())
            || self.ramp.is_some_and(|r| {
                r.from.input != 0.0 || self.heading.weights(self.mix_to).input != 0.0
            })
    }

    /// Input channels the aligned path carries.
    pub(crate) fn inputs(&self) -> usize {
        self.pass.inputs()
    }

    /// Output channels the aligned input reaches; the rest have none.
    pub(crate) fn outputs(&self) -> usize {
        self.pass.outputs()
    }

    /// Whether output channel `c`'s aligned input is silent when the inputs
    /// in `silent` (bit `i`) are.
    pub(crate) fn passes_silence(&self, c: usize, silent: u64) -> bool {
        self.pass.silent(c, silent)
    }

    /// Fold the aligned inputs onto the outputs the blend reads
    /// ([`passed`](Self::passed)); a pass that does not fold has nothing to do.
    pub(crate) fn fold_aligned(&mut self, frames: usize) {
        let Self {
            pass,
            scratch,
            folded,
            ..
        } = self;
        if let (Some(src), Some(dst)) = (scratch.as_ref(), folded.as_mut()) {
            for c in 0..pass.outputs() {
                pass.fold_into(
                    |i| src.slot(i as u32, frames),
                    c,
                    dst.slot_mut(c as u32, frames),
                );
            }
        }
    }

    /// Fold the aligned inputs into output channel `c`'s own slot `out`
    /// (a parked node's output). Only for a pass that [`folds`](Self::folds).
    pub(crate) fn fold_aligned_into(&self, c: usize, out: &mut [f32], frames: usize) {
        let src = self
            .scratch
            .as_ref()
            .expect("a node with a channel to pass has a scratch");
        self.pass.fold_into(|i| src.slot(i as u32, frames), c, out);
    }

    /// Whether the pass folds channels (it is not output `c` = input `c`).
    pub(crate) fn folds(&self) -> bool {
        self.pass.folds()
    }

    /// Feed channel `c`'s input through the aligned-input path: its ring
    /// takes the block (when the node has a line), and with `keep` the
    /// aligned input lands in scratch slot `c` for the blend. Returns whether
    /// the aligned input is known silent.
    pub(crate) fn align(
        &mut self,
        c: usize,
        src: &[f32],
        silent: bool,
        keep: bool,
        frames: usize,
    ) -> bool {
        let Self { line, scratch, .. } = self;
        let ring = line.as_mut().and_then(|l| l.rings.get_mut(c));
        match (ring, keep) {
            (Some(ring), true) => ring.run(src, Self::slot(scratch, c, frames), silent),
            (Some(ring), false) => {
                ring.push(src, silent);
                silent
            }
            (None, true) => {
                Self::slot(scratch, c, frames).copy_from_slice(src);
                silent
            }
            (None, false) => silent,
        }
    }

    fn slot(scratch: &mut Option<Arena>, c: usize, frames: usize) -> &mut [f32] {
        scratch
            .as_mut()
            .expect("a node with a channel to pass has a scratch")
            .slot_mut(c as u32, frames)
    }

    /// Output channel `c`'s aligned input, as [`align`](Self::align) (and
    /// [`fold_aligned`](Self::fold_aligned), when the pass folds) left it.
    pub(crate) fn passed(&self, c: usize, frames: usize) -> &[f32] {
        self.folded
            .as_ref()
            .or(self.scratch.as_ref())
            .expect("a node with a channel to pass has a scratch")
            .slot(c as u32, frames)
    }

    /// Channel `c`'s ring of the bypass line, for a parked node's output.
    pub(crate) fn ring(&mut self, c: usize) -> Option<&mut AudioRing> {
        self.line.as_mut().and_then(|l| l.rings.get_mut(c))
    }

    /// The tables of what each event input holds.
    pub(crate) fn held_in(&mut self) -> &mut [Held] {
        &mut self.held_in
    }

    /// This block's segments, and advance the state past them.
    pub(crate) fn step(&mut self, frames: usize) -> Segments {
        let warm = self.warm.min(frames);
        self.warm -= warm;
        let mix = self.mix_run(frames);
        let lane: &[f32] = self
            .drive
            .as_ref()
            .filter(|_| self.lane_on)
            .and_then(|d| d.frames(0, frames))
            .unwrap_or(&[]);
        let mut seg = Segments {
            warm,
            ramp: None,
            steady: self.heading,
            mix,
        };
        if warm == frames {
            return seg;
        }
        if let Some(r) = &mut self.ramp {
            let len = (MODE_DECLICK.get() - r.k).min(frames - warm);
            seg.ramp = Some(RampRun {
                from: r.from,
                heading: self.heading,
                k: r.k,
                len,
            });
            r.k += len;
            let to = self.heading.weights(mix.at(warm + len - 1, lane));
            self.weights = r.from.toward(to, gain(r.k - 1));
            if r.k == MODE_DECLICK.get() {
                self.ramp = None;
            }
        }
        if self.ramp.is_none() {
            self.weights = self.heading.weights(mix.last(lane));
        }
        seg
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::param::PARAM_DECLICK;
    use tutti_types::{ChannelLayout, Latency};

    fn shape(latency: usize) -> Shape {
        Shape::through_unassigned(ChannelLayout::MONO).with_latency(Latency::new(Samples(latency)))
    }

    /// A unit of `shape` at `mix`, steady Active, after the block's snapshot.
    fn at(shape: &Shape, mix: f32) -> (ModeCell, ModeState) {
        let cell = ModeCell::new(NodeMode::Active, Mix::new(mix));
        let mut state = ModeState::new(cell.clone(), shape, 64);
        state.snapshot();
        (cell, state)
    }

    /// **WET pays nothing.** A plain node (no latency, no event inputs) at
    /// steady WET takes the plain op, and needs no aligned input; a latent one
    /// is never plain (it feeds its line, as it does at any mix) but at WET does
    /// not keep the aligned input beside its output. Off WET (DRY included) a
    /// block needs it, and so does the block a mix arrives back at WET on (its
    /// ramp), after which the node is plain again.
    #[test]
    fn a_node_at_wet_takes_the_plain_op_and_keeps_no_aligned_input() {
        let (_, plain) = at(&shape(0), 1.0);
        assert!(plain.is_plain());
        assert!(!plain.needs_input());

        let (_, latent) = at(&shape(480), 1.0);
        assert!(!latent.is_plain(), "a latent node feeds its line");
        assert!(!latent.needs_input(), "but keeps nothing beside its output");

        for m in [0.0, 0.25, 0.999] {
            let (_, plain) = at(&shape(0), m);
            assert!(!plain.is_plain(), "mix {m}");
            assert!(plain.needs_input(), "mix {m}");
            let (_, latent) = at(&shape(480), m);
            assert!(latent.needs_input(), "mix {m}");
        }

        let (cell, mut state) = at(&shape(0), 0.5);
        cell.store_mix(Mix::WET);
        state.snapshot();
        assert!(!state.is_plain(), "the ramp block back at WET");
        assert!(state.needs_input());
        state.step(64);
        state.snapshot();
        assert!(state.is_plain(), "and the block after it");
        assert!(!state.needs_input());
    }

    /// **A driven mix pays, an undriven one does not.** A node whose mix no
    /// plan drives keeps the plain op; one the plan drives is never plain (its
    /// lane is busy: it delivers frames every block); one whose lane was just
    /// let go is not plain until the lane has finished fading to the base
    /// (`PARAM_DECLICK` frames), and is plain again after (an idle lane left
    /// behind costs nothing).
    #[test]
    fn a_node_whose_mix_is_not_driven_keeps_the_plain_op() {
        let from = EventOut {
            node: NodeKey(1),
            port: 0,
        };
        let (_, mut state) = at(&shape(0), 1.0);
        assert!(state.is_plain(), "never driven");

        state.set_driven(true, 64);
        state.snapshot();
        assert!(!state.is_plain(), "driven");
        state.drive_step(64, Some((from, &[])));
        state.step(64);

        // Let go of the source: its lane fades to the base, blended, for the
        // declick and the block that sees it done.
        state.set_driven(false, 64);
        let fade = PARAM_DECLICK.get() / 64 + 1;
        for block in 0..fade {
            state.snapshot();
            assert!(!state.is_plain(), "fading, block {block}");
            state.drive_step(64, None);
            state.step(64);
        }
        state.snapshot();
        assert!(state.is_plain(), "faded: the plain op again");
    }

    /// A moving mix is a ramp of one block from what the previous block was
    /// given, and every block advances it, whether or not the node was called
    /// (`step`): a move made while the node was parked has arrived when it is
    /// heard.
    #[test]
    fn the_mix_advances_every_block_called_or_not() {
        let (cell, mut state) = at(&shape(0), 1.0);
        cell.store_mix(Mix::new(0.25));
        state.snapshot();
        state.snapshot(); // a block the node was not called in
        assert_eq!(state.step(64).mix.state(), MixState::Between);
    }
}
