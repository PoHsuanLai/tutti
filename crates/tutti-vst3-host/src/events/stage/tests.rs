use super::*;
use tutti_graph::UmpAt;
use tutti_ump::{Message, MidiChannel, MidiGroup, SysexStatus};

use tutti_fixtures::midi;
use tutti_graph::Offset;
use tutti_types::Samples;
use tutti_vst3_ffi::EVENT_CAPACITY;

/// A block longer than any offset these tests place an event at.
const TEST_BLOCK: usize = 1 << 16;

/// The offset `k` frames into a test block.
fn at(k: u32) -> Offset {
    Offset::new(usize::try_from(k).unwrap(), Samples(TEST_BLOCK)).unwrap()
}

fn midi_only(midi: &[UmpAt]) -> InputEvents<'_> {
    InputEvents {
        midi,
        ..Default::default()
    }
}

fn len(list: &EventList) -> usize {
    list.with_events(|staged| staged.events.len())
}

/// A per-note bend sensitivity staged in one block sets the range a
/// later block's per-note bends are read at: the list keeps it.
#[test]
fn a_per_note_bend_range_outlives_its_block() {
    use tutti_midi_protocol::PitchBendSensitivity;
    use tutti_midi_protocol::signed_f32_to_bend_u32;

    let list = EventList::input();
    let mut stager = Stager::default();
    // A note sounds on the channel: a bend with no note is held, not sent.
    let sensitivity = [
        (
            Offset::ZERO,
            midi::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, midi::MID_VELOCITY),
        ),
        (
            Offset::ZERO,
            midi::registered_rpn(
                MidiGroup::FIRST,
                MidiChannel::FIRST,
                tutti_ump::Rpn::PER_NOTE_PITCH_BEND_SENSITIVITY,
                PitchBendSensitivity::of(24).to_rpn_bits(),
            ),
        ),
    ];
    assert!(stager.stage(
        &list,
        &InputEvents {
            midi: &sensitivity,
            ..Default::default()
        },
    ));
    assert_eq!(len(&list), 1, "the Note On stages, the sensitivity nothing");

    let bend = [(
        Offset::ZERO,
        midi::per_note_pitch_bend(
            MidiGroup::FIRST,
            MidiChannel::FIRST,
            60,
            signed_f32_to_bend_u32(0.25),
        ),
    )];
    assert!(stager.stage(
        &list,
        &InputEvents {
            midi: &bend,
            ..Default::default()
        },
    ));
    list.with_events(|staged| match staged.events {
        [
            Event {
                payload: Payload::ExpressionValue(e),
                ..
            },
        ] => {
            assert!((e.value - (0.5 + 6.0 / 240.0)).abs() < 1e-6, "{}", e.value);
        }
        other => panic!("expected one tuning expression, got {other:?}"),
    });
}

