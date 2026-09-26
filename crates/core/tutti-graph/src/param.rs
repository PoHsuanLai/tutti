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
//!   doc 013 §6).
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
use std::sync::Arc;

use tutti_types::graph::OutPort;
use tutti_types::{NodeKey, ParamAddr, Samples, UnitParam};

use crate::event::{Event, EventKind};
use crate::spec::EventOut;

/// Most params one node can declare modulatable.
pub const MAX_PARAM_PORTS: usize = 8;

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
    /// If `ids` holds more than [`MAX_PARAM_PORTS`], or one param twice (in
    /// a `const` context, at compile time).
    pub const fn new(ids: &[UnitParam]) -> Self {
        assert!(
            ids.len() <= MAX_PARAM_PORTS,
            "more modulatable params than MAX_PARAM_PORTS"
        );
        let mut out = Self::NONE;
        let mut i = 0;
        while i < ids.len() {
            let mut j = 0;
            while j < i {
                assert!(
                    ids[j] as u16 != ids[i] as u16,
                    "a param is declared modulatable twice"
                );
                j += 1;
            }
            out.ids[i] = ids[i];
            i += 1;
        }
        out.len = ids.len() as u8;
        out
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
/// are ordered where they are applied, never a panic. Compared and hashed by
/// bits, so a spec holding one stays `Eq + Hash`.
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
#[derive(Clone, Copy, Debug, Default)]
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

/// One declared param port's running state, kept with its unit across
/// blocks and plans.
#[derive(Clone, Debug)]
pub(crate) struct PortState {
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
    ramps: [Ramp; MAX_PARAM_SOURCES],
}

impl Default for PortState {
    fn default() -> Self {
        Self {
            sig: 0,
            modulated: false,
            last: 0.0,
            last_base: 0.0,
            hold: 0.0,
            fade: PARAM_DECLICK.get(),
            ramps: [Ramp::default(); MAX_PARAM_SOURCES],
        }
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
    /// Whether any port is modulated or mid-declick: a unit for which this is
    /// false and whose op lists no modulated port skips the whole step.
    busy: bool,
}

impl ParamState {
    /// State for `ports` declared params at blocks of up to `max` frames.
    /// Allocates nothing when `ports` is 0.
    pub(crate) fn new(ports: usize, max: usize) -> Self {
        if ports == 0 {
            return Self::default();
        }
        Self {
            ports: vec![PortState::default(); ports],
            scratch: vec![0.0; ports * max],
            ramp_buf: vec![0.0; max],
            max,
            framed: [false; MAX_PARAM_PORTS],
            busy: false,
        }
    }

    /// Whether a unit with no modulated port this plan still has a port to
    /// run (a declick to base after a disconnect, or a port to retire).
    #[inline]
    pub(crate) fn busy(&self) -> bool {
        self.busy
    }

    /// What each port reads this block, after [`compute`](Self::compute).
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
    pub(crate) fn port(
        &mut self,
        k: usize,
        param: UnitParam,
        frames: usize,
        m: Option<(u64, ParamRange, &[(SourceIn<'_>, &ParamShaping)])>,
        base: Option<f32>,
    ) {
        let d = PARAM_DECLICK.get();
        let st = &mut self.ports[k];
        let Some(b1) = base else {
            // A node that cannot say its base is never modulated: it reads
            // its own control. No state to carry.
            st.sig = 0;
            st.modulated = false;
            st.fade = d;
            return;
        };
        let sig = m.as_ref().map_or(0, |(s, _, _)| *s);
        if sig != st.sig {
            // Sources changed: fade from where the port was.
            st.hold = if st.modulated { st.last } else { b1 };
            st.fade = 0;
            st.sig = sig;
            st.ramps = [Ramp::default(); MAX_PARAM_SOURCES];
        }
        let fading = st.fade < d;
        if sig == 0 && !fading {
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
                for (j, (src, shaping)) in sources.iter().enumerate() {
                    match src {
                        SourceIn::Audio(x) => {
                            for (o, &x) in out.iter_mut().zip(&x[..frames]) {
                                *o += shaping.apply(x);
                            }
                        }
                        SourceIn::Events(ev) => {
                            let buf = &mut self.ramp_buf[..frames];
                            render_ramps(&mut st.ramps[j], ev, param, buf);
                            for (o, &x) in out.iter_mut().zip(buf.iter()) {
                                *o += shaping.apply(x);
                            }
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
