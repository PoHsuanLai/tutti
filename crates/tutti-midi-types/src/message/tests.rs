use super::*;
use crate::from_midi1_bytes;
use tutti_fixtures::midi;
use tutti_ump::{Message, System, U7};

// Single System messages the fixtures do not build, written through the one
// encoder (`From<Message> for Ump`), so these tests read as the message sent.
fn system(s: System) -> Ump {
    Message::System(MidiGroup::FIRST, s).into()
}
fn mtc_quarter_frame(_group: MidiGroup, data: u8) -> Ump {
    system(System::TimeCode(U7::new(data).unwrap()))
}
fn song_select(_group: MidiGroup, song: u8) -> Ump {
    system(System::SongSelect(U7::new(song).unwrap()))
}
fn tune_request(_group: MidiGroup) -> Ump {
    system(System::TuneRequest)
}
fn active_sensing(_group: MidiGroup) -> Ump {
    system(System::ActiveSensing)
}
fn system_reset(_group: MidiGroup) -> Ump {
    system(System::Reset)
}

#[test]
fn note_on_decodes_full_width() {
    let msg = MidiMessage::from_ump(midi::note_on(
        MidiGroup::FIRST,
        MidiChannel::of(3),
        60,
        0xC000,
    ));
    assert!(msg.is_note_on());
    assert_eq!(msg.note(), Some(60));
    assert_eq!(msg.channel(), Some(MidiChannel::of(3)));
    assert_eq!(msg.velocity(), Some(0xC000)); // full 16-bit, not narrowed
}

#[test]
fn midi1_note_on_promotes_and_decodes() {
    // A MIDI 1.0 wire note-on decodes the same way — protocol is invisible here.
    let ev = from_midi1_bytes(&[0x93, 60, 100]).unwrap();
    let msg = MidiMessage::from_ump(ev);
    assert!(msg.is_note_on());
    assert_eq!(msg.channel(), Some(MidiChannel::of(3)));
    assert_eq!(msg.note(), Some(60));
}

#[test]
fn velocity_zero_note_on_reads_as_note_off() {
    let ev = from_midi1_bytes(&[0x90, 60, 0]).unwrap();
    let msg = MidiMessage::from_ump(ev);
    assert!(msg.is_note_off());
    assert!(!msg.is_note_on());
}

#[test]
fn control_change_carries_32bit_value() {
    let msg = MidiMessage::from_ump(midi::cc(
        MidiGroup::FIRST,
        MidiChannel::of(5),
        CCNumber::BRIGHTNESS,
        0xDEAD_BEEF,
    ));
    match msg {
        MidiMessage::ControlChange {
            channel,
            index,
            value,
            ..
        } => {
            assert_eq!(channel, MidiChannel::of(5));
            assert_eq!(index, 74);
            assert_eq!(value, 0xDEAD_BEEF);
        }
        other => panic!("expected ControlChange, got {other:?}"),
    }
}

#[test]
fn same_pitch_different_channel_have_distinct_ids() {
    let a = MidiMessage::from_ump(midi::note_on(
        MidiGroup::FIRST,
        MidiChannel::of(1),
        60,
        0x8000,
    ))
    .id()
    .unwrap();
    let b = MidiMessage::from_ump(midi::note_on(
        MidiGroup::FIRST,
        MidiChannel::of(2),
        60,
        0x8000,
    ))
    .id()
    .unwrap();
    assert_ne!(a, b);
}

#[test]
fn per_note_pitch_bend_addresses_one_note() {
    let msg = MidiMessage::from_ump(midi::per_note_pitch_bend(
        MidiGroup::FIRST,
        MidiChannel::FIRST,
        64,
        0x9000_0000,
    ));
    match msg {
        MidiMessage::PerNotePitchBend { note, value, .. } => {
            assert_eq!(note, 64);
            assert_eq!(value, 0x9000_0000);
        }
        other => panic!("expected PerNotePitchBend, got {other:?}"),
    }
}

