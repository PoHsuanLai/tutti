//! Plugin (VST2/VST3/CLAP/AU) hosting for Bevy.
//!
//! Turns `tutti-plugin`'s catalog and handles into an ECS surface: a
//! [`PluginRequest`] becomes a loaded plugin bound to the transport, the MIDI
//! bus and the modulation matrix, with a native GUI window and a liveness state.
//!
//! **Bevy-only by design.** Every module here is ECS / window glue; there is no
//! Bevy-free core to gate. A non-Bevy host uses the (Bevy-free) `tutti-plugin`
//! crate directly and wires the equivalent itself.
//!
//! This module mirrors the engine's `plugin/` *tier* (`tutti-plugin` plus the
//! `tutti-plugin-server` subprocess it launches) rather than a single crate. It
//! is `plugin_host`, not `plugin`, because that name belongs to the crate's
//! composition root, [`TuttiPlugin`](crate::TuttiPlugin).
//!
//! # Life of a plugin
//!
//! A host spawns a [`PluginRequest`]. [`load`] picks it up, runs the subprocess
//! launch on a worker, and promotes the result to an `AudioNode` carrying a
//! [`PluginEmitter`]. [`bind`] then installs the transport, registers the plugin
//! with the shared MIDI resolver, and builds accumulators for whichever params
//! the host declared modulatable. [`health`] polls liveness from there on, and
//! removes `AudioNode` — the one handle everything else keys on — when a plugin
//! is finally written off.
//!
//! Sub-modules:
//! - [`load`] — off-thread loading: request → pending → promoted.
//! - [`bind`] — transport, MIDI and parameter binding once loaded.
//! - [`health`] — debounced liveness, state snapshots, unwiring the dead.
//! - [`editor`] — the GUI window's lifecycle, driven by [`SetEditorVisible`].
//! - [`scan`] — catalog scanning with per-plugin progress.
//! - [`catalog`] — probing a single plugin off the frame thread.
//! - [`native_window`] — platform helpers for child-window parenting.
//! - `live_resize` (macOS only) — AppKit live-resize observer.

use bevy_app::{App, Plugin, Update};
use bevy_ecs::prelude::*;

use crate::graph::GraphReconcileSystems;

pub mod bind;
pub mod catalog;
pub mod editor;
pub mod health;
pub mod latency;
pub mod load;
pub mod native_window;
// `render_mode` needs `crate::export`'s `ExportInFlight` to know a bounce is
// running, and that module is itself `export`-gated. This is the one place a
// `cfg` is right rather than an `is_plugin_added` check: without the feature
// the type does not exist to name, so there is no plugin to ask about.
#[cfg(feature = "export")]
pub mod render_mode;
pub mod scan;

#[cfg(target_os = "macos")]
pub mod live_resize;

/// macOS: the `NonSend` home for AppKit live-resize observers, plus its
/// reaper system. Public because the editor systems name it in their
/// signatures — the observers must never live in a `Send + Sync` component.
#[cfg(target_os = "macos")]
pub use live_resize::{reap_orphaned_live_resize_observers, LiveResizeRegistry};

#[cfg(feature = "modulation")]
pub use bind::{plugin_bind_params, PluginParamsBound};
pub use bind::{plugin_bind_transport, PluginTransportBound};
pub use catalog::{poll_probes, start_probe, InFlightProbes, PluginProbed, ProbePlugin};
pub use editor::{
    editor_is_open, plugin_editor_attach_system, plugin_editor_idle_system,
    plugin_editor_resize_request_system, plugin_editor_window_close_system,
    plugin_editor_window_resize_system, set_editor_visible_observer, PendingPluginEditor,
    PluginEditorOpen, PluginEmitter, PluginFloatingEditorOpen, SetEditorVisible, Visibility,
};
pub use health::{plugin_health_poll, plugin_state_snapshot, PluginHealth, PluginLiveness};
pub use latency::{plugin_latency_poll, CompensatedLatency};
pub use load::{
    plugin_load_promote, plugin_load_start, PendingPlugin, PluginLoadDone, PluginLoadTerminated,
    PluginRequest,
};
#[cfg(feature = "export")]
pub use render_mode::{plugin_render_mode_drive, PluginRenderMode, RenderModeAnnounced};
pub use scan::{
    poll_scan, start_scan, InFlightScan, PluginCatalogState, PluginsScanned, RescanPlugins,
    ScanProgressed,
};

