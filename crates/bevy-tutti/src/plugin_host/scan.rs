//! Async plugin-catalog scanning.
//!
//! Scanning walks the configured directories and **probes each plugin in its own
//! subprocess** — seconds per plugin cold, minutes for a large collection. It
//! cannot run on the Bevy main thread, and a host that only learns the final
//! tally cannot draw a progress bar for it.
//!
//! `tutti-plugin` already solves both. [`Plugins::rescan`] spawns the scan on
//! its own named thread and hands back a `ScanHandle` carrying a per-plugin
//! [`ScanProgress`] stream, plus a `ScanTicket` whose `try_join` is documented
//! "for frame-driven hosts (a Bevy system, a UI tick) that must not stall".
//! [`poll_scan`] is that tick.
//!
//! # Why the scan is not on `AsyncComputeTaskPool`
//!
//! Because it does not run to completion in bounded time. That pool defaults to
//! ~4 threads, and [`graph::io`](crate::graph) documents what happens when
//! long-lived work occupies them: soundfont decodes and everything else stop
//! running, with no error anywhere. The engine already puts the scan on a
//! dedicated thread; this module only polls its channels.
//!
//! # Why the catalog moves rather than being rebuilt
//!
//! [`Plugins::rescan`] **consumes** the catalog — it moves onto the scan thread,
//! and the ticket is how ownership returns. So this module removes
//! [`PluginsRes`] from the world for the duration and re-inserts it on
//! completion: "the catalog is away being scanned" becomes a state you observe
//! by its absence, not a flag that can disagree with reality.
//!
//! The previous version left `PluginsRes` in place and built a *fresh*
//! `Plugins::with_json_catalog(..)` inside a task, swapping the whole resource
//! when it finished. That discarded every unflushed in-memory edit — a user's
//! `blacklist`, `unblacklist` or `register_path` since the last `flush` — on
//! every rescan, and hardcoded the JSON impl over whatever catalog the host had
//! installed. Both engine scan paths are shaped specifically to prevent the
//! first (`rescan_sync` moves the live catalog through the scanner;
//! [`Plugins::rescan`] returns it via the ticket), and rebuilding opted out.
//!
//! [`Plugins::rescan`]: tutti_plugin::catalog::Plugins::rescan

use bevy_ecs::message::Messages;
use bevy_ecs::prelude::*;
use bevy_log::{info, warn};

use tutti_plugin::catalog::{Plugins, ScanHandle, ScanPhase, ScanProgress, ScanResult, ScanTicket};

use crate::plugin_host::PluginsRes;

/// Request to (re)scan the plugin catalog.
///
/// While the scan runs [`PluginsRes`] is **absent** — the catalog is on the scan
/// thread. Systems that read it already take `Option<Res<PluginsRes>>`; it is
/// one of the two resources `engine_ready` deliberately does not cover.
#[derive(Message)]
pub struct RescanPlugins;

/// One plugin examined, or a phase change. Forwarded verbatim from the
/// scanner's progress stream so a UI can render "scanning 41/230 — Foo.vst3".
#[derive(Message, Debug, Clone)]
pub struct ScanProgressed(pub ScanProgress);

/// Emitted once a rescan finishes and the fresh catalog is back in
/// [`PluginsRes`]. Carries the tally.
///
/// `ScanResult::newly_blacklisted` is the field worth acting on. The scanner
/// blacklists plugins that crashed or hung, *and* the dead-man's pedal
/// blacklists on force-quit / power loss / OOM-kill, so false positives are
/// expected. Blacklisted records are excluded from `records()`/`iter()`, which
/// means a hidden plugin is invisible with no explanation unless the host says
/// something — see [`catalog`](super::catalog) for the un-blacklist path.
#[derive(Message)]
pub struct PluginsScanned(pub ScanResult);

