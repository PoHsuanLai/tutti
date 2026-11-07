use super::*;
use tutti_midi_types::convert::{bend_u32_to_signed_f32, u16_to_unit_f32, u32_to_unit_f32};
use tutti_plugin_types::{NoteExpressionType, NoteExpressionValue, note_id_for};
use tutti_ump::{
    CCNumber, Message, MidiChannel, MidiGroup, System, Ump, Voice2, decode as decode_ump,
};
use tutti_vst3_ffi::EventList;

/// Encodes one event on a fresh stream (every per-note bend range at the
/// MPE default).
fn encode(event: &MidiEvent) -> Option<Event> {
    MidiEncoder::default().encode(event)
}

#[test]
fn inbound_generic_data_cc_promotes_to_cv2() {
    // A CC has no typed VST3 slot, so it rounds through a generic `Data`
    // (3-byte MIDI-1) event. Decoding that must yield MIDI-2 Channel Voice
    // 2, not CV1 — the engine sees one vocabulary regardless of source
    // (the same widening the hardware input path makes).
    use tutti_midi_types::convert::midi1_cc_to_midi2;

    let event = MidiEvent::cc(
        MidiGroup::FIRST,
        MidiChannel::of(1),
        CCNumber::BRIGHTNESS,
        midi1_cc_to_midi2(100),
    );
    let vst3 = encode(&event).expect("CC -> Data");
    assert!(
        matches!(vst3.payload, Payload::Data(_)),
        "CC should be a Data event"
    );
    let back = vst3.to_midi().expect("cc decodes");
    match decode_ump(Ump(back.data)).expect("valid UMP") {
        Message::Midi2(address, Voice2::ControlChange { controller, .. }) => {
            assert_eq!(address.channel.get(), 1);
            assert_eq!(controller.get(), 74);
        }
        other => panic!("expected CV2 ControlChange, got {other:?}"),
    }
}

#[test]
fn inbound_system_message_passes_through() {
    // A System real-time message (Timing Clock 0xF8) has no CV form and must
    // pass through unchanged, not be dropped or widened.
    let event = MidiEvent::timing_clock(MidiGroup::FIRST);
    let vst3 = encode(&event).expect("clock -> Data");
    let back = vst3.to_midi().expect("clock decodes");
    assert!(
        matches!(
            decode_ump(Ump(back.data)),
            Ok(Message::System(_, System::TimingClock))
        ),
        "timing clock should stay a System Real-Time message"
    );
}

#[test]
fn note_on_lands_in_note_on_variant() {
    let event =
        MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::of(3), 60, 0x8000).with_frame_offset(5);
    let vst3 = encode(&event).expect("NoteOn should convert");
    match &vst3.payload {
        Payload::NoteOn(e) => {
            assert_eq!(e.channel, 3);
            assert_eq!(e.pitch, 60);
            assert_eq!(vst3.sample_offset(), 5);
            assert!(e.velocity > 0.0, "expected non-zero velocity");
        }
        _ => panic!("expected NoteOn variant"),
    }

    let back = vst3.to_midi().expect("round-trip");
    assert!(back.is_note_on());
    assert_eq!(back.note(), Some(60));
    assert_eq!(back.frame_offset, 5);
}

#[test]
fn note_off_lands_in_note_off_variant() {
    const RELEASE: u16 = 0x4000;
    let event = MidiEvent::note_off(MidiGroup::FIRST, MidiChannel::FIRST, 72, RELEASE)
        .with_frame_offset(10);
    let vst3 = encode(&event).expect("NoteOff should convert");
    match &vst3.payload {
        Payload::NoteOff(e) => {
            assert_eq!(e.pitch, 72);
            assert_eq!(vst3.sample_offset(), 10);
            assert!(
                e.velocity > 0.0,
                "release velocity was dropped on the way to the plugin — \
                 release-layer instruments get the minimum on every note-off"
            );
        }
        _ => panic!("expected NoteOff variant"),
    }

    let back = vst3.to_midi().expect("round-trip");
    assert!(back.is_note_off());
    assert_eq!(back.note(), Some(72));
    assert!(
        back.velocity_u7().is_some_and(|v| v > 0),
        "release velocity was dropped on the way back from the plugin"
    );
}

/// A frame offset past `i32::MAX` must saturate, not wrap negative.
///
/// `MidiEvent::frame_offset` is `u32` and VST3's `sampleOffset` is `i32`, so
/// a bare `as i32` turns a large offset into a negative one, which the plugin
/// indexes its own buffers with, out of bounds, where nothing here can catch it.
#[test]
fn a_huge_frame_offset_saturates_rather_than_going_negative() {
    let event = MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x8000)
        .with_frame_offset(u32::MAX);
    let vst3 = encode(&event).expect("NoteOn should convert");
    let offset = vst3.sample_offset();
    assert!(
        offset >= 0,
        "sample_offset went negative ({offset}) — the plugin will index \
         its buffers out of bounds"
    );
    assert_eq!(offset, i32::MAX, "an unrepresentable offset must saturate");
}

#[test]
fn poly_pressure_lands_in_poly_pressure_variant() {
    use tutti_midi_types::convert::midi1_cc_to_midi2;
    let event = MidiEvent::poly_pressure(
        MidiGroup::FIRST,
        MidiChannel::of(1),
        60,
        midi1_cc_to_midi2(100),
    )
    .with_frame_offset(0);
    let vst3 = encode(&event).expect("PolyPressure should convert");
    assert!(matches!(vst3.payload, Payload::PolyPressure(_)));
    let back = vst3.to_midi().expect("round-trip");
    assert_eq!(back.note(), Some(60));
}

#[test]
fn cc_falls_through_to_data_event() {
    use tutti_midi_types::convert::midi1_cc_to_midi2;
    let event = MidiEvent::cc(
        MidiGroup::FIRST,
        MidiChannel::of(2),
        CCNumber::BRIGHTNESS,
        midi1_cc_to_midi2(100),
    );
    let vst3 = encode(&event).expect("CC should convert");
    match &vst3.payload {
        Payload::Data(e) => {
            assert_eq!(e.size, 3);
            assert_eq!(e.bytes[0], 0xB0 | 2);
            assert_eq!(e.bytes[1], 74);
            assert_eq!(e.bytes[2], 100);
        }
        _ => panic!("expected Data variant for CC"),
    }
}

