//! The event inlet: MIDI a thread of your own pushes, each event on its frame.
//!
//! Driven two ways: by hand through `contract::Direct` (the node's own
//! `process` on blocks of chosen length and frame, which is what pins offsets,
//! the late rule, the counts and the rate), and through an `Editor` and an
//! `Executor` (the silent fork, and the serial-against-pooled render with the
//! ring pre-filled, every participant inside `assert_no_alloc`).
//!
//! Every figure is analytic (a frame, an offset, a capacity), none read from
//! another render. The mutations were run by hand and are recorded on each
//! test.

mod common;

use assert_no_alloc::AllocDisabler;
use common::{Kind, TestNode};
use tutti_graph::contract::{Direct, HAND_EVENT_CAPACITY};
use tutti_graph::{
    Edge, Editor, Env, EventEdge, EventIn, EventInletNode, EventInletWriter, EventKind, EventOut,
    EventTapId, EventTapReader, Executor, ForkCause, ForkMode, ForkSource, ForkTarget, Forked,
    InPort, IntoNode, Node, NodeKey, NodeParts, OutPort, PlayClock, Prepare, Refused, Source,
    TapAlign, TapEvent, Transport, TransportChanges,
};
use tutti_types::{At, Frame, Moment, SampleRate, Samples};
use tutti_ump::Ump;

#[global_allocator]
static A: AllocDisabler = AllocDisabler;

const R48: SampleRate = SampleRate(48_000.0);
const R441: SampleRate = SampleRate(44_100.0);

fn note(n: u32) -> Ump {
    Ump([0x2090_0000 | n << 8 | 100, 0, 0, 0])
}

fn off(n: u32) -> Ump {
    Ump([0x2080_0000 | n << 8, 0, 0, 0])
}

fn cc(controller: u32) -> Ump {
    Ump([0x20B0_0000 | controller << 8, 0, 0, 0])
}

type Rig = (Direct<EventInletNode>, EventInletWriter);

fn rig(capacity: usize) -> Rig {
    let (node, tx) = EventInletNode::new(capacity);
    (Direct::new(node, R48, 64), tx)
}

/// `(offset, status nibble, data byte 1)` of what the last block emitted.
fn sent(d: &Direct<EventInletNode>) -> Vec<(u32, u32, u32)> {
    d.events_out(0)
        .iter()
        .map(|e| match e.kind {
            EventKind::Midi(Ump(w)) => (e.offset.get(), (w[0] >> 20) & 0xf, (w[0] >> 8) & 0x7f),
            _ => unreachable!(),
        })
        .collect()
}

/// The keys of what the last block emitted.
fn keys(d: &Direct<EventInletNode>) -> Vec<u32> {
    sent(d).into_iter().map(|(_, _, k)| k).collect()
}

/// A block of `len` frames starting at `frame` of the clock at `rate`,
/// wherever the last one ended: what the executor hands a node after a
/// re-prepare or a stretch it did not run.
fn block_at(d: &mut Direct<EventInletNode>, frame: u64, rate: SampleRate, len: usize) {
    d.block_in(&Env {
        frame: Frame(frame),
        sample_rate: rate,
        block_len: Samples(len),
        transport: Transport::default(),
        changes: TransportChanges::NONE,
        play: PlayClock::ZERO,
    });
}

fn reprepare(d: &mut Direct<EventInletNode>, rate: SampleRate) {
    d.node.prepare(&Prepare::new(rate, Samples(64)));
}

