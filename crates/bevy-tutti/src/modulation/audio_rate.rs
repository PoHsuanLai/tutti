//! Audio-rate delivery: a route as a per-sample param modulation in the
//! graph.
//!
//! The third delivery tier, beside the per-frame scalar and the beat-evaluated
//! curve. Where those hand a *value* to a sink, this declares a modulation the
//! graph renders (design doc 013 item 6):
//!
//! ```text
//! ModulatorNode ──(shaping: depth · polarity · curve)──► the node's param
//!  (the source)      summed with the param's own control, clamped once,
//!                    per frame, by the graph's fused param step
//! ```
//!
//! # What the graph does, and what is left here
//!
//! The graph owns the arithmetic: each declared param of a node (its
//! `ParamFeed`, `AudioGraphRes::declares_param`) is `clamp(base + Σ shaped
//! sources)` per frame, where the **base is the node's own control** — the
//! cell an authored write, `AudioParam`, `write_param` and `set_param` all
//! reach. An unconnected param reads that control, never 0, so a route can be
//! connected and disconnected by a commit; the graph declicks the change.
//!
//! This reconciler only declares: per `(target, param)`, the group's source
//! nodes and shapings and the declared range, written into the graph value
//! with `AudioGraphRes::set_param_mod` when they change. It used to build a
//! sub-graph per param — an `AtomicSourceNode` base, a `ParamSumNode`, a
//! `ParamShaperNode` per route — into extra input ports the node had to be
//! born with, and to keep a base cell, a clamp cell and each shaper's shaping
//! on entities to diff against; none of that exists any more.
//!
//! # Why this is a reconciler and not part of `rebuild`
//!
//! [`rebuild`](super::rebuild) compiles the *value* matrix — a routing table the
//! driver reads. This declares graph modulation, reconciled against what it
//! last declared so an unchanged frame writes nothing (a changed declaration
//! is a recompile). The source's node (`ModSourceNode`, the `LfoNode`) is
//! spawned once by [`ensure_source_nodes`] and never respawned, so a depth
//! edit does not restart the modulator's phase.

use bevy_ecs::prelude::*;
use std::collections::HashMap;

use tutti_core::AudioNode;
use tutti_types::ParamAddr;

use crate::graph::{AudioGraphRes, GraphDirty};
use crate::modulation::components::{ModClock, ModDelivery, ModParamRange, ModRoute};
use crate::modulation::driver::ParamKey;

/// One param's audio-rate modulation, as this reconciler declared it to the
/// graph.
#[derive(Debug, Clone, PartialEq)]
pub struct AudioRateParam {
    /// The graph node the param is on.
    pub node: AudioNode,
    /// Each route's source node and shaping, in route order.
    pub sources: Vec<(AudioNode, tutti_nodes::ParamModShaping)>,
    /// The declared range the sum is clamped to.
    pub range: (f32, f32),
    /// The declared base last written to the node's control.
    pub base: f32,
}

/// Every param driven at audio rate, by the param it drives.
///
/// The reconciler's memory of what it declared, keyed by `(target entity,
/// param)` — the same key the value matrix groups by, because however many
/// routes drive one param, they sum into *one* value.
#[derive(Resource, Debug, Default)]
pub struct AudioRateRoutes(pub HashMap<ParamKey, AudioRateParam>);

impl AudioRateRoutes {
    /// What drives `param` on `target` at audio rate, if anything does.
    pub fn get(&self, target: Entity, param: ParamAddr) -> Option<&AudioRateParam> {
        self.0.get(&(target, param))
    }

    /// Whether this param is driven at audio rate.
    ///
    /// The audio-rate sibling of
    /// [`ModulationMatrix::is_modulated`](super::ModulationMatrix::is_modulated).
    /// Unlike the control-rate tier, it changes nothing about where an
    /// authored write goes: the graph rides the modulation on the node's own
    /// control, so [`write_param`](crate::graph::write_param) writes the
    /// control either way.
    pub fn is_audio_rate(&self, target: Entity, param: ParamAddr) -> bool {
        self.0.contains_key(&(target, param))
    }
}

