//! Taps as plan ops: any port, any width, aligned by the compiler or raw,
//! lossless below the ring's capacity and counted above it, bit-identical on
//! the parallel executor, and allocation-free on every participant. A closed
//! tap compiles to no op, and a render is the same with a tap open or closed.

use tutti_graph::testing as common;

use assert_no_alloc::AllocDisabler;
use common::{Kind, TestNode, prepare};
use tutti_graph::{Edge, InPort, OutPort, Source};
use tutti_graph::{
    Editor, Executor, ForkCause, ForkMode, ForkSource, ForkTarget, Forked, IntoNode, MissingSource,
    Node, NodeParts, Op, TapReader, Transport,
};
use tutti_graph::{MeterId, MeterSpec, NodeKey, TapAlign, TapId, TapSpec};
use tutti_types::{Beat, Bpm, ChannelLayout, Samples};

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
            vec![Edge::Direct(Source::Global(0))],
        );
        t.edges.insert(
            InPort {
                node: NodeKey(gain),
                port: 0,
            },
            vec![Edge::Direct(out(lag))],
        );
        gains.push(out(gain));
    }
    ed.spec_mut().topology.set_outputs(gains.clone());
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

/// The graph input at frame `t`.
fn input_at(t: u64) -> f32 {
    ((t * 7919) % 20_011) as f32 / 20_011.0 - 0.5
}

/// What chain `k` plays at frame `t` once aligned: the input, `LATENCY` frames
/// late (every chain is compensated up to the slowest), through its gain.
const LATENCY: u64 = 5 * (CHAINS - 1);

fn gain_of(k: u64) -> f32 {
    0.5 + k as f32 / 10.0
}

/// Chain `k` as it leaves its node, `delay` frames behind the input.
fn chain_at(k: u64, delay: u64, t: u64) -> f32 {
    t.checked_sub(delay)
        .map_or(0.0, |u| input_at(u) * gain_of(k))
}

/// The aligned signal of chain `k`: what its output channel carries.
fn aligned_at(k: u64, t: u64) -> f32 {
    chain_at(k, LATENCY, t)
}

/// One block; returns the output channels.
fn block(r: &mut Rig, n: usize, frame: u64) -> Vec<Vec<f32>> {
    let input: Vec<f32> = (0..n).map(|k| input_at(frame + k as u64)).collect();
    let mut chans = vec![vec![0.0f32; n]; CHAINS as usize];
    let mut outs: Vec<&mut [f32]> = chans.iter_mut().map(Vec::as_mut_slice).collect();
    let t = Transport::new(true, Bpm::of(120.0), Beat(0.0), None);
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
        let n = tap.pop_into(&mut buf).get();
        if n == 0 {
            break;
        }
        into.extend_from_slice(&buf[..n * tap.width()]);
    }
}

fn commit(r: &mut Rig) {
    r.ed.commit().expect("compiles");
    r.exec.apply_pending();
    r.ed.collect();
}

/// What a run read: the output channels and each tap, interleaved.
#[derive(Debug, PartialEq)]
struct Read {
    outputs: Vec<Vec<f32>>,
    /// The six outputs' sources, aligned.
    aligned: Vec<f32>,
    /// Chains 0 and 5, raw.
    raw: Vec<f32>,
    /// Chain 2, aligned.
    single: Vec<f32>,
    /// `Global(0)` and a `Zero` lane, aligned.
    edges: Vec<f32>,
    lost: usize,
}

const BLOCK: usize = 100;

