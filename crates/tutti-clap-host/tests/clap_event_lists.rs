//! The event lists as a plugin drives them: through the raw `clap_input_events`
//! / `clap_output_events` vtables the FFI crate builds, with hand-made events
//! (null pointers, undersized headers, a foreign space, a misaligned event).
//!
//! These are the unit tests that used raw pointers; the host crate forbids
//! `unsafe`, so they sit here, where a test may use it. Each test's property
//! and its recorded mutation are as they were.

use std::ptr;
use tutti_graph::UmpAt;

use clap_sys::events::{
    CLAP_CORE_EVENT_SPACE_ID, CLAP_EVENT_MIDI, CLAP_EVENT_MIDI_SYSEX, CLAP_EVENT_NOTE_CHOKE,
    CLAP_EVENT_NOTE_END, CLAP_EVENT_NOTE_EXPRESSION, CLAP_EVENT_NOTE_OFF, CLAP_EVENT_NOTE_ON,
    CLAP_EVENT_PARAM_GESTURE_BEGIN, CLAP_EVENT_PARAM_GESTURE_END, CLAP_EVENT_PARAM_MOD,
    CLAP_EVENT_PARAM_VALUE, clap_event_header, clap_event_midi, clap_event_midi_sysex,
    clap_event_note, clap_event_note_expression, clap_event_param_gesture, clap_event_param_mod,
    clap_event_param_value, clap_input_events, clap_output_events,
};
use tutti_clap_ffi::{
    EventSink, EventSource, header_size, input_raw_for_test, output_raw_for_test,
};
use tutti_clap_host::{ClapEvent, EventList, InputEventList, NoteExpressionType, OutputEventList};
use tutti_graph::Offset;
use tutti_plugin_types::{
    ParameterChanges, ParameterQueue, RtMidiEvents, SysExAssembler, SysExStep, WireMidi, unframed,
};
use tutti_types::Samples;
use tutti_ump::Ump;

/// The packet of `words`, zero-padded to four, `k` frames into a 100-frame block.
fn packet(k: u32, words: &[u32]) -> UmpAt {
    let mut w = [0u32; 4];
    w[..words.len()].copy_from_slice(words);
    (
        Offset::new(k as usize, Samples(100)).expect("inside the block"),
        Ump(w),
    )
}

/// The raw output list of `output`, valid while `output` is alive and unmoved.
fn raw_out(output: &mut OutputEventList) -> *const clap_output_events {
    let sink: *mut dyn EventSink = output;
    // SAFETY: each test keeps `output` alive and in place while it uses the
    // list, and touches `output` only between pushes.
    unsafe { output_raw_for_test(sink) }
}

/// The raw input list of `list`, valid while `list` is alive and unmoved.
fn raw_in(list: &InputEventList) -> *const clap_input_events {
    let source: *const dyn EventSource = list;
    // SAFETY: as `raw_out`.
    unsafe { input_raw_for_test(source) }
}

/// The size of the core event struct of `type_`, as the table the FFI crate
/// holds it.
fn core_event_size(type_: u16) -> Option<u32> {
    Some(match type_ {
        CLAP_EVENT_NOTE_ON | CLAP_EVENT_NOTE_OFF | CLAP_EVENT_NOTE_CHOKE | CLAP_EVENT_NOTE_END => {
            header_size::<clap_event_note>()
        }
        CLAP_EVENT_MIDI => header_size::<clap_event_midi>(),
        CLAP_EVENT_NOTE_EXPRESSION => header_size::<clap_event_note_expression>(),
        CLAP_EVENT_PARAM_VALUE => header_size::<clap_event_param_value>(),
        CLAP_EVENT_PARAM_MOD => header_size::<clap_event_param_mod>(),
        CLAP_EVENT_PARAM_GESTURE_BEGIN | CLAP_EVENT_PARAM_GESTURE_END => {
            header_size::<clap_event_param_gesture>()
        }
        CLAP_EVENT_MIDI_SYSEX => header_size::<clap_event_midi_sysex>(),
        _ => return None,
    })
}

