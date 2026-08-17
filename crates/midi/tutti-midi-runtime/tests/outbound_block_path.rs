//! The outbound MIDI path, assembled with **no adapter and no Bevy**.
//!
//! `MidiPostBlock` was complete and RT-wired long before anything built one:
//! `tutti-cpal` held an `Option<MidiPostBlock>` and called `run()` in the
//! callback, but no code anywhere constructed the value, so a hosted plugin's
//! MIDI-out reached nothing. The fix was two lines in the Bevy adapter — which
//! is only correct if the assembly is genuinely engine-side.
//!
//! This test is what pins that. If it ever needs a type from `bevy-tutti` to
//! compile, the outbound path has leaked into the adapter and the adapter has
//! stopped being a wrapper.
//!
//! The block phases run by hand rather than through a device: the question is
//! whether the wiring is *expressible*, not whether CPAL works.

use std::sync::Arc;

use tutti_midi_runtime::{MidiBus, MidiInPort, MidiPostBlock, MidiPreBlock};
use tutti_midi_types::ump::MidiEvent;
use tutti_midi_types::{MidiChannel, MidiGroup};
use tutti_midi_types::{MidiMessage, MidiRoute, MidiRoutingTable};

const BLOCK: usize = 64;

/// The shared state both phases read, plus the destination unit.
///
/// The *same* routing snapshot handle goes to both phases deliberately: a node's
/// MIDI-out is routed by exactly the rules a hardware input is, and two tables
/// would let the two directions disagree about where a channel goes.
fn rig() -> (MidiPreBlock, MidiPostBlock, MidiInPort) {
    let mut routing = MidiRoutingTable::new();
    let bus = MidiBus::new();

    let dest = MidiInPort::new();
    bus.insert(dest.sender());

    routing.set_routes(vec![MidiRoute::new().with_target(dest.unit_id())], None);
    routing.commit();

    let mut pre = MidiPreBlock::new(routing.snapshot_arc());
    pre.set_queue(Arc::new(bus.clone()));

    let mut post = MidiPostBlock::new(routing.snapshot_arc());
    post.set_queue(Arc::new(bus.clone()));

    (pre, post, dest)
}

fn note_on() -> MidiEvent {
    // A velocity with no 7-bit spelling, so a truncating path is visible.
    MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0xC000)
}

/// An emitted event reaches the destination unit's inbox, at full MIDI 2.0
/// width, with nothing from `bevy-tutti` involved.
#[test]
fn an_emitted_event_reaches_the_destination_inbox() {
    let (pre, post, dest) = rig();
    let sink = post.sink();

    // One audio block: pre.run → (the graph renders, a node emits) → post.run.
    pre.run(BLOCK);
    assert_eq!(sink.extend(&[note_on()]), 1, "the sink accepted the event");
    post.run();

    let mut buf = [MidiEvent::noop(); 16];
    let n = dest.poll(BLOCK, &mut buf);
    assert_eq!(n, 1, "the emitted event reached the destination inbox");

    match buf[0].message() {
        MidiMessage::NoteOn { velocity, note, .. } => {
            assert_eq!(note, 60);
            assert_eq!(velocity, 0xC000, "16-bit velocity survives the fan-out");
        }
        other => panic!("expected a note-on, got {other:?}"),
    }
}

/// **`post.run()` is what delivers.** Emitting without it must leave the inbox
/// empty.
///
/// That is the whole reason the phase runs *after* `engine.process`: delivery
/// has to be independent of the order the emitting nodes happened to be
/// scheduled in. If an event could arrive mid-render, a consumer polled earlier
/// in the same block would miss it and one polled later would not — the same
/// event landing in different blocks depending on graph topology.
#[test]
fn nothing_is_delivered_until_the_post_block_runs() {
    let (_pre, post, dest) = rig();
    let sink = post.sink();

    assert_eq!(sink.extend(&[note_on()]), 1);

    let mut buf = [MidiEvent::noop(); 16];
    assert_eq!(
        dest.poll(BLOCK, &mut buf),
        0,
        "an emitted event must not be visible before the post-block runs"
    );

    post.run();
    assert_eq!(
        dest.poll(BLOCK, &mut buf),
        1,
        "and must be visible immediately after"
    );
}

/// With no router installed the sink is still drained, not left pending.
///
/// Leaving events queued would fan them out on a *later* block, against a
/// `frame_offset` computed for the block they were emitted in — an event
/// arriving with a timestamp from the past.
#[test]
fn an_unrouted_post_block_drains_rather_than_accumulates() {
    let routing = MidiRoutingTable::new();
    let post = MidiPostBlock::new(routing.snapshot_arc()); // no set_queue
    let sink = post.sink();

    assert_eq!(sink.extend(&[note_on(), note_on()]), 2);
    post.run();
    assert!(
        sink.is_empty(),
        "events must be discarded, not held for a later block"
    );
}
