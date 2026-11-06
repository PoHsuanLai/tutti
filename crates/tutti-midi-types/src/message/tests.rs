use super::*;

#[test]
fn note_on_decodes_full_width() {
    let msg = MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::new(3), 60, 0xC000).message();
    assert!(msg.is_note_on());
    assert_eq!(msg.note(), Some(60));
    assert_eq!(msg.channel(), Some(MidiChannel::new(3)));
    assert_eq!(msg.velocity(), Some(0xC000)); // full 16-bit, not narrowed
}

#[test]
fn midi1_note_on_promotes_and_decodes() {
    // A MIDI 1.0 wire note-on decodes the same way — protocol is invisible here.
    let ev = MidiEvent::from_midi1_bytes(0, &[0x93, 60, 100]).unwrap();
    let msg = ev.message();
    assert!(msg.is_note_on());
    assert_eq!(msg.channel(), Some(MidiChannel::new(3)));
    assert_eq!(msg.note(), Some(60));
}

#[test]
fn velocity_zero_note_on_reads_as_note_off() {
    let ev = MidiEvent::from_midi1_bytes(0, &[0x90, 60, 0]).unwrap();
    let msg = ev.message();
    assert!(msg.is_note_off());
    assert!(!msg.is_note_on());
}

#[test]
fn control_change_carries_32bit_value() {
    let msg = MidiEvent::cc(
        MidiGroup::FIRST,
        MidiChannel::new(5),
        CCNumber::BRIGHTNESS,
        0xDEAD_BEEF,
    )
    .message();
    match msg {
        MidiMessage::ControlChange {
            channel,
            index,
            value,
            ..
        } => {
            assert_eq!(channel, MidiChannel::new(5));
            assert_eq!(index, 74);
            assert_eq!(value, 0xDEAD_BEEF);
        }
        other => panic!("expected ControlChange, got {other:?}"),
    }
}

#[test]
fn same_pitch_different_channel_have_distinct_ids() {
    let a = MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::new(1), 60, 0x8000)
        .message()
        .id()
        .unwrap();
    let b = MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::new(2), 60, 0x8000)
        .message()
        .id()
        .unwrap();
    assert_ne!(a, b);
}

#[test]
fn per_note_pitch_bend_addresses_one_note() {
    let msg = MidiEvent::per_note_pitch_bend(MidiGroup::FIRST, MidiChannel::FIRST, 64, 0x9000_0000)
        .message();
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
        MidiEvent::timing_clock(MidiGroup::FIRST).message(),
        MidiMessage::TimingClock { .. }
    ));
    assert!(matches!(
        MidiEvent::start(MidiGroup::FIRST).message(),
        MidiMessage::Start { .. }
    ));
    assert!(matches!(
        MidiEvent::stop(MidiGroup::FIRST).message(),
        MidiMessage::Stop { .. }
    ));
    assert!(matches!(
        MidiEvent::continue_msg(MidiGroup::FIRST).message(),
        MidiMessage::Continue { .. }
    ));
    assert_eq!(
        MidiEvent::timing_clock(MidiGroup::FIRST)
            .message()
            .channel(),
        None
    );
}

#[test]
fn system_common_data_messages_decode_their_payload() {
    match MidiEvent::mtc_quarter_frame(MidiGroup::FIRST, 0x5F).message() {
        MidiMessage::TimeCode { code, .. } => assert_eq!(code, 0x5F),
        other => panic!("expected TimeCode, got {other:?}"),
    }
    match MidiEvent::song_position(MidiGroup::FIRST, 12345).message() {
        MidiMessage::SongPosition { position, .. } => assert_eq!(position, 12345),
        other => panic!("expected SongPosition, got {other:?}"),
    }
    match MidiEvent::song_select(MidiGroup::FIRST, 0x4F).message() {
        MidiMessage::SongSelect { song, .. } => assert_eq!(song, 0x4F),
        other => panic!("expected SongSelect, got {other:?}"),
    }
}

#[test]
fn tune_request_stays_other() {
    // Tune Request is System Common but has no modeled variant — it must
    // still round-trip through `Other`.
    let tune = MidiEvent::tune_request(MidiGroup::FIRST);
    assert_eq!(tune.message(), MidiMessage::Other(tune));
}

