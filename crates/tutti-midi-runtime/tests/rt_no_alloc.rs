//! Regression gate for RT-safety: the MIDI border nodes process a block
//! without allocating, events flowing — a device inlet's through the ingest
//! node's translation onto its ports, a control thread's through the event
//! inlet into the event tap's ring, and the clock's ticks.
//!
//! Mutation (run): a `Vec::new()` + push per block in `MidiIngestNode::process`
//! → the gate panics.

use assert_no_alloc::AllocDisabler;
use tutti_fixtures::midi;
use tutti_fixtures::midi::{note_off, registered_rpn};
use tutti_graph::NodeKey;
use tutti_graph::{
    Editor, EventEdge, EventIn, EventInletNode, EventInletWriter, EventOut, Harmony, LoopRange,
    Prepare, Transport,
};
use tutti_midi_runtime::{
    ClockNode, HarmonyNode, LIVE_INLET_CAPACITY, MidiClipNode, MidiIngestNode, MidiKeys,
    MidiMailbox, TimedHarmony, TimedMidiEvent,
};
use tutti_types::{At, Beat, Bpm, Frame, FrameClock, Moment, SampleRate, Samples};
use tutti_ump::{MidiChannel, MidiGroup, NoteNumber, U7, Ump};

mod common;

#[global_allocator]
static A: AllocDisabler = AllocDisabler;

fn note_on(ch: u8, note: u8) -> Ump {
    midi::note_on(
        MidiGroup::FIRST,
        MidiChannel::of(ch),
        note,
        midi::MID_VELOCITY,
    )
}

/// A device's inlet (key `inlet`) feeding the first event input of the ingest
/// node at key 1: what the device's thread pushes into.
fn device(ed: &mut Editor, inlet: u64) -> EventInletWriter {
    let (node, writer) = EventInletNode::pair(LIVE_INLET_CAPACITY);
    ed.insert(NodeKey(inlet), "device", node);
    ed.spec_mut().connect_events(
        EventIn {
            node: NodeKey(1),
            port: 0,
        },
        EventEdge::Direct(EventOut {
            node: NodeKey(inlet),
            port: 0,
        }),
    );
    writer
}

/// The first frame of block `block` (blocks of 512).
fn frame_of(block: u64, offset: u64) -> At {
    At::Frame(Frame(512 * block + offset))
}

/// A fresh note on four channels for block `block`.
fn busy(device: &mut EventInletWriter, block: u64) {
    for i in 0..4u64 {
        device
            .push_at(
                Moment::new(Frame(512 * block + 40 - 10 * i), SampleRate(48_000.0)),
                note_on(i as u8 * 3, 60),
            )
            .unwrap();
    }
}

#[test]
fn the_border_nodes_process_without_allocating() {
    let (mut ed, mut exec) = Editor::new(Prepare::new(SampleRate(48_000.0), Samples(512)));
    ed.insert(NodeKey(1), "in", MidiIngestNode::new(1));
    let mut dev = device(&mut ed, 4);
    let (inlet, writer) = EventInletNode::pair(LIVE_INLET_CAPACITY);
    ed.insert(NodeKey(2), "keys", inlet);
    let mut keys = MidiKeys::new(writer);
    let clock = ed.insert(NodeKey(3), "clock", ClockNode::new());
    let mut out = common::sink(&mut ed, 0, &[(1, 0), (1, 3), (2, 0), (3, 0)]);
    clock.set_enabled(true);
    ed.commit().unwrap();
    let t = Transport::new(true, Bpm::of(120.0), Beat(0.0), None);
    let mut drain = [(0, Ump([0; 4])); 256];
    // Warm up outside the gate.
    busy(&mut dev, 0);
    exec.process(512, &t, &[], &mut []);
    let _ = out.poll_into(&mut drain);

    let (tx, rx) = MidiMailbox::pair();
    assert_no_alloc::assert_no_alloc(|| {
        for b in 1..200u32 {
            busy(&mut dev, u64::from(b));
            keys.note_on(
                At::NextBlock,
                MidiChannel::FIRST,
                NoteNumber::of(60),
                0xC800,
            )
            .unwrap();
            let at = Beat(f64::from(b) * 512.0 / 24_000.0);
            exec.process(
                512,
                &Transport::new(true, Bpm::of(120.0), at, None),
                &[],
                &mut [],
            );
            let n = out.poll_into(&mut drain);
            // Both halves of a mailbox, too.
            tx.queue_stamped(&drain[..n.min(4)]);
            let _ = rx.poll_into(&mut drain);
        }
    });
    assert_eq!(out.dropped(), 0);
}

