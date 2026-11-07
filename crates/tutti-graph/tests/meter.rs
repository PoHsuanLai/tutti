//! Level meters as plan ops: per channel on any port or on the outputs,
//! exact on analytic signals, peak held until read, bit-identical on the
//! parallel executor and allocation-free on every participant. A closed meter
//! compiles to no op.
//!
//! The reference interpreter (`reference.rs`) renders a `Topology`'s outputs
//! and knows neither taps nor meters: a meter reads the signal and changes
//! nothing, so there is nothing for it to compare. The properties here are
//! analytic figures and the serial render against the parallel one.

mod common;

use assert_no_alloc::AllocDisabler;
use common::{Kind, TestNode, prepare};
use tutti_graph::{
    CommitError, Cx, Editor, Executor, ForkCause, ForkMode, ForkSource, ForkTarget, Forked,
    IntoNode, Io, Level, MeterReader, Node, NodeParts, Op, Shape, Status, Transport, Unforkable,
};
use tutti_types::graph::{Edge, InPort, OutPort, Source};
use tutti_types::{Beat, Bpm, ChannelLayout, MeterId, MeterSpec, NodeKey, Samples};

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

const MAX_BLOCK: usize = 1024;

fn out(node: u64) -> Source {
    Source::Node(OutPort {
        node: NodeKey(node),
        port: 0,
    })
}

struct Rig {
    ed: Editor,
    exec: Executor,
    gated: bool,
}

fn rig(workers: usize, no_alloc: bool) -> Rig {
    let (ed, mut exec) = Editor::new(prepare(MAX_BLOCK));
    if workers > 1 {
        exec.set_pool(Some(if no_alloc {
            common::TestPool::no_alloc(workers)
        } else {
            common::test_pool(workers)
        }));
    }
    Rig {
        ed,
        exec,
        gated: no_alloc,
    }
}

/// `values.len()` constants, nodes `1..`, each its own output channel.
fn constants(r: &mut Rig, values: &[f32]) {
    let mut outputs = Vec::new();
    for (k, &value) in values.iter().enumerate() {
        let key = 1 + k as u64;
        r.ed.insert(
            NodeKey(key),
            "const",
            Forkable(Kind::Const { value, width: 1 }),
        );
        outputs.push(out(key));
    }
    r.ed.spec_mut().topology.outputs = outputs;
}

fn commit(r: &mut Rig) {
    r.ed.commit().expect("compiles");
    r.exec.apply_pending();
    r.ed.collect();
}

/// One block of `n` frames; returns the output channels.
fn block(r: &mut Rig, n: usize) -> Vec<Vec<f32>> {
    let width = r.ed.spec().topology.outputs.len();
    let mut chans = vec![vec![0.0f32; n]; width];
    let mut outs: Vec<&mut [f32]> = chans.iter_mut().map(Vec::as_mut_slice).collect();
    let t = Transport::new(true, Bpm(120.0), Beat(0.0), None);
    let ins: [&[f32]; 0] = [];
    // Only the render is inside the gate: this helper's own buffers allocate.
    if r.gated {
        assert_no_alloc::assert_no_alloc(|| r.exec.process(n, &t, &ins, &mut outs));
    } else {
        r.exec.process(n, &t, &ins, &mut outs);
    }
    chans
}

fn meter_ops(r: &Rig) -> (usize, usize) {
    let plan = r.ed.base().expect("a plan was sent");
    (
        plan.ops().len(),
        plan.ops()
            .iter()
            .filter(|o| matches!(o, Op::Meter { .. }))
            .count(),
    )
}

fn level(m: &mut MeterReader) -> Vec<(f32, f32, usize)> {
    m.read()
        .iter()
        .map(|l| (l.peak.get(), l.rms.get(), l.frames.get()))
        .collect()
}

