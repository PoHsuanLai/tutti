//! Plugin health: liveness polling, proactive state snapshots, and teardown
//! that does not block the frame.
//!
//! # Why a state and not a bool
//!
//! The engine reports liveness as [`tutti_plugin::handles::PluginStatus`] — a
//! latched death plus the reason for it. That answers *whether* a plugin died
//! and *why*, but not the condition this module exists for:
//!
//! - **A failed call returns before the death is published.** The bridge marks
//!   the crash on its own thread with a `Release` store, so a system that checks
//!   immediately after a failed call can legitimately see the plugin alive.
//!   Reacting to one observation races the publish.
//! - **`Alive` does not mean healthy.** A peer that answers a `GetParameter`
//!   with a well-formed reply of the *wrong kind* leaves the engine reporting
//!   `Alive` while the call returns `None`. The engine documents this and
//!   declines to model it, because how many failures over how long is a host's
//!   policy — this module is that host.
//!
//! So liveness is debounced here: [`PluginLiveness::Failing`] counts consecutive
//! observations and only [`PluginLiveness::Dead`] unwires. The cost is that a
//! genuinely dead plugin emits silence for a few extra frames — which it was
//! emitting anyway, since a crashed bridge returns zeros rather than erroring.
//!
//! # Why the cause is read, not subscribed to
//!
//! `PluginHandle::on_invalidate` fires [`PluginInvalidation::Crashed`] with the
//! same cause this module reports, and a callback would learn of a death
//! sooner. It is deliberately not used, for the reason
//! [`plugin_latency_poll`](super::latency::plugin_latency_poll) gives: a
//! callback cannot touch the `World`, so it would need a channel and a drain
//! system — a second route to a value the engine already owns and will hand
//! over on request.
//!
//! Reading it here keeps one owner. The engine latches the cause where the
//! crash is noticed and `status()` returns it at any later time, so polling
//! loses nothing but the few frames the debounce was already spending. A
//! subscriber would arrive earlier and still have to wait out the same count.
//!
//! [`PluginInvalidation::Crashed`]: tutti_plugin::handles::PluginInvalidation::Crashed
//!
//! # Why snapshots have to be proactive
//!
//! `save_state` short-circuits on the crash flag and returns `None`. By the time
//! a host *knows* a plugin died, its state is already unreadable. Anything a
//! session wants to survive a crash must have been captured while the plugin was
//! healthy, which is what [`plugin_state_snapshot`] does.

use bevy_ecs::prelude::*;
use bevy_log::error;

// Aliased at the import rather than named in full at the use site: this module
// defines its own `PluginStatus`-shaped type (`PluginLiveness`), and two types
// one word apart in the same file is how a reader mistakes the host's belief
// for the engine's observation.
use tutti_plugin::handles::PluginStatus as EngineStatus;

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
///
/// Distinct from [`tutti_plugin::handles::PluginStatus`], which reports what the
/// *engine* has observed: a latched death, or nothing yet. This adds the
/// debounce between them — `Failing` is a belief this module forms by counting,
/// and has no engine counterpart by design.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum PluginLiveness {
    /// Answering normally.
    #[default]
    Healthy,
    /// Recently unhealthy, but not yet written off — see the module docs on why
    /// one observation is not enough.
    Failing { consecutive: u8 },
    /// Written off. The node has been unwired; the plugin is not coming back
    /// without a fresh load, because the engine offers no relaunch.
    ///
    /// The cause comes from the engine's latch, so it names the actual failure
    /// — a refused connection, a protocol mismatch, a dropped stream.
    Dead { cause: String },
}

/// Liveness and last-known-good state for one loaded plugin.
///
/// Inserted by the load path alongside [`PluginEmitter`].
#[derive(Component, Debug, Default)]
pub struct PluginHealth {
    pub status: PluginLiveness,
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
        matches!(self.status, PluginLiveness::Dead { .. })
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
        let EngineStatus::Dead { cause } = plugin.handle.status() else {
            // Any healthy observation resets the count: the debounce is for
            // *consecutive* failures, and a single late reply is not a death.
            if health.status != PluginLiveness::Healthy {
                health.status = PluginLiveness::Healthy;
            }
            continue;
        };

