//! Capturing a node's controls on the way into the graph.
//!
//! Some of what a host does to a node is not audio routing and not a scalar
//! param: binding a hosted plugin's meter, polling its latency. (MIDI is
//! neither: it travels on event edges.) Each needs the *concrete* node, and
//! the graph holds only boxed nodes it owns outright.
//!
//! So the question is asked **once, of the owned node, before it is
//! inserted**, and the answer is kept on the entity:
//!
//! | Component | Captured from | Read by |
//! |---|---|---|
//! | `PluginShadow` (`plugin`) | a loaded `PluginClient` (`CapturedControls::for_plugin`), or an in-process VST2 plugin's meter (`CapturedControls::for_plugin_meter`) | plugin meter bind and latency poll |
//!
//! (A node's params need no capture: its controls are its `ParamSet`, which
//! the insert addresses by key, [`SessionNode::params`](crate::graph::SessionNode::params).)
//!
//! Every one holds only state the node shares with the graph's copy (the
//! plugin's controls), so the component reaches the
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
//! capture (`CapturedControls::for_plugin` for a plugin, the default for
//! any other node) and [`bind`](CapturedControls::bind) yourself.
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
//! with a [`CapturedControls`] and [`bind`](CapturedControls::bind).

use bevy_ecs::prelude::*;

use tutti_core::{AudioNode, NodeKey};

/// The controls captured from one node, before it moved into the graph.
///
/// Empty for every node but a hosted plugin.
#[must_use = "captured controls do nothing until they are bound to the entity"]
#[derive(Default)]
pub struct CapturedControls {
    #[cfg(feature = "plugin")]
    plugin: Option<tutti_plugin::handles::PluginControls>,
    /// An in-process VST2 plugin's meter cell, its one control (a plugin with
    /// [`PluginControls`](tutti_plugin::handles::PluginControls) carries its
    /// meter in them).
    #[cfg(feature = "plugin")]
    plugin_meter: Option<tutti_plugin::handles::PluginMeter>,
}

impl CapturedControls {
    /// Captures the controls of a loaded out-of-process plugin.
    ///
    /// Call it on the unbound client before it is bound and inserted
    /// ([`AudioGraphRes::insert_plugin`](crate::graph::AudioGraphRes::insert_plugin)):
    /// its [`PluginControls`](tutti_plugin::handles::PluginControls) become the
    /// entity's `PluginShadow` once [bound](Self::bind).
    #[cfg(feature = "plugin")]
    pub fn for_plugin(client: &tutti_plugin::handles::PluginClient) -> Self {
        Self {
            plugin: Some(client.controls()),
            plugin_meter: None,
        }
    }

    /// Captures an in-process VST2 plugin's meter cell
    /// ([`Plugin::meter`](tutti_plugin::catalog::Plugin::meter)), its one
    /// control: it becomes the entity's `PluginShadow` once [bound](Self::bind),
    /// so the meter binding gives it the project meter as it does an
    /// out-of-process plugin.
    #[cfg(feature = "plugin")]
    pub fn for_plugin_meter(meter: tutti_plugin::handles::PluginMeter) -> Self {
        Self {
            plugin: None,
            plugin_meter: Some(meter),
        }
    }

    /// Binds `entity` to `node`: inserts [`AudioNode`] and every captured control.
    ///
    /// The whole binding in one step, which is how every insertion path in this
    /// crate forms it.
    pub fn bind(self, entity: &mut EntityWorldMut, node: AudioNode) {
        entity.insert(node);
        self.replace(entity, node);
    }

    /// Replaces the entity's captured controls with these, keeping its
    /// [`AudioNode`] — the crossfade case, where the node changes under a
    /// surviving handle.
    ///
    /// A control this node does not have is **removed**: the old plugin's
    /// would otherwise stay reachable under the new node's id.
    ///
    /// The plugin binding latch is cleared too. `PluginMeterBound` says
    /// "*this* plugin has its meter installed", and the incoming
    /// `PluginClient` has none: left in place it would stop the binding
    /// system from ever installing it.
    pub(crate) fn replace(self, entity: &mut EntityWorldMut, node: AudioNode) {
        let node: NodeKey = node.0;
        let _ = (&entity, node);
        #[cfg(feature = "plugin")]
        {
            entity.remove::<crate::plugin_host::PluginMeterBound>();
        }
        #[cfg(feature = "plugin")]
        match (self.plugin, self.plugin_meter) {
            (Some(controls), _) => {
                entity.insert(crate::plugin_host::PluginShadow::new(node, controls));
            }
            (None, Some(meter)) => {
                entity.insert(crate::plugin_host::PluginShadow::meter_only(node, meter));
            }
            (None, None) => {
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
    #[cfg(feature = "plugin")]
    e.try_remove::<crate::plugin_host::PluginShadow>();
}
