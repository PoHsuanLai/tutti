//! Plugin health, latency and state snapshots: the session's
//! `Phase::Plugins`, run each frame, and what it reports placed on the
//! entities (design 015 §4.4).
//!
//! # What the session does
//!
//! [`tutti_session::Session`] keeps a record for each plugin it hosts
//! (`tutti_session::HostedPlugin`) and its `Phase::Plugins` does the work:
//!
//! - **Liveness, debounced.** The engine reports a latched death plus its
//!   cause ([`tutti_plugin::handles::PluginStatus`]); a failed call returns
//!   before the death is published, and `Alive` does not mean healthy, so
//!   one observation is not enough. `tutti-plugin`'s [`Liveness`] declares
//!   a plugin dead after at least [`Liveness::DEATHS`] consecutive unhealthy
//!   observations spanning at least [`Liveness::DEFAULT_WINDOW`] (50 ms), on
//!   the wall clock each frame passes, so it holds at any frame rate. A
//!   dead plugin's node is removed and reported once, with the engine's
//!   cause.
//! - **Latency.** A plugin may change its latency while loaded (a
//!   linear-phase EQ switched into an oversampling mode); the figure its node
//!   declares now (`PluginControls::declared_latency`) is handed to the
//!   graph when it moved, so the next commit re-plans compensation.
//! - **GUI edits** (`modulation`): what the plugin reports reaches the knob
//!   of the lane on that parameter.
//! - **Snapshots.** `save_state` short-circuits on the crash flag, so by the
//!   time a host *knows* a plugin died its state is unreadable: a healthy
//!   plugin's state is captured every
//!   [`SNAPSHOT_INTERVAL`](tutti_session::SNAPSHOT_INTERVAL) (8 s, or the
//!   session's `snapshot_interval`), as a job, one in flight at a time.
//!
//! # What this module adds
//!
//! [`plugin_health_poll`] runs the phase and places its reports:
//! [`PluginHealth::status`] mirrors the session's belief; a death removes
//! `AudioNode` (and the editor) from the entity that held the node, which
//! is the whole ECS teardown; a latency change marks the graph changed; a
//! snapshot job goes on `AsyncComputeTaskPool` (`save_state` is a blocking
//! round trip with a ten-second timeout, which the frame thread must not
//! wait out). [`plugin_state_snapshot`] lands finished jobs
//! (`Session::land_snapshot`) and copies the stored bytes onto the
//! component.

use std::collections::HashMap;
use std::time::Instant;

use bevy_ecs::prelude::*;
use bevy_tasks::{block_on, futures_lite::future, AsyncComputeTaskPool, Task};

use tutti_core::AudioNode;
#[cfg(doc)]
use tutti_plugin::Liveness;
use tutti_session::{Phase, PluginSnapshotTaken, SessionEvent};

use crate::graph::AudioGraphRes;
use crate::plugin_host::editor::PluginEditorOpen;

/// What the session believes about a plugin's liveness: `Healthy`,
/// `Failing` (the debounce's count) or `Dead` (with the engine's cause).
pub use tutti_session::PluginLiveness;

