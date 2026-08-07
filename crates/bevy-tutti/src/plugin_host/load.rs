//! Off-thread plugin loading: trigger component, in-flight marker, promotion.
//!
//! Loading a plugin spawns a subprocess, waits out a fixed startup delay,
//! handshakes a socket and sets up shared memory — half a second at best, and
//! up to fifteen before the timeouts give up. That cannot happen on the frame
//! thread, and it cannot happen synchronously inside a reconcile system: eight
//! plugins in a project would freeze the app for two minutes.
//!
//! # Why a component and not a `spawn_plugin` command
//!
//! [`SpawnAudioNode`](crate::graph::SpawnAudioNode) is a command because
//! `Net::add` returns the `NodeId` *inside* a deferred closure and takes a unit
//! that already exists. A load has neither property: it spans frames, and it can
//! arrive before the catalog is scanned or the engine is up. A one-shot queued
//! closure has no frame-to-frame residency, so it cannot retry — **the component
//! is the retry state**. This mirrors [`PlaySoundFont`], for the same reason.
//!
//! There is deliberately no second entry point. When this crate last had two
//! ways to name one thing (`MidiTargetRegistry::insert_target` beside `port`)
//! the two lookups drifted and the redundant one was deleted rather than
//! reconciled.
//!
//! # Shape
//!
//! [`PluginRequest`] (public fields, host-authored) → [`PendingPlugin`] (private
//! fields, machinery) → `AudioNode` + [`PluginEmitter`], with
//! [`PluginLoadDone`] triggered on the entity either way and
//! [`PluginLoadTerminated`] marking the attempt as spent.
//!
//! [`PlaySoundFont`]: crate::soundfont::PlaySoundFont

use bevy_ecs::prelude::*;
use bevy_log::{error, info, warn};
use bevy_tasks::{block_on, futures_lite::future, AsyncComputeTaskPool, Task};

use tutti_core::SampleRate;
use tutti_plugin::catalog::Plugin;
use tutti_plugin::catalog::PluginId;
use tutti_plugin::handles::{PluginClient, PluginHandle};
use tutti_plugin::BridgeError;

use crate::graph::{AudioGraphRes, GraphDirty};
use crate::plugin_host::editor::PluginEmitter;
use crate::plugin_host::health::PluginHealth;
use crate::plugin_host::PluginsRes;

/// Compile-time proof that a loaded plugin can cross a thread boundary, which
/// is what lets the load run on the task pool rather than the frame thread.
///
/// Load-bearing rather than decorative: this module is inside a subsystem whose
/// editor calls are main-thread-pinned and guarded by `assert_main_thread`, so
/// "which thread may hold this?" is a live question here in a way it is not for
/// an ordinary node.
const _: () = {
    fn assert_send<T: Send>() {}
    fn proof() {
        // `Plugin` is what actually crosses to the task pool; the two it
        // wraps are proved alongside it so a regression names the culprit.
        assert_send::<Plugin>();
        assert_send::<PluginClient>();
        assert_send::<PluginHandle>();
        assert_send::<BridgeError>();
    }
    let _ = proof;
};

/// How many loads may be in flight at once.
///
/// `AsyncComputeTaskPool` defaults to about four threads, and
/// [`graph::io`](crate::graph) documents what long-lived work parked there does:
/// soundfont decodes and everything else stop running, with no error anywhere. A
/// load is bounded but slow, so leaving headroom is the difference between a
/// busy project loading and the whole pool wedged behind it.
const MAX_CONCURRENT_LOADS: usize = 2;

