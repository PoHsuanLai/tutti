//! Crashed-plugin detection: polls each plugin's `is_crashed()` and
//! unwires the entity from the graph.

use bevy_ecs::prelude::*;

use tutti_core::graph::AudioEmitter;
use tutti_core::graph::AudioGraphRes;

use crate::editor::{PluginEditorOpen, PluginEmitter};

/// Detects crashed plugins and removes them from the graph.
///
/// Polls `handle.is_crashed()` for all plugin entities. If a plugin has
/// crashed, removes the graph node and despawns `PluginEmitter` + `PluginEditorOpen`.
pub fn plugin_crash_detect_system(
    mut commands: Commands,
    mut graph: ResMut<AudioGraphRes>,
    mut dirty: ResMut<tutti_core::graph::GraphDirty>,
    query: Query<(Entity, &AudioEmitter, &PluginEmitter)>,
) {
    let mut edited = false;

    for (entity, audio, plugin) in query.iter() {
        if plugin.handle.is_crashed() {
            bevy_log::error!(
                "Plugin '{}' crashed (entity {entity:?}), removing from graph",
                plugin.handle.name()
            );

            if graph.0.contains(audio.node_id) {
                graph.0.remove(audio.node_id);
                edited = true;
            }

            commands
                .entity(entity)
                .remove::<PluginEmitter>()
                .remove::<PluginEditorOpen>()
                .remove::<AudioEmitter>();
        }
    }

    // Stage only; the Commit-phase `commit_graph` coalesces (this system is
    // anchored before that phase).
    if edited {
        dirty.0 = true;
    }
}
