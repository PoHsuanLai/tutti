//! Plugin (VST2/VST3/CLAP/AU) hosting for Bevy (feature `plugin`; formats
//! with `vst2`, `vst3`, `clap`, `au`).
//!
//! Turns `tutti-plugin`'s catalog and handles into an ECS surface: a
//! [`PluginRequest`] becomes a loaded plugin bound to the transport, the MIDI
//! bus and modulation lanes, with a native GUI window and a liveness state.
//!
//! Every module here is ECS and window glue over `tutti-plugin` and the
//! `tutti-plugin-server` subprocess it launches; a non-Bevy host uses
//! `tutti-plugin` directly. [`TuttiHostingPlugin`] adds it all, and
//! [`TuttiPlugin`](crate::TuttiPlugin) adds that with the `plugin` feature.
//!
//! # Life of a plugin
//!
//! A host spawns a [`PluginRequest`]. [`load`] picks it up, runs the session's
//! load job (`tutti_session::PluginLoad`) on a worker, and promotes the result
//! (`Session::add_plugin`) to an `AudioNode` carrying a [`PluginEmitter`]:
//! the plugin is bound into the graph as a graph node (reading its transport
//! from each block's `Env`), and its controls are captured on the way.
//! [`bind`] then gives it the project meter. From there the session hosts it
//! (`tutti_session::HostedPlugin`): [`health`] runs the session's
//! `Phase::Plugins` each frame (liveness, latency, GUI edits, snapshots) and
//! removes `AudioNode` — the one handle everything else keys on — when a
//! plugin is finally written off.
//!
//! Sub-modules:
//! - [`load`] — off-thread loading: request → pending → promoted.
//! - [`bind`] — meter binding once loaded.
//! - [`health`] — the session's `Phase::Plugins`, placed on the entities.
//! - `render_mode` (`export`) — the session's `Phase::RenderMode`.
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
// `render_mode` is scheduled by `crate::export::ExportPlugin`, after its
// systems, and that module is itself `export`-gated: without the feature
// the systems do not exist to order against.
#[cfg(feature = "export")]
pub mod render_mode;
pub mod scan;

#[cfg(target_os = "macos")]
pub mod live_resize;

/// macOS: the `NonSend` home for AppKit live-resize observers, plus its
/// reaper system. Public because the editor systems name it in their
/// signatures — the observers must never live in a `Send + Sync` component.
#[cfg(target_os = "macos")]
pub use live_resize::{LiveResizeRegistry, reap_orphaned_live_resize_observers};

pub use bind::{PluginMeterBound, PluginShadow, plugin_bind_meter};
pub use catalog::{InFlightProbes, PluginProbed, ProbePlugin, poll_probes, start_probe};
pub use editor::{
    PendingPluginEditor, PluginEditorOpen, PluginEmitter, PluginFloatingEditorOpen,
    SetEditorVisible, Visibility, editor_is_open, plugin_editor_attach_system,
    plugin_editor_idle_system, plugin_editor_resize_request_system,
    plugin_editor_window_close_system, plugin_editor_window_resize_system,
    set_editor_visible_observer,
};
pub use health::{
    PluginGaveUp, PluginHealth, PluginLiveness, PluginRecovered, PluginReloading,
    plugin_health_poll, plugin_state_snapshot,
};
pub use load::{
    PendingPlugin, PluginLoadDone, PluginLoadTerminated, PluginRequest, plugin_load_promote,
    plugin_load_start,
};
#[cfg(feature = "export")]
pub use render_mode::plugin_render_mode;
pub use scan::{
    InFlightScan, PluginCatalogState, PluginsScanned, RescanPlugins, ScanProgressed, poll_scan,
    start_scan,
};

/// The main-thread token, as a non-send resource: it pins plugin editor systems
/// to the main thread and hands them the proof the editor calls take.
///
/// AppKit (macOS), Win32 and X11 window operations must happen there, and JUCE,
/// VSTGUI and every other plugin GUI framework assumes it. The resource holds a
/// [`MainThread`](tutti_plugin::MainThread), which is `!Send`, and is inserted
/// with `insert_non_send`: Bevy runs a system that takes
/// `NonSend<PluginEditorMainThread>` on the thread that owns the world, so the
/// scheduler pins it, and [`token`](Self::token) is the `&MainThread` every
/// main-thread entry of `tutti-plugin` and `tutti-session` takes.
#[derive(Debug)]
pub struct PluginEditorMainThread(tutti_plugin::MainThread);

impl PluginEditorMainThread {
    /// Wraps a claimed token: insert it with `World::insert_non_send_resource`
    /// (or `App::insert_non_send`) on the main thread.
    pub fn new(main: tutti_plugin::MainThread) -> Self {
        Self(main)
    }

    /// The proof that the system holding this resource is on the main thread.
    pub fn token(&self) -> &tutti_plugin::MainThread {
        &self.0
    }
}

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
/// Present throughout a rescan: [`scan`] works on a snapshot and applies its
/// deltas to this resource on the main thread, so the world's one catalog stays
/// readable and every row a system sees is whole.
///
/// `Plugins` is plain data and channels, so Bevy's `ResMut<PluginsRes>`
/// exclusivity is the only synchronization needed — no extra `Mutex`.
#[derive(Resource, Debug)]
pub struct PluginsRes(pub tutti_plugin::catalog::Plugins);

impl PluginsRes {
    /// Wraps a catalog as the world's one [`PluginsRes`].
    #[must_use]
    pub fn new(plugins: tutti_plugin::catalog::Plugins) -> Self {
        Self(plugins)
    }
}