/// Ask for a plugin to be loaded onto this entity.
///
/// Spawn an entity carrying this; the load systems do the rest. Configure it the
/// idiomatic Bevy way — `Default` plus struct-update syntax — rather than
/// builder methods.
///
/// ```ignore
/// commands.spawn(PluginRequest {
///     id: PluginId::from_path("/path/to/Foo.vst3"),
///     sample_rate: config.sample_rate,
///     ..Default::default()
/// });
/// ```
///
/// The component is left in place after the load so the request stays inspectable
/// (and so a host can see *what* an entity is). What stops it being retried is
/// [`PluginLoadTerminated`].
#[derive(Component, Debug, Clone)]
pub struct PluginRequest {
    /// Which plugin. Get one from `PluginsRes`'s `find`/`records`, or
    /// `PluginId::from_path` for a known path.
    pub id: PluginId,
    /// Rate to instantiate at. Read this off
    /// [`AudioConfig`](crate::graph::AudioConfig) rather than assuming 44.1k —
    /// the plugin is built for this rate and a mismatch is audible.
    ///
    /// `SampleRate`, which is what `AudioConfig::sample_rate` already is: the
    /// doc above and the example below both said to copy it from there, and
    /// the field then untyped it on arrival.
    pub sample_rate: SampleRate,
    /// Optional preset chunk to restore once loaded, as returned by
    /// `PluginHandle::save_state`.
    pub state: Option<Vec<u8>>,
}

impl Default for PluginRequest {
    fn default() -> Self {
        Self {
            id: PluginId::from_path(std::path::PathBuf::new()),
            // Not 0.0: a zero rate reaches the plugin and is a far more
            // confusing failure than a plausible default that is merely wrong.
            sample_rate: SampleRate::SR_48K,
            state: None,
        }
    }
}

/// An in-flight load.
///
/// Private field, no constructor: [`plugin_load_start`] inserts this and
/// [`plugin_load_promote`] drains it. A host that could build one could hand the
/// promoter a task belonging to no request.
#[derive(Component)]
pub struct PendingPlugin {
    task: Task<Result<Plugin, BridgeError>>,
}

/// This entity's load attempt is finished — successfully or not — and must not
/// be retried.
///
/// The trigger query is steady-state so that a request arriving before the
/// catalog or engine is ready converges once they are, rather than being dropped
/// on the one frame it could not proceed. That retry is the point, but it needs
/// a stop: without this marker a plugin that fails permanently would respawn a
/// subprocess every frame, each attempt costing up to fifteen seconds against a
/// four-thread pool.
///
/// It carries no payload deliberately. The *reason* rides on
/// [`PluginLoadDone`], because a completed load happens once and polling a
/// result component every frame to notice it is a hand-rolled one-shot — the
/// same argument `ExportDone` makes.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PluginLoadTerminated;

/// Triggered on the request entity when its load finishes, successfully or not.
///
/// Observe it at the spawn site, where the surrounding context is still in
/// scope:
///
/// ```ignore
/// commands
///     .spawn(PluginRequest { id, sample_rate, ..Default::default() })
///     .observe(|done: On<PluginLoadDone>| {
///         if let Err(e) = &done.result {
///             warn!("plugin failed to load: {e}");
///         }
///     });
/// ```
#[derive(EntityEvent, Debug)]
pub struct PluginLoadDone {
    pub entity: Entity,
    pub result: Result<(), BridgeError>,
}

/// Query filter for the steady-state load trigger: carries a [`PluginRequest`]
/// but is neither loading ([`PendingPlugin`]), already loaded (it would have an
/// `AudioNode`), nor spent ([`PluginLoadTerminated`]).
type LoadNotStarted = (
    Without<PendingPlugin>,
    Without<PluginLoadTerminated>,
    Without<tutti_core::AudioNode>,
);