/// The routes wanting audio rate, grouped by the param they drive, each group
/// in a stable order (the route entities'), each route with its entity.
fn group_routes<'a>(
    routes: impl Iterator<Item = (Entity, &'a ModRoute)>,
) -> HashMap<ParamKey, Vec<(Entity, &'a ModRoute)>> {
    let mut grouped: HashMap<ParamKey, Vec<(Entity, &ModRoute)>> = HashMap::new();
    for (entity, route) in routes {
        if !route.enabled || route.delivery != ModDelivery::PerSample {
            continue;
        }
        grouped
            .entry((route.target, route.param))
            .or_default()
            .push((entity, route));
    }
    grouped
        .into_iter()
        .map(|(k, mut v)| {
            v.sort_by_key(|(e, _)| *e);
            (k, v)
        })
        .collect()
}

/// A group's routes as the graph's sources: one per source node, in first
/// route order, each the sum of its routes' shapings — the graph lists a
/// source once per param, and the old chain summed one shaper per route, so
/// two routes from one `ModSource` sum rather than one replacing the other.
fn merged_sources(
    sources: &[(AudioNode, tutti_nodes::ParamModShaping)],
) -> Vec<(AudioNode, tutti_graph::ParamShaping)> {
    let mut by_node: Vec<(AudioNode, Vec<tutti_nodes::ParamModShaping>)> = Vec::new();
    for &(n, s) in sources {
        match by_node.iter_mut().find(|(m, _)| *m == n) {
            Some((_, v)) => v.push(s),
            None => by_node.push((n, vec![s])),
        }
    }
    by_node
        .into_iter()
        .map(|(n, v)| (n, tutti_nodes::ParamModShaping::summed(&v)))
        .collect()
}

/// The graph node standing in for a `ModSource` on the audio-rate path.
///
/// The value path builds a `tutti_mod::Modulator` — a pure `phase -> value`
/// function with no ports — because the driver samples it directly. Audio rate
/// needs something the graph can read a signal from, which is
/// [`ModulatorNode`](tutti_nodes::ModulatorNode) wrapping that same modulator.
///
/// One modulator, two adapters: the marker records which entity's node is which
/// so a source driving both tiers is still one authored source.
#[derive(Component, Debug, Clone, Copy)]
pub struct ModSourceNode(pub Entity);

/// Give every source feeding an audio-rate route a renderable node.
///
/// Runs before [`reconcile_audio_rate`], which needs the node to exist before
/// it can name it as a source. Sources with no audio-rate route get nothing —
/// the value path does not need a node and should not pay for one.
pub fn ensure_source_nodes(
    mut commands: Commands,
    graph: Option<ResMut<AudioGraphRes>>,
    dirty: Option<ResMut<GraphDirty>>,
    routes: Query<&ModRoute>,
    sources: Query<(
        &crate::modulation::ModSource,
        &crate::modulation::ModSourceRate,
    )>,
    existing: Query<&ModSourceNode>,
) {
    let (Some(mut graph), Some(mut dirty)) = (graph, dirty) else {
        return;
    };

    for route in routes.iter() {
        if !route.enabled || route.delivery != ModDelivery::PerSample {
            continue;
        }
        if existing.get(route.source).is_ok() {
            continue;
        }
        let Ok((source, rate)) = sources.get(route.source) else {
            continue;
        };

        // The same shape and rate the value path would build, under the audio
        // adapter instead of the driver.
        let mut node = tutti_nodes::LfoNode::new(source.shape);
        node = match rate.clock {
            ModClock::Synced { beats_per_cycle } => node.with_beat_sync(beats_per_cycle),
            ModClock::Free { hz } => node.with_frequency(hz),
        };

        let entity = commands.spawn(graph.insert(node)).id();
        commands.entity(route.source).insert(ModSourceNode(entity));
        dirty.0 = true;
    }
}