/// An event pushed for frame F is emitted in the block that holds F, on offset
/// F minus the block's first frame, once, for block sizes 1, 64, 100 and 512
/// and for frames on a block's first and last frame and either side of the
/// edge, pushed in reverse order (the node sorts).
///
/// Mutation (run, recorded here): emitting at `Offset::ZERO` whatever the due
/// offset is fails every size above 1; reading the frame from the block's own
/// offset instead of `Env::due` (`w.frame % len`) fails 100 and 512 on the
/// frames past the first block; an event sent one block early (the
/// `NotYet` break removed, so a future event is emitted at offset 0) fails all
/// of them.
#[test]
fn an_event_lands_on_its_frame_across_block_sizes() {
    for size in [1usize, 64, 100, 512] {
        let s = size as u64;
        let mut frames: Vec<u64> = vec![
            0,
            1,
            s - 1,
            s,
            s + 1,
            2 * s - 1,
            2 * s,
            2 * s + 1,
            700,
            1023,
        ];
        frames.sort_unstable();
        frames.dedup();
        frames.retain(|&f| f < 1_100);
        let (node, mut tx) = EventInletNode::new(64);
        let mut d = Direct::new(node, R48, size);
        for (k, &f) in frames.iter().enumerate().rev() {
            tx.push(At::Frame(Frame(f)), note(k as u32)).unwrap();
        }
        let mut got = Vec::new();
        let (mut start, mut blocks) = (0u64, 0u64);
        while start < 1_100 {
            d.block();
            for (offset, _, key) in sent(&d) {
                got.push((start + u64::from(offset), key));
            }
            start += s;
            blocks += 1;
        }
        let want: Vec<(u64, u32)> = frames
            .iter()
            .enumerate()
            .map(|(k, &f)| (f, k as u32))
            .collect();
        assert_eq!(got, want, "block size {size} ({blocks} blocks)");
        assert_eq!(
            (tx.lost(), tx.late(), tx.evicted(), tx.refused()),
            (0, 0, 0, 0)
        );
    }
}

/// `NextBlock` is the next block's first frame and is not late; a frame in the
/// past lands at offset 0 and is counted late, not dropped; a frame ahead
/// waits across blocks, is not lost, and lands on its offset.
///
/// Mutation (run, recorded here): dropping a late event (the clamp to `start`
/// replaced by `continue`) fails the second block; not counting late fails the
/// `late() == 1`; counting a `NextBlock` as late fails the first `late() == 0`;
/// emitting an event ahead at once (a `NotYet` taken as `In(0)`) fails the
/// empty blocks before frame 532.
#[test]
fn next_block_late_and_not_yet() {
    let (mut d, mut tx) = rig(256);
    d.block();
    d.block();
    tx.push(At::NextBlock, note(1)).unwrap();
    d.block(); // 128..192
    assert_eq!(sent(&d), [(0, 9, 1)]);
    assert_eq!(tx.late(), 0, "as soon as possible is not late");
    tx.push(At::Frame(Frame(5)), note(2)).unwrap();
    d.block(); // 192..256
    assert_eq!(sent(&d), [(0, 9, 2)], "a past frame lands at offset 0");
    assert_eq!(tx.late(), 1);
    tx.push(At::Frame(Frame(64 * 8 + 20)), note(3)).unwrap();
    for _ in 0..3 {
        d.block(); // 256..448
        assert_eq!(sent(&d), []);
    }
    d.block(); // 448..512: not yet
    assert_eq!(sent(&d), []);
    d.block(); // 512..576
    assert_eq!(sent(&d), [(20, 9, 3)]);
    d.block();
    assert_eq!(sent(&d), [], "and only once");
    assert_eq!(
        (tx.late(), tx.lost(), tx.evicted(), tx.refused()),
        (1, 0, 0, 0)
    );
}

/// Events due together leave in the order they were pushed: a late one, a
/// `NextBlock` and the same frame, and a note-off pushed after its note-on at
/// one frame never overtakes it.
///
/// Mutation (run, recorded here): clamping nothing on arrival (a past frame
/// kept as it is) sorts the past frame 5 before the earlier-pushed `NextBlock`
/// and fails the order; `partition_point` with `<` instead of `<=` puts the
/// note-off before its note-on and fails.
#[test]
fn equal_frames_leave_in_push_order() {
    let (mut d, mut tx) = rig(256);
    d.block();
    d.block();
    tx.push(At::Frame(Frame(128 + 40)), note(61)).unwrap();
    tx.push(At::Frame(Frame(128 + 10)), note(62)).unwrap();
    tx.push(At::NextBlock, note(64)).unwrap();
    tx.push(At::Frame(Frame(5)), note(63)).unwrap();
    tx.push(At::Frame(Frame(128 + 20)), note(70)).unwrap();
    tx.push(At::Frame(Frame(128 + 20)), off(70)).unwrap();
    d.block();
    assert_eq!(
        sent(&d),
        [
            (0, 9, 64),
            (0, 9, 63),
            (10, 9, 62),
            (20, 9, 70),
            (20, 8, 70),
            (40, 9, 61)
        ]
    );
}

