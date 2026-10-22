//! Events: what flows on an event port.
//!
//! Events are **graph ports**, not a side channel, so the one compiler pass
//! that aligns audio (PDC) aligns notes and automation too. An event input
//! port may have several sources, merged deterministically by `(offset,
//! source order)`, where source order is the source port's `(NodeKey, port)`
//! — layering a keyboard and a clip is the normal case, not an edge case.
//! Automation is carried as **ramp** events ([`ParamRamp`]: relative, from
//! wherever the param stands) and as **curve segments** ([`ParamSegment`]:
//! absolute, between their own endpoints), both moving along a closed set of
//! shapes ([`SegmentShape`]) a param port evaluates per frame.
//!
//! # Why a raw UMP payload and not `tutti-midi-types`
//!
//! `tutti_midi_types::MidiEvent` is the right *shape* (a frame offset plus four
//! UMP words), but the crate links `midi2`, `midly`, `bitflags` and `thiserror`,
//! and this crate sits below everything that would ever build a node. [`Ump`]
//! carries the same four words, so converting at a node's edge is a copy of 16
//! bytes, and the graph does not link a MIDI codec to move them.

use std::cell::Cell;

use tutti_types::{ParamAddr, ParamKey, SamplePosition, Samples, Unit, UnitParam};

use crate::time::Offset;

/// Four raw Universal MIDI Packet words.
///
/// The meaningful prefix is determined by the message type in the first word's
/// top nibble, exactly as `tutti_midi_types::MidiEvent::data_words` reads it;
/// unused words are zero. The graph never inspects them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Ump(pub [u32; 4]);

/// A ramp of one parameter from the value it has at the event's frame to a
/// target, starting there: linear, or along a [`SegmentShape`]
/// ([`with_shape`](Self::with_shape)).
///
/// **Relative, unlike a [`ParamSegment`].** Its start value is whatever the
/// receiving port's source holds on the event's frame, so the shape is
/// evaluated exactly as a segment's is, with that value as `from`: frame `j`
/// after the event (`j = 0` its own) reads
/// `from + (target - from) · shape.eval((j + 1) / duration)`, and `target`
/// from frame `duration - 1` on. Re-sending one restarts it from where the
/// source stands.
///
/// A shape that needs `from` to be chosen ([`SegmentShape::Exp`]'s
/// geometric `k = ln(to / from)`) wants a [`ParamSegment`], which names its
/// start value: a shape is a function of progress alone.
///
/// Addressed by [`ParamAddr`], so it reaches both tutti's own parameters and a
/// foreign unit's (a hosted plugin's numeric id) without a later breaking
/// change:
///
/// - **Built-in params** are built from a typed [`ParamKey<U>`] and a `U`,
///   and read back the same way, so the unit is checked at both ends. In
///   between the value travels type-erased (the stable [`UnitParam`] id and
///   the raw `f32`), because one event port carries ramps for parameters of
///   different units and an `Event` has to be one `Copy` type. The raw value
///   is private: nothing reads a built-in ramp without naming its key.
/// - **Foreign params** ([`ParamRamp::foreign`]) carry an opaque `u32` id and
///   a raw value in the foreign unit's own normalisation. That is the C ABI
///   boundary the units rule stops at: tutti does not know the unit.
///
/// ```
/// use tutti_graph::{ParamRamp, SegmentShape};
/// use tutti_types::{Db, Hz, ParamKey, Samples};
/// let r = ParamRamp::new(ParamKey::<Hz>::CUTOFF, Hz(800.0), Samples(64));
/// assert_eq!(r.target(ParamKey::<Hz>::CUTOFF), Some(Hz(800.0)));
/// assert_eq!(r.target(ParamKey::<Db>::THRESHOLD), None); // another param
/// assert_eq!(r.foreign_target(7), None); // not a foreign param at all
/// assert_eq!(r.shape(), SegmentShape::Linear);
/// let eased = r.with_shape(SegmentShape::PowerIn(2.0));
/// assert_eq!(eased.shape(), SegmentShape::PowerIn(2.0));
/// ```
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ParamRamp {
    addr: ParamAddr,
    target: f32,
    // Frames, saturated to `u32` (what the param port counts in anyway):
    // a `usize` would align the whole event to 8 and push it past 32
    // bytes.
    duration: u32,
    // The shape, packed as a segment packs it (a tag byte and an `f32`),
    // so an `Event` stays 32 bytes.
    shape_param: f32,
    shape: ShapeTag,
}

impl ParamRamp {
    /// Ramp built-in parameter `key` linearly to `target` over `duration`
    /// (zero is a step).
    pub fn new<U: Unit<Raw = f32>>(key: ParamKey<U>, target: U, duration: Samples) -> Self {
        Self::packed(ParamAddr::Unit(key.id()), target.to_raw(), duration)
    }

    /// Ramp a foreign unit's parameter `id` linearly to the raw `target`
    /// over `duration`. The value is in the foreign unit's own terms (see the
    /// type docs).
    pub fn foreign(id: u32, target: f32, duration: Samples) -> Self {
        Self::packed(ParamAddr::Id(id), target, duration)
    }

    fn packed(addr: ParamAddr, target: f32, duration: Samples) -> Self {
        let (shape, shape_param) = SegmentShape::Linear.pack();
        Self {
            addr,
            target,
            duration: saturate(duration),
            shape_param,
            shape,
        }
    }

    /// This ramp along `shape` instead of a straight line: a glide that
    /// eases in or out from wherever the parameter stands. A zero duration
    /// is still a step.
    #[must_use]
    pub fn with_shape(mut self, shape: SegmentShape) -> Self {
        (self.shape, self.shape_param) = shape.pack();
        self
    }

    /// The shape it moves along ([`SegmentShape::Linear`] unless
    /// [`with_shape`](Self::with_shape) set one).
    pub fn shape(&self) -> SegmentShape {
        SegmentShape::unpack(self.shape, self.shape_param)
    }

    /// Which parameter this ramps — for routing on the address.
    pub fn addr(&self) -> ParamAddr {
        self.addr
    }

    /// The target, in `key`'s unit, if this ramp is for `key`.
    pub fn target<U: Unit<Raw = f32>>(&self, key: ParamKey<U>) -> Option<U> {
        (self.addr == ParamAddr::Unit(key.id())).then(|| U::from_raw(self.target))
    }

    /// The raw target, if this ramp is for foreign parameter `id`.
    pub fn foreign_target(&self, id: u32) -> Option<f32> {
        (self.addr == ParamAddr::Id(id)).then_some(self.target)
    }

    /// The target as the wire carries it, whatever the unit: for the
    /// graph's own param modulation, which is erased over the unit as the
    /// port it drives is.
    pub(crate) fn raw_target(&self) -> f32 {
        self.target
    }

    /// How long the ramp takes (saturated at `u32::MAX` frames, about a
    /// day at 48 kHz).
    pub fn duration(&self) -> Samples {
        Samples(self.duration as usize)
    }
}

/// A frame count as a `u32`, saturating.
fn saturate(n: Samples) -> u32 {
    u32::try_from(n.get()).unwrap_or(u32::MAX)
}

