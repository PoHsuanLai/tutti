//! Sidechain wiring as a Bevy [`Relationship`].
//!
//! [`SidechainOf`] is a one-to-one relationship from a *source* entity
//! (the audio that drives the sidechain) to a *target* entity (the
//! compressor / gate / whatever exposes a sidechain input bus). The
//! relationship carries the target input-port index where that bus begins
//! (`= main_input_channels`); a stereo-main plugin's sidechain is port `2`,
//! a plain two-input effect's is port `1`.
//! [`SidechainSources`] is its automatic relationship-target counterpart
//! on the target side.
//!
//! The add system [`reconcile_sidechain_links`] runs in
//! [`GraphReconcileSystems::Spawn`]: on `Added<SidechainOf>` it looks up
//! both entities' [`AudioNode`] and calls
//! `graph.connect(src_node, 0, target_node, port)`. Removal is handled by
//! the [`reconcile_sidechain_remove`] observer (`On<Remove, SidechainOf>`),
//! which disconnects the same port.
//!
//! Pure graph-op binding — no DAW vocabulary. The DAW concept of "this
//! compressor's sidechain follows this kick drum's bus" is built on
//! top of this primitive in dawai/mixer.

use bevy_ecs::prelude::*;

use crate::graph::AudioNode;

use super::reconcile::GraphDirty;
use crate::graph::AudioGraphRes;

/// "This entity's audio drives `target`'s sidechain input bus."
///
/// Insert on the *source* entity. The target side automatically grows a
/// [`SidechainSources`] component listing every source pointing at it.
///
/// `port` is the target's input-port index where the sidechain bus begins.
/// In the flat port layout a multi-bus plugin node exposes, the sidechain
/// bus's first port sits at `main_input_channels`, **not** a fixed `1` —
/// a stereo-main plugin has its sidechain at port `2`. The caller knows
/// the target's main input width (from the plugin's reported bus layout)
/// and declares it here. Use [`SidechainOf::new`] for the common case
/// of a port computed from the main width, or [`SidechainOf::at_port_one`]
/// for a plain two-input effect whose second port *is* the sidechain.
#[derive(Component, Debug, Clone, Copy)]
#[relationship(relationship_target = SidechainSources)]
pub struct SidechainOf {
    /// The target entity exposing the sidechain input.
    #[relationship]
    pub target: Entity,
    /// Input-port index on the target where the sidechain bus begins
    /// (= the target node's main input channel count).
    pub port: usize,
}

impl SidechainOf {
    /// Sidechain into `target`, with the sidechain bus beginning at
    /// `port` (the target's main input channel count).
    #[inline]
    pub fn new(target: Entity, port: usize) -> Self {
        Self { target, port }
    }

    /// Sidechain into a plain two-input effect: the second port (`1`) is
    /// the sidechain. Equivalent to `new(target, 1)`.
    #[inline]
    pub fn at_port_one(target: Entity) -> Self {
        Self { target, port: 1 }
    }

    /// The target entity.
    #[inline]
    pub fn target(self) -> Entity {
        self.target
    }
}

/// Auto-maintained list of every entity sidechained into this one.
///
/// Bevy's relationship infrastructure keeps this in sync with
/// [`SidechainOf`]. Read it on the target side to discover sources;
/// don't insert it manually.
#[derive(Component, Debug, Default)]
#[relationship_target(relationship = SidechainOf)]
pub struct SidechainSources(Vec<Entity>);

impl SidechainSources {
    pub fn iter(&self) -> impl Iterator<Item = Entity> + '_ {
        self.0.iter().copied()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }
}

