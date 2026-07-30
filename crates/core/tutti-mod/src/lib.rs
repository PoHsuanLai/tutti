//! Modulation for the Tutti engine — source, target, and routing.
//!
//! Three roles: rules (routing table), dispatch (router), receive (target).
//!
//! ## Quick start — [`ModMatrix`]
//! The front door is a fluent builder; you rarely touch the primitives below.
//! ```
//! # #[cfg(feature = "routing")] {
//! use tutti_mod::{ModMatrix, Lfo, LfoShape, SourceRate};
//! use tutti_types::{Beat, Hz, Seconds};
//!
//! let mut m = ModMatrix::new();
//! let cutoff = m.target(1000.0, 0.0, 2000.0);   // a modulatable param
//! let gain   = m.target(0.5,    0.0, 1.0);
//!
//! // Each source runs at its own rate: 2 Hz free, 1 cycle/beat synced.
//! m.route(Lfo::new(LfoShape::Sine), SourceRate::free_running(Hz(2.0), 0.0))
//!     .to(&cutoff).depth(1.0);
//! m.route(Lfo::new(LfoShape::Triangle), SourceRate::beat_synced(Hz(1.0), 0.0))
//!     .to(&gain).depth(0.5);
//!
//! let mut driver = m.build();
//! // Once per frame: pass the transport beat + seconds since the last frame.
//! for i in 0..4 { driver.run(Beat(i as f64 * 0.25), Seconds(1.0 / 60.0)); }
//! let hz = cutoff.value();                           // read the modulated value
//! # assert!((0.0..=2000.0).contains(&hz));
//! # }
//! ```
//!
//! ## The source (pure floor — `--no-default-features`)
//! A [`Modulator`] is `phase -> value` (state-threaded, like `Iterator::scan`):
//! an LFO is `sin`, sample & hold is stepped noise. It knows nothing of graphs,
//! ports, or audio — the same modulator could drive a filter cutoff, a UI
//! colour, or a spring. [`shape`], [`fold`], [`curve_apply`] are the shared math.
//!
//! ## One value, three sampling rates
//!
//! The rates below are **not three designs**. They are one function read at
//! three speeds, and knowing that is the difference between picking a rate and
//! thinking you must pick a mechanism.
//!
//! [`Curve`] is that function: `beat -> Option<f32>`, holding no clock and
//! consulting no loop range, so the *reader* supplies the position. An
//! automation envelope, an LFO, a constant, and the summing [`LayeredCurve`]
//! are all `Curve`s. Because `LayeredCurve` is itself one, the accumulator
//! (`clamp(base + Σ layer(beat), [min, max])`) is shared across every rate —
//! one summation rule, the rate chosen by whoever reads it.
//!
//! | Rate | Sink | How it gets the beat |
//! |---|---|---|
//! | per **frame** | [`AtomicTarget`] | the driver is handed the beat; it collapses to a scalar and mirrors it into an `AtomicF32` |
//! | per **block** | a plugin's param producer | holds the [`LayeredCurve`] and samples it at each block's real beats |
//! | per **sample** | `AutomationLane`, `ModulatorNode` (`tutti-units`) | the beat arrives as a *signal* on the node's `BEAT_PORTS` inputs |
//!
//! **Which do I want?** The frame rate is the default and is always correct —
//! ask for more only when the sink reads faster than the frame rate, where a
//! frame scalar shows up as a staircase and a finer rate traces the ramp. The
//! per-sample tier costs a graph edge (the node must be wired to the transport
//! clock); the per-block tier costs nothing extra but requires a sink that
//! accepts a curve, which [`AtomicTarget`] deliberately does not — it collapses
//! at a fixed beat, so a curve stored there would never move.
//!
//! The tiers agree on *values* by construction, not by coincidence: both the
//! scalar path ([`ModPreFrame::run`]) and the curve path ([`ShapedCurve`])
//! apply the identical `shape(raw, depth, polarity, curve) * (max - min)`
//! expression to the same [`shape`] function, so a route that switches delivery
//! does not change what the listener hears.
//!
//! **Where the tiers are reached from.** A [`Modulator`] is rate-agnostic — the
//! *adapter* around it picks the tier. `tutti_mod::Lfo` sampled by
//! [`ModPreFrame`] is frame-rate; the same `Lfo` inside
//! `tutti_units::ModulatorNode` (aliased `LfoNode`) is per-sample. One
//! modulator, two adapters — not two LFOs.
//!
//! One gap is known and deliberate: the routing subsystem cannot currently
//! deliver a curve to a **per-sample** sink for a native param, because
//! [`AtomicTarget`] is the only sink native nodes use. Its module doc tracks
//! the audio-rate sink as later work.
//!
//! ## The target + routing (the `routing`/`bevy` features)
//! - **Receive** — [`ModTarget`]: a keyed accumulator (`base + Σ keyed offsets`,
//!   clamped). Concrete: [`AtomicTarget`] (mirrors its value into a shared
//!   `AtomicF32` the consumer reads lock-free), a frame-rate cap over a
//!   [`LayeredCurve`].
//! - **Dispatch** — [`ModRouter`] / [`ModBus`]: an id→target map keyed by
//!   [`ModTargetId`].
//! - **Rules** — [`ModRoutingSnapshot`] / [`ModRoutingTable`]: an `RtPublish`-hot-
//!   swapped mod-matrix of [`ModEdge`]s.
//! - **Driver** — [`ModPreFrame`]: the once-per-frame producer that samples each
//!   source and dispatches its shaped offset by id.
//!   It owns the sources and threads their state across frames.
//!
//! ## Cascading — a source that modulates another source
//! A [`SourceRate`]'s frequency is a [`Rate`]: either a constant, or a
//! [`Param<Hz>`](tutti_types::Param) read fresh each frame. Point an
//! [`AtomicTarget`] at that same cell and one LFO drives another's rate, using
//! the ordinary target/edge machinery — no special case in the driver:
//! ```
//! # #[cfg(feature = "routing")] {
//! use tutti_mod::{AtomicTarget, Lfo, LfoShape, SourceRate, Sourced};
//! use tutti_types::{Hz, Param};
//!
//! let rate: Param<Hz> = Param::new(Hz(2.0));
//! // The target writes the cell the source reads — one cell, two views.
//! let target = AtomicTarget::with_mirror(2.0, 2.0, 10.0, rate.as_atomic());
//! let wobbling = Sourced::new(Lfo::new(LfoShape::Sine),
//!     SourceRate::free_running(rate.clone(), 0.0));
//! # let _ = (target, wobbling);
//! # }
//! ```
//! Build one cell and clone the handle: a separately-minted `Param<Hz>`
//! compiles and modulates nothing. A cascade lags by at most one frame, which
//! is what lets a cycle (A drives B's rate, B drives A's) settle instead of
//! recursing.
//!
//! ## Module map
//! - `modulator`, `lfo`, `shape` — the pure source + math.
//! - `id` — [`ModTargetId`] (target address) + [`LayerKey`] (contributor key).
//! - `target` — [`ModTarget`], the keyed sink.
//! - `param`, `router`, `routing`, `driver` — the routing subsystem
//!   (feature-gated).
//! - `matrix` — [`ModMatrix`], the fluent builder over all of the above.