/// How a [`ParamSegment`] or a [`ParamRamp`] moves from its start value to
/// its end value: a
/// function `g` of its progress `t` in `[0, 1]`, with `g(0) = 0` and
/// `g(1) = 1`; the value is `from + (to - from) · g(t)`.
///
/// A **closed set**, so a segment stays a `Copy` value a few words long on
/// the audio thread rather than an `Arc<dyn Curve>`. Every shape is monotone
/// between its endpoints, so a segment never leaves `[from, to]`.
///
/// A parameter that is not finite, or out of the range a variant states,
/// evaluates as [`Linear`](Self::Linear): the audio thread never computes a
/// NaN from a shape.
///
/// **Platform figures.** `Linear`, `Hold`, and the power shapes at exponents
/// 0.5 and 1–5 evaluate with IEEE-exact operations (multiplication, `sqrt`),
/// so they render the same bits on every target. Other exponents (`powf`)
/// and [`Exp`](Self::Exp) (`exp_m1`) go through the platform's libm and may
/// differ in the last ulp between targets; within one target they are
/// deterministic, so a parallel render still equals the serial one.
///
/// ```
/// use tutti_graph::SegmentShape;
/// assert_eq!(SegmentShape::Linear.eval(0.25), 0.25);
/// assert_eq!(SegmentShape::PowerIn(2.0).eval(0.5), 0.25);
/// assert_eq!(SegmentShape::PowerOut(2.0).eval(0.5), 0.75);
/// assert_eq!(SegmentShape::Hold.eval(0.99), 0.0);
/// assert_eq!(SegmentShape::PowerIn(f32::NAN).eval(0.5), 0.5); // as linear
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum SegmentShape {
    /// `t`. The default: a [`ParamRamp`]'s shape unless it names one.
    #[default]
    Linear,
    /// `0` until the end, then `1`: the start value held, and a step onto
    /// the end value on the segment's last frame.
    Hold,
    /// `t^p`, `p > 0`: an ease-in. `p > 1` starts slow (`2` is a quadratic,
    /// `audio-automation`'s "exponential"), `p < 1` starts fast (`0.5` is a
    /// square root, its "logarithmic").
    PowerIn(f32),
    /// `1 - (1 - t)^p`, `p > 0`: the ease-out mirror of
    /// [`PowerIn`](Self::PowerIn).
    PowerOut(f32),
    /// [`PowerIn`](Self::PowerIn) over the first half and
    /// [`PowerOut`](Self::PowerOut) over the second, `p > 0`: an S-curve
    /// (`2` is the quadratic "S-curve").
    PowerInOut(f32),
    /// `(e^(k·t) - 1) / (e^k - 1)`: a true exponential with curvature `k`
    /// (`|k|` past 64 is clamped). `k > 0` starts slow, `k < 0` starts fast;
    /// `k` near 0 is linear. `k = ln(to / from)` makes the value geometric
    /// (a frequency sweep linear in pitch).
    Exp(f32),
}

impl SegmentShape {
    /// `g(t)`, `t` clamped to `[0, 1]` (a NaN `t` is 0). Allocation-free:
    /// what a param port calls once per frame.
    #[inline]
    pub fn eval(self, t: f32) -> f32 {
        let t = if t >= 0.0 { t.min(1.0) } else { 0.0 };
        match self {
            Self::Linear => t,
            Self::Hold => {
                if t < 1.0 {
                    0.0
                } else {
                    1.0
                }
            }
            Self::PowerIn(p) if valid_exponent(p) => pow(t, p),
            Self::PowerOut(p) if valid_exponent(p) => 1.0 - pow(1.0 - t, p),
            Self::PowerInOut(p) if valid_exponent(p) => {
                // `2^(p-1) · t^p` is `(2t)^p / 2`, which keeps the integer
                // exponents on multiplications.
                if t < 0.5 {
                    pow(2.0 * t, p) / 2.0
                } else {
                    1.0 - pow(2.0 - 2.0 * t, p) / 2.0
                }
            }
            Self::Exp(k) if k.is_finite() => {
                let k = k.clamp(-EXP_CURVATURE_MAX, EXP_CURVATURE_MAX);
                if k.abs() < 1e-4 {
                    t
                } else {
                    (k * t).exp_m1() / k.exp_m1()
                }
            }
            // An invalid parameter: linear (see the type docs).
            Self::PowerIn(_) | Self::PowerOut(_) | Self::PowerInOut(_) | Self::Exp(_) => t,
        }
    }

    /// The shape as its tag and parameter: how a segment and a ramp
    /// store it.
    pub(crate) fn pack(self) -> (ShapeTag, f32) {
        match self {
            Self::Linear => (ShapeTag::Linear, 0.0),
            Self::Hold => (ShapeTag::Hold, 0.0),
            Self::PowerIn(p) => (ShapeTag::PowerIn, p),
            Self::PowerOut(p) => (ShapeTag::PowerOut, p),
            Self::PowerInOut(p) => (ShapeTag::PowerInOut, p),
            Self::Exp(k) => (ShapeTag::Exp, k),
        }
    }

    /// The inverse of [`pack`](Self::pack).
    pub(crate) fn unpack(tag: ShapeTag, param: f32) -> Self {
        match tag {
            ShapeTag::Linear => Self::Linear,
            ShapeTag::Hold => Self::Hold,
            ShapeTag::PowerIn => Self::PowerIn(param),
            ShapeTag::PowerOut => Self::PowerOut(param),
            ShapeTag::PowerInOut => Self::PowerInOut(param),
            ShapeTag::Exp => Self::Exp(param),
        }
    }
}

/// The largest `|k|` [`SegmentShape::Exp`] evaluates: `e^64` is far inside
/// `f32`, so the ratio never overflows to `inf / inf`.
const EXP_CURVATURE_MAX: f32 = 64.0;

fn valid_exponent(p: f32) -> bool {
    p.is_finite() && p > 0.0
}

/// `x^p` for `x` in `[0, 1]`: the integer exponents up to 5 and the square
/// root by IEEE-exact operations, anything else through `powf`.
#[inline]
fn pow(x: f32, p: f32) -> f32 {
    if p == 1.0 {
        x
    } else if p == 2.0 {
        x * x
    } else if p == 3.0 {
        x * x * x
    } else if p == 4.0 {
        let x2 = x * x;
        x2 * x2
    } else if p == 5.0 {
        let x2 = x * x;
        x2 * x2 * x
    } else if p == 0.5 {
        x.sqrt()
    } else {
        x.powf(p)
    }
}

/// [`SegmentShape`]'s variant, as a byte: a segment or a ramp stores its
/// shape as this and an `f32`, so an [`Event`] stays 32 bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum ShapeTag {
    Linear,
    Hold,
    PowerIn,
    PowerOut,
    PowerInOut,
    Exp,
}