#[test]
fn system_transport_messages_decode_first_class() {
    // Timing clock / start / stop / continue are modeled variants now, not
    // `Other`. They carry no channel.
    assert!(matches!(
        MidiMessage::from_ump(midi::timing_clock(MidiGroup::FIRST)),
        MidiMessage::TimingClock
    ));
    assert!(matches!(
        MidiMessage::from_ump(midi::start(MidiGroup::FIRST)),
        MidiMessage::Start
    ));
    assert!(matches!(
        MidiMessage::from_ump(midi::stop(MidiGroup::FIRST)),
        MidiMessage::Stop
    ));
    assert!(matches!(
        MidiMessage::from_ump(midi::continue_msg(MidiGroup::FIRST)),
        MidiMessage::Continue
    ));
    assert_eq!(
        MidiMessage::from_ump(midi::timing_clock(MidiGroup::FIRST)).channel(),
        None
    );
}

#[test]
fn system_common_data_messages_decode_their_payload() {
    match MidiMessage::from_ump(mtc_quarter_frame(MidiGroup::FIRST, 0x5F)) {
        MidiMessage::TimeCode { code, .. } => assert_eq!(code, 0x5F),
        other => panic!("expected TimeCode, got {other:?}"),
    }
    match MidiMessage::from_ump(midi::song_position(MidiGroup::FIRST, 12345)) {
        MidiMessage::SongPosition { position, .. } => assert_eq!(position, 12345),
        other => panic!("expected SongPosition, got {other:?}"),
    }
    match MidiMessage::from_ump(song_select(MidiGroup::FIRST, 0x4F)) {
        MidiMessage::SongSelect { song, .. } => assert_eq!(song, 0x4F),
        other => panic!("expected SongSelect, got {other:?}"),
    }
}

#[test]
fn tune_request_stays_other() {
    // Tune Request is System Common but has no modeled variant — it must
    // still round-trip through `Other`.
    let tune = tune_request(MidiGroup::FIRST);
    assert_eq!(MidiMessage::from_ump(tune), MidiMessage::Other(tune));
}

#[test]
fn absolute_channel_controllers_decode_structured() {
    // RPN and NRPN differ only in namespace; both carry bank/index/data.
    let rpn = midi::registered_controller(
        MidiGroup::FIRST,
        MidiChannel::of(3),
        0x12,
        0x34,
        0xDEAD_BEEF,
    );
    assert_eq!(
        MidiMessage::from_ump(rpn),
        MidiMessage::RegisteredController {
            channel: MidiChannel::of(3),
            namespace: ControllerNamespace::Registered,
            bank: 0x12,
            index: 0x34,
            data: 0xDEAD_BEEF,
        }
    );

    let nrpn = midi::assignable_controller(
        MidiGroup::FIRST,
        MidiChannel::of(9),
        0x01,
        0x02,
        0x0000_1000,
    );
    assert_eq!(
        MidiMessage::from_ump(nrpn),
        MidiMessage::RegisteredController {
            channel: MidiChannel::of(9),
            namespace: ControllerNamespace::Assignable,
            bank: 0x01,
            index: 0x02,
            data: 0x0000_1000,
        }
    );
}

#[test]
fn relative_channel_controllers_decode_structured() {
    let rpn =
        midi::relative_registered_controller(MidiGroup::FIRST, MidiChannel::of(5), 0x40, 0x07, 42);
    assert_eq!(
        MidiMessage::from_ump(rpn),
        MidiMessage::RelativeController {
            channel: MidiChannel::of(5),
            namespace: ControllerNamespace::Registered,
            bank: 0x40,
            index: 0x07,
            delta: 42,
        }
    );

    let nrpn =
        midi::relative_assignable_controller(MidiGroup::FIRST, MidiChannel::of(0), 0x7F, 0x7E, 7);
    assert_eq!(
        MidiMessage::from_ump(nrpn),
        MidiMessage::RelativeController {
            channel: MidiChannel::of(0),
            namespace: ControllerNamespace::Assignable,
            bank: 0x7F,
            index: 0x7E,
            delta: 7,
        }
    );
}

