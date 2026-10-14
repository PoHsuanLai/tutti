//! The sampler's nodes declare a cost class (doc 013, "Phase 6, part (b)"):
//! a plain read is `Moderate`, a stretched voice and a pool are `Heavy`.
//! Declaring one changes only the parallel schedule: sibling reads pack into
//! fewer tasks than undeclared ones, and the render on a pool is bit for bit
//! the serial one.

use std::sync::Arc;

use tutti_core::graph::{Edge, InPort, OutPort, Source};
use tutti_core::{
    Beat, Bpm, ChannelLayout, NodeKey, SampleRate, Samples, StretchFactor, WorkerPool,
};
use tutti_graph::{
    Cost, Cx, Editor, Executor, Io, Node, Prepare, Schedule, Shape, Status, Transport, Unforkable,
};
use tutti_nodes::ChannelSumNode;
use tutti_sampler::{MemorySource, Playback, Voice, VoiceNode, VoicePool, VoiceSource, Wave};

const SR: f64 = 48_000.0;
const BPM: f64 = 120.0;
/// Small enough that a stereo `Moderate` read is under the packing grain.
const MAX_BLOCK: usize = 64;
const READS: u64 = 12;
const STRETCHED: u64 = 2;

/// A stereo tone, `secs` long, every sample distinct.
fn tone(secs: f64) -> Arc<Wave> {
    let mut w = Wave::new(2, SR);
    for i in 0..(secs * SR) as usize {
        let t = i as f32 / SR as f32;
        w.push_frame(&[(t * 2_765.0).sin() * 0.4, (t * 4_148.0).sin() * 0.3]);
    }
    Arc::new(w)
}

