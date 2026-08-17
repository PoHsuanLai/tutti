//! Hardware loopback test using the macOS IAC Driver.
//!
//! Requires the IAC Driver to be enabled in Audio MIDI Setup. Sends MIDI
//! messages out through IAC, receives them back, and verifies correctness.
//!
//! Shows the **ergonomic API**: you build and inspect [`MidiMessage`] — the
//! "just tell me what it is" view — and never touch UMP words, `midi2`
//! internals, or the MIDI-1-vs-2 distinction. `MidiEvent::from()` builds the
//! wire event from a message; `event.message()` decodes one back.

use std::sync::Arc;
use std::thread;
use std::time::Duration;

use tutti_midi_hardware::{HardwareMidiInputs, MidiEvent, MidiMessage, MidiSession, NoteId};
use tutti_midi_types::{MidiChannel, MidiGroup};

fn main() {
    let pm = Arc::new(HardwareMidiInputs::new(256));
    let io = MidiSession::new(pm.clone());

    let inputs = io.inputs();
    let Some(iac_input) = inputs.iter().find(|d| d.name.contains("IAC")) else {
        eprintln!("ERROR: IAC Driver not found. Enable it in Audio MIDI Setup.");
        std::process::exit(1);
    };
    println!(
        "Found IAC input: [{}] {}",
        iac_input.id.raw(),
        iac_input.name
    );

    io.connect_input(iac_input.id).unwrap();
    thread::sleep(Duration::from_millis(200));
    assert!(
        io.is_any_input_connected(),
        "Should be connected to IAC input"
    );
    println!("Connected inputs: {:?}", io.connected_input_names());

    io.connect_output_by_name("IAC").unwrap();
    thread::sleep(Duration::from_millis(200));

    // Drain anything buffered from before the test started.
    pm.cycle_start_read_all_inputs(512, |_, _| {});

    // --- Send a message, read back exactly one, hand it to `check`. ---
    //
    // `MidiEvent::try_from(msg)` encodes the wire event (fallible only for the
    // `Other` catch-all); on the way back in, `event.message()` decodes to a
    // `MidiMessage`. The app speaks messages at both ends — the port and codec
    // handle the wire form, and the MIDI-1-vs-2 distinction never surfaces.
    let roundtrip = |label: &str, msg: MidiMessage, check: &dyn Fn(MidiMessage) -> bool| {
        io.send(&[MidiEvent::try_from(msg).expect("message is encodable")]);
        thread::sleep(Duration::from_millis(100));
        let mut first: Option<MidiEvent> = None;
        pm.cycle_start_read_all_inputs(512, |_, ev| {
            if first.is_none() {
                first = Some(ev);
            }
        });
        match first {
            None => println!("  FAIL [{label}]: no events received"),
            Some(ev) => {
                let got = ev.message();
                println!(
                    "  {} [{label}]: {got:?}",
                    if check(got) { "PASS" } else { "FAIL" }
                );
            }
        }
    };

    println!("\n=== Test 1: Note On ===");
    roundtrip(
        "note-on",
        // Velocity is the full 16-bit MIDI 2.0 value; `u16::MAX` ~= velocity 127.
        MidiMessage::NoteOn {
            frame_offset: 0,
            id: note_id(0, 60),
            channel: 0,
            note: 60,
            velocity: 0xC800,
            attribute: None,
        },
        &|m| m.is_note_on() && m.note() == Some(60) && m.channel() == Some(0),
    );

    println!("\n=== Test 2: Note Off ===");
    roundtrip(
        "note-off",
        MidiMessage::NoteOff {
            frame_offset: 0,
            id: note_id(0, 60),
            channel: 0,
            note: 60,
            velocity: 0x4000,
            attribute: None,
        },
        &|m| m.is_note_off() && m.note() == Some(60),
    );

    println!("\n=== Test 3: Control Change (CC74) ===");
    roundtrip(
        "cc74",
        MidiMessage::ControlChange {
            frame_offset: 0,
            channel: 0,
            index: 74,
            value: 0xFFFF_FFFF,
        },
        &|m| matches!(m, MidiMessage::ControlChange { index: 74, .. }),
    );

    println!("\n=== Test 4: Pitch Bend (center) ===");
    roundtrip(
        "pitch-bend",
        MidiMessage::PitchBend {
            frame_offset: 0,
            channel: 0,
            value: 0x8000_0000, // bipolar center
        },
        &|m| matches!(m, MidiMessage::PitchBend { .. }),
    );

    println!("\n=== Test 5: Rapid burst (10 notes) ===");
    for n in 60..70u8 {
        io.send(&[MidiEvent::note_on(
            MidiGroup::FIRST,
            MidiChannel::FIRST,
            n,
            0xA000,
        )]);
    }
    thread::sleep(Duration::from_millis(200));
    let mut count = 0usize;
    let mut all_note_on = true;
    pm.cycle_start_read_all_inputs(512, |_, ev| {
        count += 1;
        all_note_on &= ev.message().is_note_on();
    });
    println!(
        "  {}: received {count}/10 events, all note_on={all_note_on}",
        if count == 10 && all_note_on {
            "PASS"
        } else {
            "FAIL"
        },
    );

    io.disconnect_input(iac_input.id);
    io.disconnect_output();

    println!("\nAll tests complete!");
}

/// The per-note identity for a `(channel, note)` on the MIDI-1 path — what the
/// decoder reconstructs on the way back in, so the sent and received ids match.
fn note_id(channel: u8, note: u8) -> NoteId {
    NoteId::from_channel_note(channel, note)
}
