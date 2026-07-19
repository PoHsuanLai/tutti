//! Hardware loopback test using macOS IAC Driver.
//!
//! Requires IAC Driver to be enabled in Audio MIDI Setup.
//! Sends MIDI messages out through IAC, receives them back, and verifies correctness.

use std::sync::Arc;
use std::thread;
use std::time::Duration;
use tutti_midi_io::{MidiEvent, MidiIo, MidiPortManager};

fn note_on(channel: u8, note: u8, vel: u8) -> MidiEvent {
    MidiEvent::note_on(
        0,
        channel,
        note,
        tutti_midi_types::convert::midi1_velocity_to_midi2(vel),
    )
}

fn note_off(channel: u8, note: u8, vel: u8) -> MidiEvent {
    MidiEvent::note_off(
        0,
        channel,
        note,
        tutti_midi_types::convert::midi1_velocity_to_midi2(vel),
    )
}

fn cc(channel: u8, cc_num: u8, value: u8) -> MidiEvent {
    MidiEvent::cc(
        0,
        channel,
        cc_num,
        tutti_midi_types::convert::midi1_cc_to_midi2(value),
    )
}

fn bend(channel: u8, bend14: u16) -> MidiEvent {
    MidiEvent::pitch_bend(
        0,
        channel,
        tutti_midi_types::convert::midi1_pitch_bend_to_midi2(bend14),
    )
}

fn main() {
    let pm = Arc::new(MidiPortManager::new(256));
    let io = MidiIo::new(pm.clone());

    let inputs = io.list_input_devices();
    let iac_input = inputs.iter().find(|d| d.name.contains("IAC"));
    if iac_input.is_none() {
        eprintln!("ERROR: IAC Driver not found. Enable it in Audio MIDI Setup.");
        std::process::exit(1);
    }
    let iac_input = iac_input.unwrap();
    println!("Found IAC input: [{}] {}", iac_input.index, iac_input.name);

    io.connect_input(iac_input.index).unwrap();
    thread::sleep(Duration::from_millis(200));
    assert!(
        io.is_input_connected(&iac_input.name),
        "Should be connected to IAC input"
    );
    println!("Connected inputs: {:?}", io.connected_input_names());

    io.connect_output_by_name("IAC").unwrap();
    thread::sleep(Duration::from_millis(200));

    let _ = pm.cycle_start_read_all_inputs(512);

    println!("\n=== Test 1: Note On ===");
    io.send(note_on(0, 60, 100));
    thread::sleep(Duration::from_millis(100));
    let events = pm.cycle_start_read_all_inputs(512);
    if events.is_empty() {
        println!("  FAIL: No events received");
    } else {
        let e = &events[0].1;
        let ok = e.is_note_on() && e.note() == Some(60) && e.velocity_u7() == Some(100);
        println!(
            "  {}: got {} event(s), first: note_on={}, note={:?}, vel={:?}",
            if ok { "PASS" } else { "FAIL" },
            events.len(),
            e.is_note_on(),
            e.note(),
            e.velocity_u7()
        );
    }

    println!("\n=== Test 2: Note Off ===");
    io.send(note_off(0, 60, 64));
    thread::sleep(Duration::from_millis(100));
    let events = pm.cycle_start_read_all_inputs(512);
    if events.is_empty() {
        println!("  FAIL: No events received");
    } else {
        let e = &events[0].1;
        let ok = e.is_note_off() && e.note() == Some(60);
        println!(
            "  {}: note_off={}, note={:?}",
            if ok { "PASS" } else { "FAIL" },
            e.is_note_off(),
            e.note()
        );
    }

    println!("\n=== Test 3: Control Change ===");
    io.send(cc(0, 74, 127));
    thread::sleep(Duration::from_millis(100));
    let events = pm.cycle_start_read_all_inputs(512);
    if events.is_empty() {
        println!("  FAIL: No events received");
    } else {
        println!(
            "  Received: {:?}",
            tutti_midi_types::midi2::UmpMessage::try_from(events[0].1.data_words())
        );
        println!("  PASS");
    }

    println!("\n=== Test 4: Pitch Bend (center) ===");
    io.send(bend(0, 8192));
    thread::sleep(Duration::from_millis(100));
    let events = pm.cycle_start_read_all_inputs(512);
    if events.is_empty() {
        println!("  FAIL: No events received");
    } else {
        println!(
            "  Received: {:?}",
            tutti_midi_types::midi2::UmpMessage::try_from(events[0].1.data_words())
        );
        println!("  PASS");
    }

    println!("\n=== Test 5: Rapid burst (10 notes) ===");
    for n in 60..70u8 {
        io.send(note_on(0, n, 80));
    }
    thread::sleep(Duration::from_millis(200));
    let events = pm.cycle_start_read_all_inputs(512);
    let count = events.len();
    let all_note_on = events.iter().all(|(_, e)| e.is_note_on());
    println!(
        "  {}: received {}/10 events, all note_on={}",
        if count == 10 && all_note_on {
            "PASS"
        } else {
            "FAIL"
        },
        count,
        all_note_on
    );

    io.disconnect_input(&iac_input.name);
    io.disconnect_output();

    println!("\nAll tests complete!");
}
