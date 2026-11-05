use super::*;
use tutti_midi_types::convert::{bend_u32_to_signed_f32, u16_to_unit_f32, u32_to_unit_f32};
use tutti_midi_types::midi2::channel_voice2::ChannelVoice2 as Cv2;
use tutti_midi_types::midi2::{Channeled, UmpMessage};
use tutti_midi_types::{CCNumber, MidiChannel, MidiGroup};
use tutti_plugin_types::{NoteExpressionType, NoteExpressionValue, note_id_for};
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
    // (mirrors the hardware input path's `normalize` promotion).
    use tutti_midi_types::convert::midi1_cc_to_midi2;

    let event = MidiEvent::cc(
        MidiGroup::FIRST,
        MidiChannel::new(1),
        CCNumber::BRIGHTNESS,
        midi1_cc_to_midi2(100),
    );
    let vst3 = encode(&event).expect("CC -> Data");
    assert!(
        matches!(vst3.payload, Payload::Data(_)),
        "CC should be a Data event"
    );
    let back = vst3.to_midi().expect("cc decodes");
    match UmpMessage::try_from(back.data_words()).expect("valid UMP") {
        UmpMessage::ChannelVoice2(Cv2::ControlChange(m)) => {
            assert_eq!(u8::from(m.channel()), 1);
            assert_eq!(u8::from(m.control()), 74);
        }
        other => panic!("expected CV2 ControlChange, got {other:?}"),
    }
}

#[test]
fn inbound_system_message_passes_through() {
    // A System real-time message (Timing Clock 0xF8) has no CV form and must
    // pass through `normalize` unchanged, not be dropped or promoted.
    let event = MidiEvent::timing_clock(MidiGroup::FIRST);
    let vst3 = encode(&event).expect("clock -> Data");
    let back = vst3.to_midi().expect("clock decodes");
    assert!(
        matches!(
            UmpMessage::try_from(back.data_words()),
            Ok(UmpMessage::SystemCommon(_))
        ),
        "timing clock should stay a System Real-Time message"
    );
}

#[test]
fn note_on_lands_in_note_on_variant() {
    let event =
        MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::new(3), 60, 0x8000).with_frame_offset(5);
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
        MidiChannel::new(1),
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
        MidiChannel::new(2),
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
    let event = MidiEvent::program_change(MidiGroup::FIRST, MidiChannel::new(9), 42, None);
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
        MidiChannel::new(5),
        67,
        0x8000,
    ))
    .unwrap();
    let off = encode(&MidiEvent::note_off(
        MidiGroup::FIRST,
        MidiChannel::new(5),
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
        MidiChannel::new(5),
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
            MidiChannel::new(channel),
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
        MidiChannel::new(3),
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
    match UmpMessage::try_from(back.data_words()).expect("decodes") {
        UmpMessage::ChannelVoice2(Cv2::NoteOn(m)) => {
            let velocity = u16_to_unit_f32(m.velocity());
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
    match UmpMessage::try_from(m.data_words()).expect("UMP") {
        UmpMessage::ChannelVoice2(Cv2::ControlChange(msg)) => {
            assert_eq!((u8::from(msg.channel()), u8::from(msg.control())), (3, 74));
            let value = u32_to_unit_f32(msg.control_change_data());
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
    match UmpMessage::try_from(pb_m.data_words()).expect("UMP") {
        UmpMessage::ChannelVoice2(Cv2::ChannelPitchBend(msg)) => {
            assert!(bend_u32_to_signed_f32(msg.pitch_bend_data()).abs() < 0.01)
        }
        other => panic!("expected PitchBend, got {other:?}"),
    }

    // Aftertouch (channel pressure).
    let at = Event::at(
        0,
        Payload::LegacyCcOut(legacy_cc(kAfterTouch as u8, 1, 127, 0)),
    );
    let at_m = at.to_midi().unwrap();
    match UmpMessage::try_from(at_m.data_words()).expect("UMP") {
        UmpMessage::ChannelVoice2(Cv2::ChannelPressure(msg)) => {
            assert_eq!(u8::from(msg.channel()), 1)
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
