//! The nodes at the graph's MIDI border (doc 013, rewrite item 5):
//! [`MidiInputNode`] sends a wire's events out of their channels' ports,
//! translated; [`MidiQueueNode`] sends what a control thread pushed;
//! [`MidiOutNode`] hands what reaches it back to one; [`ClockNode`] ticks
//! Beat Clock on the frames the transport reaches, a start inside a block
//! included.

use std::sync::{Arc, Mutex};

use tutti_core::{Beat, Bpm, NodeKey, SampleRate, Samples};
use tutti_graph::{
    Cx, Editor, EventEdge, EventIn, EventKind, EventOut, Executor, ForkMode, ForkTarget, Io, Node,
    Offset, Prepare, Shape, Status, Transport, TransportChanges, Ump,
};
use tutti_midi_runtime::{
    ClockNode, MidiInputNode, MidiOutNode, MidiQueueNode, CHANNELLESS_PORT, MIDI_INPUT_PORTS,
};
use tutti_midi_types::mpe::{MpeMode, MpeZoneConfig};
use tutti_midi_types::{cc, CCNumber, MidiChannel, MidiEvent, MidiGroup, MidiIn};

const RATE: f64 = 48_000.0;

/// `(sink port, absolute frame, event)` of every MIDI event a sink received.
type Seen = Arc<Mutex<Vec<(usize, u64, MidiEvent)>>>;

/// Logs the MIDI on each of its `ports` event inputs; a silent mono output,
/// so a fork has somewhere to end.
#[derive(Clone)]
struct Sink {
    ports: u16,
    seen: Seen,
}

impl Node for Sink {
    fn shape(&self) -> Shape {
        Shape::audio(
            tutti_core::ChannelLayout::EMPTY,
            tutti_core::ChannelLayout::MONO,
        )
        .with_events(self.ports, 0)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let mut seen = self.seen.lock().unwrap();
        for p in 0..usize::from(self.ports) {
            for e in io.events(p) {
                if let EventKind::Midi(Ump(data)) = e.kind {
                    let ev = MidiEvent {
                        frame_offset: e.offset.get(),
                        data,
                    };
                    seen.push((p, cx.env.frame.get() + u64::from(e.offset.get()), ev));
                }
            }
        }
        io.output(0).fill(0.0);
        Status::Modified
    }
    fn reset(&mut self) {}
}

/// A source at key 1 whose `ports` event outputs each feed the sink's input
/// of the same number (key 2), blocks of at most `max`.
fn rig<N: tutti_graph::IntoNode>(
    source: N,
    ports: u16,
    max: usize,
) -> (Editor, Executor, N::Controls, Seen) {
    let (mut ed, exec) = Editor::new(Prepare::new(SampleRate(RATE), Samples(max)));
    let controls = ed.insert(NodeKey(1), "source", source);
    let seen = Seen::default();
    ed.insert(
        NodeKey(2),
        "sink",
        tutti_graph::ForkByClone(Sink {
            ports,
            seen: Arc::clone(&seen),
        }),
    );
    for p in 0..ports {
        ed.spec_mut().connect_events(
            EventIn {
                node: NodeKey(2),
                port: p,
            },
            EventEdge::Direct(EventOut {
                node: NodeKey(1),
                port: p,
            }),
        );
    }
    ed.spec_mut().topology.outputs = vec![tutti_core::graph::Source::Node(
        tutti_core::graph::OutPort {
            node: NodeKey(2),
            port: 0,
        },
    )];
    ed.commit().expect("commits");
    (ed, exec, controls, seen)
}

fn block(exec: &mut Executor, len: usize) {
    let mut out = vec![0.0f32; len];
    exec.process(len, &Transport::default(), &[], &mut [&mut out[..]]);
}

/// A wire that hands out one scripted batch per poll.
struct Wire(Mutex<std::collections::VecDeque<Vec<MidiEvent>>>);

impl Wire {
    fn new(batches: Vec<Vec<MidiEvent>>) -> Arc<Self> {
        Arc::new(Self(Mutex::new(batches.into())))
    }
}

impl MidiIn for Wire {
    fn poll_block(&self, _block_size: usize, buffer: &mut [MidiEvent]) -> usize {
        let batch = self.0.lock().unwrap().pop_front().unwrap_or_default();
        let n = batch.len().min(buffer.len());
        buffer[..n].copy_from_slice(&batch[..n]);
        n
    }
}

fn note(channel: u8, note: u8, offset: u32) -> MidiEvent {
    MidiEvent::note_on_7bit(MidiGroup::FIRST, MidiChannel::new(channel), note, 100)
        .with_frame_offset(offset)
}

