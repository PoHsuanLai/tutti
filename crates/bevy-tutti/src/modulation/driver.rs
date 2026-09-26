//! The built matrix and the two systems that maintain it.
//!
//! [`rebuild`] compiles the ECS declaration into a routing table when it
//! changes; [`drive`] runs the driver once per frame. The split is the point:
//! rebuilding resets a source's phase, so it must not happen on the frame a
//! depth slider moves.

use bevy_ecs::prelude::*;
use std::collections::HashMap;
use std::sync::Arc;

use tutti_mod::{ModEdge, ModPreFrame, ModRoutingTable, ModTarget, ModTargetId};
use tutti_types::{ParamAddr, Samples, Seconds};

use crate::graph::TransportRes;
use crate::modulation::components::{ModDelivery, ModParamRange, ModRoute};
use crate::modulation::source::CollectedModSources;
use crate::modulation::target::ModTargetResolver;

/// The address of one modulated parameter: which entity, which param.
///
/// The key the claim set is looked up by. A param reconciler asks
/// "is `(entity, param)` modulated?" and skips its own write if so — the
/// modulation driver is the single writer of any param it owns.
pub type ParamKey = (Entity, ParamAddr);

/// The live modulation matrix, compiled from the ECS declaration.
///
/// Holds the driver, the routing table it publishes through, and the target
/// registry. `targets` is the **claim set**: a param present here is written by
/// [`drive`] every frame, so a plain reconciler writing the same atomic would
/// fight it. Ordering alone cannot settle that — whoever runs last wins, and
/// "last" is a scheduling accident — so the lookup is the mechanism.
#[derive(Resource, Default)]
pub struct ModulationMatrix {
    driver: Option<ModPreFrame>,
    table: ModRoutingTable,
    /// Every param the driver owns, and the accumulator behind it.
    targets: HashMap<ParamKey, Arc<dyn ModTarget>>,
    /// `targets` keyed by routing address, for edge construction.
    ids: HashMap<ParamKey, ModTargetId>,
}

impl ModulationMatrix {
    /// Whether the modulation driver owns this param.
    ///
    /// A param reconciler must consult this before writing: if it returns true,
    /// the authored value belongs on the accumulator's *base*
    /// (`set_base`), not written straight to the node atomic,
    /// or the next frame's modulation flush will overwrite it.
    pub fn is_modulated(&self, entity: Entity, param: ParamAddr) -> bool {
        self.targets.contains_key(&(entity, param))
    }

    /// The accumulator behind a modulated param, if the driver owns it.
    pub fn target(&self, entity: Entity, param: ParamAddr) -> Option<&Arc<dyn ModTarget>> {
        self.targets.get(&(entity, param))
    }

    /// Update a modulated param's authored base.
    ///
    /// The write a param reconciler makes *instead* of touching the node
    /// directly. Modulation is `base + Σ offsets`, so an authored change lands
    /// here and survives the next flush rather than being clobbered by it.
    /// Returns false if the param is not modulated, in which case the caller
    /// owns the write.
    ///
    /// Crate-internal on purpose: this is only *half* a write. A caller that
    /// stops here silently drops every write to an unmodulated param — the
    /// control works until someone deletes its LFO. Owning both halves is what
    /// [`AudioParam`](crate::graph::AudioParam) is for, and inserting one is
    /// the public way to author a param value.
    pub(crate) fn set_base(&self, entity: Entity, param: ParamAddr, base: f32) -> bool {
        match self.targets.get(&(entity, param)) {
            Some(target) => {
                target.set_base(base);
                true
            }
            None => false,
        }
    }

    /// How many params the driver currently owns.
    pub fn len(&self) -> usize {
        self.targets.len()
    }

    /// Whether the driver owns no params — so a `drive` pass would write
    /// nothing.
    pub fn is_empty(&self) -> bool {
        self.targets.is_empty()
    }
}

