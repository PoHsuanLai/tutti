//! `FlexTextAssembler`: M2-104 section 7.5.9 (UTF-8 text, zero padding only in
//! a Complete or End packet) and section 7.5.2 (what may sit between a Start and
//! its End), checked against packets built from the spec's fields, not only
//! against the writer.

use tutti_ump::{
    Flex, FlexAddress, FlexTextAssembler, FlexTextKind, FlexTextStep, Form, Message, MidiGroup,
    System, Ump, Utility, flex_text_packets,
};

const G0: MidiGroup = MidiGroup::of(0);

fn packet(group: MidiGroup, kind: FlexTextKind, form: Form, bytes: &[u8]) -> Ump {
    let mut padded = [0u8; 12];
    padded[..bytes.len()].copy_from_slice(bytes);
    Ump::from(Message::Flex(
        group,
        Flex::Text {
            address: FlexAddress::Group,
            kind,
            form,
            bytes: padded,
        },
    ))
}

const LYRICS: FlexTextKind = FlexTextKind::Lyrics;

fn assembler(lanes: usize) -> FlexTextAssembler<Vec<u8>> {
    FlexTextAssembler::new(vec![0; 64 * lanes], 64, lanes)
}

/// The text a push completes, if it does.
fn text(a: &mut FlexTextAssembler<Vec<u8>>, ump: Ump) -> Option<String> {
    match a.push(ump) {
        FlexTextStep::Done(t) => Some(t.text.to_owned()),
        _ => None,
    }
}

fn feed(a: &mut FlexTextAssembler<Vec<u8>>, umps: impl IntoIterator<Item = Ump>) -> Vec<String> {
    umps.into_iter().filter_map(|u| text(a, u)).collect()
}

/// Every length from empty to past five packets comes back whole, including a
/// text whose multi-byte characters straddle the 12-byte packets (a packet may
/// end inside a character, 7.5.9: the reader joins the bytes before it decodes).
#[test]
fn a_written_text_reads_back_whole() {
    let mut a = FlexTextAssembler::new(vec![0; 256], 256, 1);
    for len in 0..70 {
        // 'é' is two bytes: odd offsets put a character across a packet edge.
        let original: String = std::iter::repeat_n("é", len).collect::<String>() + "x";
        let packets = flex_text_packets(G0, FlexAddress::Group, LYRICS, &original).unwrap();
        assert_eq!(feed(&mut a, packets), [original], "{len}");
    }
    assert_eq!((a.dropped(), a.invalid()), (0, 0));
}

/// The message carries its Group, address and kind.
#[test]
fn a_message_names_where_it_came_from() {
    let mut a = assembler(1);
    let g = MidiGroup::of(5);
    let ump = Ump::from(Message::Flex(
        g,
        Flex::Text {
            address: FlexAddress::Channel(tutti_ump::MidiChannel::of(7)),
            kind: FlexTextKind::Composer,
            form: Form::Complete,
            bytes: *b"Bach\0\0\0\0\0\0\0\0",
        },
    ));
    let FlexTextStep::Done(t) = a.push(ump) else {
        panic!("not done")
    };
    assert_eq!(
        (t.group, t.address, t.kind, t.text),
        (
            g,
            FlexAddress::Channel(tutti_ump::MidiChannel::of(7)),
            FlexTextKind::Composer,
            "Bach"
        )
    );
}

/// 7.5.10.1: a Complete packet of all zeros is a melisma, an empty text.
#[test]
fn an_all_zero_complete_packet_is_an_empty_text() {
    let mut a = assembler(1);
    assert_eq!(
        text(&mut a, packet(G0, LYRICS, Form::Complete, b"")),
        Some(String::new())
    );
    assert_eq!(a.invalid(), 0);
}

/// 7.5.9: zero may occur only in an End or Complete packet; the text of a
/// Start (and, by the same sentence, a Continue) has none. The whole message
/// is refused once, its End skipped, and the next message is fine.
/// Spec: m2-104/7.5.9#3
#[test]
fn a_zero_in_a_start_or_continue_refuses_the_message() {
    for bad_form in [Form::Start, Form::Continue] {
        let mut a = assembler(1);
        let mut umps = vec![];
        if bad_form == Form::Continue {
            umps.push(packet(G0, LYRICS, Form::Start, b"abcdefghijkl"));
        }
        umps.push(packet(G0, LYRICS, bad_form, b"abcdef"));
        umps.push(packet(G0, LYRICS, Form::End, b"tail"));
        assert_eq!(feed(&mut a, umps), Vec::<String>::new(), "{bad_form:?}");
        assert_eq!((a.invalid(), a.dropped()), (1, 0), "{bad_form:?}");
        let ok = packet(G0, LYRICS, Form::Complete, b"fine");
        assert_eq!(text(&mut a, ok).as_deref(), Some("fine"));
    }
}

