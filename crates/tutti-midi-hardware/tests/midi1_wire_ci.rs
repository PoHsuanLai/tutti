//! The MIDI-1 wire to MIDI-CI seam, end to end.
//!
//! MIDI-CI (M2-101) is Universal SysEx precisely so it works over a MIDI-1.0
//! transport: that is how two devices negotiate before either knows the other
//! speaks MIDI 2.0. So a real CI probe arrives as raw `F0 7E ... F7` bytes even
//! on a UMP-era stack. The codec's own round-trip tests start from payload bytes
//! and so cannot catch a break at the wire edge (a dropped `0xF0`, a mis-sized
//! payload split).
//!
//! This asserts the whole promotion chain: a Discovery encoded by
//! `tutti_midi_device::CiMessage`, framed with `tutti_ump::frame` into the MIDI
//! 1.0 byte stream, `Midi1Parser`, UMP SysEx7 packets, `SysexAssembler`, a typed
//! `CiMessage` equal to the one sent.
//!
//! Mutation (run): `Midi1Parser` pushing a packet at five bytes instead of six,
//! and its last packet always `Complete` instead of `End` -> fails each time.
//! `frame` omitting the closing `0xF7` -> fails (the message never completes).
//! `CiMessage::encode` dropping the Output Path ID byte -> fails (the decoded
//! message differs).

use tutti_midi_device::{
    CiCategories, CiHeader, CiMessage, Decoded, DiscoveryData, MaxSysexSize, Muid,
};
use tutti_ump::{Message, Midi1Parser, MidiGroup, SysexAssembler, SysexStep, U7, Ump, frame};

#[test]
fn midi1_wire_sysex_promotes_to_a_typed_ci_message() {
    // A Discovery probe exactly as a peer device would send it. A Discovery, not
    // a reply: section 5.5.4 makes the path id the initiator's own, and
    // `function_block` is reply-only, so `None` is the value
    // section 5.6.2 names for a device that represents none.
    let probe = CiMessage::Discovery {
        header: CiHeader::new(Muid::new(0x1234).expect("a valid MUID")),
        data: DiscoveryData {
            manufacturer: [0x00, 0x21, 0x09],
            family: 0x0042,
            family_model: 0x0007,
            software_revision: [1, 2, 3, 4],
            categories: CiCategories::PROFILE_CONFIGURATION,
            max_sysex_size: MaxSysexSize::of(512),
            output_path_id: U7::of(3),
            function_block: None,
        },
    };

    // Flatten it to the MIDI-1.0 byte stream a DIN or serial port delivers:
    // F0 <payload> F7.
    let payload = probe.encode();
    let wire: Vec<u8> = frame(&payload).collect();
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
            SysexStep::Complete(at) => Some(CiMessage::decode(assembler.message(at).payload())),
            _ => None,
        });

    assert_eq!(
        decoded,
        Some(Decoded::Message(probe)),
        "a CI probe off the MIDI-1 wire must arrive intact"
    );
}
