//! Registering a single plugin, off the frame thread.
//!
//! # What is *not* here
//!
//! Most catalog operations need no wrapper. `blacklist`, `unblacklist`,
//! `clear_blacklist`, `remove`, `records`, `find`, `info`, `prune_missing`,
//! `recover_crash` and `flush` are all cheap, synchronous calls on
//! [`PluginsRes`] — a host reaches them through `ResMut<PluginsRes>` directly.
//! Wrapping each in a message-and-system pair would be a second way to say the
//! same thing, and the two would drift.
//!
//! Two facts a host does need to know, because nothing enforces them:
//!
//! - **Mutations are in-memory until `flush`.** `blacklist`, `unblacklist`,
//!   `remove` and [`ProbePlugin`] all edit the live catalog and nothing else. A
//!   host that never calls `PluginsRes::flush` loses them at exit.
//! - **Blacklisted records are invisible.** `records()`, `iter()` and `find()`
//!   all skip them, so a plugin hidden by a scan is simply absent with no
//!   explanation. `ScanResult::newly_blacklisted` on
//!   [`PluginsScanned`](super::PluginsScanned) is the signal to say something,
//!   and `Plugins::blacklisted()` lists them with reasons. The engine treats
//!   false positives as expected — the dead-man's pedal fires on force-quit and
//!   power loss, not only on a plugin crash — so an un-blacklist affordance is
//!   not optional.
//!
//! # What *is* here
//!
//! Probing one plugin, which is the one catalog operation that cannot be a
//! direct call: it spawns a subprocess and waits on a handshake, up to about
//! seven seconds if the plugin hangs. [`ProbePlugin`] runs it on the task pool.

use bevy_ecs::message::{Message, MessageReader, MessageWriter};
use bevy_ecs::prelude::*;
use bevy_log::{info, warn};
use bevy_tasks::{block_on, futures_lite::future, AsyncComputeTaskPool, Task};

use tutti_plugin::catalog::{PluginId, PluginRecord};
use tutti_plugin::BridgeError;

use crate::plugin_host::PluginsRes;

/// Probe one plugin file and add it to the catalog — the drag-a-`.vst3`-in path.
///
/// Cheaper than a rescan by the size of the plugin directory: this probes
/// exactly one file. Emits [`PluginProbed`] either way.
#[derive(Message, Debug, Clone)]
pub struct ProbePlugin(pub std::path::PathBuf);

/// Outcome of a [`ProbePlugin`].
///
/// `Ok` means the record is in the live catalog. It is **not** yet persisted —
/// call `PluginsRes::flush` for that.
#[derive(Message, Debug)]
pub struct PluginProbed {
    pub path: std::path::PathBuf,
    pub result: Result<PluginId, BridgeError>,
}

/// In-flight probes, keyed by path so the same file is not probed twice at once.
///
/// Private: [`start_probe`] fills this and [`poll_probes`] drains it.
#[derive(Resource, Default)]
pub struct InFlightProbes {
    tasks: std::collections::HashMap<std::path::PathBuf, Task<Result<PluginRecord, BridgeError>>>,
}

impl InFlightProbes {
    /// How many probes are running. Diagnostics only.
    pub fn len(&self) -> usize {
        self.tasks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tasks.is_empty()
    }
}

/// Spawn a probe per requested path.
///
/// The probe itself needs no catalog — it reads the file and asks a throwaway
/// subprocess what it is — so nothing is borrowed across the await. Only the
/// resulting record needs the catalog, and that happens in [`poll_probes`].
pub fn start_probe(
    mut requests: MessageReader<ProbePlugin>,
    mut in_flight: ResMut<InFlightProbes>,
) {
    for ProbePlugin(path) in requests.read() {
        if in_flight.tasks.contains_key(path) {
            continue; // already probing this one
        }
        let probe_path = path.clone();
        let task =
            AsyncComputeTaskPool::get().spawn(async move { PluginRecord::probe(&probe_path) });
        in_flight.tasks.insert(path.clone(), task);
    }
}

/// Install finished probes into the catalog.
///
/// Ungated on the engine, like the scan: probing touches the filesystem and a
/// throwaway subprocess, never the graph.
pub fn poll_probes(
    mut in_flight: ResMut<InFlightProbes>,
    plugins: Option<ResMut<PluginsRes>>,
    mut probed: MessageWriter<PluginProbed>,
) {
    if in_flight.tasks.is_empty() {
        return;
    }
    // Absent while a rescan owns the catalog. Leave the tasks in place — they
    // are finished either way, and the next frame can install them.
    let Some(mut plugins) = plugins else {
        return;
    };

    let mut done: Vec<(std::path::PathBuf, Result<PluginRecord, BridgeError>)> = Vec::new();
    for (path, task) in in_flight.tasks.iter_mut() {
        if let Some(result) = block_on(future::poll_once(task)) {
            done.push((path.clone(), result));
        }
    }

    for (path, result) in done {
        in_flight.tasks.remove(&path);
        let outcome = match result {
            Ok(record) => {
                info!("probed plugin '{}'", record.descriptor.name);
                Ok(plugins.0.register_record(record))
            }
            Err(e) => {
                warn!("failed to probe '{}': {e}", path.display());
                Err(e)
            }
        };
        probed.write(PluginProbed {
            path,
            result: outcome,
        });
    }
}
