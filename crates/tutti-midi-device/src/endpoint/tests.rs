use tutti_ump::{
    BlockFilter, BlockSelect, DeviceIdentity, Direction as BlockDirection, DiscoveryFilter,
    FirstGroup, FunctionBlockId, GroupSpan, Message, Midi1Port, Protocol, Stream, StreamConfig, U7,
    UiHint, Ump,
};

use super::*;
use crate::name::{BlockName, EndpointName, ProductInstanceId};

const ALL: DiscoveryFilter = DiscoveryFilter {
    endpoint_info: true,
    device_identity: true,
    endpoint_name: true,
    product_instance_id: true,
    stream_configuration: true,
};
const NONE: DiscoveryFilter = DiscoveryFilter {
    endpoint_info: false,
    device_identity: false,
    endpoint_name: false,
    product_instance_id: false,
    stream_configuration: false,
};

fn id(n: u8) -> FunctionBlockId {
    FunctionBlockId::new(n).unwrap()
}

fn block(n: u8, first: u8, span: u8) -> FunctionBlockBuilder {
    FunctionBlock::builder(
        id(n),
        BlockDirection::Bidirectional,
        FirstGroup::new(first).unwrap(),
        GroupSpan::new(span).unwrap(),
    )
}

fn identity() -> DeviceIdentity {
    let u = |n| U7::new(n).unwrap();
    DeviceIdentity {
        manufacturer: [u(0), u(0x21), u(0x09)],
        family: [u(1), u(2)],
        model: [u(3), u(4)],
        revision: [u(5), u(6), u(7), u(8)],
    }
}

const FULL: Capabilities = Capabilities {
    midi1: true,
    midi2: true,
    rx_jr: true,
    tx_jr: true,
};

/// A device with a name, an id, an identity and two blocks, the second a
/// MIDI-CI 1.2 block (message version 2) with two SysEx 8 streams.
fn device() -> Endpoint {
    Endpoint::new(FULL)
        .with_identity(identity())
        .with_name(EndpointName::new("ABCDEFGHIJKLMNOPQRSTUVWXYZ0123").unwrap())
        .with_instance_id(ProductInstanceId::new("SN-0001").unwrap())
        .with_static_function_blocks(true)
        .with_function_block(
            block(0, 0, 1)
                .name(BlockName::new("Keys").unwrap())
                .build()
                .unwrap(),
        )
        .with_function_block(
            block(3, 4, 2)
                .ui_hint(UiHint::Both)
                .midi_ci(2, 2)
                .name(BlockName::new("Piano").unwrap())
                .build()
                .unwrap(),
        )
}

fn ask(endpoint: &mut Endpoint, stream: Stream) -> Vec<Ump> {
    let mut out = Vec::new();
    endpoint.reply(Message::Stream(stream), &mut out);
    out
}

fn discovery(filter: DiscoveryFilter) -> Stream {
    Stream::EndpointDiscovery {
        version: UMP_VERSION,
        filter,
    }
}

fn status(u: &Ump) -> u32 {
    assert_eq!(u.message_type(), 0xF);
    (u.0[0] >> 16) & 0x3FF
}

fn words(u: &Ump) -> [u32; 4] {
    u.0
}

fn block_discovery(block: BlockSelect, info: bool, name: bool) -> Stream {
    Stream::FunctionBlockDiscovery {
        block,
        filter: BlockFilter { info, name },
    }
}

// M2-104 §7.1.2: version 1.1, static, NFB = highest block number + 1 = 4,
// M2 and M1, RXJR and TXJR. Word 1: 1 << 31 | 4 << 24 | 1 << 9 | 1 << 8 | 2 | 1.
#[test]
fn endpoint_info_words() {
    let out = ask(
        &mut device(),
        discovery(DiscoveryFilter {
            endpoint_info: true,
            ..NONE
        }),
    );
    assert_eq!(out.len(), 1);
    assert_eq!(words(&out[0]), [0xF001_0101, 0x8400_0303, 0, 0]);
}

#[test]
fn a_full_discovery_is_answered_in_the_order_of_the_spec() {
    let out = ask(&mut device(), discovery(ALL));
    let order: Vec<u32> = out.iter().map(status).collect();
    // Info, Identity, Name x3 packets, Product Instance Id, Stream Configuration.
    assert_eq!(order, [0x01, 0x02, 0x03, 0x03, 0x03, 0x04, 0x06]);
}

