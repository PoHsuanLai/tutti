//! `flex_text_packets`: a text as Flex Data packets (M2-104 section 7.5.9:
//! Complete for up to 12 bytes, else Start, Continue, End, the last padded
//! with zeros).

use tutti_ump::{
    Flex, FlexAddress, FlexTextKind, Form, Message, MidiGroup, decode, flex_text_packets,
};

fn packets(text: &str) -> Vec<(Form, [u8; 12])> {
    flex_text_packets(
        MidiGroup::of(3),
        FlexAddress::Group,
        FlexTextKind::Lyrics,
        text,
    )
    .unwrap()
    .map(|ump| {
        let Ok(Message::Flex(
            group,
            Flex::Text {
                address,
                kind,
                form,
                bytes,
            },
        )) = decode(ump)
        else {
            panic!("not Flex text");
        };
        assert_eq!(group, MidiGroup::of(3));
        assert_eq!(address, FlexAddress::Group);
        assert_eq!(kind, FlexTextKind::Lyrics);
        (form, bytes)
    })
    .collect()
}

#[test]
fn twelve_bytes_are_one_complete_packet() {
    assert_eq!(
        packets("twelve bytes"),
        [(Form::Complete, *b"twelve bytes")]
    );
}

#[test]
fn an_empty_text_is_one_complete_packet_of_zeros() {
    assert_eq!(packets(""), [(Form::Complete, [0; 12])]);
}

#[test]
fn a_long_text_is_start_continue_end() {
    let got = packets("0123456789ab0123456789abXYZ");
    assert_eq!(got.len(), 3);
    assert_eq!(got[0], (Form::Start, *b"0123456789ab"));
    assert_eq!(got[1], (Form::Continue, *b"0123456789ab"));
    assert_eq!(got[2].0, Form::End);
}

#[test]
fn the_last_packet_is_padded_with_zeros() {
    let got = packets("0123456789abXYZ");
    assert_eq!(got[1], (Form::End, *b"XYZ\0\0\0\0\0\0\0\0\0"));
}

#[test]
fn the_length_is_exact() {
    for (text, n) in [
        ("", 1),
        ("a", 1),
        ("0123456789ab", 1),
        ("0123456789abc", 2),
        ("0123456789ab0123456789ab", 2),
    ] {
        let it = flex_text_packets(
            MidiGroup::of(0),
            FlexAddress::Group,
            FlexTextKind::ClipName,
            text,
        )
        .unwrap();
        assert_eq!(it.len(), n, "{text}");
        assert_eq!(it.count(), n, "{text}");
    }
}

/// Zero is the padding, so a text holding one is refused.
#[test]
fn a_nul_is_refused() {
    assert!(
        flex_text_packets(
            MidiGroup::of(0),
            FlexAddress::Group,
            FlexTextKind::Lyrics,
            "a\0b"
        )
        .is_none()
    );
}