fn pooled_value(id: u32, k: usize) -> f64 {
    ((id as usize + k) % 7) as f64 / 8.0
}

#[test]
fn test_output_events_push_note_on() {
    let mut output = OutputEventList::new();
    let event = ClapEvent::note_on(0, 0, 60, 0.8);
    let header = event.header();

    let list_ptr = raw_out(&mut output);
    unsafe {
        let push_fn = (*list_ptr).try_push.unwrap();
        let result = push_fn(list_ptr, header as *const clap_event_header);
        assert!(result);
    }

    assert_eq!(output.events().len(), 1);
}

#[test]
fn fill_gestures_captures_gesture_and_mod_events() {
    // Gesture begin/end + param-mod on the output side must not be
    // silently dropped. They land in the CLAP-private gesture list, and
    // `fill_param_changes` still ignores them (only PARAM_VALUE flows there).
    use clap_sys::events::{
        CLAP_EVENT_PARAM_GESTURE_BEGIN, CLAP_EVENT_PARAM_GESTURE_END, clap_event_param_gesture,
        clap_event_param_mod,
    };

    let mut output = OutputEventList::new();
    let list_ptr = raw_out(&mut output);

    let begin = clap_event_param_gesture {
        header: clap_event_header {
            size: header_size::<clap_event_param_gesture>(),
            time: 0,
            space_id: CLAP_CORE_EVENT_SPACE_ID,
            type_: CLAP_EVENT_PARAM_GESTURE_BEGIN,
            flags: 0,
        },
        param_id: 7,
    };
    let end = clap_event_param_gesture {
        header: clap_event_header {
            size: header_size::<clap_event_param_gesture>(),
            time: 4,
            space_id: CLAP_CORE_EVENT_SPACE_ID,
            type_: CLAP_EVENT_PARAM_GESTURE_END,
            flags: 0,
        },
        param_id: 7,
    };
    let modev = clap_event_param_mod {
        header: clap_event_header {
            size: header_size::<clap_event_param_mod>(),
            time: 2,
            space_id: CLAP_CORE_EVENT_SPACE_ID,
            type_: CLAP_EVENT_PARAM_MOD,
            flags: 0,
        },
        param_id: 9,
        cookie: ptr::null_mut(),
        note_id: -1,
        port_index: -1,
        channel: -1,
        key: -1,
        amount: 0.25,
    };
    // Also push a plain PARAM_VALUE to prove the two paths stay separate.
    let value = ClapEvent::param_value(3, 5, 0.9);

    unsafe {
        let push = (*list_ptr).try_push.unwrap();
        assert!(push(list_ptr, &begin.header as *const _));
        assert!(push(list_ptr, &modev.header as *const _));
        assert!(push(list_ptr, &end.header as *const _));
        assert!(push(list_ptr, value.header() as *const _));
    }

    let mut gestures: Vec<ClapEvent> = Vec::new();
    output.fill_gestures(&mut gestures);
    assert_eq!(gestures.len(), 3, "begin + mod + end captured");
    assert!(
        gestures
            .iter()
            .any(|e| matches!(e, ClapEvent::ParamGestureBegin(_)))
    );
    assert!(
        gestures
            .iter()
            .any(|e| matches!(e, ClapEvent::ParamGestureEnd(_)))
    );
    assert!(gestures.iter().any(|e| matches!(e, ClapEvent::ParamMod(_))));

    // fill_param_changes only picks up the PARAM_VALUE, not gestures/mods.
    // Reserved: `fill_param_changes` fills within the set's room, and an
    // unreserved set has none. Two queues, so a gesture or mod event
    // wrongly queued would show as a second one.
    let mut changes = ParameterChanges::reserved(2, 10);
    assert_eq!(
        output.fill_param_changes(&mut changes),
        0,
        "nothing past the room"
    );
    assert_eq!(changes.queues.len(), 1, "only the PARAM_VALUE queued");
    assert_eq!(changes.queues[0].param_id, 5u32.into());

    // Draining clears the pool.
    output.fill_gestures(&mut gestures);
    assert_eq!(gestures.len(), 3);
    gestures.clear();
    assert!(gestures.is_empty());
}

