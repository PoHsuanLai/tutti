//! Audio-rate delivery: materialising a route as a per-sample graph edge.
//!
//! The third delivery tier, beside the per-frame scalar and the beat-evaluated
//! curve. Where those hand a *value* to a sink, this builds the chain the engine
//! renders:
//!
//! ```text
//! ModulatorNode ─► ParamShaperUnit ─► ParamSumUnit ─► node's param port
//!  (the source)     (depth·polarity     (base + Σ offsets,
//!                    ·curve, LUT)        one clamp)
//! ```
//!
//! # Why this is a reconciler and not part of `rebuild`
//!
//! [`rebuild`](super::rebuild) compiles the *value* matrix — a routing table the
//! driver reads. This compiles a *graph*, and a graph is reconciled against what
//! already exists rather than rebuilt from scratch: respawning the chain every
//! time a depth slider moved would re-allocate nodes on a frame that only
//! needed one atomic written, and would revert the authored base — the chain's
//! base cell does not survive a respawn.
//!
//! **What "do not respawn" protects, precisely.** The instinct is "it restarts
//! the LFO", and that is true of a *source* node but not of this chain: the
//! `LfoNode` lives on the `ModSource` entity ([`ModSourceNode`]), spawned once
//! by [`ensure_source_nodes`] and skipped forever after. So a shaper can be
//! swapped without touching a modulator's phase, which is what
//! `reshape_chain` does for an edit no setter can carry.
//!
//! So the two are deliberately separate systems over the same declaration. A
//! route asks for audio rate with [`ModDelivery::PerSample`]; anything else
//! stays on the value path untouched.
//!
//! # The base chain is not optional
//!
//! A node born with a param port reads that port *unconditionally* — it cannot
//! ask whether anything is connected. An unconnected input in `Net` is `Zero`,
//! so a port with nothing feeding it delivers the param as literal `0.0`, not
//! as its authored value: a distortion at drive 0 is silence, not a passthrough.
//!
//! That is why `spawn_chain` wires base → sum → port in the same call that
//! claims the port. "Ports on now, base later" is not a cheap idle state, it is
//! a broken node. (`tutti-units`' `born_with_ports` test pins this.)

use bevy_ecs::prelude::*;
use std::collections::HashMap;

use tutti_types::{ParamAddr, UnitParam};
// The three chain units are not imported here: `build_param_mod` owns their
// construction, which is what keeps the base cell reachable.

use crate::graph::{AudioGraphRes, GraphDirty, PortSource, PortSources};
use crate::modulation::components::{ModClock, ModDelivery, ModParamRange, ModRoute};
use crate::modulation::driver::ParamKey;

/// The graph chain materialising one modulated param's audio-rate routes.
///
/// Keyed by `(target entity, param)` — the same key the value matrix groups by,
/// because the constraint is the same: however many routes drive one param, they
/// must sum into *one* value. Here that is literal, as `ParamSumUnit`'s arity.
#[derive(Debug, Clone)]
pub struct ParamChain {
    /// Feeds the sum's base port — the authored value, riding under the
    /// modulation. Its cell is [`base_cell`](ParamChain::base_cell), which is
    /// where every authored write for this param lands.
    pub base: Entity,
    /// `base + Σ offsets`, clamped once. Its arity is a function of the whole
    /// route group, which is why chains are built per param and not per route.
    pub sum: Entity,
    /// One shaper per route, in the order their offsets occupy the sum's ports.
    pub shapers: Vec<Entity>,
    /// The shaping each live shaper was **built with**.
    ///
    /// `ParamShaperUnit` bakes depth, polarity and curve into a LUT at
    /// construction and exposes no setter, so the only way to know a route's
    /// shaping has moved is to remember what the node was made from. Without
    /// this the reconciler's sole identity test is the group's *arity*, and a
    /// depth slider — which changes no count — is invisible to it.
    shaping: Vec<tutti_units::ParamModShaping>,
    /// The sink's param-port index, resolved from `ParamPorts` at spawn.
    pub port: usize,
    /// The atomic the base unit reads — **the chain's single base owner**.
    ///
    /// An authored write lands here, not on the node: a node whose param port
    /// is wired never reads its own atomic. Held so the chain outlives its
    /// construction value — without it the base is frozen at whatever
    /// `ModParamRange` said when the chain was built, which is what
    /// [`write_param`](crate::graph::write_param)'s audio-rate branch and
    /// [`refresh_base`](Self::refresh_base) both exist to prevent.
    base_cell: std::sync::Arc<tutti_core::AtomicF32>,
    /// The sum's live clamp range.
    ///
    /// Held for the same reason as [`base_cell`](Self::base_cell), and against
    /// the same failure: a range is authored state that moves without the graph
    /// moving, and it is baked into `ParamSumUnit` at construction. Without a
    /// handle the only way to apply a new range is to rebuild the sum — which
    /// rebuilds the chain, which loses the base.
    bounds: std::sync::Arc<tutti_units::ClampBounds>,
}

