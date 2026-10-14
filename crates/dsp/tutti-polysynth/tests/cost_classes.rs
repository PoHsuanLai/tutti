//! The synth declares a cost class from its oscillator lanes (doc 013,
//! "Phase 6, part (b)"): `Moderate` up to sixteen (`max_voices` × unison),
//! `Heavy` past them. Declaring one changes only the parallel schedule:
//! sibling synths pack into fewer tasks than undeclared ones, and the render
//! on a pool is bit for bit the serial one.

use std::sync::Arc;

use tutti_core::graph::{Edge, InPort, OutPort, Source};
use tutti_core::{Beat, Bpm, ChannelLayout, NodeKey, SampleRate, Samples, WorkerPool};
use tutti_graph::{
    Cost, Cx, Editor, EventEdge, EventIn, EventOut, Executor, Io, Node, Prepare, Schedule, Shape,
    Status, Transport, Unforkable,
};
use tutti_midi_runtime::{MidiClipNode, TimedMidiEvent};
use tutti_midi_types::{MidiChannel, MidiEvent, MidiGroup};
use tutti_nodes::ChannelSumNode;
use tutti_polysynth::{OscillatorType, PolySynth, SynthConfig, UnisonConfig};

const SR: f64 = 48_000.0;
const BPM: f64 = 120.0;
/// Small enough that a `Moderate` stereo synth is under the packing grain.
const MAX_BLOCK: usize = 64;
/// Synths at the default eight voices (`Moderate`).
const LIGHT: u64 = 8;
/// Synths at 32 voices (`Heavy`), so the plan still spreads.
const HEAVY: u64 = 2;

fn synth(max_voices: usize, unison: u8) -> PolySynth {
    PolySynth::new(SynthConfig {
        sample_rate: SampleRate(SR),
        oscillator: OscillatorType::Saw,
        max_voices,
        unison: (unison > 1).then(|| UnisonConfig {
            voice_count: unison,
            ..Default::default()
        }),
        ..Default::default()
    })
    .expect("synth builds")
}

/// `node` as a node that declares nothing: the cost model's default.
struct Undeclared<N>(N);

impl<N: Node> Node for Undeclared<N> {
    fn shape(&self) -> Shape {
        self.0.shape().with_cost(Cost::Heavy)
    }
    fn prepare(&mut self, p: &Prepare) {
        self.0.prepare(p);
    }
    fn process(&mut self, cx: &Cx<'_>, io: Io<'_>) -> Status {
        self.0.process(cx, io)
    }
    fn reset(&mut self) {
        self.0.reset();
    }
}

/// Two notes for synth `k`, in the first hundredth of a beat.
fn notes(k: u64) -> Vec<TimedMidiEvent> {
    (0..2)
        .map(|i| {
            TimedMidiEvent::new(
                Beat(0.001 * (k + 3 * i) as f64),
                MidiEvent::note_on(
                    MidiGroup::FIRST,
                    MidiChannel::FIRST,
                    48 + (k * 5 + i * 7) as u8,
                    0xFFFF,
                ),
            )
        })
        .collect()
}

/// [`LIGHT`] eight-voice synths and [`HEAVY`] 32-voice ones, each played
/// by a clip of its own, summed (a `Light` bus) onto two global outputs;
/// on a pool of `workers` when more than one. `declared`: the eight-voice
/// synths as shipped, or wrapped to declare nothing.
fn graph(workers: usize, declared: bool) -> Executor {
    let (mut ed, mut exec) = Editor::new(Prepare::new(SampleRate(SR), Samples(MAX_BLOCK)));
    if workers > 1 {
        exec.set_pool(Some(Arc::new(WorkerPool::new(workers))));
    }
    const SUM: NodeKey = NodeKey(1_000);
    let sources = LIGHT + HEAVY;
    ed.insert(
        SUM,
        "sum",
        ChannelSumNode::new(sources as usize, ChannelLayout::STEREO),
    );
    for k in 0..sources {
        let (clip, key) = (NodeKey(100 + k), NodeKey(k + 1));
        ed.insert(clip, "clip", MidiClipNode::new(notes(k)));
        match (k < LIGHT, declared) {
            (true, true) => {
                ed.insert(key, "synth", synth(8, 1));
            }
            (true, false) => {
                ed.insert(key, "synth", Unforkable(Undeclared(synth(8, 1))));
            }
            (false, _) => {
                ed.insert(key, "synth", synth(32, 1));
            }
        }
        ed.spec_mut().connect_events(
            EventIn { node: key, port: 0 },
            EventEdge::Direct(EventOut {
                node: clip,
                port: 0,
            }),
        );
        for c in 0..2u16 {
            let port = InPort {
                node: SUM,
                port: k as u16 * 2 + c,
            };
            let from = Source::Node(OutPort { node: key, port: c });
            ed.spec_mut()
                .topology
                .edges
                .insert(port, Edge::Direct(from));
        }
    }
    ed.spec_mut().topology.outputs = (0..2)
        .map(|port| Source::Node(OutPort { node: SUM, port }))
        .collect();
    ed.commit().expect("commits");
    exec.apply_pending();
    ed.collect();
    exec
}

