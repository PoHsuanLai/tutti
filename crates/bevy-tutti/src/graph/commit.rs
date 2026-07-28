//! Publishing a frame's graph edits to the audio thread.

use bevy_ecs::prelude::*;

use crate::graph::{AudioGraphRes, GraphDirty};

/// Runs a graph commit once iff any reconcile system mutated the graph.
///
/// Commits via [`Net::commit_output_arity_change`](fundsp::net::Net::commit_output_arity_change)
/// so a master layout change may alter the global output arity; that method and
/// [`crate::Engine::process_segment`] document the RT-buffer contract this
/// relies on. Identical to plain `commit()` when the arity is unchanged.
///
/// **Pinned to the main thread** via [`NonSendMarker`](bevy_ecs::system::NonSendMarker).
/// The commit deallocates the previous graph version — which includes any
/// in-process plugin nodes whose `Drop` tears down a native editor window
/// (AppKit/Win32/X11). Those teardowns are only legal on the host's main/UI
/// thread; running this on a worker thread (the default for a parallel system)
/// panicked the plugin-host main-thread guard. The marker is zero-cost and
/// forces main-thread scheduling without an exclusive-system signature.
pub fn commit_graph(
    _main: bevy_ecs::system::NonSendMarker,
    mut graph: ResMut<AudioGraphRes>,
    mut dirty: ResMut<GraphDirty>,
) {
    if !dirty.0 {
        return;
    }
    graph.0.commit_output_arity_change();
    dirty.0 = false;
}
