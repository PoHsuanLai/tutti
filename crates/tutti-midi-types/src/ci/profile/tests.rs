use super::super::*;
use super::*;
use tutti_ump::MidiGroup;

fn header() -> CiHeader {
    CiHeader {
        device_id: CI_DEVICE_ID_FUNCTION_BLOCK,
        ci_version: CI_VERSION,
        source: Muid(0x10),
        destination: Muid(0x20),
    }
}

fn msg(state: ProfileState) -> CiMessage {
    CiMessage::Profile {
        header: header(),
        state,
    }
}

/// Every `ProfileState` survives `encode` → `decode` unchanged.
///
/// One table over the variants rather than a test each: the property is
/// identical in every row, so a per-variant test adds a function name and no
/// assertion. A row per variant also makes an *unlisted* variant visible as
/// a gap in the table, which four separate tests do not.
///
/// Structural round-tripping is the floor, not the ceiling — it only proves
/// the codec agrees with itself. The tests below this one pin what it cannot:
/// spec sub-IDs, byte positions and field ranges, where agreeing with
/// ourselves is exactly the failure.
#[test]
fn every_profile_state_round_trips_through_encode_decode() {
    let id = ProfileId([0x7E, 1, 2, 3, 4]);
    let states = [
        ProfileState::Inquiry,
        ProfileState::InquiryReply {
            enabled: vec![id, ProfileId([0x7E, 5, 6, 7, 8])],
            disabled: vec![ProfileId([0x00, 9, 10, 11, 12])],
        },
        // An empty list is a distinct shape from a populated one: a length
        // written as "at least one" would still round-trip the case above.
        ProfileState::InquiryReply {
            enabled: Vec::new(),
            disabled: Vec::new(),
        },
        ProfileState::Added(id),
        ProfileState::Removed(id),
        ProfileState::Enabled(id),
        ProfileState::Disabled(id),
        ProfileState::SetOn(id),
        ProfileState::SetOff(id),
        ProfileState::DetailsInquiry {
            profile: ProfileId([0x7E, 9, 8, 7, 6]),
            target: 0x01,
        },
        ProfileState::DetailsReply {
            profile: ProfileId([0x7E, 9, 8, 7, 6]),
            target: 0x01,
            data: vec![4, 0, 16],
        },
        // 200 bytes needs the second length byte, which a 1-byte length or a
        // careless mask would drop.
        ProfileState::SpecificData {
            profile: ProfileId([1, 2, 3, 4, 5]),
            data: (0..200u8).map(|b| b & 0x7F).collect(),
        },
    ];

    for state in states {
        let m = msg(state);
        assert_eq!(
            CiMessage::decode(&m.encode()).expect("decodes"),
            m,
            "round trip of {m:?}"
        );
    }
}

#[test]
fn added_and_removed_use_the_spec_sub_ids() {
    // Their bodies are a bare profile id, identical to Enabled/Disabled —
    // so the sub-ID is the only thing distinguishing "I gained this
    // profile" from "I turned it on", two very different claims.
    let id = ProfileId([1, 2, 3, 4, 5]);
    let sub_id = |s: ProfileState| msg(s).encode()[3];
    assert_eq!(sub_id(ProfileState::Added(id)), 0x26);
    assert_eq!(sub_id(ProfileState::Removed(id)), 0x27);
    assert_eq!(sub_id(ProfileState::Enabled(id)), 0x24);
    assert_eq!(sub_id(ProfileState::Disabled(id)), 0x25);
}

#[test]
fn details_target_survives_both_halves_of_its_range() {
    // §7.6.1 splits the target byte: 0x00-0x3F is Registered Target Data
    // (one format across all profiles), 0x40-0x7F is Profile Specific. The
    // codec must carry either untouched — masking to 6 bits, say, would
    // silently turn a profile-specific target into a registered one.
    for target in [0x00, 0x3F, 0x40, 0x7F] {
        let m = msg(ProfileState::DetailsInquiry {
            profile: ProfileId([1, 2, 3, 4, 5]),
            target,
        });
        match CiMessage::decode(&m.encode()).unwrap() {
            CiMessage::Profile {
                state: ProfileState::DetailsInquiry { target: got, .. },
                ..
            } => assert_eq!(got, target),
            other => panic!("expected DetailsInquiry, got {other:?}"),
        }
    }
}

#[test]
fn specific_data_uses_a_four_byte_length() {
    // §7.12 Table 28 gives this length four bytes, unlike the two every
    // other length in this family uses. Assert the byte positions: a
    // 2-byte length here would round-trip through this crate's own codec happily
    // and be unparseable by anything else.
    let m = msg(ProfileState::SpecificData {
        profile: ProfileId([1, 2, 3, 4, 5]),
        data: vec![0xA, 0xB, 0xC],
    });
    let bytes = m.encode();
    let body_at = bytes.len() - 5 - 4 - 3; // profile + len32 + data
    assert_eq!(&bytes[body_at..body_at + 5], &[1, 2, 3, 4, 5]);
    assert_eq!(
        &bytes[body_at + 5..body_at + 9],
        &[3, 0, 0, 0],
        "length is four LSB-first 7-bit bytes"
    );
    assert_eq!(CiMessage::decode(&bytes).unwrap(), m);
}

#[test]
fn set_and_report_round_trip_over_sysex7() {
    for state in [
        ProfileState::SetOn(ProfileId([1, 2, 3, 4, 5])),
        ProfileState::SetOff(ProfileId([1, 2, 3, 4, 5])),
        ProfileState::Enabled(ProfileId([1, 2, 3, 4, 5])),
        ProfileState::Disabled(ProfileId([1, 2, 3, 4, 5])),
    ] {
        let m = msg(state);
        let mut events = Vec::new();
        ci_to_sysex7(MidiGroup::FIRST, &m, &mut events);
        assert_eq!(sysex7_to_ci(&events).expect("reassembles"), m);
    }
}