#[test]
fn pitch_bend_falls_through_to_data_event() {
    use tutti_midi_types::convert::midi1_pitch_bend_to_midi2;
    let event = MidiEvent::pitch_bend(
        MidiGroup::FIRST,
        MidiChannel::FIRST,
        midi1_pitch_bend_to_midi2(8192),
    );
    let vst3 = encode(&event).expect("PitchBend should convert");
    match &vst3.payload {
        Payload::Data(e) => {
            assert_eq!(e.size, 3);
            assert_eq!(e.bytes[0] & 0xF0, 0xE0);
            let value = (e.bytes[1] as u16) | ((e.bytes[2] as u16) << 7);
            assert_eq!(value, 8192, "PitchBend should round-trip to center");
        }
        _ => panic!("expected Data variant for PitchBend"),
    }
}

#[test]
fn program_change_falls_through_to_data_event() {
    let event = MidiEvent::program_change(MidiGroup::FIRST, MidiChannel::of(9), 42, None);
    let vst3 = encode(&event).expect("ProgramChange should convert");
    match &vst3.payload {
        Payload::Data(e) => {
            assert_eq!(e.size, 3);
            assert_eq!(e.bytes[0], 0xC0 | 9);
            assert_eq!(e.bytes[1], 42);
        }
        _ => panic!("expected Data variant for ProgramChange"),
    }
}

/// The note-expression `typeId` table, asserted against the **absolute
/// numeric ids from `ivstnoteexpression.h`** — deliberately hardcoded here
/// rather than read back from `expression_type_id`, because a test
/// that compares the table to itself passes with a wrong table.
///
/// `kVolumeTypeID = 0`, `kPanTypeID = 1`, `kTuningTypeID = 2`,
/// `kVibratoTypeID = 3`, `kExpressionTypeID = 4`, `kBrightnessTypeID = 5`.
#[test]
fn note_expression_type_ids_match_the_vst3_spec() {
    assert_eq!(expression_type_id(NoteExpressionType::Volume), Some(0));
    assert_eq!(expression_type_id(NoteExpressionType::Pan), Some(1));
    assert_eq!(expression_type_id(NoteExpressionType::Tuning), Some(2));
    assert_eq!(expression_type_id(NoteExpressionType::Vibrato), Some(3));
    assert_eq!(
        expression_type_id(NoteExpressionType::Expression),
        Some(4),
        "kExpressionTypeID is 4 — NOT Brightness"
    );
    assert_eq!(
        expression_type_id(NoteExpressionType::Brightness),
        Some(5),
        "kBrightnessTypeID is 5"
    );
    // VST3 has no standard note-expression id for poly pressure; it rides
    // the kPolyPressureEvent path instead.
    assert_eq!(expression_type_id(NoteExpressionType::Pressure), None);
}

/// The decoder must mirror the same absolute ids, so an incoming 5 is
/// Brightness (not dropped) and an incoming 4 is Expression (not
/// mislabelled Brightness).
#[test]
fn expression_type_from_id_matches_the_vst3_spec() {
    assert_eq!(expression_type_from_id(0), Some(NoteExpressionType::Volume));
    assert_eq!(expression_type_from_id(1), Some(NoteExpressionType::Pan));
    assert_eq!(expression_type_from_id(2), Some(NoteExpressionType::Tuning));
    assert_eq!(
        expression_type_from_id(3),
        Some(NoteExpressionType::Vibrato)
    );
    assert_eq!(
        expression_type_from_id(4),
        Some(NoteExpressionType::Expression)
    );
    assert_eq!(
        expression_type_from_id(5),
        Some(NoteExpressionType::Brightness)
    );
    // kTextTypeID (6) / kPhonemeTypeID (7) carry text, not a value.
    assert_eq!(expression_type_from_id(6), None);
    assert_eq!(expression_type_from_id(7), None);
}

/// Every VST3-encodable dimension survives a to-id → from-id round trip.
/// Combined with the two absolute-id tests above, this pins both halves.
#[test]
fn note_expression_type_round_trips_for_every_encodable_dimension() {
    for ty in [
        NoteExpressionType::Volume,
        NoteExpressionType::Pan,
        NoteExpressionType::Tuning,
        NoteExpressionType::Vibrato,
        NoteExpressionType::Expression,
        NoteExpressionType::Brightness,
    ] {
        let id = expression_type_id(ty).expect("VST3-encodable");
        assert_eq!(expression_type_from_id(id), Some(ty), "{ty:?} @ id {id}");
    }
}

/// A plugin-defined `typeId` survives instead of being dropped.
///
/// VST3 reserves `kCustomStart` (100000) upward for dimensions a plugin
/// declares through `INoteExpressionController`. Decoding those to `None`
/// has the caller discard the event, so a plugin whose expressiveness is
/// entirely custom looks silent.
#[test]
fn a_plugin_defined_type_id_is_carried_not_dropped() {
    for id in [100_000, 100_001, 8, 12345, u32::MAX] {
        assert_eq!(
            expression_type_from_id(id),
            Some(NoteExpressionType::Custom(id)),
            "custom id {id} must survive decoding"
        );
    }
}

/// A custom id round-trips unchanged — it came from a VST3 plugin, so it
/// goes back out as the same number.
#[test]
fn a_custom_type_id_round_trips_unchanged() {
    for id in [100_000, 8, 999] {
        let ty = expression_type_from_id(id).expect("carried");
        assert_eq!(expression_type_id(ty), Some(id));
    }
}

/// The text slots stay out of the value path in BOTH directions.
///
/// `kTextTypeID` / `kPhonemeTypeID` ride `NoteExpressionTextEvent` and carry
/// a string. Admitting them as `Custom` would let the encoder emit a value
/// event on a text id — a malformed event the plugin would misread.
#[test]
fn the_text_type_ids_never_enter_the_value_path() {
    for id in [6, 7] {
        assert_eq!(
            expression_type_from_id(id),
            None,
            "id {id} carries text, not a value"
        );
        assert_eq!(
            expression_type_id(NoteExpressionType::Custom(id)),
            None,
            "a Custom({id}) must not be encodable as a value event"
        );
    }
}

