use super::*;

fn utf16(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

/// Encodes `event` and decodes it again, as a plugin's `getEvent` and `addEvent` would.
fn through_c(event: &Event, arena: &[u16], out: &mut TextArena) -> Event {
    let c = event.to_c(arena, &[]);
    // SAFETY: `c` was just built by `to_c` from the arena it is decoded with.
    unsafe { Event::from_c(&c, out) }.expect("decodes")
}

/// `Event` stays `Copy` with the text-bearing variants: they hold a `TextRef`
/// index, not an owned string.
#[test]
fn event_is_copy() {
    fn assert_copy<T: Copy>() {}
    assert_copy::<Event>();
}

/// Every tag the SDK defines decodes to its kind, and the payload's kind is the
/// tag written to the C struct (the header cannot contradict its variant).
///
/// Mutation (run): `to_c` writing `EventKind::NoteOn` for every payload → the
/// other kinds fail.
#[test]
fn the_wire_tag_is_derived_from_the_payload() {
    let mut arena = TextArena::new();
    let payloads = [
        Payload::NoteOn(NoteOn::new(1, 60, 0.5, 7)),
        Payload::NoteOff(NoteOff::new(1, 60, 0.5, 7)),
        Payload::Data(Data::new(0, &[0x90, 60, 100]).unwrap()),
        Payload::PolyPressure(PolyPressure::new(1, 60, 0.5, 7)),
        Payload::ExpressionValue(ExpressionValue::new(2, 7, 0.5)),
        Payload::ExpressionText(ExpressionText {
            type_id: 0,
            note_id: 7,
            text: arena.intern(&utf16("x")),
        }),
        Payload::Chord(Chord {
            root: 60,
            bass_note: 60,
            mask: 0,
            text: TextRef::default(),
        }),
        Payload::Scale(Scale {
            root: 60,
            mask: 0,
            text: TextRef::default(),
        }),
        Payload::ExpressionInt(ExpressionInt::new(3, 7, 1)),
        Payload::LegacyCcOut(LegacyCcOut(vst3::Steinberg::Vst::LegacyMIDICCOutEvent {
            controlNumber: 7,
            channel: 0,
            value: 0,
            value2: 0,
        })),
    ];
    for payload in payloads {
        let event = Event::at(3, payload);
        let c = event.to_c(&arena, &[]);
        assert_eq!(EventKind::try_from(c.r#type), Ok(payload.kind()));
        assert_eq!(c.sampleOffset, 3);
    }
    let sysex = Event::at(0, Payload::SysEx(SysExRef::new(0, 4)));
    let c = sysex.to_c(&[], &[0xF0, 1, 2, 0xF7]);
    assert_eq!(EventKind::try_from(c.r#type), Ok(EventKind::Data));
}

/// Mutation (run): `TryFrom<u16>` answering `Ok(Data)` for an unknown tag → fails.
#[test]
fn an_undefined_tag_is_refused() {
    assert_eq!(EventKind::try_from(0x1234), Err(UnknownEventKind(0x1234)));
    let mut c: vst3::Steinberg::Vst::Event = crate::sdk::zeroed();
    c.r#type = 0x1234;
    // SAFETY: an unknown tag is refused before any union member is read.
    assert!(unsafe { Event::from_c(&c, &mut TextArena::new()) }.is_none());
}

/// A chord with text: intern into an arena, encode to the C struct (which
/// points into the arena), decode back into a second arena and read it out.
#[test]
fn chord_round_trips_through_c_event_with_text() {
    let name = utf16("Cmaj7");
    let mut in_arena = TextArena::new();
    let chord = Chord {
        root: 60,
        bass_note: 48,
        mask: 0b100_1001_0001,
        text: in_arena.intern(&name),
    };
    let event = Event::at(12, Payload::Chord(chord));

    let c = event.to_c(&in_arena, &[]);
    // SAFETY: the test wrote this union member itself, with the tag that names it.
    unsafe {
        assert_eq!(c.__field0.chord.root, 60);
        assert_eq!(c.__field0.chord.textLen as usize, name.len());
        assert!(!c.__field0.chord.text.is_null());
    }

    let mut out = TextArena::new();
    let back = through_c(&event, &in_arena, &mut out);
    let Payload::Chord(back_chord) = back.payload else {
        panic!("not a chord: {back:?}");
    };
    assert_eq!((back_chord.root, back_chord.bass_note), (60, 48));
    assert_eq!(back_chord.mask, chord.mask);
    assert_eq!(back_chord.text.slice(&out), Some(name.as_slice()));
    assert_eq!(back.sample_offset(), 12);
}

#[test]
fn scale_and_text_round_trip_through_c_event() {
    let scale_name = utf16("D Dorian");
    let text = utf16("staccato");
    let mut in_arena = TextArena::new();
    let scale = Event::at(
        0,
        Payload::Scale(Scale {
            root: 62,
            mask: 0x5ab5,
            text: in_arena.intern(&scale_name),
        }),
    );
    let annotation = Event::at(
        4,
        Payload::ExpressionText(ExpressionText {
            type_id: 7,
            note_id: 130,
            text: in_arena.intern(&text),
        }),
    );

    let mut out = TextArena::new();
    let Payload::Scale(s) = through_c(&scale, &in_arena, &mut out).payload else {
        panic!("not a scale");
    };
    assert_eq!((s.root, s.mask), (62, 0x5ab5));
    assert_eq!(s.text.slice(&out), Some(scale_name.as_slice()));

    let Payload::ExpressionText(t) = through_c(&annotation, &in_arena, &mut out).payload else {
        panic!("not a text");
    };
    assert_eq!((t.note_id, t.type_id), (130, 7));
    assert_eq!(t.text.slice(&out), Some(text.as_slice()));
}

#[test]
fn note_expression_int_round_trips() {
    let event = Event::at(
        9,
        Payload::ExpressionInt(ExpressionInt::new(3, 99, u64::MAX - 1233)),
    );
    let back = through_c(&event, &[], &mut TextArena::new());
    let Payload::ExpressionInt(e) = back.payload else {
        panic!("not an int expression");
    };
    assert_eq!((e.typeId, e.noteId, e.value), (3, 99, u64::MAX - 1233));
    assert_eq!(back.sample_offset(), 9);
}

/// Data past the inline room is cut to it, and a null or empty payload decodes to zeros.
///
/// Mutation (run): `from_c` copying `min(size, 8)` bytes instead of `min(size, 16)`
/// → the payload is cut → fails.
#[test]
fn data_event_with_a_bad_size_decodes_within_its_inline_bytes() {
    let bytes = [7u8; 40];
    let mut c: vst3::Steinberg::Vst::Event = crate::sdk::zeroed();
    c.r#type = EventKind::Data as u16;
    c.__field0.data = vst3::Steinberg::Vst::DataEvent {
        size: 40,
        r#type: 0,
        bytes: bytes.as_ptr(),
    };
    // SAFETY: tagged `Data`; `bytes` is readable for the 40 bytes it states.
    let Payload::Data(d) = unsafe { Event::from_c(&c, &mut TextArena::new()) }
        .unwrap()
        .payload
    else {
        panic!("not data");
    };
    assert_eq!(d.payload(), &[7u8; 16]);

    c.__field0.data.bytes = std::ptr::null();
    // SAFETY: tagged `Data`; a null `bytes` is never read.
    let Payload::Data(d) = unsafe { Event::from_c(&c, &mut TextArena::new()) }
        .unwrap()
        .payload
    else {
        panic!("not data");
    };
    assert_eq!(d.payload(), &[0u8; 16]);
}

/// Mutation (run): `to_c` answering the arena's pointer for an out-of-range ref
/// → the pointer is non-null → fails.
#[test]
fn a_text_ref_outside_the_arena_is_a_null_pointer() {
    let event = Event::at(
        0,
        Payload::Chord(Chord {
            root: 60,
            bass_note: 60,
            mask: 0,
            text: TextRef { start: 5, len: 3 },
        }),
    );
    let c = event.to_c(&[], &[]);
    // SAFETY: the test wrote this union member itself, with the tag that names it.
    unsafe {
        assert!(c.__field0.chord.text.is_null());
        assert_eq!(c.__field0.chord.textLen, 0);
    }
}

/// Each kind's tag is the SDK's own ordinal, asserted constant by constant: a
/// table that paired two names with each other's ordinals would still round-trip.
///
/// Mutation (run): swapping the `NoteOn` and `NoteOff` ordinals in `event_kinds!`
/// → both assertions fail.
#[test]
fn each_kind_is_the_sdk_ordinal() {
    use vst3::Steinberg::Vst::Event_::EventTypes_ as T;
    let tag = |sdk| u16::try_from(sdk).expect("an SDK tag fits u16");
    assert_eq!(EventKind::NoteOn as u16, tag(T::kNoteOnEvent));
    assert_eq!(EventKind::NoteOff as u16, tag(T::kNoteOffEvent));
    assert_eq!(EventKind::Data as u16, tag(T::kDataEvent));
    assert_eq!(EventKind::PolyPressure as u16, tag(T::kPolyPressureEvent));
    assert_eq!(
        EventKind::NoteExpressionValue as u16,
        tag(T::kNoteExpressionValueEvent)
    );
    assert_eq!(
        EventKind::NoteExpressionText as u16,
        tag(T::kNoteExpressionTextEvent)
    );
    assert_eq!(EventKind::Chord as u16, tag(T::kChordEvent));
    assert_eq!(EventKind::Scale as u16, tag(T::kScaleEvent));
    assert_eq!(
        EventKind::NoteExpressionIntValue as u16,
        tag(T::kNoteExpressionIntValueEvent)
    );
    assert_eq!(
        EventKind::LegacyMidiCcOut as u16,
        tag(T::kLegacyMIDICCOutEvent)
    );
}
