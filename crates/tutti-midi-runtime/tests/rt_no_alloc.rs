//! Regression gate for RT-safety: the MIDI border nodes process a block
//! without allocating, events flowing — a wire's through the input node's
//! translation onto its ports, a control thread's through the queue into the
//! out node's ring, and the clock's ticks.
//!
//! Mutation (run): a `Vec::new()` + push per block in `MidiInputNode::process`
//! → the gate panics.

use assert_no_alloc::AllocDisabler;
use tutti_graph::{Editor, EventEdge, EventIn, EventOut, Harmony, LoopRange, Prepare, Transport};
use tutti_midi_runtime::{
    ClockNode, HarmonyNode, MidiClipNode, MidiInputNode, MidiMailbox, MidiOutNode, MidiQueueNode,
    TimedHarmony, TimedMidiEvent,
};
use tutti_midi_types::MidiIn;
use tutti_midi_types::ump::MidiEvent;
use tutti_types::{Beat, Bpm, FrameClock, NodeKey, SampleRate, Samples};
use tutti_ump::{MidiChannel, MidiGroup};

#[global_allocator]
static A: AllocDisabler = AllocDisabler;

fn note_on(ch: u8, note: u8) -> MidiEvent {
    MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::of(ch), note, 0x8000)
}

/// A wire with a fresh note on four channels every poll.
struct Busy;

impl MidiIn for Busy {
    fn poll_block(&mut self, _block_size: usize, buffer: &mut [MidiEvent]) -> usize {
        for (i, slot) in buffer.iter_mut().take(4).enumerate() {
            *slot = note_on(i as u8 * 3, 60).with_frame_offset(40 - 10 * i as u32);
        }
        4
    }
}

#[test]
fn the_border_nodes_process_without_allocating() {
    let (mut ed, mut exec) = Editor::new(Prepare::new(SampleRate(48_000.0), Samples(512)));
    ed.insert(NodeKey(1), "in", MidiInputNode::new(Some(Box::new(Busy))));
    let keys = ed.insert(NodeKey(2), "keys", MidiQueueNode::new());
    let clock = ed.insert(NodeKey(3), "clock", ClockNode::new());
    let out = ed.insert(NodeKey(4), "out", MidiOutNode::new());
    clock.set_enabled(true);
    let into_out = |ed: &mut Editor, node: u64, port: u16| {
        ed.spec_mut().connect_events(
            EventIn {
                node: NodeKey(4),
                port: 0,
            },
            EventEdge::Direct(EventOut {
                node: NodeKey(node),
                port,
            }),
        );
    };
    into_out(&mut ed, 1, 0);
    into_out(&mut ed, 1, 3);
    into_out(&mut ed, 2, 0);
    into_out(&mut ed, 3, 0);
    ed.commit().unwrap();
    let t = Transport::new(true, Bpm(120.0), Beat(0.0), None);
    let mut drain = [MidiEvent::noop(); 256];
    // Warm up outside the gate.
    exec.process(512, &t, &[], &mut []);
    let _ = out.poll_into(&mut drain);

    let (tx, rx) = MidiMailbox::pair();
    assert_no_alloc::assert_no_alloc(|| {
        for b in 1..200u32 {
            keys.note_on(MidiChannel::FIRST, 60, 100);
            let at = Beat(f64::from(b) * 512.0 / 24_000.0);
            exec.process(
                512,
                &Transport::new(true, Bpm(120.0), at, None),
                &[],
                &mut [],
            );
            let n = out.poll_into(&mut drain);
            // Both halves of a mailbox, too.
            tx.queue(&drain[..n.min(4)]);
            let _ = rx.poll_into(&mut drain);
        }
    });
    assert_eq!(out.dropped(), 0);
}

/// A wire with a bend range on MPE member channel 2 every poll.
struct BendRange;

impl MidiIn for BendRange {
    fn poll_block(&mut self, _block_size: usize, buffer: &mut [MidiEvent]) -> usize {
        buffer[0] = MidiEvent::registered_controller(
            MidiGroup::FIRST,
            MidiChannel::of(2),
            tutti_midi_types::ump::RPN_BANK_MPE,
            tutti_midi_types::ump::RPN_INDEX_CHANNEL_PITCH_BEND_SENSITIVITY,
            tutti_midi_types::PitchBendSensitivity::from_semitones(12).to_rpn_bits(),
        );
        1
    }
}

/// The input node fans a member channel's bend range out over its zone
/// without allocating, and every member's port carries it.
///
/// Mutation (run): collect the fan-out into a `Vec` before writing it → the
/// gate panics.
#[test]
fn the_mpe_bend_range_fan_out_processes_without_allocating() {
    use tutti_midi_types::mpe::{MpeMode, MpeZoneConfig};
    let (mut ed, mut exec) = Editor::new(Prepare::new(SampleRate(48_000.0), Samples(512)));
    ed.insert(
        NodeKey(1),
        "in",
        MidiInputNode::new(Some(Box::new(BendRange)))
            .with_mpe(MpeMode::LowerZone(MpeZoneConfig::lower(15))),
    );
    let out = ed.insert(NodeKey(4), "out", MidiOutNode::new());
    ed.spec_mut().connect_events(
        EventIn {
            node: NodeKey(4),
            port: 0,
        },
        EventEdge::Direct(EventOut {
            node: NodeKey(1),
            port: 15,
        }),
    );
    ed.commit().unwrap();
    let t = Transport::default();
    let mut drain = [MidiEvent::noop(); 256];
    exec.process(512, &t, &[], &mut []);
    let _ = out.poll_into(&mut drain);

    let mut reached = 0;
    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..50 {
            exec.process(512, &t, &[], &mut []);
            reached += out.poll_into(&mut drain);
        }
    });
    assert_eq!(reached, 50, "the last member's port carries one per block");
}