/// A custom dimension survives staging into an event and reading back —
/// the full path the decoder feeds, not just the id map.
#[test]
fn a_custom_dimension_stages_and_reads_back() {
    let expr = NoteExpressionValue {
        sample_offset: 12,
        note_id: 3,
        expression_type: NoteExpressionType::Custom(100_042),
        value: 0.75,
    };
    let staged = expr.to_event().expect("custom id is encodable");
    match &staged.payload {
        Payload::ExpressionValue(e) => assert_eq!(e.typeId, 100_042),
        other => panic!("expected NoteExpression, got {other:?}"),
    }
    let back = staged.to_note_expression().expect("decodes back");
    assert_eq!(back.expression_type, NoteExpressionType::Custom(100_042));
    assert_eq!(back.value, 0.75);
    assert_eq!(back.note_id, 3);
}

/// `Expression` is a real VST3 dimension (id 4) and must survive staging
/// into an event and reading back out. The failure mode this guards is
/// Brightness consuming id 4, leaving Expression decoding to `None`.
#[test]
fn expression_dimension_stages_and_reads_back() {
    let expr = NoteExpressionValue {
        sample_offset: 3,
        note_id: note_id_for(2, 64),
        expression_type: NoteExpressionType::Expression,
        value: 0.25,
    };
    let ev = expr
        .to_event()
        .expect("Expression is VST3-encodable (id 4)");
    match &ev.payload {
        Payload::ExpressionValue(e) => assert_eq!(e.typeId, 4),
        other => panic!("expected NoteExpression, got {other:?}"),
    }
    let back = ev.to_note_expression().expect("decodes");
    assert_eq!(back.expression_type, NoteExpressionType::Expression);
    assert_eq!(back.note_id, expr.note_id);
    assert_eq!(back.sample_offset, 3);
}

/// A non-finite expression value is dropped, not passed through and not
/// clamped to an invented number.
///
/// `f64::clamp` is not a guard for this: it returns NaN for a NaN input,
/// so a clamp-only fix would still hand the plugin a NaN.
#[test]
fn a_non_finite_expression_value_is_dropped() {
    for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        let expr = NoteExpressionValue {
            sample_offset: 0,
            note_id: 7,
            expression_type: NoteExpressionType::Tuning,
            value: bad,
        };
        assert!(
            expr.to_event().is_none(),
            "{bad} is not a normalized value and must not reach the plugin"
        );
    }
}

/// A finite value outside `[0, 1]` is clamped rather than dropped.
///
/// Dropping would be the worse failure: the dimension would silently hold
/// whatever the plugin last saw, so a runaway modulator reads as a stuck
/// expression rather than as a clipped one.
#[test]
fn an_out_of_range_expression_value_is_clamped_not_dropped() {
    for (input, want) in [(1.25_f64, 1.0_f64), (-0.5, 0.0), (f64::MAX, 1.0)] {
        let expr = NoteExpressionValue {
            sample_offset: 0,
            note_id: 7,
            expression_type: NoteExpressionType::Tuning,
            value: input,
        };
        let staged = expr.to_event().expect("a finite value is always encodable");
        let back = staged.to_note_expression().expect("decodes");
        assert_eq!(back.value, want, "{input} must clamp to {want}");
    }
}

/// An in-range value passes through bit-exact.
///
/// Pins that the guard is a no-op for every producer inside this crate —
/// the MIDI paths all arrive pre-normalized, so a clamp that perturbed
/// them would go unseen by the two tests above.
#[test]
fn an_in_range_expression_value_is_untouched() {
    for value in [0.0_f64, f64::MIN_POSITIVE, 0.5, 0.7327, 1.0] {
        let expr = NoteExpressionValue {
            sample_offset: 0,
            note_id: 7,
            expression_type: NoteExpressionType::Tuning,
            value,
        };
        let staged = expr.to_event().expect("in-range is encodable");
        let back = staged.to_note_expression().expect("decodes");
        assert_eq!(back.value, value, "{value} is already normalized");
    }
}

#[test]
fn note_expression_is_not_a_midi_event() {
    let expr = NoteExpressionValue {
        sample_offset: 0,
        note_id: 1,
        expression_type: NoteExpressionType::Tuning,
        value: 0.5,
    };
    let vst3 = expr.to_event().expect("Tuning is VST3-encodable");
    assert!(vst3.to_midi().is_none());
}

#[test]
fn data_event_with_truncated_size_fails_gracefully() {
    let data = Data::new(0, &[0xB0]).expect("fits inline");
    assert!(Event::at(0, Payload::Data(data)).to_midi().is_none());
}

#[test]
fn note_on_and_off_share_a_derived_note_id() {
    // A note-on and its note-off for the same (channel, note) must carry the
    // same VST3 noteId so per-note events bind to the same voice.
    let on = encode(&MidiEvent::note_on(
        MidiGroup::FIRST,
        MidiChannel::of(5),
        67,
        0x8000,
    ))
    .unwrap();
    let off = encode(&MidiEvent::note_off(
        MidiGroup::FIRST,
        MidiChannel::of(5),
        67,
        0,
    ))
    .unwrap();
    let expected = note_id_for(5, 67);
    match (on.payload, off.payload) {
        (Payload::NoteOn(a), Payload::NoteOff(b)) => {
            assert_eq!(a.noteId, expected);
            assert_eq!(b.noteId, expected);
        }
        _ => panic!("expected NoteOn + NoteOff"),
    }
    // Different (channel, note) → different id.
    assert_ne!(note_id_for(5, 67), note_id_for(5, 68));
    assert_ne!(note_id_for(5, 67), note_id_for(6, 67));
}