#[test]
fn test_output_events_push_null_event() {
    let mut output = OutputEventList::new();
    let list_ptr = raw_out(&mut output);
    unsafe {
        let push_fn = (*list_ptr).try_push.unwrap();
        let result = push_fn(list_ptr, std::ptr::null());
        assert!(!result);
    }
    assert!(output.events().is_empty());
}

#[test]
fn test_output_events_push_null_list() {
    let event = ClapEvent::note_on(0, 0, 60, 0.8);
    let header = event.header();
    unsafe {
        // A real vtable, called with a null list.
        let raw = raw_out(&mut OutputEventList::new());
        let result =
            ((*raw).try_push.unwrap())(std::ptr::null(), header as *const clap_event_header);
        assert!(!result);
    }
}

/// An event of another space reuses core type numbers for other structs,
/// so it is refused, whatever its type says. Both the plain path and the
/// full-parameter-room path (`coalesce_param`) read the struct.
///
/// Mutation (run): delete the `space_id` test in `output_events_try_push` →
/// both pushes are accepted and the assertions fail; drop the
/// `refused += 1` there → the count assertion fails.
#[test]
fn output_events_refuse_a_foreign_space() {
    let mut output = OutputEventList::new();
    let list_ptr = raw_out(&mut output);
    let push = unsafe { (*list_ptr).try_push.unwrap() };

    let mut note = *match ClapEvent::note_on(0, 0, 60, 0.5) {
        ClapEvent::NoteOn(e) => Box::new(e),
        _ => unreachable!(),
    };
    note.header.space_id = 512;
    assert!(!unsafe { push(list_ptr, &note.header) });

    let mut param = clap_event_param_value {
        header: *ClapEvent::param_value(0, 1, 0.5).header(),
        param_id: 1,
        cookie: ptr::null_mut(),
        note_id: -1,
        port_index: -1,
        channel: -1,
        key: -1,
        value: 0.5,
    };
    param.header.space_id = 512;
    output.set_param_room(0, &[]);
    assert!(!unsafe { push(list_ptr, &param.header) });
    assert!(output.events().is_empty());
    assert_eq!(output.events_refused(), 2);
}

/// A `size` under the type's struct would read past the plugin's event.
///
/// Mutation (run): delete the `header.size < min` test → the push is
/// accepted and the assertion fails.
#[test]
fn output_events_refuse_an_undersized_event() {
    let mut output = OutputEventList::new();
    let list_ptr = raw_out(&mut output);
    let push = unsafe { (*list_ptr).try_push.unwrap() };

    let ClapEvent::NoteOn(mut note) = ClapEvent::note_on(0, 0, 60, 0.5) else {
        unreachable!()
    };
    note.header.size = header_size::<clap_event_header>();
    assert!(!unsafe { push(list_ptr, &note.header) });
    assert!(output.events().is_empty());

    // The full size is taken.
    note.header.size = header_size::<clap_event_note>();
    assert!(unsafe { push(list_ptr, &note.header) });
    assert_eq!(output.events().len(), 1);
}

