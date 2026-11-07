//! Taps as plan ops: any port, any width, aligned by the compiler or raw,
//! lossless below the ring's capacity and counted above it, bit-identical on
//! the parallel executor, and allocation-free on every participant.

mod common;

use assert_no_alloc::AllocDisabler;
use common::{Kind, TestNode, prepare};
use tutti_graph::{
    Editor, Executor, ForkCause, ForkMode, ForkSource, ForkTarget, Forked, IntoNode, Node,
    NodeParts, TapReader, Transport,
};
use tutti_types::graph::{Edge, InPort, OutPort, Source};
use tutti_types::{Beat, Bpm, ChannelLayout, NodeKey, Samples, TapAlign, TapId};

#[global_allocator]
static A: AllocDisabler = AllocDisabler;

/// A `Kind` whose fork is a fresh node of the same kind.
struct Forkable(Kind);

struct KindFork(Kind);

impl ForkSource for KindFork {
    fn fork(&self, _mode: ForkMode) -> Result<Forked, ForkCause> {
        Ok(Forked::new(Box::new(TestNode::new(self.0.clone()))))
    }
}

impl IntoNode for Forkable {
    type Controls = ();
    fn into_node(self) -> (Box<dyn Node>, ()) {
        (Box::new(TestNode::new(self.0)), ())
    }
    fn into_parts(self) -> NodeParts<()> {
        NodeParts {
            node: Box::new(TestNode::new(self.0.clone())),
            controls: (),
            fork: Some(Box::new(KindFork(self.0))),
        }
    }
}

const MAX_BLOCK: usize = 128;
const CHAINS: u64 = 6;

fn out(node: u64) -> Source {
    Source::Node(OutPort {
        node: NodeKey(node),
        port: 0,
    })
}

/// `CHAINS` independent chains `Global(0) -> Lag(5 * k) -> Gain`, each its own
/// output channel: different latencies, so the compiler aligns the outputs.
/// Returns the sources of the six gains.
fn build(ed: &mut Editor) -> Vec<Source> {
    ed.spec_mut().topology.inputs = ChannelLayout::MONO;
    let mut gains = Vec::new();
    for k in 0..CHAINS {
        let (lag, gain) = (10 + k, 20 + k);
        ed.insert(
            NodeKey(lag),
            "lag",
            Forkable(Kind::Lag {
                latency: 5 * k as usize,
            }),
        );
        ed.insert(
            NodeKey(gain),
            "gain",
            Forkable(Kind::Gain {
                gain: 0.5 + k as f32 / 10.0,
                width: 1,
            }),
        );
        let t = &mut ed.spec_mut().topology;
        t.edges.insert(
            InPort {
                node: NodeKey(lag),
                port: 0,
            },
            Edge::Direct(Source::Global(0)),
        );
        t.edges.insert(
            InPort {
                node: NodeKey(gain),
                port: 0,
            },
            Edge::Direct(out(lag)),
        );
        gains.push(out(gain));
    }
    ed.spec_mut().topology.outputs = gains.clone();
    gains
}

struct Rig {
    ed: Editor,
    exec: Executor,
    gains: Vec<Source>,
    gated: bool,
}

fn rig(workers: usize, no_alloc: bool) -> Rig {
    let (mut ed, mut exec) = Editor::new(prepare(MAX_BLOCK));
    if workers > 1 {
        exec.set_pool(Some(if no_alloc {
            common::TestPool::no_alloc(workers)
        } else {
            common::test_pool(workers)
        }));
    }
    let gains = build(&mut ed);
    Rig {
        ed,
        exec,
        gains,
        gated: no_alloc,
    }
}

