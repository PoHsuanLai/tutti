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
pub mod load;
pub mod native_window;
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
    plugin_editor_attach_system, plugin_editor_idle_system, plugin_editor_resize_request_system,
    plugin_editor_window_close_system, plugin_editor_window_resize_system,
    set_editor_visible_observer, PendingPluginEditor, PluginEditorOpen, PluginEmitter,
    SetEditorVisible, Visibility,
};
pub use health::{plugin_health_poll, plugin_state_snapshot, PluginHealth, PluginStatus};
pub use load::{
    plugin_load_promote, plugin_load_start, PendingPlugin, PluginLoadDone, PluginLoadTerminated,
    PluginRequest,
};
pub use scan::{
    poll_scan, start_scan, InFlightScan, PluginCatalogState, PluginsScanned, RescanPlugins,
    ScanProgressed,
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

// Hosted-plugin parameters have no ECS reconcile and no `AudioParam`-style
// component. They are runtime-discovered `u32` ids with per-instance ranges,
// which `AudioParam<U, P>` (const-generic over a closed `UnitParam` enum)
// cannot express. Automation reaches them sample-accurately over the per-block
// `ParamAutomationSource` path instead — see [`bind`].

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
pub struct TuttiHostingPlugin;

impl Plugin for TuttiHostingPlugin {
    fn build(&self, app: &mut App) {
        app.register_type::<PendingPluginEditor>();

        app.add_observer(set_editor_visible_observer);

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

        // Default plugin catalog: empty in-memory, no scan dirs. Apps that want
        // a real disk-backed catalog overwrite this resource with their own
        // `PluginsRes::new(Plugins::with_json_catalog(..))` — or any other
        // `PluginCatalog` impl — after `add_plugins(TuttiHostingPlugin)`.
        //
        // The scan reads its directories off *this* resource (the catalog moves
        // onto the scan thread and back), so overriding it is the whole
        // configuration story. There is no second config to keep in sync.
        let default_db_path = std::path::PathBuf::from(".dawai-plugins.json");
        let config = tutti_plugin::catalog::CatalogConfig::new(default_db_path, Vec::new());
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
                // Unwires a dead plugin by removing `AudioNode`; the observers
                // that hang off that removal take the node out of the graph and
                // the sender off the MIDI bus, so this must land before the
                // Commit-phase commit_graph rather than after it.
                plugin_health_poll.before(GraphReconcileSystems::Commit),
                // Ordered after the poll so a plugin declared dead this frame is
                // not asked for state it can no longer produce.
                plugin_state_snapshot.after(plugin_health_poll),
            )
                // Hosting only means anything with a live graph to host into,
                // and these systems read window messages a headless app never
                // registers. Gating the whole set keeps `plugin` usable with the
                // engine disabled.
                .run_if(crate::graph::engine_ready),
        );

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
        // `register_midi_senders`, which is why a hosted plugin could not
        // receive MIDI however it was wired.
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