#[test]
fn per_note_pitch_bend_becomes_tuning_expression() {
    use tutti_midi_types::convert::midi1_pitch_bend_to_midi2;
    // Center bend on note 67, channel 5 → Tuning expression at value 0.5,
    // bound to the same noteId the note-on for (5, 67) would carry.
    let event = MidiEvent::per_note_pitch_bend(
        MidiGroup::FIRST,
        MidiChannel::of(5),
        67,
        midi1_pitch_bend_to_midi2(8192),
    )
    .with_frame_offset(12);
    let vst3 = encode(&event).expect("per-note bend should map");
    let expr = vst3.to_note_expression().expect("is a note expression");
    assert_eq!(expr.expression_type, NoteExpressionType::Tuning);
    assert_eq!(expr.note_id, note_id_for(5, 67));
    assert_eq!(expr.sample_offset, 12);
    assert!((expr.value - 0.5).abs() < 0.01, "center bend → 0.5");
    // It is NOT a MIDI channel-voice event.
    assert!(vst3.to_midi().is_none());
}

/// A per-note pitch bend is told to the plugin as the semitones it
/// means: the bend over its channel's per-note range (±48 until the
/// stream sets one with RPN #00/07), on VST3's ±120-semitone tuning
/// scale. A quarter bend at 48 is 12 semitones, 0.5 + 12/240; after a
/// sensitivity of 24 on channel 3, 6 semitones there and 12 elsewhere.
///
/// Mutations (run): a `(bend + 1) / 2` map (full bend = ±120) →
/// 0.625, fails; the sensitivity read but not stored → channel 3 reads
/// 0.55, fails; the range set on every channel → channel 4 reads 0.525,
/// fails.
#[test]
fn a_per_note_bend_is_tuned_over_its_channels_range() {
    use tutti_midi_types::PitchBendSensitivity;
    use tutti_midi_types::convert::signed_f32_to_bend_u32;
    use tutti_midi_types::ump::{RPN_BANK_MPE, RPN_INDEX_PER_NOTE_PITCH_BEND_SENSITIVITY};

    let bend = |channel: u8, amount: f32| {
        MidiEvent::per_note_pitch_bend(
            MidiGroup::FIRST,
            MidiChannel::of(channel),
            60,
            signed_f32_to_bend_u32(amount),
        )
    };
    let tuning = |encoder: &mut MidiEncoder, e: &MidiEvent| match encoder.encode(e) {
        Some(Event {
            payload: Payload::ExpressionValue(x),
            ..
        }) => {
            assert_eq!(x.typeId, 2, "kTuningTypeID");
            x.value
        }
        other => panic!("expected a tuning expression, got {other:?}"),
    };
    let close = |got: f64, want: f64| (got - want).abs() < 1e-6;

    let mut encoder = MidiEncoder::default();
    assert!(close(
        tuning(&mut encoder, &bend(3, 0.25)),
        0.5 + 12.0 / 240.0
    ));
    assert!(close(
        tuning(&mut encoder, &bend(3, -1.0)),
        0.5 - 48.0 / 240.0
    ));

    let sensitivity = MidiEvent::registered_controller(
        MidiGroup::FIRST,
        MidiChannel::of(3),
        RPN_BANK_MPE,
        RPN_INDEX_PER_NOTE_PITCH_BEND_SENSITIVITY,
        PitchBendSensitivity::from_semitones(24).to_rpn_bits(),
    );
    assert!(
        encoder.encode(&sensitivity).is_none(),
        "the sensitivity is consumed"
    );
    assert!(close(
        tuning(&mut encoder, &bend(3, 0.25)),
        0.5 + 6.0 / 240.0
    ));
    assert!(close(
        tuning(&mut encoder, &bend(4, 0.25)),
        0.5 + 12.0 / 240.0
    ));
}

#[test]
fn per_note_controller_maps_known_indices_only() {
    use tutti_midi_types::convert::midi1_cc_to_midi2;
    // CC 74 (brightness) is a known dimension → Brightness expression.
    let known = MidiEvent::per_note_controller(
        MidiGroup::FIRST,
        MidiChannel::FIRST,
        60,
        74,
        midi1_cc_to_midi2(100),
        false,
    );
    match encode(&known) {
        Some(Event {
            payload: Payload::ExpressionValue(e),
            ..
        }) => {
            // Asserted against the SPEC's absolute id (kBrightnessTypeID = 5),
            // not against `expression_type_id` — comparing the table
            // to itself is what let the off-by-one at index 4 ship.
            assert_eq!(e.typeId, 5, "CC74 → kBrightnessTypeID");
            assert_eq!(e.noteId, note_id_for(0, 60));
        }
        other => panic!("expected Brightness note expression, got {other:?}"),
    }
    // An index with no VST3 expression counterpart is dropped.
    let unknown = MidiEvent::per_note_controller(
        MidiGroup::FIRST,
        MidiChannel::FIRST,
        60,
        33,
        midi1_cc_to_midi2(100),
        false,
    );
    assert!(encode(&unknown).is_none());
}

#[test]
fn single_packet_sysex_round_trips_through_data_event() {
    // A short SysEx (identity request) → VST3 Data event with the SysEx
    // subtype and 0xF0 … 0xF7 framing, then back to the same UMP payload.
    let payload = [0x7E, 0x7F, 0x06, 0x01];
    let event = MidiEvent::sysex7_single(MidiGroup::FIRST, &payload)
        .unwrap()
        .with_frame_offset(7);
    let vst3 = encode(&event).expect("SysEx should map to Data");
    match &vst3.payload {
        Payload::Data(e) => {
            assert_eq!(e.data_type, K_DATA_TYPE_MIDI_SYSEX);
            assert_eq!(e.size as usize, payload.len() + 2);
            assert_eq!(e.bytes[0], 0xF0);
            assert_eq!(e.bytes[1..1 + payload.len()], payload);
            assert_eq!(e.bytes[1 + payload.len()], 0xF7);
            assert_eq!(vst3.sample_offset(), 7);
        }
        _ => panic!("expected Data variant for SysEx"),
    }

    let back = vst3.to_midi().expect("SysEx Data round-trips");
    let (status, bytes, n) = back.sysex7_payload().expect("decodes as SysEx7");
    assert_eq!(status, tutti_midi_types::ump::SYSEX7_STATUS_SINGLE);
    assert_eq!(&bytes[..n], &payload);
    assert_eq!(back.frame_offset, 7);
}

