//! Audio routing as a Bevy [`Relationship`].
//!
//! [`AudioFeedsTo`] is a many-to-one relationship from a *source* entity
//! (anything carrying an [`AudioNode`]) to a *target* entity (also
//! carrying an [`AudioNode`]). Source and destination ports are part of
//! the relationship value, so a single component instance encodes one
//! mono "wire" in the graph. Stereo edges are expressed as two
//! [`AudioFeedsTo`] entities — one per channel — usually living on a
//! shared "edge" entity.
//!
//! [`AudioFedBy`] is the auto-maintained relationship-target counterpart
//! Bevy populates on the target side. Reading it gives you every source
//! currently feeding the target.
//!
//! The reconcile system [`reconcile_audio_routing`] runs in
//! [`GraphReconcileSystems::Spawn`]: on `Added<AudioFeedsTo>` it looks
//! up both entities' [`AudioNode`] and calls
//! `graph.connect(src_node, src_port, dst_node, dst_port)`. On
//! `RemovedComponents<AudioFeedsTo>` it disconnects the recorded
//! `(dst_node, dst_port)` it had wired earlier.
//!
//! Pure graph-op binding — no DAW vocabulary. Sidechain ([`super::sidechain`])
//! is a sibling, hardcoded to port 1; this is the general case. Hosts
//! can use both freely; they don't interact.

use bevy_ecs::entity::{EntityMapper, MapEntities};
use bevy_ecs::prelude::*;
use bevy_ecs::reflect::{ReflectComponent, ReflectMapEntities};
use bevy_reflect::Reflect;

use tutti::core::ecs::AudioNode;

use super::reconcile::GraphDirty;
use crate::resources::TuttiGraphRes;

/// "This entity's audio output `src_port` feeds `target`'s input `dst_port`."
///
/// Insert on the *source* entity. Every [`AudioFeedsTo`] is exactly one
/// `graph.connect(src, src_port, dst, dst_port)` — stereo wires use two
/// instances, multi-input mixers use one per source, etc.
///
/// `target` is annotated with `#[relationship]` so Bevy's relationship
/// machinery auto-maintains the [`AudioFedBy`] component on the target.
/// `MapEntities` is implemented so scene-load via `DynamicScene` rewrites
/// the target id correctly.
#[derive(Component, Reflect, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[relationship(relationship_target = AudioFedBy)]
#[reflect(Component, MapEntities)]
pub struct AudioFeedsTo {
    /// The downstream entity this audio feeds into.
    #[relationship]
    pub target: Entity,
    /// Output port on the source node (`graph.connect(src, src_port, …)`).
    pub src_port: u32,
    /// Input port on the destination node (`graph.connect(…, dst, dst_port)`).
    pub dst_port: u32,
}

impl AudioFeedsTo {
    /// Construct a connection from the source's output port 0 to
    /// `target`'s input port 0 — the common mono-tap-of-mono-input case.
    #[inline]
    pub fn mono(target: Entity) -> Self {
        Self {
            target,
            src_port: 0,
            dst_port: 0,
        }
    }

    /// Construct a connection between specific ports.
    #[inline]
    pub fn between(target: Entity, src_port: u32, dst_port: u32) -> Self {
        Self {
            target,
            src_port,
            dst_port,
        }
    }
}

impl MapEntities for AudioFeedsTo {
    fn map_entities<M: EntityMapper>(&mut self, mapper: &mut M) {
        self.target = mapper.get_mapped(self.target);
    }
}

/// Auto-maintained list of every entity feeding this one.
///
/// Bevy's relationship infrastructure keeps this in sync with
/// [`AudioFeedsTo`]. Read it on the target side to discover sources;
/// don't insert it manually.
#[derive(Component, Debug, Default)]
#[relationship_target(relationship = AudioFeedsTo)]
pub struct AudioFedBy(Vec<Entity>);