/// A constant reads its peak and RMS exactly, on the outputs and on a node
/// port, per channel: six outputs hold 2^-1 .. 2^-6 and each reads its own.
/// The master reads what the outputs carry, and the outputs are untouched.
///
/// Mutation (run, recorded here): see the list at the end of this file.
#[test]
fn a_constant_reads_exactly_per_channel() {
    let mut r = rig(1, false);
    let values: Vec<f32> = (1..=6).map(|k| 0.5f32.powi(k)).collect();
    constants(&mut r, &values);
    let mut master = r.ed.open_meter(MeterId(1), MeterSpec::Outputs);
    let mut track =
        r.ed.open_meter(MeterId(2), MeterSpec::Ports(vec![out(3), out(1)]));
    master.enable();
    track.enable();
    commit(&mut r);
    assert_eq!(master.channels(), 6);
    assert_eq!(track.channels(), 2);

    let outs = block(&mut r, 100);
    for (k, &v) in values.iter().enumerate() {
        assert!(outs[k].iter().all(|&s| s == v), "output {k} is untouched");
    }
    let want: Vec<(f32, f32, usize)> = values.iter().map(|&v| (v, v, 100)).collect();
    assert_eq!(level(&mut master), want);
    assert_eq!(
        level(&mut track),
        vec![(values[2], values[2], 100), (values[0], values[0], 100)],
        "lanes follow the sources' order"
    );
}

/// The meter op on the audio thread allocates nothing, with the meter enabled,
/// over many blocks, and the meter really read (a gate around a meter that
/// measured nothing would pass). Serial; the parallel gate is below.
///
/// Mutation (run, recorded here): `Vec::with_capacity(1)` in `meter_op` -> the
/// gate panics.
#[test]
fn measuring_is_allocation_free_with_the_meter_enabled() {
    let mut r = rig(1, true);
    constants(&mut r, &[0.5, 0.25]);
    let mut m = r.ed.open_meter(MeterId(1), MeterSpec::Outputs);
    m.enable();
    commit(&mut r);
    block(&mut r, 512);
    for _ in 0..1_000 {
        block(&mut r, 512);
    }
    assert!(m.read()[0].peak.get() > 0.0, "the meter published nothing");
}

/// A node that is silent except for one sample: 1.0 at frame `at` of its
/// first block.
struct Spike {
    at: usize,
    done: bool,
}

impl Node for Spike {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::MONO)
    }
    fn prepare(&mut self, _: &tutti_graph::Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        let o = io.output(0);
        o.fill(0.0);
        if !self.done {
            o[self.at] = 1.0;
            self.done = true;
        }
        Status::Modified
    }
    fn reset(&mut self) {}
}

/// One spike in a long block is held until it is read, whatever the blocks
/// after it, and read once; the RMS covers every frame since the last read.
///
/// Mutation (run, recorded here): see the list at the end of this file.
#[test]
fn a_spike_is_held_until_read_and_read_once() {
    let mut r = rig(1, false);
    r.ed.insert(
        NodeKey(1),
        "spike",
        Unforkable(Spike {
            at: 777,
            done: false,
        }),
    );
    r.ed.spec_mut().topology.outputs = vec![out(1)];
    let mut m = r.ed.open_meter(MeterId(1), MeterSpec::Outputs);
    m.enable();
    commit(&mut r);
    for _ in 0..4 {
        block(&mut r, 1000);
    }
    let l = m.read();
    assert_eq!(l.len(), 1);
    assert_eq!(
        l[0].peak.get(),
        1.0,
        "the spike survives three later blocks"
    );
    assert_eq!(l[0].frames, Samples(4000));
    assert_eq!(l[0].rms.get(), (1.0f64 / 4000.0).sqrt() as f32);
    assert_eq!(
        m.read(),
        vec![Level::default()],
        "taken: nothing since, and the frame count says so"
    );
    block(&mut r, 1000);
    let l = m.read();
    assert_eq!((l[0].peak.get(), l[0].frames), (0.0, Samples(1000)));
}

/// A meter measures only while enabled: a disabled one reads nothing of a
/// rendered block, and an enabled one reads the next. What was held stays
/// readable after a disable.
///
/// Mutation (run, recorded here): see the list at the end of this file.
#[test]
fn an_enabled_meter_reads_the_block() {
    let mut r = rig(1, false);
    constants(&mut r, &[0.5, 0.5]);
    let mut m = r.ed.open_meter(MeterId(1), MeterSpec::Outputs);
    commit(&mut r);
    block(&mut r, 64);
    assert_eq!(level(&mut m), vec![(0.0, 0.0, 0); 2], "off reads nothing");
    m.enable();
    block(&mut r, 64);
    assert_eq!(level(&mut m), vec![(0.5, 0.5, 64); 2]);
    block(&mut r, 64);
    m.disable();
    block(&mut r, 64);
    assert_eq!(
        level(&mut m),
        vec![(0.5, 0.5, 64); 2],
        "the block before the disable is held, the one after is not"
    );
}