/// A curve segment of one parameter: from a start value to an end value
/// along a [`SegmentShape`], over a length in frames, evaluated by the param
/// port **per frame inside the block** — sample-accurate non-linear
/// automation, with no sub-chunking at breakpoints (doc 013, decision 7).
///
/// **Absolute, unlike a [`ParamRamp`].** A ramp moves from wherever its
/// source's value stands; a segment says both its endpoints and where along
/// itself it is at the event's frame. Re-sending the segment a producer is
/// on (each block, after a seek, a loop wrap, a tempo change) is therefore
/// idempotent: a producer can be a pure function of the transport, and a
/// segment entered part-way renders what one started at its beginning
/// would have there. The value at frame `j` after the event (`j = 0` is the
/// event's own frame) is
///
/// ```text
/// t = (position + j) / length
/// t < 1:  from + (to - from) · shape.eval(t)
/// t >= 1: to, held until the source's next event
/// ```
///
/// [`new`](Self::new) builds the ramp convention (`position = 1`: the
/// event's frame is one step in, `to` lands on frame `duration - 1`);
/// [`resumed`](Self::resumed) sets a fractional position and length, for a
/// producer whose breakpoints fall between frames. A **negative position**
/// is a delayed start: the source holds `from` until `position + j` reaches
/// 0, then moves. A length that is not finite and above zero (zero,
/// negative, infinite, NaN) is a step to `to` on the event's frame.
///
/// **Interrupted** by any later ramp or segment event for the same
/// parameter on the same source, from that event's frame: a segment jumps to
/// its own value there (it is absolute), a ramp moves on from the value the
/// interrupted segment had reached.
///
/// Addressed and typed as [`ParamRamp`] is (built-in params by
/// [`ParamKey<U>`], foreign ones by an opaque `u32`). A param port's range
/// is applied when the port reads the value, never to the endpoints.
///
/// ```
/// use tutti_graph::{ParamSegment, SegmentShape};
/// use tutti_types::{Hz, ParamKey, Samples};
/// let s = ParamSegment::new(
///     ParamKey::<Hz>::CUTOFF,
///     Hz(200.0),
///     Hz(800.0),
///     Samples(480),
///     SegmentShape::PowerIn(2.0),
/// );
/// assert_eq!(s.target(ParamKey::<Hz>::CUTOFF), Some(Hz(800.0)));
/// assert_eq!(s.start(ParamKey::<Hz>::CUTOFF), Some(Hz(200.0)));
/// assert_eq!(s.shape(), SegmentShape::PowerIn(2.0));
/// // Frame 239 is 240/480 of the way: a quadratic ease-in is a quarter up.
/// assert_eq!(s.raw_value_after(Samples(239)), 200.0 + 600.0 * 0.25);
/// assert_eq!(s.raw_value_after(Samples(479)), 800.0);
/// assert_eq!(s.arrives_after(), Some(Samples(479)));
/// ```
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ParamSegment {
    // The address, packed (a `ParamAddr` takes 8 bytes and would push an
    // `Event` past 32): a built-in param's `UnitParam` discriminant, or the
    // foreign id, told apart by `foreign`.
    id: u32,
    from: f32,
    to: f32,
    // Position and length in frames. `f32`, not `SamplePosition`'s `f64`:
    // this is where the types stop, because the event must stay 32 bytes.
    // An `f32` frame count is exact to 2^24 frames (≈ 5.8 minutes at
    // 48 kHz); past that a segment's progress rounds to a few frames.
    position: f32,
    length: f32,
    shape_param: f32,
    shape: ShapeTag,
    foreign: bool,
}

impl ParamSegment {
    /// A segment of built-in parameter `key` from `from` to `to` along
    /// `shape` over `duration` frames, in the ramp convention: the event's
    /// frame is one step in, and `to` lands on its last frame (a zero
    /// duration is a step to `to`).
    pub fn new<U: Unit<Raw = f32>>(
        key: ParamKey<U>,
        from: U,
        to: U,
        duration: Samples,
        shape: SegmentShape,
    ) -> Self {
        Self::unit(key.id(), from.to_raw(), to.to_raw(), duration, shape)
    }

    /// A segment of built-in parameter `param` in its raw `f32` domain, as
    /// [`new`](Self::new) otherwise: for a producer that is erased over the
    /// unit (a control lane built from a `ParamAddr`, doc 014 §4.1), and for
    /// a param with no [`ParamKey`] yet. The raw value is exactly what
    /// [`new`](Self::new) sends for a key with this id: a unit's raw value is
    /// its bits.
    pub fn unit(
        param: UnitParam,
        from: f32,
        to: f32,
        duration: Samples,
        shape: SegmentShape,
    ) -> Self {
        Self::packed(
            u32::from(u16::from(param)),
            false,
            from,
            to,
            duration,
            shape,
        )
    }

    /// A segment of a foreign unit's parameter `id`, in its own
    /// normalisation, as [`new`](Self::new) otherwise.
    pub fn foreign(id: u32, from: f32, to: f32, duration: Samples, shape: SegmentShape) -> Self {
        Self::packed(id, true, from, to, duration, shape)
    }

    fn packed(
        id: u32,
        foreign: bool,
        from: f32,
        to: f32,
        duration: Samples,
        shape: SegmentShape,
    ) -> Self {
        let (tag, shape_param) = shape.pack();
        Self {
            id,
            from,
            to,
            position: 1.0,
            length: duration.get() as f32,
            shape_param,
            shape: tag,
            foreign,
        }
    }

    /// This segment, `position` frames along a `length`-frame span at the
    /// event's frame: for a producer whose breakpoints fall between frames,
    /// or that enters a segment part-way (a seek, a loop wrap, each block's
    /// re-send). Both are fractional; see the type docs for the value each
    /// frame gets. A length that is not finite and above zero is a step to
    /// `to`; a negative position holds `from` until it reaches 0.
    pub fn resumed(mut self, position: SamplePosition, length: SamplePosition) -> Self {
        self.position = position.get() as f32;
        self.length = length.get() as f32;
        self
    }

    /// Which parameter this moves — for routing on the address.
    pub fn addr(&self) -> ParamAddr {
        if self.foreign {
            return ParamAddr::Id(self.id);
        }
        // Built from a `UnitParam` by `new`, so it converts back.
        u16::try_from(self.id)
            .ok()
            .and_then(|id| UnitParam::try_from(id).ok())
            .map_or(ParamAddr::Id(self.id), ParamAddr::Unit)
    }

    /// Whether this segment is for `addr`. No conversion: what the param
    /// port asks of every event, per event.
    #[inline]
    pub(crate) fn is_for(&self, addr: ParamAddr) -> bool {
        match addr {
            ParamAddr::Unit(p) => !self.foreign && self.id == u32::from(u16::from(p)),
            ParamAddr::Id(id) => self.foreign && self.id == id,
        }
    }

    /// The end value, in `key`'s unit, if this segment is for `key`.
    pub fn target<U: Unit<Raw = f32>>(&self, key: ParamKey<U>) -> Option<U> {
        self.is_for(ParamAddr::Unit(key.id()))
            .then(|| U::from_raw(self.to))
    }

    /// The start value, in `key`'s unit, if this segment is for `key`.
    pub fn start<U: Unit<Raw = f32>>(&self, key: ParamKey<U>) -> Option<U> {
        self.is_for(ParamAddr::Unit(key.id()))
            .then(|| U::from_raw(self.from))
    }

