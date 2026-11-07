use super::*;
use crate::message::{ControllerNamespace, MidiMessage};

#[test]
fn registered_controller_decodes_via_midi2() {
    use midi2::channel_voice2::ChannelVoice2;
    use midi2::{Channeled, UmpMessage};
    let ev = MidiEvent::registered_controller(
        MidiGroup::FIRST,
        MidiChannel::of(3),
        0x00,
        0x06,
        0xDEAD_BEEF,
    );
    match UmpMessage::try_from(ev.data_words()).unwrap() {
        UmpMessage::ChannelVoice2(ChannelVoice2::RegisteredController(m)) => {
            assert_eq!(u8::from(m.channel()), 3);
            assert_eq!(u8::from(m.bank()), 0x00);
            assert_eq!(u8::from(m.index()), 0x06);
            assert_eq!(m.controller_data(), 0xDEAD_BEEF);
        }
        other => panic!("expected RegisteredController, got {other:?}"),
    }
}

#[test]
fn assignable_controller_decodes_via_midi2() {
    use midi2::channel_voice2::ChannelVoice2;
    use midi2::{Channeled, UmpMessage};
    let ev = MidiEvent::assignable_controller(
        MidiGroup::FIRST,
        MidiChannel::of(9),
        0x12,
        0x34,
        0x0000_1000,
    );
    match UmpMessage::try_from(ev.data_words()).unwrap() {
        UmpMessage::ChannelVoice2(ChannelVoice2::AssignableController(m)) => {
            assert_eq!(u8::from(m.channel()), 9);
            assert_eq!(u8::from(m.bank()), 0x12);
            assert_eq!(u8::from(m.index()), 0x34);
            assert_eq!(m.controller_data(), 0x0000_1000);
        }
        other => panic!("expected AssignableController, got {other:?}"),
    }
}

/// Decode one relative-controller event through the structured view,
/// panicking on any other message.
fn decode_relative(ev: &MidiEvent) -> (ControllerNamespace, u8, u8, i32) {
    match ev.message() {
        MidiMessage::RelativeController {
            namespace,
            bank,
            index,
            delta,
            ..
        } => (namespace, bank, index, delta),
        other => panic!("expected RelativeController, got {other:?}"),
    }
}

#[test]
fn relative_controllers_carry_signed_deltas() {
    // M2-104 §7.4.8: the data field "contains a Two's Complement value, to
    // provide negative and positive relative control" — a decrement must
    // survive as a negative number, not a huge unsigned one.
    for delta in [1i32, -1, 127, -128, i32::MAX, i32::MIN, 0] {
        let rpn = MidiEvent::relative_registered_controller(
            MidiGroup::FIRST,
            MidiChannel::of(3),
            0x12,
            0x34,
            delta,
        );
        assert_eq!(
            decode_relative(&rpn),
            (ControllerNamespace::Registered, 0x12, 0x34, delta),
            "registered delta {delta}"
        );

        let nrpn = MidiEvent::relative_assignable_controller(
            MidiGroup::FIRST,
            MidiChannel::of(9),
            0x01,
            0x02,
            delta,
        );
        assert_eq!(
            decode_relative(&nrpn),
            (ControllerNamespace::Assignable, 0x01, 0x02, delta),
            "assignable delta {delta}"
        );
    }
}

#[test]
fn relative_and_absolute_controllers_are_distinct_messages() {
    // Same address space (§7.4.8: "these new messages act upon the same
    // address space… and use the same controller Banks"), different status —
    // so an absolute set is never mistaken for a relative nudge.
    let absolute =
        MidiEvent::registered_controller(MidiGroup::FIRST, MidiChannel::of(3), 0x12, 0x34, 5);
    assert!(
        !matches!(absolute.message(), MidiMessage::RelativeController { .. }),
        "an absolute RPN is not a relative one"
    );

    let relative = MidiEvent::relative_registered_controller(
        MidiGroup::FIRST,
        MidiChannel::of(3),
        0x12,
        0x34,
        5,
    );
    assert_ne!(relative.data_words()[0], absolute.data_words()[0]);
    assert!(matches!(
        relative.message(),
        MidiMessage::RelativeController { .. }
    ));
}