/// Non-send marker that pins plugin editor systems to the main thread.
///
/// AppKit (macOS), Win32 and X11 window operations must happen there, and JUCE,
/// VSTGUI and every other plugin GUI framework assumes it. Inserted with
/// `insert_non_send`, so any system taking `NonSend<PluginEditorMainThread>` is
/// pinned by Bevy's own scheduler rather than by convention.
pub struct PluginEditorMainThread;

/// Whether this app has windowing, and so whether a plugin editor can exist.
///
/// The editor systems read `WindowCloseRequested` and `WindowResized`. Those
/// message resources are registered by Bevy's window plugin, which a headless
/// host does not add — and an ungated `MessageReader` over an unregistered
/// message fails parameter validation on the first frame rather than quietly
/// reading nothing.
///
/// Keyed on the message resource rather than on a window *existing*: a host with
/// windowing but no window open yet is still a host whose editors will work, and
/// the plugin's own window is spawned by this module anyway.
pub fn windowing_ready(
    close_events: Option<Res<bevy_ecs::message::Messages<bevy_window::WindowCloseRequested>>>,
    resize_events: Option<Res<bevy_ecs::message::Messages<bevy_window::WindowResized>>>,
) -> bool {
    close_events.is_some() && resize_events.is_some()
}

/// The plugin discovery and loading catalog: the on-disk DB plus the scan-dir
/// config.
///
/// **Absent while a rescan is running** — [`scan`] moves the catalog onto the
/// scan thread and re-inserts it on completion, so every reader takes
/// `Option<Res<PluginsRes>>`.
///
/// `Plugins` is `Send + Sync` (the `PluginCatalog` trait carries those
/// supertraits, which propagate through `Box<dyn ..>`), so Bevy's
/// `ResMut<PluginsRes>` exclusivity is the only synchronization needed — no
/// extra `Mutex`.
#[derive(Resource)]
pub struct PluginsRes(pub tutti_plugin::catalog::Plugins);

impl PluginsRes {
    /// Wraps a catalog as the world's one [`PluginsRes`].
    pub fn new(plugins: tutti_plugin::catalog::Plugins) -> Self {
        Self(plugins)
    }
}

// Hosted-plugin parameters have no ECS reconcile and no `AudioParam`-style
// component. They are runtime-discovered `u32` ids with per-instance ranges,
// which `AudioParam<U, P>` (const-generic over a closed `UnitParam` enum)
// cannot express. Automation reaches them sample-accurately over the per-block
// `ParamAutomationSource` path instead — see the `bind` module.

/// Bevy plugin: plugin load, engine binding, health, editor lifecycle, and
/// catalog scanning.
///
/// Inserts:
/// - [`PluginEditorMainThread`] non-send marker to pin editor systems to
///   the main thread (AppKit / Win32 / X11).
/// - [`PluginsRes`] containing an empty in-memory plugin catalog (no scan
///   dirs configured by default — apps that want disk-backed scanning
///   should override the resource at startup with a
///   `Plugins::with_json_catalog(...)`). The scan systems read their
///   directories off this one resource, so there is nothing else to keep in
///   sync with it.
///
/// Requires [`crate::graph::GraphReconcilePlugin`] (which configures the
/// `GraphReconcileSystems` set) to be added before this plugin.
///
/// Also requires the host to have initialised Bevy's task pools — a
/// `TaskPoolPlugin`, or the `DefaultPlugins`/`MinimalPlugins` that include one.
/// Loading and scanning both run on `AsyncComputeTaskPool`; this plugin does not
/// add one itself, since a host that configured its own pool sizes would have
/// them silently replaced.
pub struct TuttiHostingPlugin;

