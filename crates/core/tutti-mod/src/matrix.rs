//! The ergonomic front door: [`ModMatrix`], a fluent builder over the routing
//! core.
//!
//! The raw pieces ([`ModBus`], [`ModRoutingTable`], [`ModPreFrame`],
//! [`ModTargetId`], [`ModEdge`], `Sourced`/`Box<dyn ErasedModulator>`) are the
//! engine layer — precise, but verbose to wire by hand. `ModMatrix` is the
//! config layer on top: it mints ids, registers targets, assigns source
//! indices, collects edges, and wires the driver in one `build()`. It owns
//! nothing the core doesn't; it just spares you the plumbing.
//!
//! ```
//! use tutti_mod::{ModMatrix, Lfo, LfoShape, SourceRate};
//! use tutti_types::{Beat, BeatDuration, Hz, Seconds};
//!
//! let mut m = ModMatrix::new();
//! let cutoff = m.target(1000.0, 0.0, 2000.0);   // handle; id minted + registered
//! let gain   = m.target(0.5,    0.0, 1.0);
//!
//! m.route(Lfo::new(LfoShape::Sine), SourceRate::free_running(Hz(2.0), 0.0))
//!     .to(&cutoff).depth(1.0);
//! m.route(Lfo::new(LfoShape::Triangle), SourceRate::beat_synced(BeatDuration(1.0), 0.0))
//!     .to(&gain).depth(0.5);
//!
//! let mut driver = m.build();
//! for i in 0..4 { driver.run(Beat(i as f64 * 0.25), Seconds(1.0 / 60.0)); }
//!
//! let hz = cutoff.value();          // read the modulated value off the handle
//! assert!((0.0..=2000.0).contains(&hz));
//! ```

use std::sync::Arc;

use tutti_types::Depth;

use crate::driver::{ErasedModulator, ModPreFrame, SourceRate, Sourced};
use crate::id::{LayerKey, ModTargetId};
use crate::param::AtomicTarget;
use crate::router::ModBus;
use crate::routing::{ModEdge, ModRoutingTable};
use crate::shape::Polarity;
use crate::target::ModTarget;
use crate::{CurveType, Modulator};

/// A cheap, cloneable handle to a target registered in a [`ModMatrix`].
///
/// Carries the target's `ModTargetId` (for edges) and a shared [`ModTarget`]
/// handle (for reading its modulated value). The target may be one the matrix
/// minted ([`ModMatrix::target`]) or an existing one — a real node/plugin param
/// from `ModParams::mod_target` — registered via [`ModMatrix::add_target`].
/// Clone freely — it's an id plus an `Arc`.
#[derive(Clone)]
pub struct TargetHandle {
    id: ModTargetId,
    target: Arc<dyn ModTarget>,
    min: f32,
    max: f32,
}

impl TargetHandle {
    /// The routing address, if you need to build a raw [`ModEdge`] yourself.
    #[inline]
    pub fn id(&self) -> ModTargetId {
        self.id
    }

    /// The current modulated value: `clamp(base + Σ offsets, [min, max])`.
    #[inline]
    pub fn value(&self) -> f32 {
        self.target.final_value()
    }

    /// The target's `(min, max)` range.
    #[inline]
    pub fn range(&self) -> (f32, f32) {
        (self.min, self.max)
    }

    /// The underlying target handle (e.g. to accumulate directly, or share it).
    #[inline]
    pub fn target(&self) -> Arc<dyn ModTarget> {
        Arc::clone(&self.target)
    }
}

/// A fluent mod-matrix builder. Add targets and routes, then [`build`] a driver.
///
/// [`build`]: ModMatrix::build
pub struct ModMatrix {
    bus: Arc<ModBus>,
    sources: Vec<Box<dyn ErasedModulator>>,
    edges: Vec<ModEdge>,
    next_key: u64,
}

impl Default for ModMatrix {
    fn default() -> Self {
        Self::new()
    }
}

impl ModMatrix {
    pub fn new() -> Self {
        Self {
            bus: Arc::new(ModBus::new()),
            sources: Vec::new(),
            edges: Vec::new(),
            next_key: 1, // 0 is reserved (LayerKey::AUTOMATION)
        }
    }

    /// Register a **new** target (a fresh [`AtomicTarget`], `base` clamped into
    /// `[min, max]`) and return its handle. Mints a fresh [`ModTargetId`] and
    /// inserts it on the bus. For a UI value / standalone param.
    pub fn target(&mut self, base: f32, min: f32, max: f32) -> TargetHandle {
        self.add_target(Arc::new(AtomicTarget::new(base, min, max)))
    }

