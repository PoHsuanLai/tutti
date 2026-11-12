//! Edge fades (doc 030 section 4): a lane an editor marks `EdgeFade::In` or
//! `Out` moves its weight linearly over `EDGE_DECLICK` frames, counted from its
//! first block under the fade and across blocks, and the editor learns when it
//! ended (`Editor::settle_fades`). `Editor` itself fades nothing unless asked.
//!
//! Every figure is analytic: the weights below are powers of two over 256, so
//! the expected sample is exact.

use tutti_graph::testing as common;

use common::{Kind, TestNode, prepare};
use tutti_graph::{
    EDGE_DECLICK, Edge, EdgeFade, EdgeGain, EdgeId, Editor, Executor, NodeKey, OutPort, Sink,
    Source, Transport, Unforkable,
};
use tutti_types::Amplitude;

const BLOCK: usize = 64;
const SPAN: usize = 256;

fn key(n: u64) -> NodeKey {
    NodeKey(n)
}

fn out(node: u64) -> Source {
    Source::Node(OutPort {
        node: key(node),
        port: 0,
    })
}

fn output_edge(from: u64) -> EdgeId {
    EdgeId {
        sink: Sink::Output(0),
        from: Edge::Direct(out(from)),
    }
}

/// Constants `values` (keys `1..`) summed at output 0.
struct Rig {
    ed: Editor,
    exec: Executor,
}

impl Rig {
    fn new(values: &[f32]) -> Self {
        let (mut ed, mut exec) = Editor::new(prepare(BLOCK));
        for (i, &value) in values.iter().enumerate() {
            let n = i as u64 + 1;
            let k = Kind::Const { value, width: 1 };
            ed.insert(key(n), "const", Unforkable(TestNode::new(k)));
        }
        ed.spec_mut().topology.outputs = vec![(1..=values.len() as u64).map(out).collect()];
        ed.commit().expect("commits");
        exec.apply_pending();
        ed.collect();
        Self { ed, exec }
    }

    fn blocks(&mut self, n: usize) -> Vec<f32> {
        let mut all = Vec::new();
        for _ in 0..n {
            let mut block = vec![f32::NAN; BLOCK];
            self.exec.process(
                BLOCK,
                &Transport::default(),
                &[],
                &mut [block.as_mut_slice()],
            );
            all.extend(block);
        }
        self.ed.collect();
        all
    }

    /// Commit what the spec says now and apply it.
    fn commit(&mut self) {
        self.ed.commit().expect("commits");
        self.exec.apply_pending();
        self.ed.collect();
    }
}

fn faded(base: f32) -> EdgeGain {
    EdgeGain::new(Amplitude::new(base))
}

/// **A fade in starts at exactly 0 and is the weight at `EDGE_DECLICK`.**
/// Frame `j` of the fade is `base * j / 256`, across the block boundaries
/// (blocks of 64), and every frame after the 256th is the base itself.
#[test]
fn a_fade_in_ramps_from_exactly_zero_and_reaches_its_weight_at_edge_declick() {
    assert_eq!(EDGE_DECLICK.get(), SPAN);
    let mut rig = Rig::new(&[1.0]);
    rig.ed.spec_mut().set_edge_gain(output_edge(1), faded(0.5));
    rig.ed
        .set_edge_fade(output_edge(1), EdgeFade::In)
        .expect("the edge exists");
    rig.commit();

    let got = rig.blocks(6);
    assert_eq!(
        got[0].to_bits(),
        0.0f32.to_bits(),
        "the first frame is +0.0"
    );
    for (j, &g) in got.iter().enumerate().take(SPAN) {
        let want = 0.5 * (j as f32 / SPAN as f32);
        assert_eq!(g.to_bits(), want.to_bits(), "frame {j}");
    }
    for (j, &g) in got.iter().enumerate().skip(SPAN) {
        assert_eq!(g, 0.5, "frame {j}: at the weight from EDGE_DECLICK on");
    }
}