impl ParamChain {
    /// The chain's base cell — see the field's doc for why it is the only
    /// address an authored write has.
    pub fn base_cell(&self) -> std::sync::Arc<tutti_core::AtomicF32> {
        std::sync::Arc::clone(&self.base_cell)
    }

    /// Store `base` if it differs from what the cell already holds.
    ///
    /// Guarded rather than unconditional: the audio thread loads this cell once
    /// per block, and a same-value store is a pointless write to a line another
    /// thread is reading.
    fn refresh_base(&self, base: f32) {
        use tutti_core::Ordering;
        if self.base_cell.load(Ordering::Acquire) != base {
            self.base_cell.store(base, Ordering::Release);
        }
    }

    /// Store `(min, max)` if either differs from what the sum already clamps to.
    ///
    /// Guarded like [`refresh_base`](Self::refresh_base), and for the same
    /// reason — but here the guard also narrows the window in which a reader can
    /// observe the two stores half-applied. It cannot close it; `ParamSumUnit`
    /// orders the pair at the read for that.
    fn refresh_bounds(&self, min: f32, max: f32) {
        if self.bounds.get() != (min, max) {
            self.bounds.set(min, max);
        }
    }
}

/// Every audio-rate chain currently materialised, by the param it drives.
///
/// The reconciler's memory of what it built. Without it a rebuild could not tell
/// "this chain already exists" from "this chain is new", and would either
/// duplicate nodes or leak them.
#[derive(Resource, Debug, Default)]
pub struct AudioRateChains(pub HashMap<ParamKey, ParamChain>);

impl AudioRateChains {
    /// The chain driving `param` on `target`, if one is built.
    pub fn get(&self, target: Entity, param: ParamAddr) -> Option<&ParamChain> {
        self.0.get(&(target, param))
    }

    /// Whether this param is driven at audio rate.
    ///
    /// The audio-rate sibling of
    /// [`ModulationMatrix::is_modulated`](super::ModulationMatrix::is_modulated),
    /// and it matters for the same reason: a param whose port is fed by a sum
    /// does not read its own atomic at all, so an authored write has to reach
    /// the sum's base cell instead of the node.
    pub fn is_audio_rate(&self, target: Entity, param: ParamAddr) -> bool {
        self.0.contains_key(&(target, param))
    }

    /// The base cell for an audio-rate param, if one is materialised.
    ///
    /// The write half of [`is_audio_rate`](Self::is_audio_rate): that predicate
    /// says an authored write must go elsewhere, and this says where.
    /// [`write_param`](crate::graph::write_param) is the caller.
    pub fn base_cell(
        &self,
        target: Entity,
        param: ParamAddr,
    ) -> Option<std::sync::Arc<tutti_core::AtomicF32>> {
        self.0.get(&(target, param)).map(ParamChain::base_cell)
    }
}

