//! Plugin (VST2/VST3/CLAP) hosting: editor lifecycle + crash detection.
//!
//! Sub-concepts:
//! - [`editor`] — open / attach / idle / window-resize / close. The 5-system
//!   choreography that owns the plugin GUI window's lifecycle.
//! - [`crash`] — polls each plugin's crashed flag and unwires from the graph.

use bevy_app::{App, Plugin, Update};
use bevy_ecs::prelude::*;

mod crash;
mod editor;
mod scan;

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

/// Bevy plugin: plugin editor lifecycle + crash detection + plugin
/// catalog resource.
///
/// Inserts:
/// - [`crate::resources::PluginEditorMainThread`] non-send marker to pin
///   editor systems to the main thread (AppKit / Win32 / X11).
/// - [`crate::resources::PluginsRes`] containing an empty in-memory
///   plugin catalog (no scan dirs configured by default — apps that
///   want disk-backed scanning should override the resource at startup
///   with a `Plugins::with_config(...).with_fresh_scan()`).
pub struct TuttiHostingPlugin;

impl Plugin for TuttiHostingPlugin {
    fn build(&self, app: &mut App) {
        app.register_type::<OpenPluginEditor>()
            .register_type::<PendingPluginEditor>();

        app.add_observer(close_editor_observer);

        app.insert_non_send_resource(crate::resources::PluginEditorMainThread);

        // Default plugin catalog: empty in-memory, no scan dirs. Apps
        // that want a real disk-backed catalog should overwrite this
        // resource with their own `PluginsRes::new(Plugins::with_config(...))`
        // after `add_plugins(TuttiHostingPlugin)`.
        let default_db_path = std::path::PathBuf::from(".dawai-plugins.json");
        let config = tutti_plugin::catalog::PluginsConfig::new(default_db_path, Vec::new());
        let plugins = tutti_plugin::catalog::Plugins::empty(config.clone());
        app.insert_resource(crate::resources::PluginsRes::new(plugins));

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
                plugin_crash_detect_system,
                trigger_plugin_scan,
                poll_plugin_scan.after(trigger_plugin_scan),
            ),
        );
    }
}
