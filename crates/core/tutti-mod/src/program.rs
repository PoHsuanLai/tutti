//! [`ControlProgram`]: what one target's control lane evaluates (design doc
//! 014 §4).
//!
//! A program is a description: a base (an automation curve, else the
//! target's knob) plus offsets (time-function [`Layer`]s and signal-driven
//! [`InputLayer`]s), each shaped by an [`Amount`] and scaled by the target's
//! span. It holds no clock and no state: equal programs evaluate to equal
//! bits at equal times, which is what lets a live lane and an export's fork of
//! it agree. Phase is a function of the time a lane hands in
//! ([`ProgramTime`]), never of the program's age, so a depth edit is a
//! republish and never a phase reset.
//!
//! Ungated: the floor, with [`Curve`]. The lane that evaluates it is
//! `tutti-nodes`' `ControlLaneNode`; the compiler that builds it from routes
//! is doc 014's PR 3.

use std::fmt;
use std::sync::Arc;

use audio_automation::CurveType;
use tutti_types::{
    Beat, BeatDuration, Depth, Frame, Hz, Phase, PhaseIncrement, SamplePosition, SampleRate,
};

use crate::curve::{Curve, CurveSegment};
use crate::shape::{shape, LfoShape, Polarity};

/// Time-function layers one program holds, at most: a lane keeps per-layer
/// state in arrays this long, so it never allocates on the audio thread.
/// [`ControlProgram::new`] refuses past it ([`ProgramError::TooManyLayers`]).
pub const MAX_LANE_LAYERS: usize = 16;

/// Event-input layers one program holds, at most; see [`MAX_LANE_LAYERS`].
/// [`ControlProgram::new`] refuses past it ([`ProgramError::TooManyInputs`]).
pub const MAX_LANE_INPUTS: usize = 8;

/// A layer's identity: what its sum order and its identity across a
/// republish are keyed by. A compiler derives it stably from what the layer
/// comes from (a route), so a republish keeps it.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct LayerId(pub u64);

/// A signal source's identity on a lane's event input: its events are
/// addressed `ParamAddr::Id(source.0)` and carry its raw value.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct SourceId(pub u32);

/// The clock a layer's phase runs on.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Clock {
    /// `cycles = beat / per_cycle + phase`: bar-locked; follows seeks and
    /// loop wraps; holds while the transport is stopped. A `per_cycle` that
    /// is not above zero leaves the layer silent.
    Beats {
        /// Beats per cycle: a span, so larger is slower.
        per_cycle: BeatDuration,
        /// Added to the cycle position.
        phase: PhaseIncrement,
    },
    /// `cycles = rate · play / sample rate + phase`, on the graph's play
    /// clock (frames since the last play start, running while stopped), or
    /// continued through `anchor` after a rate edit in the same play epoch.
    Seconds {
        /// Cycles per second. Negative runs the wave backwards.
        rate: Hz,
        /// Added to the cycle position (not to an anchored one: the anchor's
        /// phase already holds it).
        phase: PhaseIncrement,
        /// Where a rate edit takes effect, if one does.
        anchor: Option<RateAnchor>,
    },
}

/// Where a rate edit takes effect (doc 014 §4.3): in play epoch `epoch`, the
/// layer is at `phase` cycles on play frame `at`, and moves at `prev_rate`
/// before it and at the layer's rate from it. An anchor from another epoch
/// (a play start since, or a fork) is ignored.
///
/// `phase` is a bare `f64` of cycles, where the types stop: an hour of play
/// needs more precision than [`PhaseIncrement`]'s `f32`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RateAnchor {
    /// The play epoch the anchor is dated in.
    pub epoch: u32,
    /// The play frame it takes effect on.
    pub at: Frame,
    /// The layer's position there, in cycles (unwrapped).
    pub phase: f64,
    /// The rate before `at`.
    pub prev_rate: Hz,
}

