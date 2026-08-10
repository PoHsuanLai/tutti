//! The source registry: which modulator kinds this app can build.
//!
//! The mirror of [`ModTargetRegistry`](super::ModTargetRegistry), for the send
//! half. A target is registered by *node type* and resolved by downcast; a
//! source is registered by **component**, and its builder is a plain
//! constructor — [`Sourced<M>`](tutti_mod::Sourced) erases `M` at construction,
//! so nothing downstream ever recovers the concrete type.
//!
//! ```rust
//! use bevy_app::prelude::*;
//! use bevy_tutti::modulation::{ModSource, ModSourceAppExt, TuttiModulationPlugin};
//!
//! let mut app = App::new();
//! // `TuttiModulationPlugin` registers the built-in `ModSource` kind already;
//! // a host adds its own the same way, and the call is idempotent.
//! app.add_plugins(TuttiModulationPlugin);
//! app.add_mod_source::<ModSource>();
//! ```
//!
//! # Why a component, not a kind enum
//!
//! Different modulators want different parameters — an LFO a shape, a step
//! sequencer a pattern, an envelope follower attack/release. A single
//! `ModSource { kind, .. }` would have to carry the union of every kind's
//! config and leave most fields meaningless for any given one. A component per
//! kind keeps each modulator's parameters typed, individually
//! change-detectable, and reflectable — the same reasoning that already keeps
//! [`ModRate`] off `ModSource`.
//!
//! [`ModRate`] stays shared: *how a source derives phase from the transport* is
//! a property of being a source at all, not of which kind it is.

use bevy_app::App;
use bevy_ecs::prelude::*;

use std::collections::HashMap;
use std::sync::Arc;

use tutti_mod::{Curve, EdgeShape, ErasedModulator, Modulator, SourceRate};
use tutti_types::{BeatDuration, Hz, Param, ParamAddr, UnitParam};

use crate::modulation::components::{ModClock, ModRate, ModRoute};

/// A component that describes how to build one kind of modulator.
///
/// Implement it on the component carrying that modulator's parameters; the
/// component *is* the authored declaration, and [`build`](Self::build) turns it
/// into the engine object.
/// `Clone + Send + Sync + 'static` so a kind's authored parameters can be moved
/// into the curve builder the collector hands to `rebuild` — the builder
/// outlives the query borrow it was read through, and only the collector can
/// name `K`.
pub trait ModSourceKind: Component + Clone + Send + Sync + Sized + 'static {
    /// The modulator this component builds.
    type Source: Modulator + Send + Sync + 'static;

    /// Construct the modulator from the authored parameters.
    ///
    /// Rate is deliberately absent: it arrives from the entity's [`ModRate`]
    /// and is applied by the caller, so a kind cannot accidentally own two
    /// notions of frequency.
    fn build(&self) -> Self::Source;

    /// This kind as a beat-evaluated [`Curve`], if it has such a form.
    ///
    /// A curve is installed once and sampled by the *sink* at whatever rate it
    /// reads — a plugin's per-block producer traces a smooth ramp where a
    /// frame-rate scalar gives a staircase. `edge` carries the shaping the
    /// scalar path would otherwise apply per frame, so both deliveries agree on
    /// the value.
    ///
    /// `beats_per_cycle` comes from the entity's [`ModRate`], converted by the
    /// caller — the same reasoning that keeps rate off [`build`](Self::build).
    ///
    /// `None` by default, which is the honest answer for any modulator needing
    /// more than its position: a `Curve` has nowhere to thread state. Note that
    /// "stateful" is not the same as "not position-derivable" — an LFO's stepped
    /// shapes qualify once re-keyed on the cycle index (see
    /// [`BeatLfo`](tutti_mod::BeatLfo)).
    ///
    /// Returning `Some` is not a promise of curve delivery: the *sink* decides,
    /// and one that takes only scalars makes the route fall back.
    fn build_curve(
        &self,
        beats_per_cycle: BeatDuration,
        edge: EdgeShape,
    ) -> Option<Arc<dyn Curve>> {
        let _ = (beats_per_cycle, edge);
        None
    }
}

