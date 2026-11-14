//! The wire format: byte layouts from the specification's tables and
//! examples, the refusals, and sequence numbers.

use tutti_midi_network::{
    AuthChallenge, AuthMethods, AuthState, Bye, ByeReason, Command, DatagramBuilder, Digest,
    Header, Invitation, Nak, NakReason, Nonce, ParseError, PingId, RetransmitErrorReason,
    RetransmitRefusal, RetransmitRequest, SIGNATURE, SequenceNumber, SignatureError, Text, UmpData,
    UmpDataError, UserAuthChallenge, UserAuthState, UserAuthentication, Username, parse,
};
use tutti_ump::Ump;

use crate::sim::identity;

const SIG: [u8; 4] = *b"MIDI";

fn datagram(commands: &[Command]) -> Vec<u8> {
    let mut b = DatagramBuilder::new();
    for c in commands {
        b.push(c);
    }
    b.finish()
}

fn one(bytes: &[u8]) -> Result<Command, tutti_midi_network::Refused> {
    let mut all: Vec<u8> = SIG.to_vec();
    all.extend_from_slice(bytes);
    let mut it = parse(&all).unwrap();
    let first = it.next().unwrap();
    assert!(it.next().is_none(), "one command only");
    first
}

fn cmd_bytes(command: &Command) -> Vec<u8> {
    let mut v = Vec::new();
    command.write(&mut v);
    v
}

#[test]
fn signature_is_midi_in_ascii() {
    assert_eq!(SIGNATURE, 0x4D49_4449);
    assert_eq!(&datagram(&[])[..], b"MIDI");
}

#[test]
fn invitation_matches_the_specification_example_byte_for_byte() {
    // M2-124 A.1.1: capabilities 0, name "MyDev" (padded with zero), id "8shYe3h5".
    let inv = Command::Invitation(Invitation {
        methods: AuthMethods::NONE,
        identity: identity("MyDev", "8shYe3h5"),
    });
    let expected: Vec<u8> = [
        &[0x4D, 0x49, 0x44, 0x49][..],
        &[0x01, 0x04, 0x02, 0x00],
        b"MyDev\0\0\0",
        b"8shYe3h5",
    ]
    .concat();
    assert_eq!(datagram(std::slice::from_ref(&inv)), expected);
    assert_eq!(one(&expected[4..]).unwrap(), inv);
}

#[test]
fn ump_data_matches_the_timing_clock_example() {
    // M2-124 A.1.2: sequence 0x0010, one Timing Clock word on group 0.
    let data = UmpData::new(SequenceNumber::new(0x10), &[Ump([0x10F8_0000, 0, 0, 0])]).unwrap();
    let expected = [
        0x4D, 0x49, 0x44, 0x49, 0xFF, 0x01, 0x00, 0x10, 0x10, 0xF8, 0x00, 0x00,
    ];
    assert_eq!(datagram(&[Command::UmpData(data)]), expected);
}

#[test]
fn one_command_holds_two_umps_in_order() {
    // M2-124 A.1.3: Note On and Note Off on group 6 channel 1 under sequence 0x0011.
    let on = Ump([0x4590_4000, 0x1234_0000, 0, 0]);
    let off = Ump([0x4580_4000, 0x0100_0000, 0, 0]);
    let data = UmpData::new(SequenceNumber::new(0x11), &[on, off]).unwrap();
    let expected = [
        0x4D, 0x49, 0x44, 0x49, 0xFF, 0x04, 0x00, 0x11, 0x45, 0x90, 0x40, 0x00, 0x12, 0x34, 0x00,
        0x00, 0x45, 0x80, 0x40, 0x00, 0x01, 0x00, 0x00, 0x00,
    ];
    assert_eq!(datagram(&[Command::UmpData(data.clone())]), expected);
    assert_eq!(data.packets().collect::<Vec<_>>(), vec![on, off]);
}

