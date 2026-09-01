#![forbid(unsafe_code)]
#![doc = include_str!("../README.md")]

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
pub use driver::{ErasedModulator, ModPreFrame, Rate, SourceClock, SourceRate, Sourced};
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
/// use tutti_types::{Beat, BeatDuration, Seconds};
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
///         SourceRate::beat_synced(BeatDuration(1.0), 0.0))),      // source 0
///     Box::new(Sourced::new(Lfo::new(LfoShape::Triangle),
///         SourceRate::beat_synced(BeatDuration(1.0), 0.0))),      // source 1
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
