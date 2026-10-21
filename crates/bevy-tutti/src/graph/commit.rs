//! Publishing a frame's graph edits to the audio thread.

use bevy_ecs::prelude::*;
use tutti_session::Phase;

use crate::graph::latency::{ChannelCompensation, GraphLatency};
use crate::graph::{AudioGraphRes, PendingCrossfades};

/// The session's `Collect` phase, first in the frame
/// ([`SessionPhaseSystems`](super::SessionPhaseSystems)`(Phase::Collect)`,
/// before [`GraphReconcileSystems::Spawn`](super::GraphReconcileSystems::Spawn)), as
/// `Session::update` runs it first: frees what the audio thread retired,
/// drops what an export left behind, sends a re-prepare's resumed plan, and
/// swaps in the crossfades that waited out a re-prepare.
///
/// **A crossfade that waited lands here**: the session retries it before its
/// collect, and this binds the incoming node's controls to the entity that
/// asked ([`PendingCrossfades`]).
///
/// First in the frame means one frame's wall time after the previous commit,
/// not just before the next. With a device, the audio thread may not yet
/// have picked up that commit's version, so a node it retires is freed a
/// frame later than when the collect ran beside the commit. That is benign:
/// the free still happens here, on the main thread, and the next collect
/// takes it.
///
/// **Pinned to the main thread** via [`NonSendMarker`](bevy_ecs::system::NonSendMarker).
/// The collect deallocates retired graph versions, which include any
/// in-process plugin nodes whose `Drop` tears down a native editor window
/// (AppKit/Win32/X11). Those teardowns are only legal on the host's main/UI
/// thread, and a worker thread (the default for a parallel system) trips
/// the plugin-host main-thread guard (the session asserts it in both
/// phases). The marker is zero-cost and forces main-thread scheduling
/// without an exclusive-system signature.
pub fn collect_graph(
    _main: bevy_ecs::system::NonSendMarker,
    graph: Option<ResMut<AudioGraphRes>>,
    pending: Option<ResMut<PendingCrossfades>>,
    mut commands: Commands,
) {
    // `build_into`'s graph; `engine_ready` does not guarantee it (it reads
    // `AudioEngineState`, which a host can insert alone).
    let Some(mut graph) = graph else {
        return;
    };
    // Every frame, edited or not: the collect is where the units the audio
    // thread retired are freed (here, on the main thread, for the reason
    // above) and where settings a full ring held go out.
    let events = graph.run_phase(Phase::Collect, std::time::Instant::now());
    if let Some(mut pending) = pending {
        pending.resolve(events, &mut commands);
    }
}

/// The session's `Commit` phase
/// ([`SessionPhaseSystems`](super::SessionPhaseSystems)`(Phase::Commit)`, in
/// [`GraphReconcileSystems::Commit`](super::GraphReconcileSystems::Commit)):
/// commits once iff any edit was made since the last commit, publishing the
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
/// **Pinned to the main thread**, as [`collect_graph`] is: the session
/// asserts it in this phase too.
pub fn commit_graph(
    _main: bevy_ecs::system::NonSendMarker,
    graph: Option<ResMut<AudioGraphRes>>,
    table: Option<ResMut<ChannelCompensation>>,
    total: Option<ResMut<GraphLatency>>,
) {
    let Some(mut graph) = graph else {
        return;
    };
    // A commit refused for now (commits still in flight, or a re-prepare
    // between its halves) leaves the edit flag set, and the frame's edits go
    // out on a later frame, together. The phase reports nothing: what lands
    // in it is the collect's.
    let _ = graph.run_phase(Phase::Commit, std::time::Instant::now());
    crate::graph::latency::mirror(&graph, table, total);
}

/// The session's `Reclaim` phase, last in the frame
/// ([`SessionPhaseSystems`](super::SessionPhaseSystems)`(Phase::Reclaim)`,
/// after [`GraphReconcileSystems::Commit`](super::GraphReconcileSystems::Commit)):
/// frees what the audio thread let go of in every cell the session
/// publishes to, and in the published cells of the controls kept on node
/// entities ([`NodeControls`](super::NodeControls)): a MIDI clip's
/// (feature `midi`) and a voice's placement (feature `sampler`).
///
/// A value a block still held when it was replaced stays retired until a
/// scan after the block ends; a cell that is not published again gets that
/// scan here, every frame. A lock and a length check per cell when nothing
/// is retired. Frees no unit, so it runs on any thread.
pub fn reclaim_graph(
    graph: Option<ResMut<AudioGraphRes>>,
    #[cfg(feature = "midi")] clips: Query<
        &super::NodeControls<tutti_midi_runtime::MidiClipControls>,
    >,
    #[cfg(feature = "sampler")] voices: Query<&crate::sampler::VoiceCommands>,
) {
    if let Some(mut graph) = graph {
        let _ = graph.run_phase(Phase::Reclaim, std::time::Instant::now());
    }
    #[cfg(feature = "midi")]
    for clip in &clips {
        clip.0.reclaim();
    }
    #[cfg(feature = "sampler")]
    for voice in &voices {
        voice.0.reclaim();
    }
}