/// Recompile the routing table from the ECS declaration.
///
/// Runs only when the modulation *shape* changed — a source, a rate, a route,
/// or a route despawning. Deliberately **not** on an authored base change:
/// rebuilding mints new source objects, and a fresh `Sourced` starts at phase
/// zero, so rebuilding on a slider drag would restart every LFO in the graph
/// sixty times a second.
///
/// # A rebuild re-seeds every base from `ModParamRange`
///
/// Worth stating because it decides who owns a param's base. Resolving a target
/// calls `ModParams::mod_target(param, range.base, ..)`, and every impl —
/// native and plugin alike — *constructs* an accumulator seeded with that
/// `base`. Since `rebuild` replaces `matrix.targets` wholesale, a base
/// previously set through
/// [`write_param`](crate::graph::write_param) is discarded and reset to
/// whatever `ModParamRange` last declared.
///
/// Measured, not inferred: with a document base of 5.0, a `write_param` write
/// of 8.0, and a `+1.0` modulator, the param reads 9.0 — then 6.0 after a
/// rebuild triggered by a *route* change that never touched the base.
///
/// **For a control-rate param, `ModParamRange` is therefore the authority on
/// the base, and `write_param` is only authoritative between rebuilds.** In
/// a typical host that is invisible, because its one producer of `ModParamRange`
/// reads the same authored document the `write_param` call sites do, so the two
/// always agree.
/// It stops being invisible the moment a base can move without the document
/// moving — a MIDI-learn ride, a plugin writing its own param back, an
/// automation lane evaluated outside the document. Any such writer must reach
/// `ModParamRange`, not just the accumulator.
///
/// **This does not hold for an audio-rate (`PerSample`) param**, and the
/// difference is worth knowing before generalising the rule. Such a param is
/// not in `matrix.targets` at all (see the skip below), so nothing here
/// reconstructs it: its base lives in the chain's own `Arc<AtomicF32>`, which
/// survives every rebuild because `reconcile_audio_rate` preserves a chain of
/// the right shape. There the ownership is the other way round — the cell is
/// authoritative, and `ModParamRange` edits are folded *into* it by
/// `ParamChain::refresh_base`.
#[allow(clippy::type_complexity)]
pub fn rebuild(
    mut matrix: ResMut<ModulationMatrix>,
    resolver: ModTargetResolver,
    mut collected: ResMut<CollectedModSources>,
    routes: Query<&ModRoute>,
    ranges: Query<&ModParamRange>,
    // A re-captured `ModParamsHandle` (a crossfade) is not listed here: it
    // raises `collected.dirty` in `source::mark_dirty_on_route_change`, which
    // must see it anyway so the rebuild has sources to bind.
    changed: Query<Entity, Or<(Changed<ModRoute>, Changed<ModParamRange>)>>,
    mut removed: RemovedComponents<ModRoute>,
    // The re-seeded base also goes to the node's fork snapshot (below). Both
    // optional: a host can run modulation over sinks that are not graph nodes.
    mut graph: Option<ResMut<crate::graph::AudioGraphRes>>,
    graph_nodes: Query<&tutti_core::AudioNode>,
) {
    // Source changes arrive as `collected.dirty` rather than a `Changed<K>`
    // filter: a kind's component type cannot be named here, so each kind
    // reports its own movement from its collector.
    let dirty = !changed.is_empty() || !removed.is_empty() || collected.dirty;
    // `removed` is an event reader: draining it is what marks the frame's
    // removals as seen, so it happens whether or not a rebuild follows.
    removed.clear();
    if !dirty {
        return;
    }
    // Cleared here rather than at the top of the frame: a change that arrives
    // while the engine is not ready keeps the flag raised until a rebuild
    // actually consumes it.
    collected.dirty = false;

    // Source registry order fixes the indices `ModEdge::source` refers to, so
    // it is built once and consulted by every edge below. Which kinds exist is
    // the registry's business — nothing here names a modulator type.
    let mut registry: Vec<Box<dyn tutti_mod::ErasedModulator>> = Vec::new();
    let mut source_index: HashMap<Entity, usize> = HashMap::new();
    for (entity, source) in collected.sources.drain(..) {
        source_index.insert(entity, registry.len());
        registry.push(source);
    }

    let mut targets: HashMap<ParamKey, Arc<dyn ModTarget>> = HashMap::new();
    let mut ids: HashMap<ParamKey, ModTargetId> = HashMap::new();
    let mut edges: Vec<ModEdge> = Vec::new();
    let bus = resolver.bus();

    for (i, route) in routes.iter().enumerate() {
        if !route.enabled {
            continue;
        }
        // A per-sample route is delivered as a graph chain by
        // `modulation::audio_rate`, which feeds the sink's param port directly.
        // Giving it an edge here as well would put the driver on the node's
        // atomic while the sum drives its port — two writers over one param,
        // which is the exact hazard the claim set below exists to prevent.
        if route.delivery == ModDelivery::PerSample {
            continue;
        }
        let Some(&source) = source_index.get(&route.source) else {
            continue;
        };
        let key = (route.target, route.param);

        // One accumulator per param, however many routes drive it: two sources
        // on one cutoff must sum into a single value, not race two of them.
        let (id, min, max) = match ids.get(&key) {
            Some(&id) => {
                let (min, max) = targets[&key].range();
                (id, min, max)
            }
            None => {
                let Ok(range) = ranges.get(route.target) else {
                    continue;
                };
                let Some(range) = range.get(route.param) else {
                    continue;
                };
                let Some(target) = resolver.resolve(route.target, route.param, range) else {
                    continue;
                };
                // The resolve re-seeds the live base from `range.base` (the
                // target mirrors it into the node's cell as it is built). A
                // fork of the node — an export — starts from its shadow, which
                // that cell write does not reach, so the base goes there too,
                // as `write_param` sends an authored one.
                if let (ParamAddr::Unit(param), Some(graph), Ok(node)) =
                    (route.param, graph.as_mut(), graph_nodes.get(route.target))
                {
                    graph.set_param_snapshot(*node, param, range.base);
                }
                let id = ModTargetId::next();
                bus.insert(id, Arc::clone(&target));
                targets.insert(key, target);
                ids.insert(key, id);
                (id, range.min, range.max)
            }
        };

        // Each route owns a distinct layer, so two routes onto one param
        // accumulate instead of overwriting each other. Offset past
        // `AUTOMATION`, which owns layer zero.
        let layer = tutti_mod::LayerKey(i as u64 + 1);

        // A curve-delivered route installs its layer *once, here*, and the sink
        // evaluates it from then on — so it gets no `ModEdge` and the per-frame
        // driver never touches it. Both halves have to agree: an edge as well
        // would write a scalar over the curve every frame.
        if route.delivery == ModDelivery::PerBlock {
            let installed = collected
                .curves
                .get(&route.source)
                .and_then(|build| {
                    build(tutti_mod::EdgeShape {
                        depth: route.depth,
                        polarity: route.polarity,
                        curve: route.curve,
                        phase_offset: tutti_types::PhaseIncrement(0.0),
                        min,
                        max,
                    })
                })
                .is_some_and(|curve| targets[&key].accumulate_curve(layer, curve));
            if installed {
                continue;
            }
            // Fell through: either the kind has no curve form or the sink takes
            // only scalars. Scalar delivery is always correct — a staircase
            // rather than a ramp — so the route still sounds, just coarser.
        }

        edges.push(ModEdge {
            source,
            target: id,
            key: layer,
            depth: route.depth,
            min,
            max,
            polarity: route.polarity,
            curve: route.curve,
            enabled: true,
        });
    }

    let source_count = registry.len();
    matrix.table.set_edges(edges, source_count);
    matrix.table.commit();

    // Reuse the existing driver, replacing only its sources. The driver clears
    // a retired edge's layer by diffing this frame's `(target, key)` set against
    // the one it wrote last frame — memory that lives *in the driver*. A fresh
    // `ModPreFrame` starts with that set empty, so it cannot know a layer needs
    // clearing, and a deleted route's last offset stays stuck on the param
    // forever.
    let snapshot = matrix.table.snapshot_arc();
    let driver = matrix
        .driver
        .get_or_insert_with(|| ModPreFrame::new(snapshot));
    driver.set_router(bus);
    driver.set_sources(registry);

    matrix.targets = targets;
    matrix.ids = ids;
}

