//! Regression gate for RT-safety: the MIDI border nodes process a block
//! without allocating, events flowing — a wire's through the input node's
//! translation onto its ports, a control thread's through the queue into the
//! out node's ring, and the clock's ticks.
//!
//! Mutation (run): a `Vec::new()` + push per block in `MidiInputNode::process`
//! → the gate panics.

use assert_no_alloc::AllocDisabler;
use tutti_core::{Beat, Bpm, FrameClock, NodeKey, SampleRate, Samples};
use tutti_graph::{Editor, EventEdge, EventIn, EventOut, Harmony, LoopRange, Prepare, Transport};
use tutti_midi_runtime::{
    ClockNode, HarmonyNode, MidiClipNode, MidiInputNode, MidiMailbox, MidiOutNode, MidiQueueNode,
    TimedHarmony, TimedMidiEvent,
};
use tutti_midi_types::ump::MidiEvent;
use tutti_midi_types::{MidiChannel, MidiGroup, MidiIn};

#[global_allocator]
static A: AllocDisabler = AllocDisabler;

fn note_on(ch: u8, note: u8) -> MidiEvent {
    MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::new(ch), note, 0x8000)
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
            MidiChannel::new(2),
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
    let region = tutti_core::LoopRange::new(start, end).unwrap();
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