/// One period of a layer's raw value, in `[-1, 1]`.
#[derive(Clone, Debug, PartialEq)]
pub enum Wave {
    /// A closed-form shape. `Random` and `RandomSmooth` derive their steps
    /// from a hash of the cycle index and `seed`, so they replay after a
    /// seek and in a fork (seed 0 is the routing feature's `BeatLfo`
    /// sequence).
    Shape {
        /// The shape.
        shape: LfoShape,
        /// The random shapes' seed; ignored by the others.
        seed: u64,
    },
    /// One drawn period: breakpoints over `[0, 1)`, ascending, each with the
    /// curve of the piece it starts; the last piece runs to the first point
    /// of the next period.
    Drawn(Arc<[PeriodPoint]>),
}

/// A breakpoint of a [`Wave::Drawn`] period.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PeriodPoint {
    /// Where in the period, in `[0, 1)`.
    pub at: f32,
    /// The raw value there, in `[-1, 1]`.
    pub value: f32,
    /// The curve of the piece from here to the next point.
    pub curve: CurveType,
}

/// How a raw value becomes an offset: `shape(raw, depth, polarity, curve) ×
/// span`, the span read from the target's live range by the lane each block
/// (so a range edit rescales without a republish).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Amount {
    /// A fraction of the target's range; negative inverts.
    pub depth: Depth,
    /// Whether the offset swings both ways or one.
    pub polarity: Polarity,
    /// The response curve on the magnitude.
    pub curve: CurveType,
}

impl Amount {
    /// The offset `raw` gives, in the target's units.
    #[inline]
    pub fn offset(&self, raw: f32, span: f64) -> f64 {
        f64::from(shape(raw, self.depth, self.polarity, self.curve)) * span
    }

    /// Whether the offset's slope breaks where the raw value crosses zero:
    /// a bipolar amount whose curve `curve_apply` bends (`shape` curves
    /// `|x|`), so each zero crossing is a corner (doc 014 §5.3).
    #[inline]
    pub fn kinks_at_zero(&self) -> bool {
        self.polarity == Polarity::Bipolar
            && matches!(
                self.curve,
                CurveType::Exponential | CurveType::Logarithmic | CurveType::SCurve
            )
    }
}

/// A time-function layer: a wave on a clock, shaped into an offset.
#[derive(Clone, Debug, PartialEq)]
pub struct Layer {
    /// Its identity and sum order.
    pub id: LayerId,
    /// The clock its phase runs on.
    pub clock: Clock,
    /// Its raw value over one cycle.
    pub wave: Wave,
    /// How the raw value becomes an offset.
    pub amount: Amount,
    /// The offset scaled by a second wave mapped to `[0, 1]` (depth moved
    /// by an LFO). `None`: unscaled.
    pub depth_by: Option<(Clock, Wave)>,
}

/// A signal source's raw value (its events on the lane's event input,
/// addressed `ParamAddr::Id(source.0)`), shaped into an offset.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct InputLayer {
    /// Its identity and sum order (after every [`Layer`]).
    pub id: LayerId,
    /// The source whose events it reads.
    pub source: SourceId,
    /// How the source's raw value becomes an offset.
    pub amount: Amount,
}

/// The time a lane evaluates a program at.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ProgramTime {
    /// The transport's beat (held while stopped).
    pub beat: Beat,
    /// The play clock, in frames since the last play start: fractional
    /// when the time is a corner between frames. A frame count rather than
    /// `Seconds`, which is `f32`: an hour of play needs `f64` phase.
    pub play: SamplePosition,
    /// The play clock's epoch (a count of play starts).
    pub play_epoch: u32,
    /// The rate the play clock counts at.
    pub rate: SampleRate,
}

/// The next corner on each clock, strictly after a time
/// ([`ControlProgram::next_corner`]): where the sum's slope breaks or it
/// jumps.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Corners {
    /// The next corner of the beat-clocked layers and the automation. It
    /// arrives only while the beat moves.
    pub beat: Option<Beat>,
    /// The next corner of the seconds-clocked layers, on the play clock.
    pub play: Option<SamplePosition>,
}

/// Why [`ControlProgram::new`] refused a program.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProgramError {
    /// More than [`MAX_LANE_LAYERS`] layers.
    TooManyLayers {
        /// How many there were.
        count: usize,
    },
    /// More than [`MAX_LANE_INPUTS`] event-input layers.
    TooManyInputs {
        /// How many there were.
        count: usize,
    },
    /// Two layers (or two input layers) with one id.
    DuplicateLayer(LayerId),
    /// A drawn wave whose points are not ascending in `[0, 1)` with finite
    /// values, in this layer.
    BadDrawnWave(LayerId),
}