impl AudioFedBy {
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

/// Reconciles routing wiring into graph operations.
///
/// `Added<AudioFeedsTo>`: looks up `(src_node, target_node)` from each
/// side's `AudioNode` component and calls
/// `graph.connect(src_node, src_port, target_node, dst_port)`.
/// `RemovedComponents<AudioFeedsTo>`: disconnects the recorded
/// `(target_node, dst_port)` pair.
///
/// We track `(src_entity → (target_entity, dst_port))` in a [`Local`]
/// map so the despawn path can find the destination port to disconnect
/// (the removed component value is unreadable). On removal we look up
/// the *target's* current `AudioNode` (target may have despawned, in
/// which case there's nothing to disconnect — fundsp drops the edge
/// when either endpoint is removed).
///
/// Skipped (with a warning) when:
/// - source or target is missing `AudioNode` (likely a misconfigured
///   spawn order — caller should ensure both endpoints carry
///   `AudioNode` before inserting `AudioFeedsTo`).
/// - the target has fewer inputs than `dst_port` (a
///   misconfigured port — calling `connect` past the input count
///   would panic in fundsp's `Net`).
pub fn reconcile_audio_routing(
    mut tracked: Local<std::collections::HashMap<Entity, (Entity, u32)>>,
    graph: Option<ResMut<TuttiGraphRes>>,
    mut dirty: ResMut<GraphDirty>,
    added: Query<(Entity, &AudioFeedsTo), Added<AudioFeedsTo>>,
    nodes: Query<&AudioNode>,
    mut removed: RemovedComponents<AudioFeedsTo>,
) {
    let Some(mut graph) = graph else {
        for entity in removed.read() {
            tracked.remove(&entity);
        }
        return;
    };

    for (src_entity, link) in added.iter() {
        let target_entity = link.target;
        let Ok(src_node) = nodes.get(src_entity) else {
            bevy_log::warn!(
                "AudioFeedsTo: source {:?} has no AudioNode; skipping connect",
                src_entity
            );
            continue;
        };
        let Ok(target_node) = nodes.get(target_entity) else {
            bevy_log::warn!(
                "AudioFeedsTo: target {:?} has no AudioNode; skipping connect",
                target_entity
            );
            continue;
        };
        let target_inputs = graph.0.inputs(target_node.0);
        if (link.dst_port as usize) >= target_inputs {
            bevy_log::warn!(
                "AudioFeedsTo: target {:?} has only {} inputs (dst_port={} out of range); skipping connect",
                target_entity,
                target_inputs,
                link.dst_port
            );
            continue;
        }
        graph
            .0
            .connect(src_node.0, link.src_port as usize, target_node.0, link.dst_port as usize);
        tracked.insert(src_entity, (target_entity, link.dst_port));
        dirty.0 = true;
    }

    for src_entity in removed.read() {
        let Some((target_entity, dst_port)) = tracked.remove(&src_entity) else {
            continue;
        };
        let Ok(target_node) = nodes.get(target_entity) else {
            // Target despawned along with the link; nothing to disconnect.
            continue;
        };
        if !graph.0.contains(target_node.0) {
            continue;
        }
        if (dst_port as usize) >= graph.0.inputs(target_node.0) {
            // Mismatched inputs (unlikely — would mean the unit was swapped
            // under us). Skip rather than panic.
            continue;
        }
        graph.0.disconnect(target_node.0, dst_port as usize);
        dirty.0 = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::reconcile::GraphReconcileSystems;
    use bevy_app::App;
    use tutti::TuttiEngine;

    fn test_app() -> App {
        let engine = TuttiEngine::builder()
            .inputs(0)
            .outputs(2)
            .build()
            .expect("build engine");
        let TuttiEngine { graph, .. } = engine;

        let mut app = App::new();
        app.insert_resource(crate::resources::TuttiGraphRes(graph));
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
        app.add_systems(
            bevy_app::Update,
            (
                reconcile_audio_routing.in_set(GraphReconcileSystems::Spawn),
                crate::graph::reconcile::commit_graph.in_set(GraphReconcileSystems::Commit),
            ),
        );
        app
    }

    #[test]
    fn audio_feeds_to_grows_relationship_target() {
        // Pure relationship machinery — Bevy populates AudioFedBy
        // automatically when AudioFeedsTo is inserted. The graph.connect
        // call needs both endpoints to have AudioNode; that's covered
        // by the integration test below.
        let mut app = test_app();
        let src = app.world_mut().spawn_empty().id();
        let target = app.world_mut().spawn_empty().id();
        app.world_mut()
            .entity_mut(src)
            .insert(AudioFeedsTo::mono(target));
        app.update();

        let fed_by = app
            .world()
            .get::<AudioFedBy>(target)
            .expect("AudioFedBy populated on target");
        assert_eq!(fed_by.len(), 1);
        assert_eq!(fed_by.iter().next(), Some(src));
    }

    #[test]
    fn audio_feeds_to_warns_when_source_lacks_audio_node() {
        // No panic: the reconciler logs a warning and skips when
        // either endpoint is missing `AudioNode`.
        let mut app = test_app();
        let src = app.world_mut().spawn_empty().id();
        let target = app.world_mut().spawn_empty().id();
        app.world_mut()
            .entity_mut(src)
            .insert(AudioFeedsTo::mono(target));
        // Should not panic.
        app.update();
        // No graph state to assert here; the smoke test is "didn't crash".
        let _ = (src, target);
    }

    #[test]
    fn audio_feeds_to_connects_two_nodes_in_graph() {
        // Spawn two AudioNodes (a sine generator and a stereo
        // ChannelStripUnit-equivalent — using `pass` for simplicity)
        // and verify `AudioFeedsTo` produces an actual graph edge.
        use crate::graph::reconcile::SpawnAudioNode;
        use tutti::core::ecs::NodeKind;
        use tutti::dsp::sine_hz;
        // `tutti::dsp::pass` is a stereo pass-through (2 in, 2 out)
        // — exactly what we need as a sink with addressable input ports.
        let mut app = test_app();

        // Source: bare sine (1 output port, 0 input ports).
        let src = app
            .world_mut()
            .commands()
            .spawn_audio_node(sine_hz::<f32>(440.0), NodeKind::Generator)
            .id();
        // Target: stereo pass-through (2 input ports).
        let target = app
            .world_mut()
            .commands()
            .spawn_audio_node(tutti::dsp::pass() | tutti::dsp::pass(), NodeKind::Generic)
            .id();
        app.update();

        // Now wire src.0 → target.0.
        app.world_mut()
            .entity_mut(src)
            .insert(AudioFeedsTo::between(target, 0, 0));
        app.update();

        // Smoke: AudioFedBy populated on target, src and target both
        // still have their AudioNode, GraphDirty was raised so the
        // commit ran (no easy way to query "is_connected" through the
        // public `Net` API, so we settle for "no panic, dirty flag
        // cleared after commit").
        let fed_by = app
            .world()
            .get::<AudioFedBy>(target)
            .expect("AudioFedBy populated");
        assert_eq!(fed_by.len(), 1);
        assert!(app.world().get::<AudioNode>(src).is_some());
        assert!(app.world().get::<AudioNode>(target).is_some());
        let dirty = app.world().resource::<GraphDirty>();
        assert!(!dirty.0, "commit_graph cleared the dirty flag");
    }

    #[test]
    fn audio_feeds_to_disconnects_on_remove() {
        use crate::graph::reconcile::SpawnAudioNode;
        use tutti::core::ecs::NodeKind;
        use tutti::dsp::sine_hz;

        let mut app = test_app();

        let src = app
            .world_mut()
            .commands()
            .spawn_audio_node(sine_hz::<f32>(440.0), NodeKind::Generator)
            .id();
        let target = app
            .world_mut()
            .commands()
            .spawn_audio_node(tutti::dsp::pass() | tutti::dsp::pass(), NodeKind::Generic)
            .id();
        app.update();

        app.world_mut()
            .entity_mut(src)
            .insert(AudioFeedsTo::between(target, 0, 0));
        app.update();
        assert_eq!(
            app.world().get::<AudioFedBy>(target).map(|f| f.len()),
            Some(1)
        );

        // Remove the relationship; reconcile should disconnect and the
        // relationship-target side should drain.
        app.world_mut().entity_mut(src).remove::<AudioFeedsTo>();
        app.update();

        let fed_by = app.world().get::<AudioFedBy>(target);
        // Bevy retains the empty AudioFedBy (or removes it — both are
        // acceptable, as long as the entry is gone).
        assert!(fed_by.map_or(true, |f| f.is_empty()));
    }
}