#![forbid(unsafe_code)]

mod id;
mod lfo;
mod modulator;
mod shape;
mod target;

#[cfg(feature = "routing")]
mod curve;
#[cfg(feature = "routing")]
mod curve_source;
#[cfg(feature = "routing")]
mod driver;
#[cfg(feature = "routing")]
mod layered;
#[cfg(feature = "routing")]
mod matrix;
#[cfg(feature = "routing")]
mod mod_params;
#[cfg(feature = "routing")]
mod param;
#[cfg(feature = "routing")]
mod router;
#[cfg(feature = "routing")]
mod routing;

pub use audio_automation::CurveType;
pub use id::{LayerKey, ModTargetId};
pub use lfo::{Lfo, RandomState, SampleHold};
pub use modulator::Modulator;
pub use shape::{curve_apply, fold, shape, LfoShape, Polarity};
pub use target::ModTarget;

#[cfg(feature = "routing")]
pub use curve::Curve;
#[cfg(feature = "routing")]
pub use curve_source::{BeatLfo, CurveModulator, EdgeShape, ShapedCurve};
#[cfg(feature = "routing")]
pub use driver::{ErasedModulator, ModPreFrame, Rate, SourceRate, Sourced};
#[cfg(feature = "routing")]
pub use layered::LayeredCurve;
#[cfg(feature = "routing")]
pub use matrix::{ModMatrix, Route, RouteTo, TargetHandle};
#[cfg(feature = "routing")]
pub use mod_params::ModParams;
#[cfg(feature = "routing")]
pub use param::AtomicTarget;
#[cfg(feature = "routing")]
pub use router::{ModBus, ModRouter};
#[cfg(feature = "routing")]
pub use routing::{ModEdge, ModRoutingSnapshot, ModRoutingTable};

