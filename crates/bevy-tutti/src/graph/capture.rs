//! Capturing a node's controls on the way into the graph.
//!
//! Some of what a host does to a node is not audio routing and not a scalar
//! param: minting a modulation accumulator over a filter's cutoff, binding a
//! hosted plugin's meter. (MIDI is neither: it travels on event edges.) Each
//! needs the *concrete* node, and the graph holds only boxed nodes it owns
//! outright.
//!
//! So the question is asked **once, of the owned node, before it is
//! inserted**, and the answer is kept on the entity:
//!
//! | Component | Captured from | Read by |
//! |---|---|---|
//! | `ModParamsHandle` (`modulation`) | a node's `ParamSet` ([`CapturedControls::for_params`]), or a host-supplied target (`ModTargetRegistry::insert_target`) | the modulation resolver |
//! | `PluginShadow` (`plugin`) | a loaded `PluginClient` (`CapturedControls::for_plugin`) | plugin meter bind and latency poll |
//!
//! Every one holds only state the node shares with the graph's copy (its
//! `Param` cells, the plugin's controls), so the component reaches the
//! running node without the graph. Each also records the node's `AudioNode`
//! key it was captured for, and its reader ignores it when that is not the entity's
//! current [`AudioNode`] — a leftover from a node that was replaced by hand is
//! inert rather than stale.
//!
//! **That guard is `AudioNode` key equality and nothing more.** It catches a
//! different node bound to the entity; it cannot see a node replaced *under the
//! same key*.
//! `crossfade_audio_node` is such a replacement and re-captures, so it is safe;
//! a host that calls [`AudioGraphRes::replace`](crate::graph::AudioGraphRes::replace)
//! directly bypasses both the capture and the guard, and the entity keeps
//! driving the outgoing node's controls. Go through `crossfade_audio_node`, or
//! capture ([`GraphNode::captured`](crate::graph::GraphNode::captured)) and
//! [`bind`](CapturedControls::bind) yourself.
//!
//! When the node goes (its `AudioNode` is removed, or the entity despawned) the
//! captured components go with it — `drop_captured` runs from the removal
//! observer.
//!
//! Every node-insertion path in this crate runs the capture:
//! [`spawn_audio_node`](crate::graph::SpawnAudioNode),
//! [`insert_audio_node`](crate::graph::InsertAudioNode),
//! [`crossfade_audio_node`](crate::graph::crossfade_audio_node) (which replaces
//! the node, so re-captures), the soundfont promotion and the plugin load. A
//! host that inserts a node itself and binds `AudioNode` by hand does the same
//! with [`GraphNode::captured`](crate::graph::GraphNode::captured) and
//! [`bind`](CapturedControls::bind).

use bevy_ecs::prelude::*;

use tutti_core::{AudioNode, NodeKey};

/// The controls captured from one node, before it moved into the graph.
///
/// Empty for a node with no params and no plugin (an oscillator, a sum, a
/// width adapter).
#[must_use = "captured controls do nothing until they are bound to the entity"]
#[derive(Default)]
pub struct CapturedControls {
    #[cfg(feature = "modulation")]
    params: Option<std::sync::Arc<dyn tutti_mod::ModParams + Send + Sync>>,
    #[cfg(feature = "plugin")]
    plugin: Option<tutti_plugin::handles::PluginControls>,
}

impl CapturedControls {
    /// The controls of a loaded out-of-process plugin, captured from the
    /// unbound client before it is bound and inserted
    /// ([`AudioGraphRes::insert_plugin`](crate::graph::AudioGraphRes::insert_plugin)):
    /// its [`PluginControls`](tutti_plugin::handles::PluginControls) as the
    /// entity's `PluginShadow`.
    ///
    #[cfg(feature = "plugin")]
    pub fn for_plugin(client: &tutti_plugin::handles::PluginClient) -> Self {
        Self {
            #[cfg(feature = "modulation")]
            params: None,
            plugin: Some(client.controls()),
        }
    }

    /// The controls of a node whose params are a
    /// [`ParamSet`](tutti_graph::ParamSet): with `modulation`, a
    /// `ModParamsHandle` over its cells, so a route resolves on any of its
    /// params — the set already addresses them.
    /// What a [`GraphNode`](crate::graph::GraphNode) with params returns
    /// from `captured`.
    pub fn for_params(params: &tutti_graph::ParamSet) -> Self {
        let _ = params;
        Self {
            #[cfg(feature = "modulation")]
            params: Some(std::sync::Arc::new(crate::modulation::ParamSetTargets(
                params.clone(),
            ))),
            #[cfg(feature = "plugin")]
            plugin: None,
        }
    }

    /// Bind `entity` to `node`: insert [`AudioNode`] and every captured control.
    ///
    /// The whole binding in one step, which is how every insertion path in this
    /// crate forms it.
    pub fn bind(self, entity: &mut EntityWorldMut, node: AudioNode) {
        entity.insert(node);
        self.replace(entity, node);
    }

    /// Replace the entity's captured controls with these, keeping its
    /// [`AudioNode`] — the crossfade case, where the node changes under a
    /// surviving handle.
    ///
    /// A control this node does not have is **removed**: the old node's
    /// params would otherwise stay reachable under the new node's id.
    ///
    /// The plugin binding latches are cleared too. `PluginMeterBound` and
    /// `PluginParamsBound` say "*this* plugin has its meter and automation
    /// installed", and the incoming `PluginClient` has neither: left in place
    /// they would stop the binding systems from ever installing them.
    pub(crate) fn replace(self, entity: &mut EntityWorldMut, node: AudioNode) {
        let node: NodeKey = node.0;
        let _ = (&entity, node);
        #[cfg(feature = "plugin")]
        {
            entity.remove::<crate::plugin_host::PluginMeterBound>();
            #[cfg(feature = "modulation")]
            entity.remove::<crate::plugin_host::PluginParamsBound>();
        }
        #[cfg(feature = "modulation")]
        match self.params {
            Some(params) => {
                entity.insert(crate::modulation::ModParamsHandle::new(node, params));
            }
            None => {
                entity.remove::<crate::modulation::ModParamsHandle>();
            }
        }
        #[cfg(feature = "plugin")]
        match self.plugin {
            Some(controls) => {
                entity.insert(crate::plugin_host::PluginShadow::new(node, controls));
            }
            None => {
                entity.remove::<crate::plugin_host::PluginShadow>();
            }
        }
    }
}

/// Remove every captured control from `entity`, for when its node is gone.
///
/// Called from the `On<Remove, AudioNode>` observer
/// ([`reconcile_node_despawn`](crate::graph::reconcile_node_despawn)), which
/// covers a despawn, a host taking `AudioNode` off, and the dead-plugin
/// teardown in `plugin_host::health`. `try_remove`, because on a despawn the
/// entity is gone by the time the command applies, and that is fine.
pub(crate) fn drop_captured(commands: &mut Commands, entity: Entity) {
    let Ok(mut e) = commands.get_entity(entity) else {
        return;
    };
    let _ = &mut e;
    #[cfg(feature = "modulation")]
    e.try_remove::<crate::modulation::ModParamsHandle>();
    #[cfg(feature = "plugin")]
    e.try_remove::<crate::plugin_host::PluginShadow>();
}
