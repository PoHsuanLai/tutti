//! Compiler-owned parameter ports.
//!
//! A node declares the params a graph may drive
//! ([`Shape::params`](crate::Shape::params), by [`UnitParam`] id). A
//! [`GraphSpec`](crate::GraphSpec) can then drive any of them from another
//! node's event output ([`GraphSpec::connect_param`](crate::GraphSpec::connect_param)),
//! and the compiler turns each driven param into one fused step of the
//! node's op:
//!
//! ```text
//!   value[i] = clamp(held[i], range)
//!   held[i]  = the event source's value, from its first event on
//!            = base_ramp[i], before that
//! ```
//!
//! An event source **replaces** the base (doc 014 §5.6): a port takes at
//! most one. Modulation (offsets, depth, LFOs) is not the graph's: a
//! control lane sums it and sends the value (doc 014).
//!
//! The rules are documented on
//! [`GraphSpec::connect_param`](crate::GraphSpec::connect_param).
//!
//! The step is fused into the node op rather than being an op of its own:
//! its only output is the per-frame values the node reads in the same op, so
//! a separate op would need a slot, an ordering edge and a verifier rule for
//! a buffer nothing else can read. Its source is a read of the node op, so
//! the colouring and the verifier cover it like any input.

use std::hash::{Hash, Hasher};
use std::sync::Arc;

use tutti_types::{NodeKey, PackedAtomic, ParamAddr, RtScope, Samples, UnitParam};

use crate::event::{segment_progress, Event, EventKind, SegmentShape};
use crate::spec::EventOut;

/// Most params one node can declare modulatable.
pub const MAX_PARAM_PORTS: usize = 8;

/// How long a param port crossfades when its source changes: from where it
/// was to where the new source puts it (see
/// [`GraphSpec::connect_param`](crate::GraphSpec::connect_param)). 256 frames is about 5 ms at 48 kHz: long enough that a
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

    /// [`new`](Self::new), without the panic.
    ///
    /// # Errors
    ///
    /// [`ParamPortsError::TooMany`] for a list of more than
    /// [`MAX_PARAM_PORTS`], [`ParamPortsError::Duplicate`] for one naming a
    /// param twice.
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

/// The range a modulated param is clamped to, in the param's own units.
///
/// Bare `f32` bounds, not a unit type: a param port is erased over its unit
/// (a cutoff is `Hz`, a threshold `Db`), as the `ParamRamp` wire is.
/// Crossed bounds
/// are ordered where they are applied, never a panic; a NaN bound never
/// reaches the audio thread
/// ([`GraphInvalid::BadParamRange`](crate::GraphInvalid::BadParamRange)).
/// Compared and hashed by bits, so a spec holding one stays `Eq + Hash`.
/// Plain bytes (`Pod`), so a port's live range is one
/// [`PackedAtomic`] word.
#[derive(Clone, Copy, Debug, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
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

/// How one param port is driven: its range, and its event source. No
/// source is the same as no entry: the port reads its base.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct ParamMod {
    /// What the value is clamped to.
    pub range: ParamRange,
    /// The event output whose value replaces the base.
    pub source: Option<EventOut>,
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

/// A curve segment's timing and shape, as a source's running state keeps it
/// (see [`ParamSegment`](crate::ParamSegment)).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Curve {
    /// Its position along itself at its event's frame, in frames.
    pub(crate) position: f32,
    /// Its length, in frames.
    pub(crate) length: f32,
    pub(crate) shape: SegmentShape,
}

/// One event source's ramp: its value, and the move it is on — a linear
/// [`ParamRamp`](crate::ParamRamp) from the value it had, or a curve
/// segment between its own endpoints.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct Ramp {
    value: f32,
    from: f32,
    target: f32,
    /// A linear ramp's length, in frames.
    len: u32,
    /// Frames done: a ramp's steps taken, a segment's frames since its
    /// event's.
    done: u32,
    active: bool,
    /// `Some` while the move is a curve segment.
    curve: Option<Curve>,
    /// Whether the source has sent its first event: from then on its value
    /// replaces the port's base.
    started: bool,
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
        self.curve = None;
        self.started = true;
    }

    /// Start a curve segment from this frame on: its own endpoints, so the
    /// value it interrupts is not read. A second event on the same frame
    /// replaces it.
    fn start_segment(&mut self, from: f32, to: f32, curve: Curve) {
        self.from = from;
        self.target = to;
        self.len = 0;
        self.done = 0;
        self.active = true;
        self.curve = Some(curve);
        self.started = true;
    }

    /// The value at this frame, after any move started on it. A ramp: its
    /// `done`-th of `len` steps, the target from its last one (a zero-length
    /// ramp reaches it on its first frame). A segment: its shape at its
    /// progress this frame, the target once it has arrived.
    #[inline]
    fn step(&mut self) -> f32 {
        if self.active {
            if let Some(c) = self.curve {
                // `segment_value`'s arithmetic, stepped: arrived, it holds
                // the target as a finished ramp does.
                match segment_progress(c.position, c.length, self.done) {
                    Some(t) => {
                        self.value = self.from + (self.target - self.from) * c.shape.eval(t);
                    }
                    None => {
                        self.value = self.target;
                        self.active = false;
                    }
                }
                self.done = self.done.saturating_add(1);
            } else {
                self.done += 1;
                self.value = if self.done >= self.len {
                    self.active = false;
                    self.target
                } else {
                    self.from + (self.target - self.from) * (self.done as f32 / self.len as f32)
                };
            }
        }
        self.value
    }
}

