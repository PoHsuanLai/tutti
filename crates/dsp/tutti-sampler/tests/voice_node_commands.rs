//! **A standalone voice can be told to move, and the telling reaches the node
//! the graph renders.**
//!
//! Placement is not a scalar param: a window is a [`Beat`] (`f64`) plus an
//! optional duration, and truncating a beat position to `f32` re-introduces
//! the ~2²⁴ cliff that `Sample.loop_start` was moved to `SamplePosition` (f64)
//! to escape — so it takes the tier the engine reserves for multi-field
//! state: an `ArcLend` cell the handle publishes to and the node reads once
//! per block, latest wins. The cell sits in the `VoiceNodeHandle` its
//! `IntoNode` hands back.
//!
//! # The hazards this file exists to pin
//!
//! All are silent.
//!
//! 1. **The node the graph renders must read the handle's cell.** The graph
//!    renders the unit it was given, across commits and re-prepares (units
//!    move, they are not cloned), so the cell is the node's own.
//!    [`a_command_reaches_a_node_across_a_commit`] fails if the rendering unit
//!    stops hearing its handle.
//!
//! 2. **A fork must not take the live node's edits.** With the old command
//!    queue a fork sharing the receiver would have *stolen* moves from the
//!    audio thread. A read of a published cell consumes nothing, so this is
//!    now structural; [`a_fork_steals_no_commands`] stays as the end-to-end
//!    check.
//!
//! 3. **A fork must still play where the clip was last moved to**: the
//!    node's fork source reads the same cell when the fork is taken.
//!    [`a_placement_sent_after_the_insert_reaches_a_fork`] pins it.
//!
//! 4. **Only the latest window is heard.** Two moves between blocks play the
//!    second; [`the_latest_placement_wins`] pins it.

use std::sync::Arc;

use tutti_core::graph::{OutPort, Source};
use tutti_core::{Beat, BeatDuration, Bpm, ChannelLayout, NodeKey, SampleRate, Samples};
use tutti_graph::{Editor, Executor, ForkMode, ForkTarget, Prepare, Transport, Unforkable};
use tutti_io::Wave;
use tutti_sampler::testing::{block, MockTransport};
use tutti_sampler::{MemorySource, Playback, Voice, VoiceNode, VoiceSource, VoiceWindow};

const RATE: SampleRate = SampleRate(48_000.0);

/// A wave whose every frame is 1.0, so "is it inside its window" reads as
/// audible-or-silent with no waveform to reason about.
fn flat_wave(len: usize) -> Arc<Wave> {
    Arc::new(Wave::from_samples(48_000.0, &vec![1.0; len]))
}

/// A voice placed on the transport at `[start, start + 1)`.
fn voice_at(start: f64) -> Voice {
    let source = MemorySource::new(flat_wave(48_000))
        .placed_at(VoiceWindow::span(Beat(start), BeatDuration(1.0)));
    Voice {
        source: VoiceSource::Memory(source),
        play: Playback::default(),
        channel_index: None,
    }
}

fn node_at(start: f64) -> VoiceNode {
    VoiceNode::with_channels(voice_at(start), ChannelLayout::MONO)
}

/// Peak over a 64-frame block of `node` under a transport rolling at `beat`
/// (not moved) — what renders is the only vantage point.
fn peak(node: &mut dyn tutti_graph::Node, beat: f64) -> f32 {
    let t = MockTransport::rolling(Beat(beat), Bpm(120.0));
    block(node, &t, RATE, 64)[0]
        .iter()
        .fold(0.0f32, |a, s| a.max(s.abs()))
}

/// A graph holding `node` at key 1 on output 0, committed; its controls.
fn graph_with(node: VoiceNode) -> (Editor, Executor, tutti_sampler::VoiceNodeHandle) {
    let (mut ed, mut exec) = Editor::new(Prepare::new(RATE, Samples(256)));
    let handle = ed.insert(NodeKey(1), "voice", node);
    ed.spec_mut().topology.outputs = vec![Source::Node(OutPort {
        node: NodeKey(1),
        port: 0,
    })];
    ed.commit().expect("commits");
    exec.apply_pending();
    ed.collect();
    (ed, exec, handle)
}

/// Peak of one 256-frame block of `exec`'s graph under a transport rolling
/// at `beat`.
fn render_peak(exec: &mut Executor, beat: f64) -> f32 {
    let mut out = vec![0.0f32; 256];
    let t = Transport::new(true, Bpm(120.0), Beat(beat), None);
    exec.process(256, &t, &[], &mut [&mut out[..]]);
    out.iter().fold(0.0f32, |a, s| a.max(s.abs()))
}

