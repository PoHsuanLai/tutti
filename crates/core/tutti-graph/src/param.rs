//! Compiler-owned parameter modulation (design doc 013, "Rewrite order"
//! item 6).
//!
//! A node declares the params it lets the graph modulate
//! ([`Shape::params`](crate::Shape::params), by [`UnitParam`] id). A
//! [`GraphSpec`](crate::GraphSpec) can then drive any of them from other
//! nodes' outputs ([`GraphSpec::connect_param`](crate::GraphSpec::connect_param)),
//! and the compiler turns each modulated param into one fused step of the
//! node's op:
//!
//! ```text
//!   value[i] = clamp(base_ramp[i] + Σ shape_j(source_j[i]), range)
//! ```
//!
//! - **The base** is the node's own control — a `Param<U>` it hands out in
//!   its `Controls` — read once per block through
//!   [`Node::param_base`](crate::Node::param_base) and ramped linearly
//!   across the block, landing exactly on the new value at its last frame,
//!   so a fader move under modulation does not zipper.
//! - **The offsets** come from audio outputs ([`ParamFrom::Audio`], one
//!   value per frame) or from [`ParamRamp`](crate::ParamRamp) events
//!   ([`ParamFrom::Events`], a sample-accurate ramp per source), each through
//!   its own [`ParamShaping`]: the identity, or a [`ShapeLut`] (the
//!   depth · polarity · curve table `tutti_mod::shape` bakes). They are summed
//!   in source order and clamped once to the port's [`ParamRange`].
//! - **An unconnected param resolves to its base, never to 0.** A port with
//!   no source this block reads [`ParamInput::Base`], and the node uses its
//!   own control, exactly as if nothing could modulate it: the fast path
//!   costs one branch, and nothing is copied. Under `Net` an unconnected
//!   input read 0, which is why a param port had to be fixed when a node was
//!   built and fed by a base chain (`AtomicSourceNode` → `ParamSumNode`) from
//!   birth. Here a port can be connected and disconnected by any commit.
//! - **Connecting or disconnecting is declicked.** When a port's sources
//!   change (a new source, one gone, a new shaping), its output crossfades
//!   from where it was — the last value it delivered, or the base — to the
//!   new value over [`PARAM_DECLICK`] frames. Nothing else is smoothed: a
//!   step in a modulator lands on its frame (the sample-accuracy contract,
//!   doc 013 §6). A unit's **first** block is not a change: a unit placed by
//!   a commit (an insert, a hard replace, a fork, a re-prepare's resume)
//!   starts at its modulated value, as its audio starts at its first frame.
//! - **A source's state is the source's, not its slot's.** An event
//!   source's ramp (the value it holds, and a ramp under way) is kept by
//!   [`ParamFrom`], so adding or removing *another* source, or reshaping
//!   this one, does not reset it. A PDC delay that appears on an audio
//!   source (the node's arrival moved) starts full of the source's last
//!   value, and one that grows is padded with it, so the port holds rather
//!   than dropping to 0 for the delay's length.
//! - **A crossfade's base.** A [`replace`](crate::Editor::replace) with a
//!   fade keeps the key's param state, so both units hear one modulation;
//!   the base is the incoming unit's control, ramped over one block like
//!   any control move, not over the fade. Ramping it over the fade would
//!   hold the incoming unit off its own control for the fade's length, and
//!   the audio crossfade already covers the swap.
//! - **PDC.** A param source is aligned to the node's arrival like any of
//!   its inputs: a source that arrives earlier is delayed
//!   ([`DelayKey::ParamAudio`](crate::DelayKey::ParamAudio),
//!   [`DelayKey::ParamEvent`](crate::DelayKey::ParamEvent)), and a later one
//!   raises the node's arrival.
//!
//! The step is fused into the node op rather than being an op of its own:
//! its only output is the per-frame values the node reads in the same op, so
//! a separate op would need a slot, an ordering edge and a verifier rule for
//! a buffer nothing else can read. Its sources are reads of the node op, so
//! the colouring and the verifier cover them like any input.

