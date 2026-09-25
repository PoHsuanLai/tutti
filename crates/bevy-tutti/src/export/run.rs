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
use tutti_export::{render_normalized_to_file, render_to_buffers, render_to_file, RenderGraph};
use tutti_types::NodeKey;

use crate::export::request::{
    ExportDone, ExportError, ExportInFlight, ExportNode, ExportOutput, ExportRequest, ExportSource,
    ExportTarget, PreparedGraph,
};
use crate::graph::resources::{ExportRefused, Exported};
use crate::graph::{AudioConfig, AudioGraphRes};

/// Start the oldest pending [`ExportRequest`], if nothing is already running.
///
/// The expensive part — copying the graph (a `Net` clone, or a native fork) —
/// happens here, on the main thread, because the copy reads the graph
/// resource and cannot cross into a `'static` task. That is also why only one
/// starts per frame; see [`ExportInFlight`].
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
    // Each start copies the live graph on this thread, so neither half is
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

    // Snapshot the node bindings before the resource borrows: the source
    // entity resolves to its node here, and a failure names its node by
    // entity and `Name`. An exclusive system cannot hold a `Query` across the
    // `world.resource` borrows below.
    let nodes = NodeNames::of(world);

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
        (Some(graph), Some(config)) => prepare_graph(graph, config, &nodes, &request),
        // Distinguished from the above so the reason is the real one: an export
        // requested against a world with no engine is a different failure than a
        // target that cannot produce audio, and reporting the wrong one sends
        // whoever reads it looking at the wrong thing.
        _ => Err(invalid("no audio engine to export from")),
    };

    let (mut graph, ctx) = match prepared {
        Ok(prepared) => prepared,
        Err(e) => {
            world.trigger(ExportDone {
                entity,
                result: Err(nodes.name(e)),
            });
            return;
        }
    };

    let ExportRequest {
        target,
        config: mut export_config,
        clock,
        prepare,
        latency_from_graph,
        tail_from_graph,
        ..
    } = request;

    // The caller's last look at the graph, on the main thread, with the world
    // still readable. An isolated copy is born empty, so a sampler-fed tap
    // that skips this renders silence.
    if let Some(prepare) = prepare.as_ref() {
        prepare(
            PreparedGraph {
                graph: &mut graph,
                ctx: ctx.as_ref(),
            },
            world,
        );
    }
    // A fork's editor holds whatever the hook edited, uncommitted: send it,
    // and apply it here so the render's first block already runs it.
    if let RenderGraph::Graph { editor, executor } = &mut graph {
        if let Err(e) = editor.commit() {
            world.trigger(ExportDone {
                entity,
                result: Err(ExportError::Render(invalid(format!(
                    "the prepare hook left the forked graph uncommittable: {e}"
                )))),
            });
            return;
        }
        executor.apply_pending();
        editor.collect();
    }

    // The graph's own figures, asked of the graph that is rendered — after
    // the hook, which may have changed it.
    if latency_from_graph {
        export_config.render.latency = graph.reported_latency();
    }
    if let Some(unbounded) = tail_from_graph {
        export_config.render.tail = graph.reported_tail().samples().unwrap_or(unbounded);
    }

    let task = AsyncComputeTaskPool::get().spawn(async move {
        let rendered = match target {
            ExportTarget::File {
                path,
                normalize: None,
            } => {
                render_to_file(graph, &export_config, clock.as_ref(), &path).map(ExportOutput::File)
            }
            ExportTarget::File {
                path,
                normalize: Some(normalize),
            } => render_normalized_to_file(graph, &export_config, clock.as_ref(), normalize, &path)
                .map(ExportOutput::File),
            ExportTarget::Buffers => {
                render_to_buffers(graph, &export_config, clock.as_ref()).map(ExportOutput::Buffers)
            }
        };
        rendered.map_err(|e| nodes.name(e))
    });

    world.entity_mut(entity).insert(ExportInFlight::new(task));
}