/// The master meter follows the outputs: rewiring a channel to silence reads
/// zero, and a wider output set widens the meter.
///
/// Mutation (run, recorded here): see the list at the end of this file.
#[test]
fn the_master_follows_the_outputs() {
    let mut r = rig(1, false);
    constants(&mut r, &[0.5, 0.25]);
    let mut m = r.ed.open_meter(MeterId(1), MeterSpec::Outputs);
    m.enable();
    commit(&mut r);
    block(&mut r, 32);
    assert_eq!(level(&mut m), vec![(0.5, 0.5, 32), (0.25, 0.25, 32)]);

    r.ed.spec_mut().topology.outputs[0] = Source::Zero;
    r.ed.spec_mut().topology.outputs.push(out(1));
    commit(&mut r);
    assert_eq!(m.channels(), 3);
    block(&mut r, 32);
    assert_eq!(
        level(&mut m),
        vec![(0.0, 0.0, 32), (0.25, 0.25, 32), (0.5, 0.5, 32)]
    );
}

/// `read_into` fills what fits and reports it; a short buffer takes only the
/// lanes it writes, leaving the rest held.
///
/// Mutation (run, recorded here): see the list at the end of this file.
#[test]
fn read_into_takes_only_what_it_writes() {
    let mut r = rig(1, false);
    constants(&mut r, &[0.5, 0.25]);
    let mut m = r.ed.open_meter(MeterId(1), MeterSpec::Outputs);
    m.enable();
    commit(&mut r);
    block(&mut r, 16);
    let mut one = [Level::default()];
    assert_eq!(m.read_into(&mut one), 1);
    assert_eq!(one[0].peak.get(), 0.5);
    assert_eq!(
        level(&mut m),
        vec![(0.0, 0.0, 0), (0.25, 0.25, 16)],
        "lane 0 was taken, lane 1 still holds its block"
    );
}

/// The plan's meter ops: one per meter whatever its width, none once closed,
/// and a closed plan has the ops of a graph that never had a meter.
///
/// Mutation (run, recorded here): see the list at the end of this file.
#[test]
fn a_closed_meter_leaves_no_op() {
    let mut r = rig(1, false);
    constants(&mut r, &[0.5, 0.25, 0.125]);
    commit(&mut r);
    let (never, none) = meter_ops(&r);
    assert_eq!(none, 0);

    let a = r.ed.open_meter(MeterId(1), MeterSpec::Outputs);
    let _b =
        r.ed.open_meter(MeterId(2), MeterSpec::Ports(vec![out(1), out(2), out(3)]));
    commit(&mut r);
    assert_eq!(meter_ops(&r), (never + 2, 2), "one op a meter, any width");

    r.ed.close_meter(a.id());
    r.ed.close_meter(a.id());
    commit(&mut r);
    assert_eq!(meter_ops(&r), (never + 1, 1));
    r.ed.close_meter(MeterId(2));
    commit(&mut r);
    assert_eq!(meter_ops(&r), (never, 0));
    assert!(a.is_closed(), "the writer was freed with the old plan");
}

/// A dropped reader is reaped once, its op leaves the plan at the next
/// commit, and its cells are freed on the control side (a writer dropped in a
/// block is a debug panic). Rendering with nobody reading writes nothing
/// anyone sees and allocates nothing.
///
/// Mutation (run, recorded here): see the list at the end of this file.
#[test]
fn a_reader_dropped_is_reaped_and_its_cells_freed_off_the_audio_thread() {
    let mut r = rig(1, true);
    constants(&mut r, &[0.5]);
    let m = r.ed.open_meter(MeterId(1), MeterSpec::Outputs);
    m.enable();
    commit(&mut r);
    drop(m);
    for _ in 0..3 {
        block(&mut r, 64);
    }
    assert_eq!(r.ed.reap_meters(), 1);
    assert_eq!(r.ed.reap_meters(), 0, "reaped once");
    commit(&mut r);
    assert_eq!(meter_ops(&r).1, 0);
    block(&mut r, 64);
}

/// The reader is control-side state: dropping it inside a block is a debug
/// panic, as for a unit.
///
/// Mutation (run, recorded here): see the list at the end of this file.
#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "a meter reader dropped on the audio thread")]
fn a_reader_dropped_on_the_audio_thread_panics() {
    let mut r = rig(1, false);
    let m =
        r.ed.open_meter(MeterId(1), MeterSpec::Ports(vec![Source::Zero]));
    let _inside = tutti_rt::RtScope::enter();
    drop(m);
}