/// Renders `blocks` blocks with an aligned six-lane tap on the outputs'
/// sources, a raw two-lane tap on chains 0 and 5, a one-lane aligned tap on
/// chain 2 and an aligned tap on the graph input and on silence.
fn run(r: &mut Rig, blocks: usize) -> Read {
    let cap = Samples(1 << 16);
    let mut aligned =
        r.ed.open_tap(TapId(1), r.gains.clone(), TapAlign::ToOutput, cap)
            .expect("the sources exist");
    let mut raw =
        r.ed.open_tap(TapId(2), vec![r.gains[0], r.gains[5]], TapAlign::Raw, cap)
            .expect("the sources exist");
    let mut single =
        r.ed.open_tap(TapId(3), vec![r.gains[2]], TapAlign::ToOutput, cap)
            .expect("the sources exist");
    let mut edges =
        r.ed.open_tap(
            TapId(4),
            vec![Source::Global(0), Source::Zero],
            TapAlign::ToOutput,
            cap,
        )
        .expect("the sources exist");
    commit(r);
    assert_eq!(r.exec.is_parallel(), r.gated, "a pool was set iff gated");
    let mut read = Read {
        outputs: vec![Vec::new(); CHAINS as usize],
        aligned: Vec::new(),
        raw: Vec::new(),
        single: Vec::new(),
        edges: Vec::new(),
        lost: 0,
    };
    for i in 0..blocks {
        let chans = block(r, BLOCK, (i * BLOCK) as u64);
        for (o, x) in read.outputs.iter_mut().zip(chans) {
            o.extend(x);
        }
        drain(&mut aligned, &mut read.aligned);
        drain(&mut raw, &mut read.raw);
        drain(&mut single, &mut read.single);
        drain(&mut edges, &mut read.edges);
        read.lost += [&aligned, &raw, &single, &edges]
            .iter()
            .map(|t| t.take_lost().get())
            .sum::<usize>();
        r.ed.collect();
    }
    read
}

fn lane(interleaved: &[f32], width: usize, c: usize) -> Vec<f32> {
    interleaved.iter().skip(c).step_by(width).copied().collect()
}

fn frames(blocks: usize) -> impl Iterator<Item = u64> {
    0..(blocks * BLOCK) as u64
}

/// Every tap equals the analytic signal of what it names, at any width: an
/// aligned tap of the outputs' sources is each output channel (the compiler
/// gave it the compensation an output gets), a raw one is the source as it
/// leaves its node, a global input and silence work as sources, and the
/// outputs themselves are untouched by all four taps being open.
#[test]
fn a_tap_is_aligned_by_the_compiler_and_any_width() {
    let mut r = rig(1, false);
    let read = run(&mut r, 8);
    assert_eq!(read.lost, 0);
    for k in 0..CHAINS {
        let want: Vec<f32> = frames(8).map(|t| aligned_at(k, t)).collect();
        assert_eq!(read.outputs[k as usize], want, "output {k}");
        assert_eq!(lane(&read.aligned, 6, k as usize), want, "aligned lane {k}");
    }
    let want: Vec<f32> = frames(8).map(|t| aligned_at(2, t)).collect();
    assert_eq!(lane(&read.single, 1, 0), want);
    // Chain 0 has no latency of its own: raw, it is ahead of its output by
    // the whole compensation. Chain 5 is the slowest: raw is its output.
    let raw0: Vec<f32> = frames(8).map(|t| chain_at(0, 0, t)).collect();
    let raw5: Vec<f32> = frames(8).map(|t| chain_at(5, LATENCY, t)).collect();
    assert_eq!(lane(&read.raw, 2, 0), raw0);
    assert_eq!(lane(&read.raw, 2, 1), raw5);
    // The global input is a source like any other: aligned, it is delayed by
    // the whole compensation, as an output wired straight to it would be. A
    // `Zero` lane is silence.
    let input: Vec<f32> = frames(8)
        .map(|t| t.checked_sub(LATENCY).map_or(0.0, input_at))
        .collect();
    assert_eq!(lane(&read.edges, 2, 0), input);
    assert!(lane(&read.edges, 2, 1).iter().all(|&x| x == 0.0));
}

/// The parallel render, outputs and every tap, is bit-identical to the serial
/// one at several worker counts, with every participant inside
/// `assert_no_alloc`, and the serial render itself is the analytic one.
#[test]
fn the_parallel_taps_are_bit_identical_and_allocation_free() {
    let mut s = rig(1, false);
    let serial = run(&mut s, 8);
    assert!(
        serial.aligned.iter().any(|&x| x != 0.0),
        "the tap carries signal"
    );
    for workers in [2, 3, 4] {
        let mut p = rig(workers, true);
        let par = run(&mut p, 8);
        assert_eq!(serial, par, "{workers} workers");
    }
}

