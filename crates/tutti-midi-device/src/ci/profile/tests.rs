use proptest::prelude::*;
use tutti_ump::{ManufacturerId, MidiChannel, U7};

use super::super::*;

const SRC: u32 = 0x0123_4567;
const DST: u32 = 0x0007_7777;

fn muid(raw: u32) -> Muid {
    Muid::new(raw).unwrap()
}

fn header(device_id: CiDeviceId) -> CiHeader {
    CiHeader {
        device_id,
        ci_version: CI_VERSION,
        source: muid(SRC),
    }
}

/// `7E <id> 0D <sub> 02 src dst`, the preamble of a Version 2 message.
fn preamble(device_id: u8, sub: u8, destination: [u8; 4]) -> Vec<u8> {
    let mut bytes = vec![0x7E, device_id, 0x0D, sub, 0x02];
    bytes.extend(muid(SRC).to_bytes());
    bytes.extend(destination);
    bytes
}

fn to_dst() -> [u8; 4] {
    muid(DST).to_bytes()
}

const BROADCAST: [u8; 4] = [0x7F; 4];

fn piano() -> ProfileId {
    ProfileId::standard(0x01, 0x02, 0x03, 0x04)
}

fn vendor() -> ProfileId {
    ProfileId::from_bytes([0x00, 0x21, 0x09, 0x05, 0x06]).unwrap()
}

fn one_byte_vendor() -> ProfileId {
    ProfileId::from_bytes([0x41, 0x00, 0x00, 0x07, 0x08]).unwrap()
}

fn decoded(bytes: &[u8]) -> CiMessage {
    match CiMessage::decode(bytes) {
        Decoded::Message(message) => message,
        other => panic!("a message, got {other:?}"),
    }
}

fn unread(bytes: &[u8]) -> Unread {
    match CiMessage::decode(bytes) {
        Decoded::Unread(unread) => unread,
        other => panic!("an Unread, got {other:?}"),
    }
}

fn channel(n: u8) -> CiDeviceId {
    CiDeviceId::Channel(MidiChannel::of(n))
}

impl ProfileId {
    fn standard(bank: u8, number: u8, version: u8, level: u8) -> Self {
        Self::Standard(StandardProfile::new(
            U7::of(bank),
            U7::of(number),
            U7::of(version),
            U7::of(level),
        ))
    }
}

#[test]
fn a_standard_profile_id_is_7e_bank_number_version_level() {
    // §7.3.1, Table 19.
    assert_eq!(piano().to_bytes(), [0x7E, 0x01, 0x02, 0x03, 0x04]);
    let ProfileId::Standard(p) = piano() else {
        unreachable!()
    };
    assert_eq!(
        (p.bank(), p.number(), p.version(), p.level()),
        (U7::of(1), U7::of(2), U7::of(3), U7::of(4))
    );
}

#[test]
fn a_manufacturer_profile_id_is_its_sysex_id_then_two_info_bytes() {
    // §7.3.1: a three-byte id fills bytes 1 to 3; a one-byte id "is in the first byte
    // and the remaining two bytes are filled with zeroes".
    let ProfileId::Manufacturer(p) = vendor() else {
        panic!("a manufacturer's")
    };
    assert_eq!(p.manufacturer(), ManufacturerId::of(0xA109));
    assert_eq!(p.info(), [U7::of(5), U7::of(6)]);
    assert_eq!(vendor().to_bytes(), [0x00, 0x21, 0x09, 0x05, 0x06]);
    let ProfileId::Manufacturer(p) = one_byte_vendor() else {
        panic!("a manufacturer's")
    };
    assert_eq!(p.manufacturer(), ManufacturerId::of(0x41));
    assert_eq!(one_byte_vendor().to_bytes(), [0x41, 0, 0, 0x07, 0x08]);
}

#[test]
fn a_profile_id_that_is_not_one_is_refused() {
    // A one-byte id followed by a non-zero byte, and a byte with its top bit set,
    // in each position.
    assert_eq!(ProfileId::from_bytes([0x41, 0x01, 0x00, 0, 0]), None);
    assert_eq!(ProfileId::from_bytes([0x41, 0x00, 0x01, 0, 0]), None);
    for i in 0..5 {
        let mut bytes = [0x7E, 1, 2, 3, 4];
        bytes[i] = 0x80;
        assert_eq!(ProfileId::from_bytes(bytes), None, "byte {i}");
    }
    // A manufacturer with the id `7E` would read back as a Standard Profile.
    assert_eq!(
        ManufacturerProfile::new(ManufacturerId::UNIVERSAL_NON_REAL_TIME, [U7::of(0); 2]),
        None
    );
}

