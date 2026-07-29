//! Plugin health: liveness polling, proactive state snapshots, and teardown
//! that does not block the frame.
//!
//! # Why a state and not a bool
//!
//! The engine exposes liveness as a single latched `AtomicBool`. That one flag
//! collapses five distinct conditions — the peer died, a reply timed out, a
//! frame was undecodable, a length prefix was absurd, the protocol version
//! disagreed — and the `BridgeError` carrying the real cause is discarded before
//! it reaches us. It is also *write-once*: nothing ever clears it.
//!
//! Two consequences shape this module:
//!
//! - **A failed call returns before the flag is published.** The bridge marks
//!   the crash on its own thread with a `Release` store, so a system that checks
//!   immediately after a failed call can legitimately see `false`. Reacting to
//!   one observation races the publish.
//! - **`!is_crashed()` does not mean healthy.** A peer that answers a
//!   `GetParameter` with a well-formed reply of the *wrong kind* leaves the flag
//!   clear while the call returns `None`.
//!
//! So health is debounced: [`PluginStatus::Failing`] counts consecutive
//! observations and only [`PluginStatus::Dead`] unwires. The cost is that a
//! genuinely dead plugin emits silence for a few extra frames — which it was
//! emitting anyway, since a crashed bridge returns zeros rather than erroring.
//!
//! # Why snapshots have to be proactive
//!
//! `save_state` short-circuits on the crash flag and returns `None`. By the time
//! a host *knows* a plugin died, its state is already unreadable. Anything a
//! session wants to survive a crash must have been captured while the plugin was
//! healthy, which is what [`plugin_state_snapshot`] does.

use bevy_ecs::prelude::*;
use bevy_log::error;

use crate::plugin_host::editor::{PluginEditorOpen, PluginEmitter};

/// How many consecutive unhealthy observations before a plugin is declared dead.
///
/// Three rather than one because the crash flag is published from another thread
/// after the failing call returns, so a single observation races it. Three
/// frames is a handful of milliseconds — far below the threshold where a user
/// notices, and far above the publish window.
const DEATHS_BEFORE_DEAD: u8 = 3;

/// Frames between state snapshots of a healthy plugin.
///
/// A frame counter rather than a `Timer`: `bevy_time` is not a dependency of
/// this crate and nothing else in it is periodic, so a real clock would add both
/// a dependency and a convention for one system. The exact cadence does not
/// matter — this is a "something is better than nothing before a crash" backstop,
/// not a scheduling guarantee. At 60fps this is roughly every eight seconds.
const SNAPSHOT_INTERVAL_FRAMES: u32 = 512;

/// What the host believes about a plugin's liveness.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum PluginStatus {
    /// Answering normally.
    #[default]
    Healthy,
    /// Recently unhealthy, but not yet written off — see the module docs on why
    /// one observation is not enough.
    Failing { consecutive: u8 },
    /// Written off. The node has been unwired; the plugin is not coming back
    /// without a fresh load, because the engine offers no relaunch.
    Dead { cause: String },
}

/// Liveness and last-known-good state for one loaded plugin.
///
/// Inserted by the load path alongside [`PluginEmitter`].
#[derive(Component, Debug, Default)]
pub struct PluginHealth {
    pub status: PluginStatus,
    /// The most recent state captured while healthy, if any.
    ///
    /// Private: this is a snapshot [`plugin_state_snapshot`] maintains, and a
    /// host that wrote to it would be claiming a plugin said something it did
    /// not. Read it with [`snapshot`](Self::snapshot) to restore into a
    /// replacement `PluginRequest`.
    last_snapshot: Option<Vec<u8>>,
    /// Frames since the last snapshot attempt.
    frames_since_snapshot: u32,
}

impl PluginHealth {
    /// The last state captured while this plugin was healthy.
    ///
    /// Feed it to a fresh [`PluginRequest`](super::PluginRequest) to reinstate a
    /// crashed plugin — there is no in-place recovery, so a replacement entity
    /// is the only route back.
    pub fn snapshot(&self) -> Option<&Vec<u8>> {
        self.last_snapshot.as_ref()
    }

    /// Whether this plugin has been written off.
    pub fn is_dead(&self) -> bool {
        matches!(self.status, PluginStatus::Dead { .. })
    }
}

