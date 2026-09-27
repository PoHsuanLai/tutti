//! **A voice's gain reaches the node that renders, lands on the next block
//! whole, survives a commit, and is what a fork of the node starts from.**
//!
//! `tutti-nodes`' `live_controls_reach_the_node` pins the *rule* — a live
//! control value lives in shared storage, reached through the controls the
//! node was inserted with; a `&mut self` setter can only reach a node that
//! is not running. This pins the sampler's compliance with it, through the
//! door a host actually uses.
//!
//! # What replaced what
//!
//! Under `Net` the door was `Net::set(Setting)` → `VoiceNode::set`, which
//! wrote `slot.voice.play.gain` (a plain `Copy` field) on the copy the backend
//! rendered; under the graph's first cut it was `Legacy::controlled`'s
//! settings ring into the same `set`, with a shadow copy for forks
//! (`voice_gain_through_legacy_settings.rs`). Both went with `AudioUnit` (doc
//! 013 items 8 and 9): a `VoiceNode` is a graph node whose `IntoNode` hands
//! back a `VoiceNodeHandle` — its gain a `Param<Amplitude>` cell, addressable
//! as `UnitParam::Volume` through the handle's `ParamSet` — and the node reads
//! the cell once per block into its `Playback` record and its source. A fork
//! starts from the value last **set** (the authored value), with no shadow.
//!
//! # Why the assertion is on rendered audio
//!
//! Once inserted the node belongs to the executor; the handle's cell is
//! written on the control side, and a test that read the cell back would
//! prove only that the write happened somewhere. What renders is the only
//! vantage point. A voice generates its own signal (no inputs), and the
//! `a_voice_at_unity_renders_its_wave` guard proves the probe can see anything
//! at all, so a regression to silence fails loudly rather than passing every
//! comparison.

use std::sync::Arc;

use tutti_core::{Amplitude, Beat, Bpm, ChannelLayout, Frame, SampleRate, Samples, UnitParam};
use tutti_graph::{ForkMode, ForkTarget, Prepare, Solo, Transport};
use tutti_io::Wave;
use tutti_sampler::{MemorySource, Playback, Voice, VoiceNode, VoiceNodeHandle, VoiceSource};

const RATE: SampleRate = SampleRate(48_000.0);
const BLOCK: usize = 256;

/// A wave whose every frame is 1.0, so a rendered sample *is* the gain.
///
/// Deliberately flat rather than a ramp: the test compares rendered samples
/// against an expected gain directly, and a ramp would make that comparison
/// depend on which frame the read landed on.
fn flat_wave(len: usize) -> Arc<Wave> {
    Arc::new(Wave::from_samples(48_000.0, &vec![1.0f32; len]))
}

/// Rolling at 120 BPM from beat 0, as a host counting frames reports it.
fn rolling(frame: Frame) -> Transport {
    Transport::new(true, Bpm(120.0), Beat(frame.0 as f64 / 24_000.0), None)
}

/// One voice over a flat 1.0 wave, placed from beat 0, alone in a graph on a
/// rolling transport.
///
/// The transport is mandatory, not decoration: a placed voice on a stopped
/// transport renders **silence**, and every gain comparison in this file would
/// compare 0.0 against 0.0. The first version of the `Net` file had no clock
/// and all its tests failed on the guard below, which is what it is for.
fn solo_voice() -> Solo<VoiceNodeHandle> {
    let voice = Voice {
        source: VoiceSource::Memory(MemorySource::placed(flat_wave(48_000), Beat(0.0), None)),
        play: Playback::default(),
        channel_index: None,
    };
    let mut solo = Solo::new(
        VoiceNode::with_channels(voice, ChannelLayout::MONO),
        Prepare::new(RATE, Samples(BLOCK)),
    );
    solo.renderer_mut().set_transport_fn(rolling);
    solo
}

/// Every sample of the next block.
fn block(solo: &mut Solo<VoiceNodeHandle>) -> Vec<f32> {
    solo.render(BLOCK).swap_remove(0)
}

fn all_at(samples: &[f32], gain: f32) -> bool {
    samples.iter().all(|&x| (x - gain).abs() < 1e-4)
}

