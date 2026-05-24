//! Crashed-plugin detection: polls each plugin's `is_crashed()` and
//! unwires the entity from the graph.

use bevy_ecs::prelude::*;

use crate::playback::AudioEmitter;
use crate::resources::TuttiGraphRes;

use super::editor::{PluginEditorOpen, PluginEmitter};

/// Detects crashed plugins and removes them from the graph.
///
/// Polls `handle.is_crashed()` for all plugin entities. If a plugin has
/// crashed, removes the graph node and despawns `PluginEmitter` + `PluginEditorOpen`.
pub fn plugin_crash_detect_system(
    mut commands: Commands,
    graph: Option<ResMut<TuttiGraphRes>>,
    query: Query<(Entity, &AudioEmitter, &PluginEmitter)>,
) {
    let Some(mut graph) = graph else { return };

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

    if edited {
        graph.0.commit();
    }
}