        let consecutive = match health.status {
            PluginLiveness::Failing { consecutive } => consecutive.saturating_add(1),
            _ => 1,
        };

        if consecutive < DEATHS_BEFORE_DEAD {
            health.status = PluginLiveness::Failing { consecutive };
            continue;
        }

        // The engine's latched reason, not a placeholder. It is captured where
        // the crash is noticed rather than reconstructed from the flag, so it
        // names the actual failure — a refused connection, a protocol
        // mismatch, a dropped stream — including for a plugin that died before
        // this host could have subscribed to anything.
        error!(
            "plugin '{}' declared dead ({cause}); unwiring entity {entity:?}",
            plugin.handle.name()
        );
        health.status = PluginLiveness::Dead { cause };

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
        if health.status != PluginLiveness::Healthy {
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

#[cfg(test)]
mod tests {
    use super::*;
    use bevy_app::prelude::*;
    use std::sync::Arc;
    use tutti_plugin::handles::{OptionalCapabilities, ParamAddress, PluginHandle};
    use tutti_plugin::server::ParameterInfo;

    /// A backend whose liveness is whatever the test says it is.
    ///
    /// `PluginClient::new` launches a subprocess, so no test can put a real
    /// plugin here — but `PluginHandle::from_backend` takes any `HostParams +
    /// HostState`, which makes the *system* testable rather than only the
    /// decision rule extracted out of it. That matters for the case this module
    /// is about: the cause has to survive the trip from the backend, through the
    /// handle, into a component, and only an end-to-end assertion sees that.
    struct FakeBackend {
        cause: Option<String>,
    }

    impl tutti_plugin::backend::HostParams for FakeBackend {
        fn parameter_descriptors(&self) -> Option<Vec<ParameterInfo>> {
            None
        }
        fn parameter_value(&self, _id: ParamAddress) -> Option<f32> {
            None
        }
        fn set_parameter_value(&self, _id: ParamAddress, _value: f32) {}
        fn is_crashed(&self) -> bool {
            self.cause.is_some()
        }
        fn crash_cause(&self) -> Option<String> {
            self.cause.clone()
        }
    }

    impl tutti_plugin::backend::HostState for FakeBackend {
        fn save_state(&self) -> Option<Vec<u8>> {
            None
        }
        fn load_state(&self, _data: &[u8]) {}
    }

    /// A handle over a backend that reports `cause`, dead if `Some`.
    fn handle_reporting(cause: Option<&str>) -> PluginHandle {
        let backend = Arc::new(FakeBackend {
            cause: cause.map(str::to_string),
        });
        let (sender, _receiver) =
            tutti_midi_runtime::MidiMailbox::pair(tutti_midi_types::MidiUnitId::next());
        PluginHandle::from_backend(
            backend,
            OptionalCapabilities::default(),
            Default::default(),
            Default::default(),
            Default::default(),
            sender,
        )
    }

    fn test_app() -> App {
        let mut app = App::new();
        app.insert_resource(crate::graph::AudioGraphRes(tutti_core::dsp::Net::new(0, 2)));
        app.add_systems(Update, plugin_health_poll);
        app
    }

    /// Spawn a plugin that reports `cause`, run `frames` updates, return the
    /// entity.
    ///
    /// `AudioNode` is a real graph node rather than a fabricated id: its
    /// *removal* is the whole teardown this system performs, so the component
    /// has to be present for the unwire to be observable at all. A `dc` node
    /// stands in for the plugin — nothing here reads what the node computes.
    fn run(app: &mut App, cause: Option<&str>, frames: usize) -> Entity {
        let id = {
            let mut graph = app
                .world_mut()
                .resource_mut::<crate::graph::AudioGraphRes>();
            graph.0.push(Box::new(tutti_core::dsp::dc(0.0)))
        };
        let entity = app
            .world_mut()
            .spawn((
                PluginEmitter {
                    handle: handle_reporting(cause),
                },
                PluginHealth::default(),
                tutti_core::AudioNode(id),
            ))
            .id();
        for _ in 0..frames {
            app.update();
        }
        entity
    }

    /// The engine's latched reason reaches the component verbatim.
    ///
    /// This is the whole point of the change: the host used to write the fixed
    /// string "bridge reported the plugin as crashed" for every death alike,
    /// because the flag was a bool and the `BridgeError` behind it was dropped.
    /// Asserting on the *content* is what distinguishes reading the engine's
    /// cause from inventing one — an assertion that merely checked for `Dead`
    /// would pass against the placeholder.
    #[test]
    fn a_dead_plugin_reports_the_engines_cause_not_a_placeholder() {
        let mut app = test_app();
        let entity = run(
            &mut app,
            Some("could not connect to plugin-server: No such file or directory"),
            DEATHS_BEFORE_DEAD as usize,
        );

        let health = app.world().get::<PluginHealth>(entity).unwrap();
        assert_eq!(
            health.status,
            PluginLiveness::Dead {
                cause: "could not connect to plugin-server: No such file or directory".to_string(),
            },
            "the cause must be the engine's latched reason, carried through unchanged"
        );
    }

    /// A plugin is not written off on the first bad observation.
    ///
    /// The debounce is the reason this module keeps a state rather than
    /// mirroring the engine's: the crash is published from the bridge thread
    /// after the failing call returns, so one look can race the publish. Pinned
    /// at every frame below the threshold, because an off-by-one here would
    /// unwire a live plugin.
    #[test]
    fn a_crash_is_not_declared_until_the_debounce_elapses() {
        for frames in 1..DEATHS_BEFORE_DEAD as usize {
            let mut app = test_app();
            let entity = run(&mut app, Some("stream closed"), frames);

            let health = app.world().get::<PluginHealth>(entity).unwrap();
            assert_eq!(
                health.status,
                PluginLiveness::Failing {
                    consecutive: frames as u8
                },
                "at {frames} observation(s) the plugin is failing, not dead"
            );
            assert!(
                app.world().get::<tutti_core::AudioNode>(entity).is_some(),
                "a failing plugin must stay wired — unwiring it early silences a live plugin"
            );
        }
    }

    /// Declaring a plugin dead removes `AudioNode`, which is the whole teardown.
    ///
    /// The `On<Remove, AudioNode>` observers take the node out of the graph and
    /// the sender off the MIDI bus, so this system's only job is the removal. A
    /// status set without it would leave a dead plugin's node still processing.
    #[test]
    fn declaring_death_unwires_the_node() {
        let mut app = test_app();
        let entity = run(&mut app, Some("stream closed"), DEATHS_BEFORE_DEAD as usize);

        assert!(
            app.world().get::<tutti_core::AudioNode>(entity).is_none(),
            "a dead plugin must be unwired by removing AudioNode"
        );
    }

    /// A plugin the engine reports as alive is never written off, however long
    /// it runs.
    ///
    /// The negative case, and worth its own test: every assertion above fires
    /// only for a backend already reporting a crash, so none of them would
    /// notice a poll that declared death unconditionally.
    #[test]
    fn a_live_plugin_is_never_declared_dead() {
        let mut app = test_app();
        let entity = run(&mut app, None, DEATHS_BEFORE_DEAD as usize + 5);

        let health = app.world().get::<PluginHealth>(entity).unwrap();
        assert_eq!(health.status, PluginLiveness::Healthy);
        assert!(
            app.world().get::<tutti_core::AudioNode>(entity).is_some(),
            "a healthy plugin must stay wired"
        );
    }
}