/// A beat is refused with its own error, queues nothing and is not a loss.
///
/// Mutation (run, recorded here): answering `Refused::Full` fails the error;
/// queueing it as `NextBlock` fails the empty block; counting it in `lost`
/// fails the count.
#[test]
fn a_beat_is_refused() {
    let (mut d, mut tx) = rig(8);
    assert_eq!(
        tx.push(At::Beat(tutti_types::Beat(1.0)), note(60)),
        Err(Refused::Beat)
    );
    d.block();
    assert_eq!(sent(&d), []);
    assert_eq!(tx.lost(), 0);
}

/// A producer faster than the graph: the ring of 8 takes 8, refuses the rest
/// with `Full`, never blocks, and `lost()` equals the refusals; what it held
/// is emitted, and room returns once the node has drained.
///
/// Mutation (run, recorded here): not counting a refusal fails `lost`;
/// counting in `evicted` instead fails the other counts; counting twice fails
/// the figure of 4.
#[test]
fn a_full_ring_refuses_and_counts_lost() {
    let (mut d, mut tx) = rig(8);
    for k in 0..8 {
        tx.push(At::NextBlock, note(k)).unwrap();
    }
    for _ in 0..4 {
        assert_eq!(tx.push(At::NextBlock, note(99)), Err(Refused::Full));
    }
    assert_eq!(tx.lost(), 4);
    d.block();
    assert_eq!(keys(&d), (0..8).collect::<Vec<_>>());
    assert_eq!((tx.evicted(), tx.refused(), tx.late()), (0, 0, 0));
    tx.push(At::NextBlock, note(7)).expect("room after a drain");
}

/// Eight events wait (seven note-ons and, latest of all, `last`), the list is
/// full, and a due event arrives: it costs the latest note-on, never `last`.
/// A jump far ahead then shows what survived, in order.
fn full_list_keeps(last: Ump, survivor: Vec<u32>) {
    let (mut d, mut tx) = rig(8);
    for k in 0..7 {
        tx.push(At::Frame(Frame(10_000 + u64::from(k))), note(10 + k))
            .unwrap();
    }
    tx.push(At::Frame(Frame(20_000)), last).unwrap();
    d.block(); // the list is full
    assert_eq!(tx.evicted(), 0);
    tx.push(At::NextBlock, note(72)).unwrap();
    d.block();
    assert_eq!(keys(&d), [72], "a due event is not held up by later ones");
    assert_eq!(tx.evicted(), 1, "it took the room of the latest note-on");
    block_at(&mut d, 30_000, R48, 64);
    assert_eq!(keys(&d), survivor);
    assert_eq!((tx.lost(), tx.refused()), (0, 0));
}

/// A full waiting list evicts the latest event that may be lost and keeps a
/// note-off.
///
/// Mutation (run, recorded here): `must_arrive` answering false (the latest
/// event goes whatever it is) loses the note-off: key 99 is missing; evicting
/// the earliest instead of the latest loses key 10.
#[test]
fn a_full_list_keeps_the_note_off() {
    full_list_keeps(off(99), vec![10, 11, 12, 13, 14, 15, 99]);
}

/// And a panic controller (All Notes Off).
///
/// Mutation (run, recorded here): `must_arrive` ignoring
/// `is_panic_controller` loses the controller: key 123 is missing.
#[test]
fn a_full_list_keeps_a_panic_controller() {
    full_list_keeps(cc(123), vec![10, 11, 12, 13, 14, 15, 123]);
}

