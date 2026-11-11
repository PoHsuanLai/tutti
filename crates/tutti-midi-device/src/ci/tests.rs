use super::*;

fn muid(raw: u32) -> Muid {
    Muid::new(raw).expect("a valid MUID")
}

#[test]
fn muid_refuses_the_reserved_range_and_the_broadcast_value() {
    assert_eq!(Muid::new(0), Some(Muid(0)));
    assert_eq!(Muid::new(0x0FFF_FEFF).map(Muid::get), Some(0x0FFF_FEFF));
    // The reserved range, both ends, and the broadcast value (§3.3).
    for raw in [0x0FFF_FF00, 0x0FFF_FF01, 0x0FFF_FFFE, Muid::BROADCAST_RAW] {
        assert_eq!(Muid::new(raw), None, "{raw:#x}");
        assert_eq!(Muid::try_from(raw), Err(NotAMuid(raw)));
    }
    // Past 28 bits.
    assert_eq!(Muid::new(0x1000_0000), None);
    assert_eq!(Muid::new(u32::MAX), None);
    assert_eq!(u32::from(muid(0x0123_4567)), 0x0123_4567);
    assert_eq!(Muid::try_from(7), Ok(Muid(7)));
}

#[test]
fn muid_round_trips_through_wire_bytes() {
    for raw in [0u32, 1, 0x0123_4567, 0x0FFF_FEFF] {
        let m = muid(raw);
        assert_eq!(Muid::from_bytes(m.to_bytes()), Some(m));
    }
    // Least significant seven bits first.
    assert_eq!(muid(0x0123_4567).to_bytes(), [0x67, 0x0A, 0x0D, 0x09]);
    // A byte with its top bit set, and the reserved range, are not a MUID.
    assert_eq!(Muid::from_bytes([0x80, 0, 0, 0]), None);
    assert_eq!(Muid::from_bytes([0x7F, 0x7F, 0x7F, 0x7F]), None);
    assert_eq!(Muid::from_bytes([0x00, 0x7E, 0x7F, 0x7F]), None);
}

fn header() -> CiHeader {
    CiHeader::new(muid(0x1))
}

fn sample() -> DiscoveryData {
    DiscoveryData::new(
        [0x00, 0x21, 0x09],
        0x1234,
        0x0055,
        [1, 2, 3, 4],
        CiCategories::PROFILE_CONFIGURATION,
        MaxSysexSize::of(512),
    )
}

/// A broadcast Invalidate MUID with the target `5`, as bytes.
fn invalidate() -> Vec<u8> {
    CiMessage::InvalidateMuid {
        header: header(),
        target: muid(5),
    }
    .encode()
}

fn unread(bytes: &[u8]) -> Unread {
    match CiMessage::decode(bytes) {
        Decoded::Unread(unread) => unread,
        other => panic!("expected an Unread, got {other:?}"),
    }
}

fn message(bytes: &[u8]) -> CiMessage {
    match CiMessage::decode(bytes) {
        Decoded::Message(message) => message,
        other => panic!("expected a message, got {other:?}"),
    }
}

#[test]
fn invalidate_muid_round_trips() {
    let msg = CiMessage::InvalidateMuid {
        header: header(),
        target: muid(0x0ABC_DEF0 & 0x0FFF_FFFF),
    };
    let bytes = msg.encode();
    assert_eq!(
        &bytes[..5],
        [0x7E, 0x7F, 0x0D, 0x7E, CI_VERSION.get()],
        "§5.2.1 Table 5"
    );
    assert_eq!(&bytes[9..13], [0x7F; 4], "broadcast destination");
    assert_eq!(message(&bytes), msg);
}

#[test]
fn a_reply_keeps_the_device_it_goes_to() {
    let msg = CiMessage::Nak {
        header: header(),
        to: muid(0x0042_4242),
        nak: Nak::new(U7::of(0x20), Nak::STATUS_MESSAGE_NOT_SUPPORTED),
    };
    let bytes = msg.encode();
    assert_eq!(&bytes[9..13], muid(0x0042_4242).to_bytes());
    assert_eq!(message(&bytes), msg);
    assert_eq!(msg.destination(), Destination::Muid(muid(0x0042_4242)));
}

#[test]
fn non_ci_sysex_is_not_ci() {
    // Not `7E <id> 0D ...`: a manufacturer message, a different Universal
    // SysEx sub-ID#1, and a message too short for the preamble.
    assert_eq!(CiMessage::decode(&[0x01, 0x02, 0x03]), Decoded::NotCi);
    let mut other = invalidate();
    other[2] = 0x06;
    assert_eq!(CiMessage::decode(&other), Decoded::NotCi);
    assert_eq!(CiMessage::decode(&invalidate()[..12]), Decoded::NotCi);
}