/// The routes wanting audio rate, grouped by the param they drive.
///
/// Grouping is forced, not stylistic: `ParamSumUnit`'s arity is the *group's*
/// size, so nothing can be spawned until the whole group is known.
fn group_routes<'a>(
    routes: impl Iterator<Item = &'a ModRoute>,
) -> HashMap<ParamKey, Vec<&'a ModRoute>> {
    let mut grouped: HashMap<ParamKey, Vec<&ModRoute>> = HashMap::new();
    for route in routes {
        if !route.enabled || route.delivery != ModDelivery::PerSample {
            continue;
        }
        grouped
            .entry((route.target, route.param))
            .or_default()
            .push(route);
    }
    grouped
}

/// Spawn the chain for one param and declare its wiring.
///
/// Returns `None` if the sink has no audio-rate port for this param — a node
/// that was never built with `with_param_inputs` cannot be modulated at audio
/// rate, and that is a fact about the node, not an error here. The route falls
/// back to the value path, which is always correct.
#[allow(
    clippy::too_many_arguments,
    reason = "the chain needs its sink entity, its sink node, the param, its \
              range and the route group; bundling them would only move the list"
)]
fn spawn_chain(
    commands: &mut Commands<'_, '_>,
    graph: &mut AudioGraphRes,
    dirty: &mut GraphDirty,
    sink: Entity,
    target_node: tutti_core::NodeId,
    param: ParamAddr,
    range: &crate::modulation::components::ParamRange,
    routes: &[&ModRoute],
    source_nodes: &HashMap<Entity, Entity>,
) -> Option<ParamChain> {
    // Only a `UnitParam` can name a native port; a foreign `ParamAddr::Id`
    // belongs to a plugin, which has its own parameter path.
    let ParamAddr::Unit(unit) = param else {
        return None;
    };
    let port = param_port(graph, target_node, unit)?;

    // A `ModSource` entity carries a *modulator*, not a graph node — the value
    // path never needed one. `ensure_source_nodes` spawns the `LfoNode` that
    // makes the same modulator renderable, and this is where the shapers pick
    // them up. Resolved before building so a missing source aborts the whole
    // chain rather than leaving half of it in the graph.
    let feeds: Vec<Entity> = routes
        .iter()
        .map(|r| source_nodes.get(&r.source).copied())
        .collect::<Option<_>>()?;

    // Through the engine's own assembler, which is what keeps the base cell.
    // `build_param_mod`, not `wire_param_mod`: this crate declares its edges as
    // `PortSources` and diffs them against `Net` each frame, so an edge
    // connected here would be reverted on the next wire pass. The builder makes
    // the nodes; the declarations below make the edges.
    let shaping: Vec<tutti_units::ParamModShaping> = routes
        .iter()
        .map(|r| tutti_units::ParamModShaping {
            depth: r.depth,
            polarity: r.polarity,
            curve: r.curve,
        })
        .collect();
    let built =
        tutti_units::build_param_mod(&mut graph.0, range.base, range.min, range.max, &shaping);
    // **The handle, not a copy.** A node whose param port is wired never reads
    // its own atomic, so this cell is the only address an authored write has —
    // see `ParamModChain::base_cell` and `write_param`'s audio-rate branch.
    let base_cell = built.base_cell();
    let bounds = built.bounds();

    let base = commands.spawn(tutti_core::AudioNode(built.base)).id();
    let sum = commands.spawn(tutti_core::AudioNode(built.sum)).id();

    let mut shapers = Vec::with_capacity(routes.len());
    let mut sum_sources = PortSources::silent().with(0, PortSource::node(base));
    for (i, (&shaper_id, &feed)) in built.shapers.iter().zip(feeds.iter()).enumerate() {
        let shaper = commands.spawn(tutti_core::AudioNode(shaper_id)).id();
        commands.entity(shaper).insert(PortSources::from(feed));
        // Offsets occupy ports 1..=N, in group order.
        sum_sources = sum_sources.with(i + 1, PortSource::node(shaper));
        shapers.push(shaper);
    }

    commands.entity(sum).insert(sum_sources);
    // The param port joins the sink's existing declaration rather than
    // replacing it — a param port is an ordinary input port, and one
    // `PortSources` owns the whole port space (see `graph::wire`). Reading the
    // current declaration and extending it is what keeps the audio ports the
    // host declared intact.
    commands.queue(move |world: &mut World| {
        let existing = world
            .get::<PortSources>(sink)
            .cloned()
            .unwrap_or_else(PortSources::silent);
        if let Ok(mut e) = world.get_entity_mut(sink) {
            e.insert(existing.with(port, PortSource::node(sum)));
        }
    });

    dirty.0 = true;
    Some(ParamChain {
        base,
        sum,
        shapers,
        shaping,
        port,
        base_cell,
        bounds,
    })
}