/// One block; returns the output channels.
fn block(r: &mut Rig, n: usize, frame: u64) -> Vec<Vec<f32>> {
    let input: Vec<f32> = (0..n)
        .map(|k| (((frame + k as u64) * 7919) % 211) as f32 / 211.0 - 0.5)
        .collect();
    let mut chans = vec![vec![0.0f32; n]; CHAINS as usize];
    let mut outs: Vec<&mut [f32]> = chans.iter_mut().map(Vec::as_mut_slice).collect();
    let t = Transport::new(true, Bpm(120.0), Beat(0.0), None);
    let ins = [&input[..]];
    // Only the render is inside the gate: this helper's own buffers allocate.
    if r.gated {
        assert_no_alloc::assert_no_alloc(|| r.exec.process(n, &t, &ins, &mut outs));
    } else {
        r.exec.process(n, &t, &ins, &mut outs);
    }
    chans
}

fn drain(tap: &mut TapReader, into: &mut Vec<f32>) {
    let mut buf = vec![0.0f32; tap.width() * 4096];
    loop {
        let n = tap.pop_into(&mut buf).0;
        if n == 0 {
            break;
        }
        into.extend_from_slice(&buf[..n * tap.width()]);
    }
}

/// Renders `blocks` of 100 frames with an aligned six-lane tap on the outputs'
/// sources, a raw two-lane tap and a one-lane aligned tap on a single port.
/// Returns (outputs per channel, aligned tap, raw tap, single tap) interleaved.
fn run(r: &mut Rig, blocks: usize) -> (Vec<Vec<f32>>, Vec<f32>, Vec<f32>, Vec<f32>, u64) {
    let cap = Samples(1 << 16);
    let mut aligned =
        r.ed.open_tap(TapId(1), r.gains.clone(), TapAlign::ToOutput, cap);
    let mut raw =
        r.ed.open_tap(TapId(2), vec![r.gains[0], r.gains[5]], TapAlign::Raw, cap);
    let mut single =
        r.ed.open_tap(TapId(3), vec![r.gains[2]], TapAlign::ToOutput, cap);
    r.ed.commit().expect("compiles");
    r.exec.apply_pending();
    r.ed.collect();
    let mut outputs = vec![Vec::new(); CHAINS as usize];
    let (mut a, mut b, mut c) = (Vec::new(), Vec::new(), Vec::new());
    let mut lost = 0;
    for i in 0..blocks {
        let ch = block(r, 100, i as u64 * 100);
        for (o, x) in outputs.iter_mut().zip(ch) {
            o.extend(x);
        }
        drain(&mut aligned, &mut a);
        drain(&mut raw, &mut b);
        drain(&mut single, &mut c);
        lost += aligned.take_lost().0 + raw.take_lost().0 + single.take_lost().0;
        r.ed.collect();
    }
    (outputs, a, b, c, lost as u64)
}

fn lane(interleaved: &[f32], width: usize, c: usize) -> Vec<f32> {
    interleaved.iter().skip(c).step_by(width).copied().collect()
}

/// An aligned tap of the outputs' sources is the output, sample for sample, on
/// every channel (the compiler gave it the same compensation); a raw tap is
/// the source before alignment, so it differs by the channel's compensation; a
/// one-lane tap on one port works at any width.
///
/// Mutation (run): skip the `DelayKey::Tap` delay in `compile` (emit the tap
/// unaligned) -> the aligned comparison fails on the delayed channels.
#[test]
fn a_tap_is_aligned_by_the_compiler_and_any_width() {
    let mut r = rig(1, false);
    let (outputs, aligned, raw, single, lost) = run(&mut r, 8);
    assert_eq!(lost, 0);
    for (c, output) in outputs.iter().enumerate() {
        assert_eq!(&lane(&aligned, 6, c), output, "aligned lane {c}");
    }
    assert_eq!(lane(&single, 1, 0), outputs[2]);
    // Raw lane 0 is chain 0 (latency 0 of 25, so the output delayed it by
    // 25): the tap is ahead of the output by exactly that.
    let raw0 = lane(&raw, 2, 0);
    assert_eq!(
        raw0[..raw0.len() - 25],
        outputs[0][25..],
        "raw is ahead by the compensation"
    );
    // Raw lane 1 is the slowest chain: no compensation, same as its output.
    assert_eq!(lane(&raw, 2, 1), outputs[5]);
}