use std::hash::{Hash, Hasher};
use std::sync::atomic::{fence, AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;

use tutti_types::graph::OutPort;
use tutti_types::{AudioThread, NodeKey, ParamAddr, Samples, UnitParam};

use crate::event::{Event, EventKind};
use crate::spec::EventOut;

/// Most params one node can declare modulatable.
pub const MAX_PARAM_PORTS: usize = 8;

// A `Legacy` unit declares its feed's params as its ports, so a feed can
// never carry more than a shape can declare.
const _: () = assert!(
    tutti_node::MAX_FED_PARAMS == MAX_PARAM_PORTS,
    "a ParamFeed and a Shape bound modulatable params alike"
);

/// Most sources one param port can sum. A spec past it is refused
/// ([`GraphInvalid::TooManyParamSources`](crate::GraphInvalid::TooManyParamSources)):
/// the per-source ramp state lives in a fixed array, so the audio thread never
/// grows it.
pub const MAX_PARAM_SOURCES: usize = 16;

/// How long a param port crossfades when its sources change: from where it
/// was to where the new sources put it (see the `param` module docs,
/// `src/param.rs`). 256 frames is about 5 ms at 48 kHz: long enough that a
/// modulator connected at full swing is not a click, short enough that the
/// connection is heard where it was made.
pub const PARAM_DECLICK: Samples = Samples(256);

/// The params a node lets the graph modulate, in **port order** — the index
/// [`Io::param`](crate::Io::param) and [`Node::param_base`](crate::Node::param_base)
/// take. A fixed-capacity list, so [`Shape`](crate::Shape) stays `Copy`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ParamPorts {
    len: u8,
    ids: [UnitParam; MAX_PARAM_PORTS],
}

impl ParamPorts {
    /// No modulatable params.
    pub const NONE: Self = Self {
        len: 0,
        ids: [UnitParam::Cutoff; MAX_PARAM_PORTS],
    };

    /// `ids`, in this order.
    ///
    /// # Panics
    ///
    /// Where [`try_new`](Self::try_new) returns an error (in a `const`
    /// context, at compile time — where a node's list usually is).
    pub const fn new(ids: &[UnitParam]) -> Self {
        match Self::try_new(ids) {
            Ok(p) => p,
            Err(ParamPortsError::TooMany { .. }) => {
                panic!("more modulatable params than MAX_PARAM_PORTS")
            }
            Err(ParamPortsError::Duplicate(_)) => panic!("a param is declared modulatable twice"),
        }
    }

    /// [`new`](Self::new), refusing with a [`ParamPortsError`] a list of
    /// more than [`MAX_PARAM_PORTS`] or one naming a param twice.
    pub const fn try_new(ids: &[UnitParam]) -> Result<Self, ParamPortsError> {
        if ids.len() > MAX_PARAM_PORTS {
            return Err(ParamPortsError::TooMany { count: ids.len() });
        }
        let mut out = Self::NONE;
        let mut i = 0;
        while i < ids.len() {
            let mut j = 0;
            while j < i {
                if ids[j] as u16 == ids[i] as u16 {
                    return Err(ParamPortsError::Duplicate(ids[i]));
                }
                j += 1;
            }
            out.ids[i] = ids[i];
            i += 1;
        }
        out.len = ids.len() as u8;
        Ok(out)
    }

    /// The params, in port order.
    pub fn as_slice(&self) -> &[UnitParam] {
        &self.ids[..self.len as usize]
    }

    /// How many.
    pub const fn len(&self) -> usize {
        self.len as usize
    }

    /// Whether there are none.
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The port index of `param`, if it is declared.
    pub fn index_of(&self, param: UnitParam) -> Option<usize> {
        self.as_slice().iter().position(|&p| p == param)
    }
}

/// Why a list cannot be a node's [`ParamPorts`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParamPortsError {
    /// More params than [`MAX_PARAM_PORTS`].
    TooMany {
        /// How many were listed.
        count: usize,
    },
    /// A param listed twice.
    Duplicate(UnitParam),
}

impl std::fmt::Display for ParamPortsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooMany { count } => write!(
                f,
                "{count} modulatable params past a node's {MAX_PARAM_PORTS}"
            ),
            Self::Duplicate(p) => write!(f, "{p:?} is declared modulatable twice"),
        }
    }
}

impl std::error::Error for ParamPortsError {}

impl Default for ParamPorts {
    fn default() -> Self {
        Self::NONE
    }
}

/// A modulatable param of a node: the sink of a param edge.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ParamIn {
    /// The node.
    pub node: NodeKey,
    /// Which of its declared params.
    pub param: UnitParam,
}

/// What drives one offset of a param port.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ParamFrom {
    /// An audio output: its value at each frame is the raw modulation signal
    /// (an LFO, an envelope follower).
    Audio(OutPort),
    /// An event output: every [`ParamRamp`](crate::ParamRamp) on it
    /// addressed to this port's param starts a linear ramp of the source's
    /// value on its own frame, reaching the target on the ramp's last frame
    /// (a zero-length ramp is a step). The value starts at 0; other events
    /// are ignored.
    Events(EventOut),
}