/// 7.5.9: the remaining data bytes are zero: a byte after the
/// first zero of an End or Complete packet refuses it.
/// Spec: m2-104/7.5.9#2
#[test]
fn a_byte_after_the_padding_began_refuses_the_message() {
    let mut a = assembler(1);
    let mut bytes = [0u8; 12];
    bytes[..2].copy_from_slice(b"ab");
    bytes[5] = b'z';
    let complete = Ump::from(Message::Flex(
        G0,
        Flex::Text {
            address: FlexAddress::Group,
            kind: LYRICS,
            form: Form::Complete,
            bytes,
        },
    ));
    assert_eq!(text(&mut a, complete), None);
    let end = Ump::from(Message::Flex(
        G0,
        Flex::Text {
            address: FlexAddress::Group,
            kind: LYRICS,
            form: Form::End,
            bytes,
        },
    ));
    let start = packet(G0, LYRICS, Form::Start, b"abcdefghijkl");
    assert_eq!(feed(&mut a, [start, end]), Vec::<String>::new());
    assert_eq!((a.invalid(), a.dropped()), (2, 0));
}

/// 7.5.9: UTF-8 without a Byte Order Mark. A text that is not UTF-8, a
/// truncated character, and a BOM, each refused and counted once.
/// Spec: m2-104/7.5.9#1
#[test]
fn text_must_be_utf8_without_a_bom() {
    let mut a = assembler(1);
    for bad in [
        &[0xFF, b'a'][..],
        &[b'a', 0xC3][..],
        &[0xEF, 0xBB, 0xBF, b'a'][..],
    ] {
        assert_eq!(
            text(&mut a, packet(G0, LYRICS, Form::Complete, bad)),
            None,
            "{bad:?}"
        );
    }
    assert_eq!(a.invalid(), 3);
    // A character split over the join is valid.
    let start = packet(G0, LYRICS, Form::Start, b"abcdefghijk\xC3");
    let end = packet(G0, LYRICS, Form::End, b"\xA9z");
    assert_eq!(
        feed(&mut a, [start, end]),
        ["abcdefghijké"[..].to_owned() + "z"]
    );
}

/// 7.5.2: only the message's own Continue packets, System Real Time messages
/// and JR Clock messages may sit between Start and End. Those do not end it.
/// Spec: m2-104/7.5.2#2, m2-104/7.5.2#3
#[test]
fn real_time_and_jr_clock_do_not_interrupt() {
    let mut a = assembler(1);
    let start = packet(G0, LYRICS, Form::Start, b"abcdefghijkl");
    let clock = Ump::from(Message::System(G0, System::TimingClock));
    let jr = Ump::from(Message::Utility(Utility::JrClock(7)));
    let end = packet(G0, LYRICS, Form::End, b"mn");
    assert_eq!(feed(&mut a, [start, clock, jr, end]), ["abcdefghijklmn"]);
    assert_eq!(a.dropped(), 0);
}

/// 7.5.2: anything else of the Group ends the open message, once, counted;
/// its End is skipped. Another Group's traffic does not.
/// Spec: m2-104/7.5.2#2
#[test]
fn another_message_of_the_group_drops_the_open_one() {
    for other in [
        Ump::from(Message::System(G0, System::TuneRequest)),
        Ump::from(Message::Flex(G0, Flex::SetTempo(500_000))),
        packet(G0, FlexTextKind::Composer, Form::Complete, b"x"),
    ] {
        let mut a = assembler(1);
        let start = packet(G0, LYRICS, Form::Start, b"abcdefghijkl");
        let end = packet(G0, LYRICS, Form::End, b"mn");
        let got = feed(&mut a, [start, other, end]);
        // Only the interrupting Complete text can be delivered.
        assert!(got.is_empty() || got == ["x"], "{got:?}");
        assert_eq!(a.dropped(), 1, "{other:?}");
    }
    let mut a = assembler(1);
    let g1 = MidiGroup::of(1);
    let start = packet(G0, LYRICS, Form::Start, b"abcdefghijkl");
    let other = Ump::from(Message::System(g1, System::TuneRequest));
    let end = packet(G0, LYRICS, Form::End, b"mn");
    assert_eq!(feed(&mut a, [start, other, end]), ["abcdefghijklmn"]);
}