    /// The raw end value, if this segment is for foreign parameter `id`.
    pub fn foreign_target(&self, id: u32) -> Option<f32> {
        self.is_for(ParamAddr::Id(id)).then_some(self.to)
    }

    /// The raw start value, if this segment is for foreign parameter `id`.
    pub fn foreign_start(&self, id: u32) -> Option<f32> {
        self.is_for(ParamAddr::Id(id)).then_some(self.from)
    }

    /// The shape it moves along.
    pub fn shape(&self) -> SegmentShape {
        SegmentShape::unpack(self.shape, self.shape_param)
    }

    /// Its position along itself at the event's frame, in frames.
    pub fn position(&self) -> SamplePosition {
        SamplePosition(f64::from(self.position))
    }

    /// Its length, in frames.
    pub fn length(&self) -> SamplePosition {
        SamplePosition(f64::from(self.length))
    }

    /// The raw value `j` frames after the event's frame (`Samples(0)`: its
    /// own), whatever the unit: the value a param port gives this source
    /// there, for a node that takes point values (a hosted plugin's
    /// parameter) or a test. Past `u32::MAX` frames it reads the value
    /// there.
    pub fn raw_value_after(&self, j: Samples) -> f32 {
        segment_value(
            self.from,
            self.to,
            self.shape(),
            self.position,
            self.length,
            saturate(j),
        )
    }

    /// How many frames after the event's frame the segment arrives: the
    /// first `j` at which [`raw_value_after`](Self::raw_value_after) is the
    /// end value (`Samples(0)` for a step). `None` when it never arrives
    /// within `u32::MAX` frames. For a node that takes point values and
    /// must land the end value on its frame.
    pub fn arrives_after(&self) -> Option<Samples> {
        let (p, l) = (self.position, self.length);
        if segment_progress(p, l, 0).is_none() {
            return Some(Samples(0));
        }
        // Finite here. The guess is within a frame or two of the
        // predicate's answer, which decides.
        let guess = (l - p).ceil().max(0.0);
        if guess >= u32::MAX as f32 {
            return None;
        }
        let mut j = guess as u32;
        while j > 0 && segment_progress(p, l, j - 1).is_none() {
            j -= 1;
        }
        while segment_progress(p, l, j).is_some() {
            j = j.checked_add(1)?;
        }
        Some(Samples(j as usize))
    }

    /// The raw endpoints, position and length, for the graph's own param
    /// port, which is erased over the unit as the port it drives is.
    pub(crate) fn raw(&self) -> (f32, f32, f32, f32) {
        (self.from, self.to, self.position, self.length)
    }
}

/// A segment's value `j` frames after its event: `to` exactly once it
/// has arrived (see [`segment_progress`]), so a segment always lands
/// on its end value. The one definition the param port, the reference
/// interpreter and [`ParamSegment::raw_value_after`] share, as they share a
/// shaping LUT.
#[inline]
pub(crate) fn segment_value(
    from: f32,
    to: f32,
    shape: SegmentShape,
    position: f32,
    length: f32,
    j: u32,
) -> f32 {
    match segment_progress(position, length, j) {
        Some(t) => from + (to - from) * shape.eval(t),
        None => to,
    }
}

/// A segment's progress `j` frames after its event, `None` once it has
/// arrived: at 1 or past, NaN (a NaN position), or a length that is not
/// finite and above zero (a step). A negative progress (a delayed start) is
/// returned as is; the shape clamps it to 0.
#[inline]
pub(crate) fn segment_progress(position: f32, length: f32, j: u32) -> Option<f32> {
    if !(length > 0.0 && length.is_finite()) {
        return None;
    }
    let t = (position + j as f32) / length;
    (t < 1.0).then_some(t)
}

/// What an [`Event`] says.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum EventKind {
    /// A MIDI message, as UMP words.
    Midi(Ump),
    /// A parameter automation ramp.
    Ramp(ParamRamp),
    /// A parameter curve segment: a shaped, absolute move a param port
    /// evaluates per frame (see [`ParamSegment`]).
    Segment(ParamSegment),
    /// A chord or scale taking effect.
    Harmony(Harmony),
    /// A request to an **event source**: re-send where it stands, from this
    /// event's frame, as a resumed segment (doc 014 §6). What a sink that
    /// starts reading a source late (a control lane whose program newly
    /// reads a held note envelope) needs, since a source sends only when it
    /// moves. Delivered by [`Editor::schedule`](crate::Editor::schedule),
    /// which waits for the commit sent before it, so the re-send reaches the
    /// edges that commit placed. A node that sends nothing a sink could
    /// miss ignores it; so does every param port.
    Resync,
}

/// Whether a [`Harmony`] is a chord or a scale.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum HarmonyKind {
    /// The chord sounding from here: a root, a bass and the degrees.
    Chord,
    /// The scale (key) in force from here: a tonic and the degrees.
    Scale,
}

/// A harmonic context change — a chord or a scale taking effect, as a
/// sequencer's chord and scale lanes say — for a node that follows them (a
/// hosted VST3 plugin's chord and scale events).
///
/// Not MIDI: MIDI 2.0's Flex Data names a chord by type and a key by its
/// signature, which cannot carry an arbitrary degree set (a scale is any of
/// the 4096). Pitches are MIDI note numbers (`0..=127`); `degrees` is a 12-bit
/// mask relative to the root, bit `n` set when semitone `n` belongs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Harmony {
    kind: HarmonyKind,
    root: u8,
    bass: u8,
    degrees: u16,
}

impl Harmony {
    /// A chord on `root`, `bass` in the bass (equal to `root` in root
    /// position), sounding `degrees`. Pitches past 127 and mask bits past the
    /// twelfth are dropped.
    pub const fn chord(root: u8, bass: u8, degrees: u16) -> Self {
        Self {
            kind: HarmonyKind::Chord,
            root: root & 0x7f,
            bass: bass & 0x7f,
            degrees: degrees & 0xfff,
        }
    }

    /// A scale on the tonic `root` with `degrees`. As [`chord`](Self::chord);
    /// its bass is its root.
    pub const fn scale(root: u8, degrees: u16) -> Self {
        Self {
            kind: HarmonyKind::Scale,
            root: root & 0x7f,
            bass: root & 0x7f,
            degrees: degrees & 0xfff,
        }
    }

    /// Chord or scale.
    pub const fn kind(&self) -> HarmonyKind {
        self.kind
    }

    /// The root (a chord's) or tonic (a scale's), as a MIDI note number.
    pub const fn root(&self) -> u8 {
        self.root
    }

    /// The bass note, as a MIDI note number: a scale's is its root.
    pub const fn bass(&self) -> u8 {
        self.bass
    }

    /// The degrees, a 12-bit mask relative to the root.
    pub const fn degrees(&self) -> u16 {
        self.degrees
    }
}

/// One event on an event port: an offset into the current block, and what
/// happens there.
///
/// `offset` is an [`Offset`] — a position inside the block the event is
/// delivered in, never an absolute [`Frame`](tutti_types::Frame) (see
/// [`Offset`]). Slices of events handed to a node are
/// sorted by it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Event {
    /// Where in the block.
    pub offset: Offset,
    /// The payload.
    pub kind: EventKind,
}