impl ParamFrom {
    /// The node the source is an output of.
    pub const fn node(self) -> NodeKey {
        match self {
            Self::Audio(p) => p.node,
            Self::Events(p) => p.node,
        }
    }
}

/// Entries in a [`ShapeLut`].
pub const SHAPE_LUT_LEN: usize = 256;

/// A response curve over a modulator's `[-1, 1]` range, baked into a table
/// and read back with linear interpolation.
///
/// The bake and the lookup are `ParamShaperNode`'s, operation for operation —
/// the table is sampled at `x_i = i / 255 · 2 − 1` and read at
/// `(clamp(x, −1, 1) + 1) / 2 · 255` — so a table baked from
/// `tutti_mod::shape` gives the old shaper's output bit for bit. It clamps
/// its input to `[-1, 1]`: a modulator is a normalised signal.
///
/// Compared and hashed by the table's bits, so a [`GraphSpec`](crate::GraphSpec)
/// holding one is still `Eq + Hash`, and a depth that moved by an ulp is a
/// different graph.
#[derive(Clone)]
pub struct ShapeLut(Arc<[f32; SHAPE_LUT_LEN]>);

impl ShapeLut {
    /// Bake `f` over `[-1, 1]`. Allocates: build it on the control thread.
    pub fn from_fn(f: impl Fn(f32) -> f32) -> Self {
        let mut t = [0.0f32; SHAPE_LUT_LEN];
        for (i, slot) in t.iter_mut().enumerate() {
            let x = (i as f32 / (SHAPE_LUT_LEN - 1) as f32) * 2.0 - 1.0;
            *slot = f(x);
        }
        Self(Arc::new(t))
    }

    /// The table.
    pub fn table(&self) -> &[f32; SHAPE_LUT_LEN] {
        &self.0
    }

    /// The shaped value of `x`, interpolated between table points.
    #[inline]
    pub fn eval(&self, x: f32) -> f32 {
        let t = &*self.0;
        let pos = ((x.clamp(-1.0, 1.0) + 1.0) * 0.5) * (SHAPE_LUT_LEN - 1) as f32;
        let i = pos.floor() as usize;
        if i >= SHAPE_LUT_LEN - 1 {
            return t[SHAPE_LUT_LEN - 1];
        }
        let frac = pos - i as f32;
        t[i] + (t[i + 1] - t[i]) * frac
    }
}

impl PartialEq for ShapeLut {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
            || self
                .0
                .iter()
                .zip(other.0.iter())
                .all(|(a, b)| a.to_bits() == b.to_bits())
    }
}

impl Eq for ShapeLut {}

impl Hash for ShapeLut {
    fn hash<H: Hasher>(&self, state: &mut H) {
        for v in self.0.iter() {
            v.to_bits().hash(state);
        }
    }
}

impl std::fmt::Debug for ShapeLut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let t = &*self.0;
        write!(
            f,
            "ShapeLut({} → {}, {} → {})",
            -1.0,
            t[0],
            1.0,
            t[SHAPE_LUT_LEN - 1]
        )
    }
}

/// How a source's raw value becomes an offset.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ParamShaping {
    /// The value itself: the source already speaks the param's units (a
    /// `ParamRamp` lane of offsets, an envelope scaled at its node).
    Identity,
    /// Through a [`ShapeLut`] over `[-1, 1]`.
    Lut(ShapeLut),
}

impl ParamShaping {
    /// The offset for raw value `x`.
    #[inline]
    pub fn apply(&self, x: f32) -> f32 {
        match self {
            Self::Identity => x,
            Self::Lut(l) => l.eval(x),
        }
    }
}

/// One source of a param port: where it comes from, and how it is shaped.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ParamSource {
    /// The output that drives it.
    pub from: ParamFrom,
    /// How its value becomes an offset.
    pub shaping: ParamShaping,
}

/// The range a modulated param is clamped to, in the param's own units.
///
/// Bare `f32` bounds, not a unit type: a param port is erased over its unit
/// (a cutoff is `Hz`, a threshold `Db`), as the `ParamRamp` wire is — the
/// same erasure `LayeredCurve` makes on the control-rate side. Crossed bounds
/// are ordered where they are applied, never a panic; a NaN bound never
/// reaches the audio thread
/// ([`GraphInvalid::BadParamRange`](crate::GraphInvalid::BadParamRange)).
/// Compared and hashed by bits, so a spec holding one stays `Eq + Hash`.
#[derive(Clone, Copy, Debug)]
pub struct ParamRange {
    /// The lowest value.
    pub min: f32,
    /// The highest value.
    pub max: f32,
}

