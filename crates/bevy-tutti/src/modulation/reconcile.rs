//! The adapter: the frame's declarations, collected from components and
//! handed to the session's modulation phases (design 015 §4.4).
//!
//! [`reconcile`] runs after [`GraphReconcileSystems::Spawn`], so a node
//! spawned this frame has its `AudioNode`, and before the frame's commit:
//! it runs `Phase::Modulation` over the frame's declarations, keyed by
//! entities ([`Declared`]). [`after_commit`] runs `Phase::AfterCommit`
//! with them after the commit: the session re-applies while a target is
//! unresolved and commits what that built, and sends the note-envelope
//! resyncs the apply owes. The loop, the refusal's dedupe and the
//! unresolved warning are the session's; this keeps the entity mapping.
//!
//! [`GraphReconcileSystems::Spawn`]: crate::graph::GraphReconcileSystems::Spawn

use bevy_ecs::message::{Message, MessageWriter};
use bevy_ecs::prelude::*;
use bevy_ecs::system::SystemParam;

use tutti_core::AudioNode;
use tutti_graph::EventOut;
use tutti_mod::{
    Declarations, RouteDecl, RouteKey, RouteTarget, Source, SourceKey, TargetDecl, TargetKey,
};
use tutti_nodes::automation::LaneControls;
use tutti_nodes::{Feed, ReconcileError};
use tutti_session::{ModInput, ModRefusal, Phase, SessionEvent};
use tutti_types::NodeKey;

use super::declare::{
    Automation, AutomationTake, ModFeed, ModRange, ModRoute, ModSource, ParamRef,
};
use crate::graph::latency::{ChannelCompensation, GraphLatency};
use crate::graph::AudioGraphRes;

/// The app's modulation as the session applied it, read through
/// [`AudioGraphRes::modulation`]: the lanes, what is unresolved, the
/// refusal held, in the app's entities. Each read takes the session's lock
/// for that read alone, and hands back what it read.
#[derive(Clone, Copy)]
pub struct ModulationState<'a>(&'a AudioGraphRes);

impl ModulationState<'_> {
    fn read<T>(&self, f: impl FnOnce(&tutti_session::ModState) -> T) -> T {
        f(self.0.read().mod_state())
    }

    /// Why the declarations are refused now, `None` while they apply. The
    /// graph keeps what the last apply that succeeded built.
    pub fn refusal(&self) -> Option<ReconcileError> {
        self.read(|s| match s.refusal()? {
            ModRefusal::Apply(error) => Some(error.clone()),
            // The app declares through components only: the session's own
            // `mods()` is out of its reach, so a second declarer cannot be.
            _ => None,
        })
    }

    /// `target`'s lane: its node and its controls (the knob, the range, the
    /// counters). `None` while it has none.
    pub fn lane(&self, target: ParamRef) -> Option<(NodeKey, LaneControls)> {
        self.read(|s| {
            s.lane(target.key())
                .map(|(node, controls)| (node, controls.clone()))
        })
    }

    /// Writes `value` into `target`'s knob: for a plugin parameter with a
    /// lane, the lane's own knob; for a native param, the node's cell the
    /// lane reads (as [`write_param`](crate::graph::write_param) does).
    /// `false` when `target` has no lane with a knob.
    pub fn set_knob(&self, target: ParamRef, value: f32) -> bool {
        self.read(|s| s.set_knob(target.key(), value))
    }

    /// Signal source `source`'s node (a follower's, a note envelope's).
    pub fn source_node(&self, source: Entity) -> Option<NodeKey> {
        self.read(|s| s.source_node(SourceKey(source.to_bits())))
    }

    /// The targets the last apply could not resolve: their node is not in
    /// the graph (or not yet committed), or a plugin's values are not read
    /// yet.
    pub fn unresolved_targets(&self) -> impl Iterator<Item = ParamRef> {
        self.read(|s| {
            s.unresolved_targets()
                .iter()
                .filter_map(|&k| ParamRef::from_key(k))
                .collect::<Vec<_>>()
        })
        .into_iter()
    }

    /// The followers the last apply could not feed.
    pub fn unresolved_sources(&self) -> impl Iterator<Item = Entity> {
        self.read(|s| {
            s.unresolved_sources()
                .iter()
                .filter_map(|k| Entity::try_from_bits(k.0))
                .collect::<Vec<_>>()
        })
        .into_iter()
    }

    /// Note-envelope resyncs owed to the next `AfterCommit`.
    pub fn resyncs_owed(&self) -> usize {
        self.read(|s| s.resyncs_owed())
    }
}

impl AudioGraphRes {
    /// The app's modulation as applied ([`ModulationState`]).
    pub fn modulation(&self) -> ModulationState<'_> {
        ModulationState(self)
    }
}

