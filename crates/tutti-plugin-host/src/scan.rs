//! Async plugin-catalog scanning.
//!
//! `tutti-plugin`'s [`Plugins::rescan_sync`] walks the configured scan
//! dirs, probes each plugin (out-of-process / subprocess), and rewrites
//! the on-disk catalog. That is bounded-but-blocking work: it must not run
//! on the realtime audio thread *or* stall the Bevy main thread. So we run
//! it on [`AsyncComputeTaskPool`] following the `task.rs` convention — a
//! [`RescanPlugins`] message kicks off a [`Task`], stored in the
//! module-local [`InFlightScan`] resource, drained once per frame.
//!
//! The editor-lifecycle systems stay main-thread-pinned and synchronous;
//! scanning is the only async path here.
//!
//! [`Plugins::rescan_sync`]: tutti_plugin::catalog::Plugins::rescan_sync
//! [`AsyncComputeTaskPool`]: bevy_tasks::AsyncComputeTaskPool

use bevy_ecs::message::{Message, MessageReader, MessageWriter};
use bevy_ecs::prelude::*;
use bevy_log::{info, warn};
use bevy_tasks::{AsyncComputeTaskPool, Task};

use tutti_plugin::catalog::{Plugins, PluginsConfig, ScanResult};

use crate::PluginsRes;
use bevy_tasks::{block_on, futures_lite::future};

/// Config used to build the [`Plugins`] catalog that an async rescan
/// produces. Kept module-local (never the shared `resources.rs`) so the
/// scan task can clone a `Send` config instead of reaching into the live
/// `PluginsRes` (whose `Plugins` exposes no config getter).
///
/// Apps that override [`PluginsRes`] with real scan dirs should overwrite
/// this resource to match, so rescans target the same DB + directories.
#[derive(Resource, Clone)]
pub struct PluginScanConfig(pub PluginsConfig);

/// Request to (re)scan the plugin catalog. Fire this to discover plugins
/// on disk; the scan runs off the main thread and swaps [`PluginsRes`]
/// when it completes, then emits [`PluginsScanned`].
#[derive(Message)]
pub struct RescanPlugins;

/// Emitted once an async rescan finishes and the fresh catalog has been
/// installed into [`PluginsRes`]. Carries the scan tally.
#[derive(Message)]
pub struct PluginsScanned(pub ScanResult);

/// Holds the in-flight scan task. Absent when no scan is running.
#[derive(Resource, Default)]
pub struct InFlightScan(pub Option<Task<(Plugins, ScanResult)>>);

/// Trigger: on [`RescanPlugins`], spawn a sync scan on the compute pool.
///
/// Clones the [`PluginScanConfig`] and runs `Plugins::with_config(cfg)`
/// then `rescan_sync()` **inside** the Bevy task — deliberately the sync
/// scan path, not `tutti-plugin`'s own `scan_async` thread, so the work
/// lives on Bevy's task pool and is drained by [`poll_plugin_scan`]. A scan
/// already in flight swallows further requests until it finishes.
pub fn trigger_plugin_scan(
    mut requests: MessageReader<RescanPlugins>,
    mut in_flight: ResMut<InFlightScan>,
    config: Res<PluginScanConfig>,
) {
    // Drain the reader regardless; we only need to know one was requested.
    let requested = requests.read().count() > 0;
    if !requested {
        return;
    }
    if in_flight.0.is_some() {
        warn!("plugin rescan already in flight; ignoring new request");
        return;
    }

    let cfg = config.0.clone();
    let task = AsyncComputeTaskPool::get().spawn(async move {
        let mut plugins = Plugins::with_config(cfg);
        let result = plugins.rescan_sync();
        (plugins, result)
    });
    in_flight.0 = Some(task);
    info!("plugin rescan started");
}

/// Poll: when the in-flight scan completes, install the fresh catalog into
/// [`PluginsRes`] (a plain `ResMut` write — `Plugins: Send + Sync`, no
/// `Mutex`) and emit [`PluginsScanned`].
pub fn poll_plugin_scan(
    mut in_flight: ResMut<InFlightScan>,
    mut plugins_res: ResMut<PluginsRes>,
    mut scanned: MessageWriter<PluginsScanned>,
) {
    let Some(task) = in_flight.0.as_mut() else {
        return;
    };
    let Some((plugins, result)) = block_on(future::poll_once(task)) else {
        return;
    };
    in_flight.0 = None;

    plugins_res.0 = plugins;
    info!(
        scanned = result.scanned,
        new = result.new,
        failed = result.failed,
        blacklisted = result.blacklisted,
        "plugin rescan complete"
    );
    scanned.write(PluginsScanned(result));
}