/// The plan's tap ops: one per lane while open, none once closed, and a closed
/// plan has the ops of a graph that never had a tap.
#[test]
fn a_closed_tap_leaves_no_op() {
    let ops = |r: &Rig| {
        let plan = r.ed.base().expect("a plan was sent");
        let taps = plan
            .ops()
            .iter()
            .filter(|o| matches!(o, Op::Tap { .. }))
            .count();
        (plan.ops().len(), taps)
    };
    let mut r = rig(1, false);
    commit(&mut r);
    let (never, none) = ops(&r);
    assert_eq!(none, 0);

    let tap =
        r.ed.open_tap(
            TapId(1),
            vec![r.gains[0], r.gains[5]],
            TapAlign::Raw,
            Samples(64),
        )
        .expect("the sources exist");
    commit(&mut r);
    assert_eq!(
        ops(&r),
        (never + 1, 1),
        "a raw tap is one op, whatever its width"
    );

    r.ed.close_tap(tap.id());
    commit(&mut r);
    assert_eq!(
        ops(&r),
        (never, 0),
        "closed: the plan of a graph with no tap"
    );
    assert!(r.ed.base().expect("plan").ops().len() == never);

    // An aligned tap of a delayed source also carries its delay.
    let _aligned =
        r.ed.open_tap(TapId(2), vec![r.gains[0]], TapAlign::ToOutput, Samples(64))
            .expect("the sources exist");
    commit(&mut r);
    assert_eq!(
        ops(&r),
        (never + 2, 1),
        "a tap op and the delay that aligns it"
    );
}

/// A slow reader loses frames and is told how many, once for the tap whatever
/// its width; a closed tap receives nothing more.
#[test]
fn a_full_ring_counts_what_it_lost_and_a_closed_tap_receives_nothing() {
    let mut r = rig(1, false);
    let tap =
        r.ed.open_tap(TapId(1), r.gains.clone(), TapAlign::Raw, Samples(150))
            .expect("the sources exist");
    commit(&mut r);
    for i in 0..3 {
        block(&mut r, 100, i * 100);
    }
    // 300 frames pushed into a 150-frame ring that nobody drained.
    assert_eq!(tap.take_lost().get(), 150);
    assert_eq!(tap.available().get(), 150);
    assert_eq!(tap.take_lost().get(), 0, "reading clears");
    r.ed.close_tap(tap.id());
    commit(&mut r);
    block(&mut r, 100, 300);
    assert_eq!(
        tap.available().get(),
        150,
        "nothing more arrived after the close"
    );
    assert_eq!(tap.take_lost().get(), 0);
}

/// A read is whole frames: a buffer that ends mid-frame leaves its tail as it
/// was, and the reader says how wide it is.
#[test]
fn a_read_is_whole_frames() {
    let mut r = rig(1, false);
    let mut tap =
        r.ed.open_tap(TapId(1), r.gains[..3].to_vec(), TapAlign::Raw, Samples(256))
            .expect("the sources exist");
    commit(&mut r);
    block(&mut r, 100, 0);
    assert_eq!(tap.width(), 3);
    assert_eq!(tap.layout(), ChannelLayout::from_count(3));
    assert_eq!(tap.available().get(), 100);
    let mut out = [f32::MAX; 8];
    assert_eq!(tap.pop_into(&mut out).get(), 2, "two whole frames of three");
    assert_eq!(out[6..], [f32::MAX; 2], "the partial tail is untouched");
    assert_eq!(tap.available().get(), 98);
}

/// A reader dropped first costs nothing and frees nothing on the audio thread:
/// the tap stays in the plan writing nowhere, the editor reaps it, and the
/// commit that removes it retires the ring with the old state. The ring and
/// its producer are freed at the editor's collect, never in a block (a lane's
/// drop is a debug panic inside one).
#[test]
fn a_reader_dropped_first_is_reaped_and_freed_on_the_control_side() {
    let mut r = rig(1, true);
    let tap =
        r.ed.open_tap(TapId(1), r.gains.clone(), TapAlign::ToOutput, Samples(64))
            .expect("the sources exist");
    commit(&mut r);
    drop(tap);
    // Rendering with nobody reading: no write, no panic, no allocation.
    for i in 0..3 {
        block(&mut r, 100, i * 100);
    }
    assert_eq!(r.ed.reap_taps(), 1);
    assert_eq!(r.ed.reap_taps(), 0, "reaped once");
    r.ed.commit().expect("compiles");
    r.exec.apply_pending();
    assert_eq!(
        r.ed.base()
            .expect("plan")
            .ops()
            .iter()
            .filter(|o| matches!(o, Op::Tap { .. }))
            .count(),
        0
    );
    r.ed.collect();
    block(&mut r, 100, 300);
}