/// A list of nothing but note-offs and a panic that is full loses the latest
/// of them, and counts it; the earlier ones stay and a new earlier one takes
/// its place.
///
/// Mutation (run, recorded here): dropping the new event always (the `pop`
/// removed) keeps the late one and loses the early one.
#[test]
fn a_list_of_nothing_but_note_offs_loses_the_latest() {
    let (mut d, mut tx) = rig(8);
    for k in 0u64..8 {
        tx.push(At::Frame(Frame(10_000 + k)), off(10)).unwrap();
    }
    d.block();
    tx.push(At::Frame(Frame(10_000_000)), off(11)).unwrap();
    tx.push(At::Frame(Frame(9_000)), off(12)).unwrap();
    d.block();
    assert_eq!(tx.evicted(), 2);
    block_at(&mut d, 9_000, R48, 64);
    assert_eq!(keys(&d), [12], "the earlier one took the latest's place");
}

/// A loseable event never displaces a must-arrive one: a note-on pushed
/// earlier than a full list of note-offs is the event lost, and every
/// note-off still goes out (a dropped note-off would leave its note hanging).
///
/// Mutation (run, recorded here): removing `must_arrive(ump) &&` from the
/// `None` arm of `keep` pops the latest note-off for the note-on: key 30 is
/// sent and key 27 is missing.
#[test]
fn a_note_on_never_displaces_a_waiting_note_off() {
    let (mut d, mut tx) = rig(8);
    for k in 0u32..8 {
        tx.push(At::Frame(Frame(10_000 + u64::from(k))), off(20 + k))
            .unwrap();
    }
    d.block();
    tx.push(At::Frame(Frame(9_000)), note(30)).unwrap();
    d.block();
    assert_eq!(tx.evicted(), 1);
    block_at(&mut d, 9_000, R48, 64);
    assert_eq!(
        keys(&d),
        Vec::<u32>::new(),
        "the note-on was the event lost"
    );
    block_at(&mut d, 10_000, R48, 64);
    assert_eq!(
        keys(&d),
        (20..28).collect::<Vec<u32>>(),
        "every note-off went out"
    );
}

/// A port narrower than the burst: the node's list holds 1500 events, the port
/// (the harness's, 1024) takes 1024, and the other 476 are counted as
/// refused by the port, and nowhere else.
///
/// Mutation (run, recorded here): counting the refusal in `evicted` fails
/// `evicted == 0`; not counting it fails `refused`; counting it in `lost` fails
/// `lost == 0`.
#[test]
fn a_port_narrower_than_the_burst_counts_refused() {
    let burst = HAND_EVENT_CAPACITY + 476;
    let (mut d, mut tx) = rig(burst);
    for k in 0..burst {
        tx.push(At::NextBlock, note((k % 100) as u32)).unwrap();
    }
    d.block();
    assert_eq!(sent(&d).len(), HAND_EVENT_CAPACITY);
    assert_eq!(tx.refused(), 476);
    assert_eq!((tx.lost(), tx.evicted(), tx.late()), (0, 0, 0));
}

/// The clock is where the last block ended, on the clock the node ran on and
/// not a count of blocks, with the rate it was counted at; a new rate moves it
/// to the same wall-clock time.
///
/// Mutation (run, recorded here): publishing `start` instead of the block's
/// end fails the first two figures; `prepare` leaving `next` alone fails the
/// last.
#[test]
fn the_writer_reads_the_clock_and_its_rate() {
    let (mut d, tx) = rig(8);
    let at = |frame, rate| Some(Moment::new(Frame(frame), rate));
    assert_eq!(tx.clock(), at(0, R48));
    d.block();
    assert_eq!(tx.clock(), at(64, R48));
    d.set_block_len(48);
    d.block();
    assert_eq!(tx.clock(), at(112, R48));
    reprepare(&mut d, R441);
    // 112 x 44100 / 48000 = 102.9
    assert_eq!(tx.clock(), at(103, R441));
}

/// Pushed before a rate change, an event lands at the rescaled frame: the
/// same wall-clock time at the new rate, not the old frame number.
///
/// Mutation (run, recorded here): `prepare` not rescaling the waiting list
/// leaves it at frame 1000, in the block of 976..1040, and the block at 912
/// sees nothing.
#[test]
fn what_waits_is_rescaled_by_a_rate_change() {
    let (mut d, mut tx) = rig(8);
    tx.push(At::Frame(Frame(1_000)), note(60)).unwrap();
    d.block();
    d.block();
    d.block(); // the node holds it now
    assert_eq!(sent(&d), []);
    reprepare(&mut d, R441);
    // 1000 x 44100 / 48000 = 918.75, so offset 7 of the block at 912
    block_at(&mut d, 912, R441, 64);
    assert_eq!(sent(&d), [(7, 9, 60)]);
}