/// The parallel render, outputs and every tap, is bit-identical to the serial
/// one, with every participant inside `assert_no_alloc`.
///
/// Mutation (run): drop the `Op::Tap` arm in `par.rs` -> the parallel taps are
/// empty and the comparison fails.
#[test]
fn the_parallel_taps_are_bit_identical_and_allocation_free() {
    let mut s = rig(1, false);
    let serial = run_gated(&mut s);
    for workers in [2, 3, 4] {
        let mut p = rig(workers, true);
        let par = run_gated(&mut p);
        assert_eq!(serial, par, "{workers} workers");
    }
}

fn run_gated(r: &mut Rig) -> (Vec<Vec<f32>>, Vec<f32>, Vec<f32>, Vec<f32>, u64) {
    // The commit allocates and runs outside the gate; the blocks run inside.
    let cap = Samples(1 << 16);
    let mut aligned =
        r.ed.open_tap(TapId(1), r.gains.clone(), TapAlign::ToOutput, cap);
    let mut raw =
        r.ed.open_tap(TapId(2), vec![r.gains[0], r.gains[5]], TapAlign::Raw, cap);
    let mut single =
        r.ed.open_tap(TapId(3), vec![r.gains[2]], TapAlign::ToOutput, cap);
    r.ed.commit().expect("compiles");
    r.exec.apply_pending();
    r.ed.collect();
    assert_eq!(r.exec.is_parallel(), r.gated, "a pool was set iff gated");
    let mut outputs = vec![Vec::new(); CHAINS as usize];
    let (mut a, mut b, mut c) = (Vec::new(), Vec::new(), Vec::new());
    for i in 0..8 {
        let ch = block(r, 100, i as u64 * 100);
        for (o, x) in outputs.iter_mut().zip(ch) {
            o.extend(x);
        }
        drain(&mut aligned, &mut a);
        drain(&mut raw, &mut b);
        drain(&mut single, &mut c);
    }
    assert!(a.iter().any(|&x| x != 0.0), "the tap carries signal");
    (outputs, a, b, c, 0)
}

/// A slow reader loses frames and is told how many; a tap that is closed is
/// not in the plan at all and the audio thread does no work for it.
///
/// Mutation (run): drop the `fetch_add` in `tap_op` -> `take_lost` reads 0.
#[test]
fn a_full_ring_counts_what_it_lost_and_a_closed_tap_leaves_no_op() {
    let mut r = rig(1, false);
    let tap =
        r.ed.open_tap(TapId(1), vec![r.gains[0]], TapAlign::Raw, Samples(150));
    r.ed.commit().expect("compiles");
    r.exec.apply_pending();
    r.ed.collect();
    for i in 0..3 {
        block(&mut r, 100, i * 100);
    }
    // 300 frames pushed into a 150-frame ring that nobody drained.
    assert_eq!(tap.take_lost().0, 150);
    assert_eq!(tap.available().0, 150);
    assert_eq!(tap.take_lost().0, 0, "reading clears");
    r.ed.close_tap(TapId(1));
    r.ed.commit().expect("compiles");
    r.exec.apply_pending();
    r.ed.collect();
    block(&mut r, 100, 300);
    assert_eq!(
        tap.available().0,
        150,
        "nothing more arrived after the close"
    );
    assert_eq!(tap.take_lost().0, 0);
}