/// The sink's param-port index, or `None` if it exposes none for this param.
///
/// Dispatches on the concrete node type, exactly as resolving a control-rate
/// target does — `ParamPorts` is a trait on the unit, so answering needs a
/// downcast. That asymmetry with `AudioParam` (which is uniform because
/// `Net::set` addresses a param by name) is inherent, not incidental.
fn param_port(graph: &AudioGraphRes, node: tutti_core::NodeId, param: UnitParam) -> Option<usize> {
    use tutti_units::ParamPorts;
    macro_rules! try_kinds {
        ($($ty:ty),+ $(,)?) => {
            $(
                if let Some(unit) = graph.0.node_as::<$ty>(node) {
                    return unit.param_port(param);
                }
            )+
        };
    }
    // **Every `ParamPorts` impl must be listed here.** A type that implements the
    // trait but is missing from this list answers `None`, so `PerSample` falls
    // back to per-frame — silently, because falling back is a legal outcome that
    // means "this sink has no port". There is nothing to distinguish "no port"
    // from "not dispatched", which is why the list is checked by a test rather
    // than left to review.
    //
    // The two filters were the omission that made this comment necessary: they
    // are the only units exposing `Cutoff` and `Q`, and a filter cutoff is the
    // case `ModDelivery::PerSample`'s own docs name for it ("a fast LFO on a
    // filter cutoff"). So the tier's headline use was the one it could not serve.
    try_kinds!(
        tutti_units::StereoSvfFilterNode<f32>,
        tutti_units::StereoSvfFilterNode<f64>,
        tutti_units::StereoLadderFilterNode<f32>,
        tutti_units::StereoLadderFilterNode<f64>,
        tutti_units::DistortionNode,
        tutti_units::Compressor,
        tutti_units::Gate,
        tutti_units::LimiterNode,
        tutti_units::StereoDelayLineNode,
    );
    None
}

/// The graph node standing in for a `ModSource` on the audio-rate path.
///
/// The value path builds a `tutti_mod::Modulator` — a pure `phase -> value`
/// function with no ports — because the driver samples it directly. Audio rate
/// needs something a graph edge can *connect to*, which is
/// [`ModulatorNode`](tutti_units::ModulatorNode) wrapping that same modulator.
///
/// One modulator, two adapters: the marker records which entity's node is which
/// so a source driving both tiers is still one authored source.
#[derive(Component, Debug, Clone, Copy)]
pub struct ModSourceNode(pub Entity);

/// Give every source feeding an audio-rate route a renderable node.
///
/// Runs before [`reconcile_audio_rate`], which needs the node to exist before
/// it can point a shaper at it. Sources with no audio-rate route get nothing —
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
        let mut node = tutti_units::LfoNode::new(source.shape);
        node = match rate.clock {
            ModClock::Synced { beats_per_cycle } => node.with_beat_sync(beats_per_cycle),
            ModClock::Free { hz } => node.with_frequency(hz),
        };

        let id = graph.0.add(node);
        let entity = commands.spawn(tutti_core::AudioNode(id)).id();
        commands.entity(route.source).insert(ModSourceNode(entity));
        dirty.0 = true;
    }
}