/// **No packet of a multi-packet SysEx converts on its own**: a lone
/// `START` would hand the plugin an unterminated message. The event list
/// reassembles them (`stage`'s SysEx tests pin that).
///
/// Mutation (run): best-effort `START` forwarding → the
/// `START` converts → fails.
#[test]
fn a_sysex_fragment_does_not_convert_alone() {
    let mut frags = Vec::new();
    MidiEvent::sysex7_fragments(MidiGroup::FIRST, &[1, 2, 3, 4, 5, 6, 7, 8], &mut frags);
    assert!(frags.len() > 1);
    for frag in &frags {
        assert!(encode(frag).is_none(), "fragment {frag:?} converted alone");
    }
}

#[test]
fn note_on_velocity_round_trips_at_full_width() {
    // A MIDI-2 note-on carrying a value not representable in 7 bits keeps
    // (close to) its width through the f32 VST3 struct on the way back out,
    // rather than collapsing to a 7-bit grid point.
    let velocity_u16 = 0x9123; // not a multiple of the 7-bit step
    let event = MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, velocity_u16);
    let vst3 = encode(&event).expect("converts");
    let back = vst3.to_midi().expect("round-trips");
    match decode_ump(Ump(back.data)).expect("decodes") {
        Message::Midi2(_, Voice2::NoteOn { velocity: raw, .. }) => {
            let velocity = u16_to_unit_f32(raw);
            let expected = velocity_u16 as f32 / u16::MAX as f32;
            // f32 velocity carries far more than 7 bits — error stays tiny.
            assert!(
                (velocity - expected).abs() < 1e-3,
                "velocity {velocity} vs {expected}"
            );
        }
        _ => panic!("expected NoteOn"),
    }
}

// ── int / chord / scale / text / legacy-cc-out ───────────────────────────

fn legacy_cc(control: u8, channel: i8, value: i8, value2: i8) -> LegacyCcOut {
    LegacyCcOut(vst3::Steinberg::Vst::LegacyMIDICCOutEvent {
        controlNumber: control,
        channel,
        value,
        value2,
    })
}

/// Mutation (run): `decode_int` reading `e.value.cast_signed().abs()` → -1234
/// comes back 1234 → fails.
#[test]
fn note_expression_int_round_trips_with_its_sign() {
    let value = NoteExpressionIntValue {
        sample_offset: 9,
        note_id: note_id_for(1, 50),
        type_id: 3,
        value: -1234,
    };
    let event = value.to_event().expect("always encodable");
    let Payload::ExpressionInt(raw) = event.payload else {
        panic!("not an int expression");
    };
    assert_eq!(raw.value, (-1234i64).cast_unsigned(), "the same 64 bits");
    let back = event.to_expression_int().expect("int expr");
    assert_eq!(back.value, -1234);
    assert_eq!(back.note_id, note_id_for(1, 50));
    assert_eq!((back.type_id, back.sample_offset), (3, 9));
    assert!(event.to_midi().is_none(), "not a MIDI message");
}

/// A chord, scale and text value stage into a list, intern their text, and read
/// back from the event and the arena.
///
/// Mutation (run): `Utf16Chord::to_event` staging `bass_note` as the root →
/// the chord comes back with root 48 → fails.
#[test]
fn harmony_values_round_trip_through_a_list() {
    let utf16 = |s: &str| s.encode_utf16().collect::<Vec<u16>>();
    let chord = Utf16Chord {
        sample_offset: 12,
        root: 60,
        bass_note: 48,
        mask: 0b100_1001_0001,
        text: utf16("Cmaj7"),
    };
    let scale = Utf16Scale {
        sample_offset: 0,
        root: 62,
        mask: 0x5ab5,
        text: utf16("D Dorian"),
    };
    let text = Utf16Text {
        sample_offset: 4,
        note_id: note_id_for(2, 64),
        type_id: 7,
        text: utf16("staccato"),
    };

    let list = EventList::input();
    let mut stage = list.begin();
    for event in [
        chord.to_event(&mut stage),
        scale.to_event(&mut stage),
        text.to_event(&mut stage),
    ] {
        assert!(stage.push(event));
    }
    drop(stage);

    list.with_events(|staged| {
        let by_kind = |f: fn(&Payload) -> bool| {
            staged
                .events
                .iter()
                .find(|e| f(&e.payload))
                .expect("staged")
        };
        let back = by_kind(|p| matches!(p, Payload::Chord(_)))
            .to_chord(staged.text)
            .expect("chord");
        assert_eq!((back.root, back.bass_note, back.mask), (60, 48, chord.mask));
        assert_eq!((back.sample_offset, &back.text), (12, &chord.text));

        let back = by_kind(|p| matches!(p, Payload::Scale(_)))
            .to_scale(staged.text)
            .expect("scale");
        assert_eq!(
            (back.root, back.mask, &back.text),
            (62, 0x5ab5, &scale.text)
        );

        let back = by_kind(|p| matches!(p, Payload::ExpressionText(_)))
            .to_expression_text(staged.text)
            .expect("text");
        assert_eq!((back.note_id, back.type_id), (note_id_for(2, 64), 7));
        assert_eq!(back.text, text.text);
        assert!(
            by_kind(|p| matches!(p, Payload::Chord(_)))
                .to_midi()
                .is_none()
        );
    });
}

