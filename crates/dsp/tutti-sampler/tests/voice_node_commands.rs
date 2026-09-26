//! **A standalone voice can be told to move, and the telling reaches the node
//! the graph renders.**
//!
//! Placement is the one control that cannot ride `AudioUnit::set`: `Setting`
//! carries a single `f32`, and a window is a [`Beat`] (`f64`) plus an optional
//! duration. Truncating a beat position to `f32` re-introduces the ~2²⁴ cliff
//! that `Sample.loop_start` was moved to `SamplePosition` (f64) to escape — so
//! it takes the tier this crate reserves for multi-field state: a command queue.
//!
//! `VoicePool` carries such a queue, and `VoiceNode` needs its own for the same
//! reason: without one, moving a sample clip on a timeline cannot reach a
//! playing standalone voice at all.
//!
//! # The two hazards this file exists to pin
//!
//! Both are silent, and both are the reason the channel is more than a field.
//!
//! 1. **The node the graph renders must hold the receiver.** The native graph
//!    renders the unit it was given, across commits and re-prepares (units
//!    move, they are not cloned), so the receiver is the node's own.
//!    [`a_command_reaches_a_node_across_a_commit`] fails if the rendering unit
//!    stops hearing its handle. (Under `Net`, which rendered a clone after a
//!    commit, `Clone` had to share the receiver; doc 013 item 7 removed that.)
//!
//! 2. **A copy must not drain it.** Crossbeam delivers each message to
//!    exactly one receiver, so a fork sharing the live channel would *steal*
//!    the user's edits from the audio thread — the live voice then misses a
//!    move with nothing logged anywhere. A clone gets a dead channel, and
//!    `isolate` severs one held in place.
//!    [`a_render_clone_steals_no_commands`] fails if that regresses.
//!
//! `VoicePool` reaches the same two conclusions and states them in its own
//! `Clone`; this is the single-voice half of the same rule.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use tutti_core::AudioUnit;
use tutti_core::{Beat, BeatDuration, Bpm, Timeline};
use tutti_io::Wave;
use tutti_sampler::{MemorySource, Playback, Voice, VoiceNode, VoiceSource};

/// A transport parked at a beat the test controls.
struct FixedTransport {
    playing: AtomicBool,
    beat: AtomicU64,
}

impl FixedTransport {
    fn at(beat: f64) -> Arc<Self> {
        Arc::new(Self {
            playing: AtomicBool::new(true),
            beat: AtomicU64::new(beat.to_bits()),
        })
    }
    fn seek(&self, beat: f64) {
        self.beat.store(beat.to_bits(), Ordering::Relaxed);
    }
}

impl Timeline for FixedTransport {
    fn is_rolling(&self) -> bool {
        self.playing.load(Ordering::Relaxed)
    }
    fn beat(&self) -> Beat {
        Beat(f64::from_bits(self.beat.load(Ordering::Relaxed)))
    }
    fn tempo(&self) -> Bpm {
        Bpm::new(120.0)
    }
    fn segment_generation(&self) -> u64 {
        0
    }
}

/// A wave whose every frame is 1.0, so "is it inside its window" reads as
/// audible-or-silent with no waveform to reason about.
fn flat_wave(len: usize) -> Arc<Wave> {
    Arc::new(Wave::from_samples(48_000.0, &vec![1.0; len]))
}

fn voice_at(transport: Arc<FixedTransport>, start: f64) -> Voice {
    let mut source = MemorySource::new(flat_wave(48_000));
    source.replace_transport(transport);
    source.set_window(tutti_sampler::VoiceWindow {
        start: Beat(start),
        duration: Some(BeatDuration(1.0)),
    });
    source.play();
    Voice {
        source: VoiceSource::Memory(source),
        play: Playback::default(),
        channel_index: None,
    }
}

/// Peak over a short block — the only vantage point from which a frontend clone
/// and the copy that renders differ.
fn peak(unit: &mut dyn AudioUnit, frames: usize) -> f32 {
    let mut out = [0.0f32; 1];
    let mut peak = 0.0f32;
    for _ in 0..frames {
        unit.tick(&[], &mut out);
        peak = peak.max(out[0].abs());
    }
    peak
}