/// **VST3 and CLAP read one stream's per-note bends at the same range.**
/// The same two blocks go to both formats' event lists: a per-note bend
/// sensitivity of 24 on channel 1, then a quarter bend there and on
/// channel 2, and a block later another quarter bend on channel 1. Both
/// tell their plugin 6 semitones on channel 1 (in either block) and 12
/// on channel 2 (the ±48 default). VST3 carries semitones as
/// `0.5 + st / 240`, CLAP as semitones.
#[test]
fn vst3_and_clap_bend_one_stream_by_the_same_semitones() {
    use tutti_clap_host::{ClapEvent, EventList as _, InputEventList};
    use tutti_midi_protocol::PitchBendSensitivity;
    use tutti_midi_protocol::signed_f32_to_bend_u32;

    let bend = |channel: u8| {
        (
            Offset::ZERO,
            midi::per_note_pitch_bend(
                MidiGroup::FIRST,
                MidiChannel::of(channel),
                60,
                signed_f32_to_bend_u32(0.25),
            ),
        )
    };
    let first = [
        (
            Offset::ZERO,
            midi::registered_rpn(
                MidiGroup::FIRST,
                MidiChannel::FIRST,
                tutti_ump::Rpn::PER_NOTE_PITCH_BEND_SENSITIVITY,
                PitchBendSensitivity::of(24).to_rpn_bits(),
            ),
        ),
        bend(0),
        bend(1),
    ];
    let second = [bend(0)];

    let vst3 = EventList::input();
    let mut stager = Stager::default();
    let mut clap = InputEventList::new();
    // A note sounds on each channel: a bend with no note is held, not sent.
    let notes: Vec<UmpAt> = (0..2)
        .map(|channel| {
            (
                Offset::ZERO,
                midi::note_on(
                    MidiGroup::FIRST,
                    MidiChannel::of(channel),
                    60,
                    midi::MID_VELOCITY,
                ),
            )
        })
        .collect();
    assert!(stager.stage(&vst3, &midi_only(&notes)));
    clap.add_midi_events(&notes);
    for (block, want) in [(&first[..], &[6.0, 12.0][..]), (&second[..], &[6.0][..])] {
        assert!(stager.stage(&vst3, &midi_only(block)));
        clap.clear();
        clap.add_midi_events(block);

        let vst3_st: Vec<f64> = vst3.with_events(|staged| {
            staged
                .events
                .iter()
                .map(|e| match &e.payload {
                    Payload::ExpressionValue(e) => (e.value - 0.5) * 240.0,
                    other => panic!("VST3: expected a tuning, got {other:?}"),
                })
                .collect()
        });
        let clap_st: Vec<f64> = clap
            .events()
            .iter()
            .map(|e| match e {
                ClapEvent::NoteExpression(e) => e.value,
                _ => panic!("CLAP: expected a tuning"),
            })
            .collect();
        assert_eq!(vst3_st.len(), want.len());
        assert_eq!(clap_st.len(), want.len());
        for ((v, c), w) in vst3_st.iter().zip(&clap_st).zip(want) {
            assert!((v - w).abs() < 1e-4, "VST3 {v} st, want {w}");
            assert!((c - w).abs() < 1e-4, "CLAP {c} st, want {w}");
        }
    }
}

#[test]
fn one_midi_event_stages_one_event() {
    let list = EventList::input();
    let mut stager = Stager::default();
    let midi_events = [(
        at(0),
        midi::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, midi::MID_VELOCITY),
    )];
    assert!(stager.stage(&list, &midi_only(&midi_events)));
    assert_eq!(len(&list), 1);
}

#[test]
fn clear_empties_a_staged_block() {
    let list = EventList::input();
    let mut stager = Stager::default();
    let midi_events = [
        (
            at(0),
            midi::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, midi::MID_VELOCITY),
        ),
        (
            at(10),
            midi::note_off(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0),
        ),
    ];
    assert!(stager.stage(&list, &midi_only(&midi_events)));
    assert!(list.clear());
    assert_eq!(len(&list), 0);
}

/// RT regression: staging MIDI and `clear` run every block;
/// they must reuse the pre-reserved Vec capacity without heap grow.
#[test]
fn midi_staging_is_allocation_free_after_warmup() {
    let list = EventList::input();
    let mut stager = Stager::default();
    let events = [
        (
            at(0),
            midi::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, midi::MID_VELOCITY),
        ),
        (
            at(32),
            midi::note_off(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0),
        ),
        (
            at(64),
            midi::note_on(MidiGroup::FIRST, MidiChannel::of(1), 64, 0x6000),
        ),
        (
            at(96),
            midi::note_off(MidiGroup::FIRST, MidiChannel::of(1), 64, 0),
        ),
    ];

    // Warm up — the first call may grow (the default Vec cap is 256,
    // so this shouldn't, but prime anyway).
    assert!(stager.stage(&list, &midi_only(&events)));
    assert!(list.clear());

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..10_000 {
            assert!(stager.stage(&list, &midi_only(&events)));
            assert!(list.clear());
        }
    });
}

