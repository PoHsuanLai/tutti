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

fn header(destination: Destination) -> CiHeader {
    CiHeader {
        device_id: CI_DEVICE_ID_FUNCTION_BLOCK,
        ci_version: CI_VERSION,
        source: muid(0x1),
        destination,
    }
}

#[test]
fn invalidate_muid_round_trips() {
    let msg = CiMessage::InvalidateMuid {
        header: header(Destination::Broadcast),
        target: muid(0x0ABC_DEF0 & 0x0FFF_FFFF),
    };
    let bytes = msg.encode();
    assert_eq!(&bytes[..5], [0x7E, 0x7F, 0x0D, 0x7E, CI_VERSION]);
    assert_eq!(&bytes[9..13], [0x7F; 4], "broadcast destination");
    assert_eq!(CiMessage::decode(&bytes), Some(msg));
}

#[test]
fn a_message_to_one_device_keeps_its_destination() {
    let msg = CiMessage::InvalidateMuid {
        header: header(Destination::Muid(muid(0x0042_4242))),
        target: muid(5),
    };
    assert_eq!(CiMessage::decode(&msg.encode()), Some(msg));
}

#[test]
fn non_ci_sysex_is_rejected() {
    // Not `7E <id> 0D ...`: a manufacturer message, a different Universal
    // SysEx sub-ID#1, and a message too short for the preamble.
    assert_eq!(CiMessage::decode(&[0x01, 0x02, 0x03]), None);
    let mut other = CiMessage::InvalidateMuid {
        header: header(Destination::Broadcast),
        target: muid(5),
    }
    .encode();
    other[2] = 0x06;
    assert_eq!(CiMessage::decode(&other), None);
}

#[test]
fn a_source_no_device_can_own_is_rejected() {
    let msg = CiMessage::InvalidateMuid {
        header: header(Destination::Broadcast),
        target: muid(5),
    };
    // The broadcast value as the source (bytes 5 to 8) is malformed; as the
    // destination it is fine.
    let mut bytes = msg.encode();
    bytes[5..9].copy_from_slice(&[0x7F; 4]);
    assert_eq!(CiMessage::decode(&bytes), None);
    // A reserved value as the invalidated target is malformed too.
    let mut bytes = msg.encode();
    bytes[13..17].copy_from_slice(&[0x00, 0x7E, 0x7F, 0x7F]);
    assert_eq!(CiMessage::decode(&bytes), None);
}

#[test]
fn sub_ids_this_codec_does_not_model_decode_to_none() {
    // A Profile Inquiry (0x20) with a well-formed preamble.
    let mut bytes = CiMessage::InvalidateMuid {
        header: header(Destination::Broadcast),
        target: muid(5),
    }
    .encode();
    bytes[3] = 0x20;
    assert_eq!(CiMessage::decode(&bytes), None);
}

// Mutations (run), each failing the named test:
// - `Muid::new` accepting `raw <= RESERVED_START`:
//   `muid_refuses_the_reserved_range_and_the_broadcast_value`.
// - `wire_bytes` shifting by 8 instead of 7: `muid_round_trips_through_wire_bytes`.
// - `from_wire_bytes` without the top-bit check: the same test (`0x80`).
// - `CiHeader::decode` not checking `bytes[2] != CI_SUB_ID_1`:
//   `non_ci_sysex_is_rejected`.
// - `Destination::from_bytes` treating broadcast as a `Muid` destination:
//   `invalidate_muid_round_trips`.