impl ParamRange {
    /// No clamp at all.
    pub const UNBOUNDED: Self = Self {
        min: f32::NEG_INFINITY,
        max: f32::INFINITY,
    };

    /// `min..=max`.
    pub const fn new(min: f32, max: f32) -> Self {
        Self { min, max }
    }

    /// The bounds, ordered so `clamp` cannot panic.
    #[inline]
    pub fn ordered(self) -> (f32, f32) {
        (self.min.min(self.max), self.max.max(self.min))
    }
}

impl Default for ParamRange {
    fn default() -> Self {
        Self::UNBOUNDED
    }
}

impl PartialEq for ParamRange {
    fn eq(&self, other: &Self) -> bool {
        self.min.to_bits() == other.min.to_bits() && self.max.to_bits() == other.max.to_bits()
    }
}

impl Eq for ParamRange {}

impl Hash for ParamRange {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.min.to_bits().hash(state);
        self.max.to_bits().hash(state);
    }
}

/// How one param port is modulated: its range, and its sources in source
/// order ([`ParamFrom`]'s `Ord`). No sources is the same as no entry: the
/// port reads its base.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct ParamMod {
    /// What the sum is clamped to.
    pub range: ParamRange,
    /// The sources, in source order.
    pub sources: Vec<ParamSource>,
}

/// What a node reads for one of its params this block
/// ([`Io::param`](crate::Io::param)).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ParamInput<'a> {
    /// Nothing modulates it: use the param's own control (the base), as the
    /// node would with no graph around it. The fast path.
    Base,
    /// The param's value at each frame of the block.
    Frames(&'a [f32]),
}

impl<'a> ParamInput<'a> {
    /// The per-frame values, when modulated.
    #[inline]
    pub fn frames(self) -> Option<&'a [f32]> {
        match self {
            Self::Base => None,
            Self::Frames(f) => Some(f),
        }
    }
}

/// A port's sources as a signature: equal for equal source lists (the same
/// outputs, the same shapings), so the executor can tell a port whose
/// sources changed in a commit — and crossfade it — from one that only
/// moved slots.
pub(crate) fn signature(sources: &[ParamSource]) -> u64 {
    // `DefaultHasher::new` is SipHash with fixed keys: deterministic across
    // runs and threads. 0 is reserved for "no sources".
    let mut h = std::collections::hash_map::DefaultHasher::new();
    sources.hash(&mut h);
    h.finish().max(1)
}

/// One event source's ramp: its value, and the ramp it is on.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct Ramp {
    value: f32,
    from: f32,
    target: f32,
    len: u32,
    done: u32,
    active: bool,
}

impl Ramp {
    /// Start a ramp to `target` over `len` frames from this frame on, from
    /// the value the last frame had. A second ramp on the same frame
    /// replaces the first, from the same value.
    fn start(&mut self, target: f32, len: usize) {
        self.from = self.value;
        self.target = target;
        self.len = u32::try_from(len).unwrap_or(u32::MAX);
        self.done = 0;
        self.active = true;
    }

    /// The value at this frame, after any ramp started on it: the ramp's
    /// `done`-th of `len` steps, the target from its last one (a zero-length
    /// ramp reaches it on its first frame).
    #[inline]
    fn step(&mut self) -> f32 {
        if self.active {
            self.done += 1;
            self.value = if self.done >= self.len {
                self.active = false;
                self.target
            } else {
                self.from + (self.target - self.from) * (self.done as f32 / self.len as f32)
            };
        }
        self.value
    }
}

/// Render one event source's values for a block into `out`: the ramps its
/// events for `param` start, frame by frame.
pub(crate) fn render_ramps(ramp: &mut Ramp, events: &[Event], param: UnitParam, out: &mut [f32]) {
    let want = ParamAddr::Unit(param);
    let mut next = events.iter().peekable();
    for (i, o) in out.iter_mut().enumerate() {
        while let Some(e) = next.next_if(|e| e.offset.index() <= i) {
            if let EventKind::Ramp(r) = e.kind {
                if r.addr() == want {
                    ramp.start(r.raw_target(), r.duration().get());
                }
            }
        }
        *o = ramp.step();
    }
}

/// One source's running state in a port, kept by where it comes from.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct SrcState {
    /// The source this is (`None`: an empty slot).
    from: Option<ParamFrom>,
    /// An event source's ramp.
    ramp: Ramp,
    /// The raw value it delivered on the last frame it ran: what a PDC delay
    /// that appears on it starts full of.
    last_raw: f32,
}