/// Advance every modulation source one frame and flush into the target atomics.
///
/// `dt` comes from the transport's steady sample count rather than frame time:
/// it is written by the audio clock, so a free-running LFO advances with the
/// audio it modulates instead of with the render rate, and a dropped frame does
/// not skip it forward. It is also immune to loops and seeks — which is what
/// "free-running" has to mean.
pub fn drive(
    mut matrix: ResMut<ModulationMatrix>,
    // `build_into`'s, not this plugin's; `engine_ready` does not cover it.
    transport: Option<Res<TransportRes>>,
    mut last_steady: Local<Option<i64>>,
) {
    let Some(transport) = transport else {
        return;
    };
    let Some(driver) = matrix.driver.as_mut() else {
        return;
    };

    let steady = transport.settings.steady_time();
    let elapsed = match *last_steady {
        // A backwards jump means the stream restarted; charge no time for it
        // rather than winding every free-running source backwards. The guard
        // is also what makes the cast below safe: the delta cannot be negative
        // on the branch that takes it.
        Some(prev) if steady >= prev => {
            Samples((steady - prev) as usize).to_seconds(transport.sample_rate())
        }
        _ => Seconds(0.0),
    };
    *last_steady = Some(steady);

    driver.run(transport.settings.beat(), elapsed);
}

#[cfg(test)]
mod tests {
    //! `set_base` is crate-internal, so its tests live here rather than in
    //! `tests/`. They still build a real `App` and assert on the node's own
    //! atomic — the visibility changed, not the rigor.