/// Every source declared this frame, and whether any of them changed.
///
/// Filled by one `collect::<K>` system per registered kind, then read by
/// `rebuild`. The indirection is what keeps `rebuild` a normal system: a kind's
/// component type cannot appear in `rebuild`'s signature — that is the whole
/// point of the registry — but it can appear in a system of the kind's own,
/// scheduled by `add_mod_source`.
/// Builds this source's curve form for one edge's shaping, or `None` if the
/// kind has no such form.
///
/// A boxed closure because only `collect::<K>` can name `K` — the same reason
/// the modulators arrive erased. `rebuild` calls it per route without ever
/// learning which kind it came from.
pub(crate) type CurveBuilder = Box<dyn Fn(EdgeShape) -> Option<Arc<dyn Curve>> + Send + Sync>;

/// What the registered source kinds built this frame, for [`rebuild`] to drain.
///
/// A staging buffer, not state: [`rebuild`] consumes it, and
/// `clear_collected` drops anything a frame built but no rebuild took. It
/// exists so `rebuild` can recompile without naming a kind type — each kind's
/// own collector pushes here.
///
/// [`rebuild`]: super::rebuild
#[derive(Resource, Default)]
pub struct CollectedModSources {
    pub(crate) sources: Vec<(Entity, Box<dyn ErasedModulator>)>,
    /// One per source that has a curve form, keyed by entity. Absent for a kind
    /// that declined, which is what makes a curve request fall back.
    pub(crate) curves: HashMap<Entity, CurveBuilder>,
    /// Set when any kind component or its rate changed, so `rebuild` knows to
    /// recompile without naming a kind type.
    pub(crate) dirty: bool,
}

impl CollectedModSources {
    /// How many sources the registered kinds built this frame.
    ///
    /// One per source entity. A count higher than the number of source
    /// entities means a kind was registered twice, or an entity carries two
    /// kind components — both cases where a route would bind to only one of
    /// the duplicates.
    pub fn len(&self) -> usize {
        self.sources.len()
    }

    /// Whether no kind built a source this frame.
    pub fn is_empty(&self) -> bool {
        self.sources.is_empty()
    }
}

/// Drop any sources a previous frame built but no rebuild consumed.
///
/// Runs ahead of [`mark_dirty`]. It deliberately does **not** clear `dirty`:
/// that flag is cleared by `rebuild` once it has actually recompiled, so a
/// frame where the engine is not yet ready leaves the pending change standing
/// rather than losing it.
pub(crate) fn clear_collected(mut collected: ResMut<CollectedModSources>) {
    collected.sources.clear();
    collected.curves.clear();
}

/// Give every source a route points at the cell its rate will be read from.
///
/// Runs ahead of [`mark_dirty`], and that ordering is the whole reason it is a
/// system of its own. The cell must exist *before* `collect` builds the
/// [`Sourced`](tutti_mod::Sourced) that reads it, but only a route declares
/// that a rate is modulated — and routes are resolved by
/// [`rebuild`](super::rebuild), which runs after both. Adding the cell here
/// closes that gap: the route is seen one frame, the cell exists from that
/// frame on, and every later rebuild finds it already in place.
///
/// Adding the component marks the entity changed, so `mark_dirty` sees it in
/// the same frame and the rebuild that follows picks it up — no extra dirty
/// signal, and no frame where a modulated rate is silently still constant.
pub(crate) fn ensure_rate_cells(
    mut commands: Commands,
    routes: Query<&ModRoute>,
    sources: Query<(&ModRate, Option<&ModRateCell>)>,
) {
    for route in &routes {
        // Only a route onto a *rate* needs one. Every other param on a source
        // entity — or any route onto an ordinary node — is unaffected.
        if route.param != ParamAddr::Unit(UnitParam::Rate) {
            continue;
        }
        let Ok((rate, existing)) = sources.get(route.target) else {
            continue;
        };
        if existing.is_some() {
            continue;
        }
        // Only a free-running rate is modulatable: the cell is a `Param<Hz>`,
        // which is f32, and a synced span needs f64 beat precision. Modulating
        // the divisor would also smear the transport lock that arm exists to
        // provide, so a route onto a synced source's rate is ignored rather
        // than silently reinterpreted.
        let ModClock::Free { hz } = rate.clock else {
            continue;
        };
        // Seed with the authored frequency so the first frame after wiring is
        // continuous — the source keeps running at the rate it already had
        // until an accumulator actually moves the cell.
        commands.entity(route.target).insert(ModRateCell::new(hz));
    }
}