/// A MIDI 1.0 wire CC (status 0xB0 | channel).
fn cc_ev(channel: u8, control: CCNumber, value: u8) -> MidiEvent {
    MidiEvent::from_midi1_bytes(0, &[0xB0 | channel, control.get(), value]).unwrap()
}

/// **Each event leaves on its channel's port, in offset order.** A note on
/// channel 9 before one on channel 0 in arrival order but after it in time,
/// and a Timing Clock (no channel): channel 0's on port 0 at 10, channel
/// 9's on port 9 at 40, the clock on the channelless port at 20.
///
/// Mutation: send everything out of port 0 → fails. (The sort is pinned by
/// `the_queue_sends_in_offset_order`: here each port holds one event.)
#[test]
fn each_event_leaves_on_its_channels_port() {
    let clock = MidiEvent::timing_clock(MidiGroup::FIRST).with_frame_offset(20);
    let wire = Wire::new(vec![vec![note(9, 64, 40), note(0, 60, 10), clock]]);
    let (_ed, mut exec, _c, seen) =
        rig(MidiInputNode::new(Some(wire)), MIDI_INPUT_PORTS as u16, 512);
    block(&mut exec, 512);
    let got: Vec<(usize, u64)> = seen.lock().unwrap().iter().map(|s| (s.0, s.1)).collect();
    assert_eq!(got, vec![(0, 10), (9, 40), (CHANNELLESS_PORT, 20)]);
}

/// **An (N)RPN run split across blocks assembles into one controller.**
/// The parameter select arrives in one block, its Data Entry in the next:
/// one Registered Controller arrives, on channel 3's port.
///
/// Mutation: a fresh translator each block → the Data Entry passes as a
/// plain CC → fails.
#[test]
fn an_rpn_run_split_across_blocks_is_assembled() {
    let wire = Wire::new(vec![
        vec![cc_ev(3, cc::RPN_MSB, 0), cc_ev(3, cc::RPN_LSB, 6)],
        vec![cc_ev(3, cc::DATA_ENTRY, 10)],
    ]);
    let (_ed, mut exec, _c, seen) =
        rig(MidiInputNode::new(Some(wire)), MIDI_INPUT_PORTS as u16, 512);
    block(&mut exec, 512);
    block(&mut exec, 512);
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 1, "{seen:?}");
    let (port, frame, ev) = seen[0];
    assert_eq!((port, frame), (3, 512));
    // MIDI 2.0 channel voice (type 4), Registered Controller (status 0x2).
    assert_eq!((ev.data[0] >> 28, (ev.data[0] >> 20) & 0xF), (0x4, 0x2));
}

/// **A requested MPE mode is adopted once and holds.** With the lower zone
/// requested, a note on member channel 1 binds it; a channel bend there in
/// each of the next two blocks folds into a per-note bend on the note. The
/// request latches, so it is read every block.
///
/// Mutation: adopt without comparing against the mode in force (`set_mode`
/// every block) → the voice map resets → the bends resolve to nothing and
/// are absorbed → fails.
#[test]
fn a_requested_mpe_mode_is_adopted_once() {
    let member = MidiChannel::new(1);
    let bend = MidiEvent::pitch_bend(MidiGroup::FIRST, member, 0xC000_0000);
    let wire = Wire::new(vec![
        vec![MidiEvent::note_on_7bit(MidiGroup::FIRST, member, 60, 100)],
        vec![bend],
        vec![bend],
    ]);
    let (_ed, mut exec, controls, seen) =
        rig(MidiInputNode::new(Some(wire)), MIDI_INPUT_PORTS as u16, 512);
    let mode = MpeMode::LowerZone(MpeZoneConfig::lower(6));
    controls.set_mpe_mode(mode);
    assert_eq!(controls.pending_mpe_mode(), Some(mode));
    for _ in 0..3 {
        block(&mut exec, 512);
    }
    let seen = seen.lock().unwrap();
    let folded: Vec<Option<u8>> = seen[1..].iter().map(|s| s.2.note()).collect();
    assert_eq!(folded, vec![Some(60), Some(60)], "{seen:?}");
}

/// **An input node's fork reads no wire.**
///
/// Mutation: the fork polling the live wire → it takes the batch → fails.
#[test]
fn an_input_nodes_fork_is_silent() {
    let wire = Wire::new(vec![vec![note(0, 60, 0)]]);
    let (live, _exec, _c, seen) = rig(MidiInputNode::new(Some(wire)), MIDI_INPUT_PORTS as u16, 512);
    let prepare = *live.prepare();
    let (_fork, mut fork_exec) = live
        .fork(ForkTarget::Master, ForkMode::Live, prepare)
        .expect("forks");
    block(&mut fork_exec, 512);
    assert!(seen.lock().unwrap().is_empty());
}