#[test]
fn the_inquiry_is_table_17_byte_for_byte() {
    // Device ID is the address asked; no body.
    let message = CiMessage::ProfileInquiry {
        header: header(channel(3)),
        to: muid(DST),
    };
    let want = preamble(0x03, 0x20, to_dst());
    assert_eq!(message.encode(), want);
    assert_eq!(decoded(&want), message);
    assert_eq!(SUB_ID2_PROFILE_INQUIRY, 0x20);
    assert_eq!(message.destination(), Destination::Muid(muid(DST)));
}

#[test]
fn the_reply_is_table_18_enabled_list_then_disabled_list() {
    let message = CiMessage::ProfileReply {
        header: header(CiDeviceId::Group),
        to: muid(DST),
        profiles: ProfileLists::new(vec![piano(), vendor()], vec![one_byte_vendor()]).unwrap(),
    };
    let mut want = preamble(0x7E, 0x21, to_dst());
    want.extend([2, 0]); // cep, LSB first
    want.extend([0x7E, 1, 2, 3, 4]);
    want.extend([0x00, 0x21, 0x09, 5, 6]);
    want.extend([1, 0]); // cdp
    want.extend([0x41, 0, 0, 7, 8]);
    assert_eq!(message.encode(), want);
    let CiMessage::ProfileReply { profiles, .. } = decoded(&want) else {
        panic!("a reply")
    };
    assert_eq!(profiles.enabled(), [piano(), vendor()]);
    assert_eq!(profiles.disabled(), [one_byte_vendor()]);
}

#[test]
fn a_reply_declaring_no_profiles_is_two_zero_counts() {
    // §7.13.
    let message = CiMessage::ProfileReply {
        header: header(CiDeviceId::FunctionBlock),
        to: muid(DST),
        profiles: ProfileLists::none(),
    };
    let mut want = preamble(0x7F, 0x21, to_dst());
    want.extend([0, 0, 0, 0]);
    assert_eq!(message.encode(), want);
    assert_eq!(decoded(&want), message);
}

#[test]
fn a_reply_whose_count_runs_past_the_message_is_malformed() {
    let mut bytes = preamble(0x7F, 0x21, to_dst());
    bytes.extend([2, 0, 0x7E, 1, 2, 3, 4]); // two enabled declared, one there
    assert_eq!(unread(&bytes).reason, UnreadReason::Malformed);
    let mut bytes = preamble(0x7F, 0x21, to_dst());
    bytes.extend([0, 0]); // no second count
    assert_eq!(unread(&bytes).reason, UnreadReason::Malformed);
}

#[test]
fn a_list_of_more_than_14_bits_is_refused_and_the_most_is_written() {
    let id = piano();
    assert_eq!(
        ProfileLists::new(vec![id; 0x4000], Vec::new()),
        Err(ListTooLong { len: 0x4000 })
    );
    assert_eq!(
        ProfileLists::new(Vec::new(), vec![id; 0x4000]),
        Err(ListTooLong { len: 0x4000 })
    );
    let full = ProfileLists::new(vec![id; 0x3FFF], Vec::new()).unwrap();
    let message = CiMessage::ProfileReply {
        header: header(CiDeviceId::FunctionBlock),
        to: muid(DST),
        profiles: full.clone(),
    };
    let bytes = message.encode();
    assert_eq!(&bytes[13..15], [0x7F, 0x7F]);
    let CiMessage::ProfileReply { profiles, .. } = decoded(&bytes) else {
        panic!("a reply")
    };
    assert_eq!(profiles, full);
}

#[test]
fn set_on_is_table_24_with_the_channels_requested_lsb_first() {
    let message = CiMessage::SetProfileOn {
        header: header(channel(0)),
        to: muid(DST),
        profile: piano(),
        channels: ProfileChannels::of(130),
    };
    let mut want = preamble(0x00, 0x22, to_dst());
    want.extend([0x7E, 1, 2, 3, 4]);
    want.extend([0x02, 0x01]); // 130 = 1 << 7 | 2
    assert_eq!(message.encode(), want);
    assert_eq!(decoded(&want), message);
}