/// Every core type a plugin may push is refused one byte short of its
/// struct and taken at its exact size: this pins `core_event_size`.
///
/// Mutations (run): return a size one larger for a type → its exact-size
/// push is refused; one smaller → its short push is taken; drop a type
/// from the table → both its pushes are refused.
#[test]
fn output_events_take_every_core_type_only_at_its_struct_size() {
    let note = *match ClapEvent::note_on(0, 0, 60, 0.5) {
        ClapEvent::NoteOn(e) => Box::new(e),
        _ => unreachable!(),
    };
    let midi = *match ClapEvent::midi(0, 0, [0x90, 60, 64]) {
        ClapEvent::Midi(e) => Box::new(e),
        _ => unreachable!(),
    };
    let expr = *match ClapEvent::note_expression(0, NoteExpressionType::Volume, 1, 0.5) {
        Some(ClapEvent::NoteExpression(e)) => Box::new(e),
        _ => unreachable!(),
    };
    let param = *match ClapEvent::param_value(0, 1, 0.5) {
        ClapEvent::ParamValue(e) => Box::new(e),
        _ => unreachable!(),
    };
    let pmod = clap_event_param_mod {
        header: clap_event_header {
            size: header_size::<clap_event_param_mod>(),
            time: 0,
            space_id: CLAP_CORE_EVENT_SPACE_ID,
            type_: CLAP_EVENT_PARAM_MOD,
            flags: 0,
        },
        param_id: 1,
        cookie: ptr::null_mut(),
        note_id: -1,
        port_index: -1,
        channel: -1,
        key: -1,
        amount: 0.0,
    };
    let gesture = clap_event_param_gesture {
        header: clap_event_header {
            size: header_size::<clap_event_param_gesture>(),
            time: 0,
            space_id: CLAP_CORE_EVENT_SPACE_ID,
            type_: CLAP_EVENT_PARAM_GESTURE_BEGIN,
            flags: 0,
        },
        param_id: 1,
    };
    let data = [0xF0u8, 0x7E, 0xF7];
    let sysex = clap_event_midi_sysex {
        header: clap_event_header {
            size: header_size::<clap_event_midi_sysex>(),
            time: 0,
            space_id: CLAP_CORE_EVENT_SPACE_ID,
            type_: CLAP_EVENT_MIDI_SYSEX,
            flags: 0,
        },
        port_index: 0,
        buffer: data.as_ptr(),
        size: data.len() as u32,
    };

    // (type, header, its struct's size)
    let mut cases: Vec<(u16, clap_event_header)> = Vec::new();
    for type_ in [
        CLAP_EVENT_NOTE_ON,
        CLAP_EVENT_NOTE_OFF,
        CLAP_EVENT_NOTE_CHOKE,
        CLAP_EVENT_NOTE_END,
    ] {
        let mut h = note.header;
        h.type_ = type_;
        cases.push((type_, h));
    }
    cases.push((CLAP_EVENT_MIDI, midi.header));
    cases.push((CLAP_EVENT_NOTE_EXPRESSION, expr.header));
    cases.push((CLAP_EVENT_PARAM_VALUE, param.header));
    cases.push((CLAP_EVENT_PARAM_MOD, pmod.header));
    cases.push((CLAP_EVENT_PARAM_GESTURE_BEGIN, gesture.header));
    let mut end = gesture.header;
    end.type_ = CLAP_EVENT_PARAM_GESTURE_END;
    cases.push((CLAP_EVENT_PARAM_GESTURE_END, end));
    cases.push((CLAP_EVENT_MIDI_SYSEX, sysex.header));

    for (type_, header) in cases {
        let exact = core_event_size(type_).expect("a core type");
        assert_eq!(exact, header.size, "type {type_}: the table is its struct");

        // Each push gets a struct whose header `size` is what is varied;
        // the body is the real, full-size one, so the exact push is read
        // soundly.
        let push_with = |size: u32| -> bool {
            let mut output = OutputEventList::new();
            output.reserve(4);
            let list_ptr = raw_out(&mut output);
            let push = unsafe { (*list_ptr).try_push.unwrap() };
            let mut h = header;
            h.size = size;
            let accepted = match type_ {
                CLAP_EVENT_NOTE_ON..=CLAP_EVENT_NOTE_END => {
                    let mut e = note;
                    e.header = h;
                    unsafe { push(list_ptr, &e.header) }
                }
                CLAP_EVENT_MIDI => {
                    let mut e = midi;
                    e.header = h;
                    unsafe { push(list_ptr, &e.header) }
                }
                CLAP_EVENT_NOTE_EXPRESSION => {
                    let mut e = expr;
                    e.header = h;
                    unsafe { push(list_ptr, &e.header) }
                }
                CLAP_EVENT_PARAM_VALUE => {
                    let mut e = param;
                    e.header = h;
                    unsafe { push(list_ptr, &e.header) }
                }
                CLAP_EVENT_PARAM_MOD => {
                    let mut e = pmod;
                    e.header = h;
                    unsafe { push(list_ptr, &e.header) }
                }
                CLAP_EVENT_PARAM_GESTURE_BEGIN | CLAP_EVENT_PARAM_GESTURE_END => {
                    let mut e = gesture;
                    e.header = h;
                    unsafe { push(list_ptr, &e.header) }
                }
                CLAP_EVENT_MIDI_SYSEX => {
                    let mut e = sysex;
                    e.header = h;
                    unsafe { push(list_ptr, &e.header) }
                }
                _ => unreachable!(),
            };
            assert_eq!(output.events().len(), usize::from(accepted));
            accepted
        };
        assert!(
            !push_with(exact - 1),
            "type {type_}: one byte short is refused"
        );
        assert!(push_with(exact), "type {type_}: the exact size is taken");
    }
}