/// One declared param port's running state, kept with its unit across
/// blocks and plans.
#[derive(Clone, Debug)]
pub(crate) struct PortState {
    /// Whether the port has not run yet: its first block seeds `sig` without
    /// a declick (see the module docs).
    fresh: bool,
    /// The signature of the sources it last ran with (0: none).
    sig: u64,
    /// Whether the last block delivered frames.
    modulated: bool,
    /// The last frame delivered.
    last: f32,
    /// The base at the end of the last block.
    last_base: f32,
    /// Where the declick fades from.
    hold: f32,
    /// Frames of the declick done; `PARAM_DECLICK` or more: none running.
    fade: usize,
    /// Its sources' state, in source order; the first `n_srcs` are live.
    srcs: [SrcState; MAX_PARAM_SOURCES],
    n_srcs: usize,
}

impl Default for PortState {
    fn default() -> Self {
        Self {
            fresh: true,
            sig: 0,
            modulated: false,
            last: 0.0,
            last_base: 0.0,
            hold: 0.0,
            fade: PARAM_DECLICK.get(),
            srcs: [SrcState::default(); MAX_PARAM_SOURCES],
            n_srcs: 0,
        }
    }
}

impl PortState {
    /// Re-key the sources' state to `froms`, in order: a source still here
    /// keeps its ramp and last value wherever it moved, a new one starts at
    /// rest, one gone is dropped.
    fn rekey(&mut self, froms: impl Iterator<Item = ParamFrom>) {
        let old = self.srcs;
        let n = self.n_srcs;
        self.srcs = [SrcState::default(); MAX_PARAM_SOURCES];
        let mut k = 0;
        for (slot, from) in self.srcs.iter_mut().zip(froms) {
            *slot = old[..n]
                .iter()
                .find(|s| s.from == Some(from))
                .copied()
                .unwrap_or(SrcState {
                    from: Some(from),
                    ..SrcState::default()
                });
            k += 1;
        }
        self.n_srcs = k;
    }

    /// The last raw value source `from` delivered, if it ran here.
    fn last_raw(&self, from: ParamFrom) -> Option<f32> {
        self.srcs[..self.n_srcs]
            .iter()
            .find(|s| s.from == Some(from))
            .map(|s| s.last_raw)
    }
}