impl fmt::Display for ProgramError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooManyLayers { count } => {
                write!(f, "{count} layers, past MAX_LANE_LAYERS ({MAX_LANE_LAYERS})")
            }
            Self::TooManyInputs { count } => {
                write!(
                    f,
                    "{count} input layers, past MAX_LANE_INPUTS ({MAX_LANE_INPUTS})"
                )
            }
            Self::DuplicateLayer(id) => write!(f, "two layers share id {}", id.0),
            Self::BadDrawnWave(id) => write!(
                f,
                "layer {}: a drawn wave's points must ascend in [0, 1) with finite values",
                id.0
            ),
        }
    }
}

impl std::error::Error for ProgramError {}

/// What one target's control lane evaluates: a base, plus offsets.
///
/// ```text
/// value(t) = (automation(t) where it has a value, else knob) + Σ offsets(t)
/// ```
///
/// The clamp to the target's range is the target's (a native param port
/// clamps per frame) or the plugin lane's, never the program's. Values are
/// the target's raw `f32` (Hz, linear gain, a plugin's normalised 0..1): the
/// program is erased over the unit, as the lane's `ParamSegment`s are.
///
/// Built by [`new`](Self::new), which sorts the layers by id and refuses a
/// program past the lane's fixed per-layer state; the fields are private so
/// every program a lane reads holds those invariants.
///
/// ```
/// use tutti_mod::{
///     Amount, Clock, ControlProgram, CurveType, Layer, LayerId, LfoShape, Polarity,
///     ProgramTime, Wave,
/// };
/// use tutti_types::{Beat, BeatDuration, Depth, PhaseIncrement, SamplePosition, SampleRate};
///
/// // A bar-locked square, a quarter of the range deep, on a knob of 1000.
/// let square = Layer {
///     id: LayerId(1),
///     clock: Clock::Beats { per_cycle: BeatDuration(4.0), phase: PhaseIncrement(0.0) },
///     wave: Wave::Shape { shape: LfoShape::Square, seed: 0 },
///     amount: Amount { depth: Depth(0.25), polarity: Polarity::Bipolar, curve: CurveType::Linear },
///     depth_by: None,
/// };
/// let program = ControlProgram::new(None, vec![square], vec![]).unwrap();
/// let at = |beat| ProgramTime {
///     beat: Beat(beat),
///     play: SamplePosition(0.0),
///     play_epoch: 1,
///     rate: SampleRate(48_000.0),
/// };
/// // Range 0..800: a span of 800, so the offset is ±200.
/// assert_eq!(program.value_at(Some(1000.0), &at(1.0), 800.0, &[]), Some(1200.0));
/// assert_eq!(program.value_at(Some(1000.0), &at(3.0), 800.0, &[]), Some(800.0));
/// ```
#[derive(Clone, Default)]
pub struct ControlProgram {
    automation: Option<Arc<dyn Curve>>,
    layers: Box<[Layer]>,
    inputs: Box<[InputLayer]>,
}

impl fmt::Debug for ControlProgram {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ControlProgram")
            .field("automation", &self.automation.as_ref().map(|_| "<curve>"))
            .field("layers", &self.layers)
            .field("inputs", &self.inputs)
            .finish()
    }
}

/// Equal when the layers are equal and the automation is the same curve
/// (`Arc` identity: a curve has no equality of its own).
impl PartialEq for ControlProgram {
    fn eq(&self, other: &Self) -> bool {
        let same_curve = match (&self.automation, &other.automation) {
            (None, None) => true,
            (Some(a), Some(b)) => std::ptr::addr_eq(Arc::as_ptr(a), Arc::as_ptr(b)),
            _ => false,
        };
        same_curve && self.layers == other.layers && self.inputs == other.inputs
    }
}

/// A tolerance in a clock's units (cycles, beats) at `x`: what "strictly
/// after" a corner and "just before" one mean, wide enough that a corner
/// computed from a time and that time recomputed from the corner land on
/// the same side, and far below a frame.
#[inline]
fn tol(x: f64) -> f64 {
    (x.abs() * 1e-12).max(1e-9)
}