/// What the catalog is doing, for a host that wants to render it.
///
/// Mirrors the scan rather than duplicating it: `Scanning` is only ever written
/// from a [`ScanProgress`] the scanner sent, and cleared when the ticket
/// resolves. Nothing here is derivable from anything else here.
#[derive(Resource, Debug, Clone, Default, PartialEq, Eq)]
pub enum PluginCatalogState {
    /// No scan running; [`PluginsRes`] is present.
    #[default]
    Idle,
    /// A scan is in flight and [`PluginsRes`] is absent until it completes.
    ///
    /// `total` stays 0 until the scanner leaves [`ScanPhase::Discovery`] — it
    /// cannot know how many plugins exist until it has walked the directories.
    Scanning {
        current: usize,
        total: usize,
        path: std::path::PathBuf,
        phase: ScanPhase,
    },
}

/// The in-flight scan. Absent when no scan is running.
///
/// Private fields: [`start_scan`] inserts this, a host does not build one — the
/// handle and ticket must come from the same [`Plugins::rescan`] call, and
/// pairing two halves from different scans would deadlock or cross catalogs.
#[derive(Resource)]
pub struct InFlightScan {
    handle: ScanHandle,
    /// `Option` because `try_join` consumes the ticket: [`poll_scan`] takes it
    /// by value and puts it back when the scan has not finished. That is also
    /// what makes a double-join unrepresentable.
    ticket: Option<ScanTicket>,
}

/// On [`RescanPlugins`], move the catalog onto a scan thread.
///
/// Exclusive because [`Plugins::rescan`] consumes the `Plugins`: the resource
/// has to *leave* the world, which a `ResMut` borrow cannot express.
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
    let Some(plugins) = world.remove_resource::<PluginsRes>() else {
        warn!("plugin rescan requested with no PluginsRes; ignoring");
        return;
    };

    let (handle, ticket) = plugins.0.rescan();
    world.insert_resource(InFlightScan {
        handle,
        ticket: Some(ticket),
    });
    world.insert_resource(PluginCatalogState::Scanning {
        current: 0,
        total: 0,
        path: std::path::PathBuf::new(),
        phase: ScanPhase::Discovery,
    });
    info!("plugin rescan started");
}

/// Forward scan progress, and re-install the catalog once the scan finishes.
///
/// Exclusive for the same reason as [`start_scan`] — the catalog comes back by
/// value and must be re-inserted as a resource.
///
/// Progress is drained *before* the join is attempted so the terminal
/// [`ScanPhase::Complete`] event is delivered rather than lost to the same frame
/// that tears the scan down.
pub fn poll_scan(world: &mut World) {
    if !world.contains_resource::<InFlightScan>() {
        return;
    }

    let mut progressed: Vec<ScanProgress> = Vec::new();
    let mut finished: Option<(Plugins, Option<ScanResult>)> = None;

    {
        let mut in_flight = world.resource_mut::<InFlightScan>();
        // Unbounded channel: nothing is dropped when a frame is slow, and the
        // scanner never blocks waiting for us to read.
        while let Ok(progress) = in_flight.handle.progress_rx.try_recv() {
            progressed.push(progress);
        }

        // The scan thread sends the catalog *before* the result, so a `try_join`
        // that succeeds means the tally is already waiting.
        if let Some(ticket) = in_flight.ticket.take() {
            match ticket.try_join() {
                Ok(plugins) => {
                    let result = in_flight.handle.result_rx.try_recv().ok();
                    finished = Some((plugins, result));
                }
                // Still scanning — put it back for the next tick.
                Err(ticket) => in_flight.ticket = Some(ticket),
            }
        }
    }

    if let Some(last) = progressed.last() {
        world.insert_resource(PluginCatalogState::Scanning {
            current: last.current,
            total: last.total,
            path: last.current_path.clone(),
            phase: last.phase,
        });
    }
    if !progressed.is_empty() {
        if let Some(mut writer) = world.get_resource_mut::<Messages<ScanProgressed>>() {
            for progress in progressed {
                writer.write(ScanProgressed(progress));
            }
        }
    }

    let Some((plugins, result)) = finished else {
        return;
    };

    world.remove_resource::<InFlightScan>();
    world.insert_resource(PluginsRes::new(plugins));
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
        // The catalog came back but the tally did not: the scan thread panicked
        // between the two sends. The catalog is intact and usable, so this is a
        // warning rather than a lost scan.
        None => warn!(
            "plugin rescan returned a catalog but no result; the scan thread may have panicked"
        ),
    }
}