/// Removing a metered node leaves its lane on silence, the other lanes play
/// on, and the graph still commits.
///
/// Mutation (run, recorded here): see the list at the end of this file.
#[test]
fn removing_a_metered_node_silences_its_lane() {
    let mut r = rig(1, false);
    constants(&mut r, &[0.5, 0.25]);
    let mut m =
        r.ed.open_meter(MeterId(1), MeterSpec::Ports(vec![out(1), out(2)]));
    m.enable();
    commit(&mut r);
    block(&mut r, 32);
    assert_eq!(level(&mut m), vec![(0.5, 0.5, 32), (0.25, 0.25, 32)]);

    r.ed.remove(NodeKey(2));
    commit(&mut r);
    block(&mut r, 32);
    assert_eq!(level(&mut m), vec![(0.5, 0.5, 32), (0.0, 0.0, 32)]);
}

/// Closing, then declaring the same id again, makes a fresh meter: the new
/// reader is fed from the next commit, and the old one gets nothing more. An
/// id opened over a live one replaces it the same way.
///
/// Mutation (run, recorded here): see the list at the end of this file.
#[test]
fn a_meter_can_be_redeclared() {
    let mut r = rig(1, false);
    constants(&mut r, &[0.5]);
    let mut first = r.ed.open_meter(MeterId(1), MeterSpec::Outputs);
    first.enable();
    commit(&mut r);
    block(&mut r, 32);

    let mut second = r.ed.open_meter(MeterId(1), MeterSpec::Outputs);
    second.enable();
    commit(&mut r);
    block(&mut r, 32);
    assert_eq!(
        level(&mut first),
        vec![(0.5, 0.5, 32)],
        "the first block only"
    );
    assert_eq!(level(&mut second), vec![(0.5, 0.5, 32)], "the second block");

    r.ed.close_meter(MeterId(1));
    commit(&mut r);
    let mut third = r.ed.open_meter(MeterId(1), MeterSpec::Ports(vec![out(1)]));
    third.enable();
    commit(&mut r);
    block(&mut r, 32);
    assert_eq!(level(&mut third), vec![(0.5, 0.5, 32)]);
    assert_eq!(level(&mut second), vec![(0.0, 0.0, 0)]);
}

/// A meter's lanes are fixed by how it was opened: editing a `Ports` list to
/// another length, or growing the outputs a master follows past the cells, is
/// refused at the commit.
///
/// Mutation (run, recorded here): see the list at the end of this file.
#[test]
fn a_meter_cannot_change_its_width_under_its_cells() {
    let mut r = rig(1, false);
    constants(&mut r, &[0.5, 0.25]);
    let _ports =
        r.ed.open_meter(MeterId(1), MeterSpec::Ports(vec![out(1), out(2)]));
    commit(&mut r);
    let Some(MeterSpec::Ports(sources)) = r.ed.spec_mut().topology.meters.get_mut(&MeterId(1))
    else {
        panic!("open");
    };
    sources.push(Source::Zero);
    assert_eq!(
        r.ed.commit(),
        Err(CommitError::MeterWidth {
            meter: MeterId(1),
            sources: 3,
            capacity: 2
        })
    );
    r.ed.close_meter(MeterId(1));

    let _master = r.ed.open_meter(MeterId(2), MeterSpec::Outputs);
    r.ed.spec_mut().topology.outputs = vec![Source::Zero; tutti_graph::MAX_ROOT_CHANNELS + 1];
    assert!(matches!(
        r.ed.commit(),
        Err(CommitError::MeterWidth {
            meter: MeterId(2),
            ..
        })
    ));
}

/// An export forks the graph: the fork carries no meter, so the live reader
/// never sees offline audio; the fork's own meter measures its own render.
///
/// The fork carries no meter by construction: `fork_gather` builds the fork's
/// topology piecewise from inputs, outputs and edges.
///
/// Mutation (run, recorded here): see the list at the end of this file.
#[test]
fn a_fork_meters_its_own_render_and_leaves_the_live_meter_alone() {
    let mut r = rig(1, false);
    constants(&mut r, &[0.5]);
    let mut live = r.ed.open_meter(MeterId(1), MeterSpec::Outputs);
    live.enable();
    commit(&mut r);
    block(&mut r, 32);
    assert_eq!(level(&mut live), vec![(0.5, 0.5, 32)]);

    let (mut fed, fexec) =
        r.ed.fork(ForkTarget::Master, ForkMode::Offline, prepare(MAX_BLOCK))
            .expect("forks");
    assert!(
        fed.spec().topology.meters.is_empty(),
        "the live meter is not in the fork"
    );
    let mut mine = fed.open_meter(MeterId(1), MeterSpec::Outputs);
    mine.enable();
    let mut f = Rig {
        ed: fed,
        exec: fexec,
        gated: false,
    };
    commit(&mut f);
    for _ in 0..5 {
        block(&mut f, 200);
    }
    assert_eq!(level(&mut mine), vec![(0.5, 0.5, 1000)]);
    assert_eq!(
        level(&mut live),
        vec![(0.0, 0.0, 0)],
        "the live meter saw none of the export"
    );
}