/// A Continue or End of another kind than the open message's is another
/// message's packet: it interrupts, and is not spliced on.
#[test]
fn a_packet_of_another_kind_is_not_spliced_on() {
    let mut a = assembler(1);
    let start = packet(G0, LYRICS, Form::Start, b"abcdefghijkl");
    let end = packet(G0, FlexTextKind::Ruby, Form::End, b"mn");
    assert_eq!(feed(&mut a, [start, end]), Vec::<String>::new());
    assert_eq!(a.dropped(), 1);
}

/// Two Groups assemble at once when there are lanes for them; with one lane a
/// second Start is dropped whole (counted once) and its packets skipped.
#[test]
fn groups_assemble_apart_up_to_the_lanes() {
    let g1 = MidiGroup::of(1);
    let umps = |g: MidiGroup, a: &[u8], b: &[u8]| {
        [
            packet(g, LYRICS, Form::Start, a),
            packet(g, LYRICS, Form::End, b),
        ]
    };
    let (x, y) = (
        umps(G0, b"aaaaaaaaaaaa", b"1"),
        umps(g1, b"bbbbbbbbbbbb", b"2"),
    );
    let mut a = assembler(2);
    assert_eq!(
        feed(&mut a, [x[0], y[0], x[1], y[1]]),
        ["aaaaaaaaaaaa1", "bbbbbbbbbbbb2"]
    );
    let mut a = assembler(1);
    assert_eq!(feed(&mut a, [x[0], y[0], x[1], y[1]]), ["aaaaaaaaaaaa1"]);
    assert_eq!((a.dropped(), a.invalid()), (1, 0));
}

/// A text past `max_text` is dropped whole, counted once, skipped to its End;
/// the lane is then free.
#[test]
fn a_text_over_capacity_is_dropped_whole() {
    let mut a = FlexTextAssembler::new(vec![0; 24], 24, 1);
    let long = "x".repeat(25);
    let packets = flex_text_packets(G0, FlexAddress::Group, LYRICS, &long).unwrap();
    let mut got = vec![];
    for u in packets {
        if let FlexTextStep::Done(t) = a.push(u) {
            got.push(t.text.to_owned());
        }
    }
    assert!(got.is_empty());
    assert_eq!(a.dropped(), 1);
    let exact = "y".repeat(24);
    let packets = flex_text_packets(G0, FlexAddress::Group, LYRICS, &exact).unwrap();
    let mut got = vec![];
    for u in packets {
        if let FlexTextStep::Done(t) = a.push(u) {
            got.push(t.text.to_owned());
        }
    }
    assert_eq!(got, [exact]);
    assert_eq!(a.dropped(), 1);
}

/// A Continue or End with no open message is skipped and costs nothing; a
/// packet that is no Flex text is `NotText`.
#[test]
fn orphans_are_skipped_and_other_umps_pass() {
    let mut a = assembler(1);
    for form in [Form::Continue, Form::End] {
        let step = a.push(packet(G0, LYRICS, form, b"abcdefghijkl"));
        assert_eq!(step, FlexTextStep::Held);
    }
    assert_eq!((a.dropped(), a.invalid()), (0, 0));
    let tempo = Ump::from(Message::Flex(G0, Flex::SetTempo(1)));
    assert_eq!(a.push(tempo), FlexTextStep::NotText);
}

/// An upstream gap and a reset drop the open message, once each, so the rest
/// of it is not spliced onto a later one.
#[test]
fn a_gap_or_reset_drops_the_open_message() {
    let mut a = assembler(1);
    a.push(packet(G0, LYRICS, Form::Start, b"abcdefghijkl"));
    a.lost_tail();
    assert_eq!(a.dropped(), 1);
    assert_eq!(text(&mut a, packet(G0, LYRICS, Form::End, b"mn")), None);
    a.push(packet(G0, LYRICS, Form::Start, b"abcdefghijkl"));
    a.reset();
    assert_eq!(a.dropped(), 2);
}