/// Reconciles new sidechain wiring into graph operations (add half).
///
/// `Added<SidechainOf>`: looks up `(src_node, target_node)` from each side's
/// `AudioNode` component and calls `graph.connect(src_node, 0, target_node,
/// port)`, where `port` is the sidechain bus's first input index declared on
/// the relationship (the target's main input channel count).
///
/// Stays a [`GraphReconcileSystems::Spawn`]-set system because it needs both
/// endpoints' `AudioNode` to exist this frame. The *removal* half lives in
/// [`reconcile_sidechain_remove`], an `On<Remove, SidechainOf>` observer.
pub fn reconcile_sidechain_links(
    mut graph: ResMut<AudioGraphRes>,
    mut dirty: ResMut<GraphDirty>,
    added: Query<(Entity, &SidechainOf), Added<SidechainOf>>,
    nodes: Query<&AudioNode>,
) {
    for (src_entity, link) in added.iter() {
        let target_entity = link.target;
        let port = link.port;
        let Ok(src_node) = nodes.get(src_entity) else {
            bevy_log::warn!(
                "SidechainOf: source {:?} has no AudioNode; skipping connect",
                src_entity
            );
            continue;
        };
        let Ok(target_node) = nodes.get(target_entity) else {
            bevy_log::warn!(
                "SidechainOf: target {:?} has no AudioNode; skipping connect",
                target_entity
            );
            continue;
        };
        // The sidechain port must be a real input index on the target.
        // Bare oscillators / generators (0 inputs) and effects whose input
        // width doesn't reach `port` have no such port; calling connect on
        // them panics inside fundsp's Net. Skip with a warning so
        // misconfigured wiring is loud but not fatal.
        let target_inputs = graph.0.inputs(target_node.0);
        if target_inputs <= port {
            bevy_log::warn!(
                "SidechainOf: target {:?} has {} inputs but sidechain port is {} (needs > {}); skipping connect",
                target_entity,
                target_inputs,
                port,
                port
            );
            continue;
        }
        graph.0.connect(src_node.0, 0, target_node.0, port);
        dirty.0 = true;
    }
}