/// Render one event source's values for a block into `out`: the ramps and
/// curve segments its events for `param` start, frame by frame, and
/// `base(i)` on each frame before its first event ever. A first ramp starts
/// from `base` at its own frame, whether or not a segment came before it
/// on that frame.
pub(crate) fn render_ramps(
    ramp: &mut Ramp,
    events: &[Event],
    param: UnitParam,
    out: &mut [f32],
    base: impl Fn(usize) -> f32,
) {
    let want = ParamAddr::Unit(param);
    let mut next = events.iter().peekable();
    for (i, o) in out.iter_mut().enumerate() {
        while let Some(e) = next.next_if(|e| e.offset.index() <= i) {
            match e.kind {
                EventKind::Ramp(r) if r.addr() == want => {
                    if !ramp.started {
                        ramp.value = base(i);
                    }
                    ramp.start(r.raw_target(), r.duration().get());
                }
                EventKind::Segment(s) if s.is_for(want) => {
                    // A segment never reads the value it interrupts, but a
                    // ramp that replaces it on this frame does: a first one
                    // starts from the base, as it would alone.
                    if !ramp.started {
                        ramp.value = base(i);
                    }
                    let (from, to, position, length) = s.raw();
                    let shape = s.shape();
                    ramp.start_segment(
                        from,
                        to,
                        Curve {
                            position,
                            length,
                            shape,
                        },
                    );
                }
                // Another param's, MIDI, a chord, or a resync: a port re-sends
                // nothing.
                EventKind::Ramp(_)
                | EventKind::Segment(_)
                | EventKind::Midi(_)
                | EventKind::Harmony(_)
                | EventKind::Resync => {}
            }
        }
        *o = if ramp.started { ramp.step() } else { base(i) };
    }
}

/// The live range of each of one key's declared param ports: what the fused
/// step clamps a modulated value to, read once per block.
///
/// A range is a control, as a base is: written on the control side
/// ([`Editor::set_param_range`](crate::Editor::set_param_range), and every
/// commit from the spec), read by the audio thread at the start of the
/// port's step, so a move lands on the next block and costs a store, not a
/// plan. The spec's range is its initial value; the plan carries no range.
///
/// One [`PackedAtomic`] per port: both bounds in one word, so a reader never
/// pairs one write's `min` with another's `max` (two `AtomicF32`s, as two
/// `Param<U>`s would be, could). Each is an `Arc`, so a reader outside the
/// graph (a control lane, doc 014 §4.2) can read the very cell the port
/// clamps to through a [`ParamRangeReader`].
///
/// Kept by the editor per key, and shared with the state of each unit
/// placed there that declares the same params: a
/// crossfade keeps the key's running state, and that state reads these
/// cells. A unit declaring a different list gets fresh cells, so the unit it
/// replaces never reads a range meant for another param.
#[derive(Debug)]
pub(crate) struct ParamRanges {
    declared: ParamPorts,
    cells: Box<[Arc<PackedAtomic<ParamRange>>]>,
}

impl ParamRanges {
    /// Unbounded cells for `declared`. Control side: allocates.
    pub(crate) fn new(declared: ParamPorts) -> Arc<Self> {
        Arc::new(Self {
            declared,
            cells: (0..declared.len())
                .map(|_| Arc::new(PackedAtomic::new(ParamRange::UNBOUNDED)))
                .collect(),
        })
    }

    /// The params the cells are for, in port order.
    pub(crate) fn declared(&self) -> ParamPorts {
        self.declared
    }