/// A bend range on MPE member channel 2, for the block starting at frame
/// `512 * block`.
fn bend_range(device: &mut EventInletWriter, block: u64) {
    device
        .push(
            frame_of(block, 0),
            registered_rpn(
                MidiGroup::FIRST,
                MidiChannel::of(2),
                tutti_ump::Rpn::PITCH_BEND_SENSITIVITY,
                tutti_midi_protocol::PitchBendSensitivity::of(12).to_rpn_bits(),
            ),
        )
        .unwrap();
}

/// The input node fans a member channel's bend range out over its zone
/// without allocating, and every member's port carries it.
///
/// Mutation (run): collect the fan-out into a `Vec` before writing it → the
/// gate panics.
#[test]
fn the_mpe_bend_range_fan_out_processes_without_allocating() {
    let (mut ed, mut exec) = Editor::new(Prepare::new(SampleRate(48_000.0), Samples(512)));
    ed.insert(
        NodeKey(1),
        "in",
        MidiIngestNode::new(1).with_mpe(tutti_fixtures::midi::lower_zone(15)),
    );
    let mut dev = device(&mut ed, 4);
    let mut out = common::sink(&mut ed, 0, &[(1, 15)]);
    ed.commit().unwrap();
    let t = Transport::default();
    let mut drain = [(0, Ump([0; 4])); 256];
    bend_range(&mut dev, 0);
    exec.process(512, &t, &[], &mut []);
    let _ = out.poll_into(&mut drain);

    let mut reached = 0;
    assert_no_alloc::assert_no_alloc(|| {
        for b in 1..=50 {
            bend_range(&mut dev, b);
            exec.process(512, &t, &[], &mut []);
            reached += out.poll_into(&mut drain);
        }
    });
    assert_eq!(reached, 50, "the last member's port carries one per block");
}

/// One MPE gesture on member channel 2 of a lower zone for block `block`: a
/// bend, pressure and CC 74 before the Note On, the manager's pressure, the
/// manager's CC 74, then the Note Off.
fn gesture(device: &mut EventInletWriter, block: u64) {
    let (g, ch) = (MidiGroup::FIRST, MidiChannel::of(2));
    for ev in [
        midi::pitch_bend(g, ch, 0xC000_0000),
        midi::channel_pressure(g, ch, 0x4000_0000),
        midi::cc(g, ch, tutti_ump::CCNumber::BRIGHTNESS, 0x2000_0000),
        note_on(2, 60),
        midi::channel_pressure(g, MidiChannel::FIRST, 0x6000_0000),
        midi::cc(g, MidiChannel::FIRST, tutti_ump::CCNumber::BRIGHTNESS, 7),
        note_off(g, ch, 60, 0),
    ] {
        device.push(frame_of(block, 0), ev).unwrap();
    }
}

/// The values a member was told before its Note On, the manager's pressure
/// and CC 74 combined into each held note, and a Note Off's release process
/// without allocating, block after block.
#[test]
fn the_mpe_expression_paths_process_without_allocating() {
    let (mut ed, mut exec) = Editor::new(Prepare::new(SampleRate(48_000.0), Samples(512)));
    ed.insert(
        NodeKey(1),
        "in",
        MidiIngestNode::new(1).with_mpe(tutti_fixtures::midi::lower_zone(15)),
    );
    let mut dev = device(&mut ed, 4);
    let mut out = common::sink(&mut ed, 0, &[(1, 2)]);
    ed.commit().unwrap();
    let t = Transport::default();
    let mut drain = [(0, Ump([0; 4])); 256];
    gesture(&mut dev, 0);
    exec.process(512, &t, &[], &mut []);
    let _ = out.poll_into(&mut drain);

    let mut reached = 0;
    assert_no_alloc::assert_no_alloc(|| {
        for b in 1..=50 {
            gesture(&mut dev, b);
            exec.process(512, &t, &[], &mut []);
            reached += out.poll_into(&mut drain);
        }
    });
    assert!(reached > 0, "channel 2's port carries the gesture");
}

