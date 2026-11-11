//! The nodes at the graph's MIDI border:
//! [`MidiInputNode`] sends a wire's events out of their channels' ports,
//! translated; [`MidiOutNode`] hands what reaches it back to one; [`ClockNode`] ticks
//! Beat Clock on the frames the transport reaches, a start inside a block
//! included.

use std::sync::{Arc, Mutex};
use tutti_graph::UmpAt;

use tutti_fixtures::midi::{
    continue_msg, m1_cc, note_off, note_on, pitch_bend, registered_rpn, song_position, start,
    timing_clock,
};
use tutti_graph::NodeKey;
use tutti_graph::{
    Cx, Editor, EventEdge, EventIn, EventInletNode, EventKind, EventOut, Executor, ForkMode,
    ForkTarget, Io, Node, Offset, Prepare, Shape, Status, Transport, TransportChanges,
};
use tutti_midi_protocol::channel_voice;
use tutti_midi_runtime::{
    CHANNELLESS_PORT, ClockNode, MIDI_INPUT_EVENT_CAPACITY, MIDI_INPUT_PORTS, MidiIn,
    MidiInputNode, MidiOutNode,
};
use tutti_types::{At, Beat, Bpm, Frame, SampleRate, Samples};
use tutti_ump::{
    CCNumber, KeyEvent, Message, MidiChannel, MidiGroup, NoteNumber, U7, Ump, Voice2, decode,
};

/// The note a channel voice message addresses, either protocol.
fn note_of(ump: Ump) -> Option<u8> {
    channel_voice(ump)?.1.note().map(NoteNumber::get)
}

const G: MidiGroup = MidiGroup::FIRST;

const RATE: f64 = 48_000.0;