// An event is what every event buffer, merge and delay FIFO holds by value,
// preallocated on the control side and copied on the audio thread: it must
// stay `Copy` and small, whatever payload is added (a curve segment packs
// its address and shape tag for this).
// Four UMP words cover every MIDI 2.0 message, 128-bit ones included, so a
// MIDI payload never needs to grow it. Asserted here rather than discovered
// as a slower merge.
const _: () = {
    const fn copy<T: Copy>() {}
    copy::<Event>();
    assert!(std::mem::size_of::<Event>() <= 32);
};

impl Event {
    /// A MIDI event at `offset`.
    pub const fn midi(offset: Offset, words: [u32; 4]) -> Self {
        Self {
            offset,
            kind: EventKind::Midi(Ump(words)),
        }
    }

    /// Whether this is a MIDI note-off: a MIDI 1.0 channel-voice note-off, a
    /// note-on at velocity 0 (the MIDI 1.0 spelling of the same thing), or a
    /// MIDI 2.0 channel-voice note-off. The one event the graph refuses to
    /// lose — see the delay FIFO's overflow policy.
    pub fn is_note_off(&self) -> bool {
        let EventKind::Midi(Ump(w)) = self.kind else {
            return false;
        };
        let mt = w[0] >> 28;
        let status = (w[0] >> 20) & 0xf;
        match mt {
            0x2 => status == 0x8 || (status == 0x9 && w[0] & 0x7f == 0),
            0x4 => status == 0x8,
            _ => false,
        }
    }

    /// A parameter ramp starting at `offset`.
    pub const fn ramp(offset: Offset, ramp: ParamRamp) -> Self {
        Self {
            offset,
            kind: EventKind::Ramp(ramp),
        }
    }

    /// A parameter curve segment, its event frame at `offset`.
    pub const fn segment(offset: Offset, segment: ParamSegment) -> Self {
        Self {
            offset,
            kind: EventKind::Segment(segment),
        }
    }

    /// A [`Resync`](EventKind::Resync) request at `offset`.
    pub const fn resync(offset: Offset) -> Self {
        Self {
            offset,
            kind: EventKind::Resync,
        }
    }

    /// A chord or scale taking effect at `offset`.
    pub const fn harmony(offset: Offset, harmony: Harmony) -> Self {
        Self {
            offset,
            kind: EventKind::Harmony(harmony),
        }
    }
}

/// Why a slice is not a valid [`SortedEvents`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventOrderError {
    /// The event at this index has a smaller offset than the one before it.
    Unsorted {
        /// Index into the slice.
        at: usize,
    },
    /// The event at this index is at or past the end of the block.
    OutOfBlock {
        /// Index into the slice.
        at: usize,
    },
}

/// A block's events for one port: **sorted by offset, every offset inside the
/// block** — by construction.
///
/// The only ways to get one are [`new`](Self::new), which checks,
/// [`sort`](Self::sort), which sorts (stably, so equal offsets keep their
/// order; control side — the sort may allocate), and the executor, whose event
/// slots are sorted and in-block by the rules of [`EventWriter`] and the
/// merge. A node reading a `SortedEvents` never re-checks either property.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SortedEvents<'a> {
    events: &'a [Event],
}

impl<'a> SortedEvents<'a> {
    /// No events.
    pub const EMPTY: SortedEvents<'static> = SortedEvents { events: &[] };

    /// `events`, if they are sorted by offset and every offset is below
    /// `frames`.
    ///
    /// # Errors
    ///
    /// [`EventOrderError::OutOfBlock`] or [`EventOrderError::Unsorted`],
    /// naming the first offending index.
    pub fn new(events: &'a [Event], frames: usize) -> Result<Self, EventOrderError> {
        for (i, e) in events.iter().enumerate() {
            if e.offset.index() >= frames {
                return Err(EventOrderError::OutOfBlock { at: i });
            }
            if i > 0 && events[i - 1].offset > e.offset {
                return Err(EventOrderError::Unsorted { at: i });
            }
        }
        Ok(Self { events })
    }

    /// Sorts `events` in place by offset (stably), then views them. Fails only
    /// when an offset is outside the block. Control side: may allocate.
    ///
    /// # Errors
    ///
    /// [`EventOrderError::OutOfBlock`], naming the first event at or past
    /// `frames`; `events` is left unsorted.
    pub fn sort(events: &'a mut [Event], frames: usize) -> Result<Self, EventOrderError> {
        if let Some(at) = events.iter().position(|e| e.offset.index() >= frames) {
            return Err(EventOrderError::OutOfBlock { at });
        }
        events.sort_by_key(|e| e.offset);
        Ok(Self { events })
    }

    /// Wrap a slice the executor's own rules keep sorted and in-block.
    /// Checked in debug builds.
    pub(crate) fn trusted(events: &'a [Event], frames: usize) -> Self {
        debug_assert_eq!(Self::new(events, frames).map(|_| ()), Ok(()));
        Self { events }
    }

    /// The events.
    pub fn as_slice(&self) -> &'a [Event] {
        self.events
    }

    /// Number of events.
    pub fn len(&self) -> usize {
        self.events.len()
    }

    /// Whether there are none.
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    /// The events, in order.
    pub fn iter(&self) -> std::slice::Iter<'a, Event> {
        self.events.iter()
    }
}

/// A block split at its event offsets: the iterator
/// [`Io::sub_blocks`](crate::Io::sub_blocks) returns.
///
/// Yields `(range, events)` pairs that tile the block in order: `range` is a
/// run of frames (slice indices into the block's buffers), and `events` are
/// exactly the events at `range.start` — so a node that applies `events`,
/// then renders `range`, applies every event on its own frame. A block with
/// no events is one chunk; several events on one offset arrive together, in
/// their delivered order; an event on the last frame gets a one-frame chunk.
///
/// Zero allocation: it walks the event slice it was built from.
#[derive(Clone, Debug)]
pub struct SubBlocks<'a> {
    events: &'a [Event],
    pos: usize,
    frames: usize,
}

impl<'a> SortedEvents<'a> {
    /// Split a block of `frames` at these events' offsets. `frames` is the
    /// length these events were checked against.
    pub(crate) fn sub_blocks(self, frames: usize) -> SubBlocks<'a> {
        SubBlocks {
            events: self.events,
            pos: 0,
            frames,
        }
    }
}

impl<'a> Iterator for SubBlocks<'a> {
    type Item = (std::ops::Range<usize>, SortedEvents<'a>);

    fn next(&mut self) -> Option<Self::Item> {
        if self.pos >= self.frames {
            return None;
        }
        // Every event before `pos` was yielded with an earlier chunk, so the
        // ones at `pos` are the prefix; `<=` rather than `==` only so a
        // malformed slice could never stall the walk.
        let here = self
            .events
            .iter()
            .take_while(|e| e.offset.index() <= self.pos)
            .count();
        let (now, rest) = self.events.split_at(here);
        let end = rest
            .first()
            .map_or(self.frames, |e| e.offset.index())
            .min(self.frames);
        let range = self.pos..end;
        self.events = rest;
        self.pos = end;
        Some((range, SortedEvents { events: now }))
    }
}

impl std::iter::FusedIterator for SubBlocks<'_> {}

impl<'a> IntoIterator for SortedEvents<'a> {
    type Item = &'a Event;
    type IntoIter = std::slice::Iter<'a, Event>;
    fn into_iter(self) -> Self::IntoIter {
        self.events.iter()
    }
}

impl std::ops::Deref for SortedEvents<'_> {
    type Target = [Event];
    fn deref(&self) -> &[Event] {
        self.events
    }
}