#[test]
fn a_datagram_carries_several_ump_data_commands() {
    // M2-124 A.1.4: sequences 0x3456 and 0x3457, one UMP each.
    let on = Ump([0x4590_4000, 0x1234_0000, 0, 0]);
    let off = Ump([0x4580_4000, 0x0100_0000, 0, 0]);
    let a = Command::UmpData(UmpData::new(SequenceNumber::new(0x3456), &[on]).unwrap());
    let b = Command::UmpData(UmpData::new(SequenceNumber::new(0x3457), &[off]).unwrap());
    let expected = [
        0x4D, 0x49, 0x44, 0x49, 0xFF, 0x02, 0x34, 0x56, 0x45, 0x90, 0x40, 0x00, 0x12, 0x34, 0x00,
        0x00, 0xFF, 0x02, 0x34, 0x57, 0x45, 0x80, 0x40, 0x00, 0x01, 0x00, 0x00, 0x00,
    ];
    let bytes = datagram(&[a.clone(), b.clone()]);
    assert_eq!(bytes, expected);
    let parsed: Vec<_> = parse(&bytes).unwrap().map(Result::unwrap).collect();
    assert_eq!(parsed, vec![a, b]);
}

#[test]
fn zero_length_ump_data_is_a_header_alone() {
    let idle = Command::UmpData(UmpData::idle(SequenceNumber::new(7)));
    assert_eq!(cmd_bytes(&idle), [0xFF, 0x00, 0x00, 0x07]);
    let Command::UmpData(d) = one(&cmd_bytes(&idle)).unwrap() else {
        panic!()
    };
    assert!(d.is_idle());
}

#[test]
fn session_commands_without_payload_are_a_zero_length_header() {
    assert_eq!(cmd_bytes(&Command::SessionReset), [0x82, 0, 0, 0]);
    assert_eq!(cmd_bytes(&Command::SessionResetReply), [0x83, 0, 0, 0]);
    assert_eq!(cmd_bytes(&Command::ByeReply), [0xF1, 0, 0, 0]);
}

#[test]
fn ping_and_ping_reply_carry_the_id_in_one_payload_word() {
    assert_eq!(
        cmd_bytes(&Command::Ping(PingId(0x0102_0304))),
        [0x20, 0x01, 0, 0, 1, 2, 3, 4]
    );
    assert_eq!(
        cmd_bytes(&Command::PingReply(PingId(0xDEAD_BEEF))),
        [0x21, 0x01, 0, 0, 0xDE, 0xAD, 0xBE, 0xEF]
    );
}

#[test]
fn retransmit_request_puts_the_sequence_in_the_header_and_the_count_in_the_payload() {
    let req = Command::RetransmitRequest(RetransmitRequest {
        first: SequenceNumber::new(0xFFFE),
        count: 3,
    });
    assert_eq!(cmd_bytes(&req), [0x80, 0x01, 0xFF, 0xFE, 0x00, 0x03, 0, 0]);
    assert_eq!(one(&cmd_bytes(&req)).unwrap(), req);
}

#[test]
fn retransmit_error_puts_the_reason_in_the_header_and_the_sequence_in_the_payload() {
    let err = Command::RetransmitError(RetransmitRefusal {
        reason: RetransmitErrorReason::BufferMiss,
        first: SequenceNumber::new(0x1234),
    });
    assert_eq!(cmd_bytes(&err), [0x81, 0x01, 0x01, 0x00, 0x12, 0x34, 0, 0]);
    assert_eq!(one(&cmd_bytes(&err)).unwrap(), err);
}

#[test]
fn nak_carries_the_refused_header_then_padded_text() {
    let nak = Command::Nak(Nak {
        reason: NakReason::NotSupported,
        refused: Header::from_word(0xAB00_0000),
        text: Text::new("hello").unwrap(),
    });
    assert_eq!(
        cmd_bytes(&nak),
        [
            0x8F, 0x03, 0x01, 0x00, 0xAB, 0, 0, 0, b'h', b'e', b'l', b'l', b'o', 0, 0, 0
        ]
    );
    assert_eq!(one(&cmd_bytes(&nak)).unwrap(), nak);
    let bare = Command::Nak(Nak {
        reason: NakReason::Malformed,
        refused: Header::new(0x20, 1, 0),
        text: Text::default(),
    });
    assert_eq!(
        cmd_bytes(&bare)[1],
        1,
        "a NAK with no text has a one-word payload"
    );
}

