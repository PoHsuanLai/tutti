//! Async plugin-catalog scanning.
//!
//! Scanning walks the configured directories and **probes each plugin in its own
//! subprocess** — seconds per plugin cold, minutes for a large collection. It
//! cannot run on the Bevy main thread, and a host that only learns the final
//! tally cannot draw a progress bar for it.
//!
//! `tutti-plugin` already solves both. [`Plugins::spawn_rescan`] spawns the scan on
//! its own named thread over a snapshot of the catalog and hands back a `Scan`
//! carrying a per-plugin [`ScanProgress`] stream and the deltas the scan finds.
//! [`poll_scan`] is the tick that applies them.
//!
//! # Why the scan is not on `AsyncComputeTaskPool`
//!
//! Because it does not run to completion in bounded time. That pool defaults to
//! ~4 threads, and [`graph::pump`](crate::graph::pump) documents what happens when
//! long-lived work occupies them: soundfont decodes and everything else stop
//! running, with no error anywhere. The engine already puts the scan on a
//! dedicated thread; this module only polls its channels.
//!
//! # Why [`PluginsRes`] stays in the world
//!
//! The scan works on a snapshot, so the catalog is neither moved nor locked:
//! it stays readable for the whole scan, and an edit made meanwhile (a
//! `blacklist`, an `unblacklist`, a `register_path`) is not lost. The world's
//! one catalog is the table's one owner; [`poll_scan`] runs on that thread and
//! applies each delta through `Plugins::poll_rescan`, so a system reading
//! `PluginsRes` sees a file's old row or its new one, never a half state.
//!
//! [`Plugins::spawn_rescan`]: tutti_plugin::catalog::Plugins::spawn_rescan

use bevy_ecs::message::Messages;
use bevy_ecs::prelude::*;
use bevy_log::{debug, info, warn};

use tutti_plugin_db::{Scan, ScanPhase, ScanPoll, ScanProgress, ScanResult};

use crate::plugin_host::PluginsRes;

/// Request to (re)scan the plugin catalog.
///
/// While the scan runs [`PluginsRes`] stays present and readable; rows change
/// as [`poll_scan`] applies what the scan finds.
#[derive(Message, Debug)]
pub struct RescanPlugins;

/// One plugin examined, or a phase change. Forwarded verbatim from the
/// scanner's progress stream so a UI can render "scanning 41/230 — Foo.vst3".
#[derive(Message, Debug, Clone)]
pub struct ScanProgressed(
    /// The scanner's own progress record, unmodified.
    pub ScanProgress,
);

/// Emitted once a rescan finishes and its last row is in [`PluginsRes`].
/// Carries the tally.
///
/// `ScanResult::newly_blacklisted` is the field worth acting on. The scanner
/// blacklists plugins that crashed or hung, *and* the dead-man's pedal
/// blacklists on force-quit / power loss / OOM-kill, so false positives are
/// expected. Blacklisted files are excluded from `entries()`/`iter()`, which
/// means a hidden plugin is invisible with no explanation unless the host says
/// something — see [`catalog`](super::catalog) for the un-blacklist path.
#[derive(Message, Debug)]
pub struct PluginsScanned(
    /// The scan tally: scanned, new, failed, blacklisted, newly blacklisted.
    pub ScanResult,
);

/// What the catalog is doing, for a host that wants to render it.
///
/// Mirrors the scan rather than duplicating it: `Scanning` is only ever written
/// from a [`ScanProgress`] the scanner sent, and cleared when the scan ends.
/// Nothing here is derivable from anything else here.
#[derive(Resource, Debug, Clone, Default, PartialEq, Eq)]
pub enum PluginCatalogState {
    /// No scan running.
    #[default]
    Idle,
    /// A scan is in flight. [`PluginsRes`] is present and readable throughout.
    ///
    /// `total` stays 0 until the scanner leaves [`ScanPhase::Discovery`] — it
    /// cannot know how many plugins exist until it has walked the directories.
    Scanning {
        /// How many plugins have been examined so far.
        current: usize,
        /// How many there are in total, or 0 during [`ScanPhase::Discovery`].
        total: usize,
        /// The plugin being examined right now.
        path: std::path::PathBuf,
        /// Which stage of the scan this is.
        phase: ScanPhase,
    },
}

/// The in-flight scan. Absent when no scan is running.
///
/// Private field: [`start_scan`] inserts this, a host does not build one — the
/// scan must come from the catalog in [`PluginsRes`], whose rows it updates.
#[derive(Resource, Debug)]
pub struct InFlightScan {
    scan: Scan,
}

