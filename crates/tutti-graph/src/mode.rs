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

use tutti_types::{AtomicF32, Mix, Samples};

use crate::NodeKey;
use crate::arena::Arena;
use crate::fade::CrossfadeCurve;
use crate::gain::ramp_at;
use crate::held::Held;
use crate::kernels::AudioRing;
use crate::node::Shape;

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
}

impl std::fmt::Display for ModeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoSuchNode { node } => write!(f, "no node {} to set the mode of", node.0),
        }
    }
}

impl std::error::Error for ModeError {}

/// A node's [`Mix`] as the spec holds it (doc 013, "Bypass and mute are graph
/// state", M4): the blend of the node's output with its aligned input,
/// `out = y * m + x̂ * (1 - m)`. [`Mix::WET`] (all of the node's output) is
/// the canonical absent entry of [`GraphSpec::mixes`](crate::GraphSpec::mixes).
///
/// Compared and hashed by bits, as [`ParamRange`](crate::ParamRange) is, so a
/// spec holding one stays `Eq + Hash`.
#[derive(Clone, Copy, Debug)]
pub struct NodeMix {
    /// The blend position: finite and in `0..=1`
    /// ([`GraphInvalid::BadMix`](crate::GraphInvalid::BadMix)).
    pub mix: Mix,
}

impl NodeMix {
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
}

impl std::fmt::Display for MixError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoSuchNode { node } => write!(f, "no node {} to set the mix of", node.0),
            Self::OutOfRange => write!(f, "a mix must be in 0..=1"),
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
}

impl ModeCell {
    /// A cell holding `mode` and `mix`. Control side: allocates.
    pub(crate) fn new(mode: NodeMode, mix: Mix) -> Self {
        Self {
            mode: Arc::new(AtomicU8::new(mode.to_bits())),
            mix: Arc::new(AtomicF32::new(mix.get())),
        }
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
/// ([`ramp_at`], as an edge's gain ramps).
#[derive(Clone, Copy, Debug)]
pub(crate) struct MixRun {
    from: f32,
    to: f32,
    frames: usize,
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
        self.from.to_bits() == 1.0f32.to_bits() && self.to.to_bits() == 1.0f32.to_bits()
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

    /// Whether the mix does not move across the block.
    fn is_static(&self) -> bool {
        self.from.to_bits() == self.to.to_bits()
    }

    /// The mix at frame `i` of the block.
    #[inline]
    fn at(&self, i: usize) -> f32 {
        if self.is_static() {
            self.to
        } else {
            ramp_at(self.from, self.to, i, self.frames as f32)
        }
    }

    /// The mix of the block's last frame (a ramp's last sample, which
    /// `from + (to - from) * 1.0` need not make exactly `to`).
    fn last(&self) -> f32 {
        self.at(self.frames - 1)
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
    pub(crate) fn blend(&self, out: &mut [f32], x: Option<&[f32]>) {
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
                let to = r.heading.weights(self.mix.at(i));
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
            NodeMode::Active => self.blend_active(out, x, at),
            NodeMode::Muted => out[at..].fill(0.0),
            NodeMode::Bypassed => put(out, at..out.len()),
        }
    }

    /// The steady Active frames from `at` on, against the block's mix.
    fn blend_active(&self, out: &mut [f32], x: Option<&[f32]>, at: usize) {
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
            let w = m.at(i);
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
        let main = main_channels(shape);
        (!latency.is_zero() && main > 0).then(|| Self {
            rings: (0..main).map(|_| AudioRing::new(latency)).collect(),
        })
    }
}

/// The channels a bypass passes: output `c` is input `c` for `c` below the
/// narrower width (`Status::Bypass`'s rule).
fn main_channels(shape: &Shape) -> usize {
    shape.audio_in.count().min(shape.audio_out.count()) as usize
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
    main: usize,
    line: Option<BypassLine>,
    /// The aligned input of a block that needs it away from the node's own
    /// slots: one slot per passed channel.
    scratch: Option<Arena>,
    /// What each event input has been sent and not ended, to close when the
    /// node is bypassed.
    held_in: Vec<Held>,
}

impl ModeState {
    /// The state of a unit about to be placed: steady in `cell`'s mode, with
    /// no fade (a fork, a new unit). Control side: allocates its line, its
    /// scratch and its tables.
    pub(crate) fn new(cell: ModeCell, shape: &Shape, max_block: usize) -> Self {
        let mode = cell.load();
        let mix = cell.load_mix().get();
        let main = main_channels(shape);
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
            inert: shape.audio_out.count() == 0 && shape.event_out == 0,
            latency: shape.latency.samples().get(),
            main,
            line: BypassLine::for_shape(shape),
            scratch: (main > 0).then(|| Arena::new(main, max_block)),
            held_in: (0..shape.event_in).map(|_| Held::new()).collect(),
        }
    }

    /// Read the cell, and decide which op the block runs: once per block,
    /// before the first op, so every op of the block sees one mode. (The
    /// state moves only inside the unit's own op, after that decision.)
    #[inline]
    pub(crate) fn snapshot(&mut self) {
        self.seen = self.cell.load();
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
                && self.held_in.is_empty());
    }

    /// The block's mix: the previous block's to this one's, across `frames`.
    fn mix_run(&self, frames: usize) -> MixRun {
        MixRun {
            from: self.mix_from,
            to: self.mix_to,
            frames,
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

    /// Channels the aligned input has.
    pub(crate) fn main(&self) -> usize {
        self.main
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

    /// Channel `c`'s aligned input, as [`align`](Self::align) left it.
    pub(crate) fn aligned(&self, c: usize, frames: usize) -> &[f32] {
        self.scratch
            .as_ref()
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
            let to = self.heading.weights(mix.at(warm + len - 1));
            self.weights = r.from.toward(to, gain(r.k - 1));
            if r.k == MODE_DECLICK.get() {
                self.ramp = None;
            }
        }
        if self.ramp.is_none() {
            self.weights = self.heading.weights(mix.last());
        }
        seg
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_types::{ChannelLayout, Latency};

    fn shape(latency: usize) -> Shape {
        Shape::audio(ChannelLayout::MONO, ChannelLayout::MONO)
            .with_latency(Latency::new(Samples(latency)))
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
    ///
    /// Mutation (each run, each failed): `snapshot` leaving `plain` without the
    /// `mix_run(0).is_wet()` term -> the plain node is plain off WET;
    /// `needs_input` without its mix term -> a blend has no `x̂`; `is_wet`
    /// comparing only `to` -> the ramp block back at WET is plain and the last
    /// assertion fails.
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

    /// A moving mix is a ramp of one block from what the previous block was
    /// given, and every block advances it, whether or not the node was called
    /// (`step`): a move made while the node was parked has arrived when it is
    /// heard.
    ///
    /// Mutation: advancing `mix_from` in `step` rather than `snapshot` -> the
    /// parked blocks never advance and the block after them ramps.
    #[test]
    fn the_mix_advances_every_block_called_or_not() {
        let (cell, mut state) = at(&shape(0), 1.0);
        cell.store_mix(Mix::new(0.25));
        state.snapshot();
        state.snapshot(); // a block the node was not called in
        assert_eq!(state.step(64).mix.state(), MixState::Between);
    }
}