    use super::*;
    use bevy_app::prelude::*;

    use crate::graph::{AudioGraphRes, CapturedControls, GraphReconcilePlugin, TransportRes};
    use crate::modulation::{
        LfoShape, ModParamRange, ModRoute, ModSource, ModSourceRate, ModTargetRegistry,
        TuttiModulationPlugin,
    };
    use crate::AudioEngineState;
    use tutti_core::transport::Transport;
    use tutti_core::AudioNode;
    use tutti_nodes::DistortionNode;
    use tutti_types::{Depth, Hz, UnitParam};

    const BASE_DRIVE: f32 = 5.0;

    fn app_with_graph() -> (App, Entity) {
        let mut app = App::new();

        // `headless`, which has a backend to commit into: this app runs the
        // full reconcile pipeline, and `commit_graph` commits.
        app.insert_resource(AudioGraphRes::headless(0, 1));
        app.insert_resource(TransportRes(Transport::new(48_000.0)));
        app.insert_resource(AudioEngineState::Running);
        app.add_plugins((GraphReconcilePlugin, TuttiModulationPlugin));
        app.world_mut()
            .resource_mut::<ModTargetRegistry>()
            .register::<DistortionNode>();

        // Registered first, then captured and pushed: the capture is what makes
        // the node modulatable, and it only runs on the unit before insertion.
        let unit = DistortionNode::new(tutti_nodes::ShapeKind::Tanh, 1.0);
        let drive = DriveCell(unit.drive());
        let controls = CapturedControls::capture(app.world(), &unit);
        let node = {
            let mut graph = app.world_mut().resource_mut::<AudioGraphRes>();
            let node = graph.insert(unit);
            graph.set_outputs_from(node);
            node
        };
        let mut entity = app.world_mut().spawn(drive);
        controls.bind(&mut entity, node);
        let target = entity.id();
        app.world_mut()
            .entity_mut(target)
            .insert(ModParamRange::default().with(
                ParamAddr::Unit(UnitParam::Drive),
                BASE_DRIVE,
                0.0,
                10.0,
            ));
        (app, target)
    }

    /// The node's own drive atomic, taken from the unit before it moved into
    /// the graph. Shared with every clone of the node, so it reads what the DSP
    /// reads.
    #[derive(Component)]
    struct DriveCell(std::sync::Arc<tutti_core::AtomicF32>);

    fn node_drive(app: &App, entity: Entity) -> f32 {
        app.world()
            .get::<DriveCell>(entity)
            .unwrap()
            .0
            .load(core::sync::atomic::Ordering::Acquire)
    }

    fn advance_transport(app: &mut App, samples: i64) {
        let transport = app.world().resource::<TransportRes>().clone();
        let current = transport.settings.steady_time();
        transport
            .settings
            .steady_time
            .store(current + samples, core::sync::atomic::Ordering::Relaxed);
    }

    #[test]
    fn set_base_moves_a_modulated_param_without_fighting_the_driver() {
        // The single-writer rule in practice: an authored change lands on the
        // accumulator's base, so the next flush carries it rather than
        // reverting it. Writing the node atomic directly loses it in a frame.
        let (mut app, target) = app_with_graph();
        let lfo = app
            .world_mut()
            // A square at zero rate holds a constant offset rather than
            // sweeping — the base shift stays legible against it.
            .spawn((
                ModSource::new(LfoShape::Square),
                ModSourceRate::free_running(Hz(0.0)),
            ))
            .id();
        app.world_mut().spawn(
            ModRoute::new(lfo, target, ParamAddr::Unit(UnitParam::Drive)).with_depth(Depth(0.1)),
        );

        app.update();
        let before = node_drive(&app, target);

        let matrix = app.world().resource::<ModulationMatrix>();
        assert!(matrix.set_base(target, ParamAddr::Unit(UnitParam::Drive), 8.0));

        advance_transport(&mut app, 480);
        app.update();
        let after = node_drive(&app, target);

        assert!(
            (after - before - 3.0).abs() < 0.2,
            "base moved 5 -> 8, so the value should follow: {before} -> {after}"
        );
    }

    #[test]
    fn set_base_declines_a_param_it_does_not_own() {
        let (mut app, target) = app_with_graph();
        app.update();

        let matrix = app.world().resource::<ModulationMatrix>();
        assert!(
            !matrix.set_base(target, ParamAddr::Unit(UnitParam::Drive), 8.0),
            "nothing routes here, so the caller owns the write"
        );
    }