#[test]
fn bye_carries_the_reason_in_the_header_and_optional_text() {
    let plain = Command::Bye(Bye::new(ByeReason::Timeout));
    assert_eq!(cmd_bytes(&plain), [0xF0, 0x00, 0x04, 0x00]);
    let worded = Command::Bye(Bye {
        reason: ByeReason::UserTerminated,
        text: Text::new("bye").unwrap(),
    });
    assert_eq!(
        cmd_bytes(&worded),
        [0xF0, 0x01, 0x01, 0x00, b'b', b'y', b'e', 0]
    );
    assert_eq!(one(&cmd_bytes(&worded)).unwrap(), worded);
}

#[test]
fn bye_reasons_round_trip_every_code_including_reserved() {
    for code in 0..=255u8 {
        assert_eq!(ByeReason::from_code(code).code(), code);
    }
    assert_eq!(ByeReason::from_code(0x43), ByeReason::AuthenticationFailed);
    assert_eq!(ByeReason::from_code(0x80), ByeReason::InvitationCanceled);
    assert_eq!(ByeReason::from_code(0x20), ByeReason::Reserved(0x20));
}

#[test]
fn accepted_and_pending_carry_the_host_identity_like_an_invitation() {
    let id = identity("Studio", "HOST-1");
    let accepted = Command::Accepted(id);
    // Name "Studio" is 2 words, id "HOST-1" 2 words.
    assert_eq!(
        cmd_bytes(&accepted),
        [
            0x10, 0x04, 0x02, 0x00, b'S', b't', b'u', b'd', b'i', b'o', 0, 0, b'H', b'O', b'S',
            b'T', b'-', b'1', 0, 0
        ]
    );
    assert_eq!(cmd_bytes(&Command::Pending(id))[0], 0x11);
    assert_eq!(one(&cmd_bytes(&accepted)).unwrap(), accepted);
}

#[test]
fn authentication_required_puts_the_state_in_data_two_and_the_nonce_before_the_name() {
    let nonce = Nonce::new(*b"nUWrn*@#$hjfwnkL").unwrap();
    let c = Command::AuthRequired(AuthChallenge {
        state: AuthState::DigestWrong,
        nonce,
        identity: identity("Studio", "HOST-1"),
    });
    let bytes = cmd_bytes(&c);
    // payload: 4 nonce + 2 name + 2 id words; csd1 is the name's 2 words.
    assert_eq!(&bytes[..4], [0x12, 0x08, 0x02, 0x01]);
    assert_eq!(&bytes[4..20], b"nUWrn*@#$hjfwnkL");
    assert_eq!(&bytes[20..28], b"Studio\0\0");
    assert_eq!(one(&bytes).unwrap(), c);
}

#[test]
fn user_authentication_required_has_three_states() {
    for (state, byte) in [
        (UserAuthState::First, 0),
        (UserAuthState::DigestWrong, 1),
        (UserAuthState::UserNotFound, 2),
    ] {
        let c = Command::UserAuthRequired(UserAuthChallenge {
            state,
            nonce: Nonce::new(*b"XI|~=NNRVaD;XCPL").unwrap(),
            identity: identity("A", "B"),
        });
        let bytes = cmd_bytes(&c);
        assert_eq!(bytes[0], 0x13);
        assert_eq!(bytes[3], byte);
        assert_eq!(one(&bytes).unwrap(), c);
    }
}

#[test]
fn invitation_with_authentication_is_the_digest_alone_in_eight_words() {
    let digest = Digest::new([7; 32]);
    let bytes = cmd_bytes(&Command::InvitationWithAuth(digest));
    assert_eq!(&bytes[..4], [0x02, 0x08, 0x00, 0x00]);
    assert_eq!(&bytes[4..], [7; 32]);
}

#[test]
fn invitation_with_user_authentication_appends_the_padded_username() {
    let auth = Command::InvitationWithUserAuth(UserAuthentication {
        digest: Digest::new([1; 32]),
        username: Username::new("Rosa").unwrap(),
    });
    let bytes = cmd_bytes(&auth);
    assert_eq!(&bytes[..4], [0x03, 0x09, 0x00, 0x00]);
    assert_eq!(&bytes[36..], b"Rosa");
    assert_eq!(one(&bytes).unwrap(), auth);
}