/// The reader is control-side state: dropping it inside a block is a debug
/// panic, as for a unit.
#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "a tap reader dropped on the audio thread")]
fn a_reader_dropped_on_the_audio_thread_panics() {
    let mut r = rig(1, false);
    let tap =
        r.ed.open_tap(TapId(1), vec![r.gains[0]], TapAlign::Raw, Samples(8))
            .expect("the sources exist");
    let _inside = tutti_rt::RtScope::enter();
    drop(tap);
}

/// An export forks the graph: the fork carries no tap, so the live consumer
/// never sees offline audio; the export opens its own tap on the fork and,
/// because its loop drains between blocks (no deadline), loses nothing.
///
/// The fork carries no tap by construction: `fork_gather` copies the live
/// topology's inputs, outputs and edges piecewise into the fork's editor, never
/// its taps.
#[test]
fn a_fork_carries_no_tap_and_an_export_reads_its_own_losslessly() {
    let mut r = rig(1, false);
    let live =
        r.ed.open_tap(TapId(1), r.gains.clone(), TapAlign::ToOutput, Samples(1024))
            .expect("the sources exist");
    commit(&mut r);

    let (mut fed, fexec) =
        r.ed.fork(ForkTarget::Master, ForkMode::Offline, prepare(MAX_BLOCK))
            .expect("forks");
    assert!(
        fed.spec().topology.taps.is_empty(),
        "the live tap is not in the fork"
    );
    let mut mine = fed
        .open_tap(
            TapId(1),
            r.gains.clone(),
            TapAlign::ToOutput,
            Samples(MAX_BLOCK),
        )
        .expect("the sources exist");
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
        mine.take_lost().get(),
        0,
        "a loop that drains per block loses nothing"
    );
    assert_eq!(got.len(), 20 * MAX_BLOCK * CHAINS as usize);
    assert_eq!(
        live.available().get(),
        0,
        "the live tap saw none of the export"
    );
}

/// Removing a node a tap reads leaves the tap on silence in that lane, as an
/// output wired to it would be, and the graph still commits.
///
/// Two layers hold this: `Editor::remove` turns the lane to `Zero`, and the
/// compiler reads a missing node as silence.
#[test]
fn removing_a_tapped_node_silences_its_lane() {
    let mut r = rig(1, false);
    let mut tap =
        r.ed.open_tap(
            TapId(1),
            vec![r.gains[0], r.gains[1]],
            TapAlign::Raw,
            Samples(1024),
        )
        .expect("the sources exist");
    commit(&mut r);
    r.ed.remove(NodeKey(21));
    commit(&mut r);
    block(&mut r, 100, 0);
    let mut got = Vec::new();
    drain(&mut tap, &mut got);
    assert_eq!(got.len(), 200);
    let first: Vec<f32> = (0..100).map(|t| chain_at(0, 0, t)).collect();
    assert_eq!(lane(&got, 2, 0), first, "the other lane plays on");
    assert!(
        lane(&got, 2, 1).iter().all(|&x| x == 0.0),
        "the removed node's lane is silent"
    );
}

/// Closing a tap twice, or one that never opened, is a no-op: the plan stays
/// the graph's own.
#[test]
fn closing_a_tap_twice_is_a_no_op() {
    let mut r = rig(1, false);
    let tap =
        r.ed.open_tap(TapId(1), vec![r.gains[0]], TapAlign::Raw, Samples(8))
            .expect("the sources exist");
    commit(&mut r);
    r.ed.close_tap(tap.id());
    r.ed.close_tap(tap.id());
    r.ed.close_tap(TapId(99));
    commit(&mut r);
    let plan = r.ed.base().expect("plan");
    assert!(!plan.ops().iter().any(|o| matches!(o, Op::Tap { .. })));
}