/// Why [`EventWriter::push`] refused an event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventRejected {
    /// The port's preallocated buffer is full. The event is dropped; the
    /// executor counts it (see
    /// [`Executor::dropped_events`](crate::Executor::dropped_events)).
    Full,
    /// `offset` is at or past the end of the block.
    OutOfBlock,
    /// `offset` is before the previous event's. A port's events are delivered
    /// sorted, and sorting on the audio thread would mean either an allocating
    /// stable sort or an unstable one that reorders equal offsets — so the
    /// writer requires order instead of restoring it.
    OutOfOrder,
}

/// The preallocated sink a node writes one event output port through.
///
/// Never allocates: it refuses past its capacity rather than growing, and the
/// capacity was reserved on the control thread.
pub struct EventWriter<'a> {
    // `None` only for a detached writer, which the executor uses to fill the
    // unused tail of its fixed-size port table and never hands to a node.
    buf: Option<&'a mut Vec<Event>>,
    cap: usize,
    frames: u32,
    // A shared counter rather than a field: the executor reads it after the
    // node returns, while the writer itself is still borrowed by the `Io` the
    // node consumed.
    dropped: Option<&'a Cell<u32>>,
}

impl<'a> EventWriter<'a> {
    /// A writer over `buf`, which the caller has cleared. At most `cap` events
    /// are accepted, and only at offsets `< frames`.
    pub(crate) fn new(
        buf: &'a mut Vec<Event>,
        cap: usize,
        frames: u32,
        dropped: &'a Cell<u32>,
    ) -> Self {
        debug_assert!(buf.is_empty());
        Self {
            buf: Some(buf),
            cap,
            frames,
            dropped: Some(dropped),
        }
    }

    /// A writer that accepts nothing: a placeholder for an unused port slot.
    pub(crate) const fn detached() -> Self {
        Self {
            buf: None,
            cap: 0,
            frames: 0,
            dropped: None,
        }
    }

    fn reject(&self, why: EventRejected) -> Result<(), EventRejected> {
        if let Some(d) = self.dropped {
            d.set(d.get() + 1);
        }
        Err(why)
    }

    /// Appends `event`. Offsets must be non-decreasing and inside the block.
    /// Audio thread: never allocates.
    ///
    /// # Errors
    ///
    /// [`EventRejected::OutOfBlock`], [`EventRejected::OutOfOrder`], or
    /// [`EventRejected::Full`] when the port's declared capacity is reached.
    /// A refused event is dropped and counted in
    /// [`Executor::dropped_events`](crate::Executor::dropped_events); the
    /// events already written stand.
    pub fn push(&mut self, event: Event) -> Result<(), EventRejected> {
        if event.offset.get() >= self.frames {
            return self.reject(EventRejected::OutOfBlock);
        }
        if self.len() >= self.cap {
            return self.reject(EventRejected::Full);
        }
        let Some(buf) = self.buf.as_deref_mut() else {
            return self.reject(EventRejected::Full);
        };
        if buf.last().is_some_and(|e| e.offset > event.offset) {
            return self.reject(EventRejected::OutOfOrder);
        }
        buf.push(event);
        Ok(())
    }

    /// How many events this port has accepted so far this block.
    pub fn len(&self) -> usize {
        self.buf.as_ref().map_or(0, |b| b.len())
    }