// §7.1.1: "Each bit set will result in an individual reply."
#[test]
fn each_filter_bit_asks_for_its_own_reply() {
    let rows = [
        (
            DiscoveryFilter {
                endpoint_info: true,
                ..NONE
            },
            vec![0x01],
        ),
        (
            DiscoveryFilter {
                device_identity: true,
                ..NONE
            },
            vec![0x02],
        ),
        (
            DiscoveryFilter {
                endpoint_name: true,
                ..NONE
            },
            vec![0x03, 0x03, 0x03],
        ),
        (
            DiscoveryFilter {
                product_instance_id: true,
                ..NONE
            },
            vec![0x04],
        ),
        (
            DiscoveryFilter {
                stream_configuration: true,
                ..NONE
            },
            vec![0x06],
        ),
        (NONE, vec![]),
    ];
    for (filter, expect) in rows {
        let out = ask(&mut device(), discovery(filter));
        assert_eq!(
            out.iter().map(status).collect::<Vec<_>>(),
            expect,
            "{filter:?}"
        );
    }
}

// A part the endpoint does not have is not answered (§7.1.3 to §7.1.5 say
// "may").
#[test]
fn a_part_an_endpoint_lacks_is_left_out() {
    let mut bare = Endpoint::new(FULL);
    let out = ask(&mut bare, discovery(ALL));
    assert_eq!(out.iter().map(status).collect::<Vec<_>>(), [0x01, 0x06]);
}

// §7.1.1: a higher version's appended fields are ignored and the reply is the
// same.
#[test]
fn a_discovery_of_another_version_is_answered() {
    let newer = Stream::EndpointDiscovery {
        version: tutti_ump::UmpVersion { major: 1, minor: 9 },
        filter: ALL,
    };
    assert_eq!(ask(&mut device(), newer).len(), 7);
}

// Figure 14, §7.1.3: word 1 holds the manufacturer's three bytes, word 2 the
// family and model, word 3 the revision. §7.1.4: a 30 byte name is a Start (14),
// a Continue (14) and an End (2 then zeros). §7.1.5: "SN-0001" is Complete.
#[test]
fn identity_name_and_instance_id_words() {
    let out = ask(&mut device(), discovery(ALL));
    let w: Vec<[u32; 4]> = out.iter().map(words).collect();
    assert_eq!(w[1], [0xF002_0000, 0x0000_2109, 0x0102_0304, 0x0506_0708]);
    assert_eq!(w[2], [0xF403_4142, 0x4344_4546, 0x4748_494A, 0x4B4C_4D4E]);
    assert_eq!(w[3], [0xF803_4F50, 0x5152_5354, 0x5556_5758, 0x595A_3031]);
    assert_eq!(w[4], [0xFC03_3233, 0, 0, 0]);
    assert_eq!(w[5], [0xF004_534E, 0x2D30_3030, 0x3100_0000, 0]);
    // §7.1.6.3, Figure 19: MIDI 1.0 Protocol, JR off: 0x01 << 8.
    assert_eq!(w[6], [0xF006_0100, 0, 0, 0]);
}

// §7.1.8 Figure 22: active bit 15, block number 14-8, UI hint 5-4, MIDI 1.0
// 3-2, direction 1-0; word 1: first group, groups spanned, MIDI-CI version, SysEx 8
// streams. Block 3: 0x8000 | 0x0300 | 0x30 | 0 | 0b11 = 0x8333, then 4, 2, 2, 2.
// §7.1.9 Figure 23: block number in bits 15-8 then thirteen name bytes.
#[test]
fn function_block_info_and_name_words() {
    let out = ask(
        &mut device(),
        block_discovery(BlockSelect::One(id(3)), true, true),
    );
    assert_eq!(out.len(), 2);
    assert_eq!(words(&out[0]), [0xF011_8333, 0x0402_0202, 0, 0]);
    assert_eq!(words(&out[1]), [0xF012_0350, 0x6961_6E6F, 0, 0]);

    // The other way round: MIDI-CI version 1, 16 streams, block 7, Groups 5 to 7,
    // so a swapped pair of bytes shows. 0x8000 | 0x0700 | 0b11, then 5, 3, 1, 16.
    let mut e =
        Endpoint::new(FULL).with_function_block(block(7, 5, 3).midi_ci(1, 16).build().unwrap());
    let out = ask(
        &mut e,
        block_discovery(BlockSelect::One(id(7)), true, false),
    );
    assert_eq!(words(&out[0]), [0xF011_8703, 0x0503_0110, 0, 0]);
}