/// Sets its flag when dropped, so a panic while rendering still lets the reader
/// thread finish (instead of hanging the scope).
struct Raise<'a>(&'a std::sync::atomic::AtomicBool);

impl Drop for Raise<'_> {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::Release);
    }
}

/// The lanes of a tap cannot tear, whatever the reader does. A reader thread
/// drains in small pops with pauses against a ring that overflows (256 frames,
/// 100-frame blocks), so frames are dropped; every frame that arrives must have
/// all its lanes from the same instant. The lanes read independent chains (so
/// they are produced by different tasks of a pool render), and each frame's
/// instant is recovered from lane 0's value (the input is injective over the
/// run) and the other lanes checked against the analytic chains at that
/// instant. Accounting is exact: frames read plus frames counted lost is every
/// frame rendered.
///
/// This is the review's repro: before the tap was one op that writes its lanes
/// by one count, lane 1 accepted frames lane 0 refused and the lanes stayed
/// offset for good (hundreds of torn frames in a run, lane 0 alone counting).
fn concurrent_drain(workers: usize, width: usize, align: TapAlign) {
    const BLOCKS: usize = 130;
    let mut r = rig(workers, workers > 1);
    let sources: Vec<Source> = (0..width).map(|k| r.gains[k * 5 / (width - 1)]).collect();
    let chains: Vec<u64> = (0..width as u64)
        .map(|k| k * 5 / (width as u64 - 1))
        .collect();
    let tap =
        r.ed.open_tap(TapId(1), sources, align, Samples(256))
            .expect("the sources exist");
    commit(&mut r);
    let delay = |k: u64| match align {
        TapAlign::Raw => 5 * k,
        TapAlign::ToOutput => LATENCY,
    };
    let done = std::sync::atomic::AtomicBool::new(false);
    let (frames, lost) = std::thread::scope(|scope| {
        let reader = scope.spawn(|| {
            let mut tap = tap;
            let mut got = Vec::new();
            let mut buf = vec![0.0f32; width * 5];
            loop {
                let n = tap.pop_into(&mut buf).get();
                got.extend_from_slice(&buf[..n * width]);
                if n == 0 {
                    if done.load(std::sync::atomic::Ordering::Acquire) && tap.available().get() == 0
                    {
                        break;
                    }
                } else {
                    std::thread::sleep(std::time::Duration::from_micros(100));
                }
            }
            (got, tap.take_lost().get())
        });
        let finished = Raise(&done);
        for i in 0..BLOCKS {
            block(&mut r, BLOCK, (i * BLOCK) as u64);
        }
        drop(finished);
        reader.join().expect("the reader finished")
    });
    assert!(lost > 0, "the ring overflowed, so the test saw drops");
    let read = frames.len() / width;
    assert_eq!(
        read + lost,
        BLOCKS * BLOCK,
        "every frame is read or counted lost"
    );
    // Instants by lane 0's value (injective over the run, but for silence).
    let at: std::collections::HashMap<u32, u64> = (0..(BLOCKS * BLOCK) as u64)
        .map(|t| (chain_at(chains[0], delay(chains[0]), t).to_bits(), t))
        .collect();
    let mut torn = 0;
    for frame in frames.chunks_exact(width) {
        let Some(&t) = at.get(&frame[0].to_bits()) else {
            continue;
        };
        if frame[0] == 0.0 {
            continue;
        }
        for (k, &x) in frame.iter().enumerate() {
            if x.to_bits() != chain_at(chains[k], delay(chains[k]), t).to_bits() {
                torn += 1;
                break;
            }
        }
    }
    assert_eq!(torn, 0, "torn frames of {read}");
}

#[test]
fn lanes_do_not_tear_under_a_concurrent_drain() {
    for width in [2, 6] {
        for align in [TapAlign::Raw, TapAlign::ToOutput] {
            concurrent_drain(1, width, align);
        }
    }
}

/// The same, on the pool: the chains run in different tasks and the tap op
/// joins them, and every participant is inside `assert_no_alloc`.
#[test]
fn lanes_do_not_tear_on_the_pool() {
    for workers in [2, 4] {
        for width in [2, 6] {
            for align in [TapAlign::Raw, TapAlign::ToOutput] {
                concurrent_drain(workers, width, align);
            }
        }
    }
}

