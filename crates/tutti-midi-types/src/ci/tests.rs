use super::*;

#[test]
fn muid_round_trips_through_wire_bytes() {
    for raw in [0u32, 1, 0x0123_4567, 0x0FFF_FFFE] {
        let m = Muid(raw & 0x0FFF_FFFF);
        assert_eq!(Muid::from_bytes(m.to_bytes()), m);
    }
}

#[test]
fn from_seed_masks_to_28_bits_and_dodges_broadcast() {
    assert_eq!(Muid::from_seed(0xFFFF_FFFF).0 & !0x0FFF_FFFF, 0);
    // A seed that would land on broadcast is nudged aside.
    assert_ne!(Muid::from_seed(0x0FFF_FFFF), Muid::BROADCAST);
}

// NOTE: `CiHeader::encode` / `decode` are private, and every `CiMessage`
// round-trip in this module and in `profile` / `property` / `discovery`
// carries a header through both — `invalidate_muid_round_trips_over_sysex7`
// below compares whole messages, header included. A separate header-only
// round-trip asserts nothing those do not.

#[test]
fn invalidate_muid_round_trips_over_sysex7() {
    let msg = CiMessage::InvalidateMuid {
        header: CiHeader {
            device_id: CI_DEVICE_ID_FUNCTION_BLOCK,
            ci_version: CI_VERSION,
            source: Muid(0x1),
            destination: Muid::BROADCAST,
        },
        target: Muid(0x0ABC_DEF0 & 0x0FFF_FFFF),
    };
    let mut events = Vec::new();
    ci_to_sysex7(MidiGroup::FIRST, &msg, &mut events);
    let back = sysex7_to_ci(&events).expect("reassembles");
    assert_eq!(back, msg);
}

#[test]
fn non_ci_sysex_is_rejected() {
    // A random SysEx7 (not starting 0x7E … 0x0D) is not a CI message.
    let mut events = Vec::new();
    crate::ump::sysex7_fragments(MidiGroup::FIRST, &[0x01, 0x02, 0x03], &mut events);
    assert!(sysex7_to_ci(&events).is_none());
}
