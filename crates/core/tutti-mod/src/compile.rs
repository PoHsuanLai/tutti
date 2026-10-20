//! [`compile`]: declarations in, one [`ControlProgram`] per target out
//! (design doc 014 §7).
//!
//! A modulation matrix is what a host *says*: sources, routes from a source
//! to a target param (or to another route's depth), and per target a range
//! and an automation curve. [`compile`] turns that into what the engine
//! *runs*: a program per target for its control lane, a description of the
//! node each signal-driven source needs, and the event edges from those
//! nodes to the lanes. It is pure: no clock, no state, no `ArcLend`. Equal
//! declarations (in any order) compile to equal [`Compiled`]s, so a host
//! republishes only the programs that changed.
//!
//! - **Stable ids.** A layer's [`LayerId`] derives from its route's key and
//!   its depth-by route's key, so a depth edit keeps every id, and the sum
//!   order with it. A signal source keeps its [`SourceId`] across compiles
//!   (`prev`'s); a new one takes the lowest id free.
//! - **Rate edits** are dated: given the previous compile and the executor's
//!   play reading, a seconds-clocked layer whose rate changed gets a
//!   [`RateAnchor`] [`RATE_ANCHOR_MARGIN`] frames ahead, at the phase the
//!   previous program reaches there, so its phase is continuous (§4.3).
//! - **Refusals** are by name ([`CompileError`]).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::Arc;

use audio_automation::CurveType;
use tutti_types::{
    Amplitude, Beat, BeatDuration, Depth, ParamAddr, PhaseIncrement, PlayReading, SamplePosition,
    SampleRate, Samples, Seconds,
};

use crate::curve::Curve;
use crate::program::{
    Amount, Clock, ControlProgram, DepthBy, InputLayer, Layer, LayerId, ProgramError, ProgramTime,
    RateAnchor, SourceId, Wave,
};
use crate::shape::Polarity;

/// How far ahead of the executor's play reading a rate edit takes effect,
/// in frames (doc 014 §4.3): at least two of the largest block (4096
/// frames, the export's cap) plus slack for the publish to land, so every
/// lane reads the new program before its anchor and switches on the same
/// frame. About a quarter of a second at 48 kHz.
pub const RATE_ANCHOR_MARGIN: Samples = Samples(12_288);

/// How far ahead of the play reading a pending anchor must still be for a
/// second rate edit to reuse its frame: two of the largest block (4096
/// frames), the margin less its publish slack. Nearer than that, a publish
/// that lands late would reach the lanes after the anchor, and they would
/// jump by the difference of the two new rates times the lag; the edit
/// re-anchors at [`RATE_ANCHOR_MARGIN`] instead (see `anchored`).
const ANCHOR_REUSE_MIN: Samples = Samples(8_192);

/// A source's key: the host's identity for it (a Bevy host: the entity's
/// bits). Only compared.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct SourceKey(pub u64);

/// A route's key; see [`SourceKey`]. A layer's [`LayerId`] derives from it.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct RouteKey(pub u64);

/// A target: one param of one node (a Bevy host: the node entity's bits).
/// A plugin parameter is `ParamAddr::Id`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct TargetKey {
    /// The node, in the host's identity.
    pub node: u64,
    /// The param on it.
    pub param: ParamAddr,
}

/// An ADSR's stages, for a note envelope ([`Source::NoteEnvelope`]).
///
/// The sustain is an [`Amplitude`], the held level as a fraction of the
/// envelope's peak (1.0): the convention `tutti-polysynth`'s
/// `EnvelopeConfig` already uses. The compiler refuses one outside `[0, 1]`:
/// a note envelope's raw value stays in `[0, 1]`.
///
/// Equality is bitwise, so it is [`Eq`].
#[derive(Clone, Copy, Debug)]
pub struct Adsr {
    /// Note-on to the peak.
    pub attack: Seconds,
    /// The peak to the sustain level.
    pub decay: Seconds,
    /// The held level, `0..=1` of the peak.
    pub sustain: Amplitude,
    /// Note-off to zero.
    pub release: Seconds,
}

impl Adsr {
    fn bits(&self) -> [u32; 4] {
        [
            self.attack.get().to_bits(),
            self.decay.get().to_bits(),
            self.sustain.get().to_bits(),
            self.release.get().to_bits(),
        ]
    }