/// A refused apply (doc 014 §8.2): the declarations as they stand do not
/// apply, and the graph keeps what it had. Sent once when a refusal starts
/// (or changes); [`ModulationState::refusal`] holds it while it lasts.
#[derive(Message, Clone, Debug, PartialEq)]
pub struct ModulationRefused {
    /// Why.
    pub error: ReconcileError,
}

impl ModulationRefused {
    fn refusal(&self) -> ModRefusal {
        ModRefusal::Apply(self.error.clone())
    }

    /// The route entity it names: a route whose follower is downstream of
    /// its own target (`ReconcileError::Cycle`), or one the compile refused.
    pub fn route(&self) -> Option<Entity> {
        Entity::try_from_bits(self.refusal().route()?.0)
    }

    /// The target it names, when it names one.
    pub fn target(&self) -> Option<ParamRef> {
        ParamRef::from_key(self.refusal().target()?)
    }
}

impl std::fmt::Display for ModulationRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match (self.route(), self.target()) {
            (Some(route), _) => write!(f, "{} (route {route})", self.error),
            (None, Some(target)) => write!(
                f,
                "{} (entity {}, {:?})",
                self.error, target.entity, target.param
            ),
            (None, None) => self.error.fmt(f),
        }
    }
}

/// Everything the adapter reads off the world.
#[derive(SystemParam)]
pub struct Declared<'w, 's> {
    sources: Query<'w, 's, (Entity, &'static ModSource)>,
    routes: Query<'w, 's, (Entity, &'static ModRoute)>,
    ranges: Query<'w, 's, &'static ModRange>,
    automation: Query<'w, 's, (&'static Automation, Option<&'static AutomationTake>)>,
    nodes: Query<'w, 's, &'static AudioNode>,
    entities: &'w bevy_ecs::entity::Entities,
    #[cfg(feature = "plugin")]
    plugins: Query<'w, 's, &'static crate::plugin_host::PluginParamValues>,
}

/// The frame's declarations, owned.
struct Collected {
    sources: Vec<(SourceKey, Source)>,
    routes: Vec<(RouteKey, RouteDecl)>,
    targets: Vec<(TargetKey, TargetDecl)>,
}

impl Collected {
    fn declarations(&self) -> Declarations<'_> {
        Declarations {
            sources: &self.sources,
            routes: &self.routes,
            targets: &self.targets,
        }
    }
}

impl Declared<'_, '_> {
    fn alive(&self, entity: Entity) -> bool {
        self.entities.contains(entity)
    }

    /// Sources, routes and targets, keyed by entity bits. A route whose
    /// source or target entity is gone is left out, and so is a target on a
    /// gone entity: despawning a source (or a node) removes what it built.
    /// Two [`ModRange`]s, or two [`Automation`]s, on one target are both
    /// declared, and the compile refuses them
    /// (`CompileError::DuplicateTarget`; `tutti_session::declare_targets`).
    fn collect(&self) -> Collected {
        let mut sources: Vec<(SourceKey, Source)> = self
            .sources
            .iter()
            .map(|(e, s)| (SourceKey(e.to_bits()), s.kind.declare()))
            .collect();
        sources.sort_by_key(|(k, _)| *k);

        let targets = tutti_session::declare_targets(
            self.ranges
                .iter()
                .filter(|range| self.alive(range.param.entity))
                .map(|range| (range.param.key(), range.min, range.max)),
            self.automation
                .iter()
                .filter(|(automation, _)| self.alive(automation.target.entity))
                // An open take: the knob (the gesture) drives the param.
                .map(|(automation, take)| {
                    (
                        automation.target.key(),
                        take.is_none().then(|| automation.curve.clone()),
                    )
                }),
        );

        let mut routes: Vec<(RouteKey, RouteDecl)> = self
            .routes
            .iter()
            .filter(|(_, r)| {
                r.enabled && self.sources.contains(r.source) && self.alive(r.target.entity)
            })
            .map(|(e, r)| {
                (
                    RouteKey(e.to_bits()),
                    RouteDecl {
                        source: SourceKey(r.source.to_bits()),
                        to: RouteTarget::Param(r.target.key()),
                        depth: r.depth,
                        polarity: r.polarity,
                        curve: r.curve,
                    },
                )
            })
            .collect();
        routes.sort_by_key(|(k, _)| *k);

        Collected {
            sources,
            routes,
            targets,
        }
    }
}

impl Declared<'_, '_> {
    /// The node bound to `entity`.
    fn node_of(&self, entity: Entity) -> Option<AudioNode> {
        self.nodes.get(entity).ok().copied()
    }
}

/// The session's view of the app's keys while it applies (design 015
/// §4.6): every key this adapter mints is an entity's bits
/// (`Entity::to_bits`), decoded here and nowhere else.
impl tutti_session::ModKeys for Declared<'_, '_> {
    fn node(&self, target: u64) -> Option<AudioNode> {
        self.node_of(Entity::try_from_bits(target)?)
    }

