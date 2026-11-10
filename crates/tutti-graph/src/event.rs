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
//! shapes ([`Law`]) a param port evaluates per frame.
//!
//!
//! A MIDI event carries a [`tutti_ump::Ump`]. The graph reads one fact of it,
//! [`Event::is_note_off`], which the delay FIFO's overflow policy needs, and
//! that decode is `tutti-ump`'s.

use std::cell::Cell;

use tutti_curve::Law;
use tutti_types::{ParamAddr, ParamKey, SamplePosition, Samples, Unit, UnitAddr};
use tutti_ump::Ump;

use crate::time::Offset;

/// A ramp of one parameter from the value it has at the event's frame to a
/// target, starting there: linear, or along a [`Law`]
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
/// A shape that needs `from` to be chosen ([`Law::Exp`]'s
/// geometric `k = ln(to / from)`) wants a [`ParamSegment`], which names its
/// start value: a shape is a function of progress alone.
///
/// Addressed by [`ParamAddr`], so it reaches both tutti's own parameters and a
/// foreign unit's (a hosted plugin's numeric id) without a later breaking
/// change:
///
/// - **Built-in params** are built from a typed [`ParamKey<U>`] and a `U`,
///   and read back the same way, so the unit is checked at both ends. In
///   between the value travels type-erased (the stable [`UnitAddr`] id and
///   the raw `f32`), because one event port carries ramps for parameters of
///   different units and an `Event` has to be one `Copy` type. The raw value
///   is private: nothing reads a built-in ramp without naming its key.
/// - **Foreign params** ([`ParamRamp::foreign`]) carry an opaque `u32` id and
///   a raw value in the foreign unit's own normalisation. That is the C ABI
///   boundary the units rule stops at: tutti does not know the unit.
///
/// ```
/// use tutti_graph::{ParamRamp, Law};
/// use tutti_types::{Db, Hz, ParamKey, Samples};
/// let r = ParamRamp::new(ParamKey::<Hz>::CUTOFF, Hz(800.0), Samples(64));
/// assert_eq!(r.target(ParamKey::<Hz>::CUTOFF), Some(Hz(800.0)));
/// assert_eq!(r.target(ParamKey::<Db>::THRESHOLD), None); // another param
/// assert_eq!(r.foreign_target(7), None); // not a foreign param at all
/// assert_eq!(r.shape(), Law::Linear);
/// let eased = r.with_shape(Law::PowerIn(2.0));
/// assert_eq!(eased.shape(), Law::PowerIn(2.0));
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
        Self::packed(ParamAddr::unit(key.addr()), target.to_raw(), duration)
    }

    /// Ramp a foreign unit's parameter `id` linearly to the raw `target`
    /// over `duration`. The value is in the foreign unit's own terms (see the
    /// type docs).
    ///
    /// A hosted plugin's node starts the ramp from where the parameter
    /// stood at the end of the frame before the ramp's (the segment it was
    /// on, else the value the node last gave it). On a parameter the node has
    /// never given a value it knows no start, and the ramp is a jump to
    /// `target` on its frame, whatever its duration and shape.
    pub fn foreign(id: u32, target: f32, duration: Samples) -> Self {
        Self::packed(ParamAddr::Id(id), target, duration)
    }

    fn packed(addr: ParamAddr, target: f32, duration: Samples) -> Self {
        let (shape, shape_param) = ShapeTag::pack(Law::Linear);
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
    pub fn with_shape(mut self, shape: Law) -> Self {
        (self.shape, self.shape_param) = ShapeTag::pack(shape);
        self
    }

    /// The shape it moves along ([`Law::Linear`] unless
    /// [`with_shape`](Self::with_shape) set one).
    pub fn shape(&self) -> Law {
        self.shape.unpack(self.shape_param)
    }

    /// Which parameter this ramps — for routing on the address.
    pub fn addr(&self) -> ParamAddr {
        self.addr
    }

    /// The target, in `key`'s unit, if this ramp is for `key`.
    pub fn target<U: Unit<Raw = f32>>(&self, key: ParamKey<U>) -> Option<U> {
        (self.addr == ParamAddr::unit(key.addr())).then(|| U::from_raw(self.target))
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

/// A [`Law`]'s variant, as a byte: a segment or a ramp stores its law as this
/// and an `f32`, so an [`Event`] stays 32 bytes.
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

impl ShapeTag {
    /// The law as its tag and parameter: how a segment and a ramp store it.
    fn pack(law: Law) -> (Self, f32) {
        match law {
            Law::Linear => (Self::Linear, 0.0),
            Law::Hold => (Self::Hold, 0.0),
            Law::PowerIn(p) => (Self::PowerIn, p),
            Law::PowerOut(p) => (Self::PowerOut, p),
            Law::PowerInOut(p) => (Self::PowerInOut, p),
            Law::Exp(k) => (Self::Exp, k),
        }
    }

    /// The inverse of [`pack`](Self::pack).
    fn unpack(self, param: f32) -> Law {
        match self {
            Self::Linear => Law::Linear,
            Self::Hold => Law::Hold,
            Self::PowerIn => Law::PowerIn(param),
            Self::PowerOut => Law::PowerOut(param),
            Self::PowerInOut => Law::PowerInOut(param),
            Self::Exp => Law::Exp(param),
        }
    }
}

/// A curve segment of one parameter: from a start value to an end value
/// along a [`Law`], over a length in frames, evaluated by the param
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
/// use tutti_graph::{ParamSegment, Law};
/// use tutti_types::{Hz, ParamKey, Samples};
/// let s = ParamSegment::new(
///     ParamKey::<Hz>::CUTOFF,
///     Hz(200.0),
///     Hz(800.0),
///     Samples(480),
///     Law::PowerIn(2.0),
/// );
/// assert_eq!(s.target(ParamKey::<Hz>::CUTOFF), Some(Hz(800.0)));
/// assert_eq!(s.start(ParamKey::<Hz>::CUTOFF), Some(Hz(200.0)));
/// assert_eq!(s.shape(), Law::PowerIn(2.0));
/// // Frame 239 is 240/480 of the way: a quadratic ease-in is a quarter up.
/// assert_eq!(s.raw_value_after(Samples(239)), 200.0 + 600.0 * 0.25);
/// assert_eq!(s.raw_value_after(Samples(479)), 800.0);
/// assert_eq!(s.arrives_after(), Some(Samples(479)));
/// ```
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ParamSegment {
    // The address, packed (a `ParamAddr` takes 8 bytes and would push an
    // `Event` past 32): a built-in param's packed `UnitAddr` (index in the
    // high byte of its `u16`, the `UnitParam` discriminant in the low), or the
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
        shape: Law,
    ) -> Self {
        Self::unit(key.addr(), from.to_raw(), to.to_raw(), duration, shape)
    }

    /// A segment of built-in parameter `param` (a [`UnitParam`](tutti_types::UnitParam),
    /// which is its index-0 address, or a [`UnitAddr`]) in its raw `f32` domain, as
    /// [`new`](Self::new) otherwise: for a producer that is erased over the
    /// unit (a control lane built from a `ParamAddr`, doc 014 §4.1), and for
    /// a param with no [`ParamKey`] yet. The raw value is exactly what
    /// [`new`](Self::new) sends for a key with this id: a unit's raw value is
    /// its bits.
    pub fn unit(
        param: impl Into<UnitAddr>,
        from: f32,
        to: f32,
        duration: Samples,
        shape: Law,
    ) -> Self {
        Self::packed(
            u32::from(param.into().to_u16()),
            false,
            from,
            to,
            duration,
            shape,
        )
    }

    /// A segment of a foreign unit's parameter `id`, in its own
    /// normalisation, as [`new`](Self::new) otherwise.
    pub fn foreign(id: u32, from: f32, to: f32, duration: Samples, shape: Law) -> Self {
        Self::packed(id, true, from, to, duration, shape)
    }

    fn packed(id: u32, foreign: bool, from: f32, to: f32, duration: Samples, shape: Law) -> Self {
        let (tag, shape_param) = ShapeTag::pack(shape);
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
    #[must_use]
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
        // Built from a `UnitAddr` by `new`, so it converts back.
        u16::try_from(self.id)
            .ok()
            .and_then(|id| UnitAddr::try_from(id).ok())
            .map_or(ParamAddr::Id(self.id), ParamAddr::unit)
    }

    /// Whether this segment is for `addr`. No conversion: what the param
    /// port asks of every event, per event.
    #[inline]
    pub(crate) fn is_for(&self, addr: ParamAddr) -> bool {
        match addr.as_unit() {
            Some(a) => !self.foreign && self.id == u32::from(a.to_u16()),
            None => matches!(addr, ParamAddr::Id(id) if self.foreign && self.id == id),
        }
    }

    /// The end value, in `key`'s unit, if this segment is for `key`.
    pub fn target<U: Unit<Raw = f32>>(&self, key: ParamKey<U>) -> Option<U> {
        self.is_for(ParamAddr::unit(key.addr()))
            .then(|| U::from_raw(self.to))
    }

    /// The start value, in `key`'s unit, if this segment is for `key`.
    pub fn start<U: Unit<Raw = f32>>(&self, key: ParamKey<U>) -> Option<U> {
        self.is_for(ParamAddr::unit(key.addr()))
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
    pub fn shape(&self) -> Law {
        self.shape.unpack(self.shape_param)
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
    shape: Law,
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

/// A MIDI packet at its position in the current block: what a MIDI edge
/// (`MidiIn`, `MidiOut`, a plugin's input) carries.
///
/// An alias, not a type: both halves are already checked ([`Offset`] against
/// a block, [`Ump`] as the packet), so the pair adds no rule of its own, and a
/// site destructures it (`for &(at, ump) in events`). Inside the graph the same
/// pair is an [`Event`] with [`EventKind::Midi`]. A position not yet checked
/// against a block (a plugin's report, a ring crossing threads) is a raw `u32`,
/// not an `Offset`, and so not an `UmpAt`.
pub type UmpAt = (Offset, Ump);

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
    #[must_use]
    pub fn is_note_off(&self) -> bool {
        matches!(self.kind, EventKind::Midi(u) if u.is_note_off())
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
#[non_exhaustive]
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
    #[must_use]
    pub fn as_slice(&self) -> &'a [Event] {
        self.events
    }

    /// Number of events.
    #[must_use]
    pub fn len(&self) -> usize {
        self.events.len()
    }

    /// Whether there are none.
    #[must_use]
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
    pub(crate) fn sub_blocks(self, frames: Samples) -> SubBlocks<'a> {
        SubBlocks {
            events: self.events,
            pos: 0,
            frames: frames.0,
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
#[non_exhaustive]
pub enum EventRejected {
    /// The port's preallocated buffer is full: at its declared capacity for
    /// any event, or at [`note_off_room`] past it for a note-off. The event is
    /// dropped; the executor counts it (see
    /// [`Executor::dropped_event_count`](crate::Executor::dropped_event_count)).
    Full,
    /// `offset` is at or past the end of the block.
    OutOfBlock,
    /// `offset` is before the previous event's. A port's events are delivered
    /// sorted, and sorting on the audio thread would mean either an allocating
    /// stable sort or an unstable one that reorders equal offsets — so the
    /// writer requires order instead of restoring it.
    OutOfOrder,
}

/// How many events a port of declared capacity `cap` can hold when every
/// one past `cap` is a note-off: `cap` plus a reserve of an eighth and 8.
///
/// What every buffer downstream of the port is priced at (the compiler's
/// [`EventSlotCapacity`](crate::EventSlotCapacity), a delay FIFO's rate), so a
/// note-off the port took always fits the slot, the merge and the FIFO behind
/// it.
#[must_use]
pub const fn note_off_room(cap: usize) -> usize {
    cap.saturating_add(cap / 8 + 8)
}

/// The fewest events per block [`EventDensity::capacity`] sizes a port for: a
/// burst at one instant (a chord's note-ons, a controller sweep) is not
/// spread over the block, so a short block still needs room for it.
pub const MIN_EVENT_PORT_CAPACITY: u32 = 256;

/// The most events per block [`EventDensity::capacity`] sizes a port for: one
/// per frame of the engine's default largest block
/// (`tutti_engine::DEFAULT_BLOCK_CAPACITY`, 8192). A longer block does not get a
/// larger port: a port is 32 bytes an event, 295 KB at the cap with its
/// note-off room, and a PDC delay on it holds up to `3.75 ×` that again
/// (about 2.5 MB: see `EventSlotCapacity::fifo`). Past the cap a port still
/// refuses and counts, it does not grow.
pub const MAX_EVENT_PORT_CAPACITY: u32 = 8_192;

/// How many events a source writes per frame at the most, for sizing its
/// event port: a port of a block of `n` frames then holds `n × density`,
/// between [`MIN_EVENT_PORT_CAPACITY`] and [`MAX_EVENT_PORT_CAPACITY`].
///
/// A port's capacity is a count per block, so a source whose rate is per
/// second (a clip, a controller) loses events once the block is longer than
/// the capacity was sized for. This is the other half of the declaration:
/// the node reads its [`Prepare`](crate::Prepare) in
/// [`Node::prepare`](crate::Node::prepare), asks for a capacity, and returns
/// it from `shape`, which the editor reads after every prepare and every
/// re-prepare. The allocation is the compiler's and the executor's, on the
/// control side.
///
/// ```
/// use tutti_graph::{EventDensity, Prepare};
/// use tutti_types::{SampleRate, Samples};
///
/// let one_per_frame = EventDensity::per_frame(1);
/// let at = |block| one_per_frame.capacity(&Prepare::new(SampleRate(48_000.0), Samples(block)));
/// assert_eq!(at(64), 256); // the floor
/// assert_eq!(at(1024), 1024);
/// assert_eq!(at(1 << 20), 8_192); // the cap
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EventDensity {
    per_kiloframe: u32,
}

impl EventDensity {
    /// `n` events in every frame.
    #[must_use]
    pub const fn per_frame(n: u32) -> Self {
        Self {
            per_kiloframe: n.saturating_mul(1024),
        }
    }

    /// `n` events in every 1024 frames: a density under one per frame.
    #[must_use]
    pub const fn per_kiloframe(n: u32) -> Self {
        Self { per_kiloframe: n }
    }

    /// The declared capacity for a port of a block of `prepare`'s
    /// [`max_block`](crate::Prepare::max_block).
    #[must_use]
    pub fn capacity(self, prepare: &crate::Prepare) -> u32 {
        let frames = prepare.max_block().get() as u64;
        let events = (frames * u64::from(self.per_kiloframe)).div_ceil(1024);
        events
            .clamp(
                u64::from(MIN_EVENT_PORT_CAPACITY),
                u64::from(MAX_EVENT_PORT_CAPACITY),
            )
            .try_into()
            .unwrap_or(MAX_EVENT_PORT_CAPACITY)
    }
}

/// What one writer counted this block: events refused or withheld, and
/// refused events a node later wrote (`EventWriter::recovered`).
#[derive(Debug, Default)]
pub(crate) struct PortCount {
    pub(crate) refused: Cell<u32>,
    pub(crate) recovered: Cell<u32>,
}

/// The preallocated sink a node writes one event output port through.
///
/// Never allocates: it refuses past its capacity rather than growing, and the
/// capacity was reserved on the control thread.
///
/// # Note-offs have priority
///
/// A port at its declared capacity refuses any new event but a note-off
/// (`Event::is_note_off`), which is taken until the port holds
/// [`note_off_room`] events. The reserve costs a node that keeps to its
/// declaration nothing (it is room beyond what the node declared); it
/// is what lets a node that refused a note-on still end the notes it did
/// start. Order is not touched: nothing already written is moved or evicted.
///
/// The writer does not pair: a note-on it refused leaves its note-off to
/// whoever wrote it. A node that tracks the notes it starts (the clip node)
/// keeps the pairing itself and reports the note-off it then withholds with
/// [`withhold`](Self::withhold); a relay that does not track (hardware input,
/// the queue) passes the note-off on, which a receiver treats as a no-op for
/// a key that is not sounding.
pub struct EventWriter<'a> {
    // `None` only for a detached writer, which the executor uses to fill the
    // unused tail of its fixed-size port table and never hands to a node.
    buf: Option<&'a mut Vec<Event>>,
    cap: usize,
    // `note_off_room(cap)`, computed once.
    room: usize,
    frames: u32,
    // Counted into a cell rather than a field: the executor reads it after
    // the node returns, while the writer itself is still borrowed by the `Io`
    // the node consumed. One per port, so the loss is reported per port.
    dropped: Option<&'a PortCount>,
}

impl std::fmt::Debug for EventWriter<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventWriter")
            .field("cap", &self.cap)
            .field("frames", &self.frames)
            .field("len", &self.buf.as_ref().map(|b| b.len()))
            .finish_non_exhaustive()
    }
}

impl<'a> EventWriter<'a> {
    /// A writer over `buf`, which the caller has cleared. At most `cap`
    /// events are accepted (a note-off, [`note_off_room`] of them), and only
    /// at offsets `< frames`.
    pub(crate) fn new(
        buf: &'a mut Vec<Event>,
        cap: usize,
        frames: u32,
        dropped: &'a PortCount,
    ) -> Self {
        debug_assert!(buf.is_empty());
        Self {
            buf: Some(buf),
            cap,
            room: note_off_room(cap),
            frames,
            dropped: Some(dropped),
        }
    }

    /// A writer that accepts nothing: a placeholder for an unused port slot.
    pub(crate) const fn detached() -> Self {
        Self {
            buf: None,
            cap: 0,
            room: 0,
            frames: 0,
            dropped: None,
        }
    }

    fn reject(&self, why: EventRejected) -> Result<(), EventRejected> {
        self.count_lost();
        Err(why)
    }

    fn count_lost(&self) {
        if let Some(d) = self.dropped {
            d.refused.set(d.refused.get() + 1);
        }
    }

    /// Appends `event`. Offsets must be non-decreasing and inside the block.
    /// Audio thread: never allocates.
    ///
    /// # Errors
    ///
    /// [`EventRejected::OutOfBlock`], [`EventRejected::OutOfOrder`], or
    /// [`EventRejected::Full`] when the port's declared capacity is reached
    /// (a note-off is refused only at [`note_off_room`]).
    /// A refused event is dropped and counted in
    /// [`Executor::dropped_event_count`](crate::Executor::dropped_event_count); the
    /// events already written stand.
    pub fn push(&mut self, event: Event) -> Result<(), EventRejected> {
        if event.offset.get() >= self.frames {
            return self.reject(EventRejected::OutOfBlock);
        }
        let len = self.len();
        // The only test on the way in when the port is not full.
        if len >= self.cap && (len >= self.room || !event.is_note_off()) {
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

    /// Counts one event the node withheld rather than wrote: it was not
    /// refused by the port, but the node left it out because the event it
    /// belongs to was (a note-off whose note-on the port refused). Without
    /// it the port's count is short by the notes a refusal then unpaired.
    pub fn withhold(&self) {
        self.count_lost();
    }

    /// Says that an event the port refused earlier (counted when it was) has
    /// now been written: a note-off a node kept owed and sent in a later
    /// block. The refusal stays in the dropped count; the executor's lost
    /// count (`Executor::lost_event_count`) takes it back.
    pub fn recovered(&self) {
        if let Some(d) = self.dropped {
            d.recovered.set(d.recovered.get() + 1);
        }
    }

    /// How many events this port has accepted so far this block.
    #[must_use]
    pub fn len(&self) -> usize {
        self.buf.as_ref().map_or(0, |b| b.len())
    }

    /// Whether nothing has been written yet this block.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Merge `sources` (each sorted by offset) into `out`, ordered by
/// `(offset, source index)`. At most `cap` events land; the rest are counted.
///
/// A k-way merge rather than concatenate-and-sort: the stable sort that would
/// give the same order allocates, and the unstable one does not give it.
///
/// When the sources hold more than `cap`, the note-offs keep their room: the
/// others are cut (the later ones first) so that every note-off lands, up to
/// `cap`. Order is that of the merge either way. When they fit, which is the
/// case by construction (a merge's slot holds all its sources), nothing but
/// the sum of the sources' lengths is looked at.
pub(crate) fn merge_into(sources: &[&[Event]], out: &mut Vec<Event>, cap: usize) -> u32 {
    let total: usize = sources.iter().map(|s| s.len()).sum();
    let room = cap.saturating_sub(out.len());
    // How many events that are not note-offs may land: all of them while all
    // fit, else what the note-offs leave of the room.
    let mut others = usize::MAX;
    if total > room {
        let offs: usize = sources
            .iter()
            .map(|s| s.iter().filter(|e| e.is_note_off()).count())
            .sum();
        others = room.saturating_sub(offs);
    }
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
        let fits = out.len() < cap;
        if e.is_note_off() {
            if fits {
                out.push(e);
            } else {
                dropped += 1;
            }
        } else if fits && others > 0 {
            others = others.saturating_sub(1);
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
        let dropped = PortCount::default();
        let mut w = EventWriter::new(&mut buf, 2, 16, &dropped);
        assert_eq!(w.push(ev(5, 0)), Ok(()));
        assert_eq!(w.push(ev(4, 0)), Err(EventRejected::OutOfOrder));
        assert_eq!(w.push(ev(16, 0)), Err(EventRejected::OutOfBlock));
        assert_eq!(w.push(ev(5, 0)), Ok(()));
        assert_eq!(w.push(ev(6, 0)), Err(EventRejected::Full));
        assert_eq!(w.len(), 2);
        assert_eq!(dropped.refused.get(), 3);
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
            .sub_blocks(Samples(frames))
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
            ParamKey::<Hz>::CUTOFF.addr().to_u16() as u32,
            0.5,
            Samples(0),
        );
        assert_eq!(foreign.target(ParamKey::<Hz>::CUTOFF), None);
        assert_eq!(foreign.foreign_target(0), Some(0.5));
        let builtin = ParamRamp::new(ParamKey::<Hz>::CUTOFF, Hz(100.0), Samples(0));
        assert_eq!(builtin.foreign_target(0), None);
    }

    /// An event is a note-off when its packet is (`Ump::is_note_off` pins the
    /// spellings); a ramp is not one.
    ///
    /// Mutation: make `is_note_off` ignore the kind and return true -> the ramp
    /// assertion fails.
    #[test]
    fn only_a_midi_note_off_event_is_a_note_off() {
        assert!(Event::midi(Offset::ZERO, [0x2080_3c40, 0, 0, 0]).is_note_off());
        assert!(!Event::midi(Offset::ZERO, [0x2090_3c40, 0, 0, 0]).is_note_off());
        assert!(!Event::ramp(Offset::ZERO, ParamRamp::foreign(1, 0.0, Samples(0))).is_note_off());
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
        let id = u32::from(key.addr().to_u16());
        let foreign = ParamSegment::foreign(id, 0.25, 0.75, Samples(8), Law::Exp(2.0));
        assert_eq!(foreign.target(key), None);
        assert_eq!(foreign.foreign_target(id), Some(0.75));
        assert_eq!(foreign.foreign_start(id), Some(0.25));
        assert_eq!(foreign.addr(), ParamAddr::Id(id));
        let s = ParamSegment::new(key, Hz(1.0), Hz(2.0), Samples(8), Law::PowerOut(3.0))
            .resumed(SamplePosition(2.5), SamplePosition(10.0));
        assert_eq!(s.addr(), ParamAddr::unit(key.addr()));
        assert_eq!(s.foreign_target(id), None);
        assert_eq!(s.start(key), Some(Hz(1.0)));
        assert_eq!(s.shape(), Law::PowerOut(3.0));
        assert_eq!(s.position(), SamplePosition(2.5));
        assert_eq!(s.length(), SamplePosition(10.0));
        // 2.5 frames along at its event, 7.5 more to go: the end value
        // lands on frame 8, held after.
        assert!(s.raw_value_after(Samples(7)) < 2.0);
        assert_eq!(s.raw_value_after(Samples(8)), 2.0);
        assert_eq!(s.raw_value_after(Samples(1_000)), 2.0);
        assert_eq!(s.arrives_after(), Some(Samples(8)));
        // A zero length is a step.
        let step = ParamSegment::new(key, Hz(1.0), Hz(2.0), Samples(0), Law::Hold);
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
        let late = ParamSegment::new(key, Hz(1.0), Hz(2.0), Samples(0), Law::Linear)
            .resumed(SamplePosition(-3.0), SamplePosition(4.0));
        assert_eq!(late.raw_value_after(Samples(2)), 1.0);
        assert_eq!(late.raw_value_after(Samples(3)), 1.0);
        assert_eq!(late.raw_value_after(Samples(5)), 1.5);
        assert_eq!(late.arrives_after(), Some(Samples(7)));
    }

    fn off(offset: u32) -> Event {
        Event::midi(Offset::raw(offset), [0x2080_3c00, 0, 0, 0])
    }

    /// A merge that must cut keeps the note-offs and cuts the others, later
    /// ones first, and keeps the merge order. Sources `a = [on@0, on@1,
    /// off@2]` and `b = [on@0, off@3]` into room for 3: both note-offs land
    /// (2), leaving one other, which is the first in merge order (`a`'s
    /// note-on at 0). The three others cut are counted: 2.
    ///
    /// Mutation: treat every event alike in `merge_into` (drop the
    /// note-off arm) → the output is `[a0, b0, a1]` and loses both
    /// note-offs → fails.
    #[test]
    fn a_merge_that_must_cut_keeps_its_note_offs() {
        let a = [ev(0, 1), ev(1, 2), off(2)];
        let b = [ev(0, 3), off(3)];
        let mut out = Vec::with_capacity(3);
        let dropped = merge_into(&[&a, &b], &mut out, 3);
        assert_eq!(dropped, 2);
        assert_eq!(out, vec![ev(0, 1), off(2), off(3)]);
    }

    /// A merge that fits is the plain merge, note-offs or not.
    #[test]
    fn a_merge_that_fits_cuts_nothing() {
        let a = [ev(0, 1), off(2)];
        let b = [off(1), ev(3, 3)];
        let mut out = Vec::with_capacity(4);
        assert_eq!(merge_into(&[&a, &b], &mut out, 4), 0);
        assert_eq!(out, vec![ev(0, 1), off(1), off(2), ev(3, 3)]);
    }

    /// A writer at its capacity refuses a note-on and takes a note-off, up
    /// to `note_off_room`, and every refusal and every withheld event is
    /// counted once.
    ///
    /// Mutation: `room: note_off_room(cap)` → `room: cap` in `new` → the
    /// note-off is refused → fails. Mutation: `withhold` without
    /// `count_lost` → the count is one short → fails.
    #[test]
    fn a_writer_at_capacity_takes_note_offs_to_its_room() {
        let mut buf = Vec::with_capacity(16);
        let dropped = PortCount::default();
        let mut w = EventWriter::new(&mut buf, 2, 16, &dropped);
        w.push(ev(0, 1)).unwrap();
        w.push(ev(1, 2)).unwrap();
        assert_eq!(w.push(ev(2, 3)), Err(EventRejected::Full));
        // Room for 2 + 0 + 8: eight note-offs.
        for k in 0..8 {
            assert_eq!(w.push(off(3 + k)), Ok(()), "note-off {k}");
        }
        assert_eq!(w.push(off(12)), Err(EventRejected::Full));
        w.withhold();
        w.recovered();
        assert_eq!((dropped.refused.get(), dropped.recovered.get()), (3, 1));
        assert_eq!(w.len(), 10);
    }

    /// The capacity a density asks for: the floor on a short block, one per
    /// frame on a long one, the cap beyond that, rounded up.
    ///
    /// Mutation: drop the `clamp` → the first and last figures are wrong →
    /// fails.
    #[test]
    fn a_density_sizes_a_port_by_the_block() {
        let at = |density: EventDensity, block| {
            density.capacity(&crate::Prepare::new(
                tutti_types::SampleRate(48_000.0),
                Samples(block),
            ))
        };
        let frame = EventDensity::per_frame(1);
        assert_eq!(at(frame, 1), MIN_EVENT_PORT_CAPACITY);
        assert_eq!(at(frame, 512), 512);
        assert_eq!(at(frame, 8192), 8192);
        assert_eq!(at(frame, 1 << 20), MAX_EVENT_PORT_CAPACITY);
        // A quarter per frame, rounded up: 1000 frames hold 250.
        let quarter = EventDensity::per_kiloframe(256);
        assert_eq!(at(quarter, 4096), 1024);
        assert_eq!(at(quarter, 1001), MIN_EVENT_PORT_CAPACITY);
        assert_eq!(
            at(EventDensity::per_kiloframe(1), 4097),
            MIN_EVENT_PORT_CAPACITY
        );
    }
}
