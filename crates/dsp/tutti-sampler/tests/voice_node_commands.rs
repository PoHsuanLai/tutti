//! **A standalone voice can be told to move, and the telling survives a commit.**
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
//! 1. **`Clone` must share the receiver.** `AudioUnit: DynClone`, so
//!    `Net::commit` clones every node on each frontend↔backend swap. A clone
//!    that minted a fresh channel would be handed to the audio thread already
//!    deaf, and every later command would vanish with no error.
//!    [`a_command_reaches_a_node_across_a_commit`] fails if that regresses.
//!
//! 2. **A render clone must not drain it.** Crossbeam delivers each message to
//!    exactly one receiver, so an offline worker sharing the live channel
//!    *steals* the user's edits from the audio thread — the live voice then
//!    misses a move with nothing logged anywhere.
//!    [`a_render_clone_steals_no_commands`] fails if `isolate` stops severing.
//!
//! `VoicePool` reaches the same two conclusions and states them at length in its
//! own `Clone`; this is the single-voice half of the same rule.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use tutti_core::dsp::{AudioUnit, Net};
use tutti_core::{Beat, BeatDuration, Bpm, Timeline, Wave};
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

/// **A command reaches a node across a `Net::commit`.**
///
/// The first hazard, and the one a naive `Clone` would break invisibly.
/// `commit` swaps the frontend's clones over the backend, so the unit that
/// renders after it is *not* the one built by `with_commands` — it is a clone.
/// If that clone had minted its own channel, the handle would be talking to a
/// receiver nobody drains.
///
/// Asserted through the **backend**, because that is the copy `commit` puts in
/// charge. Reading the frontend would report the write as landed whatever the
/// backend holds — the mistake `live_value_survives_commit` records making.
#[test]
fn a_command_reaches_a_node_across_a_commit() {
    let transport = FixedTransport::at(10.0);
    let (node, handle) = VoiceNode::with_commands(
        voice_at(transport.clone(), 0.0),
        tutti_core::ChannelLayout::MONO,
    );

    let mut net = Net::new(0, 1);
    let id = net.push(Box::new(node));
    net.pipe_output(id);
    net.check();
    // **Hold** the backend: a dropped one never renders, so `migrate` would not
    // run and the whole point of this test would be lost.
    let mut backend = Box::new(net.backend()) as Box<dyn AudioUnit>;

    // **The vertex has to be marked *changed*, and a bare `commit` does not do
    // it.** `Net::migrate` keeps the *backend's* unit for any vertex reporting
    // `changed <= revision`, so an unchanged commit leaves the original node —
    // and its original receiver — rendering. This test passed with `Clone`
    // sabotaged until that was understood; measured, not assumed.
    //
    // `set_sample_rate` is one of the four operations that do mark a vertex
    // changed (with `reset`, `isolate` and `rebind_offline`), and it is the one
    // a host performs routinely — the engine calls it whenever the device rate
    // is established. So this is the ordinary path, not a contrivance.
    net.set_sample_rate(tutti_core::SampleRate(48_000.0));
    net.commit();
    assert!(peak(backend.as_mut(), 64) < 1e-6, "silent before the move");

    handle
        .set_placement(Beat(10.0), Some(BeatDuration(1.0)))
        .expect("send");

    assert!(
        peak(backend.as_mut(), 64) > 0.5,
        "a command must reach the node through a commit's clone. Silence means \
         `VoiceNode::clone` minted a fresh receiver, leaving the audio thread \
         deaf while the handle reports every send as succeeding."
    );
}

/// **A render clone steals nothing from the live node.**
///
/// The second hazard, and the cost of sharing the receiver. The offline region
/// render clones the live net and ticks it on a worker *while the audio thread
/// plays the original*. Crossbeam delivers each message to exactly one receiver,
/// so a clone that kept draining would consume the user's edits and the live
/// voice would silently miss them.
///
/// `AudioUnit::isolate` is where a clone severs what it must not share, and this
/// asserts the receiver is on that list. The engine's own
/// `an_isolated_pool_steals_no_commands_from_the_live_one` makes the identical
/// claim for `VoicePool`, which solves it a different way (node replacement in a
/// Prepare step) because its clone already holds live voices by then.
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