/// **The probe can see the wave at all.**
///
/// The guard against the whole file passing for the wrong reason: if a
/// fixture change ever leaves the voice gated or silent, a test asserting
/// "the gain is 0.25" would still fail, but one asserting "it did not change"
/// would not. This makes the baseline explicit.
#[test]
fn a_voice_at_unity_renders_its_wave() {
    let mut s = solo_voice();
    let out = block(&mut s);
    assert!(
        all_at(&out, 1.0),
        "the fixture must render its flat 1.0 wave at unity before any gain \
         assertion means anything; got {:?}. Silence here means the voice is \
         gated, and every other test in this file is vacuous.",
        &out[..4]
    );
}

/// **A gain set through the node's controls lands on the next block, whole,
/// and survives a commit.** Addressed as a host addresses any node's param —
/// `UnitParam::Volume` through its `ParamSet` — with no knowledge that it is
/// a sampler voice; and through the typed door, `set_gain`.
///
/// Mutation (run): `VoiceNode::drain_commands` not reading the gain cell
/// (the `if gain != play.gain` block removed) → the block stays at 1.0 →
/// fails. Mutation (run): `VoiceNode::param_set` built over a detached copy
/// of the cell (`Param::new(self.gain.load()).as_atomic()`) → fails.
#[test]
fn a_gain_set_through_the_controls_reaches_the_running_voice() {
    let mut s = solo_voice();
    assert!(all_at(&block(&mut s), 1.0), "unity before the edit");
    assert!(s.controls().params().set(UnitParam::Volume, 0.25));
    let after = block(&mut s);
    assert!(
        all_at(&after, 0.25),
        "every sample of the next block at the new gain; got {:?}",
        &after[..4]
    );
    s.renderer_mut()
        .editor_mut()
        .commit()
        .expect("an unrelated commit");
    assert!(
        all_at(&block(&mut s), 0.25),
        "the value did not survive the commit"
    );
    s.controls().set_gain(Amplitude::new(0.5));
    assert!(all_at(&block(&mut s), 0.5), "the typed door");
    assert_eq!(s.controls().gain(), Amplitude::new(0.5));
}

/// **A fork of the node starts from the gain last set** — what `Playback`'s
/// gain record and `Legacy::controlled`'s shadow were for: an export renders
/// the fader the user set. And the fork shares nothing: a live move after the
/// fork does not reach it.
///
/// Mutation (run): `VoiceNodeFork::fork` leaving the copy's `play.gain` as
/// inserted (the `voice.play.gain = gain` write removed) → the fork renders
/// at unity → fails. Mutation (run): the fork reading the live cell rather
/// than the authored value, with a modulation composite left in it → the
/// fork renders at 0.9 → fails.
#[test]
fn a_fork_starts_from_the_gain_last_set_and_shares_nothing() {
    let mut s = solo_voice();
    s.controls().params().set(UnitParam::Volume, 0.25);
    // A modulation driver's composite, left in the live cell.
    s.controls()
        .params()
        .cell(UnitParam::Volume)
        .expect("the voice has a gain")
        .store(0.9, std::sync::atomic::Ordering::Release);
    let (fork_editor, fork_exec) = s
        .renderer_mut()
        .editor()
        .fork(
            ForkTarget::Master,
            ForkMode::Live,
            Prepare::new(RATE, Samples(BLOCK)),
        )
        .expect("a memory voice forks");
    s.controls().params().set(UnitParam::Volume, 0.75);
    let mut fork = tutti_graph::Renderer::new(fork_editor, fork_exec);
    fork.set_transport_fn(rolling);
    let out = fork.render(BLOCK).swap_remove(0);
    assert!(
        all_at(&out, 0.25),
        "the fork renders the gain last set, and no live move after it; got {:?}",
        &out[..4]
    );
}

/// **A param the voice does not own is refused, not misapplied.**
///
/// The convention that lets a host push a param without dispatching on node
/// type: `ParamSet::set` answers `false` and writes nothing, and the voice
/// renders as before.
///
/// Mutation (run): `VoiceNode::param_set` also addressing `Pan`, at the gain
/// cell → the write lands on the gain → fails.
#[test]
fn an_unowned_param_is_ignored() {
    let mut s = solo_voice();
    assert!(!s.controls().params().set(UnitParam::Pan, 0.0));
    assert!(
        all_at(&block(&mut s), 1.0),
        "a Pan write must leave a voice's gain alone"
    );
}