/// Build the graph this request renders, plus the offline context its nodes
/// were rebound onto (`None` for a `Net` master export, which keeps the live
/// transport bindings the caller's own clock drives).
///
/// Refuses, with the reason, when the requested node has no outputs — there is
/// nothing to render from it — and, on the native backend, when a node the
/// copy needs cannot be forked (named by the caller, [`NodeNames::name`]).
fn prepare_graph(
    graph: &AudioGraphRes,
    config: &AudioConfig,
    nodes: &NodeNames,
    request: &ExportRequest,
) -> Result<(RenderGraph, Option<OfflineTransport>), tutti_export::Error> {
    let node = match request.source {
        ExportSource::Master => None,
        // Resolved here rather than stored: see `ExportSource::Node`.
        ExportSource::Node(entity) => Some(nodes.node(entity).ok_or_else(|| invalid(NO_OUTPUTS))?),
    };

    // The timeline every transport-aware node in the copy is re-seated on. The
    // caller supplies it, because the caller also supplies the `clock` the
    // renderer advances and the two must be the same object — `RenderClock` is
    // advance-only, so there is no reading one back out of the other.
    // Manufacturing one here is what made a tap on a 90 BPM project rebind its
    // voices to a 120 BPM playhead that nothing then advanced.
    let ctx: OfflineTransport = match request.offline.clone() {
        Some(timeline) => timeline,
        // No transport named: a default at the device's rate. Right for a
        // graph with no musical time, and the reason `on_timeline` is worth
        // calling for anything else.
        None => Arc::new(OfflineTimeline::new(&OfflineTimelineConfig {
            start_beat: 0.0.into(),
            tempo: 120.0.into(),
            sample_rate: config.sample_rate,
            loop_range: None,
        })),
    };

    // A `Net` node export and every native fork are isolated, rebound onto
    // `ctx` and reset; a `Net` master export is a plain clone — see
    // `AudioGraphRes::export`.
    match graph.export(node, &ctx, request.config.render.sample_rate) {
        Ok(Exported {
            graph,
            rebound: true,
        }) => Ok((graph, Some(ctx))),
        Ok(Exported {
            graph,
            rebound: false,
        }) => Ok((graph, None)),
        Err(ExportRefused::NoOutputs) => Err(invalid(NO_OUTPUTS)),
        Err(ExportRefused::Render(e)) => Err(e),
    }
}

/// Why a node export found nothing to render: the entity is not a node, or
/// its node has no outputs.
const NO_OUTPUTS: &str = "export target node has no outputs";

fn invalid(reason: impl Into<String>) -> tutti_export::Error {
    tutti_export::Error::InvalidConfig(reason.into())
}

/// Every entity bound to a graph node, by entity and by node key — what
/// resolves an [`ExportSource::Node`] to its node, and what names a node an
/// export failed on. Taken on the main thread when the render starts, and
/// moved into the render task (a fork can fail mid-render, long after the
/// world was last readable).
struct NodeNames {
    nodes: HashMap<Entity, tutti_core::AudioNode>,
    by_key: HashMap<NodeKey, (Entity, Option<String>)>,
}

impl NodeNames {
    fn of(world: &mut World) -> Self {
        let mut nodes = HashMap::new();
        let mut by_key = HashMap::new();
        for (entity, node, name) in world
            .query::<(Entity, &tutti_core::AudioNode, Option<&Name>)>()
            .iter(world)
        {
            nodes.insert(entity, *node);
            by_key.insert(
                crate::graph::native::key(*node),
                (entity, name.map(|n| n.as_str().to_owned())),
            );
        }
        Self { nodes, by_key }
    }

    fn node(&self, entity: Entity) -> Option<tutti_core::AudioNode> {
        self.nodes.get(&entity).copied()
    }

    fn export_node(&self, key: NodeKey) -> ExportNode {
        let (entity, name) = self
            .by_key
            .get(&key)
            .map_or((None, None), |(e, n)| (Some(*e), n.clone()));
        ExportNode { entity, name, key }
    }

    /// `e`, with the node it is about named by entity and `Name`.
    fn name(&self, e: tutti_export::Error) -> ExportError {
        match e {
            tutti_export::Error::NotForkable { key } => ExportError::NotForkable {
                node: self.export_node(key),
            },
            tutti_export::Error::Fork(tutti_graph::ForkError::Source { key, cause }) => {
                ExportError::ForkSource {
                    node: self.export_node(key),
                    cause,
                }
            }
            tutti_export::Error::ForkFailed { key, kind, cause } => ExportError::ForkFailed {
                node: self.export_node(key),
                kind,
                cause,
            },
            other => ExportError::Render(other),
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
