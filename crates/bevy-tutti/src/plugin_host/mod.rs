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

use crate::graph::GraphReconcileSystems;

pub mod crash;
pub mod editor;
pub mod native_window;
pub mod scan;

#[cfg(target_os = "macos")]
pub mod live_resize;

/// macOS: the `NonSend` home for AppKit live-resize observers, plus its
/// reaper system. Public because the editor systems name it in their
/// signatures — the observers must never live in a `Send + Sync` component.
#[cfg(target_os = "macos")]
pub use live_resize::{reap_orphaned_live_resize_observers, LiveResizeRegistry};

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
/// this. Inserted as `insert_non_send` so any system that takes
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

// NOTE: `reconcile_plugin_params` moved out with the `PluginParam` component it
// read (which left tutti-core). `PluginEmitter` stays here; a host imports it
// via the `bevy_tutti` umbrella.

/// Bevy plugin: plugin editor lifecycle + crash detection + async catalog
/// scanning + param reconciliation.
///
/// Inserts:
/// - [`PluginEditorMainThread`] non-send marker to pin editor systems to
///   the main thread (AppKit / Win32 / X11).
/// - [`PluginsRes`] containing an empty in-memory plugin catalog (no scan
///   dirs configured by default — apps that want disk-backed scanning
///   should override the resource at startup with a
///   `Plugins::with_json_catalog(...).with_fresh_scan()`).
///
/// Schedules the editor-lifecycle + crash-detect + scan systems in `Update`.
/// (The `PluginParam` reconcile + epoch bump moved to
/// `dawai_model::engine_bind::plugin_host` with the `PluginParam` component.)
///
/// Requires [`crate::graph::GraphReconcilePlugin`] (which configures the
/// `GraphReconcileSystems` set) to be added before this plugin.
pub struct TuttiHostingPlugin;

impl Plugin for TuttiHostingPlugin {
    fn build(&self, app: &mut App) {
        app.register_type::<OpenPluginEditor>()
            .register_type::<PendingPluginEditor>();

        app.add_observer(close_editor_observer);

        // `Plugin::build` runs on the thread that builds the `App`, which for
        // a windowed Bevy app is the main/UI thread — the same thread every
        // `NonSend` editor system below is pinned to. Marking it here is what
        // arms the `assert_main_thread()` guards throughout the VST3/CLAP/AU/
        // VST2 hosts: those are `debug_assert!`s that *also* no-op while the
        // main thread is unrecorded, so with no caller anywhere in the engine
        // every main-thread guard in the plugin layer was decorative in every
        // configuration.
        //
        // Idempotent (`OnceLock::set`): a host that already marked its own
        // main thread wins and this call is a no-op.
        tutti_plugin::mark_main_thread();

        app.insert_non_send(PluginEditorMainThread);

        // macOS AppKit live-resize observers. `NonSend` so Bevy pins every
        // access — and therefore every `removeObserver` drop — to the main
        // thread, replacing the old `unsafe impl Send + Sync` on the handle.
        #[cfg(target_os = "macos")]
        app.insert_non_send(live_resize::LiveResizeRegistry::default());

        // Default plugin catalog: empty in-memory, no scan dirs. Apps
        // that want a real disk-backed catalog should overwrite this
        // resource with their own `PluginsRes::new(Plugins::with_json_catalog(...))`
        // after `add_plugins(TuttiHostingPlugin)`.
        let default_db_path = std::path::PathBuf::from(".dawai-plugins.json");
        let config = tutti_plugin::catalog::CatalogConfig::new(default_db_path, Vec::new());
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
                // Open inserts `PendingPluginEditor`; attach reads it. Without
                // this ordering an editor takes one or two frames to appear
                // depending on scheduling.
                plugin_editor_open_system,
                plugin_editor_attach_system.after(plugin_editor_open_system),
                plugin_editor_idle_system,
                plugin_editor_resize_request_system.after(plugin_editor_idle_system),
                plugin_editor_window_resize_system.after(plugin_editor_resize_request_system),
                plugin_editor_window_close_system,
                // Removes a crashed plugin's node + sets GraphDirty (no inline
                // commit), so anchor it before the Commit-phase commit_graph.
                plugin_crash_detect_system.before(GraphReconcileSystems::Commit),
                trigger_plugin_scan,
                poll_plugin_scan.after(trigger_plugin_scan),
            )
                // Hosting only means anything with a live graph to host into,
                // and these systems read window messages a headless app never
                // registers. Gating the whole set keeps `plugin` usable with the
                // engine disabled.
                .run_if(crate::graph::engine_ready),
        );

        // Reaps AppKit observers for editors that lost `PluginEditorOpen`
        // without going through `close_editor_observer` — chiefly
        // `plugin_crash_detect_system`, which is not main-thread pinned.
        #[cfg(target_os = "macos")]
        app.add_systems(
            Update,
            live_resize::reap_orphaned_live_resize_observers.after(plugin_crash_detect_system),
        );
        // The PluginParam reconcile + epoch bump moved to
        // dawai_model::engine_bind::plugin_host (with the PluginParam component).
    }
}