/// "Did a route, a range, or a sink's node move this frame?" — the
/// reconciler's dirty gate.
///
/// A named alias because the tuple is unreadable inline and clippy is right to
/// say so; the `()` fetch is deliberate, since only the emptiness matters.
///
/// # Why `AudioNode` is in here
///
/// `spawn_chain` needs the sink's graph node in order to resolve its param
/// port, and returns `None` when the sink has none yet. That is the right answer
/// at the time, but not a *permanent* one — so `Changed<AudioNode>` is in the
/// gate to ask again when the node lands.
///
/// The order it covers is the ordinary one. A host that compiles a document
/// declares routes and spawns nodes in the same frame, and a node inserted
/// through `insert_audio_node` lands as a *deferred* command — so the route is
/// visible one frame before the `AudioNode` is. Without this arm the route is
/// evaluated exactly once, against a sink that has no node, and falls back to
/// per-frame permanently. Nothing reports it, because falling back is a legal
/// outcome meaning "this sink exposes no port".
///
/// `Changed` rather than `Added`: replacing a node's unit (a crossfade, a
/// re-arity) rewrites the component, and the new node's port index need not
/// match the old one's.
type RouteChanged<'w, 's> = Query<
    'w,
    's,
    (),
    Or<(
        Changed<ModRoute>,
        Changed<ModParamRange>,
        Changed<tutti_core::AudioNode>,
    )>,
>;

/// Reconcile audio-rate routes into graph chains.
#[allow(
    clippy::too_many_arguments,
    reason = "Bevy systems declare their data access as parameters; every one \
              here is a distinct query or resource the reconcile genuinely reads"
)]
pub fn reconcile_audio_rate(
    mut commands: Commands,
    mut chains: ResMut<AudioRateChains>,
    graph: Option<ResMut<AudioGraphRes>>,
    dirty: Option<ResMut<GraphDirty>>,
    routes: Query<&ModRoute>,
    ranges: Query<&ModParamRange>,
    nodes: Query<&tutti_core::AudioNode>,
    source_nodes: Query<(Entity, &ModSourceNode)>,
    changed: RouteChanged,
    mut removed: RemovedComponents<ModRoute>,
) {
    let is_dirty = !changed.is_empty() || !removed.is_empty();
    removed.clear();
    if !is_dirty {
        return;
    }
    let (Some(mut graph), Some(mut dirty)) = (graph, dirty) else {
        return;
    };

    let source_nodes: HashMap<Entity, Entity> =
        source_nodes.iter().map(|(e, n)| (e, n.0)).collect();
    let grouped = group_routes(routes.iter());

    // Retire chains whose routes are gone, so a deleted route does not leave a
    // sum feeding a stale offset into the node forever.
    let stale: Vec<ParamKey> = chains
        .0
        .keys()
        .filter(|key| !grouped.contains_key(*key))
        .copied()
        .collect();
    for key in stale {
        if let Some(chain) = chains.0.remove(&key) {
            despawn_chain(&mut commands, &mut graph, &mut dirty, key.0, &chain);
        }
    }

    for (key, group) in grouped {
        // An existing chain of the right shape is left alone: respawning would
        // restart every LFO in it.
        //
        // But "left alone" must not mean "left stale". The base is authored
        // state that changes without the chain's *shape* changing — a fader
        // move, a document edit — and `ModParamRange` is in this system's dirty
        // gate precisely so those arrive here. Without this refresh the base is
        // read once at spawn and never again, and an authored edit to an
        // audio-rate param is silently discarded for the chain's whole life.
        //
        // Cheap and in-place: guarded atomic stores, no respawn. The bounds
        // ride along for the same reason the base does — a range is authored
        // state that moves without the graph moving, and `ParamSumUnit` holds
        // both in a shared cell rather than baking them at construction.
        //
        // Shaping is handled just below, and needs more than a store because
        // `ParamShaperUnit` has no setter either.
        if chains
            .0
            .get(&key)
            .is_some_and(|c| c.shapers.len() == group.len())
        {
            if let Some(range) = ranges.get(key.0).ok().and_then(|r| r.get(key.1)) {
                if let Some(chain) = chains.0.get(&key) {
                    chain.refresh_base(range.base);
                    chain.refresh_bounds(range.min, range.max);
                }
            }
            reshape_chain(
                &mut commands,
                &mut graph,
                &mut dirty,
                &mut chains,
                key,
                &group,
                &source_nodes,
            );
            continue;
        }
        if let Some(old) = chains.0.remove(&key) {
            despawn_chain(&mut commands, &mut graph, &mut dirty, key.0, &old);
        }
        let (target, param) = key;
        let Ok(node) = nodes.get(target) else {
            continue;
        };
        let Ok(range) = ranges.get(target) else {
            continue;
        };
        let Some(range) = range.get(param) else {
            continue;
        };
        if let Some(chain) = spawn_chain(
            &mut commands,
            &mut graph,
            &mut dirty,
            target,
            node.0,
            param,
            range,
            &group,
            &source_nodes,
        ) {
            chains.0.insert(key, chain);
        }
    }
}