#[test]
fn relative_controller_delta_stays_signed() {
    // M2-104 §7.4.8: the data field "contains a Two's Complement value". A
    // decrement must arrive as a negative `i32`, not as a near-`u32::MAX`
    // reinterpretation that would slam the parameter to full scale.
    for delta in [-1i32, -128, -0x0100_0000, i32::MIN, 0, 1, i32::MAX] {
        for (build, namespace) in [
            (
                midi::relative_registered_controller
                    as fn(MidiGroup, MidiChannel, u8, u8, i32) -> Ump,
                ControllerNamespace::Registered,
            ),
            (
                midi::relative_assignable_controller,
                ControllerNamespace::Assignable,
            ),
        ] {
            let ev = build(MidiGroup::FIRST, MidiChannel::of(3), 0x12, 0x34, delta);
            match MidiMessage::from_ump(ev) {
                MidiMessage::RelativeController {
                    delta: decoded,
                    namespace: ns,
                    ..
                } => {
                    assert_eq!(decoded, delta, "delta {delta} for {namespace:?}");
                    assert_eq!(ns, namespace);
                }
                other => panic!("expected RelativeController, got {other:?}"),
            }
        }
    }
}

#[test]
fn bank_and_index_are_not_transposed() {
    // Distinct bank/index values, so a swap in either the decode or the
    // re-encode shows up rather than cancelling out.
    let ev = midi::registered_controller(MidiGroup::FIRST, MidiChannel::of(3), 0x11, 0x22, 1);
    match MidiMessage::from_ump(ev) {
        MidiMessage::RegisteredController { bank, index, .. } => {
            assert_eq!(bank, 0x11);
            assert_eq!(index, 0x22);
        }
        other => panic!("expected RegisteredController, got {other:?}"),
    }
}

#[test]
fn channel_controllers_round_trip_to_identical_wire_bytes() {
    // Decode → re-encode must reproduce the source packet exactly, for all four
    // channel-wide controller messages.
    for ev in [
        midi::registered_controller(
            MidiGroup::FIRST,
            MidiChannel::of(3),
            0x12,
            0x34,
            0xDEAD_BEEF,
        ),
        midi::assignable_controller(
            MidiGroup::FIRST,
            MidiChannel::of(9),
            0x01,
            0x02,
            0x0000_1000,
        ),
        midi::relative_registered_controller(
            MidiGroup::FIRST,
            MidiChannel::of(5),
            0x40,
            0x07,
            -1234,
        ),
        midi::relative_assignable_controller(
            MidiGroup::FIRST,
            MidiChannel::of(15),
            0x7F,
            0x00,
            i32::MIN,
        ),
    ] {
        let msg = MidiMessage::from_ump(ev);
        assert!(
            !matches!(msg, MidiMessage::Other(_)),
            "must not fall through to Other: {msg:?}"
        );
        let back = Ump::try_from(msg).expect("channel controller re-encodable");
        assert_eq!(back.words(), ev.words(), "wire mismatch for {msg:?}");
        assert_eq!(back, ev, "round-trip mismatch for {msg:?}");
    }
}

#[test]
fn channel_controllers_report_their_channel() {
    // The `channel()` accessor's `|`-chain must cover the new variants —
    // otherwise they read as channel-less system messages.
    assert_eq!(
        MidiMessage::from_ump(midi::registered_controller(
            MidiGroup::FIRST,
            MidiChannel::of(11),
            0,
            6,
            0
        ))
        .channel(),
        Some(MidiChannel::of(11))
    );
    assert_eq!(
        MidiMessage::from_ump(midi::relative_assignable_controller(
            MidiGroup::FIRST,
            MidiChannel::of(4),
            0,
            6,
            0
        ))
        .channel(),
        Some(MidiChannel::of(4))
    );
}