/// RT regression for the full-source path: staging MIDI + every other event
/// source (including arena-interned chord/scale/text) must stay
/// allocation-free once the events Vec and the text arena have warmed up.
#[test]
fn staging_every_source_is_allocation_free_after_warmup() {
    let list = EventList::input();
    let mut stager = Stager::default();
    let midi = [
        (
            at(0),
            midi::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, midi::MID_VELOCITY),
        ),
        (
            at(64),
            midi::note_off(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0),
        ),
    ];
    let note_expr = [NoteExpressionValue {
        sample_offset: 0,
        note_id: 0,
        expression_type: tutti_plugin_types::NoteExpressionType::Tuning,
        value: 0.5,
    }];
    let cmaj: Vec<u16> = "Cmaj7".encode_utf16().collect();
    let chords = [Utf16Chord {
        sample_offset: 0,
        root: 60,
        bass_note: 48,
        mask: 0,
        text: cmaj.clone(),
    }];
    let dorian: Vec<u16> = "D Dorian".encode_utf16().collect();
    let scales = [Utf16Scale {
        sample_offset: 0,
        root: 62,
        mask: 0,
        text: dorian.clone(),
    }];
    let lyric: Vec<u16> = "la".encode_utf16().collect();
    let texts = [Utf16Text {
        sample_offset: 0,
        note_id: 0,
        type_id: 0,
        text: lyric.clone(),
    }];
    let ints = [NoteExpressionIntValue {
        sample_offset: 0,
        note_id: 0,
        type_id: 0,
        value: 1,
    }];

    let src = InputEvents {
        midi: &midi,
        note_expressions: &note_expr,
        chords: &chords,
        scales: &scales,
        expr_texts: &texts,
        expr_ints: &ints,
    };

    // Warm up every buffer (events Vec / text arena).
    assert!(stager.stage(&list, &src));
    assert!(list.clear());

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..10_000 {
            assert!(stager.stage(&list, &src));
            assert!(list.clear());
        }
    });
}

/// The kind and identity of a staged event, to read an order back: a
/// MIDI note by its pitch, a per-note expression by its note id, a
/// chord by its root, a scale by its root.
fn tag(e: &Event) -> (&'static str, i32) {
    match &e.payload {
        Payload::NoteOn(n) => ("on", i32::from(n.pitch)),
        Payload::NoteOff(n) => ("off", i32::from(n.pitch)),
        Payload::ExpressionValue(x) => ("expr", x.noteId),
        Payload::ExpressionInt(x) => ("int", x.noteId),
        Payload::Chord(c) => ("chord", i32::from(c.root)),
        Payload::Scale(c) => ("scale", i32::from(c.root)),
        Payload::ExpressionText(x) => ("text", x.note_id),
        other => panic!("unexpected event {other:?}"),
    }
}

fn staged(list: &EventList) -> Vec<(i32, (&'static str, i32))> {
    list.with_events(|staged| {
        staged
            .events
            .iter()
            .map(|e| (e.sample_offset(), tag(e)))
            .collect()
    })
}

fn expr(offset: i32, note_id: i32) -> NoteExpressionValue {
    NoteExpressionValue {
        sample_offset: offset,
        note_id,
        expression_type: tutti_plugin_types::NoteExpressionType::Tuning,
        value: 0.5,
    }
}

fn chord(offset: i32, root: i16) -> Utf16Chord {
    Utf16Chord {
        sample_offset: offset,
        root,
        bass_note: 0,
        mask: 0,
        text: "C".encode_utf16().collect(),
    }
}

/// **A full block from every source stages without allocating**:
/// `EVENT_CAPACITY` events (256 MIDI on even frames, 256 per-note
/// expressions on odd ones, and four of each harmony kind on frames
/// running backwards), every source in its own order and the sources
/// interleaved, so the sort moves events across the whole list. Std's
/// stable sort allocates past 73 of these.
#[test]
fn a_full_block_from_every_source_stages_without_allocating() {
    use tutti_plugin_types::{HARMONY_CAPACITY, MIDI_STACK_CAPACITY};
    let midi: Vec<UmpAt> = (0..MIDI_STACK_CAPACITY)
        .map(|i| {
            (
                at(2 * i as u32),
                midi::note_on(
                    MidiGroup::FIRST,
                    MidiChannel::FIRST,
                    (i % 128) as u8,
                    midi::MID_VELOCITY,
                ),
            )
        })
        .collect();
    let note_expressions: Vec<NoteExpressionValue> = (0..MIDI_STACK_CAPACITY as i32)
        .map(|i| expr(2 * i + 1, i))
        .collect();
    let backwards = |i: usize| (HARMONY_CAPACITY - i) as i32 * 100;
    let chords: Vec<Utf16Chord> = (0..HARMONY_CAPACITY)
        .map(|i| chord(backwards(i), i as i16))
        .collect();
    let scales: Vec<Utf16Scale> = (0..HARMONY_CAPACITY)
        .map(|i| Utf16Scale {
            sample_offset: backwards(i),
            root: i as i16,
            mask: 0,
            text: "D".encode_utf16().collect(),
        })
        .collect();
    let expr_texts: Vec<Utf16Text> = (0..HARMONY_CAPACITY)
        .map(|i| Utf16Text {
            sample_offset: backwards(i),
            note_id: i as i32,
            type_id: 0,
            text: "la".encode_utf16().collect(),
        })
        .collect();
    let expr_ints: Vec<NoteExpressionIntValue> = (0..HARMONY_CAPACITY)
        .map(|i| NoteExpressionIntValue {
            sample_offset: backwards(i),
            note_id: i as i32,
            type_id: 0,
            value: 1,
        })
        .collect();
    let src = InputEvents {
        midi: &midi,
        note_expressions: &note_expressions,
        chords: &chords,
        scales: &scales,
        expr_texts: &expr_texts,
        expr_ints: &expr_ints,
    };

    let list = EventList::input();
    let mut stager = Stager::default();
    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..4 {
            assert!(stager.stage(&list, &src));
        }
    });
    assert_eq!(len(&list), EVENT_CAPACITY, "every event fits the room");
    assert_eq!(list.dropped(), 0);
    let offsets: Vec<i32> = staged(&list).iter().map(|(o, _)| *o).collect();
    assert!(offsets.is_sorted(), "in frame order: {offsets:?}");

    // Twice the room: the rest is dropped, counted, never grown into.
    let twice = InputEvents {
        midi: &[midi.as_slice(), midi.as_slice()].concat(),
        note_expressions: &[note_expressions.as_slice(), note_expressions.as_slice()].concat(),
        ..src
    };
    assert_no_alloc::assert_no_alloc(|| stager.stage(&list, &twice));
    assert_eq!(len(&list), EVENT_CAPACITY);
    let offered = 2 * (MIDI_STACK_CAPACITY + MIDI_STACK_CAPACITY) + 4 * HARMONY_CAPACITY;
    assert_eq!(list.dropped(), (offered - EVENT_CAPACITY) as u64);
}

