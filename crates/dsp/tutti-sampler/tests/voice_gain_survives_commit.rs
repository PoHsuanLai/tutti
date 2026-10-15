//! **A voice's gain reaches the node that renders, lands on the next block
//! whole, survives a commit, and is what a fork of the node starts from.**
//!
//! `tutti-nodes`' `live_controls_reach_the_node` pins the *rule* — a live
//! control value lives in shared storage, reached through the controls the
//! node was inserted with; a `&mut self` setter can only reach a node that
//! is not running. This pins the sampler's compliance with it, through the
//! door a host actually uses.
//!
//! # The door
//!
//! A `VoiceNode` is a graph node whose `IntoNode` hands back a
//! `VoiceNodeHandle` — its gain a `Param<Amplitude>` cell, addressable as
//! `UnitParam::Volume` through the handle's `ParamSet` — and the node reads
//! the cell once per block into its `Playback` record and its source. A fork
//! starts from the value last **set** (the authored value).
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

use std::sync::atomic::Ordering;

use tutti_core::{
    Amplitude, AtomicF32, Beat, BeatDuration, Bpm, ChannelLayout, Depth, Frame, NodeKey,
    PhaseIncrement, SampleRate, Samples, UnitParam,
};
use tutti_graph::{EventOut, ForkMode, ForkTarget, ParamIn, ParamRange, Prepare, Solo, Transport};
use tutti_io::Wave;
use tutti_mod::{Amount, Clock, ControlProgram, CurveType, Layer, LayerId, Polarity};
use tutti_nodes::automation::ControlLaneNode;
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
/// compare 0.0 against 0.0. A setup without a rolling clock fails on the
/// guard below, which is what it is for.
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