/// One source's input this block, as the fused step reads it.
#[derive(Clone, Copy)]
pub(crate) enum SourceIn<'a> {
    /// Audio: one value per frame.
    Audio(&'a [f32]),
    /// Events: the port's sorted events.
    Events(&'a [Event]),
}

/// What a unit keeps for its declared param ports: their state and the
/// buffer their per-frame values are written into. Built on the control side
/// with the unit's commit, so the audio thread never allocates one.
#[derive(Debug, Default)]
pub(crate) struct ParamState {
    ports: Vec<PortState>,
    scratch: Vec<f32>,
    /// Scratch for one event source's values.
    ramp_buf: Vec<f32>,
    max: usize,
    /// Per port: whether this block's step delivered frames.
    framed: [bool; MAX_PARAM_PORTS],
    /// Whether any port delivered frames this block, is mid-declick, or has
    /// yet to run: a unit for which this is false and whose op lists no
    /// modulated port skips the whole step.
    busy: bool,
    /// Where the event sources' ramps are published for a fork (see
    /// [`ParamTap`]).
    tap: Option<Arc<ParamTap>>,
}

impl ParamState {
    /// State for `ports` declared params at blocks of up to `max` frames,
    /// publishing to `tap` and starting from `seeds` (per port, the event
    /// sources' ramps a fork carries over). Allocates nothing when `ports`
    /// is 0.
    pub(crate) fn new(
        ports: usize,
        max: usize,
        tap: Option<Arc<ParamTap>>,
        seeds: &[Vec<(ParamFrom, Ramp)>],
    ) -> Self {
        if ports == 0 {
            return Self::default();
        }
        let mut state = vec![PortState::default(); ports];
        for (st, seed) in state.iter_mut().zip(seeds) {
            for (slot, &(from, ramp)) in st.srcs.iter_mut().zip(seed) {
                *slot = SrcState {
                    from: Some(from),
                    ramp,
                    last_raw: ramp.value,
                };
            }
            st.n_srcs = seed.len().min(MAX_PARAM_SOURCES);
        }
        Self {
            ports: state,
            scratch: vec![0.0; ports * max],
            ramp_buf: vec![0.0; max],
            max,
            framed: [false; MAX_PARAM_PORTS],
            // Its first block runs the step whatever the plan says, to
            // seed each port (see `PortState::fresh`).
            busy: true,
            tap,
        }
    }

    /// Whether the step must run although the plan modulates nothing here:
    /// a declick to base after a disconnect, or a port that has not run yet.
    /// After the step, whether any port delivered frames this block.
    #[inline]
    pub(crate) fn busy(&self) -> bool {
        self.busy
    }

    /// What each port reads this block, after [`port`](Self::port).
    pub(crate) fn inputs(&self, frames: usize) -> [ParamInput<'_>; MAX_PARAM_PORTS] {
        let mut out = [ParamInput::Base; MAX_PARAM_PORTS];
        for (k, o) in out.iter_mut().enumerate().take(self.ports.len()) {
            if self.framed[k] {
                *o = ParamInput::Frames(&self.scratch[k * self.max..k * self.max + frames]);
            }
        }
        out
    }

    /// How many params it keeps state for.
    pub(crate) fn port_count(&self) -> usize {
        self.ports.len()
    }

    /// The last raw value `from` delivered into port `k`, if it ran there:
    /// what a PDC delay that appears on it starts full of.
    pub(crate) fn last_raw(&self, k: usize, from: ParamFrom) -> Option<f32> {
        self.ports.get(k).and_then(|p| p.last_raw(from))
    }

    /// Start a block: no port delivers frames until [`port`](Self::port)
    /// says so.
    #[inline]
    pub(crate) fn begin(&mut self) {
        self.framed = [false; MAX_PARAM_PORTS];
        self.busy = false;
    }

    /// Run the fused step for declared port `k` (param `param`) over one
    /// block: its modulation this plan (its signature, range and sources, in
    /// source order — `None` when nothing modulates it), and its base
    /// (`None` for a node that cannot say, which then reads its own control,
    /// unmodulated).
    #[allow(
        clippy::type_complexity,
        reason = "one borrowed view of a port's plan entry, built per block on the stack"
    )]
    pub(crate) fn port(
        &mut self,
        k: usize,
        param: UnitParam,
        frames: usize,
        m: Option<(u64, ParamRange, &[(SourceIn<'_>, &ParamShaping, ParamFrom)])>,
        base: Option<f32>,
    ) {
        let d = PARAM_DECLICK.get();
        let st = &mut self.ports[k];
        let Some(b1) = base else {
            // A node that cannot say its base is never modulated: it reads
            // its own control. No state to carry.
            *st = PortState::default();
            return;
        };
        let sig = m.as_ref().map_or(0, |(s, _, _)| *s);
        let froms = || m.iter().flat_map(|(_, _, s)| s.iter().map(|&(_, _, f)| f));
        if st.fresh {
            // The first block: nothing to fade from (module docs).
            st.fresh = false;
            st.sig = sig;
            st.rekey(froms());
        } else if sig != st.sig {
            // Sources changed: fade from where the port was.
            st.hold = if st.modulated { st.last } else { b1 };
            st.fade = 0;
            st.sig = sig;
            st.rekey(froms());
        }
        let fading = st.fade < d;
        if sig == 0 && !fading {
            if st.modulated {
                publish(self.tap.as_deref(), k, st);
            }
            st.modulated = false;
            return;
        }
        let from = if st.modulated { st.last_base } else { b1 };
        let out = &mut self.scratch[k * self.max..k * self.max + frames];
        let n = frames as f32;
        match m {
            Some((_, range, sources)) => {
                // `f32`'s `Sum` folds from `-0.0`; so does this, so the sum is
                // bit-identical to the old `ParamSumNode`'s `base + Σ`.
                out.fill(-0.0);
                for (j, (src, shaping, _)) in sources.iter().enumerate() {
                    let s = &mut st.srcs[j];
                    match src {
                        SourceIn::Audio(x) => {
                            for (o, &x) in out.iter_mut().zip(&x[..frames]) {
                                *o += shaping.apply(x);
                            }
                            s.last_raw = x[frames - 1];
                        }
                        SourceIn::Events(ev) => {
                            let buf = &mut self.ramp_buf[..frames];
                            render_ramps(&mut s.ramp, ev, param, buf);
                            for (o, &x) in out.iter_mut().zip(buf.iter()) {
                                *o += shaping.apply(x);
                            }
                            s.last_raw = buf[frames - 1];
                        }
                    }
                }
                let (lo, hi) = range.ordered();
                for (i, o) in out.iter_mut().enumerate() {
                    *o = (base_at(from, b1, i, frames, n) + *o).clamp(lo, hi);
                }
            }
            None => {
                for (i, o) in out.iter_mut().enumerate() {
                    *o = base_at(from, b1, i, frames, n);
                }
            }
        }
        if fading {
            for (i, o) in out.iter_mut().enumerate() {
                let j = st.fade + i;
                if j >= d {
                    break;
                }
                let g = 1.0 - (j + 1) as f32 / d as f32;
                *o += (st.hold - *o) * g;
            }
            st.fade = (st.fade + frames).min(d);
        }
        st.last = out[frames - 1];
        st.last_base = b1;
        st.modulated = true;
        self.framed[k] = true;
        self.busy = true;
        publish(self.tap.as_deref(), k, st);
    }
}

impl Drop for ParamState {
    /// A unit's param state is freed on the control side: a retired unit's
    /// comes back in its commit box (`Commit::spare_params`). One that
    /// allocated nothing (no declared params) may go anywhere.
    fn drop(&mut self) {
        if !self.ports.is_empty() {
            AudioThread::check_not_current("a unit's param state");
        }
    }
}

/// Publish port `k`'s event-source ramps to `tap`, if there is one.
#[inline]
fn publish(tap: Option<&ParamTap>, k: usize, st: &PortState) {
    if let Some(tap) = tap {
        if let Some(p) = tap.ports.get(k) {
            p.write(&st.srcs[..st.n_srcs]);
        }
    }
}

/// A unit's event-source ramps as the audio thread last left them, readable
/// on the control side: what [`Editor::fork`](crate::Editor::fork) seeds a
/// forked unit's ports with, so a held automation value is where the live
/// graph has it rather than at 0.
///
/// One per unit with declared params, kept by the editor per key and shared
/// with the unit's [`ParamState`]. Each port is a sequence lock over atomics
/// (no `unsafe`): the audio thread, its only writer, never waits; a reader
/// retries while a write is under way.
#[derive(Debug)]
pub(crate) struct ParamTap {
    ports: Box<[TapPort]>,
}

#[derive(Debug, Default)]
struct TapPort {
    /// Odd while a write is under way.
    seq: AtomicU32,
    /// How many of `srcs` are live.
    n: AtomicU32,
    srcs: [TapSrc; MAX_PARAM_SOURCES],
}

#[derive(Debug, Default)]
struct TapSrc {
    node: AtomicU64,
    port: AtomicU32,
    value: AtomicU32,
    from: AtomicU32,
    target: AtomicU32,
    len: AtomicU32,
    done: AtomicU32,
    active: AtomicBool,
}

impl ParamTap {
    /// A tap for `ports` declared params. Control side: allocates.
    pub(crate) fn new(ports: usize) -> Arc<Self> {
        Arc::new(Self {
            ports: (0..ports).map(|_| TapPort::default()).collect(),
        })
    }

    /// How many ports it covers.
    pub(crate) fn port_count(&self) -> usize {
        self.ports.len()
    }

    /// Port `k`'s event sources and their ramps, as last published.
    pub(crate) fn read(&self, k: usize) -> Vec<(EventOut, Ramp)> {
        let Some(p) = self.ports.get(k) else {
            return Vec::new();
        };
        loop {
            let s1 = p.seq.load(Ordering::Acquire);
            if s1 & 1 == 1 {
                std::hint::spin_loop();
                continue;
            }
            let n = (p.n.load(Ordering::Relaxed) as usize).min(MAX_PARAM_SOURCES);
            let out: Vec<(EventOut, Ramp)> = p.srcs[..n]
                .iter()
                .map(|s| {
                    let f = |a: &AtomicU32| f32::from_bits(a.load(Ordering::Relaxed));
                    (
                        EventOut {
                            node: NodeKey(s.node.load(Ordering::Relaxed)),
                            port: s.port.load(Ordering::Relaxed) as u16,
                        },
                        Ramp {
                            value: f(&s.value),
                            from: f(&s.from),
                            target: f(&s.target),
                            len: s.len.load(Ordering::Relaxed),
                            done: s.done.load(Ordering::Relaxed),
                            active: s.active.load(Ordering::Relaxed),
                        },
                    )
                })
                .collect();
            fence(Ordering::Acquire);
            if p.seq.load(Ordering::Relaxed) == s1 {
                return out;
            }
        }
    }
}

impl TapPort {
    /// Publish `srcs`' event sources. Audio thread; wait-free.
    fn write(&self, srcs: &[SrcState]) {
        let s = self.seq.load(Ordering::Relaxed);
        self.seq.store(s.wrapping_add(1), Ordering::Relaxed);
        fence(Ordering::Release);
        let mut n = 0;
        for src in srcs {
            let Some(ParamFrom::Events(e)) = src.from else {
                continue;
            };
            let t = &self.srcs[n];
            let r = &src.ramp;
            t.node.store(e.node.0, Ordering::Relaxed);
            t.port.store(u32::from(e.port), Ordering::Relaxed);
            t.value.store(r.value.to_bits(), Ordering::Relaxed);
            t.from.store(r.from.to_bits(), Ordering::Relaxed);
            t.target.store(r.target.to_bits(), Ordering::Relaxed);
            t.len.store(r.len, Ordering::Relaxed);
            t.done.store(r.done, Ordering::Relaxed);
            t.active.store(r.active, Ordering::Relaxed);
            n += 1;
        }
        self.n.store(n as u32, Ordering::Relaxed);
        self.seq.store(s.wrapping_add(2), Ordering::Release);
    }
}

/// The base at frame `i` of an `frames`-frame block ramping `from → to`:
/// linear, landing exactly on `to` at the last frame.
#[inline]
fn base_at(from: f32, to: f32, i: usize, frames: usize, n: f32) -> f32 {
    if i + 1 == frames {
        to
    } else {
        from + (to - from) * ((i + 1) as f32 / n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::ParamRamp;
    use crate::time::Offset;
    use tutti_types::{Hz, ParamKey};

    /// A ramp event starts on its frame and lands on its target on its last
    /// frame; a zero-length one is a step on its frame.
    ///
    /// Mutation (run): `step` interpolating one frame behind (`(done - 1) /
    /// len`) → the ramp's first frame stays at 0 → fails.
    #[test]
    fn a_ramp_lands_on_its_last_frame() {
        let ev = |at: usize, t: f32, len: usize| {
            Event::ramp(
                Offset::new(at, Samples(16)).expect("in block"),
                ParamRamp::new(ParamKey::<Hz>::CUTOFF, Hz(t), Samples(len)),
            )
        };
        let events = [ev(4, 100.0, 4), ev(12, -5.0, 0)];
        let mut r = Ramp::default();
        let mut out = [0.0f32; 16];
        render_ramps(&mut r, &events, UnitParam::Cutoff, &mut out);
        assert_eq!(&out[..4], &[0.0; 4], "nothing before the event");
        assert_eq!(out[4], 25.0);
        assert_eq!(out[7], 100.0, "on target at the ramp's last frame");
        assert_eq!(out[11], 100.0, "held");
        assert_eq!(out[12], -5.0, "a step lands on its frame");
        // Another param's ramp is not this port's.
        let mut r = Ramp::default();
        render_ramps(&mut r, &events, UnitParam::Q, &mut out);
        assert!(out.iter().all(|&x| x == 0.0));
    }

    /// A list a node cannot declare is a named error, not a panic.
    ///
    /// Mutation (run): drop the duplicate check from `try_new` → the
    /// repeated list is accepted → fails.
    #[test]
    fn an_undeclarable_list_is_a_named_error() {
        use UnitParam::*;
        let full = [Cutoff, Q, Drive, Wet, Threshold, Ratio, Attack, Release];
        assert_eq!(
            ParamPorts::try_new(&full).map(|p| p.len()),
            Ok(MAX_PARAM_PORTS)
        );
        let over = [
            Cutoff, Q, Drive, Wet, Threshold, Ratio, Attack, Release, Volume,
        ];
        assert_eq!(
            ParamPorts::try_new(&over),
            Err(ParamPortsError::TooMany { count: 9 })
        );
        assert_eq!(
            ParamPorts::try_new(&[Q, Cutoff, Q]),
            Err(ParamPortsError::Duplicate(Q))
        );
    }

    /// The LUT reads back its own table points, and clamps its input.
    ///
    /// Mutation: dropping the clamp in `eval` → `eval(3.0)` indexes past
    /// the table and panics → fails.
    #[test]
    fn a_lut_reads_back_its_table() {
        let lut = ShapeLut::from_fn(|x| x * x * x);
        assert_eq!(lut.eval(-1.0), -1.0);
        assert_eq!(lut.eval(1.0), 1.0);
        assert_eq!(lut.eval(3.0), 1.0);
        assert_eq!(lut.eval(-3.0), -1.0);
        let x = (10.0f32 / 255.0) * 2.0 - 1.0;
        assert!((lut.eval(x) - lut.table()[10]).abs() < 1e-6);
    }
}
