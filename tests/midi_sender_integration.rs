//! Verify the producer-handle MIDI surface end-to-end without requiring
//! an audio device. Each MIDI-consuming node owns a `MidiEventSlot` and
//! exposes a `MidiSender` the caller pushes events through; nodes poll
//! their own receivers during `tick()`.

#![cfg(all(feature = "midi", feature = "synth"))]

use tutti_core::AudioUnit;
use tutti_midi_runtime::{MidiBus, MidiEventSlot};
use tutti_synth::SynthHandle;

#[test]
fn synth_sender_drives_audio() {
    let mut synth = SynthHandle::new(44100.0)
        .sine()
        .poly(1)
        .adsr(0.001, 0.0, 1.0, 0.1)
        .build()
        .unwrap();

    synth.midi_sender().note_on(0, 60, 100);

    let mut peak = 0.0f32;
    for _ in 0..2000 {
        let mut out = [0.0f32; 2];
        synth.tick(&[], &mut out);
        peak = peak.max(out[0].abs().max(out[1].abs()));
    }
    assert!(peak > 0.01, "expected audio after note-on, peak={peak}");
}

#[test]
fn bus_routes_events_to_synth_via_sender() {
    let mut synth = SynthHandle::new(44100.0)
        .sine()
        .poly(1)
        .adsr(0.001, 0.0, 1.0, 0.1)
        .build()
        .unwrap();

    let bus = MidiBus::new();
    bus.insert(synth.midi_sender());

    use tutti_core::midi::MidiTarget;
    use tutti_midi_types::ump::MidiEvent;
    let unit_id = synth.midi_unit_id();
    let event = MidiEvent::note_on(0, 0, 60, (100u16) << 9);
    bus.queue(unit_id, &[event]);

    let mut peak = 0.0f32;
    for _ in 0..2000 {
        let mut out = [0.0f32; 2];
        synth.tick(&[], &mut out);
        peak = peak.max(out[0].abs().max(out[1].abs()));
    }
    assert!(
        peak > 0.01,
        "bus-routed event should drive synth, peak={peak}"
    );
}

#[test]
fn dropped_receiver_does_not_panic() {
    use tutti_midi_runtime::tutti_midi_types::MidiUnitId;
    let (sender, receiver) = MidiEventSlot::pair(MidiUnitId::next());
    drop(receiver);
    sender.note_on(0, 60, 100);
    sender.note_off(0, 60);
}