/// **The guard: the fixture can tell inside-the-window from outside.**
///
/// Every assertion below is "did the window move", which is meaningless unless
/// the probe distinguishes the two states in the first place. If a fixture
/// change ever leaves the voice silent in both, this fails first and says so —
/// rather than the real tests passing because 0.0 never changed.
#[test]
fn the_probe_can_tell_inside_from_outside_a_window() {
    let mut node = node_at(0.0);
    assert!(
        peak(&mut node, 0.0) > 0.5,
        "a voice whose window contains the playhead must sound"
    );
    assert!(
        peak(&mut node, 10.0) < 1e-6,
        "and must be silent once the playhead leaves it — without this contrast \
         every placement assertion in this file is vacuous"
    );
}

/// **A published placement reaches the voice**, on its next block.
///
/// The claim the cell exists to make, at its simplest: the window starts
/// where the playhead is not, a placement moves it, and the voice starts
/// sounding.
///
/// Mutation (run): `VoiceNode::drain_commands` not applying the published
/// window (the `apply_placement` call removed) → silent after the move →
/// fails.
#[test]
fn a_queued_placement_moves_a_live_voice() {
    // Window at beat 0, playhead at 10: silent.
    let (mut node, handle) = node_at(0.0).with_handle();
    assert!(
        peak(&mut node, 10.0) < 1e-6,
        "silent before the move — the playhead is outside the window"
    );

    handle.set_placement(Beat(10.0), Some(BeatDuration(1.0)));

    assert!(
        peak(&mut node, 10.0) > 0.5,
        "moving the window onto the playhead must make the voice sound — a \
         command that never arrived leaves it silent"
    );
}

/// **A command reaches a node across a commit and a re-prepare.**
///
/// The graph renders the unit it was given: a commit that adds a node
/// leaves it in place, and a re-prepare (a rate change) checks the unit out,
/// prepares it and sends it back — moved, never cloned. So the cell is the
/// node's own, and what this pins is that the node inserted through its
/// `IntoNode` (as `bevy-tutti` inserts it) hears a placement set after both,
/// through the handle the insert handed back.
///
/// Mutation (run): `VoiceNode::process` not calling `drain_commands` → the
/// voice never moves → fails. Mutation (run): `with_handle` handing the
/// handle a fresh cell rather than the node's (`Arc::new(ArcLend::new(None))`
/// in place of `Arc::clone(&self.placement)`) → fails. (Rendering a clone
/// taken at insert is not a possible mutation: nothing clones a graph node.)
#[test]
fn a_command_reaches_a_node_across_a_commit() {
    let (mut ed, mut exec, handle) = graph_with(node_at(0.0));
    assert!(
        render_peak(&mut exec, 10.0) < 1e-6,
        "silent before the move"
    );

    // A graph edit: another node, committed.
    ed.insert(NodeKey(2), "other", Unforkable(node_at(0.0)));
    ed.commit().expect("commits");
    exec.apply_pending();
    ed.collect();
    // A re-prepare at another rate: the units go out and come back.
    ed.reprepare(Prepare::new(SampleRate(44_100.0), Samples(256)))
        .expect("reprepares");
    exec.apply_pending();
    ed.collect();
    exec.apply_pending();
    ed.collect();
    assert!(
        render_peak(&mut exec, 10.0) < 1e-6,
        "still silent: nothing was sent"
    );

    handle.set_placement(Beat(10.0), Some(BeatDuration(1.0)));

    assert!(
        render_peak(&mut exec, 10.0) > 0.5,
        "a placement must reach the node the graph renders. Silence means the \
         unit rendering does not read the cell the handle publishes to, or it \
         no longer reads it."
    );
}

