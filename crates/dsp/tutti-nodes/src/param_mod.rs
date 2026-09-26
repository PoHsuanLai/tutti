//! Audio-rate param modulation units — the per-sample tier of the modulation
//! matrix.
//!
//! Three small `AudioUnit`s that materialize an audio-rate modulation edge:
//!
//! ```text
//! source ─► ParamShaperNode ─► ParamSumNode ─► (param-input port on target)
//!           (depth·polarity·    (base + Σ mods,
//!            curve via LUT)      one clamp)
//! ```
//!
//! The base port is fed by an [`AtomicSourceNode`] holding the authored value,
//! so the UI/automation handle path is unchanged: the same atomic a control-rate
//! [`AtomicTarget`](tutti_mod::AtomicTarget) mirrors into becomes this chain's
//! base. That is what makes the two tiers compose rather than compete — one
//! base, both tiers, no second accumulator.
//!
//! **Build an edge with [`wire_param_mod`]** (or [`build_param_mod`] if you own
//! your wiring), not by assembling the three units by hand. The invariant above
//! is a property of *how they are connected and whose cell the base is*, so it
//! is one an assembler can hold and loose units cannot. The returned
//! [`ParamModChain::base_cell`] is that cell; a caller that mints a private one
//! instead gets a param frozen at its construction value, silently.
//!
//! All three are RT-safe: no allocation and no locks in `tick`/`process` — only
//! atomic loads, arithmetic, and read-only LUT lookups.

use std::sync::Arc;

use tutti_core::dsp::Net;
use tutti_core::dsp::NodeId;
use tutti_core::Signal;
use tutti_core::{AtomicF32, AudioUnit, BufferMut, BufferRef, Ordering, SignalFrame, Tail};
use tutti_mod::{shape, CurveType, Polarity};

/// LUT resolution for [`ParamShaperNode`]. 256 points + linear interpolation is
/// inaudibly smooth for a control-shaping curve and keeps the table in L1.
const LUT_N: usize = 256;

/// Maps a raw modulation signal to a shaped offset via `depth · polarity ·
/// curve`, baked into a fixed LUT at construction so `tick`/`process` are
/// branch-light (normalize → table lookup → lerp).
///
/// Input domain is `[-1, 1]` (bipolar CV or a normalized audio signal). The
/// output is the additive offset [`ParamSumNode`] adds onto the base.
///
/// The shaping is [`tutti_mod::shape`] — the *same* function the control-rate
/// path applies in `ModPreFrame::run`. Baking it into a LUT here is a
/// performance decision, not a second implementation: both tiers agree on
/// values because they call one function.
#[derive(Clone)]
pub struct ParamShaperNode {
    lut: Arc<[f32; LUT_N]>,
}

impl ParamShaperNode {
    /// Bakes `depth`, `polarity` and `curve` into a lookup table over the
    /// modulator's `[-1, 1]` output range.
    ///
    /// **The shaping is fixed at construction — there is no setter.** A route
    /// whose depth or curve changes needs a *new* unit, which is why
    /// [`ParamModShaping`] derives `PartialEq`: comparing the declaration
    /// against what the node was built from is the only way a reconciler can
    /// see that a depth slider moved.
    ///
    /// Allocates the table, so build it off the audio thread.
    pub fn new(depth: impl Into<tutti_types::Depth>, polarity: Polarity, curve: CurveType) -> Self {
        let depth = depth.into();
        let mut lut = [0.0_f32; LUT_N];
        for (i, slot) in lut.iter_mut().enumerate() {
            // Map LUT index → input x ∈ [-1, 1].
            let x = (i as f32 / (LUT_N - 1) as f32) * 2.0 - 1.0;
            *slot = shape(x, depth, polarity, curve);
        }
        Self { lut: Arc::new(lut) }
    }

    /// Shaped offset for input `x`, linearly interpolated between LUT points.
    #[inline]
    fn eval(&self, x: f32) -> f32 {
        let pos = ((x.clamp(-1.0, 1.0) + 1.0) * 0.5) * (LUT_N - 1) as f32;
        let i = pos.floor() as usize;
        if i >= LUT_N - 1 {
            return self.lut[LUT_N - 1];
        }
        let frac = pos - i as f32;
        self.lut[i] + (self.lut[i + 1] - self.lut[i]) * frac
    }
}

impl AudioUnit for ParamShaperNode {
    fn inputs(&self) -> usize {
        1
    }
    fn outputs(&self) -> usize {
        1
    }

