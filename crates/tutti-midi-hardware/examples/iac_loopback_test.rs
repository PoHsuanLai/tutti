//! Hardware loopback test using the macOS IAC Driver.
//!
//! Requires the IAC Driver to be enabled in Audio MIDI Setup. Sends MIDI
//! messages out through IAC, receives them back, and verifies correctness.
//!
//! Shows the **message API**: you build a [`tutti_ump::Message`] and inspect
//! the MIDI 2.0 message a packet means, and never touch UMP words or the
//! MIDI-1-vs-2 distinction. `Ump::from` builds the wire packet from a message;
//! `tutti_midi_protocol::channel_voice` reads one back as MIDI 2.0, whichever
//! protocol the port carried it in.

use std::sync::Arc;
use std::thread;
use std::time::Duration;
use tutti_types::SampleRate;

use tutti_fixtures::midi::note_on;
use tutti_graph::Offset;
use tutti_midi_hardware::{HardwareMidiDrain, HardwareMidiInputs, MidiSession};
use tutti_midi_protocol::channel_voice;
use tutti_ump::{
    Address, Attribute, CCNumber, KeyEvent, Message, MidiChannel, MidiGroup, NoteNumber, Ump,
    Voice2,
};

/// Whether `ev` starts a note, in either protocol.
fn is_note_on(ev: Ump) -> bool {
    matches!(ev.key_event(), Some(KeyEvent::On(_)))
}

/// A MIDI 2.0 message on the first group, channel 1.
fn voice(voice: Voice2) -> Message {
    Message::Midi2(
        Address {
            group: MidiGroup::FIRST,
            channel: MidiChannel::FIRST,
        },
        voice,
    )
}

fn main() {
    let pm = Arc::new(HardwareMidiInputs::new(256, SampleRate::new(44_100.0)));
    // The audio side's drain: what a `MidiInputNode` would own.
    let mut drain = HardwareMidiDrain::new(Arc::clone(&pm));
    let io = MidiSession::open(pm.clone()).expect("open MIDI");

    let inputs = io.inputs();
    let Some(iac_input) = inputs.iter().find(|d| d.name.contains("IAC")) else {
        eprintln!("ERROR: IAC Driver not found. Enable it in Audio MIDI Setup.");
        std::process::exit(1);
    };
    println!("Found IAC input: [{}] {}", iac_input.id, iac_input.name);

    io.connect_input(&iac_input.id).unwrap();
    thread::sleep(Duration::from_millis(200));
    assert!(
        io.is_any_input_connected(),
        "Should be connected to IAC input"
    );
    println!("Connected inputs: {:?}", io.connected_input_names());

    io.connect_output_by_name("IAC").unwrap();
    thread::sleep(Duration::from_millis(200));

    // Drain anything buffered from before the test started.
    drain.cycle_start_read_all_inputs(512, |_, _| {});

    // --- Send a message, read back exactly one, hand it to `check`. ---
    //
    // `Ump::from(msg)` encodes the wire packet; on the way back in,
    // `channel_voice` reads it as the MIDI 2.0 message it means. The app speaks
    // messages at both ends — the port handles the wire form, and the
    // MIDI-1-vs-2 distinction never surfaces.
    let mut roundtrip = |label: &str, msg: Message, check: &dyn Fn(Voice2) -> bool| {
        io.send(&[(Offset::ZERO, Ump::from(msg))]);
        thread::sleep(Duration::from_millis(100));
        let mut first: Option<Ump> = None;
        drain.cycle_start_read_all_inputs(512, |_, (_, ev)| {
            if first.is_none() {
                first = Some(ev);
            }
        });
        match first {
            None => println!("  FAIL [{label}]: no events received"),
            Some(ev) => {
                let got = channel_voice(ev);
                println!(
                    "  {} [{label}]: {got:?}",
                    if got.is_some_and(|(_, v)| check(v)) {
                        "PASS"
                    } else {
                        "FAIL"
                    }
                );
            }
        }
    };

    println!("\n=== Test 1: Note On ===");
    roundtrip(
        "note-on",
        // Velocity is the full 16-bit MIDI 2.0 value; `u16::MAX` ~= velocity 127.
        voice(Voice2::NoteOn {
            note: NoteNumber::of(60),
            velocity: 0xC800,
            attribute: Attribute::None,
        }),
        &|v| matches!(v, Voice2::NoteOn { note, velocity, .. } if note.get() == 60 && velocity > 0),
    );

    println!("\n=== Test 2: Note Off ===");
    roundtrip(
        "note-off",
        voice(Voice2::NoteOff {
            note: NoteNumber::of(60),
            velocity: 0x4000,
            attribute: Attribute::None,
        }),
        &|v| matches!(v, Voice2::NoteOff { note, .. } if note.get() == 60),
    );

    println!("\n=== Test 3: Control Change (CC74) ===");
    roundtrip(
        "cc74",
        voice(Voice2::ControlChange {
            controller: CCNumber::of(74),
            value: 0xFFFF_FFFF,
        }),
        &|v| matches!(v, Voice2::ControlChange { controller, .. } if controller.get() == 74),
    );

    println!("\n=== Test 4: Pitch Bend (center) ===");
    roundtrip(
        "pitch-bend",
        voice(Voice2::PitchBend(0x8000_0000)), // bipolar center
        &|v| matches!(v, Voice2::PitchBend(_)),
    );

    println!("\n=== Test 5: Rapid burst (10 notes) ===");
    for n in 60..70u8 {
        io.send(&[(
            Offset::ZERO,
            note_on(MidiGroup::FIRST, MidiChannel::FIRST, n, 0xA000),
        )]);
    }
    thread::sleep(Duration::from_millis(200));
    let mut count = 0usize;
    let mut all_note_on = true;
    drain.cycle_start_read_all_inputs(512, |_, (_, ev)| {
        count += 1;
        all_note_on &= is_note_on(ev);
    });
    println!(
        "  {}: received {count}/10 events, all note_on={all_note_on}",
        if count == 10 && all_note_on {
            "PASS"
        } else {
            "FAIL"
        },
    );

    io.disconnect_input(&iac_input.id);
    io.disconnect_output();

    println!("\nAll tests complete!");
}
