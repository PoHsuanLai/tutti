//! Level meters as plan ops: per channel on any port or on the outputs,
//! exact on analytic signals, peak held until read, bit-identical on the
//! parallel executor and allocation-free on every participant. A closed meter
//! compiles to no op.
//!
//! The reference interpreter (`reference.rs`) renders a `Topology`'s outputs
//! and knows neither taps nor meters: a meter reads the signal and changes
//! nothing, so there is nothing for it to compare. The properties here are
//! analytic figures and the serial render against the parallel one.

use tutti_graph::testing as common;

use assert_no_alloc::AllocDisabler;
use common::{Kind, TestNode, prepare};
use tutti_graph::{
    CommitError, Cx, Editor, Executor, ForkCause, ForkMode, ForkSource, ForkTarget, Forked,
    IntoNode, Io, Level, MeterReader, Node, NodeParts, Op, Shape, Status, Transport, Unforkable,
};
use tutti_graph::{Edge, InPort, OutPort, Source};
use tutti_graph::{MeterId, MeterSpec, NodeKey};
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
    r.ed.spec_mut().topology.set_outputs(outputs);
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
    let t = Transport::new(true, Bpm::of(120.0), Beat(0.0), None);
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
#[test]
fn a_constant_reads_exactly_per_channel() {
    let mut r = rig(1, false);
    let values: Vec<f32> = (1..=6).map(|k| 0.5f32.powi(k)).collect();
    constants(&mut r, &values);
    let mut master =
        r.ed.open_meter(MeterId(1), MeterSpec::Outputs)
            .expect("the sources exist");
    let mut track =
        r.ed.open_meter(MeterId(2), MeterSpec::raw(vec![out(3), out(1)]))
            .expect("the sources exist");
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
#[test]
fn measuring_is_allocation_free_with_the_meter_enabled() {
    let mut r = rig(1, true);
    constants(&mut r, &[0.5, 0.25]);
    let mut m =
        r.ed.open_meter(MeterId(1), MeterSpec::Outputs)
            .expect("the sources exist");
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
        Shape::source_unassigned(ChannelLayout::MONO)
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
    r.ed.spec_mut().topology.set_outputs(vec![out(1)]);
    let mut m =
        r.ed.open_meter(MeterId(1), MeterSpec::Outputs)
            .expect("the sources exist");
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
#[test]
fn an_enabled_meter_reads_the_block() {
    let mut r = rig(1, false);
    constants(&mut r, &[0.5, 0.5]);
    let mut m =
        r.ed.open_meter(MeterId(1), MeterSpec::Outputs)
            .expect("the sources exist");
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
#[test]
fn the_master_follows_the_outputs() {
    let mut r = rig(1, false);
    constants(&mut r, &[0.5, 0.25]);
    let mut m =
        r.ed.open_meter(MeterId(1), MeterSpec::Outputs)
            .expect("the sources exist");
    m.enable();
    commit(&mut r);
    block(&mut r, 32);
    assert_eq!(level(&mut m), vec![(0.5, 0.5, 32), (0.25, 0.25, 32)]);

    r.ed.spec_mut().topology.outputs[0] = Vec::new();
    r.ed.spec_mut().topology.outputs.push(vec![out(1)]);
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
#[test]
fn read_into_takes_only_what_it_writes() {
    let mut r = rig(1, false);
    constants(&mut r, &[0.5, 0.25]);
    let mut m =
        r.ed.open_meter(MeterId(1), MeterSpec::Outputs)
            .expect("the sources exist");
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
#[test]
fn a_closed_meter_leaves_no_op() {
    let mut r = rig(1, false);
    constants(&mut r, &[0.5, 0.25, 0.125]);
    commit(&mut r);
    let (never, none) = meter_ops(&r);
    assert_eq!(none, 0);

    let a =
        r.ed.open_meter(MeterId(1), MeterSpec::Outputs)
            .expect("the sources exist");
    let _b =
        r.ed.open_meter(MeterId(2), MeterSpec::raw(vec![out(1), out(2), out(3)]))
            .expect("the sources exist");
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
#[test]
fn a_reader_dropped_is_reaped_and_its_cells_freed_off_the_audio_thread() {
    let mut r = rig(1, true);
    constants(&mut r, &[0.5]);
    let m =
        r.ed.open_meter(MeterId(1), MeterSpec::Outputs)
            .expect("the sources exist");
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
#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "a meter reader dropped on the audio thread")]
fn a_reader_dropped_on_the_audio_thread_panics() {
    let mut r = rig(1, false);
    let m =
        r.ed.open_meter(MeterId(1), MeterSpec::raw(vec![Source::Zero]))
            .expect("the sources exist");
    let _inside = tutti_rt::RtScope::enter();
    drop(m);
}

/// Removing a metered node leaves its lane on silence, the other lanes play
/// on, and the graph still commits.
#[test]
fn removing_a_metered_node_silences_its_lane() {
    let mut r = rig(1, false);
    constants(&mut r, &[0.5, 0.25]);
    let mut m =
        r.ed.open_meter(MeterId(1), MeterSpec::raw(vec![out(1), out(2)]))
            .expect("the sources exist");
    m.enable();
    commit(&mut r);
    block(&mut r, 32);
    assert_eq!(level(&mut m), vec![(0.5, 0.5, 32), (0.25, 0.25, 32)]);

    r.ed.remove(NodeKey(2));
    commit(&mut r);
    block(&mut r, 32);
    assert_eq!(level(&mut m), vec![(0.5, 0.5, 32), (0.0, 0.0, 32)]);
}

/// A key is the author's and can be inserted again after a removal: a meter
/// left naming the removed node must not start reading the new one.
#[test]
fn a_reused_key_is_not_read_by_the_old_meter() {
    let mut r = rig(1, false);
    constants(&mut r, &[0.5, 0.25]);
    let mut m =
        r.ed.open_meter(MeterId(1), MeterSpec::raw(vec![out(2)]))
            .expect("the sources exist");
    m.enable();
    commit(&mut r);
    block(&mut r, 32);
    assert_eq!(level(&mut m), vec![(0.25, 0.25, 32)]);

    r.ed.remove(NodeKey(2));
    r.ed.insert(
        NodeKey(2),
        "again",
        Forkable(Kind::Const {
            value: 0.125,
            width: 1,
        }),
    );
    commit(&mut r);
    block(&mut r, 32);
    assert_eq!(
        level(&mut m),
        vec![(0.0, 0.0, 32)],
        "the meter followed the removal, not the key"
    );
}

/// What a run of `blocks` blocks puts on the outputs, as bits, with the
/// meters `mode` asks for declared and enabled. Three paths from one varying
/// input: latent by 30 and by 11 frames, and bare (delayed to meet the
/// slowest), ragged block sizes, stopped blocks and a seek.
fn output_bits(workers: usize, master: bool, track: bool) -> Vec<u32> {
    let mut r = rig(workers, workers > 1);
    r.ed.spec_mut().topology.inputs = ChannelLayout::MONO;
    let mut outputs = Vec::new();
    for (k, latency) in [30usize, 11, 0].into_iter().enumerate() {
        let (lag, gain) = (10 + k as u64, 20 + k as u64);
        r.ed.insert(NodeKey(lag), "lag", Forkable(Kind::Lag { latency }));
        r.ed.insert(
            NodeKey(gain),
            "gain",
            Forkable(Kind::Gain {
                gain: 0.5 + k as f32 / 10.0,
                width: 1,
            }),
        );
        let t = &mut r.ed.spec_mut().topology;
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
        outputs.push(out(gain));
    }
    r.ed.spec_mut().topology.set_outputs(outputs.clone());
    let mut readers = Vec::new();
    if master {
        readers.push(
            r.ed.open_meter(MeterId(1), MeterSpec::Outputs)
                .expect("the sources exist"),
        );
    }
    if track {
        readers.push(
            r.ed.open_meter(MeterId(2), MeterSpec::aligned(outputs))
                .expect("the sources exist"),
        );
    }
    for m in &readers {
        m.enable();
    }
    commit(&mut r);
    let mut bits = Vec::new();
    let mut frame = 0u64;
    let sizes = [64usize, 100, 37, 128, 1, 90, 50, 128, 64, 13];
    for (i, &n) in sizes.iter().enumerate() {
        let input: Vec<f32> = (0..n as u64)
            .map(|k| (((frame + k) * 7919) % 20_011) as f32 / 20_011.0 - 0.5)
            .collect();
        frame += n as u64;
        let mut chans = vec![vec![0.0f32; n]; 3];
        let mut outs: Vec<&mut [f32]> = chans.iter_mut().map(Vec::as_mut_slice).collect();
        // Stopped for blocks 2, 3 and 9; a seek at block 5.
        let playing = !matches!(i, 2 | 3 | 9);
        let beat = if i >= 5 { 64.0 } else { 0.0 };
        let t = Transport::new(playing, Bpm::of(120.0), Beat(beat), None);
        let ins = [&input[..]];
        if r.gated {
            assert_no_alloc::assert_no_alloc(|| r.exec.process(n, &t, &ins, &mut outs));
        } else {
            r.exec.process(n, &t, &ins, &mut outs);
        }
        bits.extend(chans.iter().flatten().map(|s| s.to_bits()));
        for m in &mut readers {
            let _ = m.read();
        }
        r.ed.collect();
    }
    bits
}

/// **A meter changes no output bit.** With latency on two of three master
/// paths, ragged blocks, stopped blocks and a seek, the outputs are identical
/// with no meter, a master meter, an aligned track meter and both, serial and
/// on a pool of three inside `assert_no_alloc`.
#[test]
fn a_meter_changes_no_output_bit() {
    let bare = output_bits(1, false, false);
    assert!(bare.iter().any(|&b| f32::from_bits(b) != 0.0), "signal");
    for (master, track) in [(true, false), (false, true), (true, true)] {
        assert_eq!(
            output_bits(1, master, track),
            bare,
            "serial {master} {track}"
        );
        assert_eq!(output_bits(3, master, track), bare, "pool {master} {track}");
    }
    assert_eq!(output_bits(3, false, false), bare, "pool, no meter");
}

/// A node replaced by one with fewer outputs leaves the meter's lanes past the
/// new width on silence, and the commit succeeds: a display must not wedge the
/// graph. (A tap does the same: `tests/tap.rs`.)
#[test]
fn a_narrowed_node_silences_the_lanes_past_its_width_and_commits() {
    let mut r = rig(1, false);
    r.ed.insert(
        NodeKey(1),
        "wide",
        Forkable(Kind::Const {
            value: 0.5,
            width: 2,
        }),
    );
    let port = |p| {
        Source::Node(OutPort {
            node: NodeKey(1),
            port: p,
        })
    };
    r.ed.spec_mut().topology.set_outputs(vec![port(0)]);
    let mut m =
        r.ed.open_meter(MeterId(1), MeterSpec::raw(vec![port(0), port(1)]))
            .expect("the sources exist");
    m.enable();
    commit(&mut r);
    block(&mut r, 32);
    assert_eq!(level(&mut m), vec![(0.5, 0.5, 32), (0.5, 0.5, 32)]);

    // The same key, now one output wide: the meter's second lane names a port
    // that no longer exists.
    r.ed.insert(
        NodeKey(1),
        "narrow",
        Forkable(Kind::Const {
            value: 0.25,
            width: 1,
        }),
    );
    commit(&mut r);
    block(&mut r, 32);
    assert_eq!(level(&mut m), vec![(0.25, 0.25, 32), (0.0, 0.0, 32)]);
}

/// The pulse source: 1.0 for its first `len` frames, then silence.
struct Pulse {
    len: usize,
    done: usize,
}

impl Node for Pulse {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(ChannelLayout::MONO)
    }
    fn prepare(&mut self, _: &tutti_graph::Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        for (k, s) in io.output(0).iter_mut().enumerate() {
            *s = if self.done + k < self.len { 1.0 } else { 0.0 };
        }
        self.done += io.output(0).len();
        Status::Modified
    }
    fn reset(&mut self) {}
}

/// **The master reads what the outputs carry, at the outputs' time.** With a
/// latent node on one path the compiler delays the other output to meet it;
/// the master meter reads each channel after that delay, so a pulse that the
/// output carries at frames 30..80 reads at 30..80, not 0..50 (a meter that
/// ran ahead of the audio would go quiet while delayed audio still plays).
/// A raw track meter on the same source reads the pulse where it leaves the
/// node, and an aligned one reads it with the output.
///
/// 40-frame block, 50-frame pulse, 30 frames of latency: the aligned lane has
/// 10 ones in the first block (RMS 0.5), the raw one 40 (RMS 1.0).
#[test]
fn the_master_reads_at_the_outputs_time() {
    let mut r = rig(1, false);
    r.ed.insert(NodeKey(1), "pulse", Unforkable(Pulse { len: 50, done: 0 }));
    r.ed.insert(NodeKey(2), "pulse", Unforkable(Pulse { len: 50, done: 0 }));
    r.ed.insert(NodeKey(3), "lag", Forkable(Kind::Lag { latency: 30 }));
    r.ed.spec_mut().topology.edges.insert(
        InPort {
            node: NodeKey(3),
            port: 0,
        },
        vec![Edge::Direct(out(2))],
    );
    // Output 0 is the latent path, output 1 the bare pulse (delayed to meet it).
    r.ed.spec_mut().topology.set_outputs(vec![out(3), out(1)]);
    let mut master =
        r.ed.open_meter(MeterId(1), MeterSpec::Outputs)
            .expect("the sources exist");
    let mut raw =
        r.ed.open_meter(MeterId(2), MeterSpec::raw(vec![out(1)]))
            .expect("the sources exist");
    let mut aligned =
        r.ed.open_meter(MeterId(3), MeterSpec::aligned(vec![out(1)]))
            .expect("the sources exist");
    for m in [&master, &raw, &aligned] {
        m.enable();
    }
    commit(&mut r);
    let outs = block(&mut r, 40);
    // What the output carries: the pulse from frame 30.
    assert!(outs[1][..30].iter().all(|&s| s == 0.0));
    assert!(outs[1][30..].iter().all(|&s| s == 1.0));
    let tenth = (10.0f64 / 40.0).sqrt() as f32;
    assert_eq!(
        level(&mut master),
        vec![(1.0, tenth, 40), (1.0, tenth, 40)],
        "both outputs at the outputs' time"
    );
    assert_eq!(
        level(&mut raw),
        vec![(1.0, 1.0, 40)],
        "raw: as it leaves the node"
    );
    assert_eq!(
        level(&mut aligned),
        vec![(1.0, tenth, 40)],
        "aligned: with the output"
    );
    // The pulse ends at frame 50 at the node and at 80 on the output.
    block(&mut r, 40);
    assert_eq!(
        level(&mut raw),
        vec![(1.0, (10.0f64 / 40.0).sqrt() as f32, 40)]
    );
    assert_eq!(
        level(&mut master)[1],
        (1.0, 1.0, 40),
        "still playing at frame 79"
    );
}

/// Closing, then declaring the same id again, makes a fresh meter: the new
/// reader is fed from the next commit, and the old one gets nothing more. An
/// id opened over a live one replaces it the same way.
#[test]
fn a_meter_can_be_redeclared() {
    let mut r = rig(1, false);
    constants(&mut r, &[0.5]);
    let mut first =
        r.ed.open_meter(MeterId(1), MeterSpec::Outputs)
            .expect("the sources exist");
    first.enable();
    commit(&mut r);
    block(&mut r, 32);

    let mut second =
        r.ed.open_meter(MeterId(1), MeterSpec::Outputs)
            .expect("the sources exist");
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
    let mut third =
        r.ed.open_meter(MeterId(1), MeterSpec::raw(vec![out(1)]))
            .expect("the sources exist");
    third.enable();
    commit(&mut r);
    block(&mut r, 32);
    assert_eq!(level(&mut third), vec![(0.5, 0.5, 32)]);
    assert_eq!(level(&mut second), vec![(0.0, 0.0, 0)]);
}

/// A meter's lanes are fixed by how it was opened: editing a `Ports` list to
/// another length, or growing the outputs a master follows past the cells, is
/// refused at the commit.
#[test]
fn a_meter_cannot_change_its_width_under_its_cells() {
    let mut r = rig(1, false);
    constants(&mut r, &[0.5, 0.25]);
    let _ports =
        r.ed.open_meter(MeterId(1), MeterSpec::raw(vec![out(1), out(2)]))
            .expect("the sources exist");
    commit(&mut r);
    let Some(MeterSpec::Ports { sources, .. }) =
        r.ed.spec_mut().topology.meters.get_mut(&MeterId(1))
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

    let _master =
        r.ed.open_meter(MeterId(2), MeterSpec::Outputs)
            .expect("the sources exist");
    r.ed.spec_mut()
        .topology
        .set_outputs(vec![Source::Zero; tutti_graph::MAX_ROOT_CHANNELS + 1]);
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
#[test]
fn a_fork_meters_its_own_render_and_leaves_the_live_meter_alone() {
    let mut r = rig(1, false);
    constants(&mut r, &[0.5]);
    let mut live =
        r.ed.open_meter(MeterId(1), MeterSpec::Outputs)
            .expect("the sources exist");
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
    let mut mine = fed
        .open_meter(MeterId(1), MeterSpec::Outputs)
        .expect("the sources exist");
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
        vec![Edge::Direct(out(1))],
    );
    r.ed.spec_mut().topology.outputs[5] = vec![out(20)];
    let mut master =
        r.ed.open_meter(MeterId(1), MeterSpec::Outputs)
            .expect("the sources exist");
    let mut track =
        r.ed.open_meter(MeterId(2), MeterSpec::raw(vec![out(2), out(20)]))
            .expect("the sources exist");
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