/// **A fork of the node starts from the gain last set**: an export renders
/// the fader the user set. And the fork shares nothing: a live move after the
/// fork does not reach it.
///
/// Ported in doc 014 PR 5: this also pinned the fork reading the
/// "authored" value rather than a modulation driver's composite left in the
/// live cell. The driver and `ParamSet::authored` are deleted (§10): a lane
/// sends its composite to the param port and never writes the cell, so the
/// cell is the gain last set, and that half of the test (a store of 0.9
/// into the cell standing in for the driver) went with its subject.
///
/// Mutation (run): `VoiceNodeFork::fork` leaving the copy's `play.gain` as
/// inserted (the `voice.play.gain = gain` write removed) → the fork renders
/// at unity → fails. Mutation (run, PR 5): the forked node's gain `Param`
/// set to a handle of the live node's (`node.gain = live_gain.handle()`)
/// → the live move to 0.75 reaches the fork → fails.
#[test]
fn a_fork_starts_from_the_gain_last_set_and_shares_nothing() {
    let mut s = solo_voice();
    s.controls().params().set(UnitParam::Volume, 0.25);
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

/// **A control lane drives the voice's gain around its own cell** (doc
/// 014 §4.2): the knob at 0.25 plus a constant half of a 0..1 range renders
/// every sample of the flat wave at 0.75, the cell still holds 0.25 (the
/// lane sends to the port and never writes it), and a knob write rides
/// under the lane's offset (0.5 + 0.5 = 1.0). The gain is a port since doc
/// 014 PR 5 ([`VOICE_PARAMS`](tutti_sampler::VOICE_PARAMS)); before, a route
/// onto it was refused with `NoSuchParam` once the frame-rate driver (which
/// wrote the cell) went.
///
/// Mutation (run, PR 5): `VoiceNode::process` passing `None` to
/// `drain_commands` (the port ignored) → the block renders at 0.25 →
/// fails. Mutation (run, PR 5): `VOICE_PARAMS` not declared in the shape →
/// `param_range_cell` is `None` → fails.
#[test]
fn a_lane_drives_the_voice_gain_around_its_cell() {
    let mut s = solo_voice();
    s.controls().params().set(UnitParam::Volume, 0.25);
    let knob = s
        .controls()
        .params()
        .cell(UnitParam::Volume)
        .expect("the gain");
    let at = ParamIn {
        node: s.key(),
        param: UnitParam::Volume,
    };
    drive_volume(s.renderer_mut().editor_mut(), at, knob.clone(), 0.5);

    let out = block(&mut s);
    assert!(
        all_at(&out, 0.75),
        "the knob plus the lane's offset; got {:?}",
        &out[..4]
    );
    assert_eq!(
        knob.load(Ordering::Acquire),
        0.25,
        "the lane never writes the knob"
    );
    s.controls().params().set(UnitParam::Volume, 0.5);
    let out = block(&mut s);
    assert!(
        all_at(&out, 1.0),
        "a knob write rides under the offset; got {:?}",
        &out[..4]
    );
}

/// Wires a control lane onto `at` (a `Volume` port) whose knob is `knob`
/// and whose one layer adds a constant `depth` of a 0..1 range, and
/// commits it.
fn drive_volume(ed: &mut tutti_graph::Editor, at: ParamIn, knob: Arc<AtomicF32>, depth: f32) {
    ed.set_param_range(at, ParamRange::new(0.0, 1.0))
        .expect("the node is placed");
    let range = ed
        .param_range_cell(at)
        .expect("the node declares its gain as a port");
    let layer = Layer {
        id: LayerId(1),
        clock: Clock::Beats {
            per_cycle: BeatDuration(1.0),
            phase: PhaseIncrement(0.0),
        },
        wave: tutti_mod::Wave::Const(1.0),
        amount: Amount {
            depth: Depth(depth),
            polarity: Polarity::Bipolar,
            curve: CurveType::Linear,
        },
        depth_by: None,
    };
    let program = ControlProgram::new(None, vec![layer], vec![]).expect("valid");
    let lane = ControlLaneNode::for_unit(UnitParam::Volume, program, Some(knob), Some(range));
    ed.insert(NodeKey(9_000), "lane", lane);
    ed.spec_mut().connect_param(
        at,
        EventOut {
            node: NodeKey(9_000),
            port: 0,
        },
    );
    ed.commit().expect("the lane commits");
}

/// **A control lane drives a standalone memory source's gain too**: a
/// `MemorySource` inserted as its own node addresses its gain as `Volume`
/// and declares it as a port (doc 014 PR 5), so the knob at 0.25 plus a
/// constant half of a 0..1 range renders the flat wave at 0.75, and the
/// cell still holds 0.25.
///
/// Mutation (run, PR 5): `MemorySource::process` leaving `driven_gain` at
/// `None` (the port ignored) → 0.25 → fails. Mutation (run, PR 5): its
/// shape not declaring `VOICE_PARAMS` → `param_range_cell` is `None` →
/// fails.
#[test]
fn a_lane_drives_a_memory_sources_gain_around_its_cell() {
    let mut s = Solo::new(
        MemorySource::with_channels(flat_wave(48_000), ChannelLayout::MONO),
        Prepare::new(RATE, Samples(BLOCK)),
    );
    s.renderer_mut().set_transport_fn(rolling);
    assert!(s.controls().set(UnitParam::Volume, 0.25));
    let knob = s.controls().cell(UnitParam::Volume).expect("the gain");
    let at = ParamIn {
        node: s.key(),
        param: UnitParam::Volume,
    };
    drive_volume(s.renderer_mut().editor_mut(), at, knob.clone(), 0.5);
    let out = s.render(BLOCK).swap_remove(0);
    assert!(
        all_at(&out, 0.75),
        "the knob plus the lane's offset; got {:?}",
        &out[..4]
    );
    assert_eq!(
        knob.load(Ordering::Acquire),
        0.25,
        "the lane never writes the knob"
    );
}

/// **A voice's gain is a port, its base the cell** (doc 014 PR 5,
/// [`VOICE_PARAMS`](tutti_sampler::VOICE_PARAMS)): the value a driven port
/// starts its connect crossfade from.
///
/// Mutation (run, PR 5): `VoiceNode::param_base` answering unity → fails.
#[test]
fn a_voices_gain_port_reads_its_cell_as_its_base() {
    use tutti_graph::Node;

    let voice = Voice {
        source: VoiceSource::Memory(MemorySource::placed(flat_wave(64), Beat(0.0), None)),
        play: Playback {
            gain: Amplitude::new(0.25),
            ..Playback::default()
        },
        channel_index: None,
    };
    let node = VoiceNode::with_channels(voice, ChannelLayout::MONO);
    assert_eq!(
        node.shape().params.as_slice(),
        &tutti_sampler::VOICE_PARAMS[..]
    );
    assert_eq!(node.param_base(0), Some(0.25));
    assert_eq!(node.param_base(1), None);
}
