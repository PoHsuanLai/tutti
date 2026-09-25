//! Publishing a frame's graph edits to the audio thread.

use bevy_ecs::prelude::*;

use crate::graph::latency::{ChannelCompensation, GraphLatency};
use crate::graph::{AudioGraphRes, GraphDirty};

/// Runs a graph commit once iff any reconcile system mutated the graph, and
/// publishes the PDC figures of the plan it sent.
///
/// The commit accepts a changed global output arity, so a master layout change
/// may alter it; `AudioGraphRes::commit`'s docs and `tutti_core`'s
/// `Engine::process_segment` state the RT-buffer contract this relies on.
///
/// **The figures go out with every plan**, whether or not the host added
/// [`LatencyCompensationPlugin`](crate::LatencyCompensationPlugin): the graph
/// compensates every commit, so a disk source pre-rolling by
/// [`ChannelCompensation`] must see the plan the audio thread runs, not a
/// table nothing fills. Read off the plan the commit sent, so nothing is
/// compiled twice.
///
/// **Pinned to the main thread** via [`NonSendMarker`](bevy_ecs::system::NonSendMarker).
/// The commit deallocates the previous graph version — which includes any
/// in-process plugin nodes whose `Drop` tears down a native editor window
/// (AppKit/Win32/X11). Those teardowns are only legal on the host's main/UI
/// thread, and a worker thread — the default for a parallel system — trips the
/// plugin-host main-thread guard. The marker is zero-cost and forces
/// main-thread scheduling without an exclusive-system signature.
pub fn commit_graph(
    _main: bevy_ecs::system::NonSendMarker,
    graph: Option<ResMut<AudioGraphRes>>,
    dirty: Option<ResMut<GraphDirty>>,
    table: Option<Res<ChannelCompensation>>,
    total: Option<ResMut<GraphLatency>>,
) {
    // Both come from different plugins than each other (`build_into` and
    // `GraphReconcilePlugin`), and `engine_ready` guarantees neither — it reads
    // `AudioEngineState`, which a host can insert alone. Nothing to commit into
    // is a no-op, not a crash.
    let (Some(mut graph), Some(mut dirty)) = (graph, dirty) else {
        return;
    };
    // Every frame, dirty or not: this is where the units the audio thread
    // retired are freed (here, on the main thread, for the reason above) and
    // where settings a full ring held go out.
    let repreparing = graph.is_repreparing();
    graph.collect();
    // A re-prepare resumed in that collect: it sent the resumed plan, whose
    // shapes are new (a lookahead is a time, so a rate change moves
    // latencies). Its figures are the ones to publish now.
    let resumed = repreparing && !graph.is_repreparing();
    // A native commit can be refused for now (commits still in flight, or a
    // re-prepare between its halves): the flag stays set and the whole frame's
    // edits go out on a later frame, together.
    let committed = dirty.0 && graph.commit();
    if committed {
        dirty.0 = false;
    }
    if committed || resumed {
        crate::graph::latency::publish(&graph, table.as_deref(), total.map(|t| t.into_inner()));
    }
}