// A 30 byte block name: 13 bytes a packet, so Start, Continue, End with 4.
#[test]
fn a_long_block_name_splits_at_thirteen_bytes() {
    let name = BlockName::new("0123456789abcdefghijklmnopqrst").unwrap();
    let mut e = Endpoint::new(FULL).with_function_block(block(1, 0, 1).name(name).build().unwrap());
    let out = ask(
        &mut e,
        block_discovery(BlockSelect::One(id(1)), false, true),
    );
    assert_eq!(
        out.iter().map(words).collect::<Vec<_>>(),
        [
            [0xF412_0130, 0x3132_3334, 0x3536_3738, 0x3961_6263],
            [0xF812_0164, 0x6566_6768, 0x696A_6B6C, 0x6D6E_6F70],
            [0xFC12_0171, 0x7273_7400, 0, 0],
        ]
    );
}

// §7.1.7: 0xFF is every block, a number one block.
#[test]
fn function_block_discovery_answers_one_block_or_all() {
    let all = ask(
        &mut device(),
        block_discovery(BlockSelect::All, true, false),
    );
    assert_eq!(all.iter().map(status).collect::<Vec<_>>(), [0x11, 0x11]);
    assert_eq!((all[0].0[0] >> 8) & 0x7F, 0);
    assert_eq!((all[1].0[0] >> 8) & 0x7F, 3);
    let one = ask(
        &mut device(),
        block_discovery(BlockSelect::One(id(0)), true, false),
    );
    assert_eq!(one.len(), 1);
    assert_eq!((one[0].0[0] >> 8) & 0x7F, 0);
    let three = ask(
        &mut device(),
        block_discovery(BlockSelect::One(id(3)), true, false),
    );
    assert_eq!(three.len(), 1);
    assert_eq!((three[0].0[0] >> 8) & 0x7F, 3);
}

// "i" and "n" are separate bits, and for 0xFF all infos come before the names.
#[test]
fn function_block_filter_bits_pick_info_and_name() {
    let both = ask(&mut device(), block_discovery(BlockSelect::All, true, true));
    assert_eq!(
        both.iter().map(status).collect::<Vec<_>>(),
        [0x11, 0x11, 0x12, 0x12]
    );
    let name = ask(
        &mut device(),
        block_discovery(BlockSelect::All, false, true),
    );
    assert_eq!(name.iter().map(status).collect::<Vec<_>>(), [0x12, 0x12]);
    assert!(
        ask(
            &mut device(),
            block_discovery(BlockSelect::All, false, false)
        )
        .is_empty()
    );
}

// The spec asks for 0 to 31 (§7.1.7) and names no reply for the rest: a number
// above 31, which decode keeps, and a block that is not there, answer nothing.
#[test]
fn an_out_of_range_or_absent_block_answers_nothing() {
    let wire = Ump([0xF010_2003, 0, 0, 0]);
    let Message::Stream(stream) = wire.decode().unwrap() else {
        panic!("a Stream message")
    };
    assert!(ask(&mut device(), stream).is_empty());
    assert!(
        ask(
            &mut device(),
            block_discovery(BlockSelect::One(id(31)), true, true)
        )
        .is_empty()
    );
    assert!(
        ask(
            &mut device(),
            block_discovery(BlockSelect::One(id(1)), true, true)
        )
        .is_empty()
    );
}

fn request(protocol: Protocol, rx_jr: bool, tx_jr: bool) -> Stream {
    Stream::StreamConfigurationRequest(StreamConfig {
        protocol,
        rx_jr,
        tx_jr,
    })
}

fn notified(out: &[Ump]) -> StreamConfig {
    assert_eq!(out.len(), 1);
    match out[0].decode().unwrap() {
        Message::Stream(Stream::StreamConfigurationNotification(c)) => c,
        other => panic!("{other:?}"),
    }
}

// §7.1.6.2 and §7.1.6.3: the reply reports what is now active, and the new
// configuration is kept.
#[test]
fn a_supported_request_is_honoured_and_reported() {
    let mut e = device();
    let out = ask(&mut e, request(Protocol::Midi2, true, true));
    assert_eq!(words(&out[0]), [0xF006_0203, 0, 0, 0]);
    assert_eq!(e.active().protocol, Protocol::Midi2);
    let again = ask(
        &mut e,
        discovery(DiscoveryFilter {
            stream_configuration: true,
            ..NONE
        }),
    );
    assert_eq!(words(&again[0]), [0xF006_0203, 0, 0, 0]);
    // JR off is always possible.
    let out = ask(&mut e, request(Protocol::Midi2, false, false));
    assert_eq!(words(&out[0]), [0xF006_0200, 0, 0, 0]);
}