/// A plugin-emitted legacy-MIDI-CC-out event decodes to the CC / pitch-bend
/// / poly-pressure MIDI message it stands for.
#[test]
fn legacy_cc_out_decodes_to_midi() {
    use vst3::Steinberg::Vst::ControllerNumbers_::{kAfterTouch, kPitchBend};

    // Plain CC 74 = 100 on channel 3.
    let cc = Event::at(5, Payload::LegacyCcOut(legacy_cc(74, 3, 100, 0)));
    let m = cc.to_midi().expect("CC decodes");
    assert_eq!(m.frame_offset, 5);
    match decode_ump(Ump(m.data)).expect("UMP") {
        Message::Midi2(address, Voice2::ControlChange { controller, value }) => {
            assert_eq!((address.channel.get(), controller.get()), (3, 74));
            let value = u32_to_unit_f32(value);
            assert!((value - 100.0 / 127.0).abs() < 0.01);
        }
        other => panic!("expected CC, got {other:?}"),
    }

    // Pitch bend: 14-bit from (value=LSB, value2=MSB). Center = 0,0x40.
    let pb = Event::at(
        0,
        Payload::LegacyCcOut(legacy_cc(kPitchBend as u8, 0, 0, 0x40)),
    );
    let pb_m = pb.to_midi().unwrap();
    match decode_ump(Ump(pb_m.data)).expect("UMP") {
        Message::Midi2(_, Voice2::PitchBend(bend)) => {
            assert!(bend_u32_to_signed_f32(bend).abs() < 0.01)
        }
        other => panic!("expected PitchBend, got {other:?}"),
    }

    // Aftertouch (channel pressure).
    let at = Event::at(
        0,
        Payload::LegacyCcOut(legacy_cc(kAfterTouch as u8, 1, 127, 0)),
    );
    let at_m = at.to_midi().unwrap();
    match decode_ump(Ump(at_m.data)).expect("UMP") {
        Message::Midi2(address, Voice2::ChannelPressure(_)) => {
            assert_eq!(address.channel.get(), 1)
        }
        other => panic!("expected ChannelPressure, got {other:?}"),
    }
}

/// Chord / scale / text / int events are not channel-voice MIDI.
#[test]
fn harmony_events_are_not_midi() {
    let chord = Event::at(
        0,
        Payload::Chord(Chord {
            root: 60,
            bass_note: 60,
            mask: 0,
            text: TextRef::default(),
        }),
    );
    assert!(chord.to_midi().is_none());
}

// ── reading through `tutti_ump::Message` (design 017 stage 2) ─────────────

mod message {
    use super::*;
    use tutti_ump::{
        Address, Attribute, Bank, Flex, NoteNumber, Sysex7Packet, SysexStatus, U7, U14, Utility,
        Voice1,
    };

    fn address(channel: u8) -> Address {
        Address {
            group: MidiGroup::FIRST,
            channel: MidiChannel::of(channel),
        }
    }

    fn note(n: u8) -> NoteNumber {
        NoteNumber::new(n).unwrap()
    }

    fn u7(v: u8) -> U7 {
        U7::new(v).unwrap()
    }

    fn packet(message: Message) -> MidiEvent {
        MidiEvent::from_ump(7, Ump::from(message).words())
    }

    fn data_bytes(event: &Event) -> [u8; 3] {
        let Payload::Data(d) = &event.payload else {
            panic!("expected a Data event, got {event:?}");
        };
        let b = d.payload();
        [b[0], b[1], b[2]]
    }

    /// A MIDI 1.0 Note On at velocity 0 widens to a Note Off at velocity
    /// 0x8000 (M2-104 v1.1.2 D.3.1), which VST3 carries as the release
    /// velocity `0x8000 / 0xFFFF`, about 0.5. It was a release velocity of 0.
    ///
    /// Mutation (run): `voice1` building the Note Off with velocity 0 instead
    /// of calling `widen` → 0.0 is not 0x8000 / 0xFFFF → fails.
    #[test]
    fn a_midi1_note_on_at_velocity_zero_is_a_note_off_at_one_half() {
        let event = packet(Message::Midi1(
            address(2),
            Voice1::NoteOn {
                note: note(60),
                velocity: u7(0),
            },
        ));
        let vst3 = encode(&event).expect("a note");
        let Payload::NoteOff(off) = vst3.payload else {
            panic!("expected a Note Off, got {vst3:?}");
        };
        assert_eq!((off.channel, off.pitch), (2, 60));
        assert_eq!(off.velocity, 32768.0 / 65535.0);
        assert!((off.velocity - 0.5).abs() < 1e-4);
    }

    /// A MIDI 2.0 Note On at velocity 0 stays a Note Off at release velocity
    /// 0, as it always has been here (M2-104 §7.4.2 keeps it a Note On; that is
    /// for the translation stage).
    ///
    /// Mutation (run): deleting the velocity-0 arm of `voice2` → a Note On at
    /// velocity 0.0 → fails.
    #[test]
    fn a_midi2_note_on_at_velocity_zero_is_a_note_off_at_zero() {
        let event = packet(Message::Midi2(
            address(2),
            Voice2::NoteOn {
                note: note(60),
                velocity: 0,
                attribute: Attribute::None,
            },
        ));
        let vst3 = encode(&event).expect("a note");
        let Payload::NoteOff(off) = vst3.payload else {
            panic!("expected a Note Off, got {vst3:?}");
        };
        assert_eq!((off.channel, off.pitch, off.velocity), (2, 60, 0.0));
    }

    /// A reserved attribute type is ignored (M2-104 §7.1.1) and the note still
    /// plays; a Pitch attribute is read but not applied, so the Note On's
    /// `tuning` stays 0.
    ///
    /// Mutation (run): `voice2` returning `None` when the attribute is
    /// `Attribute::Reserved` → no event for type 4 → fails.
    #[test]
    fn a_reserved_attribute_type_does_not_stop_the_note() {
        for attribute in [
            Attribute::Reserved {
                kind: 0x04,
                data: 0xABCD,
            },
            Attribute::Reserved {
                kind: 0xFF,
                data: 0,
            },
            Attribute::Pitch(0x3C00),
            Attribute::Manufacturer(0x1234),
        ] {
            let event = packet(Message::Midi2(
                address(1),
                Voice2::NoteOn {
                    note: note(60),
                    velocity: 0x8000,
                    attribute,
                },
            ));
            let vst3 = encode(&event).unwrap_or_else(|| panic!("{attribute:?} stopped the note"));
            let Payload::NoteOn(on) = vst3.payload else {
                panic!("expected a Note On, got {vst3:?}");
            };
            assert_eq!(
                (on.channel, on.pitch, on.tuning),
                (1, 60, 0.0),
                "{attribute:?}"
            );
            assert_eq!(on.velocity, 32768.0 / 65535.0);
        }
    }