/// An export forks the graph: the fork carries no tap, so the live consumer
/// never sees offline audio; the export opens its own tap on the fork, and
/// because its loop drains between blocks (no deadline) nothing is lost.
///
/// The fork carries no tap by construction: `fork_gather` copies the live
/// topology's inputs, outputs and edges piecewise into the fork's editor, never
/// its taps, so the one-line `taps: BTreeMap::new()` in the fork's spec literal
/// is only what the struct needs (a mutation copying the live taps there left
/// this test passing, which is the point: nothing downstream reads them).
#[test]
fn a_fork_carries_no_tap_and_an_export_reads_its_own_losslessly() {
    let mut r = rig(1, false);
    let live =
        r.ed.open_tap(TapId(1), r.gains.clone(), TapAlign::ToOutput, Samples(1024));
    r.ed.commit().expect("compiles");
    r.exec.apply_pending();
    r.ed.collect();

    let (mut fed, fexec) =
        r.ed.fork(ForkTarget::Master, ForkMode::Offline, prepare(MAX_BLOCK))
            .expect("forks");
    assert!(
        fed.spec().topology.taps.is_empty(),
        "fork_has_no_tap: the live tap is not in the fork"
    );
    let gains = r.gains.clone();
    let mut mine = fed.open_tap(TapId(1), gains, TapAlign::ToOutput, Samples(MAX_BLOCK));
    fed.commit().expect("compiles");
    let mut f = Rig {
        ed: fed,
        exec: fexec,
        gains: Vec::new(),
        gated: false,
    };
    f.exec.apply_pending();
    f.ed.collect();
    let mut got = Vec::new();
    for i in 0..20 {
        // The export's loop: render a block, drain it, render the next.
        block(&mut f, MAX_BLOCK, i * MAX_BLOCK as u64);
        drain(&mut mine, &mut got);
    }
    assert_eq!(
        mine.take_lost().0,
        0,
        "an offline loop that drains per block loses nothing"
    );
    assert_eq!(got.len(), 20 * MAX_BLOCK * CHAINS as usize);
    assert_eq!(live.available().0, 0, "the live tap saw none of the export");
}

/// Cost, not correctness: nanoseconds per block with no tap, with a two-lane
/// open tap drained every block, and the same block through today's
/// `AudioTap` is measured in `tutti-metering`. Run in release:
/// `cargo test --release -p tutti-graph --test tap -- --ignored --nocapture`.
#[test]
#[ignore = "a measurement"]
fn tap_cost_per_block() {
    fn time(with_tap: Option<TapAlign>, frames: usize) -> f64 {
        let (mut ed, mut exec) = Editor::new(prepare(frames));
        let gains = build(&mut ed);
        let mut tap = with_tap
            .map(|align| ed.open_tap(TapId(1), vec![gains[0], gains[1]], align, Samples(8192)));
        ed.commit().expect("compiles");
        exec.apply_pending();
        ed.collect();
        let input = vec![0.1f32; frames];
        let mut chans = vec![vec![0.0f32; frames]; CHAINS as usize];
        let mut sink = vec![0.0f32; 2 * frames];
        let t = Transport::new(true, Bpm(120.0), Beat(0.0), None);
        let mut best = f64::MAX;
        for _ in 0..7 {
            let start = std::time::Instant::now();
            for _ in 0..20_000 {
                let mut outs: Vec<&mut [f32]> = chans.iter_mut().map(Vec::as_mut_slice).collect();
                exec.process(frames, &t, &[&input], &mut outs);
                if let Some(tap) = tap.as_mut() {
                    let (a, b) = sink.split_at_mut(frames);
                    tap.pop_lane(0, a);
                    tap.pop_lane(1, b);
                }
            }
            best = best.min(start.elapsed().as_nanos() as f64 / 20_000.0);
        }
        best
    }
    for frames in [64, 256] {
        let off = time(None, frames);
        for align in [TapAlign::Raw, TapAlign::ToOutput] {
            let on = time(Some(align), frames);
            println!(
                "{frames} frames: no tap {off:.0} ns, two-lane {align:?} tap + drain {on:.0} ns, +{:.0} ns",
                on - off
            );
        }
    }
}