/// Opening an id again replaces the tap: the new reader is fed from the next
/// commit, the old one gets nothing more, and the old writer is retired on the
/// control side (a free in a block is a debug panic).
#[test]
fn opening_an_id_again_replaces_the_tap() {
    let mut r = rig(1, false);
    let first =
        r.ed.open_tap(TapId(1), vec![r.gains[0]], TapAlign::Raw, Samples(1024))
            .expect("the sources exist");
    commit(&mut r);
    block(&mut r, 100, 0);
    let second =
        r.ed.open_tap(TapId(1), vec![r.gains[0]], TapAlign::Raw, Samples(1024))
            .expect("the sources exist");
    commit(&mut r);
    block(&mut r, 100, 100);
    assert_eq!(
        first.available().get(),
        100,
        "the old reader got only the first block"
    );
    assert_eq!(
        second.available().get(),
        100,
        "the new reader got the second"
    );
}

/// A tap's lanes are fixed by how it was opened: editing `TapSpec::Ports`' sources to
/// another length through the spec is refused at the commit (the writer the
/// executor holds has that many rings), and opening the id again at another
/// width replaces the tap at the new one, lanes and all.
#[test]
fn a_tap_cannot_change_its_width_under_its_writer() {
    let mut r = rig(1, false);
    let _wide =
        r.ed.open_tap(TapId(1), r.gains[..3].to_vec(), TapAlign::Raw, Samples(64))
            .expect("the sources exist");
    commit(&mut r);
    let Some(TapSpec::Ports { sources, .. }) = r.ed.spec_mut().topology.taps.get_mut(&TapId(1))
    else {
        panic!("a ports tap")
    };
    sources.push(Source::Zero);
    assert_eq!(
        r.ed.commit(),
        Err(tutti_graph::CommitError::TapWidth {
            tap: TapId(1),
            sources: 4,
            opened: 3
        })
    );
    // Reopening at width 1 replaces it, and the lanes are the new tap's.
    let mut narrow =
        r.ed.open_tap(TapId(1), vec![r.gains[2]], TapAlign::Raw, Samples(256))
            .expect("the sources exist");
    commit(&mut r);
    block(&mut r, 100, 0);
    assert_eq!(narrow.width(), 1);
    let mut got = vec![f32::NAN; 100];
    assert_eq!(narrow.pop_into(&mut got).get(), 100);
    let want: Vec<f32> = (0..100).map(|t| chain_at(2, 10, t)).collect();
    assert_eq!(got, want);
}

/// `pop_planes` is `pop_into` without the interleave: the frames written
/// together come out of each lane in order, up to what was asked and what is
/// there, and a lane with no plane still drops the frames so the lanes stay
/// level.
#[test]
fn pop_planes_reads_whole_frames_and_keeps_the_lanes_level() {
    let (mut reader, mut writer) = TapReader::pair(TapId(7), 3, Samples(8));
    writer.push_interleaved(&[1.0, 10.0, 100.0, 2.0, 20.0, 200.0, 3.0, 30.0, 300.0]);
    let (mut a, mut b) = ([9.0f32; 4], [9.0f32; 4]);
    // Two planes for three lanes, two frames wanted of three there.
    let n = reader.pop_planes([&mut a[..], &mut b[..]], Samples(2));
    assert_eq!(n.get(), 2);
    assert_eq!((a, b), ([1.0, 2.0, 9.0, 9.0], [10.0, 20.0, 9.0, 9.0]));
    // The third lane dropped its two frames with the others: the next frame
    // is the third of every lane.
    let (mut a, mut b, mut c) = ([0.0f32; 1], [0.0f32; 1], [0.0f32; 1]);
    let n = reader.pop_planes([&mut a[..], &mut b[..], &mut c[..]], Samples(8));
    assert_eq!(n.get(), 1);
    assert_eq!((a[0], b[0], c[0]), (3.0, 30.0, 300.0));
}

/// A wide `Const` node (key 1, two outputs of `value`) outside the outputs.
fn wide(r: &mut Rig, value: f32, width: usize) {
    r.ed.insert(NodeKey(1), "const", Forkable(Kind::Const { value, width }));
}

