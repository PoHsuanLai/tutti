//! Start renders, and report them when they finish.
//!
//! Two systems and no policy. [`start_exports`] turns each new
//! [`ExportRequest`] into a task on the shared compute pool;
//! [`poll_exports`] drives those tasks and triggers [`ExportDone`].
//!
//! The render itself is synchronous and `Send` (tutti-export spawns no threads
//! by design — "a host that wants a render off the main thread already owns a
//! task pool"). So this module is the *whole* of what Bevy adds: a place to run
//! it, and a way to hear about it.

use std::sync::Arc;

use bevy_ecs::prelude::*;
use bevy_tasks::{block_on, futures_lite::future, AsyncComputeTaskPool};

use tutti_core::transport::{OfflineContext, OfflineTimeline, OfflineTimelineConfig};
use tutti_core::{AudioUnit, SampleRate};
use tutti_export::{render_normalized_to_file, render_to_buffers, render_to_file};

use crate::export::request::{
    ExportDone, ExportInFlight, ExportOutput, ExportRequest, ExportSource, ExportTarget,
    NetPopulator,
};
use crate::graph::{AudioConfig, AudioGraphRes};

/// Turn each new [`ExportRequest`] into a running task.
///
/// The expensive part — cloning and isolating the net — happens here, on the
/// main thread, because the clone borrows the graph resource and cannot cross
/// into a `'static` task. See [`ExportSource::Node`] for what that means for a
/// caller spawning several at once.
/// Exclusive because a registered [`PopulateNet`] is handed `&World`: filling a
/// render's voices is a read of arbitrary app state (which clips exist, their
/// decoded audio), and no fixed `SystemParam` list here could anticipate what a
/// host needs to read. The work is main-thread-bound regardless — the net clone
/// borrows the graph resource and cannot cross into a `'static` task.
pub fn start_exports(world: &mut World) {
    // Collect first: the borrow of `world` for the query must end before the
    // populator gets its own `&World`.
    let pending: Vec<Entity> = world
        .query_filtered::<Entity, With<ExportRequest>>()
        .iter(world)
        // No `Added<>`: the component's *presence* is the pending flag, and
        // removing it below is what marks a request started. One spawned while
        // the engine was down must still be picked up on a later frame.
        .collect();
    if pending.is_empty() {
        return;
    }

    for entity in pending {
        // Take the request out; from here the entity is either in flight or has
        // reported a failure, never still pending.
        let Some(request) = world.entity_mut(entity).take::<ExportRequest>() else {
            continue;
        };

        let prepared = {
            let graph = world.resource::<AudioGraphRes>();
            let config = world.resource::<AudioConfig>();
            prepare_net(graph, config, &request)
        };

        let Some((mut net, ctx)) = prepared else {
            world.trigger(ExportDone {
                entity,
                result: Err(tutti_export::Error::InvalidConfig(
                    "export target node has no outputs".into(),
                )),
            });
            continue;
        };

        // Fill the clone's voices from the app's world, if a filler was
        // registered. An isolated clone is born empty, so without this a
        // sampler-fed tap renders silence.
        if let Some(ctx) = ctx.as_ref() {
            if let Some(populator) = world.remove_resource::<NetPopulator>() {
                populator.0.populate(&mut net, ctx, world);
                world.insert_resource(populator);
            }
        }

        let ExportRequest {
            target,
            config: export_config,
            clock,
            ..
        } = request;

        let task = AsyncComputeTaskPool::get().spawn(async move {
            match target {
                ExportTarget::File {
                    path,
                    normalize: None,
                } => render_to_file(net, &export_config, clock.as_ref(), &path)
                    .map(ExportOutput::File),
                ExportTarget::File {
                    path,
                    normalize: Some(normalize),
                } => render_normalized_to_file(net, &export_config, clock.as_ref(), normalize, &path)
                    .map(ExportOutput::File),
                ExportTarget::Buffers => {
                    render_to_buffers(net, &export_config, clock.as_ref()).map(ExportOutput::Buffers)
                }
            }
        });

        world.entity_mut(entity).insert(ExportInFlight { task });
    }
}

/// Build the net this request renders, plus the offline context its nodes were
/// rebound onto (`None` for a master export, which keeps the live transport
/// bindings the caller's own clock drives).
///
/// Returns `None` when the requested node has no outputs — there is nothing to
/// render from it.
fn prepare_net(
    graph: &AudioGraphRes,
    config: &AudioConfig,
    request: &ExportRequest,
) -> Option<(tutti_core::dsp::Net, Option<OfflineContext>)> {
    match request.source {
        // The whole graph as-is. `Clone` drops the backend, so this net is
        // already safe to render on a worker; nothing is shared that the live
        // graph is still reading.
        ExportSource::Master => Some((graph.0.clone(), None)),

        ExportSource::Node(target) => {
            let pending = graph.0.clone_isolated(target)?;

            // The clock the render drives. Every transport-aware node in the
            // clone is re-seated on it below, and the same `Arc` is what the
            // renderer advances — binding both ends to one timeline is what
            // keeps voices from reading a playhead nothing moves.
            let timeline = Arc::new(OfflineTimeline::new(&OfflineTimelineConfig {
                start_beat: 0.0,
                tempo: 120.0.into(),
                sample_rate: SampleRate(config.sample_rate),
                loop_range: None,
            }));
            let ctx = OfflineContext::new(
                timeline as Arc<dyn tutti_core::Timeline>,
                tutti_core::Beat::new(0.0),
                tutti_core::Bpm(120.0),
            );

            // Isolate (sever live inputs) and rebind (re-point at `ctx`) in the
            // one order they may happen — see `PendingClone::isolate_for_offline`.
            let mut net = pending.isolate_for_offline(&ctx);

            // Reset every node's internal state. The clone inherited the live
            // nodes' filter memory, reverb tails and delay lines as of clone
            // time; rendering from those would make the result depend on *when*
            // the render was started — nondeterministic, and it breaks any
            // cache keyed on "what does this node sound like".
            net.reset();

            Some((net, Some(ctx)))
        }
    }
}

/// Drive in-flight renders; trigger [`ExportDone`] on the ones that finished.
pub fn poll_exports(mut commands: Commands, mut in_flight: Query<(Entity, &mut ExportInFlight)>) {
    for (entity, mut export) in in_flight.iter_mut() {
        let Some(result) = block_on(future::poll_once(&mut export.task)) else {
            continue; // still running
        };
        commands
            .entity(entity)
            .remove::<ExportInFlight>()
            .trigger(move |entity: Entity| ExportDone { entity, result });
    }
}
