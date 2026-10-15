//! The adapter: the frame's declarations, collected from components and
//! applied to the graph by `tutti_nodes::Modulation` (doc 014 §8.2).
//!
//! [`reconcile`] runs after [`GraphReconcileSystems::Spawn`], so a node
//! spawned this frame has its `AudioNode`, and before the frame's commit,
//! which sends what the apply staged. A native target resolves only once
//! its node is in a committed plan, so a target on a node new this frame is
//! unresolved at first: [`after_commit`] re-applies right after the commit
//! while any target is unresolved, and commits again, so the lane plays from
//! the frame its node does. It also sends the note-envelope resyncs the
//! apply owes, which wait for the commit that placed their nodes.

use std::collections::BTreeMap;

use bevy_ecs::message::{Message, MessageWriter};
use bevy_ecs::prelude::*;
use bevy_ecs::system::SystemParam;

use tutti_core::AudioNode;
use tutti_graph::EventOut;
use tutti_mod::{
    Declarations, RouteDecl, RouteKey, RouteTarget, Source, SourceKey, TargetDecl, TargetKey,
};
use tutti_nodes::automation::LaneControls;
use tutti_nodes::{Feed, Modulation, ReconcileError};
use tutti_types::NodeKey;

use super::declare::{
    Automation, AutomationTake, ModFeed, ModRange, ModRoute, ModSource, ParamRef,
};
use crate::graph::{AudioGraphRes, GraphDirty};

/// The app's modulation as applied: the reconciler, and what its last apply
/// left unresolved or refused.
#[derive(Resource, Default)]
pub struct ModulationState {
    modulation: Modulation,
    refusal: Option<ReconcileError>,
    unresolved_targets: Vec<TargetKey>,
    unresolved_sources: Vec<SourceKey>,
}

impl ModulationState {
    /// The reconciler (its lanes, sources and last compile).
    pub fn modulation(&self) -> &Modulation {
        &self.modulation
    }

    /// Why the declarations are refused now, `None` while they apply. The
    /// graph keeps what the last apply that succeeded built.
    pub fn refusal(&self) -> Option<&ReconcileError> {
        self.refusal.as_ref()
    }

    /// `target`'s lane: its node and its controls (the knob, the range, the
    /// counters). `None` while it has none.
    pub fn lane(&self, target: ParamRef) -> Option<(NodeKey, &LaneControls)> {
        self.modulation.lane(target.key())
    }

    /// Writes `value` into `target`'s knob: for a plugin parameter with a
    /// lane, the lane's own knob; for a native param, the node's cell the
    /// lane reads (as [`write_param`](crate::graph::write_param) does).
    /// `false` when `target` has no lane with a knob.
    pub fn set_knob(&self, target: ParamRef, value: f32) -> bool {
        self.lane(target)
            .is_some_and(|(_, controls)| controls.set_knob(value))
    }

    /// Signal source `source`'s node (a follower's, a note envelope's).
    pub fn source_node(&self, source: Entity) -> Option<NodeKey> {
        self.modulation.source(SourceKey(source.to_bits()))
    }