    fn valid(&self) -> bool {
        let time = |s: Seconds| s.get() >= 0.0 && s.get().is_finite();
        time(self.attack)
            && time(self.decay)
            && time(self.release)
            && (0.0..=1.0).contains(&self.sustain.get())
    }
}

impl PartialEq for Adsr {
    fn eq(&self, other: &Self) -> bool {
        self.bits() == other.bits()
    }
}

impl Eq for Adsr {}

/// What a source is.
#[derive(Clone, Debug, PartialEq)]
pub enum Source {
    /// A wave on a clock: a closed-form layer, no node. The clock's anchor
    /// is the compiler's to set; a declared one is refused
    /// ([`CompileError::AnchorDeclared`]).
    Lfo {
        /// One period.
        wave: Wave,
        /// The clock it runs on.
        clock: Clock,
    },
    /// A constant the user moves, a raw value (`shape` saturates past ±1).
    /// Moving it recompiles what it reaches: a layer of [`Wave::Const`].
    Macro {
        /// The raw value.
        value: f32,
    },
    /// An envelope follower: a node whose audio input the host wires, its
    /// level sent into each lane it reaches (doc 014 §6).
    Follower {
        /// Rise time constant.
        attack: Seconds,
        /// Fall time constant.
        release: Seconds,
    },
    /// A note envelope: a node on a MIDI input, its ADSR sent into each
    /// lane it reaches (doc 014 §6).
    NoteEnvelope {
        /// The stages.
        adsr: Adsr,
    },
}

/// Where a route goes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RouteTarget {
    /// A target param: a layer (or an input layer) of its program.
    Param(TargetKey),
    /// Another route's depth: that route's layer is scaled by this route's
    /// source through this route's amount ([`DepthBy`]). The other route
    /// must go to a param (a chain is refused), and both sources must be
    /// time functions (an LFO or a macro).
    Depth(RouteKey),
}

/// A route: a source into a target, shaped by an amount.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct RouteDecl {
    /// What modulates.
    pub source: SourceKey,
    /// What it modulates.
    pub to: RouteTarget,
    /// A fraction of the target's range (of the scale, for a depth route).
    pub depth: Depth,
    /// Both ways or one.
    pub polarity: Polarity,
    /// The response curve on the magnitude.
    pub curve: CurveType,
}

impl RouteDecl {
    fn amount(&self) -> Amount {
        Amount {
            depth: self.depth,
            polarity: self.polarity,
            curve: self.curve,
        }
    }
}

/// A target's range and automation. The knob is a cell, not a declaration
/// (doc 014 §4.2), and so is the range the lane reads: the range here is
/// checked, and the host stores it into the target's range cell.
#[derive(Clone)]
pub struct TargetDecl {
    /// The range's bottom.
    pub min: f32,
    /// The range's top.
    pub max: f32,
    /// Replaces the knob where it has a value. A curve that is not frozen
    /// (one whose [`Curve::frozen`] returns a copy) is refused: a program
    /// holds the curve itself, compared by identity.
    pub automation: Option<Arc<dyn Curve>>,
}

impl fmt::Debug for TargetDecl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TargetDecl")
            .field("min", &self.min)
            .field("max", &self.max)
            .field("automation", &self.automation.as_ref().map(|_| "<curve>"))
            .finish()
    }
}

/// Everything a host declares, as slices of `(key, value)` in any order.
#[derive(Clone, Copy, Debug)]
pub struct Declarations<'a> {
    /// The sources.
    pub sources: &'a [(SourceKey, Source)],
    /// The routes.
    pub routes: &'a [(RouteKey, RouteDecl)],
    /// Per target: range and automation.
    pub targets: &'a [(TargetKey, TargetDecl)],
}

/// The node a signal-driven source needs, described: what its constructor
/// takes (`EnvelopeFollowerNode::from_decl`, `NoteEnvelopeNode::from_decl`
/// in `tutti-nodes`), and nothing a running node keeps.
///
/// Equality is bitwise, so it is [`Eq`].
#[derive(Clone, Copy, Debug)]
pub struct SourceNode {
    /// The address its events carry (`ParamAddr::Id(id.0)`) on each lane's
    /// event input.
    pub id: SourceId,
    /// Which node, and its params.
    pub kind: SignalKind,
}