#[test]
fn a_version_1_set_on_has_no_channels_field_and_reads_as_zero() {
    // §5.3: "Number Channels Requested" was added in Version 2.
    let mut bytes = preamble(0x7F, 0x22, to_dst());
    bytes[4] = 0x01;
    bytes.extend([0x7E, 1, 2, 3, 4]);
    let CiMessage::SetProfileOn { channels, .. } = decoded(&bytes) else {
        panic!("a Set On")
    };
    assert_eq!(channels, ProfileChannels::BY_ADDRESS);
    // One byte of the two is malformed, not absent.
    bytes.push(0x05);
    assert_eq!(unread(&bytes).reason, UnreadReason::Malformed);
}

#[test]
fn set_off_is_table_25_with_two_reserved_bytes_that_are_ignored() {
    let message = CiMessage::SetProfileOff {
        header: header(CiDeviceId::Group),
        to: muid(DST),
        profile: piano(),
    };
    let mut want = preamble(0x7E, 0x23, to_dst());
    want.extend([0x7E, 1, 2, 3, 4, 0, 0]);
    assert_eq!(message.encode(), want);
    assert_eq!(decoded(&want), message);
    // "Reserved": whatever a sender puts there, the message is the same.
    let n = want.len();
    want[n - 2..].copy_from_slice(&[0x55, 0x66]);
    assert_eq!(decoded(&want), message);
}

#[test]
fn enabled_and_disabled_reports_are_tables_26_and_27_to_the_broadcast_muid() {
    for (sub, enabled) in [(0x24u8, true), (0x25, false)] {
        let (header, profile, channels) = (header(channel(9)), piano(), ProfileChannels::of(3));
        let message = if enabled {
            CiMessage::ProfileEnabled {
                header,
                profile,
                channels,
            }
        } else {
            CiMessage::ProfileDisabled {
                header,
                profile,
                channels,
            }
        };
        let mut want = preamble(0x09, sub, BROADCAST);
        want.extend([0x7E, 1, 2, 3, 4, 3, 0]);
        assert_eq!(message.encode(), want, "{sub:#x}");
        assert_eq!(decoded(&want), message);
        assert_eq!(message.destination(), Destination::Broadcast);
        // "Shall always have the Destination MUID field set to the Broadcast MUID."
        let mut unicast = want.clone();
        unicast[9..13].copy_from_slice(&to_dst());
        assert_eq!(unread(&unicast).reason, UnreadReason::WrongDestination);
    }
}

#[test]
fn a_version_1_report_has_no_channel_count_and_reads_as_zero() {
    let mut bytes = preamble(0x7F, 0x24, BROADCAST);
    bytes[4] = 0x01;
    bytes.extend([0x7E, 1, 2, 3, 4]);
    let CiMessage::ProfileEnabled {
        header, channels, ..
    } = decoded(&bytes)
    else {
        panic!("a report")
    };
    assert_eq!(header.ci_version.get(), 1);
    assert_eq!(channels, ProfileChannels::BY_ADDRESS);
}

#[test]
fn added_and_removed_reports_are_tables_20_and_21_to_the_broadcast_muid() {
    let added = CiMessage::ProfileAdded {
        header: header(CiDeviceId::FunctionBlock),
        profile: vendor(),
    };
    let removed = CiMessage::ProfileRemoved {
        header: header(CiDeviceId::FunctionBlock),
        profile: vendor(),
    };
    for (message, sub) in [(&added, 0x26u8), (&removed, 0x27)] {
        let mut want = preamble(0x7F, sub, BROADCAST);
        want.extend([0x00, 0x21, 0x09, 5, 6]);
        assert_eq!(message.encode(), want);
        assert_eq!(&decoded(&want), message);
        let mut unicast = want.clone();
        unicast[9..13].copy_from_slice(&to_dst());
        assert_eq!(unread(&unicast).reason, UnreadReason::WrongDestination);
    }
}