impl ControlProgram {
    /// A program of `automation` (taken as its [`frozen`](Curve::frozen)
    /// copy when it has one, so the lane never reads a cell through it),
    /// `layers` and `inputs`, each sorted by id.
    ///
    /// # Errors
    ///
    /// Past [`MAX_LANE_LAYERS`] or [`MAX_LANE_INPUTS`], two layers with one
    /// id, or a drawn wave whose points do not ascend in `[0, 1)` with
    /// finite values.
    pub fn new(
        automation: Option<Arc<dyn Curve>>,
        mut layers: Vec<Layer>,
        mut inputs: Vec<InputLayer>,
    ) -> Result<Self, ProgramError> {
        if layers.len() > MAX_LANE_LAYERS {
            return Err(ProgramError::TooManyLayers {
                count: layers.len(),
            });
        }
        if inputs.len() > MAX_LANE_INPUTS {
            return Err(ProgramError::TooManyInputs {
                count: inputs.len(),
            });
        }
        layers.sort_by_key(|l| l.id);
        inputs.sort_by_key(|l| l.id);
        if let Some(w) = layers.windows(2).find(|w| w[0].id == w[1].id) {
            return Err(ProgramError::DuplicateLayer(w[0].id));
        }
        if let Some(w) = inputs.windows(2).find(|w| w[0].id == w[1].id) {
            return Err(ProgramError::DuplicateLayer(w[0].id));
        }
        for l in &layers {
            let waves = std::iter::once(&l.wave).chain(l.depth_by.as_ref().map(|(_, w)| w));
            for w in waves {
                if let Wave::Drawn(points) = w {
                    let in_range = points
                        .iter()
                        .all(|p| (0.0..1.0).contains(&p.at) && p.value.is_finite());
                    let ascending = points.windows(2).all(|w| w[0].at < w[1].at);
                    if !(in_range && ascending) {
                        return Err(ProgramError::BadDrawnWave(l.id));
                    }
                }
            }
        }
        Ok(Self {
            automation: automation.map(|c| c.frozen().unwrap_or(c)),
            layers: layers.into_boxed_slice(),
            inputs: inputs.into_boxed_slice(),
        })
    }

    /// A program of automation alone: the curve where it has a value, the
    /// knob elsewhere.
    pub fn from_automation(curve: Arc<dyn Curve>) -> Self {
        Self {
            automation: Some(curve.frozen().unwrap_or(curve)),
            layers: Box::new([]),
            inputs: Box::new([]),
        }
    }

    /// The automation curve, if any.
    pub fn automation(&self) -> Option<&Arc<dyn Curve>> {
        self.automation.as_ref()
    }

    /// The time-function layers, in id order.
    pub fn layers(&self) -> &[Layer] {
        &self.layers
    }

    /// The event-input layers, in id order.
    pub fn inputs(&self) -> &[InputLayer] {
        &self.inputs
    }

    /// Exactly one curve's segments: automation, no layers, no inputs. A
    /// native target's lane sends such a program's shaped pieces as they
    /// are (exact mode, doc 014 §5.4).
    pub fn is_pure_automation(&self) -> bool {
        self.automation.is_some() && self.layers.is_empty() && self.inputs.is_empty()
    }

    /// The value at `at`, before any clamp: the automation's value where it
    /// has one (and it is finite), else `knob`, plus every layer's offset
    /// and every input's (`inputs[k]` is the raw value of
    /// [`inputs()`](Self::inputs)`[k]`'s source, `None` while it has not
    /// sent, which adds nothing).
    ///
    /// `span` is the target's `max − min`, which every offset scales by. A
    /// span that is not finite leaves the base alone (an unranged target
    /// takes no modulation). `None` only with no automation value and no
    /// knob.
    ///
    /// Pure and allocation-free. Offsets are summed in `f64` from `-0.0`, in
    /// id order (layers, then inputs), and rounded to `f32` once, so a base
    /// with no offsets passes through to the bit.
    pub fn value_at(
        &self,
        knob: Option<f32>,
        at: &ProgramTime,
        span: f64,
        inputs: &[Option<f32>],
    ) -> Option<f32> {
        self.evaluate(knob, at, span, inputs, false)
    }