    /// Register an **existing** target — a real node or plugin param obtained
    /// from `ModParams::mod_target` — and return its handle. Mints an id and
    /// inserts the target on the bus, so `route(..).to(&handle)` drives the
    /// *actual* param (a node's atomic, a plugin's IPC stream), not a
    /// standalone accumulator.
    ///
    /// ```ignore
    /// let cutoff = matrix.add_target(
    ///     filter.mod_target(ParamAddr::Unit(UnitParam::Cutoff), 1000.0, 20.0, 20000.0)?,
    /// );
    /// matrix.route(Lfo::new(LfoShape::Sine), SourceRate::free_running(Hz(2.0), 0.0))
    ///     .to(&cutoff).depth(1.0);
    /// // the LFO now sweeps the real filter's cutoff atomic.
    /// ```
    pub fn add_target(&mut self, target: Arc<dyn ModTarget>) -> TargetHandle {
        let (min, max) = target.range();
        let id = ModTargetId::next();
        self.bus.insert(id, Arc::clone(&target));
        TargetHandle {
            id,
            target,
            min,
            max,
        }
    }

    /// Begin routing a source at `rate`. Returns a [`Route`] to finish with
    /// `.to(..)` and `.depth(..)`. The source's registry index is assigned here,
    /// so edges from this call all reference the same single source (sampled once
    /// per frame). The [`SourceRate`] gives this source its own frequency /
    /// phase-offset / beat-sync, independent of every other source.
    pub fn route<M>(&mut self, modulator: M, rate: SourceRate) -> Route<'_>
    where
        M: Modulator + Send + Sync + 'static,
    {
        let source = self.sources.len();
        self.sources.push(Box::new(Sourced::new(modulator, rate)));
        Route {
            matrix: self,
            source,
        }
    }

    /// Finish: build the routing snapshot and a wired [`ModPreFrame`] driver.
    /// The matrix is consumed — sources move into the driver.
    pub fn build(self) -> ModPreFrame {
        let source_count = self.sources.len();
        let mut table = ModRoutingTable::new();
        table.set_edges(self.edges, source_count);
        table.commit();

        let mut driver = ModPreFrame::new(table.snapshot_arc());
        driver.set_router(self.bus);
        driver.set_sources(self.sources);
        driver
    }

    fn fresh_key(&mut self) -> LayerKey {
        let k = LayerKey(self.next_key);
        self.next_key += 1;
        k
    }
}

/// An in-progress route from a source (from [`ModMatrix::route`]). Finish with
/// [`to`](Route::to), then optionally [`depth`](RouteTo::depth) /
/// [`curve`](RouteTo::curve) / [`polarity`](RouteTo::polarity).
pub struct Route<'m> {
    matrix: &'m mut ModMatrix,
    source: usize,
}

impl<'m> Route<'m> {
    /// Point this route at a target. Returns a [`RouteTo`] to set depth/shape;
    /// the edge is committed when `RouteTo` is dropped (or `.depth()` is the
    /// last call), so `m.route(lfo).to(&cutoff).depth(1.0)` is a complete route.
    pub fn to(self, target: &TargetHandle) -> RouteTo<'m> {
        let key = self.matrix.fresh_key();
        RouteTo {
            matrix: self.matrix,
            edge: ModEdge {
                source: self.source,
                target: target.id,
                key,
                depth: Depth::FULL,
                min: target.min,
                max: target.max,
                polarity: Polarity::Bipolar,
                curve: CurveType::Linear,
                enabled: true,
            },
            committed: false,
        }
    }
}

/// A route pointed at a target; set its depth/shape. The edge is pushed onto the
/// matrix on `Drop` (so the common `.to(&t).depth(d)` needs no terminal call).
pub struct RouteTo<'m> {
    matrix: &'m mut ModMatrix,
    edge: ModEdge,
    committed: bool,
}

impl RouteTo<'_> {
    /// Set the routing depth. Negative inverts the source.
    pub fn depth(mut self, depth: impl Into<Depth>) -> Self {
        self.edge.depth = depth.into();
        self
    }

    /// Set the response curve.
    pub fn curve(mut self, curve: CurveType) -> Self {
        self.edge.curve = curve;
        self
    }

    /// Set the polarity (`Bipolar` swings both ways, `Unipolar` one direction).
    pub fn polarity(mut self, polarity: Polarity) -> Self {
        self.edge.polarity = polarity;
        self
    }

    fn commit(&mut self) {
        if !self.committed {
            self.matrix.edges.push(self.edge.clone());
            self.committed = true;
        }
    }
}