    /// This host is a sink for what VST3 has no event for. The SDK's `Event`
    /// has no raw UMP event and its `Data` event has the one subtype
    /// `kMidiSysEx`, so an `Unknown` packet cannot be forwarded: it is `None`,
    /// not an error, and so is every kind with neither a VST3 event nor a
    /// MIDI 1.0 form.
    ///
    /// Mutation (run): the `Utility` arm of `message` returning a Data event
    /// → fails on the Noop.
    #[test]
    fn what_vst3_has_no_event_for_is_a_sink() {
        let sinks = [
            // Reserved message type 0x6, and a Channel Voice 2 status 0x7 that
            // is undefined: `decode` cannot frame them.
            MidiEvent::from_ump(0, &[0x6000_1234]),
            MidiEvent::from_ump(0, &[0x4070_0000, 0]),
            packet(Message::Utility(Utility::Noop)),
            packet(Message::Utility(Utility::JrClock(9))),
            // An Endpoint Discovery (stream) and a SysEx 8 packet (Data128).
            MidiEvent::from_ump(0, &[0xF000_0101, 0x1F, 0, 0]),
            MidiEvent::from_ump(0, &[0x5001_0000, 0, 0, 0]),
            packet(Message::Flex(MidiGroup::FIRST, Flex::SetTempo(50_000_000))),
            packet(Message::Midi2(
                address(0),
                Voice2::Registered {
                    bank: u7(1),
                    index: u7(2),
                    value: 5,
                },
            )),
            packet(Message::Midi2(
                address(0),
                Voice2::PerNoteManagement {
                    note: note(60),
                    detach: true,
                    reset: false,
                },
            )),
            // The start of a multi-packet SysEx stands for nothing alone.
            packet(Message::Sysex7(
                MidiGroup::FIRST,
                Sysex7Packet::new(SysexStatus::Start, &[1, 2, 3]).unwrap(),
            )),
        ];
        for event in sinks {
            assert!(encode(&event).is_none(), "{event:?} reached the plugin");
        }
    }

    /// Each System message is carried as its MIDI 1.0 bytes; Song Position
    /// is LSB first.
    ///
    /// Mutation (run): `system_bytes` taking `[lsb, msb]` the other way
    /// round for Song Position → fails.
    #[test]
    fn a_system_message_is_carried_as_its_midi1_bytes() {
        let cases = [
            (System::TimeCode(u7(0x35)), [0xF1, 0x35, 0]),
            (
                System::SongPosition(U14::new(0x1234).unwrap()),
                [0xF2, 0x34, 0x24],
            ),
            (System::SongSelect(u7(9)), [0xF3, 9, 0]),
            (System::TuneRequest, [0xF6, 0, 0]),
            (System::TimingClock, [0xF8, 0, 0]),
            (System::Start, [0xFA, 0, 0]),
            (System::Continue, [0xFB, 0, 0]),
            (System::Stop, [0xFC, 0, 0]),
            (System::ActiveSensing, [0xFE, 0, 0]),
            (System::Reset, [0xFF, 0, 0]),
        ];
        for (system, bytes) in cases {
            let event = packet(Message::System(MidiGroup::FIRST, system));
            let vst3 = encode(&event).unwrap_or_else(|| panic!("{system:?} dropped"));
            assert_eq!(data_bytes(&vst3), bytes, "{system:?}");
        }
    }

    /// A MIDI 1.0 controller, program, pressure and bend are carried as they
    /// came; a MIDI 2.0 one is narrowed to the top bits (M2-115 §3.4).
    ///
    /// Mutation (run): `narrow_bend(bend)` replaced by `narrow_bend(bend >> 1)` in `voice2` →
    /// the centre bend is 0x4000 → fails.
    #[test]
    fn controllers_are_carried_at_midi1_resolution() {
        let carried = |message| data_bytes(&encode(&packet(message)).expect("carried"));
        let ch = address(3);
        assert_eq!(
            carried(Message::Midi1(
                ch,
                Voice1::ControlChange {
                    controller: CCNumber::new(74).unwrap(),
                    value: u7(100),
                }
            )),
            [0xB3, 74, 100]
        );
        assert_eq!(
            carried(Message::Midi1(
                ch,
                Voice1::PitchBend(U14::new(0x2001).unwrap())
            )),
            [0xE3, 0x01, 0x40]
        );
        assert_eq!(
            carried(Message::Midi2(
                ch,
                Voice2::ControlChange {
                    controller: CCNumber::new(74).unwrap(),
                    value: 0xFFFF_FFFF,
                }
            )),
            [0xB3, 74, 127]
        );
        assert_eq!(
            carried(Message::Midi2(ch, Voice2::PitchBend(0x8000_0000))),
            [0xE3, 0x00, 0x40]
        );
        assert_eq!(
            carried(Message::Midi2(ch, Voice2::ChannelPressure(0x8000_0000))),
            [0xD3, 0x40, 0]
        );
        assert_eq!(
            carried(Message::Midi2(
                ch,
                Voice2::ProgramChange {
                    program: u7(9),
                    bank: None
                }
            )),
            [0xC3, 9, 0]
        );
    }

    /// A banked Program Change is Bank Select, Bank Select LSB and the
    /// Program Change. The bank is read the way `MidiEvent::program_change`
    /// writes it (see `bank_program`), so bank `0x0085` is CC 0 = 1 and CC 32
    /// = 5, and a Program Change with no bank is not a bank.
    ///
    /// Mutation (run): swapping `bank.lsb` and `bank.msb` in `bank_program`
    /// → CC 0 is 5 → fails.
    #[test]
    fn a_bank_is_read_the_way_the_constructor_writes_it() {
        let banked =
            MidiEvent::program_change(MidiGroup::FIRST, MidiChannel::of(1), 9, Some(0x0085));
        assert_eq!(
            midi::bank_program(&banked),
            Some([[0xB1, 0, 1], [0xB1, 32, 5], [0xC1, 9, 0]])
        );
        // Built from spec words (MSB 2 at bits 15-8, LSB 5 at bits 7-0): this
        // host sends the packet's LSB as CC 0 and its MSB as CC 32, so the
        // halves arrive swapped until the constructor writes the spec layout.
        let spec_words = MidiEvent::from_ump(0, &[0x41C1_0001, 0x0900_0205]);
        assert_eq!(
            midi::bank_program(&spec_words),
            Some([[0xB1, 0, 5], [0xB1, 32, 2], [0xC1, 9, 0]])
        );
        let plain = MidiEvent::program_change(MidiGroup::FIRST, MidiChannel::of(1), 9, None);
        assert_eq!(midi::bank_program(&plain), None);
    }