    /// The left limit at `at`: [`value_at`](Self::value_at) just before it,
    /// on every clock. It differs from `value_at` only where something
    /// jumps exactly at `at` (a square's edge, a random step, a breakpoint
    /// pair): what a piece ending at a corner aims at.
    pub fn value_before(
        &self,
        knob: Option<f32>,
        at: &ProgramTime,
        span: f64,
        inputs: &[Option<f32>],
    ) -> Option<f32> {
        self.evaluate(knob, at, span, inputs, true)
    }

    fn evaluate(
        &self,
        knob: Option<f32>,
        at: &ProgramTime,
        span: f64,
        inputs: &[Option<f32>],
        left: bool,
    ) -> Option<f32> {
        let base = self
            .automation
            .as_ref()
            .and_then(|c| {
                let b = at.beat.get();
                c.value_at(Beat(if left { b - tol(b) } else { b }))
            })
            .filter(|v| v.is_finite())
            .or(knob)?;
        if !span.is_finite() {
            return Some(base);
        }
        let mut acc = -0.0f64;
        for l in self.layers.iter() {
            if let Some(o) = l.offset(at, span, left) {
                acc += o;
            }
        }
        for (k, l) in self.inputs.iter().enumerate() {
            if let Some(x) = inputs.get(k).copied().flatten() {
                acc += l.amount.offset(x, span);
            }
        }
        Some((f64::from(base) + acc) as f32)
    }

    /// The next corner strictly after `at` on each clock: the earliest of
    /// the automation's next breakpoint (where [`Curve::segment_at`] can
    /// say it), each layer's slope breaks and jumps (a square's edges, a
    /// saw's wrap, a triangle's peaks, a random step, a drawn point), each
    /// curved bipolar layer's zero crossings (doc 014 §5.3), each
    /// `depth_by` wave's, and a rate anchor's frame. Inputs' corners are the
    /// lane's: it holds their segments.
    pub fn next_corner(&self, at: &ProgramTime) -> Corners {
        let mut out = Corners::default();
        let mut beat = |b: f64| {
            if b.is_finite() && out.beat.is_none_or(|o| b < o.get()) {
                out.beat = Some(Beat(b));
            }
        };
        if let Some(c) = &self.automation {
            let b = at.beat.get();
            let next = match c.segment_at(at.beat) {
                Some(CurveSegment::Move { end, .. }) => Some(end.get()),
                Some(CurveSegment::Hold { until, .. }) => until.map(Beat::get),
                None => None,
            };
            if let Some(n) = next.filter(|&n| n > b + tol(b)) {
                beat(n);
            }
        }
        let mut corners = [Corners::default(); 2];
        for l in self.layers.iter() {
            corners[0] = l.clock.next_corner(at, &l.wave, l.amount.kinks_at_zero());
            corners[1] = l
                .depth_by
                .as_ref()
                .map_or(Corners::default(), |(c, w)| c.next_corner(at, w, false));
            for c in corners {
                if let Some(b) = c.beat {
                    beat(b.get());
                }
                if let Some(p) = c.play {
                    if p.get().is_finite() && out.play.is_none_or(|o| p.get() < o.get()) {
                        out.play = Some(p);
                    }
                }
            }
        }
        out
    }
}

impl Layer {
    /// This layer's offset at `at` (just before it when `left`), `None`
    /// when its clock gives no position.
    fn offset(&self, at: &ProgramTime, span: f64, left: bool) -> Option<f64> {
        let raw = self.wave.raw_at(self.clock.cycles(at)?, left);
        let mut off = self.amount.offset(raw, span);
        if let Some((clock, wave)) = &self.depth_by {
            let m = clock
                .cycles(at)
                .map_or(1.0, |c| (f64::from(wave.raw_at(c, left)) + 1.0) * 0.5);
            off *= m;
        }
        Some(off)
    }
}

