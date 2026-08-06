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
use bevy_tasks::{block_on, futures_lite::future, AsyncComputeTaskPool, Task};

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
///
/// Not `Debug`: the in-flight snapshot is a `Task`, which is not.
#[derive(Component, Default)]
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
    /// The snapshot currently being fetched, if one is in flight.
    ///
    /// `save_state` is a blocking round-trip with a **ten-second** timeout, so
    /// it cannot run on the frame thread — see [`plugin_state_snapshot`]. This
    /// holds it on the task pool the same way [`PendingPlugin`] holds a load.
    ///
    /// Also the concurrency guard: `Some` means "already asking", which is what
    /// stops a plugin that takes longer than the interval from accumulating one
    /// task per interval against a peer that is not answering.
    ///
    /// [`PendingPlugin`]: super::load::PendingPlugin
    pending_snapshot: Option<Task<Option<Vec<u8>>>>,
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
/// # Why the fetch is off-thread
///
/// `save_state` is a blocking round-trip to the plugin subprocess, bounded by a
/// **ten-second** timeout. This system runs in `Update`, so calling it directly
/// put that whole worst case on the frame thread: a plugin that stopped
/// answering — precisely the plugin whose state is most worth having — froze the
/// app for ten seconds.
///
/// The interval mitigated the *cost* and not the *hazard*: it made the freeze
/// rare rather than short, and rare-and-catastrophic is the harder bug to
/// attribute. So the call moved to `AsyncComputeTaskPool`, the shape
/// [`plugin_load_start`](super::load::plugin_load_start) already uses for the
/// load itself, and this system only starts and collects.
///
/// Skipped entirely once a plugin is failing: the call would block against a
/// peer that is already not answering, and would return `None` anyway.
pub fn plugin_state_snapshot(mut plugins: Query<(&PluginEmitter, &mut PluginHealth)>) {
    for (plugin, mut health) in plugins.iter_mut() {
        // Collect first, so a snapshot that finished during this frame lands
        // before the interval is considered again. Draining after the start
        // branch would leave a completed task sitting for a whole extra
        // interval.
        if let Some(task) = health.pending_snapshot.as_mut() {
            match block_on(future::poll_once(task)) {
                // `None` from the plugin means it declined, or the bridge went
                // down while we were asking. Keep the previous snapshot rather
                // than overwriting a good one with nothing.
                Some(result) => {
                    health.pending_snapshot = None;
                    if let Some(state) = result {
                        health.last_snapshot = Some(state);
                    }
                }
                // Still fetching. Do not start another — see the field docs.
                None => continue,
            }
        }

        if health.status != PluginLiveness::Healthy {
            continue;
        }
        health.frames_since_snapshot = health.frames_since_snapshot.saturating_add(1);
        if health.frames_since_snapshot < SNAPSHOT_INTERVAL_FRAMES {
            continue;
        }
        health.frames_since_snapshot = 0;

        // The handle is cloned into the task rather than borrowed: it is
        // `Arc`-backed, so this is cheap, and it is what lets the round-trip
        // outlive the frame that asked for it.
        let handle = plugin.handle.clone();
        health.pending_snapshot =
            Some(AsyncComputeTaskPool::get().spawn(async move { handle.state().save_state() }));
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
        /// What `save_state` answers, and a count of how often it was asked.
        ///
        /// The count is what distinguishes "the snapshot is off-thread" from
        /// "the snapshot happens to work": a system that started a fresh task
        /// every frame would still produce the right bytes, and only the number
        /// of calls shows it.
        state: Option<Vec<u8>>,
        saves: Arc<std::sync::atomic::AtomicUsize>,
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
            self.saves
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            self.state.clone()
        }
        fn load_state(&self, _data: &[u8]) {}
    }

    /// A handle over a backend that reports `cause`, dead if `Some`.
    fn handle_reporting(cause: Option<&str>) -> PluginHandle {
        handle_with(cause, None, Arc::default())
    }

    /// A handle whose backend answers `state` and counts its `save_state` calls.
    fn handle_with(
        cause: Option<&str>,
        state: Option<Vec<u8>>,
        saves: Arc<std::sync::atomic::AtomicUsize>,
    ) -> PluginHandle {
        let backend = Arc::new(FakeBackend {
            cause: cause.map(str::to_string),
            state,
            saves,
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

    // ---- snapshots ---------------------------------------------------------

    /// An app running only the snapshot system, with a plugin that answers
    /// `state` and counts how often it was asked.
    fn snapshot_app(state: Option<Vec<u8>>) -> (App, Entity, Arc<std::sync::atomic::AtomicUsize>) {
        // The pool is process-global and may already exist from another test in
        // this binary — `get_or_init` rather than `init`, which panics on the
        // second call.
        bevy_tasks::AsyncComputeTaskPool::get_or_init(bevy_tasks::TaskPool::new);

        let mut app = App::new();
        app.add_systems(Update, plugin_state_snapshot);
        let saves = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let entity = app
            .world_mut()
            .spawn((
                PluginEmitter {
                    handle: handle_with(None, state, Arc::clone(&saves)),
                },
                PluginHealth::default(),
            ))
            .id();
        (app, entity, saves)
    }

    /// Run frames until the in-flight snapshot resolves, or give up.
    ///
    /// The fetch is on the task pool, so the frame that starts it is not the
    /// frame that collects it — a fixed frame count would be a race. Bounded so
    /// a genuine hang fails the test rather than hanging the suite.
    /// Run frames until one full snapshot cycle has started *and* resolved.
    ///
    /// Both halves are driven here rather than by a frame count at the call
    /// site: the fetch runs on the task pool, so the frame that starts it is
    /// never the frame that collects it, and the interval counter resets on the
    /// *start* frame — so "interval frames, then look" both races the pool and
    /// overshoots the next cycle. Only `pending_snapshot` transitioning
    /// `None → Some → None` identifies one cycle unambiguously.
    ///
    /// Bounded so a genuine hang fails the test rather than hanging the suite.
    fn run_until_collected(app: &mut App, entity: Entity) {
        let in_flight = |app: &App| {
            app.world()
                .get::<PluginHealth>(entity)
                .unwrap()
                .pending_snapshot
                .is_some()
        };

        let mut started = false;
        for _ in 0..(SNAPSHOT_INTERVAL_FRAMES as usize * 2 + 1000) {
            app.update();
            if !started {
                started = in_flight(app);
            } else if !in_flight(app) {
                return;
            }
        }
        panic!("the snapshot never started, or never resolved");
    }

    /// A captured snapshot reaches the component.
    ///
    /// End-to-end through the task pool: the bytes cross a thread boundary and
    /// land a frame or more after the one that asked. Without this the whole
    /// off-thread rework could silently capture nothing.
    #[test]
    fn a_snapshot_is_captured_and_stored() {
        let (mut app, entity, saves) = snapshot_app(Some(vec![1, 2, 3]));
        run_until_collected(&mut app, entity);

        let health = app.world().get::<PluginHealth>(entity).unwrap();
        assert_eq!(
            health.snapshot(),
            Some(&vec![1, 2, 3]),
            "the captured state must reach the component"
        );
        assert_eq!(
            saves.load(std::sync::atomic::Ordering::Relaxed),
            1,
            "one interval is one save, not one per frame"
        );
    }

    /// The fetch does not block the frame that starts it.
    ///
    /// This is the property the change exists for. `save_state` has a
    /// ten-second timeout, so a system calling it inline stalls the app for that
    /// long against a plugin that stopped answering. Asserting that the
    /// triggering frame leaves the task *in flight* is what distinguishes
    /// "spawned onto the pool" from "called inline and finished" — an inline
    /// call would have `pending_snapshot == None` and the bytes already stored
    /// on that same frame.
    #[test]
    fn the_triggering_frame_does_not_wait_for_the_plugin() {
        let (mut app, entity, _) = snapshot_app(Some(vec![7]));
        for _ in 0..SNAPSHOT_INTERVAL_FRAMES {
            app.update();
        }

        let health = app.world().get::<PluginHealth>(entity).unwrap();
        assert!(
            health.pending_snapshot.is_some(),
            "the fetch must still be in flight on the frame that started it"
        );
        assert_eq!(
            health.snapshot(),
            None,
            "nothing can be stored yet — that would mean the frame waited"
        );
    }

    /// Snapshots happen on the interval, not every frame.
    ///
    /// Three intervals is three saves — the cadence the whole design rests on,
    /// since `save_state` is a round-trip and a per-frame one would be a
    /// permanent load on the plugin.
    ///
    /// # What this deliberately does NOT cover
    ///
    /// The `pending_snapshot` guard against *restarting* an in-flight fetch is
    /// **not** pinned here, and cannot be in this crate's test configuration.
    /// `bevy-tutti` pulls `bevy_tasks` without its `multi_threaded` feature, so
    /// `AsyncComputeTaskPool` is the single-threaded pool: a spawned task runs
    /// on whichever thread polls it, which is the frame thread. A fake that
    /// blocked inside `save_state` to hold the fetch open therefore deadlocks
    /// the very `app.update()` that would release it — verified, it hangs
    /// indefinitely.
    ///
    /// So the in-flight window that the guard exists for does not occur here at
    /// all, and any assertion about it would pass whether the guard were
    /// present or absent. That was confirmed the expensive way: an earlier
    /// version of this test asserted the guard's behaviour and stayed **green**
    /// with the guard deleted.
    ///
    /// The guard is still correct and still load-bearing — the app crates
    /// (`dawai-frontend`, `dawai-model`, …) *do* enable `multi_threaded`, which
    /// is the configuration it protects. Covering it needs an integration test
    /// in a crate that enables that feature, which does not exist yet. Stated
    /// rather than faked, because a test that cannot fail claims coverage it
    /// does not have.
    #[test]
    fn snapshots_happen_on_the_interval_not_every_frame() {
        let (mut app, entity, saves) = snapshot_app(Some(vec![9]));
        for _ in 0..3 {
            run_until_collected(&mut app, entity);
        }

        assert_eq!(
            saves.load(std::sync::atomic::Ordering::Relaxed),
            3,
            "three completed intervals is three saves"
        );
    }

    /// A plugin that declines leaves the previous snapshot intact.
    ///
    /// `None` means the plugin refused or the bridge dropped mid-call. Storing
    /// it would replace a good recovery point with nothing, which is worse than
    /// a stale one — the snapshot exists precisely for the moment the plugin
    /// stops answering.
    #[test]
    fn a_declined_save_does_not_erase_the_previous_snapshot() {
        let (mut app, entity, _) = snapshot_app(None);
        run_until_collected(&mut app, entity);

        // Seed a good snapshot the way a successful capture would have, then
        // let another interval decline over the top of it.
        app.world_mut()
            .get_mut::<PluginHealth>(entity)
            .unwrap()
            .last_snapshot = Some(vec![4, 2]);
        run_until_collected(&mut app, entity);

        let health = app.world().get::<PluginHealth>(entity).unwrap();
        assert_eq!(
            health.snapshot(),
            Some(&vec![4, 2]),
            "a declined save must keep the last good snapshot"
        );
    }
}