    /// The targets the last apply could not resolve: their node is not in
    /// the graph (or not yet committed), or a plugin's values are not read
    /// yet.
    pub fn unresolved_targets(&self) -> impl Iterator<Item = ParamRef> + '_ {
        self.unresolved_targets
            .iter()
            .filter_map(|&k| ParamRef::from_key(k))
    }

    /// The followers the last apply could not feed.
    pub fn unresolved_sources(&self) -> impl Iterator<Item = Entity> + '_ {
        self.unresolved_sources
            .iter()
            .filter_map(|s| Entity::try_from_bits(s.0))
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
    /// The route entity it names: a route whose follower is downstream of
    /// its own target (`ReconcileError::Cycle`), or one the compile refused.
    pub fn route(&self) -> Option<Entity> {
        use tutti_mod::CompileError as C;
        let key = match &self.error {
            ReconcileError::Cycle { route } => *route,
            ReconcileError::Compile(
                C::DuplicateRoute(route)
                | C::BadRoute(route)
                | C::UnknownSource { route }
                | C::UnknownRoute { route }
                | C::UndeclaredTarget { route }
                | C::DepthByChain { route }
                | C::DepthBySignal { route }
                | C::TwoDepthBy { route },
            ) => *route,
            _ => return None,
        };
        Entity::try_from_bits(key.0)
    }

    /// The target it names, when it names one.
    pub fn target(&self) -> Option<ParamRef> {
        use tutti_mod::CompileError as C;
        match &self.error {
            ReconcileError::WrongTarget(t)
            | ReconcileError::NoSuchParam(t)
            | ReconcileError::NoSuchEventInput(t)
            | ReconcileError::PortTaken(t)
            | ReconcileError::SharedPort { first: t, .. }
            | ReconcileError::Compile(
                C::DuplicateTarget(t)
                | C::NanRange(t)
                | C::LiveCurve(t)
                | C::LayerIdCollision(t)
                | C::TooManyLayers { target: t, .. }
                | C::TooManyInputs { target: t, .. },
            ) => ParamRef::from_key(*t),
            _ => None,
        }
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

/// What the adapter asks of the app's entities while it applies: the
/// `ModHost` seam, in entities (`AudioGraphRes::modulate` turns it into one).
pub(crate) trait ModEntities {
    /// The node bound to `entity`.
    fn node(&self, entity: Entity) -> Option<AudioNode>;
    /// Plugin parameter `id`'s value on `entity`: `None` while its values
    /// are not read yet, `Some(None)` for a parameter it does not list.
    fn plugin_value(&self, entity: Entity, id: u32) -> Option<Option<f32>>;
    /// What feeds source `source`'s node.
    fn feed(&self, source: Entity) -> Option<Feed>;
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

impl Declared<'_, '_> {
    fn alive(&self, entity: Entity) -> bool {
        self.entities.contains(entity)
    }

    /// Sources, routes and targets, keyed by entity bits. A route whose
    /// source or target entity is gone is left out, and so is a target on a
    /// gone entity: despawning a source (or a node) removes what it built.
    fn collect(&self) -> Collected {
        let mut sources: Vec<(SourceKey, Source)> = self
            .sources
            .iter()
            .map(|(e, s)| (SourceKey(e.to_bits()), s.kind.declare()))
            .collect();
        sources.sort_by_key(|(k, _)| *k);

        let mut targets: BTreeMap<TargetKey, TargetDecl> = BTreeMap::new();
        for range in &self.ranges {
            if !self.alive(range.param.entity) {
                continue;
            }
            let decl = targets.entry(range.param.key()).or_insert(TargetDecl {
                min: range.min,
                max: range.max,
                automation: None,
            });
            decl.min = range.min;
            decl.max = range.max;
        }
        for (automation, take) in &self.automation {
            if !self.alive(automation.target.entity) {
                continue;
            }
            let decl = targets
                .entry(automation.target.key())
                .or_insert(TargetDecl {
                    min: f32::NEG_INFINITY,
                    max: f32::INFINITY,
                    automation: None,
                });
            // An open take: the knob (the gesture) drives the param.
            if take.is_none() {
                decl.automation = Some(automation.curve.clone());
            }
        }

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
            targets: targets.into_iter().collect(),
        }
    }
}

impl ModEntities for Declared<'_, '_> {
    fn node(&self, entity: Entity) -> Option<AudioNode> {
        self.nodes.get(entity).ok().copied()
    }

    fn plugin_value(&self, entity: Entity, id: u32) -> Option<Option<f32>> {
        #[cfg(feature = "plugin")]
        if let Ok(values) = self.plugins.get(entity) {
            return values.get(id);
        }
        // A node no plugin load mirrors (a host's own node taking parameter
        // events): no knob to start from.
        let _ = (entity, id);
        Some(None)
    }

    fn feed(&self, source: Entity) -> Option<Feed> {
        let (_, source) = self.sources.get(source).ok()?;
        match source.kind.feed()? {
            ModFeed::Audio(entity) => Some(Feed::Audio(self.node(entity)?.key())),
            ModFeed::Events { entity, port } => Some(Feed::Events(EventOut {
                node: self.node(entity)?.key(),
                port,
            })),
        }
    }
}

/// One apply: the frame's declarations onto the graph. Returns whether it
/// asked for a commit.
fn apply(
    declared: &Declared<'_, '_>,
    graph: &mut AudioGraphRes,
    state: &mut ModulationState,
    refused: &mut MessageWriter<ModulationRefused>,
) -> bool {
    let collected = declared.collect();
    let decl = Declarations {
        sources: &collected.sources,
        routes: &collected.routes,
        targets: &collected.targets,
    };
    match graph.modulate(&mut state.modulation, &decl, declared) {
        Ok(applied) => {
            state.refusal = None;
            state.unresolved_targets = applied.unresolved_targets;
            state.unresolved_sources = applied.unresolved_sources;
            applied.commit || applied.resync
        }
        Err(error) => {
            if state.refusal.as_ref() != Some(&error) {
                let message = ModulationRefused { error };
                bevy_log::error!("modulation refused: {message}");
                state.refusal = Some(message.error.clone());
                refused.write(message);
            }
            false
        }
    }
}

/// Applies the frame's declarations (doc 014 §8.2): collects them, keyed by
/// entities, and hands them to `Modulation::apply` on the graph's editor.
/// A program edit (a depth, a curve, a range, a macro's value, an automation
/// curve) republishes the lanes it changes and stages nothing; a new lane or
/// signal node, or a moved edge, is staged for the frame's commit.
///
/// Every frame: a frame that changes nothing republishes nothing and commits
/// nothing. A refusal ([`ModulationRefused`]) changes nothing in the graph.
pub fn reconcile(
    declared: Declared,
    graph: Option<ResMut<AudioGraphRes>>,
    mut state: ResMut<ModulationState>,
    mut dirty: ResMut<GraphDirty>,
    mut refused: MessageWriter<ModulationRefused>,
) {
    // `engine_ready` reads a state a host may insert without an engine.
    let Some(mut graph) = graph else { return };
    if apply(&declared, &mut graph, &mut state, &mut refused) {
        dirty.0 = true;
    }
}

/// After the frame's commit: re-applies while a target is unresolved (its
/// node was new, or rebound, before that commit) and commits what that
/// built, then sends the resyncs owed.
pub fn after_commit(
    _main: bevy_ecs::system::NonSendMarker,
    declared: Declared,
    graph: Option<ResMut<AudioGraphRes>>,
    mut state: ResMut<ModulationState>,
    mut dirty: ResMut<GraphDirty>,
    mut refused: MessageWriter<ModulationRefused>,
) {
    let Some(mut graph) = graph else { return };
    // Nothing to resolve while the frame's own commit waits (refused for
    // now): its nodes are not in a plan yet.
    if !state.unresolved_targets.is_empty() && !dirty.0 {
        if graph.has_uncommitted_edit() {
            // A node staged outside the frame's commit (a host's own insert
            // through `AudioGraphRes`): a target resolves only from a
            // committed plan, so commit it, and re-apply next frame.
            if !graph.commit() {
                dirty.0 = true;
            }
        } else if apply(&declared, &mut graph, &mut state, &mut refused) && !graph.commit() {
            dirty.0 = true;
        }
    }
    if state.modulation.resyncs_owed() > 0 && !dirty.0 {
        graph.send_mod_resyncs(&mut state.modulation);
    }
}