#[test]
fn the_details_inquiry_is_table_22_and_the_reply_table_23() {
    let inquiry = CiMessage::ProfileDetailsInquiry {
        header: header(channel(2)),
        to: muid(DST),
        profile: piano(),
        target: InquiryTarget::NUMBER_OF_CHANNELS,
    };
    let mut want = preamble(0x02, 0x28, to_dst());
    want.extend([0x7E, 1, 2, 3, 4, 0x00]);
    assert_eq!(inquiry.encode(), want);
    assert_eq!(decoded(&want), inquiry);

    let data = ChannelsDetail {
        in_use: ProfileChannels::of(0),
        max: ProfileChannels::of(200),
    }
    .to_data();
    let reply = CiMessage::ProfileDetailsReply {
        header: header(channel(2)),
        to: muid(DST),
        profile: piano(),
        target: InquiryTarget::NUMBER_OF_CHANNELS,
        data: data.clone(),
    };
    let mut want = preamble(0x02, 0x29, to_dst());
    want.extend([0x7E, 1, 2, 3, 4, 0x00]);
    // M2-102 Table 8: length 4, in use 0, maximum 200 = 1 << 7 | 72.
    want.extend([0x04, 0x00, 0x00, 0x00, 72, 1]);
    assert_eq!(reply.encode(), want);
    let CiMessage::ProfileDetailsReply { data: got, .. } = decoded(&want) else {
        panic!("a reply")
    };
    assert_eq!(got.as_channels().unwrap().max, ProfileChannels::of(200));
    assert_eq!(got.as_channels().unwrap().in_use, ProfileChannels::of(0));
    assert_eq!(got, data);
}

#[test]
fn a_details_reply_whose_length_runs_past_the_message_or_holds_a_non_data_byte_is_malformed() {
    let mut bytes = preamble(0x02, 0x29, to_dst());
    bytes.extend([0x7E, 1, 2, 3, 4, 0x40, 0x03, 0x00, 1, 2]); // 3 declared, 2 there
    assert_eq!(unread(&bytes).reason, UnreadReason::Malformed);
    let mut bytes = preamble(0x02, 0x29, to_dst());
    bytes.extend([0x7E, 1, 2, 3, 4, 0x40, 0x01, 0x00, 0x80]);
    assert_eq!(unread(&bytes).reason, UnreadReason::Malformed);
}

#[test]
fn profile_specific_data_is_table_28_with_a_four_byte_length() {
    let data = SpecificData::new(vec![1, 2, 3]).unwrap();
    for (to, wire) in [
        (Destination::Muid(muid(DST)), to_dst()),
        (Destination::Broadcast, BROADCAST),
    ] {
        let message = CiMessage::ProfileSpecificData {
            header: header(channel(15)),
            to,
            profile: vendor(),
            data: data.clone(),
        };
        let mut want = preamble(0x0F, 0x2F, wire);
        want.extend([0x00, 0x21, 0x09, 5, 6]);
        want.extend([3, 0, 0, 0]);
        want.extend([1, 2, 3]);
        assert_eq!(message.encode(), want);
        assert_eq!(decoded(&want), message);
        assert_eq!(message.destination(), to);
    }
    // A length of 128 is `00 01 00 00`.
    let long = CiMessage::ProfileSpecificData {
        header: header(channel(0)),
        to: Destination::Muid(muid(DST)),
        profile: piano(),
        data: SpecificData::new(vec![9; 128]).unwrap(),
    };
    assert_eq!(&long.encode()[18..22], [0x00, 0x01, 0x00, 0x00]);
    assert_eq!(decoded(&long.encode()), long);
}

#[test]
fn profile_specific_data_that_stops_short_or_declares_a_huge_length_is_malformed() {
    let mut bytes = preamble(0x0F, 0x2F, to_dst());
    bytes.extend([0x7E, 1, 2, 3, 4, 5, 0, 0, 0, 1, 2]); // 5 declared, 2 there
    assert_eq!(unread(&bytes).reason, UnreadReason::Malformed);
    let mut bytes = preamble(0x0F, 0x2F, to_dst());
    bytes.extend([0x7E, 1, 2, 3, 4, 0x7F, 0x7F, 0x7F, 0x7F]); // 2^28 - 1 declared
    assert_eq!(unread(&bytes).reason, UnreadReason::Malformed);
}

#[test]
fn data_that_cannot_be_carried_is_refused_at_construction() {
    assert_eq!(
        SpecificData::new(vec![0x80]),
        Err(ProfileDataError::NotSevenBit)
    );
    assert_eq!(
        DetailsData::new(vec![0; 0x4000]),
        Err(ProfileDataError::TooLong)
    );
    assert!(DetailsData::new(vec![0x7F; 0x3FFF]).is_ok());
}