fn port(node: u64, port: u16) -> Source {
    Source::Node(OutPort {
        node: NodeKey(node),
        port,
    })
}

/// A node replaced by one with fewer outputs leaves the tap's lanes past the
/// new width on silence, and the commit succeeds: a recording or a display
/// must not wedge the graph. The silence is zeros in the stream, not a loss
/// (`take_lost` stays 0) and not a gap (the stream keeps its length), and the
/// other lanes play on, on the serial and the pooled executor alike.
#[test]
fn a_narrowed_node_silences_the_tap_lanes_past_its_width_and_commits() {
    for (workers, gated) in [(1, false), (3, true)] {
        let mut r = rig(workers, gated);
        wide(&mut r, 0.5, 2);
        let mut tap =
            r.ed.open_tap(
                TapId(1),
                vec![port(1, 0), port(1, 1)],
                TapAlign::Raw,
                Samples(4096),
            )
            .expect("the sources exist");
        commit(&mut r);
        block(&mut r, 100, 0);
        wide(&mut r, 0.25, 1);
        commit(&mut r);
        block(&mut r, 100, 100);
        let mut got = Vec::new();
        drain(&mut tap, &mut got);
        assert_eq!(got.len(), 400, "the stream keeps its length");
        assert!(lane(&got, 2, 0)[..100].iter().all(|&x| x == 0.5));
        assert!(lane(&got, 2, 1)[..100].iter().all(|&x| x == 0.5));
        assert!(lane(&got, 2, 0)[100..].iter().all(|&x| x == 0.25));
        assert!(
            lane(&got, 2, 1)[100..].iter().all(|&x| x == 0.0),
            "the vanished port reads silence"
        );
        assert_eq!(tap.take_lost().get(), 0, "silence is not a loss");
    }
}

/// A global input that goes past the declared width after the open is the same
/// as a narrowed node: silence, not a refused commit.
#[test]
fn a_tap_on_a_global_input_that_goes_missing_reads_silence() {
    let mut r = rig(1, false);
    r.ed.spec_mut().topology.inputs = ChannelLayout::STEREO;
    let mut tap =
        r.ed.open_tap(
            TapId(1),
            vec![Source::Global(0), Source::Global(1)],
            TapAlign::Raw,
            Samples(1024),
        )
        .expect("both inputs exist when it opens");
    // The graph goes back to one input before anything is committed.
    r.ed.spec_mut().topology.inputs = ChannelLayout::MONO;
    commit(&mut r);
    block(&mut r, 100, 0);
    let mut got = Vec::new();
    drain(&mut tap, &mut got);
    let want: Vec<f32> = (0..100).map(input_at).collect();
    assert_eq!(lane(&got, 2, 0), want);
    assert!(lane(&got, 2, 1).iter().all(|&x| x == 0.0));
}

/// Opening refuses a source that does not exist at that moment, uncommitted
/// edits included, and opens nothing: a bad port, a key no node has and a
/// global input past the graph's width, for a tap and for a meter, each named
/// in the error. A lane that is fine does not hide a bad one after it, and a
/// source inserted but not yet committed is accepted.
#[test]
fn opening_refuses_a_source_that_does_not_exist_now() {
    let mut r = rig(1, false);
    wide(&mut r, 0.5, 2);
    let cap = Samples(1024);
    let tap =
        |r: &mut Rig, sources: Vec<Source>| r.ed.open_tap(TapId(9), sources, TapAlign::Raw, cap);
    assert_eq!(
        tap(&mut r, vec![port(1, 0), port(1, 5)]).err(),
        Some(MissingSource::Port {
            node: NodeKey(1),
            port: 5,
            width: 2
        }),
        "a port past the node's width"
    );
    assert_eq!(
        tap(&mut r, vec![port(77, 0)]).err(),
        Some(MissingSource::Node { node: NodeKey(77) }),
        "a typo'd key"
    );
    assert_eq!(
        tap(&mut r, vec![Source::Global(3)]).err(),
        Some(MissingSource::Global {
            channel: 3,
            width: 1
        }),
        "a global input past the graph's width"
    );
    let meter = |r: &mut Rig, sources: Vec<Source>| {
        r.ed.open_meter(MeterId(9), MeterSpec::raw(sources)).err()
    };
    assert_eq!(
        meter(&mut r, vec![port(1, 2)]),
        Some(MissingSource::Port {
            node: NodeKey(1),
            port: 2,
            width: 2
        }),
    );
    assert_eq!(
        meter(&mut r, vec![port(77, 0)]),
        Some(MissingSource::Node { node: NodeKey(77) }),
        "a meter on a missing node"
    );
    assert_eq!(
        meter(&mut r, vec![Source::Global(1)]),
        Some(MissingSource::Global {
            channel: 1,
            width: 1
        }),
    );
    assert!(
        r.ed.spec().topology.taps.is_empty() && r.ed.spec().topology.meters.is_empty(),
        "a refused open declares nothing"
    );
    // Nothing was committed: node 1 and the input exist only in the edited
    // spec, and that is what is checked.
    tap(&mut r, vec![port(1, 1), Source::Global(0), Source::Zero]).expect("all exist");
    assert_eq!(
        meter(&mut r, vec![port(1, 1)]),
        None,
        "it exists, uncommitted"
    );
}