    fn reset(&mut self) {}

    #[inline]
    fn tick(&mut self, input: &[f32], output: &mut [f32]) {
        output[0] = self.eval(input[0]);
    }

    fn process(&mut self, size: usize, input: &BufferRef, output: &mut BufferMut) {
        for i in 0..size {
            output.set_f32(0, i, self.eval(input.at_f32(0, i)));
        }
    }

    fn route(&mut self, _input: &SignalFrame, _frequency: f64) -> SignalFrame {
        let mut output = SignalFrame::new(1);
        output.set(0, Signal::Latency(0.0));
        output
    }

    fn get_id(&self) -> u64 {
        crate::node_id::PARAM_SHAPER_ID
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
    /// Control-rate and stateless: it stops with its input.
    fn tail(&mut self) -> Tail {
        Tail::None
    }

    fn footprint(&self) -> usize {
        std::mem::size_of::<Self>()
    }
}

/// The audio-rate analog of a control-rate accumulator's `final_value`: `base +
/// Σ offsets`, one clamp, per sample.
///
/// Port 0 is the base (authored + automation + any control-rate modulation,
/// arriving as one already-summed scalar); ports `1..=N` are shaped modulation
/// offsets. `mods == 0` passes the base through clamped.
///
/// This is the node that makes fan-in representable at all: `Net` holds one
/// source per input port and has no summing bus, so summing is a node's job.
///
/// # The bounds are live; the arity is not
///
/// `min`/`max` are shared atomics a control thread can move, because a param's
/// range is authored state that changes without the *graph* changing — a host
/// re-declaring a narrower range must not have to rebuild the chain to apply
/// it. [`bounds`](Self::bounds) hands out the handle.
///
/// `mods` stays a plain field: it is the node's **input arity**, so changing it
/// changes the node's shape and a rebuild is the only honest answer. That is
/// the real line between these two — not "scalar vs not", but whether the value
/// is something `Net` has already wired against.
///
/// Bare `f32`, not `Param<U>`: a bound is in the modulated param's own units,
/// which this node is deliberately erased over (the same erasure `LayeredCurve`
/// makes on the control-rate side). There is no single `U` to name.
#[derive(Clone)]
pub struct ParamSumNode {
    mods: usize,
    bounds: Arc<ClampBounds>,
}

/// A [`ParamSumNode`]'s live clamp range.
///
/// One allocation holding both halves, so a host that moves a range moves it
/// atomically-enough: the two stores are still independent, but they share a
/// cache line and a handle, and no reader can see a bound from a *different*
/// chain. Crossed bounds — a `min` above its `max` — are handled where the sum
/// is folded, not rejected here.
#[derive(Debug)]
pub struct ClampBounds {
    min: AtomicF32,
    max: AtomicF32,
}

impl ClampBounds {
    /// Set both halves. Control thread only.
    pub fn set(&self, min: f32, max: f32) {
        self.min.store(min, Ordering::Release);
        self.max.store(max, Ordering::Release);
    }

    /// The current `(min, max)`.
    pub fn get(&self) -> (f32, f32) {
        (
            self.min.load(Ordering::Acquire),
            self.max.load(Ordering::Acquire),
        )
    }
}

impl ParamSumNode {
    /// A summing node folding a base value plus `mods` modulation inputs,
    /// clamped to `min..=max`.
    ///
    /// The bounds are the target parameter's own range, so a stack of
    /// modulators cannot drive it outside what the parameter accepts. Unlike
    /// the shaping, they are live — see
    /// [`bounds`](Self::bounds) for control-thread writes.
    pub fn new(mods: usize, min: f32, max: f32) -> Self {
        Self {
            mods,
            bounds: Arc::new(ClampBounds {
                min: AtomicF32::new(min),
                max: AtomicF32::new(max),
            }),
        }
    }

    /// The shared clamp bounds — clone for control-thread writes.
    ///
    /// The audio-rate counterpart of moving a control-rate accumulator's
    /// `(min, max)`: a host whose authored range changed writes here rather
    /// than rebuilding the node.
    pub fn bounds(&self) -> Arc<ClampBounds> {
        Arc::clone(&self.bounds)
    }

