//! The by-hand drivers of `tutti_graph::contract` that hand a node what a
//! graph hands it: `drive_in` (one block under an `Env`, with events) and
//! `Direct` (block after block, events and a transport, without
//! allocating). A node's unit tests and allocation gates lean on them to
//! play MIDI and read the transport, so each promise is pinned here.

use tutti_graph::contract::{drive_in, Direct, Emitter};
use tutti_graph::{
    Cx, Env, Event, EventKind, Io, Node, Offset, Prepare, Shape, Status, Transport,
    TransportChanges, Ump,
};
use tutti_types::{Beat, Bpm, ChannelLayout, Frame, SampleRate, Samples};

const RATE: SampleRate = SampleRate(48_000.0);
const NOTE: EventKind = EventKind::Midi(Ump([0x2090_3c64, 0, 0, 0]));

/// One event input, two outputs: output 0 is 1.0 on each event's frame,
/// output 1 the transport's beat at each frame (`Env::transport_at`), and
/// every input event is echoed to the one event output.
struct Probe;

impl Node for Probe {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::STEREO).with_events(1, 1)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let events = io.events(0);
        for i in 0..io.frames() {
            io.output(0)[i] = 0.0;
            let at = Offset::new(i, cx.env.block_len).expect("inside");
            io.output(1)[i] = cx.env.transport_at(at).beat().get() as f32;
        }
        for e in events {
            io.output(0)[e.offset.index()] = 1.0;
            io.event_out(0).push(*e).expect("room");
        }
        Status::Modified
    }
    fn reset(&mut self) {}
}

fn at(k: usize, block: usize) -> Offset {
    Offset::new(k, Samples(block)).expect("inside the block")
}

/// `drive_in` plays each event on its frame, hands the node the `Env`'s
/// transport (rolling, so the beat moves across the block), and returns
/// what the node wrote to its event output.
///
/// Mutations (run): `drive_in` handing `SortedEvents::EMPTY` for every
/// input → no frame is marked → fails; building its `Cx` over a
/// `Transport::default()` env instead of the caller's → the beat reads 0 →
/// fails; dropping the event writers (`&mut []`) → the node's `event_out`
/// panics.
#[test]
fn drive_in_plays_events_under_the_env() {
    let env = Env {
        frame: Frame(1_000),
        sample_rate: RATE,
        block_len: Samples(64),
        transport: Transport::new(true, Bpm(120.0), Beat(4.0), None),
        changes: TransportChanges::NONE,
    };
    let events = [
        Event::midi(at(3, 64), [0x2090_3c64, 0, 0, 0]),
        Event::midi(at(40, 64), [0x2080_3c00, 0, 0, 0]),
    ];
    let got = drive_in(&mut Probe, &env, &[], &[], &[&events]);
    let marked: Vec<usize> = (0..64).filter(|&i| got.audio[0][i] == 1.0).collect();
    assert_eq!(marked, [3, 40]);
    assert_eq!(got.audio[1][0], 4.0);
    // 120 BPM at 48 kHz: 1/24 000 beat a frame.
    assert!((f64::from(got.audio[1][48]) - (4.0 + 48.0 / 24_000.0)).abs() < 1e-6);
    assert_eq!(got.events, vec![events.to_vec()]);
}

/// `Direct` plays events in the next block only, advances its frame block
/// by block (so a node placing something at an absolute frame finds it in
/// the right block), honours a shorter block length, and keeps the
/// transport it was given.
///
/// Mutations (run): not advancing `frame` after a block → the emitter at
/// frame 70 fires in block 0 never (its frame is never reached) → fails;
/// not clearing the event inputs after a block → the note sounds again in
/// block 1 → fails; `set_block_len` ignored → `output` is 64 long → fails.
#[test]
fn direct_plays_events_once_advances_and_rolls() {
    let mut probe = Direct::new(Probe, RATE, 64);
    probe.set_transport(Transport::new(true, Bpm(120.0), Beat(2.0), None));
    probe.events(0, &[Event::midi(at(5, 64), [0x2090_3c64, 0, 0, 0])]);
    probe.block();
    assert_eq!(probe.output(0)[5], 1.0);
    assert_eq!(probe.events_out(0).len(), 1);
    assert_eq!(probe.output(1)[0], 2.0);
    probe.block();
    assert!(probe.output(0).iter().all(|&s| s == 0.0), "played once");
    assert!(probe.events_out(0).is_empty());
    probe.set_block_len(16);
    probe.block();
    assert_eq!(probe.output(0).len(), 16);

    // The emitter writes at absolute frame 70: block 1 of 64-frame blocks,
    // offset 6.
    let mut emitter = Direct::new(Emitter::new(Frame(70), NOTE), RATE, 64);
    emitter.block();
    assert!(emitter.events_out(0).is_empty());
    emitter.block();
    assert_eq!(emitter.events_out(0).len(), 1);
    assert_eq!(emitter.events_out(0)[0].offset.index(), 6);
}