    /// An authored write through [`write_param`](crate::graph::write_param)
    /// survives on a **modulated** param — the branch that routes it to the
    /// accumulator's base rather than the node's atomic.
    ///
    /// This is the guard that was missing. `set_base_moves_a_modulated_param…`
    /// above calls `set_base` directly, so it passes even when `write_param`
    /// stops calling it; this one goes through the real front door.
    ///
    /// **The loss it catches is deterministic, not a race.** Measured by
    /// neutering the branch: 200/200 trials lost the write, 0/200 with it
    /// restored. `drive` re-mirrors `clamp(base + Σ layers)` into the node
    /// atomic every frame, so a direct write there is overwritten by the next
    /// flush regardless of system order — the two writers touch different
    /// fields of a mutex-guarded `LayeredCurve` and never contend.
    #[test]
    fn an_authored_write_through_write_param_survives_modulation() {
        let (mut app, target) = app_with_graph();
        let lfo = app
            .world_mut()
            // Zero-rate square: a constant offset, so the base shift stays
            // legible against it rather than sweeping.
            .spawn((
                ModSource::new(LfoShape::Square),
                ModSourceRate::free_running(Hz(0.0)),
            ))
            .id();
        app.world_mut().spawn(
            ModRoute::new(lfo, target, ParamAddr::Unit(UnitParam::Drive)).with_depth(Depth(0.1)),
        );
        app.update();

        let node = *app.world().get::<AudioNode>(target).unwrap();
        app.world_mut()
            .resource_scope(|w, mut graph: Mut<AudioGraphRes>| {
                let matrix = w.resource::<ModulationMatrix>();
                crate::graph::write_param(&mut graph, matrix, target, &node, UnitParam::Drive, 8.0);
            });

        advance_transport(&mut app, 480);
        app.update();

        let after = node_drive(&app, target);
        assert!(
            after > 7.0,
            "base moved {BASE_DRIVE} -> 8 through write_param, so the flushed \
             value should carry it; got {after}. A value near {BASE_DRIVE} means \
             the write went to the node atomic and the driver overwrote it."
        );
    }

    /// **A rebuild re-seeds the base from `ModParamRange`, discarding a
    /// `write_param` base.**
    ///
    /// Pins the ownership rule `rebuild`'s doc states. Not a bug report: it is
    /// benign in this workspace, because the only producer of `ModParamRange`
    /// reads the same authored document the `write_param` call sites do. It is
    /// pinned so that stops being an accident — a future writer that can move a
    /// base *without* the document moving (MIDI learn, a plugin writing back)
    /// changes this test, which is the signal to route that writer through
    /// `ModParamRange` too.
    #[test]
    fn a_rebuild_reseeds_the_base_from_the_declared_range() {
        let (mut app, target) = app_with_graph();
        let lfo = app
            .world_mut()
            .spawn((
                ModSource::new(LfoShape::Square),
                ModSourceRate::free_running(Hz(0.0)),
            ))
            .id();
        app.world_mut().spawn(
            ModRoute::new(lfo, target, ParamAddr::Unit(UnitParam::Drive)).with_depth(Depth(0.1)),
        );
        app.update();

        // An authored write the declared range does not know about.
        let node = *app.world().get::<AudioNode>(target).unwrap();
        app.world_mut()
            .resource_scope(|w, mut graph: Mut<AudioGraphRes>| {
                let matrix = w.resource::<ModulationMatrix>();
                crate::graph::write_param(&mut graph, matrix, target, &node, UnitParam::Drive, 8.0);
            });
        advance_transport(&mut app, 480);
        app.update();

        let carried = node_drive(&app, target);
        assert!(
            carried > 7.0,
            "between rebuilds the write_param base is authoritative; got {carried}"
        );

        // Force a rebuild without touching the base: mark the route `Changed`
        // by rewriting it with the value it already has.
        {
            let mut routes = app.world_mut().query::<&mut ModRoute>();
            let mut route = routes.single_mut(app.world_mut()).unwrap();
            route.depth = Depth(0.1);
        }
        advance_transport(&mut app, 480);
        app.update();
        // A second frame, so the driver re-applies its offset over the
        // freshly-seeded base rather than being read mid-flush.
        advance_transport(&mut app, 480);
        app.update();

        let reseeded = node_drive(&app, target);
        assert!(
            (reseeded - (BASE_DRIVE + 1.0)).abs() < 0.2,
            "the rebuild must re-seed from ModParamRange's base ({BASE_DRIVE}) \
             plus the modulator's +1.0 offset, discarding the 8.0 written \
             through write_param; got {reseeded} (was {carried} before)"
        );
    }
}