/// Liveness and last-known-good state for one loaded plugin.
///
/// Inserted by the load path alongside
/// [`PluginEmitter`](crate::plugin_host::PluginEmitter).
///
/// Not `Debug`: the in-flight snapshot is a `Task`, which is not.
#[derive(Component, Default)]
pub struct PluginHealth {
    /// What the session believes about the plugin right now. Written only
    /// by [`plugin_health_poll`]; a host reads it, or asks
    /// [`is_dead`](Self::is_dead).
    pub status: PluginLiveness,
    /// The most recent state captured while healthy, if any: a copy of the
    /// session's (`HostedPlugin::snapshot`), which outlives the record when
    /// the plugin dies.
    ///
    /// Private: a host that wrote to it would be claiming a plugin said
    /// something it did not. Read it with [`snapshot`](Self::snapshot) to
    /// restore into a replacement `PluginRequest`.
    last_snapshot: Option<Vec<u8>>,
    /// The snapshot job on the task pool, if one is in flight. The session
    /// asks for one at a time (the job holds its token), so this never
    /// replaces a live task.
    pending_snapshot: Option<Task<PluginSnapshotTaken>>,
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

// # Deferring teardown into a graveyard — deliberately not done
//
// Parking departing handles somewhere to keep the bridge-thread join off the
// frame that removed them does not work, for two reasons.
//
// **A handle clone defers nothing.** The subprocess lifetime lives in an
// `Arc<ProcessGuard>` shared between `PluginHandle` and the `PluginClient` in
// the graph; the subprocess dies when the *last* `Arc` drops. Parking a clone
// adds a reference rather than moving ownership, so the real teardown still
// happens when the graph releases the node — a graveyard only delays the socket
// cleanup while keeping the process alive longer.
//
// **And that teardown must stay where it is.** The node is dropped by the
// session's `Phase::Collect`, on the main thread, because an in-process
// plugin's `Drop` tears down a native editor window, and AppKit/Win32/X11
// teardown off-main is a crash rather than a warning.
//
// The join is also far shorter than its worst case suggests: the bridge thread
// parks with a one-millisecond timeout and the shutdown push unparks it
// immediately, so it waits out only the command actually in flight. The
// ten-second bound needs a `save_state` crossing the wire at that instant.

/// Runs the session's `Phase::Plugins` at the frame's wall clock, and places
/// what it reports (see the module docs).
///
/// Pinned to the main thread ([`NonSendMarker`](bevy_ecs::system::NonSendMarker)):
/// the phase hands latencies to the graph's editor, which collects, and a
/// dead plugin's removal may drop an in-process plugin with an editor.
///
/// Removing `AudioNode` from a dead plugin's entity is the ECS teardown: the
/// session has already removed the node, and the `On<Remove, AudioNode>`
/// observers take the sender off the MIDI bus (their graph removal is a
/// no-op by then). Any entity holding the node is unwired, with or without
/// a [`PluginHealth`] (a plugin a host inserted through
/// [`AudioGraphRes::insert_plugin`] is watched too).
pub fn plugin_health_poll(
    _main: bevy_ecs::system::NonSendMarker,
    mut commands: Commands,
    graph: Option<ResMut<AudioGraphRes>>,
    mut plugins: Query<(Entity, &AudioNode, Option<&mut PluginHealth>)>,
) {
    let Some(mut graph) = graph else {
        return;
    };
    let events = graph
        .bypass_change_detection()
        .run_phase(Phase::Plugins, Instant::now());

    let by_node: HashMap<AudioNode, Entity> = plugins
        .iter()
        .map(|(entity, node, _)| (*node, entity))
        .collect();
    // The session's belief, onto each component; written only when it moved,
    // so `Changed<PluginHealth>` means a change.
    {
        let session = graph.bypass_change_detection().session();
        for (_, node, health) in &mut plugins {
            let (Some(mut health), Some(hosted)) = (health, session.plugin(*node)) else {
                continue;
            };
            if health.status != *hosted.liveness() {
                health.status = hosted.liveness().clone();
            }
        }
    }

    for event in events {
        match event {
            SessionEvent::PluginDied { node, cause } => {
                let Some(&entity) = by_node.get(&node) else {
                    continue;
                };
                if let Ok((_, _, Some(mut health))) = plugins.get_mut(entity) {
                    health.status = PluginLiveness::Dead { cause };
                }
                commands
                    .entity(entity)
                    .remove::<PluginEditorOpen>()
                    .remove::<AudioNode>();
                graph.set_changed();
            }
            SessionEvent::PluginLatencyChanged { .. } => graph.set_changed(),
            SessionEvent::PluginSnapshotDue(job) => {
                // No entity with a `PluginHealth` to hold it: dropped
                // unrun, which frees the plugin for the next interval.
                let Some(&entity) = by_node.get(&job.node()) else {
                    continue;
                };
                if let Ok((_, _, Some(mut health))) = plugins.get_mut(entity) {
                    health.pending_snapshot =
                        Some(AsyncComputeTaskPool::get().spawn(async move { job.run() }));
                }
            }
            _ => {}
        }
    }
}

/// Lands finished snapshot jobs (`Session::land_snapshot`) and copies what
/// the session stored onto the component. A declined save keeps the
/// previous snapshot, in the session and here. Runs before
/// [`plugin_health_poll`], so a job finished during the frame is landed
/// before the phase considers the interval again.
///
/// Pinned to the main thread ([`NonSendMarker`](bevy_ecs::system::NonSendMarker))
/// with the rest of the session's plugin calls: the record it writes is the
/// one `Phase::Plugins` reads on the main thread, and a landed job drops the
/// handle clone it carried here, which for the last handle of a plugin is
/// its teardown.
pub fn plugin_state_snapshot(
    _main: bevy_ecs::system::NonSendMarker,
    graph: Option<ResMut<AudioGraphRes>>,
    mut plugins: Query<(&AudioNode, &mut PluginHealth)>,
) {
    let Some(mut graph) = graph else {
        return;
    };
    for (node, mut health) in &mut plugins {
        let Some(task) = health.pending_snapshot.as_mut() else {
            continue;
        };
        let Some(taken) = block_on(future::poll_once(task)) else {
            continue; // still fetching
        };
        health.pending_snapshot = None;
        let session = graph.bypass_change_detection().session();
        if session.land_snapshot(taken) {
            health.last_snapshot = session
                .plugin(*node)
                .and_then(tutti_session::HostedPlugin::snapshot)
                .map(<[u8]>::to_vec);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin_host::PluginEmitter;
    use bevy_app::prelude::*;
    use std::sync::Arc;
    use std::time::Duration;
    use tutti_plugin::handles::{Normalized, OptionalCapabilities, ParamAddress, PluginHandle};
    use tutti_plugin::server::ParameterInfo;
    use tutti_plugin::Liveness;

    /// A backend whose liveness is whatever the test says it is.
    ///
    /// `PluginClient::new` launches a subprocess, so no unit test can put a
    /// real plugin here — but `PluginHandle::from_backend` takes any
    /// `HostParams + HostState`, and `Graph::host_plugin` hosts it at a node,
    /// which makes the *systems* testable end to end: the cause has to
    /// survive the trip from the backend, through the handle and the
    /// session, into a component.
    struct FakeBackend {
        cause: Option<String>,
        /// What `save_state` answers, and a count of how often it was asked.
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
        fn set_parameter_value(&self, _id: ParamAddress, _value: Normalized) {}
        fn is_crashed(&self) -> bool {
            self.cause.is_some()
        }
        fn crash_cause(&self) -> Option<String> {
            self.cause.clone()
        }
        fn dropped_parameter_events(&self) -> u64 {
            0
        }
    }

    impl tutti_plugin::backend::HostState for FakeBackend {
        fn save_state(&self) -> Result<Vec<u8>, tutti_plugin::error::StateError> {
            self.saves
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            self.state
                .clone()
                .ok_or(tutti_plugin::error::StateError::NoStateRoute)
        }
        fn load_state(&self, _data: &[u8]) -> Result<(), tutti_plugin::error::StateError> {
            Ok(())
        }
    }

    /// A handle whose backend reports `cause` (dead if `Some`), answers
    /// `state` and counts its `save_state` calls.
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
        PluginHandle::from_backend(
            backend,
            OptionalCapabilities::default(),
            Default::default(),
            Default::default(),
            Default::default(),
        )
    }

    /// An app running both systems over `graph`.
    fn app_over(graph: AudioGraphRes) -> App {
        let mut app = App::new();
        app.insert_resource(graph);
        app.add_systems(Update, (plugin_state_snapshot, plugin_health_poll).chain());
        app
    }

    /// Hosts `handle` at a `dc` node (`Graph::host_plugin`) and spawns its
    /// entity, as the load path would. `AudioNode` is a real graph node: its
    /// *removal* is the teardown, so it has to be present for the unwire to
    /// be observable. Nothing reads what the node computes.
    fn host(app: &mut App, handle: PluginHandle) -> Entity {
        let node = {
            let mut graph = app.world_mut().resource_mut::<AudioGraphRes>();
            let (node, _) = graph.insert(tutti_nodes::testing::Const::mono(0.0));
            assert!(graph.session().graph().host_plugin(node, handle.clone()));
            node
        };
        app.world_mut()
            .spawn((PluginEmitter { handle }, PluginHealth::default(), node))
            .id()
    }

    /// Spawn a plugin that reports `cause`, run `frames` updates, return the
    /// entity.
    ///
    /// The frames are half of [`Liveness::DEFAULT_WINDOW`] apart (a sleep: the
    /// system passes the wall clock), so [`Liveness::DEATHS`] of them span the
    /// window, as they do at 60 Hz. The debounce's own rule is pinned in
    /// `tutti-plugin`'s `Liveness` tests and the session's phase in
    /// `tutti-session`'s `tests/plugin.rs`; these pin that the system runs
    /// the phase and places its verdict.
    ///
    /// Mutations (each run, each failed): the system not running the phase
    /// (every crash test: never Failing, never Dead); the sleep removed, so
    /// the frames run back to back inside the window (both death tests:
    /// three observations are not a death until they span it).
    fn run(cause: Option<&str>, frames: usize) -> (App, Entity) {
        let mut app = app_over(AudioGraphRes::headless(0, 2));
        let entity = host(&mut app, handle_with(cause, None, Arc::default()));
        for frame in 0..frames {
            if frame > 0 {
                std::thread::sleep(Liveness::DEFAULT_WINDOW / 2);
            }
            app.update();
        }
        (app, entity)
    }

    /// The engine's latched reason reaches the component verbatim.
    ///
    /// Asserting on the *content* is what distinguishes reading the engine's
    /// cause from inventing one: a fixed placeholder would pass a check for
    /// `Dead` alone.
    ///
    /// Mutation (run, failed): the `PluginDied` arm writing
    /// `Dead { cause: String::new() }`.
    #[test]
    fn a_dead_plugin_reports_the_engines_cause_not_a_placeholder() {
        let (app, entity) = run(
            Some("could not connect to plugin-server: No such file or directory"),
            Liveness::DEATHS as usize,
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

    /// A plugin is not written off on the first bad observation. Pinned at
    /// every frame below the threshold, because an off-by-one here would
    /// unwire a live plugin.
    ///
    /// Mutation (run, failed): the liveness not mirrored onto the component
    /// (it stays `Healthy`).
    #[test]
    fn a_crash_is_not_declared_until_the_debounce_elapses() {
        for frames in 1..Liveness::DEATHS as usize {
            let (app, entity) = run(Some("stream closed"), frames);

            let health = app.world().get::<PluginHealth>(entity).unwrap();
            assert_eq!(
                health.status,
                PluginLiveness::Failing {
                    consecutive: frames as u8
                },
                "at {frames} observation(s) the plugin is failing, not dead"
            );
            assert!(
                app.world().get::<AudioNode>(entity).is_some(),
                "a failing plugin must stay wired — unwiring it early silences a live plugin"
            );
        }
    }

    /// Declaring a plugin dead removes `AudioNode` from its entity, and the
    /// session's node with it.
    ///
    /// Mutation (run, failed): the `PluginDied` arm not removing
    /// `AudioNode` (the entity keeps it).
    #[test]
    fn declaring_death_unwires_the_node() {
        let (mut app, entity) = run(Some("stream closed"), Liveness::DEATHS as usize);
        let node = app
            .world_mut()
            .resource_mut::<AudioGraphRes>()
            .session()
            .plugins()
            .count();

        assert!(
            app.world().get::<AudioNode>(entity).is_none(),
            "a dead plugin must be unwired by removing AudioNode"
        );
        assert_eq!(node, 0, "and the session no longer hosts it");
    }

    /// A plugin the engine reports as alive is never written off, however
    /// long it runs: the negative case, which none of the above would notice
    /// a poll declaring death unconditionally fail.
    ///
    /// Mutation (run, failed): the `PluginDied` arm run for every hosted
    /// node each frame (a `Dead` written regardless).
    #[test]
    fn a_live_plugin_is_never_declared_dead() {
        let (app, entity) = run(None, Liveness::DEATHS as usize + 5);

        let health = app.world().get::<PluginHealth>(entity).unwrap();
        assert_eq!(health.status, PluginLiveness::Healthy);
        assert!(
            app.world().get::<AudioNode>(entity).is_some(),
            "a healthy plugin must stay wired"
        );
    }

    // ---- snapshots ---------------------------------------------------------
    //
    // The cadence (one per interval, one in flight, restarted on each job)
    // and the declined save's rule are the session's and pinned there
    // (`tutti-session`'s `plugin::tests`), with the in-flight guard the old
    // Bevy test could not hold open on a single-threaded pool. These pin the
    // placing: the job onto the pool, the landing, the copy.

    /// An app whose session asks for a snapshot on every phase (interval
    /// zero), with a plugin that answers `state` and counts its saves.
    fn snapshot_app(state: Option<Vec<u8>>) -> (App, Entity, Arc<std::sync::atomic::AtomicUsize>) {
        // The pool is process-global and may already exist from another test in
        // this binary — `get_or_init` rather than `init`, which panics on the
        // second call.
        AsyncComputeTaskPool::get_or_init(bevy_tasks::TaskPool::new);
        let session = tutti_session::Session::builder()
            .outputs_floor(2)
            .snapshot_interval(Duration::ZERO)
            .build_detached(tutti_core::SampleRate::DEFAULT)
            .expect("a detached session builds");
        let mut app = app_over(AudioGraphRes::from_session(session));
        let saves = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let entity = host(&mut app, handle_with(None, state, Arc::clone(&saves)));
        (app, entity, saves)
    }

    /// Run frames until at least one snapshot job has been landed. The
    /// session asks for the next job only once the last one's token is
    /// free, and a landed job frees it, so a second save means the first was
    /// landed (or dropped, which the tests' mutations tell apart). Counted
    /// in saves, not in `pending_snapshot`: with an interval of zero the
    /// frame that lands one job asks for the next. Bounded so a genuine
    /// hang fails the test rather than hanging the suite.
    fn run_until_landed(app: &mut App, saves: &std::sync::atomic::AtomicUsize) {
        for _ in 0..2000 {
            app.update();
            if saves.load(std::sync::atomic::Ordering::SeqCst) >= 2 {
                return;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        panic!("no snapshot job was landed");
    }

    /// A captured snapshot reaches the component: the job crosses to the
    /// pool, its bytes come back a frame or more later, are landed in the
    /// session, and are copied onto the component.
    ///
    /// Mutations (each run, each failed): `plugin_state_snapshot` dropping
    /// the finished job instead of landing it (the bytes never arrive); the
    /// `PluginSnapshotDue` arm dropping the job (nothing is ever saved).
    #[test]
    fn a_snapshot_is_captured_and_stored() {
        let (mut app, entity, saves) = snapshot_app(Some(vec![1, 2, 3]));
        run_until_landed(&mut app, &saves);

        let health = app.world().get::<PluginHealth>(entity).unwrap();
        assert_eq!(
            health.snapshot(),
            Some(&vec![1, 2, 3]),
            "the captured state must reach the component"
        );
    }

    /// The fetch does not block the frame that starts it: the job is on the
    /// pool, its task held by the component, and nothing is stored yet.
    /// `save_state` has a ten-second timeout, so an inline call would stall
    /// the app for that long against a plugin that stopped answering.
    ///
    /// Mutation (run, failed): the `PluginSnapshotDue` arm running the job
    /// inline and landing it (`pending_snapshot` stays `None`, the bytes
    /// already stored).
    #[test]
    fn the_triggering_frame_does_not_wait_for_the_plugin() {
        let (mut app, entity, _) = snapshot_app(Some(vec![7]));
        app.update();

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

    /// A plugin that declines leaves the component's previous snapshot
    /// intact: only what the session stored is copied.
    ///
    /// Mutation (run, failed): the copy made whatever `land_snapshot`
    /// returned (the component reads the session's `None`).
    #[test]
    fn a_declined_save_does_not_erase_the_previous_snapshot() {
        let (mut app, entity, saves) = snapshot_app(None);
        app.world_mut()
            .get_mut::<PluginHealth>(entity)
            .unwrap()
            .last_snapshot = Some(vec![4, 2]);
        run_until_landed(&mut app, &saves);

        let health = app.world().get::<PluginHealth>(entity).unwrap();
        assert_eq!(
            health.snapshot(),
            Some(&vec![4, 2]),
            "a declined save must keep the last good snapshot"
        );
    }
}