    /// A read-only view of `param`'s cell, if it is one of these ports.
    /// Control side.
    pub(crate) fn reader(&self, param: UnitParam) -> Option<ParamRangeReader> {
        self.cells
            .get(self.declared.index_of(param)?)
            .map(|c| ParamRangeReader(Arc::clone(c)))
    }

    /// Set `param`'s range; `false` if it is not one of these ports.
    /// Control side.
    pub(crate) fn store(&self, param: UnitParam, range: ParamRange) -> bool {
        let Some(cell) = self
            .declared
            .index_of(param)
            .and_then(|k| self.cells.get(k))
        else {
            return false;
        };
        cell.store(range);
        true
    }

    /// Port `k`'s range now (unbounded past the last port). Wait-free.
    #[inline]
    pub(crate) fn load(&self, k: usize) -> ParamRange {
        self.cells
            .get(k)
            .map_or(ParamRange::UNBOUNDED, |c| c.load())
    }
}

/// A read-only view of one param port's live range: the very cell the port
/// clamps to, for a reader outside the graph (a control lane, which loads it
/// once per block for its span, doc 014 §4.2). Handed out by
/// [`Editor::param_range_cell`](crate::Editor::param_range_cell).
///
/// Read-only on purpose: the spec is the range's one writer
/// ([`Editor::set_param_range`](crate::Editor::set_param_range) stores into
/// the spec and this cell together, and every commit rewrites the cell from
/// the spec), so a store that went to the cell alone would be undone by the
/// next commit.
#[derive(Clone, Debug)]
pub struct ParamRangeReader(Arc<PackedAtomic<ParamRange>>);

impl ParamRangeReader {
    /// The range now: both bounds as one store left them. Wait-free.
    #[inline]
    pub fn load(&self) -> ParamRange {
        self.0.load()
    }