// Hosted-plugin parameters have no ECS reconcile and no `AudioParam`-style
// component. They are runtime-discovered `u32` ids with per-instance ranges,
// which `AudioParam<U, P>` (const-generic over a closed `UnitParam` enum)
// cannot express. Automation reaches them sample-accurately through an
// automation node on the plugin's event input instead — see the `bind` module.

/// Adds plugin hosting: load, engine binding, health, editor lifecycle, and
/// catalog scanning.
///
/// Inserts:
/// - [`PluginEditorMainThread`] non-send marker to pin editor systems to
///   the main thread (AppKit / Win32 / X11).
/// - [`PluginsRes`] containing an empty in-memory plugin catalog (no scan
///   dirs configured by default — apps that want disk-backed scanning
///   should override the resource at startup with a
///   `Plugins::load(...)`). The scan systems read their
///   directories off this one resource, so there is nothing else to keep in
///   sync with it.
///
/// Requires [`crate::graph::GraphReconcilePlugin`] (which configures the
/// `GraphReconcileSystems` set) to be added before this plugin.
///
/// With the `modulation` feature, each loaded plugin's parameter mirror (the
/// session's, `tutti_plugin::PluginParamValues::start` in the load job) takes
/// its handle's single `on_parameter_changed` slot (the GUI edits that move a
/// lane's knob). A host installing its own callback on that handle replaces
/// it, and the knobs stop following the plugin's GUI.
///
/// Also requires the host to have initialised Bevy's task pools — a
/// `TaskPoolPlugin`, or the `DefaultPlugins`/`MinimalPlugins` that include one.
/// Loading and scanning both run on `AsyncComputeTaskPool`; this plugin does not
/// add one itself, since a host that configured its own pool sizes would have
/// them silently replaced.
///
/// # Panics
///
/// `build` claims the process's main thread (`tutti_plugin::MainThread::claim`),
/// which is first-come: the app must be built on the UI / OS main thread, before
/// any other thread claims. Built off it, or after another thread has claimed,
/// the claim is refused and `build` panics. `claim` does not ask the OS (test
/// runners build apps off the OS main thread), so the first claimer being the real
/// main thread is the host's to guarantee. An AU editor still re-checks
/// `pthread_main_np` itself.
#[derive(Debug)]
pub struct TuttiHostingPlugin;

impl Plugin for TuttiHostingPlugin {
    fn build(&self, app: &mut App) {
        app.register_type::<PendingPluginEditor>();

        app.add_observer(set_editor_visible_observer);

        // `Plugin::build` runs on the thread that builds the `App`, which for a
        // windowed Bevy app is the main/UI thread — the same thread every
        // `NonSend` editor system below is pinned to. Claiming it here makes it
        // the process's main thread, and the token goes in as the non-send
        // resource the editor systems take.
        //
        // A host that already claimed its own main thread on this one is
        // unaffected (a claim repeats on the same thread); a build on another
        // thread is refused, and that is a bug in the host, not a state to run in.
        let main = tutti_plugin::MainThread::claim()
            .expect("TuttiHostingPlugin must be built on the process's main thread");
        app.insert_non_send(PluginEditorMainThread(main));

        // macOS AppKit live-resize observers. `NonSend` so Bevy pins every
        // access — and therefore every `removeObserver` drop — to the main
        // thread, which is what keeps the observer out of a `Send + Sync`
        // component.
        #[cfg(target_os = "macos")]
        app.insert_non_send(live_resize::LiveResizeRegistry::default());

        // Default plugin catalog: empty in-memory, no scan dirs. Apps that want
        // a real disk-backed catalog overwrite this resource with their own
        // `PluginsRes::new(Plugins::load(..))` — or a `Plugins::with_db` over
        // their own store — after `add_plugins(TuttiHostingPlugin)`.
        //
        // The scan reads its directories off *this* resource, so overriding it
        // is the whole configuration story. There is no second config to keep
        // in sync.
        let default_db_path = std::path::PathBuf::from(".tutti-plugins.json");
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

        // The session's `Phase::Plugins` (health, latency, GUI edits,
        // snapshots), after a promotion so a plugin loaded this frame is
        // watched from it, and in `Spawn`: before `Compensate`, which reads
        // the graph's edit flag a latency change sets, and before the
        // `Commit`-phase commit a dead plugin's removal must reach. A
        // finished snapshot lands first, so the phase sees it.
        app.add_systems(
            Update,
            (
                plugin_state_snapshot.before(plugin_health_poll),
                plugin_health_poll.after(plugin_load_promote).in_set(
                    crate::graph::SessionPhaseSystems(tutti_session::Phase::Plugins),
                ),
            )
                .in_set(GraphReconcileSystems::Spawn)
                .run_if(crate::graph::engine_ready),
        );

        // Binding sits between spawn and commit, alongside MIDI registration and
        // route rebuilding: it needs the node to exist, and the graph edits it
        // stages must reach the same frame's commit.
        app.add_systems(
            Update,
            plugin_bind_meter
                .after(GraphReconcileSystems::Spawn)
                .before(GraphReconcileSystems::Commit)
                .run_if(crate::graph::engine_ready),
        );

        // Reaps AppKit observers for editors that lost `PluginEditorOpen`
        // without going through `set_editor_visible_observer` — chiefly
        // `plugin_health_poll`'s removal, which lands with its commands.
        #[cfg(target_os = "macos")]
        app.add_systems(
            Update,
            live_resize::reap_orphaned_live_resize_observers.after(plugin_health_poll),
        );
    }
}