/// **Events on one frame keep their source order**: the sources' order
/// (MIDI, then per-note expressions, integer expressions, chords), and
/// each source's own (a note-off before the note-on that reuses its key).
/// 330 events on ten frames, the expressions arriving on frames running
/// backwards, so the sort has many equal keys to move past each other.
#[test]
fn events_on_one_frame_keep_their_source_order() {
    // 12 MIDI events a frame, alternating off/on, the pitch their arrival.
    let midi: Vec<UmpAt> = (0..120u8)
        .map(|i| {
            let e = if i % 2 == 0 {
                (
                    Offset::ZERO,
                    midi::note_off(MidiGroup::FIRST, MidiChannel::FIRST, i, 0),
                )
            } else {
                (
                    Offset::ZERO,
                    midi::note_on(MidiGroup::FIRST, MidiChannel::FIRST, i, midi::MID_VELOCITY),
                )
            };
            (at(u32::from(i / 12)), e.1)
        })
        .collect();
    // 20 expressions a frame, frames 9 down to 0.
    let note_expressions: Vec<NoteExpressionValue> =
        (0..200).map(|j| expr(9 - j / 20, j)).collect();
    let expr_ints: Vec<NoteExpressionIntValue> = (0..4)
        .map(|j| NoteExpressionIntValue {
            sample_offset: 5,
            note_id: j,
            type_id: 0,
            value: 1,
        })
        .collect();
    let chords: Vec<Utf16Chord> = (0..4).map(|j| chord(5, j)).collect();

    let list = EventList::input();
    let mut stager = Stager::default();
    assert!(stager.stage(
        &list,
        &InputEvents {
            midi: &midi,
            note_expressions: &note_expressions,
            chords: &chords,
            expr_ints: &expr_ints,
            ..Default::default()
        },
    ));

    // The expected order, built from the sources: every event in source
    // order, then a stable sort by frame (outside any gate).
    let mut expected: Vec<(i32, (&'static str, i32))> = Vec::new();
    for i in 0..120i32 {
        let kind = if i % 2 == 0 { "off" } else { "on" };
        expected.push((i / 12, (kind, i)));
    }
    for j in 0..200 {
        expected.push((9 - j / 20, ("expr", j)));
    }
    for j in 0..4 {
        expected.push((5, ("int", j)));
    }
    for j in 0..4 {
        expected.push((5, ("chord", j)));
    }
    expected.sort_by_key(|(offset, _)| *offset);
    assert_eq!(staged(&list), expected);
}

/// **Past the room, a note-off is kept and a note-on gives way, and
/// every loss is counted once; the list never grows.** A block of
/// `EVENT_CAPACITY` note-ons and then ten note-offs keeps all ten
/// note-offs, in place of the last ten note-ons. A chord past the room
/// is dropped without spending the text arena.
#[test]
fn past_the_room_note_offs_are_kept_and_the_rest_counted() {
    let ons = EVENT_CAPACITY;
    let midi: Vec<UmpAt> = (0..ons)
        .map(|i| {
            (
                at(i as u32),
                midi::note_on(
                    MidiGroup::FIRST,
                    MidiChannel::FIRST,
                    (i % 100) as u8,
                    midi::MID_VELOCITY,
                ),
            )
        })
        .chain((0..10u8).map(|k| {
            (
                at(ons as u32),
                midi::note_off(MidiGroup::FIRST, MidiChannel::FIRST, 100 + k, 0),
            )
        }))
        .collect();
    let chords = [chord(0, 1)];
    let src = InputEvents {
        midi: &midi,
        chords: &chords,
        ..Default::default()
    };

    let list = EventList::input();
    let mut stager = Stager::default();
    let (storage, capacity) = list.with_events(|staged| (staged.events.as_ptr(), staged.capacity));
    assert_no_alloc::assert_no_alloc(|| stager.stage(&list, &src));
    assert_eq!(
        list.with_events(|staged| (staged.events.as_ptr(), staged.capacity)),
        (storage, capacity),
        "the list never grew"
    );
    assert_eq!(len(&list), EVENT_CAPACITY);
    assert_eq!(list.dropped(), 11, "ten note-ons gave way, and the chord");
    assert!(
        list.with_events(|staged| staged.text.is_empty()),
        "the dropped chord spent no text"
    );

    let events = staged(&list);
    let offs: Vec<i32> = events
        .iter()
        .filter(|(_, (kind, _))| *kind == "off")
        .map(|(_, (_, pitch))| *pitch)
        .collect();
    assert_eq!(offs, (100..110).collect::<Vec<_>>(), "every note-off kept");
    // The note-ons that gave way are the last ten staged.
    let last_on = events
        .iter()
        .rev()
        .find(|(_, (kind, _))| *kind == "on")
        .unwrap();
    assert_eq!(last_on.0, (ons - 11) as i32);
}

/// **A dense expression block does not spend the harmony kinds' room.**
/// A full MIDI block, 300 per-note expressions (44 past their budget)
/// and four chords and scales: every chord and scale is staged, and the
/// 44 expressions past `NOTE_EXPRESSION_ROOM` are what is dropped,
/// counted.
#[test]
fn note_expressions_past_their_budget_leave_the_harmony_room() {
    use tutti_plugin_types::{HARMONY_CAPACITY, MIDI_STACK_CAPACITY};
    let midi: Vec<UmpAt> = (0..MIDI_STACK_CAPACITY)
        .map(|i| {
            (
                at(i as u32),
                midi::note_on(
                    MidiGroup::FIRST,
                    MidiChannel::FIRST,
                    (i % 128) as u8,
                    midi::MID_VELOCITY,
                ),
            )
        })
        .collect();
    let note_expressions: Vec<NoteExpressionValue> = (0..NOTE_EXPRESSION_ROOM as i32 + 44)
        .map(|j| expr(j, j))
        .collect();
    let chords: Vec<Utf16Chord> = (0..HARMONY_CAPACITY as i16)
        .map(|j| chord(j as i32, j))
        .collect();
    let scales: Vec<Utf16Scale> = (0..HARMONY_CAPACITY as i16)
        .map(|j| Utf16Scale {
            sample_offset: i32::from(j),
            root: j,
            mask: 0,
            text: "D".encode_utf16().collect(),
        })
        .collect();

    let list = EventList::input();
    let mut stager = Stager::default();
    assert_no_alloc::assert_no_alloc(|| {
        stager.stage(
            &list,
            &InputEvents {
                midi: &midi,
                note_expressions: &note_expressions,
                chords: &chords,
                scales: &scales,
                ..Default::default()
            },
        )
    });
    assert_eq!(list.dropped(), 44, "only the expressions past their budget");
    let events = staged(&list);
    let count = |kind: &str| events.iter().filter(|(_, (k, _))| *k == kind).count();
    assert_eq!(count("chord"), HARMONY_CAPACITY);
    assert_eq!(count("scale"), HARMONY_CAPACITY);
    assert_eq!(count("expr"), NOTE_EXPRESSION_ROOM);
    // The first ones are kept: the budget takes them in source order.
    let last_expr = events
        .iter()
        .filter(|(_, (k, _))| *k == "expr")
        .map(|(_, (_, id))| *id)
        .max();
    assert_eq!(last_expr, Some(NOTE_EXPRESSION_ROOM as i32 - 1));
}

/// **A negative offset sorts before zero.** A per-note expression's
/// `sample_offset` is a wire `i32` nothing clamps, so a negative one
/// reaches the sort.
#[test]
fn a_negative_offset_sorts_before_zero() {
    let midi = [(
        at(0),
        midi::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, midi::MID_VELOCITY),
    )];
    let note_expressions = [expr(3, 1), expr(-1, 2)];
    let list = EventList::input();
    let mut stager = Stager::default();
    assert!(stager.stage(
        &list,
        &InputEvents {
            midi: &midi,
            note_expressions: &note_expressions,
            ..Default::default()
        },
    ));
    assert_eq!(
        staged(&list),
        vec![(-1, ("expr", 2)), (0, ("on", 60)), (3, ("expr", 1))]
    );
}

/// **Draining a plugin's SysEx allocates nothing**, whichever way the plugin
/// handed it over: a framed message in a plain `Data` event (the single-packet
/// path), and a message of several packets (the reassembled path). A message
/// with a byte whose top bit is set is malformed: it is dropped (a `Data` event
/// that does not convert is not counted, like one too long for a packet), and
/// nothing is read from it with the bit dropped.
#[test]
fn draining_a_plugins_sysex_allocates_nothing() {
    use tutti_vst3_ffi::{Data, Payload};

    // Single packet: `F0 .. F7` in a Data event of subtype 0.
    let list = EventList::input();
    let mut staging = list.begin();
    for bytes in [&[0xF0, 1, 2, 3, 0xF7][..], &[0xF0, 0xF7]] {
        let data = Data::new(0, bytes).unwrap();
        assert!(staging.push(Event::at(7, Payload::Data(data))));
    }
    staging.finish();
    let mut out = RtMidiEvents::new();
    assert_no_alloc::assert_no_alloc(|| drain_midi(&list, &mut out));
    let payloads: Vec<Vec<u8>> = out
        .iter()
        .map(|e| {
            let Ok(Message::Sysex7(_, packet)) = e.ump.decode() else {
                panic!("a SysEx7 packet");
            };
            assert_eq!((packet.status(), e.offset), (SysexStatus::Complete, 7));
            packet.data().to_vec()
        })
        .collect();
    assert_eq!(payloads, [vec![1, 2, 3], vec![]]);
    assert_eq!(list.dropped(), 0);

    // A byte with its top bit set: the message is dropped whole.
    let list = EventList::input();
    let mut staging = list.begin();
    let data = Data::new(0, &[0xF0, 0x81, 0xFF, 2, 0xF7]).unwrap();
    assert!(staging.push(Event::at(7, Payload::Data(data))));
    staging.finish();
    let mut out = RtMidiEvents::new();
    drain_midi(&list, &mut out);
    assert!(out.is_empty());
    assert!(!out.is_overflowed(), "malformed is not an overflow");

    // Several packets, staged through the reassembler and drained.
    let payload: Vec<u8> = (0..20).collect();
    let mut fragments = Vec::new();
    fragments.extend(
        tutti_ump::Sysex7Packets::checked(MidiGroup::FIRST, &payload).expect("a 7-bit payload"),
    );
    let packets: Vec<UmpAt> = fragments
        .into_iter()
        .map(|ump| (Offset::ZERO, ump))
        .collect();
    let list = EventList::input();
    assert!(Stager::default().stage(&list, &midi_only(&packets)));
    let mut out = RtMidiEvents::new();
    assert_no_alloc::assert_no_alloc(|| drain_midi(&list, &mut out));
    // 20 payload bytes, six to a packet (M2-104 section 7.7): 4 packets.
    assert_eq!(out.len(), 4);
}