/// Six chains with different gains and lags, summed in no way: every output
/// has its own signal. Metered by a master and a track meter, serial and then
/// on a pool; the readings of every block are identical bit for bit.
fn run(r: &mut Rig, blocks: usize) -> Vec<Vec<Level>> {
    let values: Vec<f32> = (0..6).map(|k| 0.1 + 0.13 * k as f32).collect();
    constants(r, &values);
    // A chain with latency so the compiler aligns the outputs.
    r.ed.insert(NodeKey(20), "lag", Forkable(Kind::Lag { latency: 7 }));
    r.ed.spec_mut().topology.edges.insert(
        InPort {
            node: NodeKey(20),
            port: 0,
        },
        Edge::Direct(out(1)),
    );
    r.ed.spec_mut().topology.outputs[5] = out(20);
    let mut master = r.ed.open_meter(MeterId(1), MeterSpec::Outputs);
    let mut track =
        r.ed.open_meter(MeterId(2), MeterSpec::Ports(vec![out(2), out(20)]));
    master.enable();
    track.enable();
    commit(r);
    assert_eq!(r.exec.is_parallel(), r.gated, "a pool was set iff gated");
    let mut read = Vec::new();
    for i in 0..blocks {
        block(r, 100 + 13 * i);
        read.push(master.read());
        read.push(track.read());
        r.ed.collect();
    }
    read
}

/// The parallel render's readings are bit-identical to the serial one at
/// several worker counts, with every participant inside `assert_no_alloc`.
///
/// Mutation (run, recorded here): see the list at the end of this file.
#[test]
fn the_parallel_meters_are_bit_identical_and_allocation_free() {
    let mut s = rig(1, false);
    let serial = run(&mut s, 8);
    assert!(
        serial.iter().flatten().any(|l| l.peak.get() > 0.0),
        "the meters carry signal"
    );
    for workers in [2, 3, 4] {
        let mut p = rig(workers, true);
        let par = run(&mut p, 8);
        assert_eq!(serial, par, "{workers} workers");
    }
}

// Mutations, each run, with the test that failed (and the others noted):
// - `block_stats` summing `s` for `s * s`: `a_constant_reads_exactly_per_channel`,
//   `a_spike_is_held_until_read_and_read_once` (RMS).
// - `Lane::take` loading the peak instead of swapping it:
//   `a_spike_is_held_until_read_and_read_once` (the second read still shows 1.0).
// - deleting the `enabled` early return in `meter_op`:
//   `an_enabled_meter_reads_the_block` (the off meter reads 0.5).
// - `Executor::rebuild` not calling `set_width` (width stays 0 for `Outputs`):
//   `the_master_follows_the_outputs` and every master reading.
// - `read_into` taking every lane, not those it writes:
//   `read_into_takes_only_what_it_writes`.
// - `Editor::close_meter` leaving the spec's `MeterSpec` in place:
//   `a_closed_meter_leaves_no_op`.
// - `Editor::reap_meters` returning 0 always: the reaped-reader test.
// - deleting `check_outside` from `MeterReader::drop`:
//   `a_reader_dropped_on_the_audio_thread_panics`.
// - deleting the `meters` loop in `Editor::remove`: the commit is refused
//   (`MeterOutOfRange`) in `removing_a_metered_node_silences_its_lane`.
// - letting the carried writer win over the one just opened in `rebuild`:
//   `a_meter_can_be_redeclared`.
// - `check_meter_widths` returning `Ok(())` at once:
//   `a_meter_cannot_change_its_width_under_its_cells`.
// - copying the live meters into the fork's editor in `fork_gather`:
//   `a_fork_meters_its_own_render_and_leaves_the_live_meter_alone`.
// - making the `Op::Meter` arm of `par.rs` a no-op: the parallel test.