/// A signal-driven source's node kind and params.
#[derive(Clone, Copy, Debug)]
pub enum SignalKind {
    /// An envelope follower: one-pole attack and release on its audio
    /// input's level, sent raw in `[0, 1]` on the play-clock grid.
    Follower {
        /// Rise time constant.
        attack: Seconds,
        /// Fall time constant.
        release: Seconds,
    },
    /// A note envelope on its MIDI input, sent raw in `[0, 1]`.
    NoteEnvelope {
        /// The stages.
        adsr: Adsr,
    },
}

impl PartialEq for SignalKind {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (
                Self::Follower { attack, release },
                Self::Follower {
                    attack: a,
                    release: r,
                },
            ) => {
                attack.get().to_bits() == a.get().to_bits()
                    && release.get().to_bits() == r.get().to_bits()
            }
            (Self::NoteEnvelope { adsr }, Self::NoteEnvelope { adsr: b }) => adsr == b,
            _ => false,
        }
    }
}

impl Eq for SignalKind {}

impl PartialEq for SourceNode {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id && self.kind == other.kind
    }
}

impl Eq for SourceNode {}

/// What [`compile`] produces.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Compiled {
    /// One program per target with a route or automation; absent: no lane.
    pub programs: BTreeMap<TargetKey, ControlProgram>,
    /// A node per signal-driven source declared, with its [`SourceId`];
    /// built by the host.
    pub nodes: BTreeMap<SourceKey, SourceNode>,
    /// Event edges, a source's node → a target's lane.
    pub edges: BTreeSet<(SourceKey, TargetKey)>,
    /// Each LFO's dated clock (its rate anchor, doc 014 §4.3), routed or
    /// not: every layer and depth scaler it drives uses this one clock, so
    /// one LFO stays phase-coherent across its targets, and the next
    /// compile dates its rate edits from it.
    pub clocks: BTreeMap<SourceKey, Clock>,
    /// The id the next new signal source takes: one past every id this
    /// lineage of compiles has given, so no id is ever given twice (doc 014
    /// §7). It wraps after `2^32` sources, skipping the ids still held.
    pub next_source: SourceId,
}

/// A clock holds no NaN: `compile` refuses a rate, period or phase that is
/// not finite (an infinite phase would make a carried anchor's jump
/// `inf − inf`), and an anchor's phase is a finite `Clock::cycles` plus
/// phase jumps that are each a difference of two finite `f32`s, so their
/// `f64` sum stays finite.
impl Eq for Compiled {}

/// Why [`compile`] refused, by name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompileError {
    /// Two sources with one key.
    DuplicateSource(SourceKey),
    /// Two routes with one key.
    DuplicateRoute(RouteKey),
    /// Two targets with one key.
    DuplicateTarget(TargetKey),
    /// A route from a source not declared.
    UnknownSource {
        /// The route.
        route: RouteKey,
    },
    /// A depth route to a route not declared.
    UnknownRoute {
        /// The depth route.
        route: RouteKey,
    },
    /// A route to a target not declared: a route needs the target's range.
    UndeclaredTarget {
        /// The route.
        route: RouteKey,
    },
    /// A source whose params are out of their domain: a NaN anywhere, a
    /// clock's rate, period or phase or a constant that is not finite, a negative or infinite time, a sustain outside `[0, 1]`, a drawn wave
    /// whose points do not ascend in `[0, 1)` with finite values.
    BadSource(SourceKey),
    /// An LFO declared with a rate anchor: anchors are the compiler's.
    AnchorDeclared(SourceKey),
    /// A route with a depth that is not finite (an infinite depth makes
    /// `inf · 0` a NaN in the value rule).
    BadRoute(RouteKey),
    /// A target range with a NaN bound.
    NanRange(TargetKey),
    /// An automation curve that is not frozen (its `Curve::frozen` returns
    /// a copy): a program compares it by identity, and reads it on the
    /// audio thread.
    LiveCurve(TargetKey),
    /// A target with more than [`MAX_LANE_LAYERS`](crate::MAX_LANE_LAYERS)
    /// time-function layers.
    TooManyLayers {
        /// The target.
        target: TargetKey,
        /// How many.
        count: usize,
    },
    /// A target with more than [`MAX_LANE_INPUTS`](crate::MAX_LANE_INPUTS)
    /// signal-source routes.
    TooManyInputs {
        /// The target.
        target: TargetKey,
        /// How many.
        count: usize,
    },
    /// A depth route to a route that is itself a depth route: `depth_by`
    /// chains are one deep.
    DepthByChain {
        /// The outer depth route.
        route: RouteKey,
    },
    /// A depth route from a signal-driven source, or to a route from one: a
    /// depth is scaled by a time function only.
    DepthBySignal {
        /// The depth route.
        route: RouteKey,
    },
    /// Two depth routes to one route: a layer has one `depth_by`.
    TwoDepthBy {
        /// The route both go to.
        route: RouteKey,
    },
    /// Two layers of one target derived the same [`LayerId`] (a 64-bit hash
    /// collision of their route keys).
    LayerIdCollision(TargetKey),
}