impl Clock {
    /// The position at `at`, in unwrapped cycles; `None` when the clock
    /// gives none (a beat span not above zero, a rate that is not finite).
    pub fn cycles(&self, at: &ProgramTime) -> Option<f64> {
        let c = match *self {
            Self::Beats { per_cycle, phase } => {
                at.beat.cycles_of(per_cycle)? + f64::from(phase.get())
            }
            Self::Seconds {
                rate,
                phase,
                anchor,
            } => {
                let sr = at.rate.get();
                if !(sr > 0.0 && sr.is_finite()) {
                    return None;
                }
                let play = at.play.get();
                match anchor.filter(|a| a.epoch == at.play_epoch) {
                    Some(a) => {
                        let d = play - a.at.get() as f64;
                        let r = if d < 0.0 { a.prev_rate } else { rate };
                        a.phase + f64::from(r.get()) * d / sr
                    }
                    None => f64::from(rate.get()) * play / sr + f64::from(phase.get()),
                }
            }
        };
        c.is_finite().then_some(c)
    }

    /// The next corner of `wave` on this clock strictly after `at` (with
    /// zero crossings when `kinks`), and an anchor's own frame, where the
    /// rate changes.
    fn next_corner(&self, at: &ProgramTime, wave: &Wave, kinks: bool) -> Corners {
        let Some(c) = self.cycles(at) else {
            return Corners::default();
        };
        match *self {
            Self::Beats { per_cycle, phase } => Corners {
                beat: wave
                    .next_corner(c, true, kinks)
                    .map(|n| Beat((n - f64::from(phase.get())) * per_cycle.get())),
                play: None,
            },
            Self::Seconds {
                rate,
                phase,
                anchor,
            } => {
                let sr = at.rate.get();
                let play = at.play.get();
                // The play frame at which a rate `r` from (`p0`, `c0`)
                // reaches the wave's next corner.
                let along = |r: Hz, p0: f64, c0: f64| {
                    let r = f64::from(r.get());
                    if r == 0.0 || !r.is_finite() {
                        return None;
                    }
                    wave.next_corner(c, r > 0.0, kinks)
                        .map(|n| p0 + (n - c0) * sr / r)
                };
                let p = match anchor.filter(|a| a.epoch == at.play_epoch) {
                    Some(a) => {
                        let t = a.at.get() as f64;
                        if play < t - tol(t) {
                            // The anchor's frame is a corner of its own.
                            Some(along(a.prev_rate, t, a.phase).map_or(t, |p| p.min(t)))
                        } else {
                            along(rate, t, a.phase)
                        }
                    }
                    None => along(rate, 0.0, f64::from(phase.get())),
                };
                Corners {
                    beat: None,
                    play: p.map(SamplePosition),
                }
            }
        }
    }
}

/// `hash_bipolar` of cycle `n` under `seed`: seed 0 is the plain sequence.
#[inline]
fn hash_cycle(n: f64, seed: u64) -> f32 {
    hash_bipolar((n as i64).wrapping_add(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) as i64))
}