/// Start loads for requests that have not been attempted yet.
///
/// Steady-state query rather than `Added<PluginRequest>`: an entity may carry a
/// request before `PluginsRes` exists (it is inserted lazily) or before the
/// engine is up, and `Added` fires exactly once. Anything unresolvable on that
/// one frame would never load at all — the same fire-once trap
/// `register_midi_senders` and the soundfont trigger both document.
pub fn plugin_load_start(
    mut commands: Commands,
    plugins: Option<Res<PluginsRes>>,
    pending: Query<&PendingPlugin>,
    requests: Query<(Entity, &PluginRequest), LoadNotStarted>,
) {
    // Absent while a rescan owns the catalog, and until a host inserts one.
    // Requests simply wait — that is what the steady-state query buys.
    let Some(plugins) = plugins else {
        return;
    };

    let in_flight = pending.iter().count();
    let mut budget = MAX_CONCURRENT_LOADS.saturating_sub(in_flight);

    for (entity, request) in requests.iter() {
        if budget == 0 {
            // Not an error: the rest are picked up on later frames.
            break;
        }
        budget -= 1;

        // Everything the worker needs, owned: `Plugin::open_with` is an
        // associated function, so no borrow of the catalog outlives the frame
        // while a subprocess takes half a second to fifteen to launch.
        let audio = plugins.0.audio_config().clone();
        let path = request.id.path().to_path_buf();
        let sample_rate = request.sample_rate;

        let task = AsyncComputeTaskPool::get().spawn(async move {
            tutti_plugin::catalog::Plugin::open_with(&audio, &path, sample_rate)
        });

        commands.entity(entity).insert(PendingPlugin { task });
    }
}

/// Drain finished loads: add the node, bind the handle, report the outcome.
///
/// Deliberately **not** gated on `engine_ready`. A load already in flight when
/// the engine tears down still has to be reported rather than left hanging —
/// the same reason `poll_exports` is ungated while `start_exports` is not. The
/// graph write is guarded on its own resource instead.
pub fn plugin_load_promote(
    mut commands: Commands,
    graph: Option<ResMut<AudioGraphRes>>,
    dirty: Option<ResMut<GraphDirty>>,
    mut pending: Query<(Entity, &PluginRequest, &mut PendingPlugin)>,
) {
    let (Some(mut graph), Some(mut dirty)) = (graph, dirty) else {
        return;
    };
    let mut edited = false;

    for (entity, request, mut slot) in pending.iter_mut() {
        let Some(result) = block_on(future::poll_once(&mut slot.task)) else {
            continue; // still loading
        };

        let outcome = match result {
            Ok(plugin) => {
                // Split before the node moves into the graph: `into_parts`
                // consumes the `Plugin`, and the handle has to outlive it.
                let (unit, handle) = plugin.into_parts();
                if let Some(blob) = &request.state {
                    // A refusal does **not** abort the load: a plugin sitting at
                    // its defaults is better than no plugin, and the user can
                    // still re-dial it. But it must not be silent — this is a
                    // project load, and before `load_state` returned a result
                    // the DAW showed a restored plugin that had restored
                    // nothing.
                    if let Err(e) = handle.state().load_state(blob) {
                        warn!(
                            "{}: saved state was not restored ({e}); the plugin is \
                             loaded at its defaults",
                            handle.name()
                        );
                    }
                }
                let name = handle.name().to_string();
                // `push`, not `add`: the unit is already boxed, and `add` boxes
                // what it is given.
                let id = graph.0.push(unit);
                edited = true;

                commands
                    .entity(entity)
                    .insert((PluginEmitter { handle }, PluginHealth::default()));
                // `AudioNode` last: its *presence* is what MIDI registration and
                // engine binding key on, and its removal is what unwires them.
                // Inserting it before the emitter would let a binding system see
                // a node whose handle has not landed yet.
                commands.entity(entity).insert(tutti_core::AudioNode(id));
                info!("plugin '{name}' loaded (entity {entity:?})");
                Ok(())
            }
            Err(e) => {
                // No `AudioNode` is inserted, so none of the teardown observers
                // have anything to undo.
                error!(
                    "plugin '{}' failed to load: {e}",
                    request.id.path().display()
                );
                Err(e)
            }
        };

        commands
            .entity(entity)
            .remove::<PendingPlugin>()
            .insert(PluginLoadTerminated)
            .trigger(move |entity: Entity| PluginLoadDone {
                entity,
                result: outcome,
            });
    }

    // Stage only; the Commit-phase `commit_graph` coalesces this with whatever
    // else edited the graph on this frame.
    if edited {
        dirty.0 = true;
    }
}
