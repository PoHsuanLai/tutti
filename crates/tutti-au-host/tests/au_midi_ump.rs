//! AU MIDI input against real units: what the legacy path does with a Group, and
//! that a UMP list reaches a unit through `MusicDeviceMIDIEventList`.
//!
//! **No unit installed on this machine takes MIDI 2.0 through the host's
//! activation.** `kAudioUnitProperty_AudioUnitMIDIProtocol` answers
//! `kAudioUnitErr_InvalidProperty` (-10879) on AUSampler, AUMIDISynth and
//! DLSMusicDevice, before and after `HostMIDIProtocol` is written (macOS 26.6);
//! it is an AUv3 (`AUAudioUnit`) property, and no AUv3 instrument ships with
//! the system. So `send_midi` takes the legacy path for every installed unit,
//! and the UMP path of `send_midi` (its choice by protocol, its packet staging)
//! is covered by `processor.rs`'s unit tests over a sink, not by a unit. What
//! these tests do run on a real unit is the call itself.

#![cfg(target_os = "macos")]

use std::sync::Mutex;

use tutti_au_ffi::{EventList, Pushed, Wire};
use tutti_au_host::AuMidiMapping;
use tutti_graph::Offset;
use tutti_ump::{CCNumber, MidiChannel, MidiGroup};

mod support;
use support::au::Au;
use support::corpus::SAMPLER;
use tutti_fixtures::midi;
use tutti_midi_protocol::widen_value;
use tutti_ump::U7;

static AU_LOCK: Mutex<()> = Mutex::new(());

const RATE: tutti_types::SampleRate = tutti_types::SampleRate::SR_48K;
const BLOCK: u32 = 512;
const GAIN: u32 = 900;
const CC: u8 = 20;

/// An AUSampler with CC 20 mapped to its `Gain` (`-96..=12` dB) and a known
/// starting value, so a controller is observable as the parameter it moves.
fn mapped_sampler() -> (Au, f32) {
    let mut au = SAMPLER.open(RATE, BLOCK);
    au.set_parameter_midi_mappings(&[]).expect("clear");
    au.add_parameter_midi_mapping(&[AuMidiMapping::control_change(GAIN, 0, CC)])
        .expect("map CC 20 to Gain");
    let before = au.get_parameter(GAIN).expect("read Gain");
    (au, before)
}

fn render(au: &mut Au) {
    let mut output = vec![vec![0.0f32; BLOCK as usize]; au.num_outputs() as usize];
    let mut refs: Vec<&mut [f32]> = output.iter_mut().map(|v| v.as_mut_slice()).collect();
    au.process(&[], &mut refs, BLOCK).expect("render");
}

fn cc(group: u8, value: u32) -> tutti_graph::UmpAt {
    (
        Offset::ZERO,
        midi::cc(
            tutti_ump::Address::new(MidiGroup::of(group), MidiChannel::FIRST),
            CCNumber::of(CC),
            value,
        ),
    )
}

/// **A unit that takes no MIDI 2.0 is the legacy path.** The installed Apple
/// instruments answer no protocol.
#[test]
fn an_installed_instrument_reports_no_protocol_and_is_legacy() {
    let _g = AU_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let au = SAMPLER.open(RATE, BLOCK);
    assert_eq!(au.midi_protocol(), None);
}

/// **The legacy path plays Group 0 and drops the rest, counted.** A MIDI 1.0
/// call has no Group, so a controller of Group 1 would otherwise move what
/// Group 0's does.
#[test]
fn the_legacy_path_drops_another_group_and_counts_it() {
    let _g = AU_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (mut au, before) = mapped_sampler();

    au.send_midi(&[cc(1, widen_value(U7::of(127)).get())]);
    render(&mut au);
    assert_eq!(au.groups_dropped(), 1);
    assert_eq!(
        au.get_parameter(GAIN).unwrap(),
        before,
        "a Group 1 controller must not be heard on Group 0's channels"
    );

    au.send_midi(&[cc(0, widen_value(U7::of(127)).get())]);
    render(&mut au);
    assert_eq!(au.groups_dropped(), 1, "Group 0 is not dropped");
    assert_ne!(au.get_parameter(GAIN).unwrap(), before, "Group 0 is heard");
}

/// **A message with no MIDI 1.0 form is dropped and counted on the legacy path**,
/// not silently.
#[test]
fn the_legacy_path_counts_a_message_it_cannot_narrow() {
    let _g = AU_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (au, _) = mapped_sampler();
    au.send_midi(&[(
        Offset::ZERO,
        midi::per_note_pitch_bend(MidiChannel::FIRST, 60, 0x8000_0000),
    )]);
    assert_eq!(au.unsendable_dropped(), 1);
    assert_eq!(au.groups_dropped(), 0);
}

/// **A UMP list reaches a unit through `MusicDeviceMIDIEventList`**: a MIDI 2.0
/// Control Change packet, built by `EventList`, moves the parameter mapped to
/// it, and a controller at 32 bits is not narrowed away (full scale lands on
/// the top of the range).
/// Spec: au.midi.MusicDeviceMIDIEventList
#[test]
fn a_ump_list_moves_a_mapped_parameter() {
    let _g = AU_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (mut au, before) = mapped_sampler();

    let mut list = EventList::with_capacity(64);
    let packet = cc(0, 0xFFFF_FFFF).1;
    assert_eq!(
        list.push(Some(Wire::Midi2), 0, packet.words()),
        Pushed::Added
    );
    au.unit()
        .midi_event_list(&list)
        .expect("MusicDeviceMIDIEventList");
    render(&mut au);

    let after = au.get_parameter(GAIN).unwrap();
    assert_ne!(after, before, "the list reached the unit");
    assert_eq!(
        after, 12.0,
        "full scale is the top of the -96..=12 dB range"
    );
}
