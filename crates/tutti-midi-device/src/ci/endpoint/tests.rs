use super::super::*;
use crate::ProductInstanceId;

fn muid(raw: u32) -> Muid {
    Muid::new(raw).unwrap()
}

fn id(text: &str) -> CiInstanceId {
    CiInstanceId::new(ProductInstanceId::new(text).unwrap()).unwrap()
}

fn inquiry() -> CiMessage {
    CiMessage::EndpointInquiry {
        header: CiHeader::new(muid(0x0123_4567)),
        to: muid(0x0007_7777),
        status: EndpointStatus::ProductInstanceId,
    }
}

fn reply(text: &str) -> CiMessage {
    CiMessage::EndpointReply {
        header: CiHeader::new(muid(0x0007_7777)),
        to: muid(0x0123_4567),
        info: EndpointInfo::ProductInstanceId(id(text)),
    }
}

#[test]
fn the_inquiry_is_table_9_byte_for_byte() {
    // 7E 7F 0D 72 ver src dst status.
    let mut want = vec![0x7E, 0x7F, 0x0D, 0x72, 0x02];
    want.extend(muid(0x0123_4567).to_bytes());
    want.extend(muid(0x0007_7777).to_bytes());
    want.push(0x00);
    assert_eq!(inquiry().encode(), want);
    assert_eq!(CiMessage::decode(&want), Decoded::Message(inquiry()));
    assert_eq!(SUB_ID2_ENDPOINT_INQUIRY, 0x72);
}

#[test]
fn the_reply_is_table_11_byte_for_byte() {
    // ... status, lid LSB first, data.
    let mut want = vec![0x7E, 0x7F, 0x0D, 0x73, 0x02];
    want.extend(muid(0x0007_7777).to_bytes());
    want.extend(muid(0x0123_4567).to_bytes());
    want.extend([0x00, 0x03, 0x00]);
    want.extend(*b"A-7");
    assert_eq!(reply("A-7").encode(), want);
    assert_eq!(CiMessage::decode(&want), Decoded::Message(reply("A-7")));
    assert_eq!(SUB_ID2_ENDPOINT_REPLY, 0x73);
}

#[test]
fn an_empty_and_a_sixteen_byte_id_round_trip() {
    for text in ["", "0123456789abcdef"] {
        let bytes = reply(text).encode();
        assert_eq!(CiMessage::decode(&bytes), Decoded::Message(reply(text)));
    }
}

#[test]
fn an_id_over_sixteen_bytes_is_not_a_ci_id_and_is_refused_on_the_wire() {
    // §5.8.3.1: "shall not be any longer than 16 bytes", where M2-104 allows 42.
    let long = ProductInstanceId::new("0123456789abcdefg").unwrap();
    assert_eq!(CiInstanceId::new(long), None);
    let mut bytes = reply("").encode();
    bytes.truncate(bytes.len() - 2);
    bytes.extend([17, 0]);
    bytes.extend(*b"0123456789abcdefg");
    let Decoded::Unread(unread) = CiMessage::decode(&bytes) else {
        panic!("unread");
    };
    assert_eq!(unread.reason, UnreadReason::Malformed);
    assert!(
        unread.nak(muid(1)).is_some(),
        "a NAK can still be addressed"
    );
}

#[test]
fn a_reply_whose_data_is_not_printable_ascii_or_runs_short_is_unread() {
    for tail in [
        &[0x00u8, 0x01, 0x00, 0x1F][..],
        &[0x00, 0x05, 0x00, b'a'][..],
        &[0x00][..],
    ] {
        let mut bytes = reply("").encode();
        bytes.truncate(bytes.len() - 3);
        bytes.extend(tail);
        let Decoded::Unread(unread) = CiMessage::decode(&bytes) else {
            panic!("{tail:?} should be unread");
        };
        assert_eq!(unread.reason, UnreadReason::Malformed, "{tail:?}");
    }
}

#[test]
fn a_reserved_status_is_unread_in_both_messages_and_nakked_generic() {
    // Table 10: 0x01 to 0x7F are reserved.
    for message in [inquiry(), reply("A")] {
        let mut bytes = message.encode();
        let at = 13;
        bytes[at] = 0x01;
        let Decoded::Unread(unread) = CiMessage::decode(&bytes) else {
            panic!("unread");
        };
        assert_eq!(unread.reason, UnreadReason::ReservedStatus);
        let Some(CiMessage::Nak { nak, .. }) = unread.nak(muid(9)) else {
            panic!("a NAK");
        };
        assert_eq!(nak.status_code, Nak::STATUS_NAK);
        assert_eq!(nak.nak_sub_id2.get(), message.sub_id2());
    }
}

#[test]
fn an_endpoint_message_to_the_broadcast_muid_is_refused() {
    // §3.3.3: neither is defined with the Broadcast MUID.
    for message in [inquiry(), reply("A")] {
        let mut bytes = message.encode();
        bytes[9..13].copy_from_slice(&[0x7F; 4]);
        let Decoded::Unread(unread) = CiMessage::decode(&bytes) else {
            panic!("unread");
        };
        assert_eq!(unread.reason, UnreadReason::WrongDestination);
    }
}

#[test]
fn an_inquiry_with_no_status_is_malformed() {
    let mut bytes = inquiry().encode();
    bytes.pop();
    let Decoded::Unread(unread) = CiMessage::decode(&bytes) else {
        panic!("unread");
    };
    assert_eq!(unread.reason, UnreadReason::Malformed);
}

#[test]
fn an_entropy_word_is_a_muid_by_its_low_28_bits_or_none_in_the_reserved_range() {
    assert_eq!(Muid::from_entropy(0xF123_4567), Some(muid(0x0123_4567)));
    assert_eq!(Muid::from_entropy(0), Some(muid(0)));
    assert_eq!(Muid::from_entropy(0x0FFF_FF00), None);
    assert_eq!(Muid::from_entropy(0xFFFF_FFFF), None, "the broadcast value");
    assert_eq!(Muid::from_entropy(0x0FFF_FEFF), Some(muid(0x0FFF_FEFF)));
}
