//! Crashed-plugin detection: polls each plugin's `is_crashed()` and
//! unwires the entity from the graph.

use bevy_ecs::prelude::*;

use tutti_core::AudioNode;

use crate::plugin_host::editor::{PluginEditorOpen, PluginEmitter};

/// Detects crashed plugins and unwires them from the graph.
///
/// Polls `handle.is_crashed()` for all plugin entities and strips the crashed
/// ones of every component that binds them to the engine.
///
/// # Removing `AudioNode` is what unwires it
///
/// This used to remove a second `AudioEmitter` handle alone and take the graph
/// node out by hand. Both halves of that were wrong. `AudioNode` survived, so the
/// `On<Remove, AudioNode>` observers never fired — the node was gone from the
/// graph but the entity still claimed one, and MIDI unregistration (which keys
/// on that same removal) never ran, leaking a sender on the bus for the life of
/// the process. And removing the node inline duplicated
/// [`reconcile_node_despawn`](crate::graph::reconcile_node_despawn), giving the
/// graph two writers for one edit.
///
/// Removing the component instead lets the observer do both jobs, so this
/// system no longer touches the graph at all — hence no `AudioGraphRes` and no
/// `GraphDirty` here.
pub fn plugin_crash_detect_system(mut commands: Commands, query: Query<(Entity, &PluginEmitter)>) {
    for (entity, plugin) in query.iter() {
        if plugin.handle.is_crashed() {
            bevy_log::error!(
                "Plugin '{}' crashed (entity {entity:?}), removing from graph",
                plugin.handle.name()
            );

            commands
                .entity(entity)
                .remove::<PluginEmitter>()
                .remove::<PluginEditorOpen>()
                .remove::<AudioNode>();
        }
    }
}