/// **The queue sends what was pushed, on its offsets, in order.** Pushed
/// out of order, with one offset past the block: 3 first, then the late one
/// on the block's last frame.
///
/// Mutation (run): skip the sort → the writer refuses the second push →
/// fails.
#[test]
fn the_queue_sends_in_offset_order() {
    let (_ed, mut exec, keys, seen) = rig(MidiQueueNode::new(), 1, 512);
    keys.queue(&[note(0, 60, 700), note(0, 62, 3)]);
    block(&mut exec, 512);
    let got: Vec<(u64, Option<u8>)> = seen
        .lock()
        .unwrap()
        .iter()
        .map(|s| (s.1, s.2.note()))
        .collect();
    assert_eq!(got, vec![(3, Some(62)), (511, Some(60))]);
}

/// **The out node hands MIDI back on the graph's clock, and counts what a
/// full ring drops.** Five blocks of 256 queued notes into a ring of 1 024
/// nobody drains: 256 dropped; what is drained is the first 1 024, the
/// first on its frame (block 0, offset 7) and the last of them on block 3's
/// (frame 1 536 plus offset 8).
///
/// Mutation (run): not counting a refused push → `dropped` stays 0 →
/// fails. Mutation (run): stamping the offset in the block alone → the last
/// reads 8 → fails.
#[test]
fn the_out_node_hands_midi_back_and_counts_drops() {
    let (mut ed, mut exec) = Editor::new(Prepare::new(SampleRate(RATE), Samples(512)));
    let keys = ed.insert(NodeKey(1), "keys", MidiQueueNode::new());
    let out = ed.insert(NodeKey(2), "out", MidiOutNode::new());
    ed.spec_mut().connect_events(
        EventIn {
            node: NodeKey(2),
            port: 0,
        },
        EventEdge::Direct(EventOut {
            node: NodeKey(1),
            port: 0,
        }),
    );
    ed.commit().unwrap();
    for _ in 0..5 {
        let batch: Vec<MidiEvent> = (0..256).map(|i| note(0, 60, 7 + i / 128)).collect();
        assert_eq!(keys.queue(&batch), 256);
        exec.process(512, &Transport::default(), &[], &mut []);
    }
    assert_eq!(out.dropped(), 256);
    let mut buf = vec![MidiEvent::noop(); 2048];
    assert_eq!(out.poll_into(&mut buf), 1024);
    assert_eq!(buf[0].frame_offset, 7);
    assert_eq!(buf[1023].frame_offset, 3 * 512 + 8);
}

/// **A start inside a block clocks from its frame.** Stopped at beat 0 for
/// 100 frames, then rolling at 120 BPM: Start and the first tick at frame
/// 100, the next tick 1 000 frames on (a 24-PPQN tick at 48 kHz, the rate
/// `prepare` gave the master).
///
/// Mutation: tick once per block with the block's first transport (not per
/// segment) → stopped all block, nothing sent → fails. Mutation: `prepare`
/// not setting the rate → the master has rate 0 and sends nothing → fails.
#[test]
fn a_start_inside_a_block_clocks_from_its_frame() {
    let (_ed, mut exec, clock, seen) = rig(ClockNode::new(), 1, 2048);
    clock.set_enabled(true);
    let stopped = Transport::new(false, Bpm(120.0), Beat(0.0), None);
    let rolling = Transport::new(true, Bpm(120.0), Beat(0.0), None);
    let mut changes = TransportChanges::NONE;
    changes
        .push(Offset::new(100, Samples(2048)).unwrap(), rolling)
        .unwrap();
    let mut out = vec![0.0f32; 2048];
    exec.process_with_changes(2048, &stopped, &changes, &[], &mut [&mut out[..]]);
    let status = |e: &MidiEvent| ((e.data[0] >> 16) & 0xFF) as u8;
    let got: Vec<(u64, u8)> = seen
        .lock()
        .unwrap()
        .iter()
        .map(|s| (s.1, status(&s.2)))
        .collect();
    assert_eq!(got, vec![(100, 0xFA), (100, 0xF8), (1_100, 0xF8)]);
}

/// **A clock's fork sends nothing.**
///
/// Mutation (run): the fork a node over the live master → it sends Start →
/// fails.
#[test]
fn a_clocks_fork_sends_nothing() {
    let (live, _exec, clock, seen) = rig(ClockNode::new(), 1, 512);
    clock.set_enabled(true);
    let prepare = *live.prepare();
    let (_fork, mut fork_exec) = live
        .fork(ForkTarget::Master, ForkMode::Live, prepare)
        .expect("forks");
    let mut out = vec![0.0f32; 512];
    fork_exec.process(
        512,
        &Transport::new(true, Bpm(120.0), Beat(0.0), None),
        &[],
        &mut [&mut out[..]],
    );
    assert!(seen.lock().unwrap().is_empty());
}