    /// A MIDI 1.0 message a plugin sends as raw bytes is widened on the way
    /// in: a Note On at velocity 0 is a Note Off at 0x8000, and a system
    /// message is forwarded unchanged.
    ///
    /// Mutation (run): `widen_midi1` returning `event` for `Message::Midi1`
    /// → the packet stays Channel Voice 1 → fails.
    #[test]
    fn raw_midi1_from_a_plugin_is_widened() {
        let on = Event::at(4, Payload::Data(Data::inline(0, [0x92, 60, 0])));
        let back = on.to_midi().expect("decodes");
        assert_eq!(back.frame_offset, 4);
        assert_eq!(
            decode_ump(Ump(back.data)),
            Ok(Message::Midi2(
                address(2),
                Voice2::NoteOff {
                    note: note(60),
                    velocity: 0x8000,
                    attribute: Attribute::None,
                }
            ))
        );
        let clock = Event::at(0, Payload::Data(Data::inline(0, [0xF8, 0, 0]))).to_midi();
        assert_eq!(
            clock.map(|m| decode_ump(Ump(m.data))),
            Some(Ok(Message::System(MidiGroup::FIRST, System::TimingClock)))
        );
    }

    /// **Every kind of message is read without allocating.** Each packet kind
    /// goes through the encoder, the bank split, the output decode and the
    /// mapping inside `assert_no_alloc`. The VST3 host has no worker pool: the
    /// calling thread is the only participant.
    ///
    /// Mutation (run): a `Box::new(message)` in `MidiEncoder::message` →
    /// `assert_no_alloc` aborts.
    #[test]
    fn every_kind_of_message_is_read_without_allocating() {
        let ch = address(4);
        let messages = [
            Message::Utility(Utility::Noop),
            Message::System(MidiGroup::FIRST, System::SongPosition(U14::MAX)),
            Message::Midi1(
                ch,
                Voice1::NoteOn {
                    note: note(60),
                    velocity: u7(0),
                },
            ),
            Message::Midi1(
                ch,
                Voice1::PolyPressure {
                    note: note(60),
                    pressure: u7(9),
                },
            ),
            Message::Midi1(ch, Voice1::PitchBend(U14::MAX)),
            Message::Sysex7(
                MidiGroup::FIRST,
                Sysex7Packet::new(SysexStatus::Complete, &[1, 2, 3]).unwrap(),
            ),
            Message::Midi2(
                ch,
                Voice2::NoteOn {
                    note: note(60),
                    velocity: 0xFFFF,
                    attribute: Attribute::Reserved { kind: 9, data: 1 },
                },
            ),
            Message::Midi2(
                ch,
                Voice2::PerNotePitchBend {
                    note: note(60),
                    value: 0x9000_0000,
                },
            ),
            Message::Midi2(
                ch,
                Voice2::AssignablePerNote {
                    note: note(60),
                    controller: 74,
                    value: 7,
                },
            ),
            Message::Midi2(
                ch,
                Voice2::ProgramChange {
                    program: u7(1),
                    bank: Some(Bank {
                        msb: u7(1),
                        lsb: u7(2),
                    }),
                },
            ),
            Message::Midi2(
                ch,
                Voice2::ControlChange {
                    controller: CCNumber::new(1).unwrap(),
                    value: 1,
                },
            ),
            Message::Midi2(
                ch,
                Voice2::Registered {
                    bank: u7(0),
                    index: u7(7),
                    value: 0x1000_0000,
                },
            ),
        ];
        let mut events: Vec<MidiEvent> = messages.iter().map(|m| packet(*m)).collect();
        events.push(MidiEvent::from_ump(0, &[0x6000_0000]));
        let raw = Event::at(1, Payload::Data(Data::inline(0, [0x90, 60, 0])));
        let legacy = Event::at(2, Payload::LegacyCcOut(legacy_cc(74, 3, 100, 0)));
        let mut encoder = MidiEncoder::default();
        assert_no_alloc::assert_no_alloc(|| {
            for event in &events {
                let _ = encoder.encode(event);
                let _ = midi::bank_program(event);
            }
            let _ = raw.to_midi();
            let _ = legacy.to_midi();
        });
    }

    /// A chord at one frame stages as three Note Ons, in order, each at the
    /// velocity it was sent at.
    ///
    /// Mutation (run): `Stager::stage` staging only the first MIDI event of the block →
    /// two events → fails.
    #[test]
    fn a_chord_stages_as_three_note_ons_in_order() {
        let events: Vec<MidiEvent> = [60u8, 64, 67]
            .into_iter()
            .map(|n| {
                packet(Message::Midi1(
                    address(0),
                    Voice1::NoteOn {
                        note: note(n),
                        velocity: u7(100),
                    },
                ))
            })
            .collect();
        let list = EventList::input();
        let mut stager = Stager::default();
        stager.stage(
            &list,
            &InputEvents {
                midi: &events,
                ..Default::default()
            },
        );
        let seen: Vec<(i16, f32, i32)> = list.with_events(|staged| {
            staged
                .events
                .iter()
                .map(|e| match e.payload {
                    Payload::NoteOn(on) => (on.pitch, on.velocity, e.sample_offset()),
                    other => panic!("expected Note Ons, got {other:?}"),
                })
                .collect()
        });
        // M2-115 §3.3: 100 widens to 100 << 9 plus its low six bits repeated,
        // 0b110100 << 3 | 0b110100 >> 3 | 0b110100 >> 9.. = 51492.
        let velocity = u16_to_unit_f32(51492);
        assert_eq!(
            seen,
            vec![(60, velocity, 7), (64, velocity, 7), (67, velocity, 7)]
        );
    }
}