#[test]
fn test_output_events_push_unknown_event_type() {
    let mut output = OutputEventList::new();
    let list_ptr = raw_out(&mut output);

    let header = clap_event_header {
        size: header_size::<clap_event_header>(),
        time: 0,
        space_id: CLAP_CORE_EVENT_SPACE_ID,
        type_: 9999,
        flags: 0,
    };
    unsafe {
        let push_fn = (*list_ptr).try_push.unwrap();
        let result = push_fn(list_ptr, &header as *const clap_event_header);
        assert!(!result);
    }
    assert!(output.events().is_empty());
}

#[test]
fn test_output_events_push_multiple() {
    let mut output = OutputEventList::new();
    let list_ptr = raw_out(&mut output);

    for i in 0..5 {
        let event = ClapEvent::note_on(i, 0, 60 + i as i16, 0.5);
        let header = event.header();
        unsafe {
            let push_fn = (*list_ptr).try_push.unwrap();
            let result = push_fn(list_ptr, header as *const clap_event_header);
            assert!(result);
        }
    }

    assert_eq!(output.events().len(), 5);
}

#[test]
fn test_output_events_push_sysex_with_data() {
    let mut output = OutputEventList::new();
    let list_ptr = raw_out(&mut output);

    let sysex_data: Vec<u8> = vec![0xF0, 0x7E, 0x7F, 0x09, 0x01, 0xF7];
    let sysex = clap_event_midi_sysex {
        header: clap_event_header {
            size: header_size::<clap_event_midi_sysex>(),
            // Deliberately non-zero: a `time` that never got copied would
            // read back as 0 and pass a `time: 0` fixture either way.
            time: 25,
            space_id: CLAP_CORE_EVENT_SPACE_ID,
            type_: CLAP_EVENT_MIDI_SYSEX,
            flags: 0,
        },
        port_index: 0,
        buffer: sysex_data.as_ptr(),
        size: sysex_data.len() as u32,
    };

    unsafe {
        let push_fn = (*list_ptr).try_push.unwrap();
        let result = push_fn(
            list_ptr,
            &sysex as *const clap_event_midi_sysex as *const clap_event_header,
        );
        assert!(result);
    }
    assert_eq!(output.events().len(), 1);
    match &output.events()[0] {
        ClapEvent::MidiSysex { inner, _data } => {
            assert_eq!(inner.header.time, 25);
            assert_eq!(inner.port_index, 0);
            assert_eq!(_data, &sysex_data);
        }
        _ => panic!("Expected MidiSysex event"),
    }
    // Sysex is its own event kind, not a generic MIDI byte triple, and it
    // drains as UMP SysEx7 packets at the plugin's frame offset.
    let drained = output.to_midi_events();
    assert_eq!(drained.len(), 1, "a 4-byte payload is one packet");
    assert_eq!(drained[0].offset, 25);
    let (status, bytes, n) =
        tutti_midi_types::sysex7_payload(&drained[0].ump).expect("a SysEx7 packet");
    assert_eq!(status, tutti_midi_types::ump::SYSEX7_STATUS_SINGLE);
    assert_eq!(&bytes[..n], &sysex_data[1..sysex_data.len() - 1]);
}