/// Observer: tears down a sidechain edge when its `SidechainOf` is removed
/// (including via despawn).
///
/// `On<Remove, SidechainOf>` fires *before* the value is dropped, so the
/// `{target, port}` it carried is still readable off the source entity — no
/// local `(src, (target, port))` map needed. Reads the target's `AudioNode`
/// and calls `graph.disconnect(target_node, port)`. Only mutates the graph +
/// sets `GraphDirty`; the per-frame `commit_graph` (Commit phase) commits.
pub fn reconcile_sidechain_remove(
    remove: On<Remove, SidechainOf>,
    links: Query<&SidechainOf>,
    nodes: Query<&AudioNode>,
    graph: Option<ResMut<AudioGraphRes>>,
    mut dirty: ResMut<GraphDirty>,
) {
    let src_entity = remove.event_target();
    let Ok(link) = links.get(src_entity) else {
        return;
    };
    let Some(mut graph) = graph else { return };

    let Ok(target_node) = nodes.get(link.target) else {
        // Target despawned along with the link; nothing to disconnect.
        return;
    };
    if graph.0.inputs(target_node.0) <= link.port {
        // We never connected (target had no such input); nothing to undo.
        return;
    }
    graph.0.disconnect(target_node.0, link.port);
    dirty.0 = true;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::reconcile::GraphReconcileSystems;
    use crate::AudioGraph;
    use crate::{GraphNet, PdcManager};
    use bevy_app::App;

    fn bare_graph(channels: usize) -> AudioGraph {
        let mut net = GraphNet::new(0, channels);
        let _backend = net.backend();
        let pdc = PdcManager::new(channels, 0);
        let midi_route = tutti_midi_types::MidiRoutingTable::new();
        AudioGraph::from_parts(net, pdc, midi_route, 48_000.0, channels)
    }

    fn test_app() -> App {
        let mut app = App::new();
        app.insert_resource(crate::graph::AudioGraphRes(bare_graph(2)));
        app.init_resource::<GraphDirty>();
        app.configure_sets(
            bevy_app::Update,
            (
                GraphReconcileSystems::Spawn,
                GraphReconcileSystems::Params,
                GraphReconcileSystems::Despawn,
                GraphReconcileSystems::Commit,
            )
                .chain(),
        );
        app.add_observer(reconcile_sidechain_remove);
        app.add_systems(
            bevy_app::Update,
            (
                reconcile_sidechain_links.in_set(GraphReconcileSystems::Spawn),
                crate::graph::reconcile::commit_graph.in_set(GraphReconcileSystems::Commit),
            ),
        );
        app
    }

    #[test]
    fn sidechain_link_grows_relationship_target() {
        // We exercise only Bevy's relationship machinery here; the actual
        // graph.connect call is verified by the integration example, which
        // spawns a node that *has* an input port 1 (e.g. a compressor).
        // Bare oscillators have zero inputs, so connecting to port 1 panics.
        let mut app = test_app();

        let src = app.world_mut().spawn_empty().id();
        let target = app.world_mut().spawn_empty().id();
        app.world_mut()
            .entity_mut(src)
            .insert(SidechainOf::at_port_one(target));
        app.update();

        let sources = app
            .world()
            .get::<SidechainSources>(target)
            .expect("SidechainSources");
        assert_eq!(sources.len(), 1);
        assert_eq!(sources.iter().next(), Some(src));
    }

    #[test]
    fn sidechain_connects_at_declared_port_not_just_one() {
        // A stereo-main plugin exposes its sidechain bus at port 2, not 1.
        // Build a 3-input target (three stacked passes = 3 in / 3 out) and
        // sidechain into port 2; the reconcile must connect there without
        // panicking and clear the dirty flag via commit.
        use crate::dsp::sine_hz;
        use crate::graph::reconcile::SpawnAudioNode;
        use crate::graph::NodeKind;
        let mut app = test_app();

        let src = app
            .world_mut()
            .commands()
            .spawn_audio_node(sine_hz::<f32>(440.0), NodeKind::Generator)
            .id();
        // 3-input target: ports 0,1 = stereo main, port 2 = sidechain.
        let target = app
            .world_mut()
            .commands()
            .spawn_audio_node(
                crate::dsp::pass() | crate::dsp::pass() | crate::dsp::pass(),
                NodeKind::Generic,
            )
            .id();
        app.update();

        app.world_mut()
            .entity_mut(src)
            .insert(SidechainOf::new(target, 2));
        app.update();

        let sources = app
            .world()
            .get::<SidechainSources>(target)
            .expect("SidechainSources");
        assert_eq!(sources.len(), 1);
        // commit ran (connect at port 2 succeeded, no panic).
        assert!(
            !app.world().resource::<GraphDirty>().0,
            "commit cleared dirty flag"
        );

        // Removing the link disconnects the same port — also no panic.
        app.world_mut().entity_mut(src).remove::<SidechainOf>();
        app.update();
        assert!(
            app.world()
                .get::<SidechainSources>(target)
                .map(|s| s.len())
                .unwrap_or(0)
                == 0
        );
    }

    #[test]
    fn sidechain_skips_when_port_out_of_range() {
        // Target has only 2 inputs (ports 0,1); declaring a sidechain at
        // port 2 must be skipped with a warning, never connected (would
        // panic in fundsp's Net), and must not raise the dirty flag.
        use crate::dsp::sine_hz;
        use crate::graph::reconcile::SpawnAudioNode;
        use crate::graph::NodeKind;
        let mut app = test_app();

        let src = app
            .world_mut()
            .commands()
            .spawn_audio_node(sine_hz::<f32>(440.0), NodeKind::Generator)
            .id();
        let target = app
            .world_mut()
            .commands()
            .spawn_audio_node(crate::dsp::pass() | crate::dsp::pass(), NodeKind::Generic)
            .id();
        app.update();

        // port 2 is out of range for a 2-input node.
        app.world_mut()
            .entity_mut(src)
            .insert(SidechainOf::new(target, 2));
        app.update();

        // Relationship target still grows (Bevy machinery), but no graph
        // edge was made, so the dirty flag stays clear.
        assert!(
            !app.world().resource::<GraphDirty>().0,
            "out-of-range port must not connect or dirty the graph"
        );
    }
}
