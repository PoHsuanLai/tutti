//! The metronome declares `Cost::Moderate` (doc 013, "Phase 6, part (b)").
//! Declaring it changes only the parallel schedule: sibling clicks pack
//! into fewer tasks than undeclared ones, and the render on a pool is bit
//! for bit the serial one.

use std::sync::Arc;

use tutti_core::{
    Beat, Bpm, ChannelLayout, ClickNode, ClickSettings, MetronomeMode, SampleRate, Samples,
    Transport, WorkerPool,
};
use tutti_graph::{Cost, Cx, Editor, Executor, Io, Node, Prepare, Schedule, Shape, Status};
use tutti_graph::{Transport as BlockTransport, Unforkable};
use tutti_types::graph::{Edge, InPort, OutPort, Source};
use tutti_types::NodeKey;

const SR: f64 = 48_000.0;
const BPM: f64 = 120.0;
/// Small enough that a stereo `Moderate` click is under the packing grain.
const MAX_BLOCK: usize = 64;
/// Clicks as shipped (`Moderate`).
const CLICKS: u64 = 12;
/// Clicks declaring nothing (`Heavy`), so the plan still spreads.
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

/// `sources` stereo inputs summed onto a stereo pair, each source scaled by
/// its own weight (so a click's place in the sum shows). `Light`.
struct Sum(usize);

impl Node for Sum {
    fn shape(&self) -> Shape {
        Shape::audio(
            ChannelLayout::from_count(2 * self.0 as u16),
            ChannelLayout::STEREO,
        )
        .with_cost(Cost::Light)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        let (ins, mut outs) = io.split();
        for c in 0..2 {
            let out = outs.get(c);
            out.fill(0.0);
            for s in 0..self.0 {
                let w = 1.0 / (s + 1) as f32;
                for (y, x) in out.iter_mut().zip(ins.get(2 * s + c)) {
                    *y += x * w;
                }
            }
        }
        Status::Modified
    }
    fn reset(&mut self) {}
}

fn click(transport: &Transport) -> ClickNode {
    let settings = Arc::new(ClickSettings::new());
    settings.set_mode(MetronomeMode::Always);
    ClickNode::new(transport, settings)
}

/// [`CLICKS`] declared clicks and [`HEAVY`] undeclared ones, summed onto
/// two global outputs; on a pool of `workers` when more than one.
/// `declared`: the clicks as shipped, or every one wrapped to declare
/// nothing.
fn graph(workers: usize, declared: bool) -> Executor {
    let transport = Transport::new(SR);
    let (mut ed, mut exec) = Editor::new(Prepare::new(SampleRate(SR), Samples(MAX_BLOCK)));
    if workers > 1 {
        exec.set_pool(Some(Arc::new(WorkerPool::new(workers))));
    }
    const SUM: NodeKey = NodeKey(1_000);
    let sources = CLICKS + HEAVY;
    ed.insert(SUM, "sum", Unforkable(Sum(sources as usize)));
    for k in 0..sources {
        let key = NodeKey(k + 1);
        if declared && k < CLICKS {
            ed.insert(key, "click", click(&transport));
        } else {
            ed.insert(key, "click", Unforkable(Undeclared(click(&transport))));
        }
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

/// Blocks from one frame to the maximum, ragged, rolling from beat 0 past
/// the second beat's click.
fn render(exec: &mut Executor) -> Vec<Vec<f32>> {
    let mut all = vec![Vec::new(); 2];
    let mut frame = 0usize;
    for i in 0..800 {
        let n = 1 + (i * 37) % MAX_BLOCK;
        let mut chunk = vec![vec![0.0f32; n]; 2];
        {
            let mut outs: Vec<&mut [f32]> = chunk.iter_mut().map(Vec::as_mut_slice).collect();
            let beat = frame as f64 * BPM / 60.0 / SR;
            let t = BlockTransport::new(true, Bpm(BPM), Beat(beat), None);
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

/// Twelve `Moderate` clicks beside two undeclared ones: the moderate ones
/// pack into fewer tasks than the same clicks declaring nothing, the plan
/// still spreads, and on a pool of two and of four the render is bit for
/// bit the serial one.
///
/// Mutation (run): the click's `with_cost(Cost::Moderate)` removed → the
/// clicks pack no further than undeclared → the task-count assert fails.
#[test]
#[cfg_attr(miri, ignore = "spawns a worker pool")]
fn moderate_clicks_pack_and_render_bit_identically() {
    assert_eq!(click(&Transport::new(SR)).shape().cost, Cost::Moderate);
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