// # On deferring teardown — deliberately not done
//
// An earlier draft parked departing handles in a graveyard resource to keep the
// bridge-thread join off the frame that removed them. Both halves of the premise
// were wrong, and the shape is recorded here so it is not re-proposed.
//
// **A handle clone defers nothing.** The subprocess lifetime lives in an
// `Arc<ProcessGuard>` shared between `PluginHandle` and the `PluginClient` in
// the graph; the subprocess dies when the *last* `Arc` drops. Parking a clone
// adds a reference rather than moving ownership, so the real teardown still
// happens when the graph releases the node — the graveyard would only have
// delayed the socket cleanup while keeping the process alive longer.
//
// **And that teardown must stay where it is.** The node is dropped by
// `commit_graph`, which is pinned to the main thread with a `NonSendMarker`
// precisely because an in-process plugin's `Drop` tears down a native editor
// window, and AppKit/Win32/X11 teardown off-main is a crash rather than a
// warning. Moving plugin drops anywhere else reintroduces the bug that marker
// exists to prevent.
//
// The join itself is also far shorter than its worst case suggests: the bridge
// thread parks with a one-millisecond timeout and the shutdown push unparks it
// immediately, so it waits out only the command actually in flight. The
// ten-second bound needs a `save_state` crossing the wire at that instant.

/// Poll liveness, and unwire plugins that have failed often enough to be
/// declared dead.
///
/// Removing `AudioNode` is the whole teardown: the `On<Remove, AudioNode>`
/// observers take the node out of the graph and the sender off the MIDI bus.
/// This system does not touch either, which is why it needs neither
/// `AudioGraphRes` nor `GraphDirty`.
pub fn plugin_health_poll(
    mut commands: Commands,
    mut plugins: Query<(Entity, &PluginEmitter, &mut PluginHealth)>,
) {
    for (entity, plugin, mut health) in plugins.iter_mut() {
        if health.is_dead() {
            continue;
        }
        if !plugin.handle.is_crashed() {
            // Any healthy observation resets the count: the debounce is for
            // *consecutive* failures, and a single late reply is not a death.
            if health.status != PluginStatus::Healthy {
                health.status = PluginStatus::Healthy;
            }
            continue;
        }

        let consecutive = match health.status {
            PluginStatus::Failing { consecutive } => consecutive.saturating_add(1),
            _ => 1,
        };

        if consecutive < DEATHS_BEFORE_DEAD {
            health.status = PluginStatus::Failing { consecutive };
            continue;
        }

        // The engine discards the `BridgeError` that carried the real cause, so
        // this is as specific as the host can be. Surfacing the true reason
        // needs an engine change; until then, do not invent one.
        let cause = "bridge reported the plugin as crashed".to_string();
        error!(
            "plugin '{}' declared dead ({cause}); unwiring entity {entity:?}",
            plugin.handle.name()
        );
        health.status = PluginStatus::Dead { cause };

        commands
            .entity(entity)
            .remove::<PluginEditorOpen>()
            .remove::<tutti_core::AudioNode>();
    }
}

/// Periodically capture a healthy plugin's state, so a later crash is
/// recoverable.
///
/// `save_state` is a blocking round-trip to the plugin subprocess — bounded by
/// the engine's state timeout, but not free — which is why this runs on an
/// interval rather than every frame. It is skipped entirely once a plugin is
/// failing: the call would block against a peer that is already not answering,
/// and would return `None` anyway.
pub fn plugin_state_snapshot(mut plugins: Query<(&PluginEmitter, &mut PluginHealth)>) {
    for (plugin, mut health) in plugins.iter_mut() {
        if health.status != PluginStatus::Healthy {
            continue;
        }
        health.frames_since_snapshot = health.frames_since_snapshot.saturating_add(1);
        if health.frames_since_snapshot < SNAPSHOT_INTERVAL_FRAMES {
            continue;
        }
        health.frames_since_snapshot = 0;

        // `None` means the plugin declined or the bridge went down between the
        // guard above and here. Keep the previous snapshot rather than
        // overwriting a good one with nothing.
        if let Some(state) = plugin.handle.save_state() {
            health.last_snapshot = Some(state);
        }
    }
}
