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

use std::collections::HashMap;
use std::sync::Arc;

use bevy_ecs::prelude::*;
use bevy_tasks::AsyncComputeTaskPool;

use tutti_core::transport::{OfflineTimeline, OfflineTimelineConfig, OfflineTransport};
use tutti_core::{AudioUnit, SampleRate};
use tutti_export::{render_normalized_to_file, render_to_buffers, render_to_file};

use crate::export::request::{
    ExportDone, ExportInFlight, ExportOutput, ExportRequest, ExportSource, ExportTarget,
    PreparedNet,
};
use crate::graph::{AudioConfig, AudioGraphRes};

/// Start the oldest pending [`ExportRequest`], if nothing is already running.
///
/// The expensive part — cloning and isolating the net — happens here, on the
/// main thread, because the clone borrows the graph resource and cannot cross
/// into a `'static` task. That is also why only one starts per frame; see
/// [`ExportInFlight`].
///
/// Exclusive because a request's `prepare` hook is handed `&World`: preparing a
/// render reads arbitrary app state (which clips exist, their decoded audio),
/// and no fixed `SystemParam` list here could anticipate what a host needs to
/// read. The work is main-thread-bound regardless.
pub fn start_exports(world: &mut World) {
    // One render at a time, enforced here rather than left to callers.
    //
    // The obvious alternative — telling callers to gate their spawn system on
    // `not(any_with_component::<ExportInFlight>)` — cannot work: a run
    // condition sees only the state left by the previous frame, so N requests
    // spawned in one frame all pass the gate and all start together. That is
    // the burst the limit exists to prevent, so the check belongs where the
    // starting happens.
    if world
        .query_filtered::<Entity, With<ExportInFlight>>()
        .iter(world)
        .next()
        .is_some()
    {
        return;
    }

    // Exactly one request per frame — `next()`, not a loop. This is the half
    // that serializes a *batch* spawned in one frame; the check above is the
    // half that stops the next frame starting on top of a still-running render.
    // Each start deep-clones the live net on this thread, so neither half is
    // redundant.
    //
    // No `Added<>` — the component's *presence* is the pending flag, and taking
    // it below is what marks a request started. One spawned while the engine was
    // down must still be picked up on a later frame.
    let Some(entity) = world
        .query_filtered::<Entity, (With<ExportRequest>, Without<ExportInFlight>)>()
        .iter(world)
        .next()
    else {
        return;
    };

    // Take the request out; from here the entity is either in flight or has
    // reported a failure, never still pending.
    let Some(request) = world.entity_mut(entity).take::<ExportRequest>() else {
        return;
    };

    // Snapshot the node bindings before the resource borrows: `prepare_net`
    // needs to resolve an entity to its `NodeId`, and an exclusive system
    // cannot hold a `Query` across the `world.resource` borrows below.
    let nodes: HashMap<Entity, tutti_core::AudioNode> = world
        .query::<(Entity, &tutti_core::AudioNode)>()
        .iter(world)
        .map(|(e, n)| (e, *n))
        .collect();

    // Fallible rather than `world.resource::<_>()`: both come from
    // `engine::build_into`, while the `engine_ready` gate on this system only
    // reads `AudioEngineState` — a value a host can insert alone. An export with
    // no engine to render from is a reportable failure, and this request is
    // already holding the channel to report it on, so it does not silently
    // vanish the way an early `return` would.
    let prepared = match (
        world.get_resource::<AudioGraphRes>(),
        world.get_resource::<AudioConfig>(),
    ) {
        (Some(graph), Some(config)) => {
            prepare_net(graph, config, &nodes, &request).ok_or("export target node has no outputs")
        }
        // Distinguished from the above so the reason is the real one: an export
        // requested against a world with no engine is a different failure than a
        // target that cannot produce audio, and reporting the wrong one sends
        // whoever reads it looking at the wrong thing.
        _ => Err("no audio engine to export from"),
    };

    let (mut net, ctx) = match prepared {
        Ok(prepared) => prepared,
        Err(reason) => {
            world.trigger(ExportDone {
                entity,
                result: Err(tutti_export::Error::InvalidConfig(reason.into())),
            });
            return;
        }
    };

    let ExportRequest {
        target,
        config: export_config,
        clock,
        prepare,
        ..
    } = request;

    // The caller's last look at the net, on the main thread, with the world
    // still readable. An isolated clone is born empty, so a sampler-fed tap
    // that skips this renders silence.
    if let Some(prepare) = prepare.as_ref() {
        prepare(
            PreparedNet {
                net: &mut net,
                ctx: ctx.as_ref(),
            },
            world,
        );
    }

    let task = AsyncComputeTaskPool::get().spawn(async move {
        match target {
            ExportTarget::File {
                path,
                normalize: None,
            } => render_to_file(net, &export_config, clock.as_ref(), &path).map(ExportOutput::File),
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

    world.entity_mut(entity).insert(ExportInFlight::new(task));
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
    nodes: &HashMap<Entity, tutti_core::AudioNode>,
    request: &ExportRequest,
) -> Option<(tutti_core::dsp::Net, Option<OfflineTransport>)> {
    match request.source {
        // The whole graph as-is, keeping its live transport bindings — the
        // caller's own clock is what drives this render.
        //
        // NOTE: this is a plain `Clone`, so it does *not* go through
        // `PendingClone` and nothing is isolated. Nodes that share live state
        // through `Clone` rather than copying it — a disk voice's ring, a mic
        // monitor's input — stay attached to what the audio thread is using.
        // For a master export that is mostly what you want (it is the live mix),
        // but it is not the safety `ExportSource::Node` gets.
        ExportSource::Master => Some((graph.0.clone(), None)),

        ExportSource::Node(entity) => {
            // Resolved here rather than stored: see `ExportSource::Node`.
            let node = nodes.get(&entity)?;
            let pending = graph.0.clone_isolated(node.0)?;

            // The timeline every transport-aware node in the clone is re-seated
            // on. The caller supplies it, because the caller also supplies the
            // `clock` the renderer advances and the two must be the same object
            // — `RenderClock` is advance-only, so there is no reading one back
            // out of the other. Manufacturing one here is what made a tap on a
            // 90 BPM project rebind its voices to a 120 BPM playhead that
            // nothing then advanced.
            let ctx: OfflineTransport = match request.offline.clone() {
                Some(timeline) => timeline,
                // No transport named: a default at the render's own rate. Right
                // for a graph with no musical time, and the reason `on_timeline`
                // is worth calling for anything else.
                None => Arc::new(OfflineTimeline::new(&OfflineTimelineConfig {
                    start_beat: 0.0,
                    tempo: 120.0.into(),
                    sample_rate: config.sample_rate,
                    loop_range: None,
                })),
            };

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
        let Some(result) = export.poll() else {
            continue; // still running
        };
        commands
            .entity(entity)
            .remove::<ExportInFlight>()
            .trigger(move |entity: Entity| ExportDone { entity, result });
    }
}