impl fmt::Display for CompileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateSource(k) => write!(f, "two sources share key {}", k.0),
            Self::DuplicateRoute(k) => write!(f, "two routes share key {}", k.0),
            Self::DuplicateTarget(k) => write!(f, "two targets share key {k:?}"),
            Self::UnknownSource { route } => {
                write!(f, "route {}: its source is not declared", route.0)
            }
            Self::UnknownRoute { route } => {
                write!(f, "route {}: the route it scales is not declared", route.0)
            }
            Self::UndeclaredTarget { route } => {
                write!(f, "route {}: its target is not declared", route.0)
            }
            Self::BadSource(k) => write!(f, "source {}: a param out of its domain", k.0),
            Self::AnchorDeclared(k) => {
                write!(f, "source {}: a rate anchor is the compiler's to set", k.0)
            }
            Self::BadRoute(k) => write!(f, "route {}: a NaN depth", k.0),
            Self::NanRange(k) => write!(f, "target {k:?}: a NaN range bound"),
            Self::LiveCurve(k) => write!(f, "target {k:?}: an automation curve not frozen"),
            Self::TooManyLayers { target, count } => write!(
                f,
                "target {target:?}: {count} layers, past MAX_LANE_LAYERS ({})",
                crate::MAX_LANE_LAYERS
            ),
            Self::TooManyInputs { target, count } => write!(
                f,
                "target {target:?}: {count} input layers, past MAX_LANE_INPUTS ({})",
                crate::MAX_LANE_INPUTS
            ),
            Self::DepthByChain { route } => {
                write!(f, "route {}: a depth_by chain deeper than one", route.0)
            }
            Self::DepthBySignal { route } => write!(
                f,
                "route {}: depth_by takes a time function on both sides",
                route.0
            ),
            Self::TwoDepthBy { route } => write!(f, "route {}: two depth routes", route.0),
            Self::LayerIdCollision(k) => write!(f, "target {k:?}: two layers share an id"),
        }
    }
}

impl std::error::Error for CompileError {}

/// The clock a macro's constant layer carries: it reads none, and a beat
/// clock has no corners for a constant.
const MACRO_CLOCK: Clock = Clock::Beats {
    per_cycle: BeatDuration(1.0),
    phase: PhaseIncrement(0.0),
};