    fn plugin_value(&self, target: u64, id: u32) -> Option<Option<f32>> {
        let entity = Entity::try_from_bits(target)?;
        #[cfg(feature = "plugin")]
        if let Ok(values) = self.plugins.get(entity) {
            return values.get(id);
        }
        // A node no plugin load mirrors (a host's own node taking parameter
        // events): no knob to start from.
        let _ = (entity, id);
        Some(None)
    }

    fn feed(&self, source: SourceKey) -> Option<Feed> {
        let (_, source) = self.sources.get(Entity::try_from_bits(source.0)?).ok()?;
        match source.kind.feed()? {
            ModFeed::Audio(entity) => Some(Feed::Audio(self.node_of(entity)?.key())),
            ModFeed::Events { entity, port } => Some(Feed::Events(EventOut {
                node: self.node_of(entity)?.key(),
                port,
            })),
        }
    }
}

/// Runs `phase` over the frame's declarations, keyed by entities, and
/// forwards a refusal as a [`ModulationRefused`] message.
fn run(
    phase: Phase,
    declared: &Declared<'_, '_>,
    graph: &mut AudioGraphRes,
    refused: &mut MessageWriter<ModulationRefused>,
) {
    let collected = declared.collect();
    let mods = ModInput {
        declarations: collected.declarations(),
        keys: declared,
    };
    for event in graph.run_phase_with_mods(phase, std::time::Instant::now(), mods) {
        if let SessionEvent::ModulationRefused(ModRefusal::Apply(error)) = event {
            refused.write(ModulationRefused { error });
        }
    }
}

/// Applies the frame's declarations (doc 014 §8.2): collects them, keyed by
/// entities, and runs the session's `Phase::Modulation` over them. A
/// program edit (a depth, a curve, a range, a macro's value, an automation
/// curve) republishes the lanes it changes and stages nothing; a new lane
/// or signal node, or a moved edge, is staged for the frame's commit.
///
/// Every frame: a frame that changes nothing republishes nothing and commits
/// nothing. A refusal ([`ModulationRefused`]) changes nothing in the graph.
pub fn reconcile(
    declared: Declared,
    graph: Option<ResMut<AudioGraphRes>>,
    mut refused: MessageWriter<ModulationRefused>,
) {
    // `engine_ready` reads a state a host may insert without an engine. What
    // the apply stages edits the session, which the frame's commit sends.
    let Some(mut graph) = graph else { return };
    run(Phase::Modulation, &declared, &mut graph, &mut refused);
}

/// After the frame's commit: the session's `Phase::AfterCommit` over the
/// frame's declarations, which re-applies while a target is unresolved
/// (its node was new, or rebound, before that commit), commits what that
/// built, and sends the resyncs owed. [`GraphLatency`] and
/// [`ChannelCompensation`] then describe the plan sent.
///
/// While the frame's own commit is still waiting (refused for now: the
/// session's edit flag is still set), the session re-applies nothing: its
/// nodes are not in a plan yet. A host's edit made through `AudioGraphRes`
/// after this frame's `commit_graph` sets the same flag, so it too waits
/// for the next frame's commit.
pub fn after_commit(
    _main: bevy_ecs::system::NonSendMarker,
    declared: Declared,
    graph: Option<ResMut<AudioGraphRes>>,
    mut refused: MessageWriter<ModulationRefused>,
    table: Option<ResMut<ChannelCompensation>>,
    total: Option<ResMut<GraphLatency>>,
) {
    let Some(mut graph) = graph else { return };
    // Nothing to re-apply or resync (the common frame): the phase would do
    // nothing, so the declarations are not collected. Mutations (run), each
    // fails a `mod_lanes` test: idle on no unresolved target alone
    // (`a_held_note_envelope_newly_routed_reads_its_level_at_once`: its
    // resync is never sent); idle on no resync alone
    // (`an_entity_rebound_to_another_param_list_keeps_modulating`, and
    // `a_re_apply_that_moves_the_latency_is_mirrored_on_that_frame`).
    let idle = {
        let session = graph.read();
        let state = session.mod_state();
        state.unresolved_targets().is_empty() && state.resyncs_owed() == 0
    };
    if idle {
        return;
    }
    run(Phase::AfterCommit, &declared, &mut graph, &mut refused);
    crate::graph::latency::mirror(&graph, table, total);
}