    /// The bounds, ordered so `clamp` cannot panic.
    ///
    /// `f32::clamp` panics if `min > max`, and the two stores in
    /// [`ClampBounds::set`] are independent — a reader can land between them
    /// and see a crossed pair for one block. Ordering the pair costs one
    /// comparison and makes that unrepresentable, which is worth more on the
    /// audio thread than a panic would be informative.
    #[inline]
    fn ordered_bounds(&self) -> (f32, f32) {
        let (min, max) = self.bounds.get();
        (min.min(max), max.max(min))
    }

    #[inline]
    fn fold(bounds: (f32, f32), base: f32, offsets: impl Iterator<Item = f32>) -> f32 {
        (base + offsets.sum::<f32>()).clamp(bounds.0, bounds.1)
    }
}

impl AudioUnit for ParamSumNode {
    fn inputs(&self) -> usize {
        1 + self.mods
    }
    fn outputs(&self) -> usize {
        1
    }

    /// Detach the clamp bounds, keeping their current values, so a fork
    /// clamps to the range it was taken with, not one moved while it runs.
    fn isolate(&mut self) {
        let (min, max) = self.bounds.get();
        self.bounds = Arc::new(ClampBounds {
            min: AtomicF32::new(min),
            max: AtomicF32::new(max),
        });
    }

    fn reset(&mut self) {}

    #[inline]
    fn tick(&mut self, input: &[f32], output: &mut [f32]) {
        let bounds = self.ordered_bounds();
        output[0] = Self::fold(bounds, input[0], input[1..=self.mods].iter().copied());
    }

    fn process(&mut self, size: usize, input: &BufferRef, output: &mut BufferMut) {
        // Read the bounds **once per block**, like `AtomicSourceNode` reads its
        // value: a range cannot meaningfully change mid-block, and two atomic
        // loads per sample is pure cost on the hottest loop in the chain.
        let bounds = self.ordered_bounds();
        for i in 0..size {
            let base = input.at_f32(0, i);
            let offsets = (1..=self.mods).map(|p| input.at_f32(p, i));
            output.set_f32(0, i, Self::fold(bounds, base, offsets));
        }
    }

    fn route(&mut self, _input: &SignalFrame, _frequency: f64) -> SignalFrame {
        let mut output = SignalFrame::new(1);
        output.set(0, Signal::Latency(0.0));
        output
    }

    fn get_id(&self) -> u64 {
        crate::node_id::PARAM_SUM_ID
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
    /// Control-rate and stateless: it stops with its input.
    fn tail(&mut self) -> Tail {
        Tail::None
    }

    fn footprint(&self) -> usize {
        std::mem::size_of::<Self>()
    }
}

/// A settable constant source: 0 inputs, 1 output, value held in a shared
/// atomic.
///
/// Feeds [`ParamSumNode`]'s base port so the existing UI/automation handle path
/// keeps working — it reads the same atomic a control-rate `AtomicTarget`
/// already mirrors into. (fundsp's `dc()` covers a *fixed* constant; this is the
/// settable equivalent.)
#[derive(Clone)]
pub struct AtomicSourceNode {
    value: Arc<AtomicF32>,
}

impl AtomicSourceNode {
    /// A source over a **private** cell, reachable only through
    /// [`shared`](Self::shared) on this value.
    ///
    /// Right for a base nothing else writes — a constant, a test fixture. For a
    /// param that is a **modulation target**, prefer [`over`](Self::over) with
    /// the cell a control-rate `AtomicTarget` mirrors into, or let
    /// [`build_param_mod`] hand you the whole chain: a private cell means the
    /// control-rate tier and the audio-rate base are two different atomics, and
    /// the authored value silently stops moving.
    ///
    /// If you keep `new`, keep the handle. Calling this and dropping the value
    /// is what freezes a param at its construction value.
    pub fn new(initial: f32) -> Self {
        Self {
            value: Arc::new(AtomicF32::new(initial)),
        }
    }

    /// Build one over an *existing* atomic — the handle a control-rate
    /// `AtomicTarget` mirrors into, so the two tiers share one base cell.
    pub fn over(value: Arc<AtomicF32>) -> Self {
        Self { value }
    }

    /// The shared atomic — clone it for control-thread writes.
    pub fn shared(&self) -> Arc<AtomicF32> {
        Arc::clone(&self.value)
    }
}

impl AudioUnit for AtomicSourceNode {
    fn inputs(&self) -> usize {
        0
    }
    fn outputs(&self) -> usize {
        1
    }

    /// Detach the base cell, keeping its current value, so a fork emits the
    /// base it was taken with, not one the control tier moves while it runs.
    fn isolate(&mut self) {
        self.value = Arc::new(AtomicF32::new(self.value.load(Ordering::Acquire)));
    }