/// Rebuild only those shapers whose declared shaping has moved.
///
/// # Why a targeted respawn rather than a setter or a whole-chain rebuild
///
/// [`ParamShaperUnit`](tutti_units::ParamShaperUnit) bakes depth, polarity and
/// curve into a LUT at construction and exposes no setter, so a moved slider
/// cannot be written into the live node — something has to be rebuilt.
///
/// Rebuilding the *chain* is what the module's anti-respawn note warns against,
/// but read that note precisely: it protects the **LFO's phase**, and the LFO is
/// not in the chain. `ensure_source_nodes` spawns it on the `ModSource` entity
/// (`ModSourceNode`) and skips any source that already has one, so it survives
/// anything done here. What a whole-chain respawn would actually cost is the
/// base cell — the authored value would silently revert to `ModParamRange`'s —
/// and two extra nodes churned for a param that only needed one.
///
/// So: one shaper out, one shaper in, the sum and the base untouched. A depth
/// drag churns exactly one node per moved route per frame.
fn reshape_chain(
    commands: &mut Commands<'_, '_>,
    graph: &mut AudioGraphRes,
    dirty: &mut GraphDirty,
    chains: &mut AudioRateChains,
    key: ParamKey,
    routes: &[&ModRoute],
    source_nodes: &HashMap<Entity, Entity>,
) {
    let Some(chain) = chains.0.get_mut(&key) else {
        return;
    };

    let mut sum_sources: Option<PortSources> = None;
    for (i, route) in routes.iter().enumerate() {
        let want = tutti_units::ParamModShaping {
            depth: route.depth,
            polarity: route.polarity,
            curve: route.curve,
        };
        if chain.shaping.get(i) == Some(&want) {
            continue;
        }

        let unit = tutti_units::ParamShaperUnit::new(want.depth, want.polarity, want.curve);
        let id = graph.0.add(unit);
        let replacement = commands.spawn(tutti_core::AudioNode(id)).id();

        // The new shaper needs the same feed the old one had. Re-declared from
        // the route rather than copied off the old entity, because the route is
        // the source of truth and the old entity is about to be despawned.
        //
        // A source with no node is not an error here — it means the LFO has not
        // spawned yet. Leaving the old shaper in place is the right failure: a
        // stale depth beats an offset port fed by nothing.
        let Some(&feed) = source_nodes.get(&route.source) else {
            commands.entity(replacement).despawn();
            continue;
        };
        commands.entity(replacement).insert(PortSources::from(feed));

        let old = std::mem::replace(&mut chain.shapers[i], replacement);
        commands.entity(old).despawn();
        chain.shaping[i] = want;

        // The sum's declaration names the shaper *entity*, so it has to be
        // re-declared with the replacement. Built once and inserted after the
        // loop so N moved routes cost one component write, not N.
        let sources = sum_sources.get_or_insert_with(|| {
            let mut s = PortSources::silent().with(0, PortSource::node(chain.base));
            for (j, &sh) in chain.shapers.iter().enumerate() {
                s = s.with(j + 1, PortSource::node(sh));
            }
            s
        });
        sources.set(i + 1, PortSource::node(replacement));
        dirty.0 = true;
    }

    if let Some(sources) = sum_sources {
        commands.entity(chain.sum).insert(sources);
    }
}

