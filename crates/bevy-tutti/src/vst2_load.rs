//! Main-thread VST2 loader.
//!
//! VST2 plugins (and many JUCE-based VST3/CLAP wrappers) bind their
//! `MessageManager` to the *first* thread that calls into the plugin
//! library. If that's a worker thread, opening the editor later
//! deadlocks because UI events are dispatched against a manager that
//! lives on the wrong thread. The fix is to do `tutti::vst2(...).build()`
//! on the main thread, *before* the host ever touches the editor.
//!
//! The system here mirrors dawai's `process_pending_vst2_loads` but at
//! the bevy-tutti layer:
//!
//! - [`PendingVst2Build`] — "build a VST2 plugin from this path on the
//!   main thread; insert it on this entity when done."
//! - [`process_pending_vst2_builds`] — pinned to the main thread via
//!   `NonSend<PluginEditorMainThread>`. Drains the pending queue, builds
//!   each plugin synchronously (VST2's `build()` is fast), inserts
//!   `AudioNode` + `PluginEmitter` (and `OpenPluginEditor` if requested),
//!   and removes [`PendingVst2Build`].

use bevy_ecs::prelude::*;

use tutti::core::ecs::{AudioNode, NodeKind};

use crate::graph::reconcile::GraphDirty;
use crate::plugin_host::{OpenPluginEditor, PluginEmitter};
use crate::resources::{PluginEditorMainThread, TuttiGraphRes};

/// "Build a VST2 plugin from `path` on the main thread."
///
/// Spawn a fresh entity with this component; the next time
/// [`process_pending_vst2_builds`] runs, the entity is upgraded to
/// `(AudioNode, NodeKind::Plugin, PluginEmitter)` (and
/// `OpenPluginEditor` if `open_editor_after` is true). Failed builds
/// log the error and despawn the entity.
#[derive(Component, Debug, Clone)]
pub struct PendingVst2Build {
    pub path: String,
    pub sample_rate: f64,
    /// Optional preset blob applied via `PluginHandle::load_state` once
    /// the plugin is built.
    pub state: Option<Vec<u8>>,
    /// If `true`, also inserts [`OpenPluginEditor`] so the host's
    /// editor-open system pops the GUI immediately.
    pub open_editor_after: bool,
}

impl PendingVst2Build {
    pub fn new(path: impl Into<String>, sample_rate: f64) -> Self {
        Self {
            path: path.into(),
            sample_rate,
            state: None,
            open_editor_after: false,
        }
    }

    pub fn with_state(mut self, state: Vec<u8>) -> Self {
        self.state = Some(state);
        self
    }

    pub fn open_editor(mut self, open: bool) -> Self {
        self.open_editor_after = open;
        self
    }
}

/// Main-thread system that builds [`PendingVst2Build`] entities.
///
/// Pinned to the main thread via `NonSend<PluginEditorMainThread>`. Bevy
/// runs `NonSend` systems on the main thread automatically when the
/// runner allows it.
pub fn process_pending_vst2_builds(
    _main_thread: NonSend<PluginEditorMainThread>,
    mut commands: Commands,
    graph: Option<ResMut<TuttiGraphRes>>,
    mut dirty: ResMut<GraphDirty>,
    pending: Query<(Entity, &PendingVst2Build)>,
) {
    let Some(mut graph) = graph else { return };

    for (entity, load) in pending.iter() {
        commands.entity(entity).remove::<PendingVst2Build>();

        let result = tutti::vst2(load.sample_rate, load.path.as_str())
            .build()
            .map_err(|e| e.to_string());

        match result {
            Ok((unit, handle)) => {
                if let Some(preset) = &load.state {
                    handle.load_state(preset);
                }
                let node_id = graph.0.add_boxed(unit);
                dirty.0 = true;

                let mut e = commands.entity(entity);
                e.insert((
                    AudioNode(node_id),
                    NodeKind::Plugin,
                    PluginEmitter { handle },
                ));
                if load.open_editor_after {
                    e.insert(OpenPluginEditor);
                }
                bevy_log::info!(
                    "PendingVst2Build: '{}' loaded on main thread, node {:?}",
                    load.path,
                    node_id
                );
            }
            Err(e) => {
                bevy_log::error!("PendingVst2Build: '{}' failed: {}", load.path, e);
                commands.entity(entity).despawn();
            }
        }
    }
}
