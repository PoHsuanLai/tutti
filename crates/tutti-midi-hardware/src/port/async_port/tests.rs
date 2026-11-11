use super::*;
use tutti_midi_protocol::channel_voice;
use tutti_ump::{KeyEvent, MidiChannel, MidiGroup, Voice2};

fn note_on(note: u8, vel: u8) -> Ump {
    tutti_fixtures::midi::note_on(
        MidiGroup::FIRST,
        MidiChannel::FIRST,
        note,
        tutti_fixtures::midi::wide_velocity(vel),
    )
}

/// A packet that is only a tag: word 0 holds `i`, so a test can tell the
/// events apart (the old tests tagged them through `frame_offset`).
fn tagged(i: u32) -> Ump {
    Ump([i, 0, 0, 0])
}

/// Test helper: drain input into a fresh Vec.
fn read_input(port: &HardwareMidiInput) -> Vec<Ump> {
    let mut buf = Vec::new();
    port.drain_limited(usize::MAX, |_, e| buf.push(e));
    buf
}

#[test]
fn drain_stops_at_limit_and_keeps_the_rest() {
    let (port, mut handle) = HardwareMidiInput::new("Limited", 16, PortId::TEST);
    for i in 0..8 {
        assert!(handle.push(tagged(i), Instant::now()));
    }

    let mut buf = Vec::new();
    let taken = port.drain_limited(3, |_, e| buf.push(e));
    assert_eq!(taken, 3, "drain must stop at the limit");
    assert_eq!(buf.len(), 3);

    // The undrained remainder is still queued, not lost.
    let rest = read_input(&port);
    assert_eq!(rest.len(), 5, "events past the limit stay in the ring");
}

#[test]
fn drain_of_zero_takes_nothing() {
    let (port, mut handle) = HardwareMidiInput::new("Zero", 16, PortId::TEST);
    assert!(handle.push(note_on(0x3C, 0x7F), Instant::now()));

    let mut buf = Vec::new();
    assert_eq!(port.drain_limited(0, |_, e| buf.push(e)), 0);
    assert!(buf.is_empty());
    assert_eq!(read_input(&port).len(), 1, "a zero budget must not consume");
}

#[test]
fn test_input_flow() {
    let (port, mut producer_handle) = HardwareMidiInput::new("Input", 256, PortId::TEST);

    let event = note_on(0x3C, 0x7F);
    assert!(producer_handle.push(event, Instant::now()));

    let events = read_input(&port);
    assert_eq!(events.len(), 1);
    let Some(KeyEvent::On(key)) = events[0].key_event() else {
        panic!("a note-on, got {:?}", events[0]);
    };
    assert_eq!(key.note(), 0x3C);
    let Some((_, Voice2::NoteOn { velocity, .. })) = channel_voice(events[0]) else {
        panic!("a MIDI 2.0 note-on");
    };
    assert_eq!(tutti_midi_protocol::narrow_velocity(velocity).get(), 0x7F);
}

#[test]
fn test_fifo_full() {
    let (_port, mut input_handle) = HardwareMidiInput::new("Full", 4, PortId::TEST);

    for i in 0..4 {
        let event = tagged(i);
        assert!(
            input_handle.push(event, Instant::now()),
            "Failed to write event {}",
            i
        );
    }

    let event = note_on(0x3C, 0x7F);
    assert!(
        !input_handle.push(event, Instant::now()),
        "FIFO should be full"
    );
}

#[test]
fn test_active_flag_toggle() {
    let (port, _handle) = HardwareMidiInput::new("ActiveTest", 256, PortId::TEST);
    assert!(port.is_active());

    port.set_active(false);
    assert!(!port.is_active());

    port.set_active(true);
    assert!(port.is_active());
}

/// A driver thread pushing while the drain pops: every event arrives, once,
/// in order. Small enough to run under miri, which is what checks the two
/// halves for a data race.
///
/// The aliasing this test cannot express (two producers on one ring) is no longer
/// writable: `push` takes `&mut self` and the handle is not `Clone`.
#[test]
fn a_driver_thread_and_the_drain_run_concurrently() {
    const EVENTS: u32 = 200;
    let (port, mut handle) = HardwareMidiInput::new("Concurrent", 8, PortId::TEST);
    let driver = std::thread::spawn(move || {
        for i in 0..EVENTS {
            let event = tagged(i);
            while !handle.push(event, Instant::now()) {
                std::thread::yield_now();
            }
        }
    });

    // Drain until the driver is done and a last pass finds nothing, so a
    // lost event fails the comparison below rather than hanging the loop.
    let mut seen = Vec::new();
    loop {
        let done = driver.is_finished();
        let taken = port.drain_limited(usize::MAX, |_, e| seen.push(e.0[0]));
        if done && taken == 0 {
            break;
        }
        std::thread::yield_now();
    }
    driver.join().expect("driver thread");
    assert_eq!(seen, (0..EVENTS).collect::<Vec<_>>());
}