/// **The guard: the fixture can tell inside-the-window from outside.**
///
/// Every assertion below is "did the window move", which is meaningless unless
/// the probe distinguishes the two states in the first place. If a fixture
/// change ever leaves the voice silent in both, this fails first and says so —
/// rather than the real tests passing because 0.0 never changed.
#[test]
fn the_probe_can_tell_inside_from_outside_a_window() {
    let transport = FixedTransport::at(0.0);
    let mut node = VoiceNode::with_channels(
        voice_at(transport.clone(), 0.0),
        tutti_core::ChannelLayout::MONO,
    );

    assert!(
        peak(&mut node, 64) > 0.5,
        "a voice whose window contains the playhead must sound"
    );

    transport.seek(10.0);
    node.reset();
    assert!(
        peak(&mut node, 64) < 1e-6,
        "and must be silent once the playhead leaves it — without this contrast \
         every placement assertion in this file is vacuous"
    );
}

/// **A queued placement reaches the voice.**
///
/// The claim the channel exists to make, at its simplest: the window starts
/// where the playhead is not, a command moves it, and the voice starts sounding.
#[test]
fn a_queued_placement_moves_a_live_voice() {
    let transport = FixedTransport::at(10.0);
    // Window at beat 0, playhead at 10: silent.
    let (mut node, handle) = VoiceNode::with_commands(
        voice_at(transport.clone(), 0.0),
        tutti_core::ChannelLayout::MONO,
    );
    assert!(
        peak(&mut node, 64) < 1e-6,
        "silent before the move — the playhead is outside the window"
    );

    handle
        .set_placement(Beat(10.0), Some(BeatDuration(1.0)))
        .expect("the queue is empty and the node is alive");

    assert!(
        peak(&mut node, 64) > 0.5,
        "moving the window onto the playhead must make the voice sound — a \
         command that never arrived leaves it silent"
    );
}

/// **A command reaches a node across a native commit and a re-prepare.**
///
/// Re-pinned with the ownership change (doc 013 item 7). Under `Net` this
/// asserted that a *clone* kept hearing the handle: `Net::commit` swapped the
/// frontend's clones over the backend, so the unit rendering after a commit
/// (with a sample-rate change marking the vertex changed) was a clone, and
/// `VoiceNode::clone` had to share the `Receiver`. The native graph renders
/// the unit it was given: a commit that adds a node leaves it in place, and
/// a re-prepare (the rate change, again) checks the unit out, prepares it and
/// sends it back — moved, never cloned. So the receiver is the node's own
/// (a clone gets a dead one), and what this pins is the native path: the
/// node built by `with_commands`, inserted as `bevy-tutti` inserts it
/// (`Legacy::controlled`), hears a placement sent after both.
///
/// Mutation (run): `VoiceNode::process` not draining its commands → the
/// voice never moves → fails. Mutation (run): `Legacy`'s adapter rendering a
/// clone of the unit taken at insert (`Legacy::controlled` handing the node
/// `unit.clone()` instead of `unit`) → the rendering copy's channel is dead
/// → fails.
#[test]
fn a_command_reaches_a_node_across_a_commit() {
    use tutti_core::graph::{OutPort, Source};
    use tutti_core::{NodeKey, SampleRate, Samples};
    use tutti_graph::{Editor, Legacy, Prepare, Transport};

    let transport = FixedTransport::at(10.0);
    let (node, handle) = VoiceNode::with_commands(
        voice_at(transport.clone(), 0.0),
        tutti_core::ChannelLayout::MONO,
    );
    let (mut ed, mut exec) = Editor::new(Prepare::new(SampleRate(48_000.0), Samples(256)));
    let (legacy, _controls) = Legacy::controlled(&mut ed, node);
    ed.insert(NodeKey(1), "voice", legacy);
    ed.spec_mut().topology.outputs = vec![Source::Node(OutPort {
        node: NodeKey(1),
        port: 0,
    })];
    ed.commit().expect("commits");
    exec.apply_pending();
    ed.collect();
    let render = |exec: &mut tutti_graph::Executor| {
        let mut out = vec![0.0f32; 256];
        exec.process(256, &Transport::default(), &[], &mut [&mut out[..]]);
        out.iter().fold(0.0f32, |a, s| a.max(s.abs()))
    };
    assert!(render(&mut exec) < 1e-6, "silent before the move");

    // A graph edit: another node, committed.
    let (other, _) = VoiceNode::with_commands(
        voice_at(transport.clone(), 0.0),
        tutti_core::ChannelLayout::MONO,
    );
    ed.insert(NodeKey(2), "other", Legacy::new(other));
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
    assert!(render(&mut exec) < 1e-6, "still silent: nothing was sent");

    handle
        .set_placement(Beat(10.0), Some(BeatDuration(1.0)))
        .expect("send");

    assert!(
        render(&mut exec) > 0.5,
        "a command must reach the node the graph renders. Silence means the \
         unit rendering is not the one `with_commands` built (a clone, whose \
         channel is dead), or it no longer drains its channel."
    );
}