/// "Did a route, a range, or a node move this frame?" — the reconciler's
/// dirty gate.
///
/// A named alias because the tuple is unreadable inline and clippy is right to
/// say so; the `()` fetch is deliberate, since only the emptiness matters.
///
/// # Why `AudioNode` is in here
///
/// The declaration needs the sink's graph node and each source's, and a route
/// is routinely visible one frame before its sink's node is (a node inserted
/// through `insert_audio_node` lands as a *deferred* command). Without this arm
/// the route is evaluated exactly once, against a sink that has no node, and
/// falls back to per-frame permanently. `Changed` rather than `Added`:
/// replacing a node's unit (a crossfade, a re-arity) rewrites the component.
type RouteChanged<'w, 's> = Query<
    'w,
    's,
    (),
    Or<(
        Changed<ModRoute>,
        Changed<ModParamRange>,
        Changed<AudioNode>,
        Changed<ModSourceNode>,
    )>,
>;

/// Declare audio-rate routes to the graph as param modulations.
///
/// Per `(target, param)` group: the param must be one the target's node
/// declares modulatable (`AudioGraphRes::declares_param`) — a node that
/// declares no such param cannot be modulated at audio rate, a fact about the
/// node rather than an error, and the route falls back to the value path,
/// which is always correct. What changed since the last declaration is
/// written; an unchanged group writes nothing.
///
/// The declared range's base is written to the node's own control when the
/// group is first declared and whenever the range moves: that control is the
/// base the modulation rides on, as the old chain's base cell was.
#[allow(
    clippy::too_many_arguments,
    reason = "Bevy systems declare their data access as parameters; every one \
              here is a distinct query or resource the reconcile genuinely reads"
)]
pub fn reconcile_audio_rate(
    mut declared: ResMut<AudioRateRoutes>,
    graph: Option<ResMut<AudioGraphRes>>,
    dirty: Option<ResMut<GraphDirty>>,
    routes: Query<(Entity, &ModRoute)>,
    ranges: Query<&ModParamRange>,
    source_nodes: Query<&ModSourceNode>,
    nodes: Query<&AudioNode>,
    changed: RouteChanged,
    mut removed: RemovedComponents<ModRoute>,
    mut unbound: RemovedComponents<AudioNode>,
) {
    // A despawned node (a source's, or the target's) is a change too: the
    // graph already dropped its param edges with it (`Editor::remove`), and
    // this keeps the declaration's memory in step.
    let is_dirty = !changed.is_empty() || !removed.is_empty() || !unbound.is_empty();
    removed.clear();
    unbound.clear();
    if !is_dirty {
        return;
    }
    let (Some(mut graph), Some(mut dirty)) = (graph, dirty) else {
        return;
    };

    let mut want: HashMap<ParamKey, AudioRateParam> = HashMap::new();
    for (key, group) in group_routes(routes.iter()) {
        let (target, param) = key;
        // Only a `UnitParam` can name a native param; a foreign
        // `ParamAddr::Id` belongs to a plugin, which has its own parameter
        // path.
        let ParamAddr::Unit(unit) = param else {
            continue;
        };
        let Ok(&node) = nodes.get(target) else {
            continue;
        };
        if !graph.declares_param(node, unit) {
            bevy_log::debug!(
                "audio-rate route on {target:?} for {unit:?}: its node declares no such \
                 modulatable param, so the route falls back to per-frame"
            );
            continue;
        }
        let Some(range) = ranges.get(target).ok().and_then(|r| r.get(param)) else {
            continue;
        };
        if range.min.is_nan() || range.max.is_nan() {
            // Declared, it would fail every commit after it (the graph
            // refuses a NaN bound): the route stays on the value path.
            bevy_log::warn!(
                "audio-rate route on {target:?} for {unit:?}: its range has a NaN bound \
                 ({} ..= {}), so the route falls back to per-frame",
                range.min,
                range.max
            );
            continue;
        }
        // A route whose source has no node (not spawned yet, or its node
        // was despawned) is left out: its offset is not there to sum, and
        // the rest of the group still is. A group with none waits.
        let routed: Vec<(Entity, AudioNode, tutti_nodes::ParamModShaping)> = group
            .iter()
            .filter_map(|&(e, r)| {
                let src = source_nodes.get(r.source).ok()?;
                let &src_node = nodes.get(src.0).ok()?;
                graph.contains(src_node).then_some((
                    e,
                    src_node,
                    tutti_nodes::ParamModShaping {
                        depth: r.depth,
                        polarity: r.polarity,
                        curve: r.curve,
                    },
                ))
            })
            .collect();
        // The graph sums at most `MAX_PARAM_SOURCES` source nodes per param
        // (a spec past it fails every commit): the routes from the first
        // that many, in route order, are declared; the rest are dropped,
        // named, and the graph keeps committing.
        let mut kept_nodes: Vec<AudioNode> = Vec::new();
        let mut dropped: Vec<Entity> = Vec::new();
        let mut sources: Vec<(AudioNode, tutti_nodes::ParamModShaping)> = Vec::new();
        for (e, n, s) in routed {
            if !kept_nodes.contains(&n) {
                if kept_nodes.len() == tutti_graph::MAX_PARAM_SOURCES {
                    dropped.push(e);
                    continue;
                }
                kept_nodes.push(n);
            }
            sources.push((n, s));
        }
        if !dropped.is_empty() {
            bevy_log::warn!(
                "audio-rate routes on {target:?} for {unit:?}: more than {} source nodes; \
                 routes {dropped:?} are not declared",
                tutti_graph::MAX_PARAM_SOURCES
            );
        }
        if sources.is_empty() {
            continue;
        }
        want.insert(
            key,
            AudioRateParam {
                node,
                sources,
                range: (range.min, range.max),
                base: range.base,
            },
        );
    }

    // Retire what is no longer wanted: the param reads its own control again.
    let stale: Vec<ParamKey> = declared
        .0
        .keys()
        .filter(|k| !want.contains_key(*k))
        .copied()
        .collect();
    for key in stale {
        let old = declared.0.remove(&key).expect("listed");
        if let ParamAddr::Unit(unit) = key.1 {
            if graph.contains(old.node) {
                graph.clear_param_mod(old.node, unit);
                dirty.0 = true;
            }
        }
    }

    for (key, p) in want {
        let ParamAddr::Unit(unit) = key.1 else {
            continue;
        };
        let old = declared.0.get(&key);
        if old == Some(&p) {
            continue;
        }
        // The base first, so the modulation's first frame rides on it.
        if old.is_none_or(|o| o.base != p.base || o.node != p.node) {
            graph.set_param(p.node, unit, p.base);
        }
        if old.is_none_or(|o| o.sources != p.sources || o.range != p.range || o.node != p.node) {
            if let Some(o) = old.filter(|o| o.node != p.node) {
                graph.clear_param_mod(o.node, unit);
            }
            let shaped = merged_sources(&p.sources);
            if let Err(e) = graph.set_param_mod(
                p.node,
                unit,
                &shaped,
                tutti_graph::ParamRange::new(p.range.0, p.range.1),
            ) {
                // Unreachable from the checks above; declared as nothing
                // rather than as a graph that cannot commit.
                bevy_log::warn!("audio-rate modulation of {unit:?} not declared: {e:?}");
                continue;
            }
        }
        dirty.0 = true;
        declared.0.insert(key, p);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy_app::prelude::*;
    use bevy_ecs::world::World;
    use tutti_types::{Depth, Hz, UnitParam};

    use crate::graph::{CapturedControls, GraphReconcilePlugin};
    use crate::modulation::{ModSourceRate, ModTargetRegistry, TuttiModulationPlugin};

    /// An app with one distortion whose Drive a route can target, ranged
    /// `range`.
    fn app_with_target(range: (f32, f32)) -> (App, Entity, AudioNode) {
        let mut app = App::new();
        app.insert_resource(AudioGraphRes::headless(0, 2));
        app.insert_resource(crate::AudioEngineState::Running);
        app.add_plugins((GraphReconcilePlugin, TuttiModulationPlugin));
        app.world_mut()
            .resource_mut::<ModTargetRegistry>()
            .register::<tutti_nodes::DistortionNode>();
        let dist = tutti_nodes::DistortionNode::new(tutti_nodes::ShapeKind::Tanh, 5.0);
        let controls = CapturedControls::capture(app.world(), &dist);
        let node = app.world_mut().resource_mut::<AudioGraphRes>().insert(dist);
        let mut target = app.world_mut().spawn(ModParamRange::default().with(
            ParamAddr::Unit(UnitParam::Drive),
            5.0,
            range.0,
            range.1,
        ));
        controls.bind(&mut target, node);
        let target = target.id();
        (app, target, node)
    }

    fn spawn_lfo(app: &mut App) -> Entity {
        app.world_mut()
            .spawn((
                crate::modulation::ModSource::new(tutti_mod::LfoShape::Sine),
                ModSourceRate::free_running(Hz(2.0)),
            ))
            .id()
    }

    fn route(app: &mut App, source: Entity, target: Entity, depth: f32) -> Entity {
        app.world_mut()
            .spawn(
                ModRoute::new(source, target, ParamAddr::Unit(UnitParam::Drive))
                    .with_depth(Depth(depth))
                    .per_sample(),
            )
            .id()
    }

    /// Whether the graph as declared compiles: what the frame's commit
    /// needs, and what a refused commit would leave stuck.
    fn commits(app: &App) -> bool {
        app.world()
            .resource::<AudioGraphRes>()
            .compensate()
            .is_some()
    }

    /// Two routes from **one** source onto one param sum, as the old chain's
    /// one-shaper-per-route did: the graph holds the source once, with the
    /// two shapings summed into its table.
    ///
    /// Mutation (run): in `merged_sources`, keep only the first route's
    /// shaping (`v[..1]`) → the table is one depth, not both → fails. Declare
    /// the routes unmerged (`s.shaping()` per route) → the graph keeps the
    /// later only → fails the same way.
    #[test]
    fn two_routes_from_one_source_sum() {
        let (mut app, target, node) = app_with_target((0.0, 10.0));
        let lfo = spawn_lfo(&mut app);
        route(&mut app, lfo, target, 0.25);
        route(&mut app, lfo, target, 0.5);
        app.update();
        app.update();

        let m = app
            .world()
            .resource::<AudioGraphRes>()
            .param_mod(node, UnitParam::Drive)
            .expect("declared");
        assert_eq!(m.sources.len(), 1, "one source node, listed once");
        let one = |d: f32| tutti_nodes::ParamModShaping {
            depth: Depth(d),
            polarity: tutti_mod::Polarity::Bipolar,
            curve: tutti_mod::CurveType::Linear,
        };
        let (a, b) = (one(0.25).shaping(), one(0.5).shaping());
        for x in [-1.0f32, -0.3, 0.0, 0.41, 1.0] {
            let want = a.apply(x) + b.apply(x);
            let got = m.sources[0].shaping.apply(x);
            assert!(
                (got - want).abs() < 1e-5,
                "x = {x}: the two routes sum to {want}, got {got}"
            );
        }
        assert!(commits(&app));
    }

    /// More routes than the graph can sum on one param: the first
    /// `MAX_PARAM_SOURCES` source nodes' routes are declared, the rest are
    /// dropped (with a warning naming them), and the graph keeps committing
    /// — where declaring them all would fail every commit after.
    ///
    /// Mutation (run): drop the cap (never push to `dropped`) → the spec
    /// holds one source too many, `set_param_mod` refuses it, nothing is
    /// declared → fails.
    #[test]
    fn routes_past_the_source_cap_are_dropped_and_the_graph_commits() {
        let (mut app, target, node) = app_with_target((0.0, 10.0));
        let n = tutti_graph::MAX_PARAM_SOURCES + 1;
        let lfos: Vec<Entity> = (0..n).map(|_| spawn_lfo(&mut app)).collect();
        let routes: Vec<Entity> = lfos
            .iter()
            .map(|&l| route(&mut app, l, target, 0.1))
            .collect();
        app.update();
        app.update();

        let m = app
            .world()
            .resource::<AudioGraphRes>()
            .param_mod(node, UnitParam::Drive)
            .expect("declared");
        assert_eq!(m.sources.len(), tutti_graph::MAX_PARAM_SOURCES);
        // The last route in entity order is the one dropped.
        let last = *routes.iter().max().expect("routes");
        let last_src = app.world().get::<ModRoute>(last).expect("route").source;
        let dropped_node = *app
            .world()
            .get::<AudioNode>(app.world().get::<ModSourceNode>(last_src).expect("node").0)
            .expect("node");
        assert!(
            !m.sources
                .iter()
                .any(|s| s.from.node() == tutti_types::graph::NodeKey(dropped_node.0.value())),
            "the last route's source is the one left out"
        );
        assert!(commits(&app), "the graph still commits");
    }

    /// A NaN bound never reaches the graph: the route stays on the value
    /// path, and the graph keeps committing. And `set_param_mod` refuses
    /// what the commit would fail on, by name, touching nothing.
    ///
    /// Mutation (run): drop the NaN check in `set_param_mod` → the direct
    /// call is accepted → fails. Dropping the reconciler's own check alone
    /// survives, by design: `set_param_mod` then refuses the range and the
    /// route stays on the value path all the same (only the warning's
    /// wording differs); with both dropped the spec holds a NaN range and
    /// does not compile → fails.
    #[test]
    fn a_nan_range_is_never_declared() {
        let (mut app, target, node) = app_with_target((f32::NAN, 10.0));
        let lfo = spawn_lfo(&mut app);
        route(&mut app, lfo, target, 0.5);
        app.update();
        app.update();
        let graph = app.world().resource::<AudioGraphRes>();
        assert!(graph.param_mod(node, UnitParam::Drive).is_none());
        assert!(
            !app.world()
                .resource::<AudioRateRoutes>()
                .is_audio_rate(target, ParamAddr::Unit(UnitParam::Drive)),
            "on the value path"
        );
        assert!(commits(&app));

        let src = *app
            .world()
            .get::<AudioNode>(app.world().get::<ModSourceNode>(lfo).expect("node").0)
            .expect("node");
        let mut graph = app.world_mut().resource_mut::<AudioGraphRes>();
        let id = tutti_graph::ParamShaping::Identity;
        let nan = tutti_graph::ParamRange::new(0.0, f32::NAN);
        assert!(matches!(
            graph.set_param_mod(node, UnitParam::Drive, &[(src, id.clone())], nan),
            Err(tutti_graph::GraphInvalid::BadParamRange { .. })
        ));
        let ok = tutti_graph::ParamRange::new(0.0, 1.0);
        assert!(matches!(
            graph.set_param_mod(
                node,
                UnitParam::Drive,
                &[(src, id.clone()), (src, id.clone())],
                ok
            ),
            Err(tutti_graph::GraphInvalid::UnsortedParamSources { .. })
        ));
        assert!(
            graph.param_mod(node, UnitParam::Drive).is_none(),
            "untouched"
        );
        assert!(commits(&app));
    }

    /// A group's routes are ordered by their entities, whatever order the
    /// query yields them in: the graph sums sources in source order, and a
    /// group that reordered from frame to frame would be declared anew each
    /// frame (a recompile and a declick for nothing).
    ///
    /// Mutation (run): drop the sort → the reversed input comes out
    /// reversed → fails.
    #[test]
    fn a_group_is_in_route_entity_order() {
        let mut world = World::new();
        let (a, b, t) = (
            world.spawn_empty().id(),
            world.spawn_empty().id(),
            world.spawn_empty().id(),
        );
        let p = ParamAddr::Unit(UnitParam::Drive);
        // Routes keyed by their own entities, the later one handed in first.
        let (first, second) = if a < b { (a, b) } else { (b, a) };
        let r1 = ModRoute::new(first, t, p).per_sample();
        let r2 = ModRoute::new(second, t, p).per_sample();
        let g = group_routes([(second, &r2), (first, &r1)].into_iter());
        let group = &g[&(t, p)];
        assert_eq!(group[0].1.source, first);
        assert_eq!(group[1].1.source, second);
    }
}