/// A reading taken at 48 kHz and pushed after the graph went to 44.1 kHz still
/// names its instant: the read, the change and the push, in that order.
///
/// Mutation (run, recorded here): the arrival rescale returning the frame
/// unrescaled lands it at 692, and the block at 628 sees nothing; stamping the
/// push with the node's rate instead of the moment's does the same.
#[test]
fn a_moment_read_before_a_re_prepare_keeps_its_instant() {
    let (mut d, mut tx) = rig(8);
    d.block();
    d.block();
    d.block();
    let read = tx.clock().expect("prepared");
    assert_eq!((read.frame(), read.rate()), (Frame(192), R48));
    reprepare(&mut d, R441);
    tx.push_at(read.after(Samples(500)), note(61)).unwrap();
    // 692 x 44100 / 48000 = 635.7, so offset 8 of the block at 628
    block_at(&mut d, 628, R441, 64);
    assert_eq!(sent(&d), [(8, 9, 61)]);
}

/// A frame pushed with `push` is at the rate the node last published, even
/// when the graph is re-prepared before the node has taken it from the ring:
/// the same wall-clock time at the new rate.
///
/// Mutation (run, recorded here): `push` stamping no rate (0, "the node's")
/// reads the frame at 44.1 kHz and lands it at 1000, not 918.75: the block at
/// 912 sees nothing.
#[test]
fn a_frame_pushed_before_a_re_prepare_is_at_the_rate_it_was_pushed() {
    let (mut d, mut tx) = rig(8);
    tx.push(At::Frame(Frame(1_000)), note(60)).unwrap();
    reprepare(&mut d, R441); // the ring still holds it
    block_at(&mut d, 912, R441, 64);
    assert_eq!(sent(&d), [(7, 9, 60)]);
}

/// An event whose frame passed while the node did not run (a re-prepare
/// suspends it) lands on the first frame of the block that follows, in the
/// order it was due, and is counted late.
///
/// Mutation (run, recorded here): without the clamp of what waits,
/// `Env::due` is `Late` for the block and the event is emitted nowhere: the
/// block at 500 sees nothing.
#[test]
fn what_came_due_while_the_node_slept_lands_on_the_first_frame() {
    let (mut d, mut tx) = rig(8);
    tx.push(At::Frame(Frame(100)), note(60)).unwrap();
    tx.push(At::Frame(Frame(120)), note(61)).unwrap();
    d.block(); // frames 0..64: both wait
    block_at(&mut d, 500, R48, 64);
    assert_eq!(sent(&d), [(0, 9, 60), (0, 9, 61)]);
    assert_eq!(tx.late(), 2);
}

/// What waits survives a reset: a note-off waiting must still arrive.
///
/// Mutation (run, recorded here): `reset` clearing the waiting list fails it.
#[test]
fn what_waits_survives_a_reset() {
    let (mut d, mut tx) = rig(8);
    tx.push(At::Frame(Frame(200)), note(70)).unwrap();
    d.block();
    d.node.reset();
    d.block();
    d.block();
    d.block();
    assert_eq!(sent(&d), [(8, 9, 70)]);
}

const MAX_BLOCK: usize = 512;

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

fn out(node: u64) -> Source {
    Source::Node(OutPort {
        node: NodeKey(node),
        port: 0,
    })
}