/// **A render clone steals nothing from the live node.**
///
/// The second hazard. A fork renders on a worker *while the audio thread plays
/// the original*. Crossbeam delivers each message to exactly one receiver, so
/// a copy that drained the live channel would consume the user's edits and the
/// live voice would silently miss them.
///
/// A clone gets a dead channel of its own (`VoiceNode::clone`), and
/// `AudioUnit::isolate` severs one held in place; this asserts the copy a
/// render takes (clone, then isolate) drains nothing. Mutation (run):
/// `VoiceNode::clone` sharing the receiver and `isolate` not severing it →
/// the render drains the move → fails. The engine's own
/// `an_isolated_pool_steals_no_commands_from_the_live_one` makes the identical
/// claim for `VoicePool`.
#[test]
fn a_render_clone_steals_no_commands() {
    let transport = FixedTransport::at(10.0);
    let (mut live, handle) = VoiceNode::with_commands(
        voice_at(transport.clone(), 0.0),
        tutti_core::ChannelLayout::MONO,
    );

    // Clone + isolate, exactly as a render does.
    let mut render = live.clone();
    render.isolate();

    handle
        .set_placement(Beat(10.0), Some(BeatDuration(1.0)))
        .expect("send");

    // The render drains first, and must take nothing.
    let _ = peak(&mut render, 64);

    assert!(
        peak(&mut live, 64) > 0.5,
        "the live node must still receive its command after a render clone has \
         ticked. Silence means the clone drained the queue — the edit reached \
         an offline worker instead of the audio thread, with nothing logged."
    );
}

/// **A node built without a channel still works.**
///
/// The compatibility half: `with_channels` and `new` predate this and have a
/// dozen call sites (resynth, the offline rebind tests, `bevy-tutti`'s
/// `insert_voice`). They get a dead `bounded(0)` receiver rather than an
/// `Option`, so the drain is one `try_recv` that answers `Empty` — no branch on
/// the block path, and no signature churn.
#[test]
fn a_channel_less_node_renders_normally() {
    let transport = FixedTransport::at(0.0);
    let mut node =
        VoiceNode::with_channels(voice_at(transport, 0.0), tutti_core::ChannelLayout::MONO);
    assert!(
        peak(&mut node, 64) > 0.5,
        "a node with no command channel must render exactly as before"
    );
}

/// **A clip moved after the node was inserted reaches a fork of it.**
///
/// A native graph (doc 013) keeps a never-processed snapshot of each node,
/// taken when it is inserted — `Legacy::controlled` clones the unit and
/// `isolate`s the clone — and forks (an export) by cloning that snapshot and
/// `isolate`-ing again. Neither copy drains the command queue (`isolate`
/// severs it, per `a_render_clone_steals_no_commands`), so a placement sent
/// after the insert would never reach them: the fork would export the clip at
/// its old position. The handle records each placement it queues, and
/// `isolate` applies the latest.
///
/// Mutation (run): `VoiceNode::isolate` not applying the recorded placement →
/// the fork keeps the window at beat 0, is silent at beat 10, and this fails.
#[test]
fn a_placement_sent_after_the_snapshot_reaches_a_fork() {
    let transport = FixedTransport::at(10.0);
    let (live, handle) = VoiceNode::with_commands(
        voice_at(transport.clone(), 0.0),
        tutti_core::ChannelLayout::MONO,
    );
    // The snapshot a native graph takes at insert.
    let mut snapshot = live.clone();
    snapshot.isolate();

    handle
        .set_placement(Beat(10.0), Some(BeatDuration(1.0)))
        .expect("send");

    // The fork: a clone of the snapshot, isolated.
    let mut fork = snapshot.clone();
    fork.isolate();
    assert!(
        peak(&mut fork, 64) > 0.5,
        "the fork must play the clip where it was moved to (beat 10)"
    );

    // Not vacuous: the snapshot itself was taken before the move and was
    // never told, so it is still at beat 0.
    assert!(
        peak(&mut snapshot, 64) < 1e-6,
        "the pre-move snapshot is silent at beat 10"
    );
}