#[test]
fn absolute_channel_controllers_decode_structured() {
    // RPN and NRPN differ only in namespace; both carry bank/index/data.
    let rpn = MidiEvent::registered_controller(
        MidiGroup::FIRST,
        MidiChannel::new(3),
        0x12,
        0x34,
        0xDEAD_BEEF,
    );
    assert_eq!(
        rpn.message(),
        MidiMessage::RegisteredController {
            frame_offset: 0,
            channel: MidiChannel::new(3),
            namespace: ControllerNamespace::Registered,
            bank: 0x12,
            index: 0x34,
            data: 0xDEAD_BEEF,
        }
    );

    let nrpn = MidiEvent::assignable_controller(
        MidiGroup::FIRST,
        MidiChannel::new(9),
        0x01,
        0x02,
        0x0000_1000,
    );
    assert_eq!(
        nrpn.message(),
        MidiMessage::RegisteredController {
            frame_offset: 0,
            channel: MidiChannel::new(9),
            namespace: ControllerNamespace::Assignable,
            bank: 0x01,
            index: 0x02,
            data: 0x0000_1000,
        }
    );
}

#[test]
fn relative_channel_controllers_decode_structured() {
    let rpn = MidiEvent::relative_registered_controller(
        MidiGroup::FIRST,
        MidiChannel::new(5),
        0x40,
        0x07,
        42,
    );
    assert_eq!(
        rpn.message(),
        MidiMessage::RelativeController {
            frame_offset: 0,
            channel: MidiChannel::new(5),
            namespace: ControllerNamespace::Registered,
            bank: 0x40,
            index: 0x07,
            delta: 42,
        }
    );

    let nrpn = MidiEvent::relative_assignable_controller(
        MidiGroup::FIRST,
        MidiChannel::new(0),
        0x7F,
        0x7E,
        7,
    );
    assert_eq!(
        nrpn.message(),
        MidiMessage::RelativeController {
            frame_offset: 0,
            channel: MidiChannel::new(0),
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
                MidiEvent::relative_registered_controller
                    as fn(MidiGroup, MidiChannel, u8, u8, i32) -> MidiEvent,
                ControllerNamespace::Registered,
            ),
            (
                MidiEvent::relative_assignable_controller,
                ControllerNamespace::Assignable,
            ),
        ] {
            let ev = build(MidiGroup::FIRST, MidiChannel::new(3), 0x12, 0x34, delta);
            match ev.message() {
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
    let ev = MidiEvent::registered_controller(MidiGroup::FIRST, MidiChannel::new(3), 0x11, 0x22, 1);
    match ev.message() {
        MidiMessage::RegisteredController { bank, index, .. } => {
            assert_eq!(bank, 0x11);
            assert_eq!(index, 0x22);
        }
        other => panic!("expected RegisteredController, got {other:?}"),
    }
}

#[test]
fn channel_controllers_round_trip_to_identical_wire_bytes() {
    // Decode → re-encode must reproduce the source packet exactly, frame
    // offset included, for all four channel-wide controller messages.
    for ev in [
        MidiEvent::registered_controller(
            MidiGroup::FIRST,
            MidiChannel::new(3),
            0x12,
            0x34,
            0xDEAD_BEEF,
        ),
        MidiEvent::assignable_controller(
            MidiGroup::FIRST,
            MidiChannel::new(9),
            0x01,
            0x02,
            0x0000_1000,
        ),
        MidiEvent::relative_registered_controller(
            MidiGroup::FIRST,
            MidiChannel::new(5),
            0x40,
            0x07,
            -1234,
        ),
        MidiEvent::relative_assignable_controller(
            MidiGroup::FIRST,
            MidiChannel::new(15),
            0x7F,
            0x00,
            i32::MIN,
        ),
    ] {
        let ev = ev.with_frame_offset(91);
        let msg = ev.message();
        assert!(
            !matches!(msg, MidiMessage::Other(_)),
            "must not fall through to Other: {msg:?}"
        );
        assert_eq!(msg.frame_offset(), 91);
        let back = MidiEvent::try_from(msg).expect("channel controller re-encodable");
        assert_eq!(
            back.data_words(),
            ev.data_words(),
            "wire mismatch for {msg:?}"
        );
        assert_eq!(back, ev, "round-trip mismatch for {msg:?}");
    }
}

#[test]
fn channel_controllers_report_their_channel() {
    // The `channel()` accessor's `|`-chain must cover the new variants —
    // otherwise they read as channel-less system messages.
    assert_eq!(
        MidiEvent::registered_controller(MidiGroup::FIRST, MidiChannel::new(11), 0, 6, 0)
            .message()
            .channel(),
        Some(MidiChannel::new(11))
    );
    assert_eq!(
        MidiEvent::relative_assignable_controller(MidiGroup::FIRST, MidiChannel::new(4), 0, 6, 0)
            .message()
            .channel(),
        Some(MidiChannel::new(4))
    );
}

#[test]
fn absolute_and_relative_are_different_variants() {
    // Same address, same bit pattern in the data field, different meaning:
    // `-1` as an absolute set is `0xFFFF_FFFF`, as a delta it is one step
    // down. The variant split is what keeps a consumer from confusing them.
    let absolute = MidiEvent::registered_controller(
        MidiGroup::FIRST,
        MidiChannel::new(3),
        0x12,
        0x34,
        0xFFFF_FFFF,
    )
    .message();
    let relative = MidiEvent::relative_registered_controller(
        MidiGroup::FIRST,
        MidiChannel::new(3),
        0x12,
        0x34,
        -1,
    )
    .message();
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
fn round_trip_preserves_frame_offset() {
    // The view must not drop sample-accurate timing (project round-trip invariant).
    let ev = MidiEvent::cc(
        MidiGroup::FIRST,
        MidiChannel::new(5),
        CCNumber::BRIGHTNESS,
        0xABCD_1234,
    )
    .with_frame_offset(137);
    let msg = ev.message();
    assert_eq!(msg.frame_offset(), 137);
    let back = MidiEvent::try_from(msg).expect("re-encodable");
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
    let ev = MidiEvent::from_ump(0, {
        use midi2::Data;
        on.data()
    });
    let msg = ev.message();
    match msg {
        MidiMessage::NoteOn { attribute, .. } => {
            assert_eq!(
                attribute,
                Some(NoteAttribute::Pitch7_9(Fixed7_9::from_bits(0x1234)))
            );
        }
        other => panic!("expected NoteOn, got {other:?}"),
    }
    let back = MidiEvent::try_from(msg).expect("re-encodable");
    assert_eq!(back, ev);
}

#[test]
fn round_trip_other_is_exact() {
    // An unmodeled message (tune request — System Common with no variant)
    // re-encodes byte-for-byte via the preserved event.
    let ev = MidiEvent::tune_request(MidiGroup::FIRST).with_frame_offset(42);
    assert!(matches!(ev.message(), MidiMessage::Other(_)));
    let back = MidiEvent::try_from(ev.message()).expect("Other round-trips");
    assert_eq!(back, ev);
}

#[test]
fn system_transport_round_trips_preserve_frame_offset() {
    // Each modeled system message must survive decode → re-encode with its
    // frame offset intact (project round-trip invariant).
    for ev in [
        MidiEvent::timing_clock(MidiGroup::FIRST),
        MidiEvent::start(MidiGroup::FIRST),
        MidiEvent::continue_msg(MidiGroup::FIRST),
        MidiEvent::stop(MidiGroup::FIRST),
        MidiEvent::mtc_quarter_frame(MidiGroup::FIRST, 0x5F),
        MidiEvent::song_position(MidiGroup::FIRST, 12345),
        MidiEvent::song_select(MidiGroup::FIRST, 0x4F),
        MidiEvent::active_sensing(MidiGroup::FIRST),
        MidiEvent::system_reset(MidiGroup::FIRST),
    ] {
        let ev = ev.with_frame_offset(77);
        let msg = ev.message();
        assert_eq!(msg.frame_offset(), 77);
        let back = MidiEvent::try_from(msg).expect("system message re-encodable");
        assert_eq!(back, ev, "round-trip mismatch for {msg:?}");
    }
}