/// An upper-zone MPE Configuration Message for block `block` when `upper`, a
/// lower-zone one otherwise.
fn mcm(device: &mut EventInletWriter, block: u64, upper: bool) {
    use tutti_midi_protocol::MpeZoneConfig;
    let message = if upper {
        MpeZoneConfig::upper(3).unwrap().to_mcm()
    } else {
        MpeZoneConfig::lower(3).unwrap().to_mcm()
    };
    device.push(frame_of(block, 0), message).unwrap();
}

/// The input node adopts a host-set MPE mode, sends its zones' reset ranges
/// and applies an MCM from the wire (rebuilding the ingest and sending that
/// zone's reset) without allocating: before each gated block the host asks
/// for a dual zone other than the one in force (outside the gate: a publish
/// is control-side), and the wire alternates upper and lower MCMs.
///
/// Mutation (run): collect `zone_defaults` into a `Vec` in
/// `adopt_mpe_request` → the gate panics.
#[test]
fn an_mpe_mode_change_and_an_mcm_process_without_allocating() {
    let (mut ed, mut exec) = Editor::new(Prepare::new(SampleRate(48_000.0), Samples(512)));
    let controls = ed.insert(NodeKey(1), "in", MidiIngestNode::new(1));
    let mut dev = device(&mut ed, 4);
    let mut out = common::sink(&mut ed, 0, &[(1, 15)]);
    ed.commit().unwrap();
    let t = Transport::default();
    let mut drain = [(0, Ump([0; 4])); 256];
    mcm(&mut dev, 0, true);
    exec.process(512, &t, &[], &mut []);
    let _ = out.poll_into(&mut drain);

    let modes = [
        tutti_fixtures::midi::dual_zones(3, 3),
        tutti_fixtures::midi::dual_zones(5, 4),
    ];
    let mut reached = 0;
    for i in 0..50 {
        controls.set_mpe_mode(modes[i % 2]);
        // The device's thread pushes the block's message: upper on the odd
        // blocks, as the first (warm-up) block's was.
        mcm(&mut dev, i as u64 + 1, i % 2 == 1);
        assert_no_alloc::assert_no_alloc(|| {
            exec.process(512, &t, &[], &mut []);
            reached += out.poll_into(&mut drain);
        });
        controls.reclaim();
    }
    // Each block's adoption resets the upper master (channel 15), and every
    // other block's upper MCM resets it again.
    assert_eq!(reached, 75);
}