#[test]
fn absolute_and_relative_are_different_variants() {
    // Same address, same bit pattern in the data field, different meaning:
    // `-1` as an absolute set is `0xFFFF_FFFF`, as a delta it is one step
    // down. The variant split is what keeps a consumer from confusing them.
    let absolute = MidiMessage::from_ump(midi::registered_controller(
        MidiGroup::FIRST,
        MidiChannel::of(3),
        0x12,
        0x34,
        0xFFFF_FFFF,
    ));
    let relative = MidiMessage::from_ump(midi::relative_registered_controller(
        MidiGroup::FIRST,
        MidiChannel::of(3),
        0x12,
        0x34,
        -1,
    ));
    assert!(matches!(
        absolute,
        MidiMessage::RegisteredController {
            data: 0xFFFF_FFFF,
            ..
        }
    ));
    assert!(matches!(
        relative,
        MidiMessage::RelativeController { delta: -1, .. }
    ));
    assert_ne!(absolute, relative);
}

#[test]
fn round_trip_is_exact() {
    // Decode → re-encode reproduces the packet (project round-trip invariant).
    let ev = midi::cc(
        MidiGroup::FIRST,
        MidiChannel::of(5),
        CCNumber::BRIGHTNESS,
        0xABCD_1234,
    );
    let msg = MidiMessage::from_ump(ev);
    let back = Ump::try_from(msg).expect("re-encodable");
    assert_eq!(back, ev);
}

#[test]
fn round_trip_preserves_note_attribute() {
    use midi2::channel_voice2::NoteAttribute;
    use midi2::num::Fixed7_9;
    // A note-on carrying a Pitch7_9 attribute must survive decode → re-encode.
    let mut on = midi2::channel_voice2::NoteOn::<[u32; 2]>::new();
    {
        use midi2::prelude::*;
        on.set_channel(u4::new(3));
        on.set_note_number(u7::new(60));
        on.set_velocity(0x8000);
        on.set_attribute(Some(NoteAttribute::Pitch7_9(Fixed7_9::from_bits(0x1234))));
    }
    let ev = crate::ump::packet_from({
        use midi2::Data;
        on.data()
    });
    let msg = MidiMessage::from_ump(ev);
    match msg {
        MidiMessage::NoteOn { attribute, .. } => {
            assert_eq!(
                attribute,
                Some(NoteAttribute::Pitch7_9(Fixed7_9::from_bits(0x1234)))
            );
        }
        other => panic!("expected NoteOn, got {other:?}"),
    }
    let back = Ump::try_from(msg).expect("re-encodable");
    assert_eq!(back, ev);
}

#[test]
fn round_trip_other_is_exact() {
    // An unmodeled message (tune request — System Common with no variant)
    // re-encodes byte-for-byte via the preserved event.
    let ev = tune_request(MidiGroup::FIRST);
    assert!(matches!(MidiMessage::from_ump(ev), MidiMessage::Other(_)));
    let back = Ump::try_from(MidiMessage::from_ump(ev)).expect("Other round-trips");
    assert_eq!(back, ev);
}

#[test]
fn system_transport_round_trips_are_exact() {
    // Each modeled system message must survive decode → re-encode unchanged
    // (project round-trip invariant).
    for ev in [
        midi::timing_clock(MidiGroup::FIRST),
        midi::start(MidiGroup::FIRST),
        midi::continue_msg(MidiGroup::FIRST),
        midi::stop(MidiGroup::FIRST),
        mtc_quarter_frame(MidiGroup::FIRST, 0x5F),
        midi::song_position(MidiGroup::FIRST, 12345),
        song_select(MidiGroup::FIRST, 0x4F),
        active_sensing(MidiGroup::FIRST),
        system_reset(MidiGroup::FIRST),
    ] {
        let msg = MidiMessage::from_ump(ev);
        let back = Ump::try_from(msg).expect("system message re-encodable");
        assert_eq!(back, ev, "round-trip mismatch for {msg:?}");
    }
}
