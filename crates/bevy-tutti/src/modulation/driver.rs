//! The built matrix and the two systems that maintain it.
//!
//! [`rebuild`] compiles the ECS declaration into a routing table when it
//! changes; [`drive`] runs the driver once per frame. The split is the point:
//! rebuilding resets a source's phase, so it must not happen on the frame a
//! depth slider moves.

use bevy_ecs::prelude::*;
use std::collections::HashMap;
use std::sync::Arc;

use tutti_mod::{Lfo, ModEdge, ModPreFrame, ModRoutingTable, ModTarget, ModTargetId, SourceRate};
use tutti_types::{ParamAddr, Seconds};

use crate::graph::TransportRes;
use crate::modulation::components::{ModParamRange, ModRate, ModRoute, ModSource};
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
    /// ([`set_base`](Self::set_base)), not written straight to the node atomic,
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
    pub fn set_base(&self, entity: Entity, param: ParamAddr, base: f32) -> bool {
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
#[allow(clippy::type_complexity)]
pub fn rebuild(
    mut matrix: ResMut<ModulationMatrix>,
    resolver: ModTargetResolver,
    sources: Query<(Entity, &ModSource, &ModRate)>,
    routes: Query<&ModRoute>,
    ranges: Query<&ModParamRange>,
    changed: Query<
        Entity,
        Or<(
            Changed<ModSource>,
            Changed<ModRate>,
            Changed<ModRoute>,
            Changed<ModParamRange>,
        )>,
    >,
    mut removed: RemovedComponents<ModRoute>,
) {
    let dirty = !changed.is_empty() || !removed.is_empty();
    // `removed` is an event reader: draining it is what marks the frame's
    // removals as seen, so it happens whether or not a rebuild follows.
    removed.clear();
    if !dirty {
        return;
    }

    // Source registry order fixes the indices `ModEdge::source` refers to, so
    // it is built once and consulted by every edge below.
    let mut registry: Vec<Box<dyn tutti_mod::ErasedModulator>> = Vec::new();
    let mut source_index: HashMap<Entity, usize> = HashMap::new();
    for (entity, source, rate) in &sources {
        source_index.insert(entity, registry.len());
        let rate = if rate.beat_synced {
            SourceRate::beat_synced(rate.frequency, rate.phase_offset)
        } else {
            SourceRate::free_running(rate.frequency, rate.phase_offset)
        };
        registry.push(Box::new(tutti_mod::Sourced::new(
            Lfo::new(source.shape),
            rate,
        )));
    }

    let mut targets: HashMap<ParamKey, Arc<dyn ModTarget>> = HashMap::new();
    let mut ids: HashMap<ParamKey, ModTargetId> = HashMap::new();
    let mut edges: Vec<ModEdge> = Vec::new();
    let bus = resolver.bus();

    for (i, route) in routes.iter().enumerate() {
        if !route.enabled {
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
                let id = ModTargetId::next();
                bus.insert(id, Arc::clone(&target));
                targets.insert(key, target);
                ids.insert(key, id);
                (id, range.min, range.max)
            }
        };

        edges.push(ModEdge {
            source,
            target: id,
            // Each route owns a distinct layer, so two routes onto one param
            // accumulate instead of overwriting each other. Offset past
            // `AUTOMATION`, which owns layer zero.
            key: tutti_mod::LayerKey(i as u64 + 1),
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
    transport: Res<TransportRes>,
    mut last_steady: Local<Option<i64>>,
) {
    let Some(driver) = matrix.driver.as_mut() else {
        return;
    };

    let steady = transport.settings.steady_time();
    let sample_rate = transport.sample_rate().get();
    let elapsed = match *last_steady {
        // A backwards jump means the stream restarted; charge no time for it
        // rather than winding every free-running source backwards.
        Some(prev) if steady >= prev => (steady - prev) as f64 / sample_rate,
        _ => 0.0,
    };
    *last_steady = Some(steady);

    driver.run(transport.settings.beat(), Seconds(elapsed as f32));
}