/// **A long SysEx a plugin emits reaches the host whole.** It used to be
/// dropped: `ClapEvent::to_midi` is one event, and a SysEx is many
/// packets.
///
/// Mutation (run): make `fill_midi_events` skip `sysex_bytes` (the old
/// `to_midi` only path) → nothing is drained → fails.
#[test]
fn a_long_sysex_from_the_plugin_drains_whole() {
    let body: Vec<u8> = (0..40).map(|i| i as u8).collect();
    let message: Vec<u8> = tutti_ump::frame(&body).collect();
    let mut output = OutputEventList::new();
    let list_ptr = raw_out(&mut output);
    let sysex = clap_event_midi_sysex {
        header: clap_event_header {
            size: header_size::<clap_event_midi_sysex>(),
            time: 7,
            space_id: CLAP_CORE_EVENT_SPACE_ID,
            type_: CLAP_EVENT_MIDI_SYSEX,
            flags: 0,
        },
        port_index: 0,
        buffer: message.as_ptr(),
        size: message.len() as u32,
    };
    unsafe {
        let push_fn = (*list_ptr).try_push.unwrap();
        assert!(push_fn(
            list_ptr,
            &sysex as *const clap_event_midi_sysex as *const clap_event_header,
        ));
    }
    let reassemble = |events: &[WireMidi]| {
        let mut asm = SysExAssembler::reserved();
        let mut done = Vec::new();
        for e in events {
            assert_eq!(e.offset, 7);
            if let SysExStep::Complete(r) = asm.push(&e.ump) {
                done.push(asm.message(r).to_vec());
            }
        }
        done
    };
    assert_eq!(reassemble(&output.to_midi_events()), vec![message.clone()]);
    let mut pool = RtMidiEvents::new();
    output.fill_midi_events(&mut pool);
    assert!(!pool.is_overflowed());
    assert_eq!(reassemble(pool.as_slice()), vec![message]);
    assert_eq!(unframed(&[0xF0, 1, 0xF7]), &[1]);
}

#[test]
fn test_output_events_push_sysex_null_buffer() {
    let mut output = OutputEventList::new();
    let list_ptr = raw_out(&mut output);

    let sysex = clap_event_midi_sysex {
        header: clap_event_header {
            size: header_size::<clap_event_midi_sysex>(),
            time: 0,
            space_id: CLAP_CORE_EVENT_SPACE_ID,
            type_: CLAP_EVENT_MIDI_SYSEX,
            flags: 0,
        },
        port_index: 0,
        buffer: std::ptr::null(),
        size: 0,
    };

    unsafe {
        let push_fn = (*list_ptr).try_push.unwrap();
        let result = push_fn(
            list_ptr,
            &sysex as *const clap_event_midi_sysex as *const clap_event_header,
        );
        // try_push accepts the event but a null sysex buffer yields no stored event.
        assert!(result);
    }
    assert!(output.events().is_empty());
}