/// The splitmix64 finalizer: a stable, platform-independent mix.
#[inline]
fn mix(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// The [`LayerId`] of `route`'s layer, scaled by `depth_by` (if any).
/// Stable: keys only, so an edit that keeps both keys keeps the id.
pub fn layer_id(route: RouteKey, depth_by: Option<RouteKey>) -> LayerId {
    let h = mix(route.0);
    LayerId(match depth_by {
        None => h,
        Some(d) => mix(h ^ mix(d.0 ^ 0xD1B5_4A32_D192_ED03)),
    })
}

/// Compiles `decl` to a program per target, a node per signal source and
/// the edges between them.
///
/// `prev` (the last compile) and `now` (the executor's next play reading,
/// `Editor::play_reading`) and `rate` (the graph's sample rate) are read
/// only to keep signal sources' ids and to date rate-edit anchors (doc 014
/// §4.3); the function is pure. With equal inputs it returns equal
/// results, whatever order the declarations come in.
///
/// # Errors
///
/// Each [`CompileError`], by name.
pub fn compile(
    decl: &Declarations<'_>,
    prev: Option<&Compiled>,
    now: PlayReading,
    rate: SampleRate,
) -> Result<Compiled, CompileError> {
    let sources = index(decl.sources, CompileError::DuplicateSource)?;
    let routes = index(decl.routes, CompileError::DuplicateRoute)?;
    let targets = index(decl.targets, CompileError::DuplicateTarget)?;

    for (&key, source) in &sources {
        check_source(key, source)?;
    }
    for (&key, route) in &routes {
        if !sources.contains_key(&route.source) {
            return Err(CompileError::UnknownSource { route: key });
        }
        if !route.depth.get().is_finite() {
            return Err(CompileError::BadRoute(key));
        }
    }
    for (&key, t) in &targets {
        if t.min.is_nan() || t.max.is_nan() {
            return Err(CompileError::NanRange(key));
        }
        if t.automation.as_ref().is_some_and(|c| c.frozen().is_some()) {
            return Err(CompileError::LiveCurve(key));
        }
    }

    // Depth routes: the route each scales, and its one scaler.
    let is_signal = |s: SourceKey| {
        matches!(
            sources[&s],
            Source::Follower { .. } | Source::NoteEnvelope { .. }
        )
    };
    let mut depth_of: BTreeMap<RouteKey, RouteKey> = BTreeMap::new();
    for (&key, route) in &routes {
        let RouteTarget::Depth(scaled) = route.to else {
            continue;
        };
        let Some(inner) = routes.get(&scaled) else {
            return Err(CompileError::UnknownRoute { route: key });
        };
        if matches!(inner.to, RouteTarget::Depth(_)) {
            return Err(CompileError::DepthByChain { route: key });
        }
        if is_signal(route.source) || is_signal(inner.source) {
            return Err(CompileError::DepthBySignal { route: key });
        }
        if depth_of.insert(scaled, key).is_some() {
            return Err(CompileError::TwoDepthBy { route: scaled });
        }
    }

    // Signal sources' nodes: `prev`'s ids kept, a new one the next never
    // given (`prev.next_source` on). An id is never given twice, so a lane
    // can never read a removed source's last segment as a new source's,
    // however many programs it skips (doc 014 §7): a lane that adopts a
    // program also forgets the inputs it does not read, so a removed
    // source's slot is free at once.
    let mut nodes = BTreeMap::new();
    let kept = |k: &SourceKey| prev.and_then(|p| p.nodes.get(k)).map(|n| n.id);
    for (&key, source) in &sources {
        if let Some(kind) = signal_kind(source) {
            if let Some(id) = kept(&key) {
                nodes.insert(key, SourceNode { id, kind });
            }
        }
    }
    let mut taken: BTreeSet<SourceId> = nodes.values().map(|n| n.id).collect();
    let mut next = prev.map_or(0, |p| p.next_source.0);
    for (&key, source) in &sources {
        if let Some(kind) = signal_kind(source) {
            if nodes.contains_key(&key) {
                continue;
            }
            // Past `u32::MAX` the ids wrap; one still held is skipped.
            while taken.contains(&SourceId(next)) {
                next = next.wrapping_add(1);
            }
            taken.insert(SourceId(next));
            nodes.insert(
                key,
                SourceNode {
                    id: SourceId(next),
                    kind,
                },
            );
            next = next.wrapping_add(1);
        }
    }

    // Layers and input layers, per target.
    let mut layers: BTreeMap<TargetKey, (Vec<Layer>, Vec<InputLayer>)> = BTreeMap::new();
    // Each layer's source (and its depth source), to name one a program
    // refuses.
    let mut origin: BTreeMap<LayerId, SourceKey> = BTreeMap::new();
    let mut edges = BTreeSet::new();
    // One dated clock per LFO, from its own clock in `prev`: every layer it
    // drives reads the same anchor, whichever target, route or depth slot.
    let mut clocks = BTreeMap::new();
    for (&key, source) in &sources {
        if let Source::Lfo { clock, .. } = source {
            let old = prev.and_then(|p| p.clocks.get(&key));
            clocks.insert(key, anchored(*clock, old, now, rate));
        }
    }
    let dated = |key: SourceKey| {
        let (clock, wave) = time_function(sources[&key]);
        (clocks.get(&key).copied().unwrap_or(clock), wave)
    };
    for (&key, route) in &routes {
        let RouteTarget::Param(target) = route.to else {
            continue;
        };
        if !targets.contains_key(&target) {
            return Err(CompileError::UndeclaredTarget { route: key });
        }
        let scaler = depth_of.get(&key).copied();
        let id = layer_id(key, scaler);
        let entry = layers.entry(target).or_default();
        origin.insert(id, route.source);
        match &sources[&route.source] {
            Source::Follower { .. } | Source::NoteEnvelope { .. } => {
                entry.1.push(InputLayer {
                    id,
                    source: nodes[&route.source].id,
                    amount: route.amount(),
                });
                edges.insert((route.source, target));
            }
            Source::Lfo { .. } | Source::Macro { .. } => {
                let (clock, wave) = dated(route.source);
                let depth_by = scaler.map(|d| {
                    let by = &routes[&d];
                    let (clock, wave) = dated(by.source);
                    DepthBy {
                        clock,
                        wave,
                        amount: by.amount(),
                    }
                });
                entry.0.push(Layer {
                    id,
                    clock,
                    wave,
                    amount: route.amount(),
                    depth_by,
                });
            }
        }
    }

    let mut programs = BTreeMap::new();
    for (&target, decl) in &targets {
        let (l, i) = layers.remove(&target).unwrap_or_default();
        if l.is_empty() && i.is_empty() && decl.automation.is_none() {
            continue;
        }
        let program = ControlProgram::new(decl.automation.clone(), l, i).map_err(|e| match e {
            ProgramError::TooManyLayers { count } => CompileError::TooManyLayers { target, count },
            ProgramError::TooManyInputs { count } => CompileError::TooManyInputs { target, count },
            ProgramError::DuplicateLayer(_) => CompileError::LayerIdCollision(target),
            // Sources and routes are checked above, so these do not reach
            // here; named by the layer's source if they do.
            ProgramError::BadDrawnWave(id) | ProgramError::NotANumber(id) => origin
                .get(&id)
                .map_or(CompileError::LayerIdCollision(target), |&s| {
                    CompileError::BadSource(s)
                }),
        })?;
        programs.insert(target, program);
    }

    Ok(Compiled {
        programs,
        nodes,
        edges,
        clocks,
        next_source: SourceId(next),
    })
}

/// `items` keyed, refusing a duplicate key.
fn index<K: Ord + Copy, V>(
    items: &[(K, V)],
    dup: impl Fn(K) -> CompileError,
) -> Result<BTreeMap<K, &V>, CompileError> {
    let mut out = BTreeMap::new();
    for (k, v) in items {
        if out.insert(*k, v).is_some() {
            return Err(dup(*k));
        }
    }
    Ok(out)
}

/// A source's params in their domain, or the refusal.
fn check_source(key: SourceKey, source: &Source) -> Result<(), CompileError> {
    let bad = Err(CompileError::BadSource(key));
    match source {
        Source::Lfo { wave, clock } => {
            let ok_clock = match *clock {
                Clock::Beats { per_cycle, phase } => {
                    per_cycle.get().is_finite() && phase.get().is_finite()
                }
                Clock::Seconds {
                    rate,
                    phase,
                    anchor,
                } => {
                    if anchor.is_some() {
                        return Err(CompileError::AnchorDeclared(key));
                    }
                    rate.get().is_finite() && phase.get().is_finite()
                }
            };
            let ok_wave = match wave {
                Wave::Shape { .. } => true,
                Wave::Const(v) => v.is_finite(),
                Wave::Drawn(points) => {
                    points
                        .iter()
                        .all(|p| (0.0..1.0).contains(&p.at.get()) && p.value.is_finite())
                        && points.windows(2).all(|w| w[0].at < w[1].at)
                }
            };
            if ok_clock && ok_wave {
                Ok(())
            } else {
                bad
            }
        }
        Source::Macro { value } => {
            if value.is_finite() {
                Ok(())
            } else {
                bad
            }
        }
        Source::Follower { attack, release } => {
            let time = |s: Seconds| s.get() >= 0.0 && s.get().is_finite();
            if time(*attack) && time(*release) {
                Ok(())
            } else {
                bad
            }
        }
        Source::NoteEnvelope { adsr } => {
            if adsr.valid() {
                Ok(())
            } else {
                bad
            }
        }
    }
}

/// A signal source's node kind; `None` for a time function.
fn signal_kind(source: &Source) -> Option<SignalKind> {
    match *source {
        Source::Follower { attack, release } => Some(SignalKind::Follower { attack, release }),
        Source::NoteEnvelope { adsr } => Some(SignalKind::NoteEnvelope { adsr }),
        Source::Lfo { .. } | Source::Macro { .. } => None,
    }
}

/// A time function's clock and wave (an LFO's own; a macro's constant).
fn time_function(source: &Source) -> (Clock, Wave) {
    match source {
        Source::Lfo { wave, clock } => (*clock, wave.clone()),
        Source::Macro { value } => (MACRO_CLOCK, Wave::Const(*value)),
        // Callers reach here with a time function only (signal sources are
        // input layers, and refused as depth scalers).
        Source::Follower { .. } | Source::NoteEnvelope { .. } => (MACRO_CLOCK, Wave::Const(0.0)),
    }
}

/// An LFO's `clock` (as declared: no anchor) dated against its own dated
/// clock in the previous compile, `old` (doc 014 §4.3). Keyed by source,
/// so a new route, a route moved to this source, or a route whose
/// `depth_by` changed reads the anchor every other layer of it reads.
///
/// - A beat clock, or no seconds clock before: as declared.
/// - The same rate: the old anchor carried, if it is in the current epoch
///   (an anchor from another epoch is ignored by the lane, so it is
///   dropped).
/// - A new rate: an anchor at `now + RATE_ANCHOR_MARGIN`, at the old
///   clock's phase there, moving at the old rate before it.
/// - A new rate while the old clock's own anchor is still at least
///   [`ANCHOR_REUSE_MIN`] ahead of `now`: the edit takes effect on that
///   frame instead, which is where the old rate gives way anyway, and the
///   publish has the room to land before it.
///
/// **The error that remains.** One anchor holds one rate change, so it
/// cannot express R0 → R1 → R2. When a second edit (to R2) comes while an
/// anchor (R0 → R1 at `a`) is pending but nearer than `ANCHOR_REUSE_MIN`,
/// the new anchor goes at `now + margin`, past `a`, with the old clock's
/// exact phase there and `prev_rate` R1; before it the new clock runs at R1
/// back from that phase. That agrees with the old clock from `a` on, but
/// between `now` and `a` the old clock ran at R0, so a lane that switches
/// to the new program at frame `s < a` steps by `(R1 − R0) · (a − s)`
/// (in cycles, with `a − s` in seconds: under `ANCHOR_REUSE_MIN` frames);
/// one that switches at or after `a` does not step. Reusing `a` instead
/// would step by `(R2 − R1) · lag` for any lane the publish reaches after
/// `a`, unbounded by the margin; re-anchoring bounds the step by the
/// frames that were left.
///
/// A changed declared phase is a deliberate jump: it moves a carried or new
/// anchor's phase by the same amount, as it moves an unanchored clock.
fn anchored(clock: Clock, old: Option<&Clock>, now: PlayReading, rate: SampleRate) -> Clock {
    let Clock::Seconds {
        rate: r, phase: p, ..
    } = clock
    else {
        return clock;
    };
    let Some(&Clock::Seconds {
        rate: old_rate,
        phase: old_phase,
        anchor: old_anchor,
    }) = old
    else {
        return clock;
    };
    let live = old_anchor.filter(|a| a.epoch == now.epoch);
    let jump = f64::from(p.get()) - f64::from(old_phase.get());
    let shifted = |a: RateAnchor| RateAnchor {
        phase: a.phase + jump,
        ..a
    };
    let anchor = if r.get().to_bits() == old_rate.get().to_bits() {
        live.map(shifted)
    } else {
        match live {
            Some(a)
                if a.at
                    .since(now.play)
                    .is_some_and(|left| left >= ANCHOR_REUSE_MIN) =>
            {
                Some(shifted(a))
            }
            _ => {
                let at = now.after(RATE_ANCHOR_MARGIN).play;
                let t = ProgramTime {
                    beat: Beat(0.0),
                    play: SamplePosition(at.get() as f64),
                    play_epoch: now.epoch,
                    rate,
                };
                old.and_then(|c| c.cycles(&t)).map(|phase| RateAnchor {
                    epoch: now.epoch,
                    at,
                    phase: phase + jump,
                    prev_rate: old_rate,
                })
            }
        }
    };
    Clock::Seconds {
        rate: r,
        phase: p,
        anchor,
    }
}

#[cfg(test)]
mod tests;
