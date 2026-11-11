use super::*;
use tutti_ump::{Direction as BlockDirection, FirstGroup, FunctionBlockId, GroupSpan};

fn block() -> FunctionBlock {
    FunctionBlock::new(
        FunctionBlockId::new(3).unwrap(),
        BlockDirection::Output,
        FirstGroup::new(4).unwrap(),
        GroupSpan::new(2).unwrap(),
    )
    .unwrap()
}

/// A capability built from an `Endpoint` is its active protocol (not what it
/// offers) and its blocks, and the Endpoint a capability describes reads back
/// the same: the OS's description and the negotiated one meet in this value.
#[test]
fn a_capability_is_the_active_protocol_and_the_blocks_of_an_endpoint() {
    let offers_both = Endpoint::new(Capabilities {
        midi1: true,
        midi2: true,
        rx_jr: true,
        tx_jr: true,
    })
    .with_function_block(block());
    let cap = UmpCapability::of(&offers_both);
    assert_eq!(cap.protocol, Protocol::Midi1, "active, not offered");
    assert_eq!(cap.blocks, [block()]);

    let cap = UmpCapability::midi2().with_blocks(vec![block()]);
    let endpoint = cap.endpoint();
    assert_eq!(endpoint.active().protocol, Protocol::Midi2);
    assert!(!endpoint.active().rx_jr && !endpoint.active().tx_jr);
    assert_eq!(UmpCapability::of(&endpoint), cap);
}

/// What is a UMP Endpoint to ask (M2-104 Appendix I): the MIDI 2.0 Protocol in
/// use, or blocks declared. A MIDI 1.0 port with none has no Stream.
#[test]
fn only_a_ump_endpoint_is_asked() {
    assert!(UmpCapability::midi2().is_ump_endpoint());
    assert!(
        UmpCapability::midi1()
            .with_blocks(vec![block()])
            .is_ump_endpoint()
    );
    assert!(!UmpCapability::midi1().is_ump_endpoint());
}

#[test]
fn a_midi1_endpoint_does_not_carry_midi2_only_messages() {
    assert!(!UmpCapability::midi1().carries_midi2_only());
    assert!(UmpCapability::midi2().carries_midi2_only());
}

/// The default must be the *conservative* one.
///
/// The engine is MIDI-2-native, which is the wrong polarity for a *device*
/// whose protocol the OS never reported. Claiming MIDI 2.0 for an unknown endpoint would send
/// per-note messages to an endpoint that cannot carry them; claiming MIDI 1.0 only
/// costs resolution. This pins which way that falls.
#[test]
fn an_unreported_capability_defaults_to_midi1() {
    assert_eq!(UmpCapability::default().protocol, Protocol::Midi1);
    assert!(!UmpCapability::default().carries_midi2_only());
}

// NOTE: `EndpointId` is deliberately untested here. Its "a stale id must
// not resolve to a different device" is a property of the *backend lookup*,
// not of `EndpointId`: `assert_ne!(from_raw(1), from_raw(2))` exercises the
// derived `PartialEq` on a one-field newtype and cannot fail while the type
// compiles. Both become worth pinning if a hand-written `PartialEq` or an
// id-namespacing scheme ever lands.