// §5.3 and §5.11: a device owes a NAK to a message it does not understand, so a
// message that is not read keeps the header it was addressed by. This is the
// Unread of a Profile Inquiry (0x20), a kind this codec does not model.
#[test]
fn a_message_that_is_not_read_keeps_its_header() {
    let mut bytes = invalidate();
    bytes[1] = 0x03; // Device ID: channel 4
    bytes[3] = 0x20; // Profile Inquiry
    bytes[4] = 0x01;
    let got = unread(&bytes);
    assert_eq!(
        got,
        Unread {
            device_id: 0x03,
            sub_id2: 0x20,
            version: 0x01,
            source: Some(muid(1)),
            destination: Some(Destination::Broadcast),
            reason: UnreadReason::UnsupportedMessage,
        }
    );
    // The NAK is addressed to the sender, from `own`, on the same Device ID,
    // naming the message and "MIDI-CI message not supported" (Table 16 0x01).
    let reply = got.nak(muid(0x77)).expect("the sender is known");
    assert_eq!(
        reply,
        CiMessage::Nak {
            header: CiHeader {
                device_id: CiDeviceId::Channel(MidiChannel::of(3)),
                ci_version: CI_VERSION,
                source: muid(0x77),
            },
            to: muid(1),
            nak: Nak::new(U7::of(0x20), U7::of(0x01)),
        }
    );
    assert_eq!(message(&reply.encode()), reply);
}

// §5.3: "If any of the reserved bits of the Message Format Version field are
// set, the Device shall reply with a NAK message (error code 0x02)"; §5.4: "A
// Receiver of a Discovery message (or any other message) with the reserved
// version bits set, reply with a NAK message using the Receiver's Message Format
// Version and NAK Status Code set to 0x02."
#[test]
fn reserved_version_bits_are_nakked_with_0x02_in_the_receivers_version() {
    for version in [0x20, 0x40, 0x80, 0xE2, 0x7F] {
        let mut bytes = invalidate();
        bytes[4] = version;
        let got = unread(&bytes);
        assert_eq!(
            got.reason,
            UnreadReason::ReservedVersionBits,
            "{version:#x}"
        );
        assert_eq!(got.version, version, "the version as sent is kept");
        let CiMessage::Nak { header, nak, .. } = got.nak(muid(9)).unwrap() else {
            panic!("a NAK")
        };
        assert_eq!(nak.status_code, U7::of(0x02));
        assert_eq!(header.ci_version, CI_VERSION);
    }
}

// §5.4: "Message Format Version 0x00 is deprecated and shall not be used."
#[test]
fn version_zero_is_refused() {
    let mut bytes = invalidate();
    bytes[4] = 0x00;
    assert_eq!(unread(&bytes).reason, UnreadReason::VersionZero);
}

// §5.3: "If the received version is higher than the Device's supported
// version, the Device shall only process the fields defined for its supported
// version and ignores any appended fields": a version the reserved bits do not
// forbid is read, with what it appended.
#[test]
fn a_higher_version_is_read_and_its_appended_fields_ignored() {
    let mut bytes = invalidate();
    bytes[4] = 0x05;
    bytes.extend_from_slice(&[0x11, 0x22]);
    let CiMessage::InvalidateMuid { header, target } = message(&bytes) else {
        panic!("Invalidate MUID")
    };
    assert_eq!(header.ci_version, CiVersion::of(5));
    assert_eq!(target, muid(5));
}

// §3.2.1 Table 5: "10-7D: Reserved".
#[test]
fn a_reserved_device_id_is_refused() {
    for id in [0x10, 0x11, 0x40, 0x7D] {
        let mut bytes = invalidate();
        bytes[1] = id;
        let got = unread(&bytes);
        assert_eq!(got.reason, UnreadReason::ReservedDeviceId, "{id:#x}");
        // The NAK still goes out, on the Function Block.
        let CiMessage::Nak { header, nak, .. } = got.nak(muid(9)).unwrap() else {
            panic!("a NAK")
        };
        assert_eq!(header.device_id, CiDeviceId::FunctionBlock);
        assert_eq!(nak.status_code, Nak::STATUS_MALFORMED);
    }
    for id in (0x00..=0x0F).chain([0x7E, 0x7F]) {
        let mut bytes = invalidate();
        bytes[1] = id;
        let CiMessage::InvalidateMuid { header, .. } = message(&bytes) else {
            panic!("Invalidate MUID")
        };
        assert_eq!(header.device_id.get(), id);
    }
}