/// Tear a chain down: despawn its entities and let the `AudioNode` remove
/// observer take the graph nodes with them.
///
/// # The sink's declaration goes too
///
/// `spawn_chain` extended the sink's `PortSources` with
/// `port -> PortSource::node(sum)`. Leaving that behind points the declaration
/// at a despawned entity, and the wire rebuild then resolves it to nothing — so
/// the param port ends up **unfed**, which this module's own docs note reads as
/// a literal `0.0` rather than as the authored value. A distortion at drive 0
/// is silence, not a passthrough.
///
/// Retiring the declaration returns the port to `Silent`, which is the state
/// `spawn_chain` found it in.
fn despawn_chain(
    commands: &mut Commands<'_, '_>,
    _graph: &mut AudioGraphRes,
    dirty: &mut GraphDirty,
    sink: Entity,
    chain: &ParamChain,
) {
    for e in std::iter::once(chain.base)
        .chain(std::iter::once(chain.sum))
        .chain(chain.shapers.iter().copied())
    {
        commands.entity(e).despawn();
    }

    // Deferred for the same reason the insert was: one `PortSources` owns the
    // sink's whole port space, so this reads the current declaration and edits
    // one port of it rather than replacing the component.
    let port = chain.port;
    commands.queue(move |world: &mut World| {
        let Some(existing) = world.get::<PortSources>(sink).cloned() else {
            return; // sink already gone — nothing to retire
        };
        if let Ok(mut e) = world.get_entity_mut(sink) {
            e.insert(existing.with(port, PortSource::Silence));
        }
    });

    dirty.0 = true;
}

#[cfg(test)]
mod param_port_tests {
    use super::*;
    use tutti_core::dsp::Net;

    /// Every unit that declares a port for `param` must be *dispatched* by
    /// [`param_port`].
    ///
    /// The list inside that function is a hand-maintained downcast chain, and a
    /// `ParamPorts` impl missing from it fails **silently**: the lookup answers
    /// `None`, which is indistinguishable from the legitimate "this sink exposes
    /// no port", so `ModDelivery::PerSample` quietly falls back to per-frame.
    ///
    /// This is not hypothetical. Both filters were absent while every other impl
    /// was present, so `Cutoff` and `Q` — the only params either of them offers,
    /// and the case `PerSample`'s own docs name ("a fast LFO on a filter
    /// cutoff") — could never reach audio rate.
    ///
    /// Asserted through the real `Net` + downcast path rather than by calling
    /// `ParamPorts` directly, because the downcast *is* what was broken: a
    /// direct call would have passed throughout the bug.
    ///
    /// Note every unit is built with its `with_param_inputs` constructor. A port
    /// exists only when the node was built to have one — `cutoff_port()` is
    /// `mod_cutoff.then_some(..)` — so a plain `new()` would make this test
    /// vacuous by reporting `None` for a correctly-dispatched type.
    #[test]
    fn every_ported_unit_is_dispatched() {
        let mut net = Net::new(0, 0);
        let cases: Vec<(tutti_core::NodeId, UnitParam, &str)> = vec![
            (
                net.add(tutti_units::StereoSvfFilterNode::<f32>::with_param_inputs(
                    2,
                    tutti_units::SvfType::LowPass,
                    tutti_types::Hz(1000.0),
                    tutti_types::Q(0.707),
                    true,
                    true,
                )),
                UnitParam::Cutoff,
                "StereoSvfFilterNode<f32>",
            ),
            (
                net.add(
                    tutti_units::StereoLadderFilterNode::<f32>::with_param_inputs(
                        2,
                        tutti_units::LadderType::LP24,
                        tutti_types::Hz(1000.0),
                        tutti_types::Resonance(0.5),
                        true,
                        false,
                        false,
                    ),
                ),
                UnitParam::Cutoff,
                "StereoLadderFilterNode<f32>",
            ),
        ];

        let graph = AudioGraphRes(net);
        for (node, param, name) in cases {
            assert!(
                param_port(&graph, node, param).is_some(),
                "`{name}` declares a port for {param:?} but `param_port` does not \
                 dispatch it — audio-rate modulation onto that param falls back \
                 to per-frame, silently"
            );
        }
    }
}