/// A key is the author's and can be inserted again after a removal: a tap left
/// naming the removed node must not start reading the new one.
#[test]
fn a_reused_key_is_not_read_by_the_old_tap() {
    let mut r = rig(1, false);
    wide(&mut r, 0.25, 1);
    let mut tap =
        r.ed.open_tap(TapId(1), vec![port(1, 0)], TapAlign::Raw, Samples(1024))
            .expect("the sources exist");
    commit(&mut r);
    block(&mut r, 100, 0);
    r.ed.remove(NodeKey(1));
    wide(&mut r, 0.125, 1);
    commit(&mut r);
    block(&mut r, 100, 100);
    let mut got = Vec::new();
    drain(&mut tap, &mut got);
    assert_eq!(got.len(), 200);
    assert!(got[..100].iter().all(|&x| x == 0.25));
    assert!(
        got[100..].iter().all(|&x| x == 0.0),
        "the tap followed the removal, not the key"
    );
}

/// An aligned tap across a narrowing: the lane that lost its port goes silent
/// at once (it has no delay ring to drain), and the other lanes, aligned by
/// rings of their own, neither shift nor leak: the surviving constant lane
/// still carries the old value for the `LATENCY` frames its ring held, and the
/// latent chain's lane is the analytic aligned signal throughout.
#[test]
fn an_aligned_tap_across_a_narrowing_keeps_the_other_lanes_level() {
    let mut r = rig(1, false);
    wide(&mut r, 0.5, 2);
    let mut tap =
        r.ed.open_tap(
            TapId(1),
            vec![port(1, 0), port(1, 1), r.gains[3]],
            TapAlign::ToOutput,
            Samples(4096),
        )
        .expect("the sources exist");
    commit(&mut r);
    for i in 0..3 {
        block(&mut r, 100, i * 100);
    }
    wide(&mut r, 0.25, 1);
    commit(&mut r);
    for i in 3..6 {
        block(&mut r, 100, i * 100);
    }
    let mut got = Vec::new();
    drain(&mut tap, &mut got);
    assert_eq!(got.len(), 600 * 3, "the stream keeps its length");
    let at = LATENCY as usize;
    // The constant is `LATENCY` frames late; the narrowing lands at frame 300.
    let lane0: Vec<f32> = (0..600usize)
        .map(|t| match t {
            t if t < at => 0.0,
            t if t - at < 300 => 0.5,
            _ => 0.25,
        })
        .collect();
    let lane1: Vec<f32> = (0..600usize)
        .map(|t| if t >= at && t < 300 { 0.5 } else { 0.0 })
        .collect();
    let lane2: Vec<f32> = (0..600u64).map(|t| aligned_at(3, t)).collect();
    assert_eq!(
        lane(&got, 3, 0),
        lane0,
        "the surviving constant keeps its ring"
    );
    assert_eq!(
        lane(&got, 3, 1),
        lane1,
        "the vanished lane is silent at once"
    );
    assert_eq!(lane(&got, 3, 2), lane2, "the latent lane is untouched");
    assert_eq!(tap.take_lost().get(), 0);
}