#[test]
fn a_source_no_device_can_own_is_unread_with_nobody_to_answer() {
    // The broadcast value as the source (bytes 5 to 8) is malformed; as the
    // destination it is fine.
    let mut bytes = invalidate();
    bytes[5..9].copy_from_slice(&[0x7F; 4]);
    let got = unread(&bytes);
    assert_eq!(got.reason, UnreadReason::BadSource);
    assert_eq!(got.source, None);
    assert_eq!(got.nak(muid(9)), None);
    // A reserved value as the invalidated target is malformed too, but the
    // header is fine, so it can be answered.
    let mut bytes = invalidate();
    bytes[13..17].copy_from_slice(&[0x00, 0x7E, 0x7F, 0x7F]);
    let got = unread(&bytes);
    assert_eq!(got.reason, UnreadReason::Malformed);
    assert!(got.nak(muid(9)).is_some());
    // A reserved destination.
    let mut bytes = invalidate();
    bytes[9..13].copy_from_slice(&[0x00, 0x7E, 0x7F, 0x7F]);
    assert_eq!(unread(&bytes).reason, UnreadReason::BadDestination);
}

// §3.3.3: "The Broadcast MUID shall be used only for the following MIDI-CI
// Messages: Discovery, Invalidate MUID, ..."; Tables 6 and 12 give those two the
// Broadcast MUID as destination. So the other three kinds refuse it, and those
// two refuse a unicast one.
#[test]
fn the_broadcast_rule_is_held_in_both_directions() {
    let mut discovery_to_one = CiMessage::Discovery {
        header: header(),
        data: sample(),
    }
    .encode();
    discovery_to_one[9..13].copy_from_slice(&muid(7).to_bytes());
    assert_eq!(
        unread(&discovery_to_one).reason,
        UnreadReason::WrongDestination
    );

    let mut invalidate_to_one = invalidate();
    invalidate_to_one[9..13].copy_from_slice(&muid(7).to_bytes());
    assert_eq!(
        unread(&invalidate_to_one).reason,
        UnreadReason::WrongDestination
    );

    for msg in [
        CiMessage::DiscoveryReply {
            header: header(),
            to: muid(7),
            data: sample(),
        },
        CiMessage::Ack {
            header: header(),
            to: muid(7),
            ack: Ack::new(U7::of(0x34), Ack::STATUS_ACK),
        },
        CiMessage::Nak {
            header: header(),
            to: muid(7),
            nak: Nak::new(U7::of(0x34), Nak::STATUS_NAK),
        },
    ] {
        let mut bytes = msg.encode();
        assert_eq!(message(&bytes), msg);
        bytes[9..13].copy_from_slice(&[0x7F; 4]);
        assert_eq!(
            unread(&bytes).reason,
            UnreadReason::WrongDestination,
            "{msg:?}"
        );
    }
}

// An ACK or a NAK is never answered with a NAK.
#[test]
fn an_unreadable_ack_or_nak_is_not_nakked() {
    for kind in [SUB_ID2_ACK, SUB_ID2_NAK] {
        let mut bytes = invalidate();
        bytes[3] = kind;
        bytes.truncate(CiHeader::LEN); // a unicast body is missing: malformed
        bytes[9..13].copy_from_slice(&muid(7).to_bytes());
        bytes.extend_from_slice(&[0x01, 0x00, 0x00, 0, 0, 0, 0, 0, 0x05, 0x00]); // ml 5, no text
        let got = unread(&bytes);
        assert_eq!(got.reason, UnreadReason::Malformed);
        assert_eq!(got.nak(muid(9)), None);
    }
}

#[cfg(feature = "serde")]
#[test]
fn the_device_id_and_version_are_range_checked_values() {
    use tutti_fixtures::{RangeContract, check_range_contract};
    check_range_contract(&RangeContract {
        new: CiDeviceId::new,
        get: CiDeviceId::get,
        of: None,
        valid: &[0x00, 0x05, 0x0F, 0x7E, 0x7F],
        invalid: &[0x10, 0x7D, 0x80, 0xFF],
    });
    check_range_contract(&RangeContract {
        new: CiVersion::new,
        get: CiVersion::get,
        of: Some(CiVersion::of),
        valid: &[0x01, 0x02, 0x1F],
        invalid: &[0x00, 0x20, 0x80, 0xFF],
    });
}