    /// Whether `self` and `other` read the same cell.
    pub fn same_cell(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

/// One declared param port's running state, kept with its unit across
/// blocks and plans.
#[derive(Clone, Copy, Debug)]
pub(crate) struct PortState {
    /// Whether the port has not run yet: its first block takes its source
    /// without a declick (see the module docs).
    fresh: bool,
    /// The event source it last ran with (`None`: none).
    source: Option<EventOut>,
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
    /// Its event source's ramp: the source's own, so it is reset when the
    /// source changes and kept otherwise.
    ramp: Ramp,
}

impl Default for PortState {
    fn default() -> Self {
        Self {
            fresh: true,
            source: None,
            modulated: false,
            last: 0.0,
            last_base: 0.0,
            hold: 0.0,
            fade: PARAM_DECLICK.get(),
            ramp: Ramp::default(),
        }
    }
}

/// What a unit keeps for its declared param ports: their state and the
/// buffer their per-frame values are written into. Built on the control side
/// with the unit's commit, so the audio thread never allocates one.
#[derive(Debug, Default)]
pub(crate) struct ParamState {
    ports: Vec<PortState>,
    scratch: Vec<f32>,
    max: usize,
    /// Per port: whether this block's step delivered frames.
    framed: [bool; MAX_PARAM_PORTS],
    /// Whether any port delivered frames this block, is mid-declick, or has
    /// yet to run: a unit for which this is false and whose op lists no
    /// driven port skips the whole step.
    busy: bool,
    /// Each port's live range (see [`ParamRanges`]); `None` only for a unit
    /// with no declared params.
    ranges: Option<Arc<ParamRanges>>,
}

impl ParamState {
    /// State for `ports` declared params at blocks of up to `max` frames,
    /// clamping to `ranges`. Allocates nothing when `ports` is 0.
    pub(crate) fn new(ports: usize, max: usize, ranges: Option<Arc<ParamRanges>>) -> Self {
        if ports == 0 {
            return Self::default();
        }
        Self {
            ports: vec![PortState::default(); ports],
            scratch: vec![0.0; ports * max],
            max,
            framed: [false; MAX_PARAM_PORTS],
            // Its first block runs the step whatever the plan says, to
            // seed each port (see `PortState::fresh`).
            busy: true,
            ranges,
        }
    }

    /// Whether the step must run although the plan drives nothing here:
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

    /// Start a block: no port delivers frames until [`port`](Self::port)
    /// says so.
    #[inline]
    pub(crate) fn begin(&mut self) {
        self.framed = [false; MAX_PARAM_PORTS];
        self.busy = false;
    }

    /// Run the fused step for declared port `k` (param `param`) over one
    /// block: its event source this plan and the source's sorted events
    /// (`None` when nothing drives it), and its base (`None` for a node that
    /// cannot say, which then reads its own control, undriven). The value
    /// is clamped to the port's live range as it stands at this block's
    /// start ([`ParamRanges`]).
    pub(crate) fn port(
        &mut self,
        k: usize,
        param: UnitParam,
        frames: usize,
        m: Option<(EventOut, &[Event])>,
        base: Option<f32>,
    ) {
        let d = PARAM_DECLICK.get();
        let st = &mut self.ports[k];
        let Some(b1) = base else {
            // A node that cannot say its base is never driven: it reads
            // its own control. No state to carry.
            *st = PortState::default();
            return;
        };
        let source = m.map(|(from, _)| from);
        if st.fresh {
            // The first block: nothing to fade from (module docs).
            st.fresh = false;
            st.source = source;
        } else if source != st.source {
            // The source changed: fade from where the port was, and start
            // the new source's ramp at rest.
            st.hold = if st.modulated { st.last } else { b1 };
            st.fade = 0;
            st.source = source;
            st.ramp = Ramp::default();
        }
        let fading = st.fade < d;
        if source.is_none() && !fading {
            st.modulated = false;
            return;
        }
        let from = if st.modulated { st.last_base } else { b1 };
        let out = &mut self.scratch[k * self.max..k * self.max + frames];
        let n = frames as f32;
        match m {
            Some((_, events)) => {
                render_ramps(&mut st.ramp, events, param, out, |i| {
                    base_at(from, b1, i, frames, n)
                });
                // The live range, read once per block: a move lands on the
                // next block, and the stored base and the source's ramp are
                // never rewritten, so a range that widens again releases
                // the value to where they put it.
                let range = self
                    .ranges
                    .as_ref()
                    .map_or(ParamRange::UNBOUNDED, |r| r.load(k));
                let (lo, hi) = range.ordered();
                for o in out.iter_mut() {
                    *o = o.clamp(lo, hi);
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

impl Drop for ParamState {
    /// A unit's param state is freed on the control side: a retired unit's
    /// comes back in its commit box (`Commit::spare_params`). One that
    /// allocated nothing (no declared params) may go anywhere.
    fn drop(&mut self) {
        if !self.ports.is_empty() {
            RtScope::check_outside("a unit's param state");
        }
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
        render_ramps(&mut r, &events, UnitParam::Cutoff, &mut out, |_| 0.0);
        assert_eq!(&out[..4], &[0.0; 4], "nothing before the event");
        assert_eq!(out[4], 25.0);
        assert_eq!(out[7], 100.0, "on target at the ramp's last frame");
        assert_eq!(out[11], 100.0, "held");
        assert_eq!(out[12], -5.0, "a step lands on its frame");
        // Another param's ramp is not this port's.
        let mut r = Ramp::default();
        render_ramps(&mut r, &events, UnitParam::Q, &mut out, |_| 0.0);
        assert!(out.iter().all(|&x| x == 0.0));
    }

    /// A segment and then a ramp on one frame, as a source's first events:
    /// the ramp replaces the segment from the base at that frame, as it
    /// would alone, under a base that moves every frame.
    ///
    /// Mutation (run): drop the base seed from `render_ramps`' segment arm
    /// → the ramp starts from the never-written value 0 (frame 4 reads 0
    /// for 2.5) → fails.
    #[test]
    fn a_ramp_after_a_first_segment_on_its_frame_starts_from_the_base() {
        use crate::event::ParamSegment;
        let at = Offset::new(4, Samples(8)).expect("in block");
        let seg = ParamSegment::new(
            ParamKey::<Hz>::CUTOFF,
            Hz(5.0),
            Hz(9.0),
            Samples(20),
            SegmentShape::Linear,
        );
        let ramp = ParamRamp::new(ParamKey::<Hz>::CUTOFF, Hz(0.0), Samples(2));
        let base = |i: usize| 1.0 + i as f32;
        let mut after_segment = [0.0f32; 8];
        let events = [Event::segment(at, seg), Event::ramp(at, ramp)];
        render_ramps(
            &mut Ramp::default(),
            &events,
            UnitParam::Cutoff,
            &mut after_segment,
            base,
        );
        let mut alone = [0.0f32; 8];
        let events = [Event::ramp(at, ramp)];
        render_ramps(
            &mut Ramp::default(),
            &events,
            UnitParam::Cutoff,
            &mut alone,
            base,
        );
        assert_eq!(after_segment, [1.0, 2.0, 3.0, 4.0, 2.5, 0.0, 0.0, 0.0]);
        assert_eq!(after_segment, alone, "as the ramp alone");
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
}