/// A loop of 64 frames walked in 512-frame blocks: each block is eight loop
/// passes of a clip (a note on and off in each, the held note ended at each
/// wrap, a sustain pedal and an off-centre bend returned at each) and of a harmony node (a chord change in each, the chord re-stated
/// at each wrap), all without allocating, block after block.
///
/// Mutation (run): a `Vec` collected per pass in the walk's `pass` → the
/// gate panics.
#[test]
fn a_block_of_many_loop_passes_walks_without_allocating() {
    const RATE: f64 = 48_000.0;
    let beat = |frame: f64| Beat(frame / 24_000.0);
    let (start, end) = (beat(1_000.0), beat(1_064.0));
    let (mut ed, mut exec) = Editor::new(Prepare::new(SampleRate(RATE), Samples(512)));
    ed.insert(
        NodeKey(1),
        "clip",
        MidiClipNode::new([
            TimedMidiEvent::new(beat(1_010.0), note_on(0, 60)),
            TimedMidiEvent::new(beat(1_040.0), note_on(0, 64)),
            // Held across every wrap, so each wrap lifts the pedal and
            // centres the bend.
            TimedMidiEvent::new(
                beat(1_020.0),
                midi::cc(
                    MidiGroup::FIRST,
                    MidiChannel::FIRST,
                    tutti_ump::CCNumber::SUSTAIN,
                    u32::MAX,
                ),
            ),
            TimedMidiEvent::new(
                beat(1_030.0),
                midi::pitch_bend(MidiGroup::FIRST, MidiChannel::FIRST, 0xA000_0000),
            ),
        ]),
    );
    ed.insert(
        NodeKey(2),
        "harmony",
        HarmonyNode::new([
            TimedHarmony::new(Beat(0.0), Harmony::chord(60, 60, 0b1001_0001)),
            TimedHarmony::new(beat(1_032.0), Harmony::chord(67, 67, 0b1001_0001)),
        ]),
    );
    let mut out = common::sink(&mut ed, 0, &[(1, 0), (2, 0)]);
    ed.commit().unwrap();
    let region = tutti_types::LoopRange::new(start, end).unwrap();
    let looping = Some(LoopRange { start, end });
    let mut clock = FrameClock::new(start, Bpm::of(120.0), SampleRate(RATE));
    let mut drain = [(0, Ump([0; 4])); 256];
    let mut block = |exec: &mut tutti_graph::Executor, clock: &mut FrameClock| {
        exec.process(
            512,
            &Transport::new(true, Bpm::of(120.0), clock.beat(), looping),
            &[],
            &mut [],
        );
        clock.advance(Samples(512), Some(region));
        out.poll_into(&mut drain)
    };
    // Warm up outside the gate; the clip sends its notes.
    assert!(
        block(&mut exec, &mut clock) >= 16,
        "eight passes of two notes"
    );

    let mut sent = 0;
    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..100 {
            sent += block(&mut exec, &mut clock);
        }
    });
    // Per pass the clip sends two note-ons, the pedal and the bend, then at the
    // wrap two note-offs, the pedal-up and the centred bend.
    assert!(sent >= 100 * 8 * 8, "every block walked its passes: {sent}");
    assert_eq!(out.dropped(), 0);
}

/// A clip that overflows its port, refuses note-offs and retries them, in
/// blocks the loss log and its ring are written in, without allocating: 300
/// notes on a 64-frame block's port of 256 (44 note-ons refused, their
/// note-offs withheld), then 200 notes ended and 200 started at once (104
/// note-offs refused, owed, retried on the next block).
///
/// Mutation (run): a `Vec` allocated in `Sending::on`'s withhold branch →
/// the gate panics. (The loss log's own full path is gated in tutti-graph's
/// `rt_no_alloc`, `losses_past_the_log_and_the_ring_do_not_allocate`.)
#[test]
fn a_clip_overflowing_its_port_refuses_and_retries_without_allocating() {
    const RATE: f64 = 48_000.0;
    let beat = |frame: f64| Beat(frame / 24_000.0);
    let note = |i: u32| note_on((i / 128 % 16) as u8, (i % 128) as u8);
    let off = |i: u32| {
        note_off(
            MidiGroup::FIRST,
            MidiChannel::of((i / 128 % 16) as u8),
            (i % 128) as u8,
            0,
        )
    };
    let mut events = Vec::new();
    for i in 0..300 {
        events.push(TimedMidiEvent::new(beat(10.0), note(i)));
        events.push(TimedMidiEvent::new(beat(500.0), off(i)));
    }
    // 200 notes that start where 200 others end, in one block.
    for i in 300..500 {
        events.push(TimedMidiEvent::new(beat(1_000.0), note(i - 300 + 1_000)));
        events.push(TimedMidiEvent::new(beat(1_100.0), off(i - 300 + 1_000)));
    }
    for i in 0..200 {
        events.push(TimedMidiEvent::new(beat(1_040.0), note(i + 2_000)));
        events.push(TimedMidiEvent::new(beat(1_041.0), off(i + 2_000)));
    }
    let (mut ed, mut exec) = Editor::new(Prepare::new(SampleRate(RATE), Samples(64)));
    ed.insert(NodeKey(1), "clip", MidiClipNode::new(events));
    let mut out = common::sink(&mut ed, 0, &[(1, 0)]);
    ed.commit().unwrap();
    let mut drain = [(0, Ump([0; 4])); 1024];
    let mut polled = 0;
    let mut f = 0u64;
    let mut block = |exec: &mut tutti_graph::Executor| {
        exec.process(
            64,
            &Transport::new(true, Bpm::of(120.0), beat(f as f64), None),
            &[],
            &mut [],
        );
        f += 64;
        out.poll_into(&mut drain)
    };
    // The first block outside the gate: lazy state, if any.
    polled += block(&mut exec);
    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..24 {
            polled += block(&mut exec);
        }
    });
    assert!(polled > 0);
    assert!(
        exec.dropped_event_count() >= 88,
        "{} refused",
        exec.dropped_event_count()
    );
}