/// Map an integer index to a stable pseudo-random value in `[-1, 1]`
/// (splitmix64 finalizer).
///
/// Addressable rather than sequential: the value for index `n` is
/// computable without having produced `n-1`, which is what lets a random
/// wave be a pure function of its clock, replayed after a seek and in a
/// fork.
#[inline]
pub(crate) fn hash_bipolar(n: i64) -> f32 {
    let mut z = (n as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^= z >> 31;
    // Top 24 bits → [0, 1) → [-1, 1].
    ((z >> 40) as f32 / (1u32 << 24) as f32) * 2.0 - 1.0
}

impl Wave {
    /// The raw value at `cycles` (just before it when `left`).
    pub fn raw_at(&self, cycles: f64, left: bool) -> f32 {
        let c = if left { cycles - tol(cycles) } else { cycles };
        let n = c.floor();
        let frac = c - n;
        match self {
            Self::Shape { shape, seed } => match shape {
                LfoShape::Random => hash_cycle(n, *seed),
                LfoShape::RandomSmooth => {
                    let (a, b) = (hash_cycle(n - 1.0, *seed), hash_cycle(n, *seed));
                    a + (b - a) * frac as f32
                }
                periodic => periodic.evaluate_periodic(Phase((frac as f32).min(1.0))),
            },
            Self::Drawn(points) => drawn_value(points, frac),
        }
    }

    /// The next corner strictly after `c` (`forward`) or before it, in
    /// cycles, with zero crossings when `kinks`. `None` for a wave with
    /// none (a sine without kinks, an empty drawing).
    fn next_corner(&self, c: f64, forward: bool, kinks: bool) -> Option<f64> {
        let n = c.floor();
        let t = tol(c);
        let mut best: Option<f64> = None;
        let mut consider = |x: f64| {
            if forward {
                if x > c + t && best.is_none_or(|b| x < b) {
                    best = Some(x);
                }
            } else if x < c - t && best.is_none_or(|b| x > b) {
                best = Some(x);
            }
        };
        let cycles = if forward {
            [n, n + 1.0, n + 2.0]
        } else {
            [n - 2.0, n - 1.0, n]
        };
        for k in cycles {
            self.corners_in(k, kinks, &mut consider);
        }
        best
    }

    /// Every corner of cycle `k`, in cycles.
    fn corners_in(&self, k: f64, kinks: bool, f: &mut impl FnMut(f64)) {
        match self {
            Self::Shape { shape, seed } => match shape {
                LfoShape::Sine => {
                    if kinks {
                        f(k);
                        f(k + 0.5);
                    }
                }
                LfoShape::Triangle => {
                    f(k + 0.25);
                    f(k + 0.75);
                    if kinks {
                        f(k);
                        f(k + 0.5);
                    }
                }
                LfoShape::Square => {
                    f(k);
                    f(k + 0.5);
                }
                LfoShape::Sawtooth | LfoShape::SawtoothDown => {
                    f(k);
                    if kinks {
                        f(k + 0.5);
                    }
                }
                LfoShape::Random => f(k),
                LfoShape::RandomSmooth => {
                    f(k);
                    if kinks {
                        let (a, b) = (hash_cycle(k - 1.0, *seed), hash_cycle(k, *seed));
                        if (a < 0.0 && b > 0.0) || (a > 0.0 && b < 0.0) {
                            f(k + f64::from(a) / (f64::from(a) - f64::from(b)));
                        }
                    }
                }
            },
            Self::Drawn(points) => {
                let len = points.len();
                for (i, p) in points.iter().enumerate() {
                    let start = k + f64::from(p.at);
                    f(start);
                    if kinks && len > 1 {
                        let (q, end) = if i + 1 < len {
                            (points[i + 1], k + f64::from(points[i + 1].at))
                        } else {
                            (points[0], k + 1.0 + f64::from(points[0].at))
                        };
                        if let Some(x) = crossing(p.curve, p.value, q.value) {
                            f(start + x * (end - start));
                        }
                    }
                }
            }
        }
    }
}

/// The drawn period's value at `frac` in `[0, 1)`.
fn drawn_value(points: &[PeriodPoint], frac: f64) -> f32 {
    let Some(first) = points.first() else {
        return 0.0;
    };
    let last = points[points.len() - 1];
    if points.len() == 1 {
        return first.value;
    }
    let after = points.partition_point(|p| f64::from(p.at) <= frac);
    let (p, start, q, end) = if after == 0 {
        // Before the first point: the last piece, wrapped.
        (
            last,
            f64::from(last.at) - 1.0,
            *first,
            f64::from(first.at),
        )
    } else if after == points.len() {
        (last, f64::from(last.at), *first, 1.0 + f64::from(first.at))
    } else {
        let p = points[after - 1];
        let q = points[after];
        (p, f64::from(p.at), q, f64::from(q.at))
    };
    let t = ((frac - start) / (end - start)) as f32;
    p.curve.interpolate(p.value, q.value, t)
}

/// Where in `(0, 1)` a `curve` piece from `a` to `b` crosses zero, if its
/// ends have opposite signs: in closed form for a line, by bisection on the
/// curve otherwise (deterministic: a fixed number of halvings).
fn crossing(curve: CurveType, a: f32, b: f32) -> Option<f64> {
    if !((a < 0.0 && b > 0.0) || (a > 0.0 && b < 0.0)) {
        return None;
    }
    if curve == CurveType::Linear {
        return Some(f64::from(a) / (f64::from(a) - f64::from(b)));
    }
    let (mut lo, mut hi) = (0.0f32, 1.0f32);
    let below = a < 0.0;
    for _ in 0..30 {
        let mid = 0.5 * (lo + hi);
        let v = curve.interpolate(a, b, mid);
        if (v < 0.0) == below {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    Some(f64::from(0.5 * (lo + hi)))
}

#[cfg(test)]
mod tests;