/// **A fork steals nothing from the live node.**
///
/// A fork renders on a worker *while the audio thread plays the original*.
/// When the placement rode a crossbeam queue, a fork draining the live
/// receiver would have consumed the user's edits and the live voice would
/// silently miss them. A placement is now a published value, and reading it
/// consumes nothing, so no mutation of the fork source can make a fork take
/// the live node's move: this property is structural and the test cannot
/// fail for that reason any more. It stays as the end-to-end check that a
/// fork rendering first leaves the live node moved; the fork reading the
/// shared cell is what `a_placement_sent_after_the_insert_reaches_a_fork`
/// pins (its mutation: the fork applying nothing). The pool's
/// `a_pools_fork_shares_nothing_with_the_live_pool` makes the queue claim
/// for `VoicePool`, which still has one.
#[test]
fn a_fork_steals_no_commands() {
    let (ed, mut exec, handle) = graph_with(node_at(0.0));
    let (_fork_ed, mut fork) = ed
        .fork(
            ForkTarget::Master,
            ForkMode::Live,
            Prepare::new(RATE, Samples(256)),
        )
        .expect("a memory voice forks");
    fork.apply_pending();

    handle.set_placement(Beat(10.0), Some(BeatDuration(1.0)));

    // The fork renders first, and must take nothing.
    let _ = render_peak(&mut fork, 10.0);

    assert!(
        render_peak(&mut exec, 10.0) > 0.5,
        "the live node must still receive its command after a fork has \
         rendered. Silence means the fork drained the queue — the edit reached \
         an offline worker instead of the audio thread, with nothing logged."
    );
}

/// **A node no handle was taken for still works.**
///
/// A node driven by hand (`with_channels`, never inserted, `with_handle`
/// never called) has an empty placement cell, and must keep the window it
/// was built with rather than lose it to the empty cell.
///
/// Mutation (run): `drain_commands` applying the cell every block with an
/// empty cell read as a window far off (`if true { let window =
/// published.unwrap_or(VoiceWindow::from(Beat(1e9))); … }`) → silent →
/// fails.
#[test]
fn a_node_with_no_handle_renders_normally() {
    let mut node = node_at(0.0);
    assert!(
        peak(&mut node, 0.0) > 0.5,
        "a node with no command channel must render exactly as before"
    );
}

/// **A clip moved after the node was inserted reaches a fork of it.**
///
/// The node's fork source keeps a copy of the voice as it was inserted, so a
/// placement set after the insert reaches a fork only because the fork source
/// applies the window the handle last published (its control-side record,
/// never the cell the audio thread reads) when the fork is taken; otherwise
/// it would export the clip at its old position.
///
/// Mutations (run): `VoiceNodeFork::fork` not applying the recorded
/// placement → the fork keeps the window at beat 0, is silent at beat 10,
/// and this fails; `set_placement` publishing without updating the record →
/// the same.
#[test]
fn a_placement_sent_after_the_insert_reaches_a_fork() {
    let (ed, _exec, handle) = graph_with(node_at(0.0));
    let prepare = Prepare::new(RATE, Samples(256));
    // A fork taken before the move: not vacuous, it is still at beat 0.
    let (_before_ed, mut before) = ed
        .fork(ForkTarget::Master, ForkMode::Live, prepare)
        .expect("forks");
    before.apply_pending();

    handle.set_placement(Beat(10.0), Some(BeatDuration(1.0)));

    let (_after_ed, mut after) = ed
        .fork(ForkTarget::Master, ForkMode::Live, prepare)
        .expect("forks");
    after.apply_pending();
    assert!(
        render_peak(&mut after, 10.0) > 0.5,
        "the fork must play the clip where it was moved to (beat 10)"
    );
    assert!(
        render_peak(&mut before, 10.0) < 1e-6,
        "a fork taken before the move is silent at beat 10"
    );
}

/// **Only the latest placement is heard.** Two moves set between two blocks
/// play the second: the node reads one published window per block, it does
/// not replay a history.
///
/// Not vacuous: the first window alone sounds at beat 10 (asserted below on
/// a second node), so a node that applied the first move and dropped the
/// second would sound there.
///
/// Mutation (run): `set_placement` publishing only when the cell is empty
/// (`if self.placement.lend().is_none()`) → the first window sticks →
/// fails.
#[test]
fn the_latest_placement_wins() {
    let (mut first_only, first_handle) = node_at(0.0).with_handle();
    first_handle.set_placement(Beat(10.0), Some(BeatDuration(1.0)));
    assert!(
        peak(&mut first_only, 10.0) > 0.5,
        "the first window alone sounds at beat 10"
    );

    let (mut node, handle) = node_at(0.0).with_handle();
    handle.set_placement(Beat(10.0), Some(BeatDuration(1.0)));
    handle.set_placement(Beat(20.0), Some(BeatDuration(1.0)));
    assert!(
        peak(&mut node, 10.0) < 1e-6,
        "the first move was replaced before the block: beat 10 is outside the \
         window the node plays"
    );
    assert!(
        peak(&mut node, 20.0) > 0.5,
        "the second move is the window the node plays"
    );
}