#[test]
fn an_inquiry_or_a_set_to_the_broadcast_muid_is_for_nobody() {
    // Tables 17, 18, 22 to 25: one device. The broadcast is for the reports.
    for (sub, body) in [
        (0x20u8, vec![]),
        (0x21, vec![0, 0, 0, 0]),
        (0x22, vec![0x7E, 1, 2, 3, 4, 0, 0]),
        (0x23, vec![0x7E, 1, 2, 3, 4, 0, 0]),
        (0x28, vec![0x7E, 1, 2, 3, 4, 0]),
        (0x29, vec![0x7E, 1, 2, 3, 4, 0, 0, 0]),
    ] {
        let mut bytes = preamble(0x7F, sub, BROADCAST);
        bytes.extend(body);
        assert_eq!(
            unread(&bytes).reason,
            UnreadReason::WrongDestination,
            "{sub:#x}"
        );
    }
}

#[test]
fn bytes_after_the_last_field_are_ignored() {
    // §5.3, as for every body.
    let mut bytes = preamble(0x7F, 0x26, BROADCAST);
    bytes.extend([0x7E, 1, 2, 3, 4, 0x55, 0x66]);
    let CiMessage::ProfileAdded { profile, .. } = decoded(&bytes) else {
        panic!("an Added Report")
    };
    assert_eq!(profile, piano());
}

#[test]
fn a_malformed_profile_message_is_unread_and_nakked_0x41_on_its_address() {
    // A Set On with a Profile Id cut short.
    let mut bytes = preamble(0x05, 0x22, to_dst());
    bytes.extend([0x7E, 1, 2]);
    let unread = unread(&bytes);
    assert_eq!(unread.reason, UnreadReason::Malformed);
    let CiMessage::Nak { header, nak, .. } = unread.nak(muid(DST)).unwrap() else {
        panic!("a NAK")
    };
    assert_eq!(header.device_id, channel(5));
    assert_eq!(nak.nak_sub_id2, U7::of(0x22));
    assert_eq!(nak.status_code, Nak::STATUS_MALFORMED);
}

#[test]
fn a_nak_for_an_unsupported_profile_carries_status_04_and_the_profile_id() {
    // §5.11.3: "Profile Configuration - the 5 bytes used here represent the Profile Id".
    let nak = Nak::new(U7::of(0x22), Nak::STATUS_PROFILE_NOT_SUPPORTED)
        .with_details(piano().to_bytes().map(U7::of));
    let message = CiMessage::Nak {
        header: header(channel(1)),
        to: muid(DST),
        nak,
    };
    let bytes = message.encode();
    assert_eq!(&bytes[13..21], [0x22, 0x04, 0x00, 0x7E, 1, 2, 3, 4]);
    assert_eq!(decoded(&bytes), message);
}

#[test]
fn profile_channels_hold_14_bits_and_zero_is_by_address() {
    assert_eq!(
        ProfileChannels::new(0x3FFF),
        Some(ProfileChannels::of(0x3FFF))
    );
    assert_eq!(ProfileChannels::new(0x4000), None);
    assert!(ProfileChannels::BY_ADDRESS.is_by_address());
    assert!(!ProfileChannels::of(1).is_by_address());
}

#[test]
fn registered_targets_are_below_0x40() {
    assert!(InquiryTarget::new(U7::of(0x3F)).is_registered());
    assert!(!InquiryTarget::new(U7::of(0x40)).is_registered());
}

#[test]
fn the_reserved_profile_sub_ids_are_not_read() {
    // Appendix D: 0x2A to 0x2E are reserved.
    for sub in 0x2A..=0x2E {
        let bytes = preamble(0x7F, sub, to_dst());
        assert_eq!(unread(&bytes).reason, UnreadReason::UnsupportedMessage);
    }
}

fn any_profile() -> impl Strategy<Value = ProfileId> {
    prop_oneof![
        (0u8..0x80, 0u8..0x80, 0u8..0x80, 0u8..0x80)
            .prop_map(|(a, b, c, d)| ProfileId::standard(a, b, c, d)),
        (0u8..0x80, 0u8..0x80, 0u8..0x80, 0u8..0x80, 0u8..0x80).prop_filter_map(
            "a manufacturer id",
            |(a, b, c, d, e)| {
                let bytes = if a == 0 {
                    [0, b, c, d, e]
                } else {
                    [a, 0, 0, d, e]
                };
                ProfileId::from_bytes(bytes).filter(|id| matches!(id, ProfileId::Manufacturer(_)))
            }
        ),
    ]
}