// §7.1.6.1 step 3: "If the Receiver is unable to change some or all parts of the
// configuration, the Receiver replies with its current configuration."
#[test]
fn what_an_endpoint_cannot_do_is_not_done() {
    let mut midi1_only = Endpoint::midi1_device(None);
    let c = notified(&ask(&mut midi1_only, request(Protocol::Midi2, true, true)));
    assert_eq!(
        c,
        StreamConfig {
            protocol: Protocol::Midi1,
            rx_jr: false,
            tx_jr: false
        }
    );
    // Some parts: MIDI 2.0 and receive are possible, transmit is not.
    let mut e = Endpoint::new(Capabilities {
        midi2: true,
        rx_jr: true,
        ..Capabilities::default()
    });
    let c = notified(&ask(&mut e, request(Protocol::Midi2, true, true)));
    assert_eq!(
        c,
        StreamConfig {
            protocol: Protocol::Midi2,
            rx_jr: true,
            tx_jr: false
        }
    );
    // A reserved Protocol changes nothing.
    let c = notified(&ask(&mut e, request(Protocol::Reserved(7), false, false)));
    assert_eq!(c.protocol, Protocol::Midi2);
}

#[test]
fn a_message_that_is_not_a_request_is_not_answered() {
    let mut e = device();
    let mut out: Vec<Ump> = Vec::new();
    e.reply(Message::Utility(tutti_ump::Utility::Noop), &mut out);
    e.reply(
        Message::Stream(Stream::StreamConfigurationNotification(e.active())),
        &mut out,
    );
    assert!(out.is_empty());
}

// Design 017 §5: a MIDI 1.0 device is an Endpoint with one inferred block.
#[test]
fn a_midi1_device_has_one_inferred_block() {
    let e = Endpoint::midi1_device(EndpointName::new("Keystep"));
    let blocks: Vec<&FunctionBlock> = e.function_blocks().collect();
    assert_eq!(blocks.len(), 1);
    let b = blocks[0];
    assert_eq!(
        (b.id().get(), b.first_group().get(), b.groups().get()),
        (0, 0, 1)
    );
    assert!(b.active());
    assert_eq!(b.direction(), BlockDirection::Bidirectional);
    assert_eq!(b.midi1(), Midi1Port::Unrestricted);
    assert_eq!(e.declared_function_blocks(), 1);
    assert!(e.static_function_blocks());
    assert_eq!(e.active().protocol, Protocol::Midi1);
    assert_eq!(e.name().unwrap().as_str(), "Keystep");
    assert_eq!(
        e.capabilities(),
        Capabilities {
            midi1: true,
            ..Capabilities::default()
        }
    );
}

// The range checks `FunctionBlockBuilder::build` adds to the field types: a
// block stays inside 16 Groups (§7.1.8), a MIDI 1.0 port is one Group, the
// direction and the port are not the reserved values, and each refusal says
// which. (The field types' own ranges are `tutti-ump`'s tests;
// `check_range_contract` needs serde, which these refusals do not use.)
#[test]
fn a_function_block_stays_inside_the_groups() {
    let new = |first, span, dir| {
        FunctionBlock::builder(
            id(0),
            dir,
            FirstGroup::new(first).unwrap(),
            GroupSpan::new(span).unwrap(),
        )
    };
    let bi = BlockDirection::Bidirectional;
    assert!(new(0, 16, bi).build().is_ok());
    assert!(new(15, 1, bi).build().is_ok());
    let past = |first, span| BlockError::PastGroup16 {
        first_group: FirstGroup::new(first).unwrap(),
        groups: GroupSpan::new(span).unwrap(),
    };
    assert_eq!(new(15, 2, bi).build(), Err(past(15, 2)));
    assert_eq!(new(1, 16, bi).build(), Err(past(1, 16)));
    assert_eq!(
        new(0, 1, BlockDirection::Reserved(0)).build(),
        Err(BlockError::ReservedDirection)
    );
    let one = || new(2, 1, bi);
    assert!(one().midi1(Midi1Port::Restricted).build().is_ok());
    assert_eq!(
        one().midi1(Midi1Port::Reserved(3)).build(),
        Err(BlockError::ReservedMidi1Port)
    );
    let two = || new(2, 2, bi);
    assert_eq!(
        two().midi1(Midi1Port::Unrestricted).build(),
        Err(BlockError::Midi1PortSpansGroups {
            groups: GroupSpan::new(2).unwrap()
        })
    );
    assert!(two().midi1(Midi1Port::No).build().is_ok());
}

#[test]
fn a_block_is_replaced_by_number_and_the_count_follows_the_highest() {
    let e = Endpoint::new(FULL)
        .with_function_block(block(5, 0, 1).build().unwrap())
        .with_function_block(block(5, 2, 3).build().unwrap());
    assert_eq!(e.declared_function_blocks(), 6);
    assert_eq!(e.function_blocks().count(), 1);
    assert_eq!(e.function_block(id(5)).unwrap().first_group().get(), 2);
    assert!(e.function_block(id(0)).is_none());
}
