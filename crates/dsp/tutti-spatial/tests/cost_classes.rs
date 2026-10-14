//! The VBAP panner declares `Cost::Moderate` (doc 013, "Phase 6, part
//! (b)"); the HRTF renderer stays `Heavy`. Declaring a class changes only
//! the parallel schedule: sibling panners pack into fewer tasks than
//! undeclared ones, and the render on a pool is bit for bit the serial one.

use std::sync::Arc;

use tutti_core::graph::{Edge, InPort, OutPort, Source};
use tutti_core::WorkerPool;
use tutti_core::{Azimuth, Beat, Bpm, ChannelLayout, Elevation, NodeKey, SampleRate, Samples};
use tutti_graph::{
    Cost, Cx, Editor, Executor, Io, Node, Prepare, Schedule, Shape, Status, Transport, Unforkable,
};
use tutti_nodes::ChannelSumNode;
use tutti_spatial::VbapPannerNode;

const SR: f64 = 48_000.0;
/// Small enough that a stereo `Moderate` panner is under the packing grain.
const MAX_BLOCK: usize = 64;
/// Panners as shipped (`Moderate`).
const PANNERS: u64 = 12;
/// Panners declaring nothing (`Heavy`), so the plan still spreads.
const HEAVY: u64 = 2;

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

/// A stereo panner aimed by `k`.
fn panner(k: u64) -> VbapPannerNode {
    let node = VbapPannerNode::stereo().expect("stereo preset");
    node.set_position(Azimuth(-30.0 + 4.0 * k as f32), Elevation(0.0));
    node
}

fn wire(ed: &mut Editor, to: NodeKey, port: u16, from: Source) {
    ed.spec_mut()
        .topology
        .edges
        .insert(InPort { node: to, port }, Edge::Direct(from));
}

/// [`PANNERS`] declared panners and [`HEAVY`] undeclared ones, each off the
/// stereo global input, summed (a `Light` bus) onto two global outputs; on
/// a pool of `workers` when more than one. `declared`: the panners as
/// shipped, or every one wrapped to declare nothing.
fn graph(workers: usize, declared: bool) -> Executor {
    let (mut ed, mut exec) = Editor::new(Prepare::new(SampleRate(SR), Samples(MAX_BLOCK)));
    if workers > 1 {
        exec.set_pool(Some(Arc::new(WorkerPool::new(workers))));
    }
    ed.spec_mut().topology.inputs = ChannelLayout::STEREO;
    const SUM: NodeKey = NodeKey(1_000);
    let sources = PANNERS + HEAVY;
    ed.insert(
        SUM,
        "sum",
        ChannelSumNode::new(sources as usize, ChannelLayout::STEREO),
    );
    for k in 0..sources {
        let key = NodeKey(k + 1);
        if declared && k < PANNERS {
            ed.insert(key, "vbap", panner(k));
        } else {
            ed.insert(key, "vbap", Unforkable(Undeclared(panner(k))));
        }
        for c in 0..2u16 {
            wire(&mut ed, key, c, Source::Global(c));
            wire(
                &mut ed,
                SUM,
                k as u16 * 2 + c,
                Source::Node(OutPort { node: key, port: c }),
            );
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

/// Blocks from one frame to the maximum, ragged, a distinct input on each
/// channel.
fn render(exec: &mut Executor) -> Vec<Vec<f32>> {
    let mut all = vec![Vec::new(); 2];
    let mut frame = 0usize;
    for i in 0..60 {
        let n = 1 + (i * 37) % MAX_BLOCK;
        let ins: Vec<Vec<f32>> = (0..2)
            .map(|c| {
                (0..n)
                    .map(|k| (((frame + k) * 7_919 + c) % 211) as f32 / 211.0 - 0.5)
                    .collect()
            })
            .collect();
        let ins: Vec<&[f32]> = ins.iter().map(Vec::as_slice).collect();
        let mut chunk = vec![vec![0.0f32; n]; 2];
        {
            let mut outs: Vec<&mut [f32]> = chunk.iter_mut().map(Vec::as_mut_slice).collect();
            let t = Transport::new(true, Bpm(120.0), Beat(0.0), None);
            exec.process(n, &t, &ins, &mut outs);
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

/// A panner of any width is `Moderate`.
///
/// Mutation (run): the panner's `with_cost` removed → fails.
#[test]
fn a_panner_is_moderate() {
    for node in [
        VbapPannerNode::stereo(),
        VbapPannerNode::quad(),
        VbapPannerNode::surround_5_1(),
    ] {
        assert_eq!(node.expect("preset").shape().cost, Cost::Moderate);
    }
}

/// Twelve `Moderate` panners beside two undeclared ones: the moderate ones
/// pack into fewer tasks than the same panners declaring nothing, the plan
/// still spreads, and on a pool of two and of four the render is bit for
/// bit the serial one.
///
/// Mutation (run): the panner's `with_cost(Cost::Moderate)` removed → the
/// panners pack no further than undeclared → the task-count assert fails.
#[test]
#[cfg_attr(miri, ignore = "spawns a worker pool")]
fn moderate_panners_pack_and_render_bit_identically() {
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
