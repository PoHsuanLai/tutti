//! An offline render must not hear the live playhead — nor steal from it.
//!
//! Under `Net` this was a per-node duty (`AudioUnit::rebind_offline`: each
//! transport-aware unit re-pointed its clock at the render's timeline), and
//! these guards pinned that every such node — `VoicePool`, `VoiceNode`, a bare
//! `MemorySource`, one nested in a sub-net — was reached. The sampler's nodes
//! are native now (doc 013 items 8 and 9): each reads the transport from its
//! block's `Env`, so a fork (`Editor::fork`, the export's) has nothing to
//! rebind — it plays on whatever transport its own renderer hands it. What is
//! left to pin is the outcome, for every node shape: a fork rendered on a
//! **stopped** render transport is silent while the live graph, on a rolling
//! one, sounds; a fork holds nothing the live graph's handles reach; and a
//! node that reads no transport is unaffected.
//!
//! Stated behaviourally, on rendered audio: "a clock lives in this field and
//! was swapped" would pass vacuously the day the position derives from
//! somewhere else.

use std::sync::Arc;

use tutti_core::graph::{OutPort, Source};
use tutti_core::{Beat, Bpm, ChannelLayout, NodeKey, SampleRate, Samples};
use tutti_graph::{
    Cx, Editor, Executor, ForkByClone, ForkMode, ForkTarget, IntoNode, Io, Node, Prepare, Shape,
    Status, Transport,
};
use tutti_io::Wave;
use tutti_sampler::{
    MemorySource, Playback, SlotId, Voice, VoiceCommand, VoiceNode, VoicePool, VoiceSource,
};

const RATE: SampleRate = SampleRate(44_100.0);

fn ramp_wave() -> Arc<Wave> {
    Arc::new(Wave::from_samples(
        44_100.0,
        &(0..4_096)
            .map(|i| (i as f32 + 1.0) / 4_096.0)
            .collect::<Vec<_>>(),
    ))
}

fn placed() -> MemorySource {
    MemorySource::placed(ramp_wave(), Beat::new(0.0), None)
}

fn voice() -> Voice {
    Voice {
        source: VoiceSource::Memory(placed()),
        play: Playback::default(),
        channel_index: None,
    }
}

fn prepare() -> Prepare {
    Prepare::new(RATE, Samples(64))
}

/// `node` at key 1 on every output channel of a `width`-wide graph; its
/// controls.
fn graph<N: IntoNode>(node: N, width: u16) -> (Editor, Executor, N::Controls) {
    let (mut ed, mut exec) = Editor::new(prepare());
    let controls = ed.insert(NodeKey(1), "under test", node);
    ed.spec_mut().topology.outputs = (0..width)
        .map(|port| {
            Source::Node(OutPort {
                node: NodeKey(1),
                port,
            })
        })
        .collect();
    ed.commit().expect("commits");
    exec.apply_pending();
    ed.collect();
    (ed, exec, controls)
}

/// The peak of 64 frames of `exec`'s graph under `transport`.
fn peak(exec: &mut Executor, width: usize, transport: Transport) -> f32 {
    let mut out = vec![vec![0.0f32; 64]; width];
    let mut refs: Vec<&mut [f32]> = out.iter_mut().map(|c| &mut c[..]).collect();
    exec.process(64, &transport, &[], &mut refs);
    out.iter().flatten().fold(0.0f32, |m, s| m.max(s.abs()))
}

fn rolling() -> Transport {
    Transport::new(true, Bpm(120.0), Beat(0.0), None)
}

fn stopped() -> Transport {
    Transport::new(false, Bpm(120.0), Beat(0.0), None)
}

/// The offline fork of `ed`'s graph (what the export takes), applied.
fn fork(ed: &Editor) -> Executor {
    let clock = Arc::new(tutti_core::OfflineTimeline::new(
        &tutti_core::OfflineTimelineConfig {
            start_beat: Beat(0.0),
            tempo: Bpm(120.0),
            sample_rate: RATE,
            loop_range: None,
        },
    ));
    let ctx = tutti_core::transport::OfflineTransport::new(clock);
    let (_ed, mut exec) = ed
        .fork(ForkTarget::Master, ForkMode::Offline(&ctx), prepare())
        .expect("forks");
    exec.apply_pending();
    exec
}

/// **A fork of a pool is born empty and channel-less, and steals no command
/// the live pool needs** — each command is delivered to exactly one consumer,
/// so a shared channel means the worker steals playback.
///
/// Mutation (run): `PoolFork` building its pool over the live pool's
/// command receiver (a clone of it kept in the fork source) → the fork
/// drains the live `AddVoice` → fails. (A fork built from the pool's voices
/// as inserted is not caught: this pool is inserted empty.)
#[test]
fn a_forked_pool_steals_no_commands_from_the_live_one() {
    let (ed, mut exec, handle) = graph(VoicePool::new(), 2);
    let mut forked = fork(&ed);

    // The live handle still feeds the ORIGINAL pool.
    handle
        .send(VoiceCommand::AddVoice {
            id: SlotId(1),
            voice: Box::new(voice()),
            stretch: None,
        })
        .expect("the command queue has room in a test");

    assert!(
        peak(&mut forked, 2, rolling()) == 0.0,
        "the render's pool must never receive live commands"
    );
    assert!(
        peak(&mut exec, 2, rolling()) > 1e-6,
        "the live pool must still receive its own commands"
    );
}