impl Plugin for TuttiHostingPlugin {
    fn build(&self, app: &mut App) {
        app.register_type::<PendingPluginEditor>();

        app.add_observer(set_editor_visible_observer);

        // `Plugin::build` runs on the thread that builds the `App`, which for a
        // windowed Bevy app is the main/UI thread — the same thread every
        // `NonSend` editor system below is pinned to. Marking it here is what
        // arms the `assert_main_thread()` guards throughout the VST3/CLAP/AU/
        // VST2 hosts: those are `debug_assert!`s that also no-op while the main
        // thread is unrecorded, so without this call every main-thread guard in
        // the plugin layer is decorative.
        //
        // Idempotent (`OnceLock::set`): a host that already marked its own main
        // thread wins and this call is a no-op.
        tutti_plugin::mark_main_thread();

        app.insert_non_send(PluginEditorMainThread);

        // macOS AppKit live-resize observers. `NonSend` so Bevy pins every
        // access — and therefore every `removeObserver` drop — to the main
        // thread, which is what keeps the observer out of a `Send + Sync`
        // component.
        #[cfg(target_os = "macos")]
        app.insert_non_send(live_resize::LiveResizeRegistry::default());

        // Default plugin catalog: empty in-memory, no scan dirs. Apps that want
        // a real disk-backed catalog overwrite this resource with their own
        // `PluginsRes::new(Plugins::with_json_catalog(..))` — or any other
        // `PluginCatalog` impl — after `add_plugins(TuttiHostingPlugin)`.
        //
        // The scan reads its directories off *this* resource (the catalog moves
        // onto the scan thread and back), so overriding it is the whole
        // configuration story. There is no second config to keep in sync.
        let default_db_path = std::path::PathBuf::from(".dawai-plugins.json");
        let config = tutti_plugin::catalog::CatalogConfig::new(
            default_db_path,
            tutti_plugin::catalog::NO_SCAN_DIRS,
        );
        app.insert_resource(PluginsRes::new(tutti_plugin::catalog::Plugins::empty(
            config,
        )));

        app.init_resource::<PluginCatalogState>();
        app.init_resource::<InFlightProbes>();
        app.add_message::<RescanPlugins>();
        app.add_message::<ScanProgressed>();
        app.add_message::<PluginsScanned>();
        app.add_message::<ProbePlugin>();
        app.add_message::<PluginProbed>();

        // Editor systems, gated on **windowing** rather than on the engine.
        //
        // They read `WindowCloseRequested` / `WindowResized`, which only exist
        // once something has added Bevy's window plugin. A headless host — a
        // renderer, a test, this crate's own examples — registers neither, and
        // an ungated `MessageReader` fails parameter validation on the first
        // frame rather than simply finding nothing to read.
        //
        // The engine is the wrong gate for these: a plugin's GUI is perfectly
        // meaningful with audio stopped, so `engine_ready` is both too strict
        // (no editor without a device) and too loose (it says nothing about
        // windows, which is what these actually need).
        app.add_systems(
            Update,
            (
                // `set_editor_visible_observer` inserts `PendingPluginEditor`;
                // attach reads it and finishes the open once Bevy has created
                // the window and its native handle exists.
                plugin_editor_attach_system,
                plugin_editor_idle_system,
                plugin_editor_resize_request_system.after(plugin_editor_idle_system),
                plugin_editor_window_resize_system.after(plugin_editor_resize_request_system),
                plugin_editor_window_close_system,
            )
                .run_if(windowing_ready),
        );

        // Health needs neither a window nor, strictly, a device — but it unwires
        // through the graph, so it runs with the engine.
        app.add_systems(
            Update,
            (
                // Unwires a dead plugin by removing `AudioNode`; the observers
                // that hang off that removal take the node out of the graph and
                // the sender off the MIDI bus, so this must land before the
                // Commit-phase commit_graph rather than after it.
                plugin_health_poll.before(GraphReconcileSystems::Commit),
                // Ordered after the poll so a plugin declared dead this frame is
                // not asked for state it can no longer produce.
                plugin_state_snapshot.after(plugin_health_poll),
                // Before `Compensate`, because it works by raising `GraphDirty`
                // and that phase reads the flag; after `plugin_health_poll`, so
                // a plugin declared dead this frame is not compensated for on
                // its way out of the graph.
                plugin_latency_poll
                    .after(plugin_health_poll)
                    .before(GraphReconcileSystems::Compensate),
            )
                .run_if(crate::graph::engine_ready),
        );

        // Announce the render mode to hosted plugins, but only if this app can
        // actually export — `ExportInFlight` is `crate::export`'s component, and
        // without that plugin the query is over a type nothing ever spawns.
        //
        // `is_plugin_added` rather than a feature flag: whether a host bounces
        // is a composition choice it makes at build time, and a flag would make
        // it a compile-time property of this crate instead.
        //
        // After both export systems, so the frame's answer has settled before it
        // is read — see the module docs on back-to-back renders. Ungated on the
        // engine for the same reason `export` is: a render already in flight
        // when audio stops still has to put its plugins back.
        #[cfg(feature = "export")]
        if app.is_plugin_added::<crate::export::ExportPlugin>() {
            app.init_resource::<PluginRenderMode>();
            app.add_systems(
                Update,
                plugin_render_mode_drive
                    .after(crate::export::poll_exports)
                    .after(crate::export::start_exports),
            );
        }

        // Scanning is deliberately **not** gated on `engine_ready`: it walks the
        // filesystem and probes subprocesses, touching neither the graph nor a
        // window. A host that wants to populate its browser before (or without)
        // starting audio must be able to. Poll before start, as `export` does,
        // so a scan that finishes between two frames is still reported.
        app.add_systems(Update, (poll_scan, start_scan).chain());

        // Single-plugin probes, ungated for the same reason and polled first for
        // the same reason.
        app.add_systems(Update, (poll_probes, start_probe).chain());

        // Loading adds a node, so promotion belongs in `Spawn` — MIDI
        // registration and the engine bindings order themselves after that
        // phase and pick a freshly promoted plugin up the same frame.
        //
        // Only the *start* is gated: it needs a catalog to read an audio config
        // from, and nothing downstream can use a plugin the engine cannot host.
        // Promotion stays ungated so a load already in flight when the engine
        // goes down is still reported rather than left hanging.
        app.add_systems(
            Update,
            (
                plugin_load_start.run_if(crate::graph::engine_ready),
                plugin_load_promote.after(plugin_load_start),
            )
                .in_set(GraphReconcileSystems::Spawn),
        );

        // `PluginClient` is only reachable through the shared MIDI resolver if
        // its type is registered — an unregistered node type is invisible to
        // `register_midi_senders`, so without this a hosted plugin receives no
        // MIDI however it is wired.
        bind::register_plugin_node_types(app);

        // Binding sits between spawn and commit, alongside MIDI registration and
        // route rebuilding: it needs the node to exist, and the graph edits it
        // stages must reach the same frame's commit.
        app.add_systems(
            Update,
            plugin_bind_transport
                .after(GraphReconcileSystems::Spawn)
                .before(GraphReconcileSystems::Commit)
                .run_if(crate::graph::engine_ready),
        );

        // Param accumulators are modulation vocabulary (`ModParamRange`,
        // `ModTargetRegistry`), so this half only exists when that feature does.
        // A `plugin` build without `modulation` still loads, binds transport and
        // receives MIDI — it just has no route to modulate a param with.
        #[cfg(feature = "modulation")]
        app.add_systems(
            Update,
            plugin_bind_params
                .after(GraphReconcileSystems::Spawn)
                .before(GraphReconcileSystems::Commit)
                .run_if(crate::graph::engine_ready),
        );

        // Reaps AppKit observers for editors that lost `PluginEditorOpen`
        // without going through `set_editor_visible_observer` — chiefly
        // `plugin_health_poll`, which is not main-thread pinned.
        #[cfg(target_os = "macos")]
        app.add_systems(
            Update,
            live_resize::reap_orphaned_live_resize_observers.after(plugin_health_poll),
        );
    }
}