/// A wire alternating an upper-zone and a lower-zone MPE Configuration
/// Message, one per poll.
struct Mcms(bool);

impl MidiIn for Mcms {
    fn poll_block(&mut self, _block_size: usize, buffer: &mut [MidiEvent]) -> usize {
        use tutti_midi_types::mpe::MpeZoneConfig;
        self.0 = !self.0;
        buffer[0] = if self.0 {
            MpeZoneConfig::upper(3).to_mcm()
        } else {
            MpeZoneConfig::lower(3).to_mcm()
        };
        1
    }
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
    use tutti_midi_types::mpe::{MpeMode, MpeZoneConfig};
    let (mut ed, mut exec) = Editor::new(Prepare::new(SampleRate(48_000.0), Samples(512)));
    let controls = ed.insert(
        NodeKey(1),
        "in",
        MidiInputNode::new(Some(Box::new(Mcms(false)))),
    );
    let out = ed.insert(NodeKey(4), "out", MidiOutNode::new());
    ed.spec_mut().connect_events(
        EventIn {
            node: NodeKey(4),
            port: 0,
        },
        EventEdge::Direct(EventOut {
            node: NodeKey(1),
            port: 15,
        }),
    );
    ed.commit().unwrap();
    let t = Transport::default();
    let mut drain = [MidiEvent::noop(); 256];
    exec.process(512, &t, &[], &mut []);
    let _ = out.poll_into(&mut drain);

    let modes = [
        MpeMode::DualZone {
            lower: MpeZoneConfig::lower(3),
            upper: MpeZoneConfig::upper(3),
        },
        MpeMode::DualZone {
            lower: MpeZoneConfig::lower(5),
            upper: MpeZoneConfig::upper(4),
        },
    ];
    let mut reached = 0;
    for i in 0..50 {
        controls.set_mpe_mode(modes[i % 2]);
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
/// wrap) and of a harmony node (a chord change in each, the chord re-stated
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
    let out = ed.insert(NodeKey(3), "out", MidiOutNode::new());
    for from in [1, 2] {
        ed.spec_mut().connect_events(
            EventIn {
                node: NodeKey(3),
                port: 0,
            },
            EventEdge::Direct(EventOut {
                node: NodeKey(from),
                port: 0,
            }),
        );
    }
    ed.commit().unwrap();
    let region = tutti_types::LoopRange::new(start, end).unwrap();
    let looping = Some(LoopRange { start, end });
    let mut clock = FrameClock::new(start, Bpm(120.0), SampleRate(RATE));
    let mut drain = [MidiEvent::noop(); 256];
    let mut block = |exec: &mut tutti_graph::Executor, clock: &mut FrameClock| {
        exec.process(
            512,
            &Transport::new(true, Bpm(120.0), clock.beat(), looping),
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
    assert!(sent >= 100 * 16, "every block walked its passes: {sent}");
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
        MidiEvent::note_off(
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
    let mut drain = [MidiEvent::noop(); 1024];
    let mut polled = 0;
    let mut f = 0u64;
    let mut block = |exec: &mut tutti_graph::Executor| {
        exec.process(
            64,
            &Transport::new(true, Bpm(120.0), beat(f as f64), None),
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
/// paid over the next blocks), a release request asked while they are owed, a seek (every held
/// note ended again) and a stop. None allocates.
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
            TimedMidiEvent::new(beat(u64::from(i)), MidiEvent::note_on(g, c, n, 0x8000)),
            TimedMidiEvent::new(beat(4_000), MidiEvent::note_off(g, c, n, 0)),
        ]
    });
    let (mut ed, mut exec) = Editor::new(Prepare::new(SampleRate(48_000.0), Samples(512)));
    let clip = ed.insert(NodeKey(1), "clip", MidiClipNode::new(events));
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
    let mut drain = [MidiEvent::noop(); 1024];
    let mut f = 0u64;
    let mut block = |exec: &mut tutti_graph::Executor, rolling: bool, frame: u64| {
        let t = Transport::new(rolling, Bpm(120.0), beat(frame), None);
        exec.process(512, &t, &[], &mut []);
        out.poll_into(&mut drain)
    };
    let mut polled = block(&mut exec, true, f);
    assert_no_alloc::assert_no_alloc(|| {
        // The table fills over blocks 0 to 4; the offs at frame 4,000 are in
        // block 7, which refuses most of them.
        for _ in 0..7 {
            f += 512;
            polled += block(&mut exec, true, f);
        }
        // A release asked while offs are owed is answered when the last is
        // paid.
        let mut ticket = Some(clip.release_held());
        let mut drained = false;
        for _ in 0..8 {
            f += 512;
            polled += block(&mut exec, true, f);
            if let Some(t) = ticket.take() {
                match t.drained() {
                    Ok(_) => drained = true,
                    Err(t) => ticket = Some(t),
                }
            }
        }
        assert!(drained);
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
    // The table filled, offs were refused, and some were paid late.
    assert!(exec.dropped_event_count() > 52 + 1_400);
    assert!(exec.lost_event_count() < exec.dropped_event_count());
}
