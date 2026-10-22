//! The cost model and coarsening (doc 013): a graph
//! too cheap to spread renders serially on a pool, a wide one spreads, and
//! cheap siblings packed into one task render bit for bit what the serial
//! walk renders.
//!
//! The random-topology check is in `tests/differential.rs`
//! (`coarsened_plans_are_bit_identical`).

mod common;

use common::{bits, test_pool, Kind, TestNode, WORKERS};
use tutti_graph::{Cost, Editor, Executor, Schedule, Transport, Unforkable};
use tutti_types::graph::{Edge, InPort, OutPort, Source};
use tutti_types::{Beat, Bpm, ChannelLayout, NodeKey, Samples};

const MAX_BLOCK: usize = 256;

fn gain(cost: Option<Cost>) -> Unforkable<TestNode> {
    let node = TestNode::new(Kind::Gain {
        gain: 0.75,
        width: 1,
    });
    Unforkable(match cost {
        Some(c) => node.with_cost(c),
        None => node,
    })
}

fn sum(inputs: usize, cost: Option<Cost>) -> Unforkable<TestNode> {
    let node = TestNode::new(Kind::Sum { inputs });
    Unforkable(match cost {
        Some(c) => node.with_cost(c),
        None => node,
    })
}

fn wire(ed: &mut Editor, node: u64, port: u16, from: Source) {
    ed.spec_mut().topology.edges.insert(
        InPort {
            node: NodeKey(node),
            port,
        },
        Edge::Direct(from),
    );
}

fn node(key: u64) -> Source {
    Source::Node(OutPort {
        node: NodeKey(key),
        port: 0,
    })
}

/// A fresh editor and executor, the executor on a pool of `workers` when
/// more than one.
fn pair(workers: usize) -> (Editor, Executor) {
    let (ed, mut exec) = Editor::new(common::prepare(MAX_BLOCK));
    if workers > 1 {
        exec.set_pool(Some(test_pool(workers)));
    }
    (ed, exec)
}

fn commit(ed: &mut Editor, exec: &mut Executor) {
    ed.commit().expect("commits");
    exec.apply_pending();
    ed.collect();
}

/// For each of `fans` global inputs, `lanes` gains off it, summed onto its
/// own output: `tiny/16` of `tutti-nodes`' bench when `fans` is 1.
fn fans(ed: &mut Editor, fans: usize, lanes: usize, gains: Option<Cost>, sums: Option<Cost>) {
    ed.spec_mut().topology.inputs = ChannelLayout::from_count(fans as u16);
    let mut outputs = Vec::new();
    for f in 0..fans {
        let s = 1_000 * (f as u64 + 1);
        ed.insert(NodeKey(s), "sum", sum(lanes, sums));
        for l in 0..lanes {
            let k = s + 1 + l as u64;
            ed.insert(NodeKey(k), "gain", gain(gains));
            wire(ed, k, 0, Source::Global(f as u16));
            wire(ed, s, l as u16, node(k));
        }
        outputs.push(node(s));
    }
    ed.spec_mut().topology.outputs = outputs;
}

/// `lanes` chains of `depth` gains declaring `cost`, each off the global
/// input, summed (lightly) onto one output.
fn wide(ed: &mut Editor, lanes: usize, depth: usize, cost: Option<Cost>) {
    ed.spec_mut().topology.inputs = ChannelLayout::MONO;
    let s = 1_000_000;
    ed.insert(NodeKey(s), "sum", sum(lanes, Some(Cost::Light)));
    for l in 0..lanes {
        let mut last = Source::Global(0);
        for d in 0..depth {
            let k = 1_000 * (l as u64 + 1) + d as u64;
            ed.insert(NodeKey(k), "gain", gain(cost));
            wire(ed, k, 0, last);
            last = node(k);
        }
        wire(ed, s, l as u16, last);
    }
    ed.spec_mut().topology.outputs = vec![node(s)];
}

/// Render `blocks` (input on every channel), every output's samples.
fn render(exec: &mut Executor, inputs: usize, outputs: usize, blocks: &[usize]) -> Vec<Vec<f32>> {
    let mut all = vec![Vec::new(); outputs];
    let mut frame = 0u64;
    for &n in blocks {
        let chans: Vec<Vec<f32>> = (0..inputs)
            .map(|c| {
                (0..n)
                    .map(|k| ((((frame + k as u64) * 7919 + c as u64) % 211) as f32) / 211.0 - 0.5)
                    .collect()
            })
            .collect();
        let ins: Vec<&[f32]> = chans.iter().map(Vec::as_slice).collect();
        let mut chunk = vec![vec![0.0f32; n]; outputs];
        {
            let mut outs: Vec<&mut [f32]> = chunk.iter_mut().map(Vec::as_mut_slice).collect();
            let t = Transport::new(true, Bpm(120.0), Beat(0.0), None);
            exec.process(n, &t, &ins, &mut outs);
        }
        for (a, c) in all.iter_mut().zip(chunk) {
            a.extend(c);
        }
        frame += n as u64;
    }
    all
}