/// The ledger on every path, with a full table: 2,100 notes sound at once
/// across two groups against 2,048 keys (the last 52 are withheld), all end
/// on one frame into a port that takes 584 of them (the rest are owed and
/// paid over the next blocks), a seek (every held note ended again) and a
/// stop. None allocates.
///
/// Mutation (run): a `Vec` collected in `NoteLedger::pay` → the gate panics.
/// A `Vec` in `Sending::on`'s full-table branch → the gate panics.
#[test]
fn a_clip_with_a_full_table_sends_and_pays_without_allocating() {
    let beat = |frame: u64| Beat(frame as f64 / 24_000.0);
    let key = |i: u32| {
        (
            MidiGroup::of((i / 2_048) as u8),
            MidiChannel::of((i % 2_048 / 128) as u8),
            (i % 128) as u8,
        )
    };
    let events = (0..2_100u32).flat_map(|i| {
        let (g, c, n) = key(i);
        [
            TimedMidiEvent::new(
                beat(u64::from(i)),
                tutti_fixtures::midi::note_on(g, c, n, tutti_fixtures::midi::MID_VELOCITY),
            ),
            TimedMidiEvent::new(beat(4_000), note_off(g, c, n, 0)),
        ]
    });
    let (mut ed, mut exec) = Editor::new(Prepare::new(SampleRate(48_000.0), Samples(512)));
    ed.insert(NodeKey(1), "clip", MidiClipNode::new(events));
    let mut out = common::sink(&mut ed, 0, &[(1, 0)]);
    ed.commit().unwrap();
    let mut drain = [(0, Ump([0; 4])); 1024];
    let mut f = 0u64;
    // Note-ons less note-offs the sink was sent.
    let mut sounding = 0i64;
    let mut block = |exec: &mut tutti_graph::Executor, rolling: bool, frame: u64| {
        let t = Transport::new(rolling, Bpm::of(120.0), beat(frame), None);
        exec.process(512, &t, &[], &mut []);
        let n = out.poll_into(&mut drain);
        for e in &drain[..n] {
            match e.1.key_event() {
                Some(tutti_ump::KeyEvent::On(_)) => sounding += 1,
                Some(tutti_ump::KeyEvent::Off(_)) => sounding -= 1,
                _ => {}
            }
        }
        n
    };
    let mut polled = block(&mut exec, true, f);
    assert_no_alloc::assert_no_alloc(|| {
        // The table fills over blocks 0 to 4; the offs at frame 4,000 are in
        // block 7, which refuses most of them.
        for _ in 0..7 {
            f += 512;
            polled += block(&mut exec, true, f);
        }
        // The refused offs are paid over the next blocks.
        for _ in 0..8 {
            f += 512;
            polled += block(&mut exec, true, f);
        }
        // A seek back: the table is empty, the notes start again.
        for k in 0..8 {
            polled += block(&mut exec, true, 512 * k);
        }
        // A jump while the table is full, then a stop.
        polled += block(&mut exec, true, 0);
        polled += block(&mut exec, false, 0);
        polled += block(&mut exec, false, 0);
    });
    assert!(polled > 0);
    assert_eq!(out.dropped(), 0, "the sink's ring took every event");
    assert_eq!(sounding, 0, "every note the sink heard begin has ended");
    // The table filled, offs were refused, and some were paid late.
    assert!(exec.dropped_event_count() > 52 + 1_400);
    assert!(exec.lost_event_count() < exec.dropped_event_count());
}

