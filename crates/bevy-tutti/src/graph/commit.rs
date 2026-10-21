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
/// table, and [`GraphLatency`] mirrors its total.
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
/// publishes to, its nodes' included (each node's
/// [`SessionNode::reclaimer`](tutti_session::SessionNode::reclaimer), kept by
/// the session graph as the node went in, and each hosted plugin's meter).
/// Which cells that is, is decided in the session, so a headless host sweeps
/// the same ones.
///
/// A value a block still held when it was replaced stays retired until a
/// scan after the block ends; a cell that is not published again gets that
/// scan here, every frame. A lock and a length check per cell when nothing
/// is retired. Frees no unit, so it runs on any thread.
pub fn reclaim_graph(graph: Option<ResMut<AudioGraphRes>>) {
    if let Some(mut graph) = graph {
        let _ = graph.run_phase(Phase::Reclaim, std::time::Instant::now());
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use bevy_app::App;
    use tutti_core::{Beat, MeterChange, MeterMap, MeterSlot, TimeSignature};

    use crate::graph::AudioGraphRes;

    /// **A frame's `Reclaim` phase frees what a block held across a
    /// replacement**, on a later frame with no publish: the adapter
    /// schedules the phase every frame. The cell is the session's project
    /// meter, read through a following `MeterSlot` (as a plugin node reads
    /// it); a held `Loan` stands in for the block.
    ///
    /// Mutation (run): `reclaim_graph` not running the session's phase →
    /// the old meter is still alive after the frame, fails.
    ///
    /// Which cells the phase sweeps is the session's, pinned in
    /// `tutti-session`'s tests; this pins only that the adapter runs it.
    #[test]
    fn a_frame_frees_a_meter_a_block_held_across_its_replacement() {
        let mut app = App::new();
        app.insert_resource(AudioGraphRes::headless(0, 2));
        app.insert_resource(crate::AudioEngineState::Running);
        app.add_plugins(crate::graph::GraphReconcilePlugin);
        app.update();

        let meter = app
            .world()
            .resource::<AudioGraphRes>()
            .read()
            .click_settings()
            .project_meter();
        let slot = MeterSlot::following_meter(Some(meter.clone()));
        let old = Arc::new(MeterMap::new([MeterChange::new(
            Beat(0.0),
            TimeSignature::from_parts(3, 4),
        )]));
        let gone = Arc::downgrade(&old);
        meter.set_meter(old);

        let mut reader = slot.reader();
        {
            let _held = reader.lend();
            meter.set_meter(Arc::new(MeterMap::default()));
            // Still held: this frame's reclaim leaves it.
            app.update();
        }
        assert!(gone.upgrade().is_some(), "retired, held across the set");
        app.update();
        assert!(gone.upgrade().is_none(), "freed by the frame's reclaim");
    }
}