fn any_address() -> impl Strategy<Value = CiDeviceId> {
    prop_oneof![
        (0u8..16).prop_map(|c| CiDeviceId::Channel(MidiChannel::of(c))),
        Just(CiDeviceId::Group),
        Just(CiDeviceId::FunctionBlock),
    ]
}

fn any_channels() -> impl Strategy<Value = ProfileChannels> {
    (0u16..0x4000).prop_map(ProfileChannels::of)
}

fn any_data() -> impl Strategy<Value = Vec<u8>> {
    proptest::collection::vec(0u8..0x80, 0..40)
}

fn head() -> impl Strategy<Value = CiHeader> {
    any_address().prop_map(header)
}

fn to() -> impl Strategy<Value = Muid> {
    Just(muid(DST))
}

fn any_message() -> impl Strategy<Value = CiMessage> {
    let lists = (
        proptest::collection::vec(any_profile(), 0..4),
        proptest::collection::vec(any_profile(), 0..4),
    )
        .prop_map(|(e, d)| ProfileLists::new(e, d).unwrap());
    prop_oneof![
        (head(), to()).prop_map(|(header, to)| CiMessage::ProfileInquiry { header, to }),
        (head(), to(), lists).prop_map(|(header, to, profiles)| {
            CiMessage::ProfileReply {
                header,
                to,
                profiles,
            }
        }),
        (head(), to(), any_profile(), any_channels()).prop_map(
            |(header, to, profile, channels)| CiMessage::SetProfileOn {
                header,
                to,
                profile,
                channels
            }
        ),
        (head(), to(), any_profile()).prop_map(|(header, to, profile)| {
            CiMessage::SetProfileOff {
                header,
                to,
                profile,
            }
        }),
        (head(), any_profile(), any_channels()).prop_map(|(header, profile, channels)| {
            CiMessage::ProfileEnabled {
                header,
                profile,
                channels,
            }
        }),
        (head(), any_profile(), any_channels()).prop_map(|(header, profile, channels)| {
            CiMessage::ProfileDisabled {
                header,
                profile,
                channels,
            }
        }),
        (head(), any_profile())
            .prop_map(|(header, profile)| CiMessage::ProfileAdded { header, profile }),
        (head(), any_profile())
            .prop_map(|(header, profile)| CiMessage::ProfileRemoved { header, profile }),
        (head(), to(), any_profile(), 0u8..0x80).prop_map(|(header, to, profile, target)| {
            CiMessage::ProfileDetailsInquiry {
                header,
                to,
                profile,
                target: InquiryTarget::new(U7::of(target)),
            }
        }),
        (head(), to(), any_profile(), 0u8..0x80, any_data()).prop_map(
            |(header, to, profile, target, data)| {
                CiMessage::ProfileDetailsReply {
                    header,
                    to,
                    profile,
                    target: InquiryTarget::new(U7::of(target)),
                    data: DetailsData::new(data).unwrap(),
                }
            }
        ),
        (head(), any::<bool>(), any_profile(), any_data()).prop_map(
            |(header, broadcast, profile, data)| CiMessage::ProfileSpecificData {
                header,
                to: if broadcast {
                    Destination::Broadcast
                } else {
                    Destination::Muid(muid(DST))
                },
                profile,
                data: SpecificData::new(data).unwrap(),
            }
        ),
    ]
}

proptest! {
    #[test]
    fn every_profile_message_decodes_to_what_was_encoded(message in any_message()) {
        let bytes = message.encode();
        prop_assert_eq!(CiMessage::decode(&bytes), Decoded::Message(message.clone()));
        // The destination the message reports is the one on the wire.
        let wire = &bytes[9..13];
        match message.destination() {
            Destination::Broadcast => prop_assert_eq!(wire, &BROADCAST),
            Destination::Muid(m) => prop_assert_eq!(wire, &m.to_bytes()),
        }
        prop_assert_eq!(bytes[3], message.sub_id2());
    }

    #[test]
    fn a_profile_id_round_trips_its_bytes(id in any_profile()) {
        prop_assert_eq!(ProfileId::from_bytes(id.to_bytes()), Some(id));
    }
}