/// Inlets 1..=n into a `Consumer` (key 10) whose audio is the only output, so
/// what the inlets emitted is audible as a bit-exact, order-sensitive signal.
/// With `tap`, inlet 1's port is also tapped raw.
fn graph(
    n: u16,
    workers: usize,
    no_alloc: bool,
    tap: bool,
    heavy: bool,
) -> (
    Editor,
    Executor,
    Vec<EventInletWriter>,
    Option<EventTapReader>,
) {
    let (mut ed, mut exec) = Editor::new(common::prepare(MAX_BLOCK));
    if workers > 1 {
        exec.set_pool(Some(if no_alloc {
            common::TestPool::no_alloc(workers)
        } else {
            common::test_pool(workers)
        }));
    }
    let mut writers = Vec::new();
    for k in 1..=u64::from(n) {
        let (node, writer) = EventInletNode::new(256);
        ed.insert(NodeKey(k), "inlet", node);
        writers.push(writer);
    }
    ed.insert(
        NodeKey(10),
        "consumer",
        Forkable(Kind::Consumer { inputs: n }),
    );
    for k in 1..=n {
        ed.spec_mut().connect_events(
            EventIn {
                node: NodeKey(10),
                port: k - 1,
            },
            EventEdge::Direct(EventOut {
                node: NodeKey(u64::from(k)),
                port: 0,
            }),
        );
    }
    // Heavy audio chains beside the inlets, so the cost model has work worth
    // a thread: the plan is spread on a pool only when that pays.
    for c in (0..4u64).filter(|_| heavy) {
        let base = 100 + c * 10;
        ed.insert(
            NodeKey(base),
            "const",
            Forkable(Kind::Const {
                value: 0.25,
                width: 1,
            }),
        );
        for g in 1..4u64 {
            ed.insert(
                NodeKey(base + g),
                "gain",
                Forkable(Kind::Gain {
                    gain: 0.5,
                    width: 1,
                }),
            );
            ed.spec_mut().topology.edges.insert(
                InPort {
                    node: NodeKey(base + g),
                    port: 0,
                },
                Edge::Direct(out(base + g - 1)),
            );
        }
    }
    let sum = if heavy { 5 } else { 1 };
    ed.insert(NodeKey(30), "sum", Forkable(Kind::Sum { inputs: sum }));
    let edges = &mut ed.spec_mut().topology.edges;
    edges.insert(
        InPort {
            node: NodeKey(30),
            port: 0,
        },
        Edge::Direct(out(10)),
    );
    for c in (0..4u16).filter(|_| heavy) {
        edges.insert(
            InPort {
                node: NodeKey(30),
                port: c + 1,
            },
            Edge::Direct(out(103 + u64::from(c) * 10)),
        );
    }
    ed.spec_mut().topology.outputs = vec![out(30)];
    let tap = tap.then(|| {
        ed.open_event_tap(
            EventTapId(1),
            EventOut {
                node: NodeKey(1),
                port: 0,
            },
            TapAlign::Raw,
            256,
        )
        .expect("the port exists")
    });
    (ed, exec, writers, tap)
}

fn commit(ed: &mut Editor, exec: &mut Executor) {
    ed.commit().expect("compiles");
    exec.apply_pending();
    ed.collect();
}

/// Renders `total` frames in blocks of `sizes` in turn, gated when asked, and
/// returns the audio's bits.
fn render(
    ed: &mut Editor,
    exec: &mut Executor,
    sizes: &[usize],
    total: usize,
    gated: bool,
) -> Vec<u32> {
    let t = Transport::default();
    let (mut done, mut i) = (0, 0);
    let mut bits = Vec::new();
    while done < total {
        let n = sizes[i % sizes.len()].min(total - done);
        let mut chan = vec![0.0f32; n];
        {
            let mut outs: Vec<&mut [f32]> = vec![chan.as_mut_slice()];
            if gated {
                assert_no_alloc::assert_no_alloc(|| exec.process(n, &t, &[], &mut outs));
            } else {
                exec.process(n, &t, &[], &mut outs);
            }
        }
        ed.collect();
        bits.extend(chan.iter().map(|s| s.to_bits()));
        done += n;
        i += 1;
    }
    bits
}

/// What the three producers push: a mix of `NextBlock`, frames on block edges
/// of every size below, equal frames, and note-offs.
fn script(k: u32) -> Vec<(At, Ump)> {
    let mut v = vec![(At::NextBlock, note(k))];
    for i in 0..30u32 {
        let frame = u64::from(i) * (13 + u64::from(k)) + u64::from(k);
        let ump = if i % 4 == 3 { off(i) } else { note(i + k) };
        v.push((At::Frame(Frame(frame)), ump));
    }
    v.push((At::Frame(Frame(100)), note(1)));
    v.push((At::Frame(Frame(100)), off(1)));
    v
}