/// `(sink port, absolute frame, event)` of every MIDI event a sink received.
type Seen = Arc<Mutex<Vec<(usize, u64, Ump)>>>;

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
            tutti_types::ChannelLayout::EMPTY,
            tutti_types::ChannelLayout::MONO,
        )
        .with_events(self.ports, 0)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let mut seen = self.seen.lock().unwrap();
        for p in 0..usize::from(self.ports) {
            for e in io.events(p) {
                if let EventKind::Midi(Ump(data)) = e.kind {
                    seen.push((p, cx.env.frame.get() + u64::from(e.offset.get()), Ump(data)));
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
    ed.spec_mut()
        .topology
        .set_outputs(vec![tutti_graph::Source::Node(tutti_graph::OutPort {
            node: NodeKey(2),
            port: 0,
        })]);
    ed.commit().expect("commits");
    (ed, exec, controls, seen)
}

fn block(exec: &mut Executor, len: usize) {
    let mut out = vec![0.0f32; len];
    exec.process(len, &Transport::default(), &[], &mut [&mut out[..]]);
}

/// A wire that hands out one scripted batch per poll.
struct Wire(std::collections::VecDeque<Vec<UmpAt>>);

impl Wire {
    fn new(batches: Vec<Vec<UmpAt>>) -> Box<Self> {
        Box::new(Self(batches.into()))
    }
}

impl MidiIn for Wire {
    fn poll_block(&mut self, _block_size: usize, buffer: &mut [UmpAt]) -> usize {
        let batch = self.0.pop_front().unwrap_or_default();
        let n = batch.len().min(buffer.len());
        buffer[..n].copy_from_slice(&batch[..n]);
        n
    }
}

/// `ump` at frame `offset`, which may lie past the 512-frame blocks these
/// tests run: it is built against a block long enough to hold it.
fn at(offset: u32, ump: Ump) -> UmpAt {
    (Offset::new(offset as usize, Samples(1 << 20)).unwrap(), ump)
}

/// A note-on with 7-bit velocity 100, widened (M2-115), at `offset`.
fn note(channel: u8, note: u8, offset: u32) -> UmpAt {
    at(
        offset,
        note_on(
            MidiGroup::FIRST,
            MidiChannel::of(channel),
            note,
            tutti_fixtures::midi::wide_velocity(100),
        ),
    )
}

/// A MIDI 1.0 CC.
fn cc_ev(channel: u8, control: CCNumber, value: u8) -> UmpAt {
    at(0, m1_cc(MidiChannel::of(channel), control, U7::of(value)))
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
    let clock = at(20, timing_clock(MidiGroup::FIRST));
    let wire = Wire::new(vec![vec![note(9, 64, 40), note(0, 60, 10), clock]]);
    let (_ed, mut exec, _c, seen) =
        rig(MidiInputNode::new(Some(wire)), MIDI_INPUT_PORTS as u16, 512);
    block(&mut exec, 512);
    let got: Vec<(usize, u64)> = seen.lock().unwrap().iter().map(|s| (s.0, s.1)).collect();
    assert_eq!(got, vec![(0, 10), (9, 40), (CHANNELLESS_PORT, 20)]);
}

/// **An (N)RPN run split across blocks assembles into one controller.**
/// The parameter select arrives in one block, its Data Entry (CC 6, then the
/// CC 38 that sends it, M2-104 D.3.3) in the next: one Registered Controller
/// arrives, on channel 3's port.
///
/// Mutation: a fresh translator each block → the Data Entry passes as a
/// plain CC → fails.
#[test]
fn an_rpn_run_split_across_blocks_is_assembled() {
    let wire = Wire::new(vec![
        vec![
            cc_ev(3, CCNumber::RPN_MSB, 0),
            cc_ev(3, CCNumber::RPN_LSB, 6),
        ],
        vec![
            cc_ev(3, CCNumber::DATA_ENTRY, 10),
            cc_ev(3, CCNumber::DATA_ENTRY_LSB, 0),
        ],
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
    assert!(
        matches!(decode(ev), Ok(Message::Midi2(_, Voice2::Registered { .. }))),
        "{ev:?}"
    );
}

/// **A requested MPE mode is adopted once and holds.** With the lower zone
/// requested, a note on member channel 1 binds it; a channel bend there in
/// each of the next two blocks folds into a per-note bend on the note. The
/// request latches, so it is read every block. Adopting it sends the zone's
/// default bend ranges once, in the first block (master and six members:
/// seven registered controllers).
///
/// Mutation: adopt without comparing against the mode in force (`set_mode`
/// every block) → the voice map resets → the bends resolve to nothing and
/// are absorbed, and the ranges go out every block → fails. Mutation
/// (run): adopt without sending the ranges → none go out → fails.
#[test]
fn a_requested_mpe_mode_is_adopted_once() {
    let member = MidiChannel::of(1);
    let bend = at(0, pitch_bend(MidiGroup::FIRST, member, 0xC000_0000));
    let wire = Wire::new(vec![
        vec![at(
            0,
            note_on(
                MidiGroup::FIRST,
                member,
                60,
                tutti_fixtures::midi::wide_velocity(100),
            ),
        )],
        vec![bend],
        vec![bend],
    ]);
    let (_ed, mut exec, controls, seen) =
        rig(MidiInputNode::new(Some(wire)), MIDI_INPUT_PORTS as u16, 512);
    let mode = tutti_fixtures::midi::lower_zone(6);
    controls.set_mpe_mode(mode);
    assert_eq!(controls.pending_mpe_mode(), Some(mode));
    for _ in 0..3 {
        block(&mut exec, 512);
    }
    let seen = seen.lock().unwrap();
    // MIDI 2.0 channel voice (type 4), Registered Controller (status 0x2).
    let is_rpn = |ev: &Ump| {
        matches!(
            decode(*ev),
            Ok(Message::Midi2(_, Voice2::Registered { .. }))
        )
    };
    let resets: Vec<u64> = seen.iter().filter(|s| is_rpn(&s.2)).map(|s| s.1).collect();
    assert_eq!(resets, vec![0; 7], "the ranges, once, at frame 0: {seen:?}");
    let music: Vec<_> = seen.iter().filter(|s| !is_rpn(&s.2)).collect();
    let folded: Vec<Option<u8>> = music[1..].iter().map(|s| note_of(s.2)).collect();
    assert_eq!(folded, vec![Some(60), Some(60)], "{seen:?}");
}

/// **A host-set mode's reset goes ahead of the block's own events.** The
/// lower zone of six members is requested, and the adopting block polls a
/// bend range (RPN #00/00, 12 semitones) on member 2 at frame 0. Both the
/// reset (±48 per member) and the wire's range (±12 per member) land on
/// frame 0; a receiver reading each member's port in order must end at the
/// wire's ±12, which came after the mode it was sent in.
///
/// Mutation (run): stage the reset after the poll loop → every member ends
/// at the reset's ±48 → fails.
#[test]
fn a_host_set_modes_reset_goes_ahead_of_the_blocks_events() {
    use tutti_midi_protocol::PitchBendSensitivity;
    use tutti_plugin_types::PerNoteBendRanges;
    use tutti_ump::Rpn;
    let range = at(
        0,
        registered_rpn(
            MidiGroup::FIRST,
            MidiChannel::of(2),
            Rpn::PITCH_BEND_SENSITIVITY,
            PitchBendSensitivity::of(12).to_rpn_bits(),
        ),
    );
    let wire = Wire::new(vec![vec![range]]);
    let (_ed, mut exec, controls, seen) =
        rig(MidiInputNode::new(Some(wire)), MIDI_INPUT_PORTS as u16, 512);
    controls.set_mpe_mode(tutti_fixtures::midi::lower_zone(6));
    block(&mut exec, 512);
    let seen = seen.lock().unwrap();
    let mut ranges = PerNoteBendRanges::default();
    let mut per_member = 0;
    for (_, frame, ev) in seen.iter() {
        if ranges.observe(ev) {
            assert_eq!(*frame, 0);
            per_member += 1;
        }
    }
    assert_eq!(
        per_member, 12,
        "the reset's six and the wire's six: {seen:?}"
    );
    for channel in 1..=6 {
        assert_eq!(
            ranges.semitones(tutti_ump::MidiChannel::of(channel)),
            12.0,
            "channel {channel}"
        );
    }
}

/// **A member channel's bend range reaches every member's port as a per-note
/// range, without losing the events polled after it.** With a lower zone of
/// seven members, a MIDI 1.0 RPN 0 run (12 semitones) on member 2 and then a
/// note on member 1: ports 1 to 7 each receive RPN #00/07 at 12 semitones,
/// and port 1 the note too.
///
/// Mutation (run): the node writes translated events back into its poll
/// buffer, as it did when each event made at most one → the seven-event
/// fan-out overwrites the unread note → fails.
#[test]
fn a_member_bend_range_fans_out_over_the_zones_ports() {
    use tutti_plugin_types::PerNoteBendRanges;
    let wire = Wire::new(vec![vec![
        cc_ev(2, CCNumber::RPN_MSB, 0),
        cc_ev(2, CCNumber::RPN_LSB, 0),
        cc_ev(2, CCNumber::DATA_ENTRY, 12),
        cc_ev(2, CCNumber::DATA_ENTRY_LSB, 0),
        note(1, 60, 5),
    ]]);
    let node = MidiInputNode::new(Some(wire)).with_mpe(tutti_fixtures::midi::lower_zone(7));
    let (_ed, mut exec, _c, seen) = rig(node, MIDI_INPUT_PORTS as u16, 512);
    block(&mut exec, 512);
    let seen = seen.lock().unwrap();
    let mut ranges = PerNoteBendRanges::default();
    let mut range_ports = Vec::new();
    for (port, _, ev) in seen.iter() {
        if ranges.observe(ev) {
            assert_eq!(ev.channel().map(u8::from), Some(*port as u8));
            range_ports.push(*port);
        }
    }
    assert_eq!(range_ports, (1..=7).collect::<Vec<_>>(), "{seen:?}");
    for channel in 1..=7 {
        assert_eq!(
            ranges.semitones(tutti_ump::MidiChannel::of(channel)),
            12.0,
            "channel {channel}"
        );
    }
    let notes: Vec<(usize, u64)> = seen
        .iter()
        .filter(|s| matches!(s.2.key_event(), Some(KeyEvent::On(_))))
        .map(|s| (s.0, s.1))
        .collect();
    assert_eq!(notes, vec![(1, 5)]);
}

/// **A translation with no room is dropped whole, and counted.** With a
/// lower zone of seven members, a block polls 507 notes on the master, a
/// MIDI 1.0 RPN 0 run (12 semitones, sent at its CC 38) on member 2, and a
/// note on member 1: 512 polled events. The run fans out to seven, which do
/// not fit after the 507 notes (512 slots), so all seven are dropped and
/// counted, and the note after them still goes out: 508 events, no per-note
/// range.
///
/// Mutation (run): stage each translated event while a slot is free (the
/// fan-out cut partway, as before) → four ranges go out and the note is
/// dropped → fails. Mutation (run): leave the counter out → 0 → fails.
#[test]
fn a_fan_out_with_no_room_is_dropped_whole_and_counted() {
    use tutti_plugin_types::PerNoteBendRanges;
    let mut batch: Vec<UmpAt> = (0..507).map(|i| note(0, 60, i)).collect();
    batch.extend([
        cc_ev(2, CCNumber::RPN_MSB, 0),
        cc_ev(2, CCNumber::RPN_LSB, 0),
        cc_ev(2, CCNumber::DATA_ENTRY, 12),
        cc_ev(2, CCNumber::DATA_ENTRY_LSB, 0),
        note(1, 61, 509),
    ]);
    assert_eq!(batch.len(), MIDI_INPUT_EVENT_CAPACITY);
    let node = MidiInputNode::new(Some(Wire::new(vec![batch])))
        .with_mpe(tutti_fixtures::midi::lower_zone(7));
    let (_ed, mut exec, controls, seen) = rig(node, MIDI_INPUT_PORTS as u16, 512);
    block(&mut exec, 512);
    let seen = seen.lock().unwrap();
    let mut ranges = PerNoteBendRanges::default();
    assert!(
        !seen.iter().any(|s| ranges.observe(&s.2)),
        "no range went out"
    );
    assert_eq!(seen.len(), 508);
    assert!(seen.iter().any(|s| s.0 == 1 && note_of(s.2) == Some(61)));
    assert_eq!(controls.dropped_events(), 7);
}

/// **A note-off has room when a block is full of other messages.** 504
/// notes on the master, a bend-range run on member 2 (sent at its CC 38)
/// that fans out to seven (504 + 7 leave one of the 512 the other messages
/// may use) and four note-offs on
/// member 1: all four go out, in the room past the capacity, and nothing is
/// dropped or counted. A note-on in their place would be dropped.
///
/// Mutation (run): limit a note-off to the capacity like the others (`limit`
/// in `Staging::whole`) → the four note-offs are dropped and counted → fails.
#[test]
fn a_note_off_has_room_past_the_capacity() {
    let mut batch: Vec<UmpAt> = (0..504).map(|i| note(0, 60, i)).collect();
    batch.extend([
        cc_ev(2, CCNumber::RPN_MSB, 0),
        cc_ev(2, CCNumber::RPN_LSB, 0),
        cc_ev(2, CCNumber::DATA_ENTRY, 12),
        cc_ev(2, CCNumber::DATA_ENTRY_LSB, 0),
    ]);
    batch.extend((0..4).map(|k| {
        at(
            510 + k,
            note_off(MidiGroup::FIRST, MidiChannel::of(1), 61 + k as u8, 0),
        )
    }));
    assert_eq!(batch.len(), MIDI_INPUT_EVENT_CAPACITY);
    let node = MidiInputNode::new(Some(Wire::new(vec![batch])))
        .with_mpe(tutti_fixtures::midi::lower_zone(7));
    let (_ed, mut exec, controls, seen) = rig(node, MIDI_INPUT_PORTS as u16, 512);
    block(&mut exec, 512);
    let seen = seen.lock().unwrap();
    assert_eq!(seen.iter().filter(|s| s.2.is_note_off()).count(), 4);
    // The run went out too: the block is full of other messages.
    assert_eq!(seen.len(), 504 + 7 + 4);
    assert_eq!(controls.dropped_events(), 0);
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
    let (inlet, mut keys) = EventInletNode::pair(256);
    ed.insert(NodeKey(1), "keys", inlet);
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
    for k in 0..5u64 {
        // The inlet holds each event for its frame: offset 7, then 8, in
        // block `k`.
        for i in 0..256u64 {
            let frame = Frame(512 * k + 7 + i / 128);
            keys.push(At::Frame(frame), note(0, 60, 0).1).unwrap();
        }
        exec.process(512, &Transport::default(), &[], &mut []);
    }
    assert_eq!(out.dropped(), 256);
    let mut buf = vec![(0, Ump([0; 4])); 2048];
    assert_eq!(out.poll_into(&mut buf), 1024);
    assert_eq!(buf[0].0, 7);
    assert_eq!(buf[1023].0, 3 * 512 + 8);
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
    let stopped = Transport::new(false, Bpm::of(120.0), Beat(0.0), None);
    let rolling = Transport::new(true, Bpm::of(120.0), Beat(0.0), None);
    let mut changes = TransportChanges::NONE;
    changes
        .push(Offset::new(100, Samples(2048)).unwrap(), rolling)
        .unwrap();
    let mut out = vec![0.0f32; 2048];
    exec.process_with_changes(2048, &stopped, &changes, &[], &mut [&mut out[..]]);
    let got: Vec<(u64, Ump)> = seen.lock().unwrap().iter().map(|s| (s.1, s.2)).collect();
    assert_eq!(
        got,
        vec![
            (100, start(G)),
            (100, timing_clock(G)),
            (1_100, timing_clock(G))
        ]
    );
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
        &Transport::new(true, Bpm::of(120.0), Beat(0.0), None),
        &[],
        &mut [&mut out[..]],
    );
    assert!(seen.lock().unwrap().is_empty());
}

/// **A block with more clock events than the port holds sends the first
/// [`CLOCK_EVENT_CAPACITY`](tutti_midi_runtime::CLOCK_EVENT_CAPACITY) and
/// counts the rest.**
///
/// 300 BPM with MTC on at 48 kHz is 120 ticks and 100 quarter-frames a
/// second, so one 65 536-frame block (1.37 s) produces more events than the
/// port's 256. The same span in four quarter-length blocks fits every event,
/// so it is the reference total: what one long block sent plus what it
/// counted must equal it, exactly.
///
/// Mutation (run): drop the `count_dropped` call (the old silent drop) → the
/// count stays 0 and the sum falls short → fails.
#[test]
fn a_clock_flood_past_the_port_capacity_is_counted() {
    use tutti_midi_runtime::CLOCK_EVENT_CAPACITY;
    const LONG: usize = 65_536;
    const BPM: f64 = 300.0;

    let run = |blocks: &[usize]| -> (usize, u64) {
        let (_ed, mut exec, clock, seen) = rig(ClockNode::new(), 1, LONG);
        clock.set_enabled(true);
        clock.set_send_mtc(true);
        let mut frame = 0usize;
        for &len in blocks {
            let beat = frame as f64 / RATE * BPM / 60.0;
            let rolling = Transport::new(true, Bpm::of(BPM), Beat(beat), None);
            let mut out = vec![0.0f32; len];
            exec.process(len, &rolling, &[], &mut [&mut out[..]]);
            frame += len;
        }
        let sent = seen.lock().unwrap().len();
        (sent, clock.dropped_events())
    };

    let (whole, none) = run(&[LONG / 4; 4]);
    assert_eq!(none, 0, "a quarter-length block fits every event");
    assert!(
        whole > CLOCK_EVENT_CAPACITY,
        "the span produces more than one block can carry ({whole})"
    );

    let (sent, dropped) = run(&[LONG]);
    assert_eq!(sent, CLOCK_EVENT_CAPACITY, "one block sends up to the cap");
    assert_eq!(
        sent as u64 + dropped,
        whole as u64,
        "every event past the cap is counted"
    );
}

/// **A transport change on the frame of a loop wrap sends one Song Position,
/// on that frame.** Looping beats 1.0..1.1 at 120 BPM and 48 kHz (24 000
/// frames a beat), the loop wraps on frame 2 400; a change of the transport
/// (to the same rolling one, at the loop's start) cuts the block there. The
/// first piece ends on the wrap, the second starts on the wrapped beat and
/// has no wrap of its own. Block start: Song Position, Continue (beat 1.0 is
/// no Start) and the tick; ticks every 1 000 frames; then on 2 400 the
/// wrap's Song Position and the tick on the loop's start.
///
/// Mutation (run): forget that a piece ended on a wrap (store `false` for
/// `prev.wrapped`) → no Song Position on frame 2 400 → fails.
#[test]
fn a_segment_boundary_on_a_wrap_sends_the_song_position_once() {
    use tutti_graph::LoopRange;
    // The loop starts at beat 1, a quarter note: 4 MIDI beats (sixteenths).
    const LOOP_START: u16 = 4;
    let (_ed, mut exec, clock, seen) = rig(ClockNode::new(), 1, 4096);
    clock.set_enabled(true);
    let looping = Some(LoopRange {
        start: Beat(1.0),
        end: Beat(1.1),
    });
    let rolling = Transport::new(true, Bpm::of(120.0), Beat(1.0), looping);
    let mut changes = TransportChanges::NONE;
    changes
        .push(Offset::new(2_400, Samples(4_000)).unwrap(), rolling)
        .unwrap();
    let mut out = vec![0.0f32; 4_000];
    exec.process_with_changes(4_000, &rolling, &changes, &[], &mut [&mut out[..]]);
    let got: Vec<(u64, Ump)> = seen.lock().unwrap().iter().map(|s| (s.1, s.2)).collect();
    assert_eq!(
        got,
        vec![
            (0, song_position(G, LOOP_START)),
            (0, continue_msg(G)),
            (0, timing_clock(G)),
            (1_000, timing_clock(G)),
            (2_000, timing_clock(G)),
            (2_400, song_position(G, LOOP_START)),
            (2_400, timing_clock(G)),
            (3_400, timing_clock(G)),
        ]
    );
}

/// **A short loop in a long block overflows the port, counted, and the
/// blocks it is cut into lose nothing.** A loop of 47.3 frames (beat 0 to
/// 47.3 / 24 000) at 120 BPM sends a Song Position and a tick on its start
/// at every wrap: about 340 wraps in 16 384 frames, so about 680 events
/// against the port's 256. One block sends the first 256 and counts the
/// rest, without panicking; the same span in blocks of 1 024 (each under the
/// cap) loses none, and what the one block sent plus what it counted is what
/// the blocks sent, exactly.
///
/// Mutation (run): drop the `count_dropped` call → the count stays 0 and the
/// sum falls short → fails.
#[test]
fn a_short_loop_in_a_long_block_is_counted_past_the_cap() {
    use tutti_graph::LoopRange;
    use tutti_midi_runtime::CLOCK_EVENT_CAPACITY;
    use tutti_types::FrameClock;
    const TOTAL: usize = 16_384;
    let end = 47.3 / 24_000.0;

    let run = |len: usize| -> (usize, u64) {
        let (_ed, mut exec, clock, seen) = rig(ClockNode::new(), 1, TOTAL);
        clock.set_enabled(true);
        let looping = Some(LoopRange {
            start: Beat(0.0),
            end: Beat(end),
        });
        let region = tutti_types::LoopRange::new(Beat::new(0.0), Beat::new(end));
        let mut host = FrameClock::new(Beat(0.0), Bpm::of(120.0), SampleRate(RATE));
        for _ in 0..TOTAL / len {
            let rolling = Transport::new(true, Bpm::of(120.0), host.beat(), looping);
            let mut out = vec![0.0f32; len];
            exec.process(len, &rolling, &[], &mut [&mut out[..]]);
            host.advance(Samples(len), region);
        }
        let sent = seen.lock().unwrap().len();
        (sent, clock.dropped_events())
    };

    let (cut, none) = run(1_024);
    assert_eq!(none, 0, "blocks of 1 024 fit every event");
    assert!(cut > 2 * CLOCK_EVENT_CAPACITY, "{cut} events in all");
    let (sent, dropped) = run(TOTAL);
    assert_eq!(sent, CLOCK_EVENT_CAPACITY);
    assert_eq!(sent as u64 + dropped, cut as u64);
}