/// Report whether kind `K`'s declaration moved this frame.
///
/// Split from `collect` and scheduled ahead of it because **building a source
/// is not free of consequence**: a fresh [`Sourced`](tutti_mod::Sourced) starts
/// at phase zero, so constructing one per frame would restart every modulator
/// sixty times a second. The build must happen only when a rebuild will
/// actually consume it, which means the dirty answer has to exist first.
#[allow(
    clippy::type_complexity,
    reason = "Bevy queries are tuple-shaped by design"
)]
fn mark_dirty<K: ModSourceKind>(
    mut collected: ResMut<CollectedModSources>,
    changed: Query<Entity, Or<(Changed<K>, Changed<ModRate>)>>,
    mut removed: RemovedComponents<K>,
) {
    // Draining the reader marks this frame's removals as seen either way.
    let any_removed = removed.read().next().is_some();
    for _ in removed.read() {}
    if !changed.is_empty() || any_removed {
        collected.dirty = true;
    }
}

/// Build every source declared by kind `K`, but only into a pending rebuild.
///
/// One instance per registered kind. `K` appears only here and in
/// [`mark_dirty`] — systems the kind's own registration scheduled — which is
/// how `rebuild` stays free of every modulator type.
fn collect<K: ModSourceKind>(
    mut collected: ResMut<CollectedModSources>,
    sources: Query<(Entity, &K, &ModRate, Option<&ModRateCell>)>,
) {
    if !collected.dirty {
        return;
    }
    for (entity, kind, rate, cell) in &sources {
        let source: Box<dyn ErasedModulator> = Box::new(tutti_mod::Sourced::new(
            kind.build(),
            source_rate(rate, cell),
        ));
        collected.sources.push((entity, source));

        // A curve is clocked by the beat, so a synced span is already in the
        // curve's own units and passes through unchanged — no reciprocal. The
        // arm carries a `BeatDuration`, which is what leaves no second reading
        // (cycles-per-beat) available to pick wrong.
        //
        // A free-running rate is in Hz and has no fixed beat mapping, so it has
        // no curve form — the scalar path stays correct for it.
        if let ModClock::Synced { beats_per_cycle } = rate.clock {
            if beats_per_cycle.get() > 0.0 {
                // Cloned into the closure: the builder outlives this query
                // borrow, and `rebuild` calls it once per route on the source.
                let kind = kind.clone();
                collected.curves.insert(
                    entity,
                    Box::new(move |edge| kind.build_curve(beats_per_cycle, edge)),
                );
            }
        }
    }
}

/// Registers a modulator kind.
pub trait ModSourceAppExt {
    /// Let source entities carrying `K` be built into modulators.
    ///
    /// Idempotent: registering a kind twice schedules one collector. Two would
    /// each push a source for the same entity, and the routing table would bind
    /// every route to only the first of them.
    fn add_mod_source<K: ModSourceKind>(&mut self) -> &mut Self;
}

impl ModSourceAppExt for App {
    fn add_mod_source<K: ModSourceKind>(&mut self) -> &mut Self {
        if !self
            .world_mut()
            .get_resource_or_init::<RegisteredModSources>()
            .0
            .insert(core::any::TypeId::of::<K>())
        {
            return self;
        }
        self.add_systems(
            bevy_app::Update,
            (
                mark_dirty::<K>.in_set(ModSourceSystems::MarkDirty),
                collect::<K>.in_set(ModSourceSystems::Collect),
            ),
        )
    }
}