    /// Whether nothing has been written yet this block.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Merge `sources` (each sorted by offset) into `out`, ordered by
/// `(offset, source index)`. At most `cap` events land; the rest are counted.
///
/// A k-way merge rather than concatenate-and-sort: the stable sort that would
/// give the same order allocates, and the unstable one does not give it.
pub(crate) fn merge_into(sources: &[&[Event]], out: &mut Vec<Event>, cap: usize) -> u32 {
    let mut heads = [0usize; crate::MAX_PORTS];
    let heads = &mut heads[..sources.len()];
    let mut dropped = 0u32;
    loop {
        let mut best: Option<(Offset, usize)> = None;
        for (i, src) in sources.iter().enumerate() {
            if let Some(e) = src.get(heads[i]) {
                // Strict `<`: on a tie the lower source index keeps it.
                if best.is_none_or(|(o, _)| e.offset < o) {
                    best = Some((e.offset, i));
                }
            }
        }
        let Some((_, i)) = best else { break };
        let e = sources[i][heads[i]];
        heads[i] += 1;
        if out.len() < cap {
            out.push(e);
        } else {
            dropped += 1;
        }
    }
    dropped
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(offset: u32, tag: u32) -> Event {
        Event::midi(Offset::raw(offset), [tag, 0, 0, 0])
    }

    /// Ties go to the earlier source; otherwise offset order.
    ///
    /// Mutation: `<` → `<=` in `merge_into` → the tie at offset 3 flips to
    /// source 1 first → fails.
    #[test]
    fn merge_orders_by_offset_then_source() {
        let a = [ev(0, 1), ev(3, 2), ev(9, 3)];
        let b = [ev(3, 10), ev(4, 11)];
        let mut out = Vec::with_capacity(8);
        let dropped = merge_into(&[&a, &b], &mut out, 8);
        assert_eq!(dropped, 0);
        let tags: Vec<u32> = out
            .iter()
            .map(|e| match e.kind {
                EventKind::Midi(Ump(w)) => w[0],
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(tags, vec![1, 2, 10, 11, 3]);
    }

    /// The merge refuses past capacity and says how many it refused.
    ///
    /// Mutation: drop the `out.len() < cap` check → `out` grows to 5 → fails.
    #[test]
    fn merge_counts_what_did_not_fit() {
        let a = [ev(0, 1), ev(1, 2), ev(2, 3)];
        let b = [ev(0, 4), ev(5, 5)];
        let mut out = Vec::with_capacity(8);
        assert_eq!(merge_into(&[&a, &b], &mut out, 3), 2);
        assert_eq!(out.len(), 3);
    }

    /// `SortedEvents::new` refuses what is unsorted or out of the block;
    /// `sort` repairs order (stably) and still refuses out-of-block.
    ///
    /// Mutation: drop the `Unsorted` check in `new` → the unsorted slice is
    /// accepted → fails. Replace `sort_by_key` with `sort_unstable_by_key` →
    /// usually still passes here, which is why the tie case below checks the
    /// payload order explicitly on a slice long enough for the unstable sort
    /// to reorder.
    #[test]
    fn sorted_events_are_sorted_and_in_block() {
        let unsorted = [ev(3, 0), ev(1, 1)];
        assert_eq!(
            SortedEvents::new(&unsorted, 8),
            Err(EventOrderError::Unsorted { at: 1 })
        );
        let late = [ev(1, 0), ev(8, 1)];
        assert_eq!(
            SortedEvents::new(&late, 8),
            Err(EventOrderError::OutOfBlock { at: 1 })
        );
        assert!(SortedEvents::new(&[ev(0, 0), ev(0, 1), ev(7, 2)], 8).is_ok());

        let mut v: Vec<Event> = (0..64).map(|i| ev(i % 3, i)).collect();
        let sorted = SortedEvents::sort(&mut v, 8).expect("in block");
        let tags: Vec<u32> = sorted
            .iter()
            .map(|e| match e.kind {
                EventKind::Midi(Ump(w)) => w[0],
                _ => unreachable!(),
            })
            .collect();
        let mut want: Vec<u32> = (0..64).collect();
        want.sort_by_key(|i| i % 3);
        assert_eq!(tags, want, "stable: equal offsets keep their order");
        let mut late = vec![ev(9, 0)];
        assert!(SortedEvents::sort(&mut late, 8).is_err());
    }

    /// The writer enforces the three invariants a consumer relies on.
    ///
    /// Mutation: delete the `OutOfOrder` branch → the second push succeeds →
    /// fails.
    #[test]
    fn writer_rejects_out_of_order_out_of_block_and_overflow() {
        let mut buf = Vec::with_capacity(4);
        let dropped = Cell::new(0);
        let mut w = EventWriter::new(&mut buf, 2, 16, &dropped);
        assert_eq!(w.push(ev(5, 0)), Ok(()));
        assert_eq!(w.push(ev(4, 0)), Err(EventRejected::OutOfOrder));
        assert_eq!(w.push(ev(16, 0)), Err(EventRejected::OutOfBlock));
        assert_eq!(w.push(ev(5, 0)), Ok(()));
        assert_eq!(w.push(ev(6, 0)), Err(EventRejected::Full));
        assert_eq!(w.len(), 2);
        assert_eq!(dropped.get(), 3);
    }

    /// The splitter under test, written the obvious way: every distinct
    /// offset (and 0) starts a chunk, each chunk runs to the next start, and
    /// its events are those whose offset equals its start.
    fn hand_split(events: &[Event], frames: usize) -> Vec<(std::ops::Range<usize>, Vec<Event>)> {
        let mut starts: Vec<usize> = std::iter::once(0)
            .chain(events.iter().map(|e| e.offset.index()))
            .collect();
        starts.sort_unstable();
        starts.dedup();
        let ends = starts
            .iter()
            .skip(1)
            .copied()
            .chain(std::iter::once(frames));
        starts
            .iter()
            .zip(ends)
            .map(|(&a, b)| {
                let here = events
                    .iter()
                    .filter(|e| e.offset.index() == a)
                    .copied()
                    .collect();
                (a..b, here)
            })
            .collect()
    }

    fn split(events: &[Event], frames: usize) -> Vec<(std::ops::Range<usize>, Vec<Event>)> {
        SortedEvents::new(events, frames)
            .expect("sorted and in the block")
            .sub_blocks(frames)
            .map(|(r, e)| (r, e.to_vec()))
            .collect()
    }

    /// The edges `sub_blocks` has to get right, spelled out: an event at
    /// offset 0 (no empty leading chunk), several on one offset (one chunk,
    /// all of them, in order), one on the last frame (a one-frame chunk), and
    /// no events at all (the whole block).
    ///
    /// Mutation: in `SubBlocks::next`, split off only the *first* event at
    /// `pos` (`split_at(here.min(1))`) → the second event at offset 5 comes
    /// out in a zero-length chunk of its own → fails.
    #[test]
    fn sub_blocks_split_at_every_offset() {
        let evs = [ev(0, 1), ev(5, 2), ev(5, 3), ev(7, 4)];
        let got = split(&evs, 8);
        assert_eq!(
            got,
            vec![
                (0..5, vec![ev(0, 1)]),
                (5..7, vec![ev(5, 2), ev(5, 3)]),
                (7..8, vec![ev(7, 4)]),
            ]
        );
        assert_eq!(split(&[], 8), vec![(0..8, vec![])]);
        assert_eq!(
            split(&[ev(3, 9)], 8),
            vec![(0..3, vec![]), (3..8, vec![ev(3, 9)])]
        );
        assert_eq!(split(&evs, 8), hand_split(&evs, 8));
    }

    proptest::proptest! {
        /// Against the hand-rolled splitter, on random sorted offsets — with
        /// offset 0, the last frame and repeats all drawn often — and random
        /// block lengths including 1.
        ///
        /// Mutation: yield all but the last of several events that share an
        /// offset → the count and the comparison both fail.
        #[test]
        fn sub_blocks_match_a_hand_rolled_splitter(
            frames in 1usize..40,
            raw in proptest::collection::vec(0u32..6, 0..24),
        ) {
            // Bias toward the edges: 0 → offset 0, 5 → the last frame,
            // anything else → a frame in between (repeats are frequent).
            let last = frames as u32 - 1;
            let mut offsets: Vec<u32> = raw
                .iter()
                .enumerate()
                .map(|(i, &r)| match r {
                    0 => 0,
                    5 => last,
                    _ => (r * 7 + i as u32) % frames as u32,
                })
                .collect();
            offsets.sort_unstable();
            let evs: Vec<Event> = offsets
                .iter()
                .enumerate()
                .map(|(i, &o)| ev(o, i as u32))
                .collect();
            let got = split(&evs, frames);
            proptest::prop_assert_eq!(&got, &hand_split(&evs, frames));
            // The chunks tile the block and every event comes out once.
            let covered: usize = got.iter().map(|(r, _)| r.len()).sum();
            proptest::prop_assert_eq!(covered, frames);
            let n: usize = got.iter().map(|(_, e)| e.len()).sum();
            proptest::prop_assert_eq!(n, evs.len());
        }
    }

    /// A foreign ramp and a built-in ramp never answer for each other, even
    /// when the foreign id equals the built-in's discriminant.
    ///
    /// Mutation: compare only the numeric id in `target` (drop the `Unit`
    /// arm) → the foreign ramp with id 0 reads back as a cutoff → fails.
    #[test]
    fn foreign_and_builtin_ramps_are_distinct_addresses() {
        use tutti_types::{Hz, ParamKey};
        let foreign = ParamRamp::foreign(
            u16::from(ParamKey::<Hz>::CUTOFF.id()) as u32,
            0.5,
            Samples(0),
        );
        assert_eq!(foreign.target(ParamKey::<Hz>::CUTOFF), None);
        assert_eq!(foreign.foreign_target(0), Some(0.5));
        let builtin = ParamRamp::new(ParamKey::<Hz>::CUTOFF, Hz(100.0), Samples(0));
        assert_eq!(builtin.foreign_target(0), None);
    }

    /// The three MIDI spellings of a note-off are recognised, and a note-on
    /// or a ramp is not one.
    ///
    /// Mutation: drop the velocity-0 note-on arm → fails.
    #[test]
    fn note_offs_are_recognised() {
        let m1 = |status: u32, vel: u32| {
            Event::midi(
                Offset::ZERO,
                [0x2000_0000 | (status << 16) | (60 << 8) | vel, 0, 0, 0],
            )
        };
        assert!(m1(0x80, 64).is_note_off());
        assert!(m1(0x90, 0).is_note_off());
        assert!(!m1(0x90, 64).is_note_off());
        assert!(Event::midi(Offset::ZERO, [0x4080_3c00, 0x8000_0000, 0, 0]).is_note_off());
        assert!(!Event::midi(Offset::ZERO, [0x4090_3c00, 0x8000_0000, 0, 0]).is_note_off());
        assert!(!Event::ramp(Offset::ZERO, ParamRamp::foreign(1, 0.0, Samples(0))).is_note_off());
    }

    /// Every shape runs from 0 to 1, monotonically, at its analytic figures
    /// (closed forms in `f64`, within a tolerance: `Exp` and a non-integer
    /// power go through libm), and a shape with an unusable parameter is
    /// linear.
    ///
    /// Mutations (run), each fails: `PowerOut` as `pow(t, p)` (the ease-in);
    /// `PowerInOut`'s first half as `pow(t, p)` (no `2t`); `Exp` with
    /// `exp_m1(k)` and `exp_m1(k t)` swapped; dropping the `valid_exponent`
    /// guard (`PowerIn(-1)` gives `inf` at 0).
    #[test]
    fn every_shape_meets_its_closed_form() {
        use SegmentShape as S;
        let closed = |s: S, t: f64| -> f64 {
            match s {
                S::Linear => t,
                S::Hold => f64::from(u8::from(t >= 1.0)),
                S::PowerIn(p) => t.powf(f64::from(p)),
                S::PowerOut(p) => 1.0 - (1.0 - t).powf(f64::from(p)),
                S::PowerInOut(p) if t < 0.5 => 2f64.powf(f64::from(p) - 1.0) * t.powf(f64::from(p)),
                S::PowerInOut(p) => 1.0 - (2.0 - 2.0 * t).powf(f64::from(p)) / 2.0,
                S::Exp(k) => {
                    let k = f64::from(k);
                    ((k * t).exp() - 1.0) / (k.exp() - 1.0)
                }
            }
        };
        let shapes = [
            S::Linear,
            S::Hold,
            S::PowerIn(2.0),
            S::PowerIn(0.5),
            S::PowerIn(5.0),
            S::PowerIn(1.7),
            S::PowerOut(3.0),
            S::PowerInOut(2.0),
            S::PowerInOut(4.0),
            S::Exp(5.0),
            S::Exp(-3.0),
        ];
        for s in shapes {
            assert_eq!(s.eval(0.0), 0.0, "{s:?} starts at 0");
            assert_eq!(s.eval(1.0), 1.0, "{s:?} ends at 1");
            let mut prev = 0.0f32;
            for i in 0..=1000 {
                let t = i as f32 / 1000.0;
                let g = s.eval(t);
                let want = closed(s, f64::from(t));
                assert!(
                    (f64::from(g) - want).abs() < 2e-6,
                    "{s:?} at {t}: {g} vs {want}"
                );
                assert!(g >= prev, "{s:?} is monotone at {t}");
                prev = g;
            }
        }
        for s in [
            S::PowerIn(-1.0),
            S::PowerOut(0.0),
            S::PowerInOut(f32::INFINITY),
            S::Exp(f32::NAN),
        ] {
            assert_eq!(s.eval(0.0), 0.0, "{s:?}");
            assert_eq!(s.eval(0.3), 0.3, "{s:?} is linear");
        }
        // Out of range and NaN progress: clamped, never a NaN.
        assert_eq!(S::Exp(2.0).eval(-1.0), 0.0);
        assert_eq!(S::PowerIn(2.0).eval(f32::NAN), 0.0);
        assert!(S::Exp(1e9).eval(0.5).is_finite());
    }

    /// A segment's address round-trips, built-in and foreign never answer
    /// for each other, and its shape and timing read back as built.
    ///
    /// Mutations (run), each fails: drop the `foreign` test in
    /// `ParamSegment::is_for` (the foreign segment with id 0 reads back as a
    /// cutoff); the step guard in `segment_progress` as `length == 0.0` (a
    /// negative or infinite length holds `from`).
    #[test]
    fn a_segment_reads_back_as_built() {
        use tutti_types::{Hz, ParamKey};
        let key = ParamKey::<Hz>::CUTOFF;
        let id = u32::from(u16::from(key.id()));
        let foreign = ParamSegment::foreign(id, 0.25, 0.75, Samples(8), SegmentShape::Exp(2.0));
        assert_eq!(foreign.target(key), None);
        assert_eq!(foreign.foreign_target(id), Some(0.75));
        assert_eq!(foreign.foreign_start(id), Some(0.25));
        assert_eq!(foreign.addr(), ParamAddr::Id(id));
        let s = ParamSegment::new(
            key,
            Hz(1.0),
            Hz(2.0),
            Samples(8),
            SegmentShape::PowerOut(3.0),
        )
        .resumed(SamplePosition(2.5), SamplePosition(10.0));
        assert_eq!(s.addr(), ParamAddr::Unit(key.id()));
        assert_eq!(s.foreign_target(id), None);
        assert_eq!(s.start(key), Some(Hz(1.0)));
        assert_eq!(s.shape(), SegmentShape::PowerOut(3.0));
        assert_eq!(s.position(), SamplePosition(2.5));
        assert_eq!(s.length(), SamplePosition(10.0));
        // 2.5 frames along at its event, 7.5 more to go: the end value
        // lands on frame 8, held after.
        assert!(s.raw_value_after(Samples(7)) < 2.0);
        assert_eq!(s.raw_value_after(Samples(8)), 2.0);
        assert_eq!(s.raw_value_after(Samples(1_000)), 2.0);
        assert_eq!(s.arrives_after(), Some(Samples(8)));
        // A zero length is a step.
        let step = ParamSegment::new(key, Hz(1.0), Hz(2.0), Samples(0), SegmentShape::Hold);
        assert_eq!(step.raw_value_after(Samples(0)), 2.0);
        assert_eq!(step.arrives_after(), Some(Samples(0)));
        // A length that is not above zero and finite is a step too, never a
        // segment held at its start.
        for l in [-4.0, f64::INFINITY, f64::NAN] {
            let s = step.resumed(SamplePosition(1.0), SamplePosition(l));
            assert_eq!(s.raw_value_after(Samples(0)), 2.0, "length {l}");
            assert_eq!(s.arrives_after(), Some(Samples(0)), "length {l}");
        }
        // A negative position holds `from` until it reaches 0, then moves.
        let late = ParamSegment::new(key, Hz(1.0), Hz(2.0), Samples(0), SegmentShape::Linear)
            .resumed(SamplePosition(-3.0), SamplePosition(4.0));
        assert_eq!(late.raw_value_after(Samples(2)), 1.0);
        assert_eq!(late.raw_value_after(Samples(3)), 1.0);
        assert_eq!(late.raw_value_after(Samples(5)), 1.5);
        assert_eq!(late.arrives_after(), Some(Samples(7)));
    }
}
