//! Bypass and mute as graph state (doc 013, "Bypass and mute are graph state"):
//! [`NodeMode`], the cell a mode is stored in, and the state the executor
//! keeps to apply one.
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

use tutti_types::Samples;

use crate::NodeKey;
use crate::arena::Arena;
use crate::fade::CrossfadeCurve;
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

    /// The weights a steady mode holds.
    fn weights(self) -> Weights {
        match self {
            Self::Active => Weights::ACTIVE,
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

/// A unit's mode, shared between the editor (the one writer) and the unit
/// (the executor reads it once per block). An `AtomicU8`: a mode is a
/// control, not a rewire.
#[derive(Clone, Debug)]
pub(crate) struct ModeCell(Arc<AtomicU8>);

impl ModeCell {
    /// A cell holding `mode`. Control side: allocates.
    pub(crate) fn new(mode: NodeMode) -> Self {
        Self(Arc::new(AtomicU8::new(mode.to_bits())))
    }

    /// Store `mode`. Wait-free.
    pub(crate) fn store(&self, mode: NodeMode) {
        self.0.store(mode.to_bits(), Ordering::Release);
    }

    /// The mode now. Wait-free.
    #[inline]
    pub(crate) fn load(&self) -> NodeMode {
        NodeMode::from_bits(self.0.load(Ordering::Acquire))
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
    pub(crate) const ACTIVE: Self = Self {
        node: 1.0,
        input: 0.0,
    };
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
/// input alone, then the frames of `ramp`, then `steady`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Segments {
    pub(crate) warm: usize,
    pub(crate) ramp: Option<RampRun>,
    pub(crate) steady: NodeMode,
}

/// The part of a fade a block runs.
#[derive(Clone, Copy, Debug)]
pub(crate) struct RampRun {
    from: Weights,
    to: Weights,
    /// The fade's frame the block's first ramp frame is.
    k: usize,
    len: usize,
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
    /// leave or fill, never multiply.
    pub(crate) fn blend(&self, out: &mut [f32], x: Option<&[f32]>) {
        let put = |out: &mut [f32], range: std::ops::Range<usize>| match x {
            Some(x) => out[range.clone()].copy_from_slice(&x[range]),
            None => out[range].fill(0.0),
        };
        put(out, 0..self.warm);
        let mut at = self.warm;
        if let Some(r) = self.ramp {
            let node_on = r.from.node != 0.0 || r.to.node != 0.0;
            let input_on = x.is_some() && (r.from.input != 0.0 || r.to.input != 0.0);
            for j in 0..r.len {
                let w = r.from.toward(r.to, gain(r.k + j));
                let i = at + j;
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
            NodeMode::Active => {}
            NodeMode::Muted => out[at..].fill(0.0),
            NodeMode::Bypassed => put(out, at..out.len()),
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
        let main = main_channels(shape);
        Self {
            cell,
            seen: mode,
            plain: false,
            heading: mode,
            weights: mode.weights(),
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
        self.plain = self.inert
            || (self.seen == NodeMode::Active
                && self.heading == NodeMode::Active
                && self.ramp.is_none()
                && self.warm == 0
                && self.line.is_none()
                && self.held_in.is_empty());
    }

    /// Whether this block's op is the plain one: steady Active with nothing
    /// to feed or track, or nothing a mode could act on.
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
        self.ramp = (self.weights != want.weights()).then_some(Ramp {
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
            || self
                .ramp
                .is_some_and(|r| r.from.input != 0.0 || self.heading.weights().input != 0.0)
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
        let mut seg = Segments {
            warm,
            ramp: None,
            steady: self.heading,
        };
        if warm == frames {
            return seg;
        }
        if let Some(r) = &mut self.ramp {
            let to = self.heading.weights();
            let len = (MODE_DECLICK.get() - r.k).min(frames - warm);
            seg.ramp = Some(RampRun {
                from: r.from,
                to,
                k: r.k,
                len,
            });
            r.k += len;
            self.weights = r.from.toward(to, gain(r.k - 1));
            if r.k == MODE_DECLICK.get() {
                self.ramp = None;
                self.weights = to;
            }
        }
        seg
    }
}