/// End-to-end: the whole modulation subsystem — source, id, sink, snapshot,
/// driver.
///
/// ```
/// # #[cfg(feature = "routing")] {
/// use std::sync::Arc;
/// use tutti_mod::{
///     Lfo, LfoShape, LayerKey, ModTargetId,
///     ModBus, ModRouter, ModTarget, AtomicTarget,
///     ModEdge, ModRoutingTable, ModPreFrame, Sourced, SourceRate, ErasedModulator,
/// };
/// use tutti_types::{Beat, Hz, Seconds};
///
/// // Two targets: a filter cutoff and an amp gain, each a ranged accumulator.
/// let cutoff = Arc::new(AtomicTarget::new(1000.0, 0.0, 2000.0));
/// let gain   = Arc::new(AtomicTarget::new(0.5, 0.0, 1.0));
/// let (id_cut, id_gain) = (ModTargetId::next(), ModTargetId::next());
///
/// // The router: id -> target sink.
/// let bus = Arc::new(ModBus::new());
/// bus.insert(id_cut,  cutoff.clone());
/// bus.insert(id_gain, gain.clone());
///
/// // The source registry (indices line up with ModEdge::source).
/// let sources: Vec<Box<dyn ErasedModulator>> = vec![
///     Box::new(Sourced::new(Lfo::new(LfoShape::Sine),
///         SourceRate::beat_synced(Hz(1.0), 0.0))),      // source 0
///     Box::new(Sourced::new(Lfo::new(LfoShape::Triangle),
///         SourceRate::beat_synced(Hz(1.0), 0.0))),      // source 1
/// ];
///
/// // The mod-matrix: source0 -> cutoff @ full depth; source1 -> gain @ half.
/// let mut table = ModRoutingTable::new();
/// table.set_edges([
///     ModEdge::linear(0, id_cut,  LayerKey(1), 1.0, 0.0, 2000.0),
///     ModEdge::linear(1, id_gain, LayerKey(1), 0.5, 0.0, 1.0),
/// ], 2);
/// table.commit();
///
/// // The driver: samples each source once/frame, dispatches by id.
/// let mut driver = ModPreFrame::new(table.snapshot_arc());
/// driver.set_router(bus.clone());
/// driver.set_sources(sources);
///
/// // Run several frames (beat-synced → phase == beat); values stay in range.
/// for i in 0..4 { driver.run(Beat(i as f64 * 0.25), Seconds(0.0)); }
/// assert!((0.0..=2000.0).contains(&cutoff.final_value()));
/// assert!((0.0..=1.0).contains(&gain.final_value()));
///
/// // Hot-swap the routing mid-stream: drop the gain edge, keep cutoff.
/// table.set_edges([
///     ModEdge::linear(0, id_cut, LayerKey(1), 1.0, 0.0, 2000.0),
/// ], 2);
/// table.commit();          // RtPublish::publish — the driver sees it next frame
/// driver.run(Beat(0.5), Seconds(0.0)); // gain's stale layer cleared; cutoff re-asserted
/// assert!((gain.final_value() - 0.5).abs() < 1e-6, "gain fell back to base");
/// # }
/// ```
#[cfg(all(doctest, feature = "routing"))]
struct _UsageDoctest;