/// The event inlet's audio side takes events from its ring, orders them, and
/// sends them out of its port without allocating. The sends stay outside the
/// gate: they are the control thread's.
///
/// Mutation (run): a `Vec::with_capacity` per block in
/// `EventInletNode::process` aborts the gate.
#[test]
fn the_event_inlet_processes_without_allocating() {
    let (mut ed, mut exec) = Editor::new(Prepare::new(SampleRate(48_000.0), Samples(512)));
    let (queue, mut tx) = EventInletNode::pair(LIVE_INLET_CAPACITY);
    ed.insert(NodeKey(1), "timed", queue);
    let mut out = common::sink(&mut ed, 0, &[(1, 0)]);
    ed.commit().unwrap();
    let t = Transport::new(true, Bpm::of(120.0), Beat(0.0), None);
    let mut drain = [(0, Ump([0; 4])); 256];
    exec.process(512, &t, &[], &mut []);
    let _ = out.poll_into(&mut drain);

    let msg = midi::m1_note_on(MidiChannel::FIRST, NoteNumber::of(60), U7::of(100));
    let mut heard = 0;
    for b in 1..200u64 {
        // Control side, outside the gate: one for this block's middle, one
        // already late, one for the next block.
        tx.push(At::Frame(Frame(b * 512 + 200)), msg).unwrap();
        tx.push(At::Frame(Frame(3)), msg).unwrap();
        tx.push(At::NextBlock, msg).unwrap();
        // The audio thread's part, inside the gate.
        assert_no_alloc::assert_no_alloc(|| {
            exec.process(512, &t, &[], &mut []);
            heard += out.poll_into(&mut drain);
        });
    }
    assert_eq!(out.dropped(), 0);
    // Every send but the last block's own frame-stamped one was heard.
    assert!(heard >= 199 * 2, "heard only {heard}");
}

/// The full-list path allocates nothing either: with the waiting list full of
/// far-future events, each block takes a due event (it displaces the latest that
/// may be lost), a far note-off (it displaces a note-on) and a later note-on (it
/// is the one dropped), every block's processing inside the gate.
///
/// Mutation (run): a `Vec::new()` + push in `keep`'s full-list branch aborts the
/// gate.
#[test]
fn the_event_inlet_with_a_full_list_processes_without_allocating() {
    let (mut ed, mut exec) = Editor::new(Prepare::new(SampleRate(48_000.0), Samples(512)));
    let (queue, mut tx) = EventInletNode::pair(LIVE_INLET_CAPACITY);
    ed.insert(NodeKey(1), "timed", queue);
    let mut out = common::sink(&mut ed, 0, &[(1, 0)]);
    ed.commit().unwrap();
    let t = Transport::new(true, Bpm::of(120.0), Beat(0.0), None);
    let mut drain = [(0, Ump([0; 4])); 256];
    let on = midi::m1_note_on(MidiChannel::FIRST, NoteNumber::of(60), U7::of(100));
    let off = midi::m1_note_off(MidiChannel::FIRST, NoteNumber::of(60), U7::of(0));
    // Outside the gate: fill the list with events for frames far ahead.
    for k in 0..256u64 {
        tx.push(At::Frame(Frame(10_000_000 + k)), on).unwrap();
    }
    exec.process(512, &t, &[], &mut []);
    let _ = out.poll_into(&mut drain);

    for b in 1..100u64 {
        // Control side, outside the gate.
        tx.push(At::NextBlock, on).unwrap();
        tx.push(At::Frame(Frame(20_000_000 + b)), off).unwrap();
        tx.push(At::Frame(Frame(30_000_000 + b)), on).unwrap();
        assert_no_alloc::assert_no_alloc(|| {
            exec.process(512, &t, &[], &mut []);
            let _ = out.poll_into(&mut drain);
        });
    }
    assert!(tx.evicted() >= 99 * 2, "dropped only {}", tx.evicted());
}