#[test]
fn invitation_capabilities_are_data_two_and_reserved_bits_are_ignored() {
    let both = Command::Invitation(Invitation {
        methods: AuthMethods::SHARED_SECRET.union(AuthMethods::USER),
        identity: identity("MyDev", "8shYe3h5"),
    });
    assert_eq!(cmd_bytes(&both)[3], 0b11);
    let mut bytes = cmd_bytes(&both);
    bytes[3] = 0xFF;
    let Command::Invitation(inv) = one(&bytes).unwrap() else {
        panic!()
    };
    assert_eq!(inv.methods.bits(), 0b11);
}

#[test]
fn the_longest_invitation_is_thirty_six_words_with_a_twenty_five_word_name() {
    let name = "n".repeat(98);
    let id = "i".repeat(42);
    let inv = Command::Invitation(Invitation {
        methods: AuthMethods::NONE,
        identity: identity(&name, &id),
    });
    let bytes = cmd_bytes(&inv);
    assert_eq!((bytes[1], bytes[2]), (36, 25));
    assert_eq!(one(&bytes).unwrap(), inv);
}

#[test]
fn every_command_round_trips() {
    let id = identity("Studio", "HOST-1");
    let nonce = Nonce::new(*b"0123456789abcdef").unwrap();
    let packets = [
        Ump([0x4090_3C00, 0x8000_0000, 0, 0]),
        Ump([0x10F8_0000, 0, 0, 0]),
    ];
    let commands = vec![
        Command::Invitation(Invitation {
            methods: AuthMethods::USER,
            identity: id,
        }),
        Command::InvitationWithAuth(Digest::new([9; 32])),
        Command::InvitationWithUserAuth(UserAuthentication {
            digest: Digest::new([3; 32]),
            username: Username::new("Ünïcode").unwrap(),
        }),
        Command::Accepted(id),
        Command::Pending(id),
        Command::AuthRequired(AuthChallenge {
            state: AuthState::First,
            nonce,
            identity: id,
        }),
        Command::UserAuthRequired(UserAuthChallenge {
            state: UserAuthState::UserNotFound,
            nonce,
            identity: id,
        }),
        Command::Ping(PingId(1)),
        Command::PingReply(PingId(u32::MAX)),
        Command::RetransmitRequest(RetransmitRequest {
            first: SequenceNumber::new(65535),
            count: 0,
        }),
        Command::RetransmitError(RetransmitRefusal {
            reason: RetransmitErrorReason::Reserved(9),
            first: SequenceNumber::new(2),
        }),
        Command::SessionReset,
        Command::SessionResetReply,
        Command::Nak(Nak {
            reason: NakReason::Reserved(0x55),
            refused: Header::from_word(0x0102_0304),
            text: Text::new("why").unwrap(),
        }),
        Command::Bye(Bye {
            reason: ByeReason::Reserved(0x99),
            text: Text::new("é").unwrap(),
        }),
        Command::ByeReply,
        Command::UmpData(UmpData::new(SequenceNumber::new(5), &packets).unwrap()),
        Command::UmpData(UmpData::idle(SequenceNumber::new(6))),
    ];
    let bytes = datagram(&commands);
    let parsed: Vec<_> = parse(&bytes).unwrap().map(Result::unwrap).collect();
    assert_eq!(parsed, commands);
    for c in &commands {
        assert_eq!(c.header().code(), c.code());
        assert_eq!(
            usize::from(c.header().payload_words()) * 4 + 4,
            cmd_bytes(c).len()
        );
    }
}

// ---- Refusals ----

#[test]
fn a_datagram_without_the_signature_is_ignored_whole() {
    assert_eq!(
        parse(b"MIDX\xFF\x00\x00\x00").unwrap_err(),
        SignatureError::Mismatch { found: 0x4D49_4458 }
    );
    assert_eq!(parse(b"MID").unwrap_err(), SignatureError::TooShort);
    assert_eq!(parse(b"").unwrap_err(), SignatureError::TooShort);
}