/// Blocks from one frame to the maximum, ragged, rolling from beat 0.
fn render(exec: &mut Executor) -> Vec<Vec<f32>> {
    let mut all = vec![Vec::new(); 2];
    let mut frame = 0usize;
    for i in 0..60 {
        let n = 1 + (i * 37) % MAX_BLOCK;
        let mut chunk = vec![vec![0.0f32; n]; 2];
        {
            let mut outs: Vec<&mut [f32]> = chunk.iter_mut().map(Vec::as_mut_slice).collect();
            let beat = frame as f64 * BPM / 60.0 / SR;
            let t = Transport::new(true, Bpm(BPM), Beat(beat), None);
            exec.process(n, &t, &[], &mut outs);
        }
        for (a, c) in all.iter_mut().zip(chunk) {
            a.extend(c);
        }
        frame += n;
    }
    all
}

fn bits(v: &[Vec<f32>]) -> Vec<Vec<u32>> {
    v.iter()
        .map(|c| c.iter().map(|x| x.to_bits()).collect())
        .collect()
}

/// Sixteen lanes and fewer are `Moderate`, more are `Heavy`, whether the
/// lanes are voices or unison sub-voices.
///
/// Mutation (run): `MODERATE_LANES` 16 → 8 → the 16-voice and the 8 × 2
/// asserts fail.
#[test]
fn the_class_follows_the_lane_count() {
    assert_eq!(synth(8, 1).shape().cost, Cost::Moderate);
    assert_eq!(synth(16, 1).shape().cost, Cost::Moderate);
    assert_eq!(synth(8, 2).shape().cost, Cost::Moderate);
    assert_eq!(synth(17, 1).shape().cost, Cost::Heavy);
    assert_eq!(synth(8, 4).shape().cost, Cost::Heavy);
    assert_eq!(synth(32, 1).shape().cost, Cost::Heavy);
}

/// Eight `Moderate` synths beside two `Heavy` ones: the moderate ones (each
/// fused with its clip) pack into fewer tasks than the same synths
/// declaring nothing, the plan still spreads, and on a pool of two and of
/// four the render is bit for bit the serial one.
///
/// Mutation (run): `PolySynth::cost` always `Heavy` → the moderate synths
/// pack no further than undeclared → the task-count assert fails.
#[test]
#[cfg_attr(miri, ignore = "spawns a worker pool")]
fn moderate_synths_pack_and_render_bit_identically() {
    let declared = graph(4, true);
    let undeclared = graph(4, false);
    let (plan, bare) = (
        declared.plan().expect("a plan"),
        undeclared.plan().expect("a plan"),
    );
    assert!(
        plan.tasks().len() < bare.tasks().len(),
        "declared {} tasks, undeclared {}",
        plan.tasks().len(),
        bare.tasks().len()
    );
    // From a one-frame block: every ragged block below is spread, so the
    // comparison is never a serial walk against itself.
    assert!(
        matches!(plan.schedule(4), Schedule::Parallel { from, .. } if from == Samples(1)),
        "{:?}",
        plan.schedule(4)
    );

    let serial = render(&mut graph(1, true));
    assert!(serial.iter().all(|c| c.iter().any(|&x| x != 0.0)));
    for workers in [2, 4] {
        let mut exec = graph(workers, true);
        assert!(exec.is_parallel());
        assert_eq!(bits(&serial), bits(&render(&mut exec)), "{workers} workers");
    }
}