/// Block lengths from one frame to the maximum, ragged.
fn ragged() -> Vec<usize> {
    let mut v: Vec<usize> = (0..40).map(|i| 1 + (i * 37) % MAX_BLOCK).collect();
    v.extend([MAX_BLOCK; 4]);
    v
}

/// `tiny/16`: sixteen light gains pack into two tasks of eight (about two
/// grains of work), and the plan, too cheap to pay for dispatch, renders
/// serially on a pool. The same graph with gains that declare nothing
/// spreads, a task per gain.
///
/// Mutations: drop pass 2 (`pack`) from `coarsen` → the light plan keeps
/// its eighteen tasks → fails; make `cost::schedule`'s `pays` always true
/// → fails.
#[test]
fn a_tiny_plan_packs_and_renders_serially() {
    let (mut ed, mut exec) = pair(4);
    fans(&mut ed, 1, 16, Some(Cost::Light), Some(Cost::Light));
    commit(&mut ed, &mut exec);
    let plan = exec.plan().expect("a plan");
    // The input, two packs of eight gains, the sum and its output.
    assert_eq!(plan.tasks().len(), 4, "{:?}", plan.tasks());
    assert_eq!(plan.schedule(4), Schedule::Serial);
    assert!(!exec.is_parallel());

    let (mut ed, mut exec) = pair(4);
    fans(&mut ed, 1, 16, None, Some(Cost::Light));
    commit(&mut ed, &mut exec);
    let plan = exec.plan().expect("a plan");
    // The input, each gain, the sum and its output.
    assert_eq!(plan.tasks().len(), 18);
    assert!(matches!(plan.schedule(4), Schedule::Parallel { .. }));
    assert!(exec.is_parallel());
}

/// Two `tiny`-shaped fans side by side keep several tasks (two sums each
/// worth one), and the cost model still keeps them serial: spreading them
/// would not pay for its dispatch.
///
/// Mutations: make `cost::schedule`'s `pays` always true (always
/// parallel) → fails; make `par_for` build the parallel state whenever the
/// plan has two tasks, ignoring the schedule → the
/// `is_parallel` assert fails.
#[test]
fn a_cheap_plan_of_several_tasks_renders_serially() {
    let (mut ed, mut exec) = pair(4);
    fans(&mut ed, 2, 8, Some(Cost::Light), Some(Cost::Light));
    commit(&mut ed, &mut exec);
    let plan = exec.plan().expect("a plan");
    assert!(plan.tasks().len() >= 2, "{:?}", plan.tasks());
    for p in [2, 3, 4, 8] {
        assert_eq!(plan.schedule(p), Schedule::Serial, "{p} participants");
    }
    assert!(!exec.is_parallel());
}

/// Eight chains of moderate nodes spread over every participant of a
/// four-participant pool, from a short block length up.
///
/// Mutation: make `cost::schedule` return `Serial` (always serial) →
/// fails.
#[test]
fn a_wide_plan_spreads() {
    let (mut ed, mut exec) = pair(4);
    wide(&mut ed, 8, 8, Some(Cost::Moderate));
    commit(&mut ed, &mut exec);
    let plan = exec.plan().expect("a plan");
    match plan.schedule(4) {
        Schedule::Parallel { wake, from } => {
            assert_eq!(wake, 4);
            assert!(from <= Samples(64), "from {from:?}");
        }
        s => panic!("{s:?}"),
    }
    assert!(exec.is_parallel());
    // A pool of two wakes both.
    assert!(matches!(
        plan.schedule(2),
        Schedule::Parallel { wake: 2, .. }
    ));
}

