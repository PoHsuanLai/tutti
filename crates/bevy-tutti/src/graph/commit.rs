//! Publishing a frame's graph edits to the audio thread.

use bevy_ecs::prelude::*;

use crate::graph::{AudioGraphRes, GraphDirty};

/// Runs a graph commit once iff any reconcile system mutated the graph.
///
/// The commit accepts a changed global output arity, so a master layout change
/// may alter it; `AudioGraphRes::commit`'s docs and `tutti_core`'s
/// `Engine::process_segment` state the RT-buffer contract this relies on.
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
) {
    // Both come from different plugins than each other (`build_into` and
    // `GraphReconcilePlugin`), and `engine_ready` guarantees neither — it reads
    // `AudioEngineState`, which a host can insert alone. Nothing to commit into
    // is a no-op, not a crash.
    let (Some(mut graph), Some(mut dirty)) = (graph, dirty) else {
        return;
    };
    // Every frame, dirty or not: this is where the
    // units the audio thread retired are freed (here, on the main thread, for
    // the reason above) and where settings a full ring held go out.
    let repreparing = graph.is_repreparing();
    graph.collect();
    // A native re-prepare resumed in that collect: its units came back with
    // new shapes (a lookahead is a time, so a rate change moves latencies),
    // and this frame's `Compensate` ran before it, on the old ones. Keep the
    // flag for one more frame, so compensation republishes the figures the
    // resumed plan runs (`ChannelCompensation`, `GraphLatency`).
    let resumed = repreparing && !graph.is_repreparing();
    if resumed {
        dirty.0 = true;
    }
    if !dirty.0 {
        return;
    }
    // A native commit can be refused for now (commits still in flight, or a
    // re-prepare between its halves): the flag stays set and the whole frame's
    // edits go out on a later frame, together.
    if graph.commit() && !resumed {
        dirty.0 = false;
    }
}
