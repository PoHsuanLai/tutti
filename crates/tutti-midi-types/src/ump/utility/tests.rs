use super::*;
use midi2::{UmpMessage, utility};
use tutti_fixtures::midi;

#[test]
fn noop_is_utility_zero_word() {
    let ev = midi::noop();
    let msg = UmpMessage::try_from(ev.words()).unwrap();
    assert!(matches!(
        msg,
        UmpMessage::Utility(utility::Utility::NoOp(_))
    ));
}

#[test]
fn jr_timestamp_decodes_via_midi2() {
    let ev = midi::jr_timestamp(0x1234);
    match UmpMessage::try_from(ev.words()).unwrap() {
        UmpMessage::Utility(utility::Utility::Timestamp(m)) => {
            assert_eq!(m.time_data(), 0x1234);
        }
        other => panic!("expected JR Timestamp, got {other:?}"),
    }
}

#[test]
fn jr_clock_decodes_via_midi2() {
    let ev = midi::jr_clock(0x89AB);
    match UmpMessage::try_from(ev.words()).unwrap() {
        UmpMessage::Utility(utility::Utility::Clock(m)) => {
            assert_eq!(m.time_data(), 0x89AB);
        }
        other => panic!("expected JR Clock, got {other:?}"),
    }
    assert_eq!(jr_clock_value(&ev), Some(0x89AB));
}

#[test]
fn jr_clock_and_timestamp_do_not_alias() {
    // Same 16-bit payload, different status (0x1 vs 0x2) and different
    // meaning: a Clock states the sender's time, a Timestamp times the
    // message after it (§7.2.2.1 / §7.2.2.2). Each accessor must reject the
    // other message, or a receiver would treat a clock announcement as
    // timing for the next event.
    let clock = midi::jr_clock(0x1234);
    let stamp = midi::jr_timestamp(0x1234);
    assert_ne!(clock.words()[0], stamp.words()[0]);
    assert_eq!(jr_clock_value(&clock), Some(0x1234));
    assert_eq!(jr_timestamp_value(&clock), None, "a Clock is not a stamp");
    assert_eq!(jr_timestamp_value(&stamp), Some(0x1234));
    assert_eq!(jr_clock_value(&stamp), None, "a stamp is not a Clock");
}

#[test]
fn utility_messages_leave_the_reserved_nibble_zero() {
    // M2-104-UM §2.1.2: utility messages are groupless; §2.1.3: reserved
    // fields "shall be set to zero". Bits 24-27 are where a group would sit
    // on a group-bearing type — on MT 0x0 they must stay clear.
    for ev in [
        midi::jr_timestamp(0xFFFF),
        midi::jr_clock(0xFFFF),
        midi::noop(),
    ] {
        let w0 = ev.words()[0];
        assert_eq!(w0 >> 28, 0x0, "utility message type");
        assert_eq!(
            (w0 >> 24) & 0x0F,
            0,
            "reserved nibble must be zero, got word {w0:#010x}"
        );
    }
}