/// On [`RescanPlugins`], start a scan of the catalog's directories.
///
/// Exclusive so the in-flight marker is in the world before any later system
/// of the frame looks for it.
///
/// If the pedal is busy (a worker from an earlier scan has not exited yet, or
/// another catalog shares the pedal path) the request is written again for
/// the next frame. It retries every frame for as long as the pedal stays busy,
/// so a pedal held for good by another catalog makes the request wait forever
/// (silently, at debug level).
pub fn start_scan(world: &mut World) {
    // Drain regardless; one request is as good as five.
    let requested = world
        .get_resource_mut::<Messages<RescanPlugins>>()
        .is_some_and(|mut m| m.drain().count() > 0);
    if !requested {
        return;
    }
    if world.contains_resource::<InFlightScan>() {
        warn!("plugin rescan already in flight; ignoring new request");
        return;
    }
    let Some(mut plugins) = world.get_resource_mut::<PluginsRes>() else {
        warn!("plugin rescan requested with no PluginsRes; ignoring");
        return;
    };

    let scan = match plugins.0.spawn_rescan() {
        Ok(scan) => scan,
        Err(busy) => {
            // A worker from an earlier scan is still winding down (or another
            // catalog shares the pedal). Ask again next frame rather than drop
            // the request.
            debug!("plugin rescan deferred: {busy}");
            world.write_message(RescanPlugins);
            return;
        }
    };
    world.insert_resource(InFlightScan { scan });
    world.insert_resource(PluginCatalogState::Scanning {
        current: 0,
        total: 0,
        path: std::path::PathBuf::new(),
        phase: ScanPhase::Discovery,
    });
    info!("plugin rescan started");
}