/// Raise the collect flag when a **route or range** moves, not only a source.
///
/// `rebuild` runs on `Or<(Changed<ModRoute>, Changed<ModParamRange>)>` *or* a
/// dirty source, and it builds its source registry by **draining**
/// `CollectedModSources::sources`. But `collect` refills that list only when
/// `dirty` is set, and `dirty` tracked source changes alone.
///
/// So a rebuild triggered by a route or range change found an empty registry,
/// failed to resolve `source_index` for every route, and **dropped every
/// accumulator**. The user-visible effect was that editing a modulated
/// parameter's authored value silently deleted its modulation — nothing errored,
/// the matrix just emptied.
///
/// Non-generic and registered once, unlike [`mark_dirty`]: a route is not
/// per-kind, and duplicating this into every kind's registration would raise the
/// same flag N times.
///
/// The phase-restart cost [`mark_dirty`] guards against still applies — a fresh
/// `Sourced` starts at phase zero — but a rebuild was *already* going to happen
/// on these frames. The choice is between rebuilding with sources and
/// rebuilding without them, and only one of those keeps the routes.
pub(crate) fn mark_dirty_on_route_change(
    mut collected: ResMut<CollectedModSources>,
    changed: Query<
        Entity,
        Or<(
            Changed<ModRoute>,
            Changed<crate::modulation::components::ModParamRange>,
        )>,
    >,
    mut removed: RemovedComponents<ModRoute>,
) {
    let any_removed = removed.read().next().is_some();
    for _ in removed.read() {}
    if !changed.is_empty() || any_removed {
        collected.dirty = true;
    }
}

/// The two per-kind phases, ordered ahead of the rebuild that consumes them.
///
/// Separate sets because the second is conditional on the first: `MarkDirty`
/// answers "did anything move?", and only then does `Collect` pay to build.
#[derive(SystemSet, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ModSourceSystems {
    /// Every registered kind reports whether its declaration changed.
    MarkDirty,
    /// Every registered kind builds its sources, if a rebuild is pending.
    Collect,
}

/// Which kinds are already registered, so `add_mod_source` can be idempotent.
#[derive(Resource, Default)]
struct RegisteredModSources(std::collections::HashSet<core::any::TypeId>);

/// The live cell a source's frequency is read from, when its rate is itself
/// modulated.
///
/// Present only on a source entity something routes *to* — added by
/// [`rebuild`](super::rebuild) when it resolves such a route, not by the user.
/// Its absence is the ordinary case and means the rate is the constant in
/// [`ModRate`].
///
/// It is a **component, not a build-time value**, and that is load-bearing:
/// `collect` reconstructs every [`Sourced`](tutti_mod::Sourced) on each
/// rebuild, so a cell minted there would be a fresh one each time and the
/// accumulator writing the *previous* cell would go unread. Living on the
/// entity, it outlives every rebuild and both halves keep pointing at one cell.
#[derive(Component, Debug, Clone)]
pub struct ModRateCell(pub(crate) Param<Hz>);

impl ModRateCell {
    /// A cell seeded with `frequency` — the rate the source ran at before
    /// anything modulated it, so the first frame after wiring is continuous.
    pub(crate) fn new(frequency: Hz) -> Self {
        Self(Param::new(frequency))
    }

    /// The shared atomic, for the accumulator that drives this rate.
    ///
    /// This must be the cell the [`Sourced`](tutti_mod::Sourced) reads — handing
    /// an accumulator any other atomic type-checks, runs, and modulates nothing.
    ///
    /// Public so a host can identify the cell (compare allocations, hand it to
    /// its own meter); it is a read handle, not a licence to write — the
    /// accumulator is the single writer.
    pub fn as_atomic(&self) -> std::sync::Arc<atomic_float::AtomicF32> {
        self.0.as_atomic()
    }

    /// The frequency the source is running at *now* — the authored rate until
    /// modulation moves it, and the modulated value thereafter.
    ///
    /// This is the live read a UI wants: `ModRate::frequency` is what the user
    /// authored and does not move.
    pub fn frequency(&self) -> Hz {
        self.0.load()
    }
}

/// Turn a [`ModRate`] component into the engine's [`SourceRate`].
///
/// `cell` is `Some` only when this source's own rate is modulated, in which
/// case the frequency is read from it each frame and the authored `Hz` serves
/// only as the value it was seeded with. Only the free-running arm has one —
/// see [`ensure_rate_cells`].
pub(crate) fn source_rate(rate: &ModRate, cell: Option<&ModRateCell>) -> SourceRate {
    match rate.clock {
        ModClock::Free { hz } => {
            let frequency: tutti_mod::Rate = match cell {
                Some(cell) => cell.0.clone().into(),
                None => hz.into(),
            };
            SourceRate::free_running(frequency, rate.phase_offset)
        }
        ModClock::Synced { beats_per_cycle } => {
            SourceRate::beat_synced(beats_per_cycle, rate.phase_offset)
        }
    }
}