/// **The plugin's output at the pool budget is grouped into the pooled
/// queues without allocating, each per parameter and time-ordered; one
/// point past it is counted.** The output list and its queues reserved
/// as activation reserves them for 2 000 parameters at 512 frames; the
/// plugin pushes `POOLED_QUEUES` parameters at full room and every other
/// one at its floor, interleaved frame by frame, inside the gate. Then
/// one more parameter pushes a point past its floor: past the list's parameter
/// room it is kept as that parameter's latest, and with the queues' pool
/// spent it replaces the queue's last point, counted once.
///
/// Mutations (run), each fails: `fill_param_changes` without
/// `make_room` (the queue's own floor only) → the moving parameters
/// lose points past their floor, counted; the over-budget point dropped
/// instead of replacing the last → the parameter ends on its earlier
/// value.
#[test]
fn plugin_output_at_the_pool_budget_groups_without_allocating() {
    const PARAMS: u32 = 2_000;
    let room = ParameterQueue::chunk_room(512);
    let budget = ParameterChanges::point_budget(PARAMS as usize, room);
    let ids: Vec<u32> = (0..PARAMS).collect();
    let mut output = OutputEventList::new();
    output.reserve(16);
    output.set_param_room(budget, &ids);
    let mut changes = ParameterChanges::reserved(PARAMS as usize, room);
    let (floor, _) = ParameterChanges::point_layout(PARAMS as usize, room);
    let push = |output: &mut OutputEventList, event: &ClapEvent| {
        let list = raw_out(output);
        // SAFETY: `list` is `output`'s own vtable, live for the call, and
        // `event` a whole `PARAM_VALUE` event.
        unsafe { (*list).try_push.expect("try_push")(list, event.header()) }
    };
    let block = |output: &mut OutputEventList, changes: &mut ParameterChanges, extra: bool| {
        output.clear();
        for k in 0..room {
            for id in 0..PARAMS {
                let moving = (id as usize) < ParameterChanges::POOLED_QUEUES;
                let third = extra && id == PARAMS - 1 && k == floor;
                if k >= floor && !moving && !third {
                    continue;
                }
                let e = ClapEvent::param_value(3 * k as u32, id, pooled_value(id, k));
                assert!(push(output, &e));
            }
        }
        output.fill_param_changes(changes)
    };
    assert_eq!(block(&mut output, &mut changes, false), 0);
    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..3 {
            assert_eq!(block(&mut output, &mut changes, false), 0);
        }
    });
    assert_eq!(changes.queues.len(), PARAMS as usize);
    for q in &changes.queues {
        let id = q.param_id.get();
        let n = if (id as usize) < ParameterChanges::POOLED_QUEUES {
            room
        } else {
            floor
        };
        let want: Vec<(i32, f64)> = (0..n)
            .map(|k| (3 * k as i32, pooled_value(id, k)))
            .collect();
        let got: Vec<(i32, f64)> = q
            .points
            .iter()
            .map(|p| (p.sample_offset, p.value.get()))
            .collect();
        assert_eq!(got, want, "parameter {id}");
    }

    let mut dropped = 0;
    assert_no_alloc::assert_no_alloc(|| dropped = block(&mut output, &mut changes, true));
    assert_eq!(dropped, 1);
    let last = changes.queue((PARAMS - 1).into()).expect("queued");
    assert_eq!(last.points.len(), floor);
    let k = floor;
    assert_eq!(
        last.points.last().map(|p| (p.sample_offset, p.value.get())),
        Some((3 * k as i32, pooled_value(PARAMS - 1, k))),
        "ends on its latest"
    );
}

/// An event a plugin builds at a 4-aligned address (the header's own
/// alignment) is read intact by `try_push`.
///
/// Mutation: dereference instead (`*header.cast::<T>()`) in `read_event`:
/// the debug build's misaligned-deref check aborts the test process.
#[test]
fn try_push_reads_an_event_the_plugin_left_4_aligned() {
    let ClapEvent::NoteOn(note) = ClapEvent::note_on(9, 2, 60, 0.75) else {
        unreachable!()
    };
    let mut backing = [0u64; 32];
    // Offset 4 into an 8-aligned buffer: aligned for the header, not the note.
    let at = unsafe { backing.as_mut_ptr().cast::<u32>().add(1) };
    assert_ne!(at as usize % align_of::<clap_event_note>(), 0);
    assert_eq!(at as usize % align_of::<clap_event_header>(), 0);
    unsafe { ptr::write_unaligned(at.cast::<clap_event_note>(), note) };

    let mut output = OutputEventList::new();
    let list_ptr = raw_out(&mut output);
    let pushed = unsafe {
        ((*list_ptr).try_push.unwrap())(list_ptr, at.cast::<clap_event_header>().cast_const())
    };
    assert!(pushed);
    match &output.events()[0] {
        ClapEvent::NoteOn(got) => {
            assert_eq!((got.header.time, got.channel, got.key), (9, 2, 60));
            assert_eq!(got.velocity, 0.75);
        }
        _ => panic!("expected NoteOn"),
    }
}

