use super::*;

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