/// **A forked `VoiceNode` plays on its render's transport, not the live one.**
/// The live graph rolls and sounds; the fork, rendered on a stopped
/// transport, renders exact silence.
///
/// Mutation (run): `place` ignoring `run.rolling()` → the fork sounds on
/// the stopped transport → fails.
#[test]
fn a_forked_voice_node_reads_its_renders_transport_not_the_live_one() {
    let (ed, mut exec, _handle) =
        graph(VoiceNode::with_channels(voice(), ChannelLayout::STEREO), 2);
    let mut forked = fork(&ed);
    let live_peak = peak(&mut exec, 2, rolling());
    let forked_peak = peak(&mut forked, 2, stopped());
    assert!(
        live_peak > 1e-6,
        "sanity: the live graph must render audio on a rolling transport, else \
         this comparison proves nothing (peak {live_peak})"
    );
    assert_eq!(
        forked_peak, 0.0,
        "the fork must render silence on its stopped transport; it rendered \
         {forked_peak} (live peak {live_peak})"
    );
    assert!(
        peak(&mut forked, 2, rolling()) > 1e-6,
        "and sound on a rolling one"
    );
}

/// **A bare `MemorySource`** sitting directly in the graph, wrapped in neither
/// a pool nor a voice node: the node `rebind_net_transport`'s type ladder
/// once skipped. It forks through `param_parts`, and reads its render's
/// transport like the others.
///
/// Mutation (run): `ParamNode::fork_fresh` for `MemorySource` leaving the
/// copy free-running (`placed = false`) → the fork plays its stopped cursor:
/// silent on the rolling transport too → the last assertion fails.
#[test]
fn a_bare_memory_source_node_is_forked_too() {
    let (ed, mut exec, _params) = graph(placed(), 2);
    let mut forked = fork(&ed);
    assert!(
        peak(&mut exec, 2, rolling()) > 1e-6,
        "sanity: it sounds live"
    );
    assert_eq!(peak(&mut forked, 2, stopped()), 0.0, "silent when stopped");
    assert!(
        peak(&mut forked, 2, rolling()) > 1e-6,
        "the fork sounds on a rolling render transport"
    );
}

/// A constant: a node that reads no transport.
#[derive(Clone)]
struct Const(f32);

impl Node for Const {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::MONO)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        io.output(0).fill(self.0);
        Status::Modified
    }
    fn reset(&mut self) {}
}

/// **A node that reads no transport is unaffected by the render's.** A fork
/// of a constant renders the constant, stopped or rolling.
///
/// A control for the tests above: no sampler code runs here, so no
/// sampler mutation reaches it. It fails if the render's transport alone
/// changed what a fork renders.
#[test]
fn pure_dsp_is_unaffected() {
    let (ed, _exec, ()) = graph(ForkByClone(Const(0.5)), 1);
    let mut forked = fork(&ed);
    for t in [stopped(), rolling()] {
        let p = peak(&mut forked, 1, t);
        assert!(
            (p - 0.5).abs() < 1e-6,
            "a pure-DSP node must be unaffected by the render's transport, got {p}"
        );
    }
}

/// Passes its input through: a node between a voice and the outputs.
#[derive(Clone)]
struct Pass;

impl Node for Pass {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::MONO, ChannelLayout::MONO)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        let (ins, mut outs) = io.split();
        outs.get(0).copy_from_slice(ins.get(0));
        Status::Modified
    }
    fn reset(&mut self) {}
}

/// **The fork reaches a voice behind another node.** Under `Net`, a voice
/// nested in a sub-net used as a node inherited `rebind_offline`'s
/// do-nothing default until `Net` forwarded it; the graph's fork walks every
/// node an output reaches, and each reads its own `Env`. A voice feeding a
/// pass-through, forked, renders silence on a stopped render transport.
///
/// Mutation (run): `place` ignoring `run.rolling()` → the fork sounds on the
/// stopped transport → fails. (The `Net`-era mutation — the fork keeping the
/// live clock — has no counterpart: a native node holds no clock to keep.)
#[test]
fn a_voice_behind_another_node_is_forked_too() {
    let (mut ed, mut exec) = Editor::new(prepare());
    ed.insert(
        NodeKey(1),
        "voice",
        VoiceNode::with_channels(voice(), ChannelLayout::MONO),
    );
    ed.insert(NodeKey(2), "pass", ForkByClone(Pass));
    ed.spec_mut().topology.edges.insert(
        tutti_core::graph::InPort {
            node: NodeKey(2),
            port: 0,
        },
        tutti_core::graph::Edge::Direct(Source::Node(OutPort {
            node: NodeKey(1),
            port: 0,
        })),
    );
    ed.spec_mut().topology.outputs = vec![Source::Node(OutPort {
        node: NodeKey(2),
        port: 0,
    })];
    ed.commit().expect("commits");
    exec.apply_pending();
    ed.collect();
    let mut forked = fork(&ed);
    let live_peak = peak(&mut exec, 1, rolling());
    assert!(live_peak > 0.0, "sanity: the voice sounds through the pass");
    assert_eq!(
        peak(&mut forked, 1, stopped()),
        0.0,
        "a voice behind another node must follow the fork's transport"
    );
}