fn stretched(wave: &Arc<Wave>) -> VoiceNode {
    VoiceNode::new(Voice {
        source: VoiceSource::Memory(MemorySource::placed(Arc::clone(wave), Beat(0.0), None)),
        play: Playback {
            stretch: StretchFactor::new(0.5),
            ..Default::default()
        },
        channel_index: None,
    })
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

/// [`READS`] placed memory reads, each starting a little later, and
/// [`STRETCHED`] stretched voices, on a pool of `workers` when more than
/// one. `declared`: the reads as shipped, or wrapped to declare nothing.
/// `bus`: every node summed (a `Light` bus) onto two global outputs, or
/// each on two global outputs of its own (a track wired straight out).
fn graph(wave: &Arc<Wave>, workers: usize, declared: bool, bus: bool) -> Executor {
    let (mut ed, mut exec) = Editor::new(Prepare::new(SampleRate(SR), Samples(MAX_BLOCK)));
    if workers > 1 {
        exec.set_pool(Some(Arc::new(WorkerPool::new(workers))));
    }
    const SUM: NodeKey = NodeKey(1_000);
    let sources = READS + STRETCHED;
    if bus {
        ed.insert(
            SUM,
            "sum",
            ChannelSumNode::new(sources as usize, ChannelLayout::STEREO),
        );
    }
    let mut outputs = Vec::new();
    for k in 0..sources {
        let key = NodeKey(k + 1);
        if k < READS {
            let read = MemorySource::placed(Arc::clone(wave), Beat(k as f64 * 0.001), None);
            if declared {
                ed.insert(key, "read", read);
            } else {
                ed.insert(key, "read", Unforkable(Undeclared(read)));
            }
        } else {
            ed.insert(key, "stretched", stretched(wave));
        }
        for c in 0..2u16 {
            let from = Source::Node(OutPort { node: key, port: c });
            if bus {
                let port = InPort {
                    node: SUM,
                    port: k as u16 * 2 + c,
                };
                ed.spec_mut()
                    .topology
                    .edges
                    .insert(port, Edge::Direct(from));
            } else {
                outputs.push(from);
            }
        }
    }
    if bus {
        outputs = (0..2)
            .map(|port| Source::Node(OutPort { node: SUM, port }))
            .collect();
    }
    ed.spec_mut().topology.outputs = outputs;
    ed.commit().expect("commits");
    exec.apply_pending();
    ed.collect();
    exec
}

/// Blocks from one frame to the maximum, ragged, rolling from beat 0.
fn render(exec: &mut Executor, channels: usize) -> Vec<Vec<f32>> {
    let mut all = vec![Vec::new(); channels];
    let mut frame = 0usize;
    for i in 0..60 {
        let n = 1 + (i * 37) % MAX_BLOCK;
        let mut chunk = vec![vec![0.0f32; n]; channels];
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

/// The classes as declared: a read and a plain voice `Moderate`, a
/// stretched voice and a pool `Heavy`.
///
/// Mutation (run): `VoiceNode::cost` always `Moderate` → the stretched
/// voice's assert fails.
#[test]
fn each_node_declares_its_class() {
    let wave = tone(0.1);
    let plain = VoiceNode::new(Voice {
        source: VoiceSource::Memory(MemorySource::placed(Arc::clone(&wave), Beat(0.0), None)),
        play: Playback::default(),
        channel_index: None,
    });
    assert_eq!(
        MemorySource::new(Arc::clone(&wave)).shape().cost,
        Cost::Moderate
    );
    assert_eq!(plain.shape().cost, Cost::Moderate);
    assert_eq!(stretched(&wave).shape().cost, Cost::Heavy);
    assert_eq!(VoicePool::new().shape().cost, Cost::Heavy);
}

/// Twelve `Moderate` reads beside two stretched voices, summed through a
/// bus and each on outputs of its own: the reads pack into fewer tasks
/// than the same reads declaring nothing, each stretched voice keeps a
/// task of its own, the plan spreads from a one-frame block, and on a pool
/// of two and of four the render is bit for bit the serial one.
///
/// Mutation (run): `MemorySource`'s `with_cost(Cost::Moderate)` removed →
/// the reads pack no further than undeclared (15 tasks each, with the bus)
/// → the task-count assert fails. Mutation (run): one round of packing in
/// `tutti-graph`'s `coarsen` (no fixed point) → without the bus, 20 tasks
/// against 14 → fails.
#[test]
#[cfg_attr(miri, ignore = "spawns a worker pool")]
fn moderate_reads_pack_and_render_bit_identically() {
    let wave = tone(1.0);
    for bus in [true, false] {
        let declared = graph(&wave, 4, true, bus);
        let undeclared = graph(&wave, 4, false, bus);
        let (plan, bare) = (
            declared.plan().expect("a plan"),
            undeclared.plan().expect("a plan"),
        );
        assert!(
            plan.tasks().len() < bare.tasks().len(),
            "bus {bus}: declared {} tasks, undeclared {}",
            plan.tasks().len(),
            bare.tasks().len()
        );
        for t in plan.tasks() {
            let ops = &plan.task_ops()[t.start as usize..(t.start + t.len) as usize];
            let heavy = ops.iter().filter(|&&o| {
                matches!(plan.ops()[o as usize], tutti_graph::Op::Node { unit, .. }
                    if plan.units()[unit as usize].shape.cost == Cost::Heavy)
            });
            assert!(heavy.count() <= 1, "two stretched voices share a task");
        }
        // From a one-frame block: every ragged block below is spread, so
        // the comparison is never a serial walk against itself.
        assert!(
            matches!(plan.schedule(4), Schedule::Parallel { from, .. } if from == Samples(1)),
            "{:?}",
            plan.schedule(4)
        );

        let channels = if bus {
            2
        } else {
            2 * (READS + STRETCHED) as usize
        };
        let serial = render(&mut graph(&wave, 1, true, bus), channels);
        assert!(serial.iter().all(|c| c.iter().any(|&x| x != 0.0)));
        for workers in [2, 4] {
            let mut exec = graph(&wave, workers, true, bus);
            assert!(exec.is_parallel());
            assert_eq!(
                bits(&serial),
                bits(&render(&mut exec, channels)),
                "bus {bus}, {workers} workers"
            );
        }
    }
}