/// **A fade out leaves from the weight it has and its last nonzero frame is
/// inside the span**; it then reads 0, and the editor is told once and drops
/// the edge, after which a commit leaves the sink silent.
#[test]
fn a_fade_out_ends_inside_the_span_and_the_edge_leaves_the_spec() {
    let mut rig = Rig::new(&[1.0]);
    // Steady for a block first: the fade leaves from weight 1.
    rig.blocks(1);
    rig.ed
        .set_edge_fade(output_edge(1), EdgeFade::Out)
        .expect("the edge exists");
    rig.commit();

    let got = rig.blocks(6);
    for (j, &g) in got.iter().enumerate().take(SPAN) {
        let want = 1.0 - j as f32 / SPAN as f32;
        assert_eq!(g.to_bits(), want.to_bits(), "frame {j}");
    }
    let last = got.iter().rposition(|&x| x != 0.0).expect("it was heard");
    assert!(
        last < SPAN,
        "the last nonzero frame {last} is inside the span"
    );
    assert!(got[SPAN..].iter().all(|&x| x == 0.0), "and silence after");

    assert!(
        rig.ed.spec().has_edge(output_edge(1)),
        "held until reported"
    );
    assert!(rig.ed.settle_fades(), "the ended fade changed the spec");
    assert!(!rig.ed.spec().has_edge(output_edge(1)), "and took the edge");
    assert!(!rig.ed.settle_fades(), "reported once");
    rig.commit();
    assert!(rig.blocks(1).iter().all(|&x| x == 0.0));
}

/// **A fade in that ended leaves a plain edge**: the report drops the fade and
/// the entry it came with, and the sum is the plain sum again.
#[test]
fn an_ended_fade_in_leaves_a_plain_edge() {
    let mut rig = Rig::new(&[1.0, 2.0]);
    rig.ed
        .set_edge_fade(output_edge(1), EdgeFade::In)
        .expect("the edge exists");
    rig.commit();
    rig.blocks(5);
    assert!(rig.ed.settle_fades());
    assert!(
        !rig.ed.spec().gains.contains_key(&output_edge(1)),
        "no entry: the edge is plain again"
    );
    rig.commit();
    assert!(rig.blocks(1).iter().all(|&x| x == 3.0));
}

/// **An edge brought back while it fades out turns round from where it is**
/// (no step): `In` after a half-done `Out` rises from the weight the lane had.
#[test]
fn a_fade_reversed_midway_continues_from_the_weight_it_had() {
    let mut rig = Rig::new(&[1.0]);
    rig.blocks(1);
    rig.ed
        .set_edge_fade(output_edge(1), EdgeFade::Out)
        .expect("the edge exists");
    rig.commit();
    let down = rig.blocks(2); // 128 frames: weight 1 - 127/256 at the end
    let at = *down.last().expect("frames");
    rig.ed
        .set_edge_fade(output_edge(1), EdgeFade::In)
        .expect("the edge exists");
    rig.commit();
    let up = rig.blocks(1);
    assert_eq!(up[0], at, "rises from the weight the fade out had reached");
    assert!(up[BLOCK - 1] > up[0], "and rises");
}

/// **The parallel executor renders a graph in the middle of fades exactly as
/// the serial one**, bit for bit, across blocks of different lengths, with
/// fades in, out and a weight moved under it.
#[test]
fn parallel_equals_serial_for_a_graph_mid_fade() {
    fn run(workers: usize) -> Vec<u32> {
        let values: Vec<f32> = (1..=8).map(|i| i as f32 * 0.37 - 1.1).collect();
        let (mut ed, mut exec) = Editor::new(prepare(BLOCK));
        if workers > 1 {
            exec.set_pool(Some(common::TestPool::new(workers)));
        }
        for (i, &value) in values.iter().enumerate() {
            let k = Kind::Const { value, width: 1 };
            ed.insert(key(i as u64 + 1), "const", Unforkable(TestNode::new(k)));
        }
        ed.spec_mut().topology.outputs = vec![(1..=8).map(out).collect()];
        ed.commit().expect("commits");
        exec.apply_pending();
        ed.collect();
        let mut got = Vec::new();
        let mut run_blocks = |exec: &mut Executor, ed: &mut Editor, sizes: &[usize]| {
            for &n in sizes {
                let mut block = vec![f32::NAN; n];
                exec.process(n, &Transport::default(), &[], &mut [block.as_mut_slice()]);
                ed.collect();
                got.extend(block.iter().map(|x| x.to_bits()));
            }
        };
        run_blocks(&mut exec, &mut ed, &[64, 64]);
        for k in 1..=8u64 {
            let fade = if k % 2 == 0 {
                EdgeFade::In
            } else {
                EdgeFade::Out
            };
            ed.set_edge_fade(output_edge(k), fade).expect("edge");
            ed.set_edge_gain(output_edge(k), Amplitude::new(0.25 * k as f32))
                .expect("edge");
        }
        ed.commit().expect("commits");
        exec.apply_pending();
        assert_eq!(exec.is_parallel(), workers > 1);
        run_blocks(&mut exec, &mut ed, &[37, 64, 5, 64, 64, 11, 64, 64, 64]);
        got
    }
    let serial = run(1);
    assert_eq!(run(4), serial, "four participants against one");
}
