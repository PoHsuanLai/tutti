//! Publishing a frame's graph edits to the audio thread.

use bevy_ecs::prelude::*;
use tutti_session::Phase;

use crate::graph::latency::{ChannelCompensation, GraphLatency};
use crate::graph::{AudioGraphRes, PendingCrossfades};

/// The session's `Collect` and `Commit` phases: frees what the audio thread
/// retired, swaps in the crossfades that waited out a re-prepare, and commits
/// once iff any edit was made since the last commit, publishing the
/// compensation of the plan it sent.
///
/// The commit accepts a changed global output arity, so a master layout change
/// may alter it; `tutti_core`'s `Engine::process_segment` states the
/// RT-buffer contract this relies on.
///
/// **The figures go out with every plan**, whether or not the host added
/// [`LatencyCompensationPlugin`](crate::LatencyCompensationPlugin): the graph
/// compensates every commit, so a disk source pre-rolling by
/// [`ChannelCompensation`] must see the plan the audio thread runs, not a
/// table nothing fills. The session publishes them as it sends the plan (and
/// when a re-prepare resumes); [`ChannelCompensation`] is the session's own
/// cell, and [`GraphLatency`] mirrors its total.
///
/// **A crossfade that waited lands here**: the session retries it before its
/// collect, and this binds the incoming node's controls to the entity that
/// asked ([`PendingCrossfades`]).
///
/// **Pinned to the main thread** via [`NonSendMarker`](bevy_ecs::system::NonSendMarker).
/// The commit deallocates the previous graph version — which includes any
/// in-process plugin nodes whose `Drop` tears down a native editor window
/// (AppKit/Win32/X11). Those teardowns are only legal on the host's main/UI
/// thread, and a worker thread — the default for a parallel system — trips the
/// plugin-host main-thread guard (the session asserts it in both phases). The
/// marker is zero-cost and forces main-thread scheduling without an
/// exclusive-system signature.
pub fn commit_graph(
    _main: bevy_ecs::system::NonSendMarker,
    graph: Option<ResMut<AudioGraphRes>>,
    table: Option<ResMut<ChannelCompensation>>,
    total: Option<ResMut<GraphLatency>>,
    pending: Option<ResMut<PendingCrossfades>>,
    mut commands: Commands,
) {
    // `build_into`'s graph; `engine_ready` does not guarantee it (it reads
    // `AudioEngineState`, which a host can insert alone). Nothing to commit
    // into is a no-op, not a crash.
    let Some(mut graph) = graph else {
        return;
    };
    let now = std::time::Instant::now();
    // Every frame, edited or not: the collect is where the units the audio
    // thread retired are freed (here, on the main thread, for the reason
    // above) and where settings a full ring held go out. A commit refused
    // for now (commits still in flight, or a re-prepare between its halves)
    // leaves the edit flag set, and the frame's edits go out on a later
    // frame, together.
    let mut events = graph.run_phase(Phase::Collect, now);
    events.extend(graph.run_phase(Phase::Commit, now));
    crate::graph::latency::mirror(&graph, table, total);
    if let Some(mut pending) = pending {
        pending.resolve(events, &mut commands);
    }
}

/// Commits `graph` now (`AudioGraphRes::commit`, which publishes on a sent
/// plan) and mirrors the figures into `table` and `total`: a commit made
/// outside [`commit_graph`] (modulation's re-apply). Returns whether the plan
/// went out.
#[cfg(feature = "modulation")]
pub(crate) fn commit_and_publish(
    graph: &mut AudioGraphRes,
    table: Option<ResMut<ChannelCompensation>>,
    total: Option<ResMut<GraphLatency>>,
) -> bool {
    let committed = graph.commit();
    if committed {
        crate::graph::latency::mirror(graph, table, total);
    }
    committed
}