/// Light gains beside heavy ones off one input, all summed: the light ones
/// pack (into fewer tasks than gains), each heavy one keeps a task of its
/// own, and at every participant count and ragged block lengths the
/// render is bit for bit the serial one.
///
/// Mutations: in `coarsen.rs`'s `finish`, skip the task edges out of any
/// task of more than one op (a dependency dropped when merging) → the
/// verifier refuses the plan in `compile` ("crosses task") → fails.
#[test]
fn packed_siblings_render_bit_identically() {
    let build = |workers: usize| {
        let (mut ed, mut exec) = pair(workers);
        ed.spec_mut().topology.inputs = ChannelLayout::MONO;
        let s = 1_000_000;
        ed.insert(NodeKey(s), "sum", sum(15, None));
        for l in 0..15u64 {
            // Every fifth gain declares nothing (heavy).
            let cost = (l % 5 != 4).then_some(Cost::Light);
            ed.insert(NodeKey(l + 1), "gain", gain(cost));
            wire(&mut ed, l + 1, 0, Source::Global(0));
            wire(&mut ed, s, l as u16, node(l + 1));
        }
        ed.spec_mut().topology.outputs = vec![node(s)];
        commit(&mut ed, &mut exec);
        exec
    };
    let serial = {
        let mut exec = build(1);
        let plan = exec.plan().expect("a plan").clone();
        tutti_graph::verify(&plan).expect("sound");
        // The input, the three heavy gains, the packed light ones (twelve
        // gains in fewer tasks), the sum and its output.
        let heavy = |op: u32| {
            matches!(plan.ops()[op as usize], tutti_graph::Op::Node { unit, .. }
                if plan.units()[unit as usize].shape.cost == Cost::Heavy
                    && plan.units()[unit as usize].shape.audio_in.count() == 1)
        };
        let mut light_tasks = 0;
        for t in plan.tasks() {
            let ops = &plan.task_ops()[t.start as usize..(t.start + t.len) as usize];
            if ops.iter().any(|&o| heavy(o)) {
                assert_eq!(ops.len(), 1, "a heavy gain shares a task");
            } else if ops.len() > 1 {
                light_tasks += 1;
            }
        }
        assert!(light_tasks >= 1, "some light gains packed");
        assert!(plan.tasks().len() < 1 + 15 + 1, "{:?}", plan.tasks());
        assert!(matches!(plan.schedule(4), Schedule::Parallel { .. }));
        render(&mut exec, 1, 1, &ragged())
    };
    assert!(serial[0].iter().any(|&x| x != 0.0));
    for workers in WORKERS {
        let mut exec = build(workers);
        assert!(exec.is_parallel());
        let parallel = render(&mut exec, 1, 1, &ragged());
        assert_eq!(bits(&serial), bits(&parallel), "{workers} workers");
    }
}

/// A pool that counts the blocks it is asked to run.
struct Counting {
    inner: std::sync::Arc<dyn tutti_graph::Pool>,
    runs: std::sync::atomic::AtomicUsize,
}

impl tutti_graph::Pool for Counting {
    fn participants(&self) -> usize {
        self.inner.participants()
    }
    fn run(&self, wake: usize, job: &(dyn Fn(usize) + Sync)) {
        self.runs.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.inner.run(wake, job);
    }
    fn waits(&self) -> tutti_graph::BlockWaits {
        self.inner.waits()
    }
}

/// The executor honours the schedule's `from` per block: a block one frame
/// shorter than `from` runs the serial walk (the pool is not asked), a
/// block of exactly `from` frames is spread (the pool runs it once).
///
/// Mutations: make `Par::spreads` always true → the short block reaches
/// the pool → fails; always false → the long block does not → fails.
#[test]
fn blocks_shorter_than_from_run_serially() {
    let (mut ed, mut exec) = Editor::new(common::prepare(MAX_BLOCK));
    let pool = std::sync::Arc::new(Counting {
        inner: test_pool(4),
        runs: std::sync::atomic::AtomicUsize::new(0),
    });
    exec.set_pool(Some(pool.clone()));
    wide(&mut ed, 4, 4, Some(Cost::Moderate));
    commit(&mut ed, &mut exec);
    let plan = exec.plan().expect("a plan").clone();
    let Schedule::Parallel { from, .. } = plan.schedule(4) else {
        panic!("{:?}", plan.schedule(4));
    };
    assert!(
        from > Samples(1) && from <= Samples(MAX_BLOCK),
        "from {from:?}"
    );
    assert!(exec.is_parallel());
    let runs = || pool.runs.load(std::sync::atomic::Ordering::Relaxed);
    render(&mut exec, 1, 1, &[from.0 - 1]);
    assert_eq!(runs(), 0, "a block shorter than `from` reached the pool");
    render(&mut exec, 1, 1, &[from.0]);
    assert_eq!(runs(), 1, "a block of `from` frames was not spread");
}