#[test]
fn a_command_longer_than_the_datagram_ends_the_datagram() {
    // Header claims a 2-word payload; one word is present.
    let mut all = SIG.to_vec();
    all.extend_from_slice(&[0x20, 0x02, 0, 0, 0, 0, 0, 1]);
    let mut it = parse(&all).unwrap();
    assert_eq!(it.next().unwrap().unwrap_err().error, ParseError::Truncated);
    assert!(it.next().is_none());
}

#[test]
fn trailing_bytes_short_of_a_header_are_truncated() {
    let mut all = datagram(&[Command::ByeReply]);
    all.extend_from_slice(&[0xF1, 0]);
    let results: Vec<_> = parse(&all).unwrap().collect();
    assert_eq!(results.len(), 2);
    assert!(results[0].is_ok());
    assert_eq!(
        results[1].as_ref().unwrap_err().error,
        ParseError::Truncated
    );
}

#[test]
fn an_unknown_command_code_is_refused_and_the_next_command_still_reads() {
    let mut all = SIG.to_vec();
    all.extend_from_slice(&[0x55, 0x01, 0x00, 0x00, 1, 2, 3, 4]);
    all.extend_from_slice(&cmd_bytes(&Command::ByeReply));
    let results: Vec<_> = parse(&all).unwrap().collect();
    let refused = results[0].as_ref().unwrap_err();
    assert_eq!(refused.error, ParseError::UnknownCode(0x55));
    assert_eq!(refused.header, Header::new(0x55, 1, 0));
    assert_eq!(results[1], Ok(Command::ByeReply));
}

#[test]
fn a_ping_without_its_payload_word_is_refused_for_length() {
    assert_eq!(
        one(&[0x20, 0x00, 0, 0]).unwrap_err().error,
        ParseError::Length {
            code: 0x20,
            words: 0
        }
    );
}

#[test]
fn payload_words_past_the_defined_fields_are_ignored() {
    let c = one(&[0x20, 0x02, 0xAB, 0xCD, 0, 0, 0, 5, 9, 9, 9, 9]).unwrap();
    assert_eq!(
        c,
        Command::Ping(PingId(5)),
        "the header's reserved data and the extra word are not read"
    );
}

#[test]
fn an_invitation_with_no_product_instance_id_is_refused() {
    // Name 1 word, id none: payload length 1.
    let e = one(&[0x01, 0x01, 0x01, 0x00, b'a', 0, 0, 0]).unwrap_err();
    assert_eq!(
        e.error,
        ParseError::Length {
            code: 0x01,
            words: 1
        }
    );
}

#[test]
fn an_invitation_with_an_all_zero_name_is_refused_as_empty() {
    let e = one(&[0x01, 0x02, 0x01, 0x00, 0, 0, 0, 0, b'i', 0, 0, 0]).unwrap_err();
    assert_eq!(e.error, ParseError::EmptyText);
}

#[test]
fn an_invitation_name_that_is_not_utf8_is_refused() {
    let e = one(&[0x01, 0x02, 0x01, 0x00, 0xFF, 0xFE, 0, 0, b'i', 0, 0, 0]).unwrap_err();
    assert_eq!(e.error, ParseError::Utf8);
}

#[test]
fn a_product_instance_id_outside_printable_ascii_is_refused() {
    let e = one(&[0x01, 0x02, 0x01, 0x00, b'n', 0, 0, 0, 0x07, 0, 0, 0]).unwrap_err();
    assert!(matches!(e.error, ParseError::InstanceId(_)));
}

#[test]
fn an_invitation_name_over_twenty_five_words_is_refused() {
    let mut bytes = vec![0x01, 27, 26, 0x00];
    bytes.extend(std::iter::repeat_n(b'n', 26 * 4));
    bytes.extend_from_slice(b"id\0\0");
    assert_eq!(
        one(&bytes).unwrap_err().error,
        ParseError::Length {
            code: 0x01,
            words: 27
        }
    );
}