/// Every event pointer an input list hands the plugin has the alignment
/// its payload struct needs (8 for note and param, 4 for `clap_event_midi`),
/// across pushes of mixed sizes. The needs are read off the C structs, so
/// the test does not depend on how rustc lays out `ClapEvent`.
///
/// Mutation: offset every odd index's pointer by 4 bytes in
/// `input_events_get`: the note-off at index 3 fails.
#[test]
fn input_event_pointers_have_the_alignment_their_payload_needs() {
    let mut events = Vec::new();
    let mut needs = Vec::new();
    for i in 0..64u32 {
        let (event, need) = match i % 4 {
            0 => (
                ClapEvent::note_on(i, 0, 60, 1.0),
                align_of::<clap_event_note>(),
            ),
            1 => (
                ClapEvent::midi(i, 0, [0x90, 60, 100]),
                align_of::<clap_event_midi>(),
            ),
            2 => (
                ClapEvent::param_value(i, 3, 0.5),
                align_of::<clap_event_param_value>(),
            ),
            _ => (
                ClapEvent::note_off(i, 0, 60, 0.0),
                align_of::<clap_event_note>(),
            ),
        };
        events.push(event);
        needs.push(need);
    }
    assert!(needs.contains(&8) && needs.contains(&4));
    let list = InputEventList::from_events(events);
    let raw = raw_in(&list);
    let n = unsafe { ((*raw).size.unwrap())(raw) };
    assert_eq!(n, 64);
    for i in 0..n {
        let h = unsafe { ((*raw).get.unwrap())(raw, i) };
        assert_eq!(h as usize % needs[i as usize], 0, "event {i} at {h:p}");
    }
}

/// **A MIDI 1.0 Note On at velocity 0 reaches the plugin as a `NOTE_OFF` with
/// velocity `0x8000 / 0xFFFF` (about 0.5), not 0** (M2-104 Appendix D.3.1).
/// The plugin reads it through the raw `clap_input_events` vtable, so the
/// figure is the one in the `clap_event_note` it is handed. A Note On at
/// MIDI 1.0 velocity 100 is a `NOTE_ON` near 100/127.
///
/// Mutation (run): `from_voice1` folding a Note On at velocity 0 into a Note
/// Off at velocity 0 → the first assertion fails.
#[test]
fn a_midi1_note_on_at_velocity_zero_reaches_the_plugin_as_a_release_at_half() {
    let mut list = InputEventList::new();
    list.reserve(4);
    list.add_midi(&packet(7, &[0x2091_3C00]));
    list.add_midi(&packet(8, &[0x2091_3C64]));
    let raw = raw_in(&list);
    assert_eq!(unsafe { ((*raw).size.unwrap())(raw) }, 2);

    let off = unsafe { ((*raw).get.unwrap())(raw, 0) };
    assert_eq!(unsafe { (*off).type_ }, CLAP_EVENT_NOTE_OFF);
    assert_eq!(unsafe { (*off).time }, 7);
    let off = unsafe { &*off.cast::<clap_event_note>() };
    assert_eq!((off.channel, off.key), (1, 60));
    assert!(
        (off.velocity - 32768.0 / 65535.0).abs() < 1e-6,
        "{}",
        off.velocity
    );

    let on = unsafe { ((*raw).get.unwrap())(raw, 1) };
    assert_eq!(unsafe { (*on).type_ }, CLAP_EVENT_NOTE_ON);
    let on = unsafe { &*on.cast::<clap_event_note>() };
    // Min-Center-Max scaling (M2-115 section 3) keeps 100 within 1% of 100/127.
    assert!(
        (on.velocity - 100.0 / 127.0).abs() < 0.01,
        "{}",
        on.velocity
    );
}