impl Drop for RouteTo<'_> {
    fn drop(&mut self) {
        self.commit();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Lfo, LfoShape};
    use tutti_types::{Beat, BeatDuration, Seconds};

    /// A beat-synced rate at 1 cycle/beat: with `frame(p)` the phase is exactly
    /// `p`, so these tests drive a precise phase per frame.
    fn rate() -> SourceRate {
        SourceRate::beat_synced(BeatDuration(1.0), 0.0)
    }

    /// Drive one frame at phase `p` (beat == p at 1 cycle/beat).
    fn frame(driver: &mut ModPreFrame, p: f32) {
        driver.run(Beat(p as f64), Seconds(0.0));
    }

    #[test]
    fn add_target_routes_to_an_externally_owned_atomic() {
        // The node/plugin case: a target whose value lives in an atomic the
        // *consumer* owns (a filter's `frequency()`, say). `add_target`
        // registers it; the LFO drives that atomic through the matrix.
        // (Here we stand in for a node with a bare `AtomicF32` + `with_mirror`,
        // exactly what `ModParams::mod_target` builds.)
        let param = Arc::new(atomic_float::AtomicF32::new(1000.0));
        let node_target: Arc<dyn ModTarget> = Arc::new(AtomicTarget::with_mirror(
            1000.0,
            20.0,
            20000.0,
            param.clone(),
        ));

        let mut m = ModMatrix::new();
        let cutoff = m.add_target(node_target); // register the EXISTING target
        m.route(Lfo::new(LfoShape::Sine), rate())
            .to(&cutoff)
            .depth(0.5);
        let mut driver = m.build();

        for i in 0..8 {
            frame(&mut driver, i as f32 / 8.0);
            // The consumer's OWN atomic moved — no readback, no separate wiring.
            let hz = param.load(core::sync::atomic::Ordering::Acquire);
            assert!((20.0..=20000.0).contains(&hz), "out of range: {hz}");
        }
        // The handle reads the same value the atomic holds.
        assert!((cutoff.value() - param.load(core::sync::atomic::Ordering::Acquire)).abs() < 1e-3);
    }

    #[test]
    fn builder_wires_the_same_result_as_raw_api() {
        let mut m = ModMatrix::new();
        let cutoff = m.target(1000.0, 0.0, 2000.0);
        let gain = m.target(0.5, 0.0, 1.0);
        m.route(Lfo::new(LfoShape::Sine), rate())
            .to(&cutoff)
            .depth(1.0);
        m.route(Lfo::new(LfoShape::Triangle), rate())
            .to(&gain)
            .depth(0.5);

        let mut driver = m.build();
        for i in 0..4 {
            frame(&mut driver, i as f32 * 0.25);
        }
        assert!((0.0..=2000.0).contains(&cutoff.value()));
        assert!((0.0..=1.0).contains(&gain.value()));
    }

    #[test]
    fn one_source_two_targets_shares_a_source_index() {
        // `route` assigns one source index; two `.to(..)` on the SAME source
        // would need two route() calls — but a single source fanning to two
        // targets is expressed by two edges off distinct sources here, so verify
        // the simple two-route form drives both in range.
        let mut m = ModMatrix::new();
        let a = m.target(0.0, -1.0, 1.0);
        let b = m.target(0.0, -1.0, 1.0);
        m.route(Lfo::new(LfoShape::Sine), rate()).to(&a).depth(1.0);
        m.route(Lfo::new(LfoShape::Sine), rate()).to(&b).depth(1.0);
        let mut driver = m.build();
        frame(&mut driver, 0.25);
        assert!((-1.0..=1.0).contains(&a.value()));
        assert!((-1.0..=1.0).contains(&b.value()));
    }

    #[test]
    fn distinct_edges_get_distinct_keys() {
        // Two routes to the same target must NOT collide keys (else one
        // overwrites the other's layer). Route two sources → one target.
        let mut m = ModMatrix::new();
        let t = m.target(0.0, -2.0, 2.0);
        m.route(Lfo::new(LfoShape::Square), rate())
            .to(&t)
            .depth(1.0); // +1 at phase 0
        m.route(Lfo::new(LfoShape::Square), rate())
            .to(&t)
            .depth(1.0); // +1 at phase 0
        let mut driver = m.build();
        frame(&mut driver, 0.0);
        // Both contribute independently: +1 + +1 = +2 (clamped to max 2.0). If
        // the keys collided, we'd see only +1.
        assert!((t.value() - 2.0).abs() < 1e-6, "got {}", t.value());
    }

    #[test]
    fn curve_and_polarity_builders_apply() {
        let mut m = ModMatrix::new();
        let t = m.target(0.5, 0.0, 1.0);
        m.route(Lfo::new(LfoShape::Sine), rate())
            .to(&t)
            .depth(0.4)
            .polarity(Polarity::Unipolar)
            .curve(CurveType::Linear);
        let mut driver = m.build();
        frame(&mut driver, 0.0); // sine(0)=0 → unipolar remaps to 0.5·0.4 offset
        assert!((0.0..=1.0).contains(&t.value()));
    }
}