/// Forwards scan progress, applies the scan's deltas to [`PluginsRes`], and
/// reports the tally once the scan finishes.
///
/// Exclusive because it touches the catalog and removes the in-flight marker in
/// one step.
///
/// Progress is drained *before* the deltas are applied so the terminal
/// [`ScanPhase::Complete`] event is delivered rather than lost to the same frame
/// that tears the scan down.
pub fn poll_scan(world: &mut World) {
    if !world.contains_resource::<InFlightScan>() {
        return;
    }

    let progressed: Vec<ScanProgress> = world
        .resource::<InFlightScan>()
        .scan
        .progress_rx
        .try_iter()
        .collect();

    if let Some(last) = progressed.last() {
        world.insert_resource(PluginCatalogState::Scanning {
            current: last.current,
            total: last.total,
            path: last.current_path.clone(),
            phase: last.phase,
        });
    }
    if !progressed.is_empty()
        && let Some(mut writer) = world.get_resource_mut::<Messages<ScanProgressed>>()
    {
        for progress in progressed {
            writer.write(ScanProgressed(progress));
        }
    }

    let poll = world.resource_scope(|world, in_flight: Mut<InFlightScan>| {
        world
            .get_resource_mut::<PluginsRes>()
            .map(|mut plugins| plugins.0.poll_rescan(&in_flight.scan))
    });

    let result = match poll {
        Some(ScanPoll::Running) => return,
        Some(ScanPoll::Done(result)) => Some(result),
        Some(ScanPoll::Died) => None,
        // The host removed the catalog mid-scan: there is nothing to apply the
        // deltas to. Dropping the scan stops its worker after the file in
        // flight.
        None => {
            world.remove_resource::<InFlightScan>();
            world.insert_resource(PluginCatalogState::Idle);
            warn!("plugin rescan abandoned: PluginsRes was removed while it ran");
            return;
        }
    };

    world.remove_resource::<InFlightScan>();
    world.insert_resource(PluginCatalogState::Idle);

    match result {
        Some(result) => {
            info!(
                scanned = result.scanned,
                new = result.new,
                failed = result.failed,
                blacklisted = result.blacklisted,
                newly_blacklisted = result.newly_blacklisted,
                "plugin rescan complete"
            );
            if let Some(mut writer) = world.get_resource_mut::<Messages<PluginsScanned>>() {
                writer.write(PluginsScanned(result));
            }
        }
        // The worker ended without a tally, so a probe panicked. Rows it sent
        // before that are applied, so the catalog is intact and usable.
        None => warn!("plugin rescan ended without a result: the scan thread panicked"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_plugin::catalog::{CatalogConfig, NO_SCAN_DIRS, Plugins};

    fn world(dir: &tempfile::TempDir) -> World {
        let mut world = World::new();
        world.init_resource::<Messages<RescanPlugins>>();
        world.init_resource::<Messages<ScanProgressed>>();
        world.init_resource::<Messages<PluginsScanned>>();
        world.init_resource::<PluginCatalogState>();
        // No scan directories: the scan finishes at once and probes nothing.
        let config = CatalogConfig::new(dir.path().join("plugins.json"), NO_SCAN_DIRS);
        world.insert_resource(PluginsRes::new(Plugins::empty(config)));
        world
    }

    /// The catalog stays in the world for the whole scan, and the scan ends in
    /// `Idle` with a tally. Mutation: remove `PluginsRes` from the world in
    /// `start_scan` (the first assertion after the start fails).
    #[test]
    fn the_catalog_stays_in_the_world_while_a_scan_runs() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut world = world(&dir);
        world.write_message(RescanPlugins);

        start_scan(&mut world);

        assert!(world.contains_resource::<PluginsRes>());
        assert!(world.contains_resource::<InFlightScan>());
        assert!(matches!(
            *world.resource::<PluginCatalogState>(),
            PluginCatalogState::Scanning { .. }
        ));

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while world.contains_resource::<InFlightScan>() {
            assert!(world.contains_resource::<PluginsRes>());
            assert!(std::time::Instant::now() < deadline, "the scan never ended");
            poll_scan(&mut world);
            std::thread::yield_now();
        }

        assert_eq!(
            *world.resource::<PluginCatalogState>(),
            PluginCatalogState::Idle
        );
        let scanned: Vec<_> = world
            .resource_mut::<Messages<PluginsScanned>>()
            .drain()
            .collect();
        assert_eq!(scanned.len(), 1);
        assert_eq!(scanned[0].0.scanned, 0);
    }

    /// A host that removes the catalog mid-scan ends the scan cleanly: the
    /// in-flight marker goes and the state returns to idle, instead of the
    /// system polling a scan with nothing to apply it to forever.
    /// Mutation: `return` before the `remove_resource::<InFlightScan>()` in the
    /// `None` arm of `poll_scan`.
    #[test]
    fn removing_the_catalog_mid_scan_ends_the_scan() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut world = world(&dir);
        world.write_message(RescanPlugins);
        start_scan(&mut world);
        world.remove_resource::<PluginsRes>();

        poll_scan(&mut world);

        assert!(!world.contains_resource::<InFlightScan>());
        assert_eq!(
            *world.resource::<PluginCatalogState>(),
            PluginCatalogState::Idle
        );
    }

    /// A request that arrives while the pedal is busy starts no scan, is not
    /// lost across frames (the message buffers are swapped between the
    /// attempts, as a frame boundary does), and starts the scan once the other
    /// worker has exited.
    /// Mutation: drop the `world.write_message(RescanPlugins)` line in
    /// `start_scan`.
    #[test]
    fn a_rescan_requested_while_the_pedal_is_busy_starts_when_it_clears() {
        use std::sync::{Mutex, mpsc};
        use tutti_plugin_db::{PluginDb, ScanPoll, Scanner};

        let dir = tempfile::TempDir::new().unwrap();
        let plugins_dir = dir.path().join("plugins");
        std::fs::create_dir(&plugins_dir).unwrap();
        std::fs::write(plugins_dir.join("a.clap"), b"x").unwrap();
        let mut world = world(&dir);
        // The catalog's pedal sits beside its database file.
        let (parked_tx, parked_rx) = mpsc::channel::<()>();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let release_rx = Mutex::new(release_rx);
        let other = Scanner::new(dir.path().join(".scanning"), move |path, format| {
            parked_tx.send(()).unwrap();
            let _ = release_rx
                .lock()
                .unwrap()
                .recv_timeout(std::time::Duration::from_secs(30));
            Ok(tutti_plugin_db::probe_plugin_fallback(path, format))
        });
        let mut other_db = PluginDb::new();
        let parked = other
            .spawn(&mut other_db, [&plugins_dir], |_| Ok(()))
            .unwrap();
        parked_rx
            .recv_timeout(std::time::Duration::from_secs(30))
            .unwrap();

        world.write_message(RescanPlugins);
        for _ in 0..3 {
            start_scan(&mut world);
            assert!(!world.contains_resource::<InFlightScan>());
            world.resource_mut::<Messages<RescanPlugins>>().update();
        }

        release_tx.send(()).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while let ScanPoll::Running = parked.poll(&mut other_db) {
            assert!(std::time::Instant::now() < deadline);
            std::thread::yield_now();
        }
        start_scan(&mut world);

        assert!(
            world.contains_resource::<InFlightScan>(),
            "the request was lost"
        );
    }
}