    fn reset(&mut self) {}

    #[inline]
    fn tick(&mut self, _input: &[f32], output: &mut [f32]) {
        output[0] = self.value.load(Ordering::Acquire);
    }

    fn process(&mut self, size: usize, _input: &BufferRef, output: &mut BufferMut) {
        // Read once per block: the value cannot change mid-block in any way a
        // consumer could rely on, and a per-sample atomic load is pure cost.
        let v = self.value.load(Ordering::Acquire);
        for i in 0..size {
            output.set_f32(0, i, v);
        }
    }

    fn route(&mut self, _input: &SignalFrame, _frequency: f64) -> SignalFrame {
        let mut output = SignalFrame::new(1);
        output.set(0, Signal::Latency(0.0));
        output
    }

    fn get_id(&self) -> u64 {
        crate::node_id::ATOMIC_SOURCE_ID
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
    /// Control-rate and stateless: it stops with its input.
    fn tail(&mut self) -> Tail {
        Tail::None
    }

    fn footprint(&self) -> usize {
        std::mem::size_of::<Self>()
    }
}

/// One materialised audio-rate param edge: its nodes, and the cell that owns
/// its base.
///
/// Returned by [`build_param_mod`] and [`wire_param_mod`]. The node ids are for
/// a host that needs to wire or retire them; [`base_cell`](Self::base_cell) is
/// the part that is easy to lose and expensive to lose.
pub struct ParamModChain {
    /// The [`AtomicSourceNode`] feeding the sum's base port.
    pub base: NodeId,
    /// The [`ParamSumNode`]: `base + Σ offsets`, clamped once.
    pub sum: NodeId,
    /// One [`ParamShaperNode`] per edge, in the order their offsets occupy the
    /// sum's ports (`1..=N`).
    pub shapers: Vec<NodeId>,
    base_cell: Arc<AtomicF32>,
    bounds: Arc<ClampBounds>,
}

impl ParamModChain {
    /// The cell the sum's base port reads — **this chain's single base owner**.
    ///
    /// Hand it to
    /// [`AtomicTarget::with_mirror`](tutti_mod::AtomicTarget::with_mirror) and
    /// the control-rate tier writes the same cell the audio-rate sum adds its
    /// offsets onto: one base, both tiers, no second accumulator. An authored
    /// write (a fader, a document edit) stores here directly.
    ///
    /// # Why this is worth a named accessor rather than a public field
    ///
    /// Because dropping it is silent. A node whose param port is wired **never
    /// reads its own atomic** — `param_writer_ownership`'s
    /// `a_wired_param_port_makes_the_node_ignore_its_atomic` pins that — so a
    /// caller that mints a private cell instead ([`AtomicSourceNode::new`])
    /// gets code that compiles, runs, renders, and freezes the authored value
    /// at whatever it was when the chain was built. Every later write lands
    /// somewhere nothing reads.
    ///
    /// That is not hypothetical: it is what `bevy-tutti`'s `spawn_chain` did,
    /// and it presented as "the cutoff knob does nothing", not as an error.
    pub fn base_cell(&self) -> Arc<AtomicF32> {
        Arc::clone(&self.base_cell)
    }

