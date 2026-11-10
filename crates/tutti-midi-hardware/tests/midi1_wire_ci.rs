//! The MIDI-1 wire to MIDI-CI seam, end to end.
//!
//! MIDI-CI (M2-101) is Universal SysEx precisely so it works over a MIDI-1.0
//! transport: that is how two devices negotiate before either knows the other
//! speaks MIDI 2.0. So a real CI probe arrives as raw `F0 7E … F7` bytes even
//! on a UMP-era stack. The codec's own round-trip tests start from UMP and so
//! cannot catch a break at the wire edge (a dropped `0xF0`, a mis-sized payload
//! split).
//!
//! This asserts the whole promotion chain: raw bytes, `Midi1Parser`, UMP
//! SysEx7 packets, `SysexAssembler`, a typed `CiMessage`.
//!
//! Mutation (run): `Midi1Parser` pushing a packet at five bytes instead of six,
//! and its last packet always `Complete` instead of `End` -> fails each time.

use tutti_midi_runtime::CiInitiator;
use tutti_midi_types::ci::{CiMessage, DiscoveryData, Muid, NO_FUNCTION_BLOCK, ci_to_sysex7};
use tutti_midi_types::sysex7_payload;
use tutti_ump::{Message, Midi1Parser, MidiGroup, SysexAssembler, SysexStep, Ump};

/// The payload bytes of SysEx7 packets, in order.
fn payload_of(packets: &[Ump]) -> Vec<u8> {
    packets
        .iter()
        .flat_map(|packet| {
            let (_status, bytes, n) = sysex7_payload(packet).expect("a SysEx7 packet");
            bytes.into_iter().take(n)
        })
        .collect()
}

#[test]
fn midi1_wire_sysex_promotes_to_a_typed_ci_message() {
    // A Discovery probe exactly as a peer device would send it.
    let peer = CiInitiator::new(
        Muid::from_seed(0x1234),
        DiscoveryData {
            manufacturer: [0x00, 0x21, 0x09],
            family: 0x0042,
            family_model: 0x0007,
            software_revision: [1, 2, 3, 4],
            categories: Default::default(),
            max_sysex_size: 512,
            // A Discovery, not a reply: section 5.5.4 makes the path id the
            // initiator's own, and `function_block` is reply-only, so
            // `NO_FUNCTION_BLOCK` is the value section 5.6.2 names for a device
            // that represents none.
            output_path_id: 0,
            function_block: NO_FUNCTION_BLOCK,
        },
    );
    let probe = peer.discovery();

    // Encode it, then flatten back to the MIDI-1.0 byte stream a DIN or serial
    // port delivers: F0 <payload> F7.
    let mut ump = Vec::new();
    ci_to_sysex7(MidiGroup::FIRST, &probe, &mut ump);
    let mut wire = vec![0xF0];
    wire.extend(payload_of(&ump));
    wire.push(0xF7);
    assert!(wire.len() > 8, "the probe spans several packets");

    // Parse the bytes, then reassemble the packets.
    let mut parser = Midi1Parser::new(MidiGroup::FIRST);
    let fragments: Vec<Ump> = parser
        .feed_all(&wire)
        .map(|message| {
            assert!(matches!(message, Message::Sysex7(..)));
            Ump::from(message)
        })
        .collect();

    let mut assembler = SysexAssembler::new(vec![0; SysexAssembler::room_for(256, 16)], 256);
    let decoded = fragments
        .iter()
        .find_map(|event| match assembler.push(*event) {
            SysexStep::Complete(at) => CiMessage::decode(assembler.message(at).payload()),
            _ => None,
        });

    assert_eq!(
        decoded,
        Some(probe),
        "a CI probe off the MIDI-1 wire must reach the negotiators intact"
    );
}