#[test]
fn a_reserved_authentication_state_is_refused() {
    let c = Command::AuthRequired(AuthChallenge {
        state: AuthState::First,
        nonce: Nonce::new(*b"0123456789abcdef").unwrap(),
        identity: identity("A", "B"),
    });
    let mut bytes = cmd_bytes(&c);
    bytes[3] = 2;
    assert_eq!(one(&bytes).unwrap_err().error, ParseError::Reserved);
}

#[test]
fn a_non_ascii_nonce_is_refused() {
    let c = Command::AuthRequired(AuthChallenge {
        state: AuthState::First,
        nonce: Nonce::new(*b"0123456789abcdef").unwrap(),
        identity: identity("A", "B"),
    });
    let mut bytes = cmd_bytes(&c);
    bytes[4] = 0xC3;
    assert_eq!(one(&bytes).unwrap_err().error, ParseError::Nonce);
}

#[test]
fn ump_data_over_sixty_four_words_is_refused() {
    let mut bytes = vec![0xFF, 65, 0, 0];
    bytes.extend(std::iter::repeat_n(0u8, 65 * 4));
    assert_eq!(
        one(&bytes).unwrap_err().error,
        ParseError::Length {
            code: 0xFF,
            words: 65
        }
    );
}

#[test]
fn ump_data_that_ends_inside_a_packet_is_refused() {
    // A MIDI 2.0 channel voice packet is two words; one is present.
    let e = one(&[0xFF, 0x01, 0, 0, 0x40, 0x90, 0x3C, 0x00]).unwrap_err();
    assert_eq!(e.error, ParseError::UmpData(UmpDataError::Truncated));
}

#[test]
fn building_ump_data_over_sixty_four_words_is_refused() {
    let seventeen = [Ump([0x5000_0000, 0, 0, 0]); 17];
    assert_eq!(
        UmpData::new(SequenceNumber::ZERO, &seventeen).unwrap_err(),
        UmpDataError::TooLong { words: 68 }
    );
}

#[test]
fn chunks_split_at_sixty_four_words_without_splitting_a_packet_and_number_on() {
    // 17 four-word packets are 68 words: 16 packets fill a command.
    let packets = [Ump([0x5000_0000, 0, 0, 0]); 17];
    let chunks = UmpData::chunks(SequenceNumber::new(0xFFFF), &packets);
    assert_eq!(chunks.len(), 2);
    assert_eq!(chunks[0].words().len(), 64);
    assert_eq!(chunks[1].words().len(), 4);
    assert_eq!(chunks[0].sequence(), SequenceNumber::new(0xFFFF));
    assert_eq!(chunks[1].sequence(), SequenceNumber::ZERO);
}

#[test]
fn texts_refuse_a_nul_and_overlong_input() {
    assert!(Text::new("a\0b").is_err());
    assert!(Text::new(&"x".repeat(1017)).is_err());
    assert!(Text::new(&"x".repeat(1016)).is_ok());
    assert!(Username::new("").is_err());
    assert!(Username::new(&"u".repeat(989)).is_err());
    assert!(Username::new(&"u".repeat(988)).is_ok());
}

// ---- Sequence numbers ----

#[test]
fn sequence_numbers_wrap_to_zero_after_ffff() {
    assert_eq!(SequenceNumber::new(0xFFFF).next(), SequenceNumber::ZERO);
    assert_eq!(
        SequenceNumber::new(0xFFFE).advanced(3),
        SequenceNumber::new(1)
    );
}

#[test]
fn distance_reads_a_number_just_past_the_wrap_as_ahead() {
    let before = SequenceNumber::new(0xFFFE);
    let after = SequenceNumber::new(2);
    assert_eq!(after.distance_from(before), 4);
    assert_eq!(before.distance_from(after), -4);
    assert_eq!(before.distance_from(before), 0);
}

mod props {
    use proptest::prelude::*;
    use tutti_midi_network::SequenceNumber;

    proptest! {
        #[test]
        fn advancing_then_measuring_returns_the_step(start in any::<u16>(), step in 0u16..0x7FFF) {
            let a = SequenceNumber::new(start);
            prop_assert_eq!(a.advanced(step).distance_from(a), i16::try_from(step).unwrap());
        }
    }
}