    /// The sum's live clamp range.
    ///
    /// Held for the same reason as [`base_cell`](Self::base_cell): a param's
    /// range is authored state that can move without the graph moving, and a
    /// host that has to rebuild the chain to apply a new range would lose the
    /// base along with it.
    pub fn bounds(&self) -> Arc<ClampBounds> {
        Arc::clone(&self.bounds)
    }
}

/// How one edge turns a raw `[-1, 1]` signal into an offset.
///
/// A tuple would do, but three positional near-interchangeable values (a float
/// newtype and two enums) is exactly the shape that gets mis-ordered.
///
/// **Deliberately carries no source node.** Shaping is a property of the edge;
/// *what drives it* is a wiring question, and the two have different owners —
/// [`wire_param_mod`] takes the sources alongside, while a declarative host
/// ([`build_param_mod`]) never tells this crate its sources at all. Putting a
/// `NodeId` here would force that host to invent one.
///
/// `PartialEq` is load-bearing rather than a convenience derive.
/// [`ParamShaperNode`] bakes these three into a LUT at construction and exposes
/// no setter, so the only way a reconciler can notice a route's shaping has
/// moved is to compare what the declaration says against what the node was
/// built from. Without that comparison a depth slider — which changes no node
/// *count* — is invisible to a reconciler keyed on shape alone.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ParamModShaping {
    /// How far the modulator swings the target, as a [`Depth`](tutti_types::Depth)
    /// fraction of the parameter's range. `0.0` is inert.
    pub depth: tutti_types::Depth,
    /// Whether the modulator's `[-1, 1]` output is applied bipolar (both
    /// directions from the base) or folded to unipolar (one direction only).
    pub polarity: Polarity,
    /// The response curve mapping the modulator's output onto the target —
    /// linear, exponential, and so on.
    pub curve: CurveType,
}

impl ParamModShaping {
    /// This edge's shaping as the graph's fused param step reads it:
    /// [`tutti_mod::shape`] baked into a [`ShapeLut`](tutti_graph::ShapeLut)
    /// over the modulator's `[-1, 1]`. Allocates the table: build it on the
    /// control thread.
    pub fn shaping(&self) -> tutti_graph::ParamShaping {
        let Self {
            depth,
            polarity,
            curve,
        } = *self;
        tutti_graph::ParamShaping::Lut(tutti_graph::ShapeLut::from_fn(move |x| {
            shape(x, depth, polarity, curve)
        }))
    }
}

/// One node of a [`ParamModParts`], named by its role in the chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ParamModPart {
    /// The [`AtomicSourceNode`] holding the authored base.
    Base,
    /// The [`ParamSumNode`]: `base + Σ offsets`, clamped once.
    Sum,
    /// The `i`th [`ParamShaperNode`], in edge order.
    Shaper(usize),
}

/// One edge *inside* an audio-rate param chain: output 0 of `from` feeds input
/// `port` of `to`.
///
/// Only the chain's own edges. What feeds each shaper and which sink port the
/// sum drives are the caller's wiring — see [`ParamModShaping`] for why the
/// chain never learns its sources.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ParamModEdge {
    /// The node whose output 0 is the signal.
    pub from: ParamModPart,
    /// The node that reads it.
    pub to: ParamModPart,
    /// `to`'s input port.
    pub port: usize,
}

/// An audio-rate param edge's nodes, **not yet in any graph**, with the edges
/// between them and the handles that outlive them.
///
/// The graph-agnostic half of [`build_param_mod`]: the owned units, the chain's
/// internal edges as data ([`edges`](Self::edges)), and the two cells a host has
/// to keep ([`base_cell`](Self::base_cell), [`bounds`](Self::bounds)). A caller
/// inserts the units into whatever graph it drives and wires the edges there;
/// [`insert_into`](Self::insert_into) is that step for a `Net`.
///
/// The handles are taken at construction, before any unit moves, so they cannot
/// be lost by inserting the units.
pub struct ParamModParts {
    /// Feeds the sum's port 0 ([`ParamModPart::Base`]).
    pub base: AtomicSourceNode,
    /// `base + Σ offsets`, clamped ([`ParamModPart::Sum`]).
    pub sum: ParamSumNode,
    /// One per edge, in the order their offsets occupy the sum's ports `1..=N`
    /// ([`ParamModPart::Shaper`]).
    pub shapers: Vec<ParamShaperNode>,
    base_cell: Arc<AtomicF32>,
    bounds: Arc<ClampBounds>,
}

impl ParamModParts {
    /// The cell the sum's base port reads — see
    /// [`ParamModChain::base_cell`], which is this same cell once inserted.
    pub fn base_cell(&self) -> Arc<AtomicF32> {
        Arc::clone(&self.base_cell)
    }

    /// The sum's live clamp range — see [`ParamModChain::bounds`].
    pub fn bounds(&self) -> Arc<ClampBounds> {
        Arc::clone(&self.bounds)
    }

