//! Plugin (VST2/VST3/CLAP/AU) hosting for Bevy: editor lifecycle, crash
//! detection, async catalog scanning, and param reconciliation.
//!
//! This crate owns the ECS surface that turns `tutti-plugin`'s loaded
//! plugin handles into Bevy entities with a native GUI editor window,
//! and reconciles `PluginParam` changes into the running audio graph.
//!
//! **Bevy-only by design.** Every module here is ECS / window glue; there is no
//! Bevy-free core to gate. A non-Bevy host uses the (Bevy-free) `tutti-plugin`
//! crate for plugin discovery / loading / `PluginHandle` param control and
//! `tutti-plugin-server` for out-of-process audio, wiring editor + scan hosting
//! itself.
//!
//! Sub-modules:
//! - [`editor`] — open / attach / idle / window-resize / close. The 5-system
//!   choreography that owns the plugin GUI window's lifecycle.
//! - [`crash`] — polls each plugin's crashed flag and unwires from the graph.
//! - [`scan`] — async plugin-catalog scanning on the compute task pool.
//! - [`native_window`] — platform helpers for child-window parenting.
//! - `live_resize` (macOS only) — AppKit live-resize observer.

use bevy_app::{App, Plugin, Update};
use bevy_ecs::prelude::*;

use tutti_core::graph::{AudioNode, GraphReconcileSystems, NodeParamEpoch, PluginParam};

pub mod crash;
pub mod editor;
pub mod native_window;
pub mod scan;

#[cfg(target_os = "macos")]
mod live_resize;

pub use crash::plugin_crash_detect_system;
pub use editor::{
    close_editor_observer, plugin_editor_attach_system, plugin_editor_idle_system,
    plugin_editor_open_system, plugin_editor_resize_request_system,
    plugin_editor_window_close_system, plugin_editor_window_resize_system, CloseEditor,
    OpenPluginEditor, PendingPluginEditor, PluginEditorOpen, PluginEmitter,
};
pub use scan::{
    poll_plugin_scan, trigger_plugin_scan, InFlightScan, PluginScanConfig, PluginsScanned,
    RescanPlugins,
};

/// Non-Send marker resource that forces plugin editor systems to run on the
/// main thread. AppKit (macOS), Win32, and X11 window operations must happen
/// on the main thread. JUCE, VSTGUI, and other plugin GUI frameworks assume
/// this. Inserted as `insert_non_send_resource` so any system that takes
/// `NonSend<PluginEditorMainThread>` is pinned to the main thread.
pub struct PluginEditorMainThread;

/// The plugin discovery + loading catalog. Owns the on-disk DB and the
/// scan-dir config; systems reach in to `register_bundled_plugin`,
/// `unregister_bundled_plugins`, `rescan`, etc.
///
/// `Plugins` is `Send + Sync` (the `PluginCatalog` trait carries
/// `Send + Sync` supertraits, which propagate through `Box<dyn ...>`), so
/// Bevy's `ResMut<PluginsRes>` exclusivity is the only synchronization
/// needed — no extra `Mutex`.
#[derive(Resource)]
pub struct PluginsRes(pub tutti_plugin::catalog::Plugins);

impl PluginsRes {
    pub fn new(plugins: tutti_plugin::catalog::Plugins) -> Self {
        Self(plugins)
    }
}

/// Reconciles `Changed<PluginParam>` into the bound [`PluginEmitter`].
///
/// `PluginHandle::set_parameter` is RT-safe fire-and-forget; the call
/// publishes to a lock-free channel that the audio thread drains. No
/// graph mutation happens here, so we don't touch `GraphDirty`.
pub fn reconcile_plugin_params(
    changed: Query<(&PluginEmitter, &PluginParam), Changed<PluginParam>>,
) {
    for (emitter, param) in changed.iter() {
        emitter.handle.set_parameter(param.id, param.value);
    }
}

/// Bump the epoch for plugin param changes (`PluginParam`).
pub fn bump_param_epoch_plugin(
    mut epoch: ResMut<NodeParamEpoch>,
    changed: Query<&AudioNode, Changed<PluginParam>>,
) {
    for node in changed.iter() {
        epoch.bump(node.0);
    }
}

/// Bevy plugin: plugin editor lifecycle + crash detection + async catalog
/// scanning + param reconciliation.
///
/// Inserts:
/// - [`PluginEditorMainThread`] non-send marker to pin editor systems to
///   the main thread (AppKit / Win32 / X11).
/// - [`PluginsRes`] containing an empty in-memory plugin catalog (no scan
///   dirs configured by default — apps that want disk-backed scanning
///   should override the resource at startup with a
///   `Plugins::with_config(...).with_fresh_scan()`).
///
/// Schedules:
/// - The editor-lifecycle + crash-detect + scan systems in `Update`.
/// - [`bump_param_epoch_plugin`] in `Update`.
/// - [`reconcile_plugin_params`] in [`GraphReconcileSystems::Params`].
///
/// Requires [`tutti_core::graph::GraphReconcilePlugin`] (which configures the
/// `GraphReconcileSystems` set) to be added before this plugin.
pub struct TuttiHostingPlugin;

impl Plugin for TuttiHostingPlugin {
    fn build(&self, app: &mut App) {
        app.register_type::<OpenPluginEditor>()
            .register_type::<PendingPluginEditor>();

        app.add_observer(close_editor_observer);

        app.insert_non_send_resource(PluginEditorMainThread);

        // Default plugin catalog: empty in-memory, no scan dirs. Apps
        // that want a real disk-backed catalog should overwrite this
        // resource with their own `PluginsRes::new(Plugins::with_config(...))`
        // after `add_plugins(TuttiHostingPlugin)`.
        let default_db_path = std::path::PathBuf::from(".dawai-plugins.json");
        let config = tutti_plugin::catalog::PluginsConfig::new(default_db_path, Vec::new());
        let plugins = tutti_plugin::catalog::Plugins::empty(config.clone());
        app.insert_resource(PluginsRes::new(plugins));

        // Async scan path: config mirrors the default catalog (apps that
        // override `PluginsRes` should overwrite `PluginScanConfig` to
        // match), an empty in-flight slot, and the rescan messages.
        app.insert_resource(PluginScanConfig(config));
        app.init_resource::<InFlightScan>();
        app.add_message::<RescanPlugins>();
        app.add_message::<PluginsScanned>();

        app.add_systems(
            Update,
            (
                plugin_editor_open_system,
                plugin_editor_attach_system,
                plugin_editor_idle_system,
                plugin_editor_resize_request_system.after(plugin_editor_idle_system),
                plugin_editor_window_resize_system.after(plugin_editor_resize_request_system),
                plugin_editor_window_close_system,
                // Removes a crashed plugin's node + sets GraphDirty (no inline
                // commit), so anchor it before the Commit-phase commit_graph.
                plugin_crash_detect_system
                    .before(GraphReconcileSystems::Commit)
                    .run_if(tutti_core::graph::engine_ready),
                trigger_plugin_scan,
                poll_plugin_scan.after(trigger_plugin_scan),
                // Param-epoch bump for plugin param changes.
                bump_param_epoch_plugin,
            ),
        );

        // `reconcile_plugin_params` writes through `PluginEmitter`, holds no
        // engine resource, so it stays ungated within the Params phase.
        app.add_systems(
            Update,
            reconcile_plugin_params.in_set(GraphReconcileSystems::Params),
        );
    }
}
