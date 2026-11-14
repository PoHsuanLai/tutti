use std::time::{Duration, Instant};

use tutti_graph::Offset;
use tutti_midi_hardware::Devices;
use tutti_midi_runtime::{Outgoing, SendAt};
use tutti_ump::{Message, MidiGroup, Sysex7Packet, SysexStatus, Ump};

fn synth(devices: &Devices) -> tutti_midi_hardware::DeviceInfo {
    let outputs: Vec<_> = devices
        .list()
        .into_iter()
        .filter(|d| d.direction.is_output())
        .collect();
    outputs
        .iter()
        .find(|d| d.name.contains("GS Wavetable"))
        .cloned()
        .unwrap_or_else(|| {
            let names: Vec<_> = outputs.iter().map(|d| d.name.as_str()).collect();
            panic!("the built-in GS Wavetable Synth output is missing; outputs: {names:?}")
        })
}

fn at(send: SendAt, ump: Ump) -> Outgoing {
    Outgoing {
        at: Offset::ZERO,
        send,
        ump,
    }
}

fn sysex(status: SysexStatus, data: &[u8]) -> Ump {
    Ump::from(Message::Sysex7(
        MidiGroup::FIRST,
        Sysex7Packet::new(status, data).unwrap(),
    ))
}

#[test]
fn the_synth_is_listed_as_an_output_with_a_stable_id_and_a_clean_input_list() {
    let devices = Devices::open().unwrap();
    let synth = synth(&devices);
    assert!(!synth.direction.is_input(), "{synth:?}");
    // A second listing names the same device by the same id.
    let again = Devices::open().unwrap();
    assert!(again.list().iter().any(|d| d.id == synth.id));
    // Inputs, of which a runner has none, are listed or not but never a
    // nameless entry.
    assert!(
        devices
            .list()
            .iter()
            .filter(|d| d.direction.is_input())
            .all(|d| !d.name.is_empty())
    );
}

#[test]
fn the_synth_takes_notes_scheduled_notes_and_a_sysex_and_opens_again_after_a_close() {
    let devices = Devices::open().unwrap();
    let id = synth(&devices).id;
    let note_on = Ump([0x2090_3C64, 0, 0, 0]);
    let note_off = Ump([0x2080_3C00, 0, 0, 0]);
    for _ in 0..2 {
        let mut out = devices.open_output(&id).expect("the synth opens");
        let now = Instant::now();
        let batch = [
            at(SendAt::Now, note_on),
            at(SendAt::At(now + Duration::from_millis(60)), note_off),
            // The GS reset as SysEx7: 11 bytes without delimiters 9, two packets.
            at(
                SendAt::At(now + Duration::from_millis(90)),
                sysex(SysexStatus::Start, &[0x41, 0x10, 0x42, 0x12, 0x40, 0x00]),
            ),
            at(
                SendAt::At(now + Duration::from_millis(90)),
                sysex(SysexStatus::End, &[0x7F, 0x00, 0x41]),
            ),
        ];
        assert_eq!(out.queue_timed(&batch), batch.len());
        // Dropping closes the port; what is not yet due is dropped with it.
        std::thread::sleep(Duration::from_millis(150));
    }
}