    /// The chain's internal edges: the base into the sum's port 0, and shaper
    /// `i` into the sum's port `i + 1`.
    ///
    /// Port 0 is the base and offsets start at 1; getting that off by one
    /// overwrites the base with the first offset, so it is stated once, here,
    /// rather than at every caller.
    pub fn edges(&self) -> impl Iterator<Item = ParamModEdge> + '_ {
        std::iter::once(ParamModEdge {
            from: ParamModPart::Base,
            to: ParamModPart::Sum,
            port: 0,
        })
        .chain((0..self.shapers.len()).map(|i| ParamModEdge {
            from: ParamModPart::Shaper(i),
            to: ParamModPart::Sum,
            port: i + 1,
        }))
    }

    /// Push every unit into `net`, **unwired**, and return their ids with the
    /// handles.
    ///
    /// The `Net` adapter. It makes no connections — not even
    /// [`edges`](Self::edges) — because a declarative host wires them itself
    /// (see [`build_param_mod`]).
    pub fn insert_into(self, net: &mut Net) -> ParamModChain {
        let base = net.push(Box::new(self.base));
        let sum = net.push(Box::new(self.sum));
        let shapers = self
            .shapers
            .into_iter()
            .map(|shaper| net.push(Box::new(shaper)))
            .collect();
        ParamModChain {
            base,
            sum,
            shapers,
            base_cell: self.base_cell,
            bounds: self.bounds,
        }
    }
}

/// Build an audio-rate param edge's nodes as owned parts, in no graph.
///
/// For a caller that inserts into its own graph. `base`, `min` and `max` are the
/// param's authored value and bounds in its own units. They stay bare `f32`:
/// the sum is unit-erased by construction (see [`ParamSumNode`]), matching
/// `LayeredCurve`'s own erasure on the control-rate side.
pub fn param_mod_parts(base: f32, min: f32, max: f32, edges: &[ParamModShaping]) -> ParamModParts {
    let base_unit = AtomicSourceNode::new(base);
    // Taken *before* the unit can move anywhere — this handle is the whole
    // point of the return value.
    let base_cell = base_unit.shared();
    let sum = ParamSumNode::new(edges.len(), min, max);
    let bounds = sum.bounds();
    let shapers = edges
        .iter()
        .map(|e| ParamShaperNode::new(e.depth, e.polarity, e.curve))
        .collect();
    ParamModParts {
        base: base_unit,
        sum,
        shapers,
        base_cell,
        bounds,
    }
}

/// Create an audio-rate param edge's nodes in `net`, **unwired**.
///
/// For a host that owns its own wiring. `bevy-tutti` is one: it declares edges
/// as `PortSources` components and diffs them against `Net` each frame, so an
/// edge `connect`ed behind the reconciler's back is reverted on the next pass.
/// Such a host wants the nodes and the base cell, and makes the connections
/// itself.
///
/// [`param_mod_parts`] followed by [`ParamModParts::insert_into`]; a host whose
/// graph is not a `Net` uses the first half alone.
///
/// A host driving `Net` directly wants [`wire_param_mod`], which is this plus
/// the connections.
pub fn build_param_mod(
    net: &mut Net,
    base: f32,
    min: f32,
    max: f32,
    edges: &[ParamModShaping],
) -> ParamModChain {
    param_mod_parts(base, min, max, edges).insert_into(net)
}

/// Create an audio-rate param edge and connect it: `base + Σ shaped(source) →
/// sink.port`.
///
/// The whole edge, for a host driving `Net` directly. `port` is the sink's
/// param-input port, from
/// [`ParamPorts::param_port`](crate::ParamPorts::param_port).
///
/// # `connect`, never `pipe_input`
///
/// `pipe_input` walks *every* input port of the sink, so it overwrites the
/// param edge this just made — silently, since the graph stays valid and the
/// param simply reverts to whatever the audio bus carries.
/// `param_port_is_clobbered_by_pipe_input` pins that hazard. Wire a ported
/// node's audio inputs with explicit `connect_input` calls.
pub fn wire_param_mod(
    net: &mut Net,
    sink: NodeId,
    port: usize,
    base: f32,
    min: f32,
    max: f32,
    edges: &[(NodeId, ParamModShaping)],
) -> ParamModChain {
    let shaping: Vec<ParamModShaping> = edges.iter().map(|&(_, s)| s).collect();
    let parts = param_mod_parts(base, min, max, &shaping);
    // Collected before `insert_into` consumes the parts.
    let internal: Vec<ParamModEdge> = parts.edges().collect();
    let chain = parts.insert_into(net);

    let id = |part: ParamModPart| match part {
        ParamModPart::Base => chain.base,
        ParamModPart::Sum => chain.sum,
        ParamModPart::Shaper(i) => chain.shapers[i],
    };
    for edge in internal {
        net.connect(id(edge.from), 0, id(edge.to), edge.port);
    }
    for (&(source, _), &shaper) in edges.iter().zip(chain.shapers.iter()) {
        net.connect(source, 0, shaper, 0);
    }
    net.connect(chain.sum, 0, sink, port);

    chain
}