/// A pooled render with the rings pre-filled equals the serial one, bit for
/// bit in its audio, with inlet 1's tap listing every pushed event at its
/// frame (the serial run is also checked against the script); every
/// participant runs inside `assert_no_alloc`.
///
/// Mutation (run, recorded here): emitting every event at `Offset::ZERO`
/// fails the analytic tap list and the audio; `partition_point` with `<`
/// (equal frames in reverse push order) fails the tap list; an allocation in
/// `process` (a `Vec::with_capacity`) aborts the gated pooled runs.
#[test]
fn the_parallel_render_equals_the_serial_one_and_allocates_nothing() {
    let run = |workers: usize, gated: bool| {
        let (mut ed, mut exec, mut writers, tap) = graph(3, workers, gated, true, true);
        for (k, w) in writers.iter_mut().enumerate() {
            for (at, ump) in script(k as u32 + 1) {
                w.push(at, ump).unwrap();
            }
        }
        commit(&mut ed, &mut exec);
        assert_eq!(exec.is_parallel(), gated);
        let bits = render(&mut ed, &mut exec, &[100, 37, 128], 600, gated);
        let mut tap = tap.expect("opened");
        let mut got = Vec::new();
        let mut buf = [TapEvent {
            frame: Frame(0),
            ump: Ump::default(),
        }; 64];
        loop {
            let n = tap.pop_into(&mut buf);
            if n == 0 {
                break;
            }
            got.extend_from_slice(&buf[..n]);
        }
        assert_eq!(tap.take_lost(), 0);
        (bits, got, writers[0].late())
    };
    let (serial_bits, serial_tap, late) = run(1, false);
    assert!(serial_bits.iter().any(|&b| b != 0), "the inlets are heard");
    // Inlet 1's script, in due order: `NextBlock` at frame 0, then each frame
    // in turn (the stable sort keeps a note-on before its note-off at 100).
    let mut want: Vec<(u64, Ump)> = script(1)
        .into_iter()
        .map(|(at, u)| match at {
            At::Frame(f) => (f.get(), u),
            _ => (0, u),
        })
        .collect();
    want.sort_by_key(|&(f, _)| f);
    let want: Vec<TapEvent> = want
        .into_iter()
        .map(|(f, ump)| TapEvent {
            frame: Frame(f),
            ump,
        })
        .collect();
    assert_eq!(serial_tap, want);
    assert_eq!(late, 0);
    for workers in [2, 3, 4] {
        let (bits, tap, _) = run(workers, true);
        assert_eq!(serial_bits, bits, "{workers} workers: audio");
        assert_eq!(serial_tap, tap, "{workers} workers: tap");
    }
}

/// The fork is silent: an export of a graph holding an inlet commits (the
/// audio inlet would refuse it), renders no event from it, and takes none
/// pushed afterwards, while the live graph hears the same pushes.
///
/// Mutation (run, recorded here): an `IntoNode` with `fork: None` makes the
/// fork fail (`expect("forks")`); a fork that would carry the live ring is not
/// constructible (the consumer is not `Clone`), so the silence assertions pin
/// the live graph hearing what the fork does not.
#[test]
fn the_fork_is_silent() {
    let (mut ed, mut exec, mut writers, _) = graph(1, 1, false, false, false);
    writers[0].push(At::Frame(Frame(10)), note(5)).unwrap();
    commit(&mut ed, &mut exec);
    let (mut fed, mut fexec) = ed
        .fork(
            ForkTarget::Master,
            ForkMode::Offline,
            common::prepare(MAX_BLOCK),
        )
        .expect("forks");
    fexec.apply_pending();
    let live = render(&mut ed, &mut exec, &[64], 64, false);
    assert!(live.iter().any(|&b| b != 0), "the live graph hears it");
    writers[0].push(At::NextBlock, note(6)).unwrap();
    let silent = render(&mut fed, &mut fexec, &[64], 128, false);
    assert!(silent.iter().all(|&b| b == 0), "the fork renders silence");
}
