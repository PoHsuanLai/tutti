use std::ffi::c_void;

use clap_sys::events::{
    clap_event_header, clap_event_note, clap_event_note_expression, clap_event_param_gesture,
    clap_event_param_mod, clap_event_param_value, CLAP_CORE_EVENT_SPACE_ID, CLAP_EVENT_MIDI,
    CLAP_EVENT_NOTE_CHOKE, CLAP_EVENT_NOTE_END, CLAP_EVENT_NOTE_EXPRESSION, CLAP_EVENT_NOTE_ON,
    CLAP_EVENT_PARAM_GESTURE_BEGIN, CLAP_EVENT_PARAM_GESTURE_END, CLAP_EVENT_PARAM_MOD,
    CLAP_EVENT_PARAM_VALUE,
};
use tutti_clap_host::{
    ClapEvent, ClapHost, EventList, HostState, InputEventList, InputStream, MidiEvent,
    NoteExpressionType, NoteName, OutputEventList, OutputStream, ParameterChanges, ParameterQueue,
    VoiceInfo,
};

// ── MIDI conversion via tutti_midi_types::MidiEvent ──
//
// clap-host's `ClapEvent::{from,to}_midi_event` bridge between VST3/CLAP's
// typed event structs and Tutti's UMP `MidiEvent`. These tests cover each
// MIDI-1 message shape we expect to round-trip cleanly.

/// NoteOn maps to CLAP's typed `note` event (NoteOn variant).
#[test]
fn test_note_on_roundtrip() {
    // Build with MIDI-2 velocity (half of u16 max ≈ 0x8000).
    let event = MidiEvent::note_on(0, 3, 60, 0x8000).with_frame_offset(10);
    let clap = ClapEvent::from_midi(&event).expect("NoteOn should convert");
    assert!(matches!(clap, ClapEvent::NoteOn(_)));
    let back = clap.to_midi().expect("round-trip should succeed");
    assert_eq!(back.frame_offset, 10);
    assert!(back.is_note_on());
    assert_eq!(back.note(), Some(60));
}

/// NoteOff maps to CLAP's typed `note` event (NoteOff variant).
#[test]
fn test_note_off_roundtrip() {
    let event = MidiEvent::note_off(0, 5, 72, 0x4000).with_frame_offset(20);
    let clap = ClapEvent::from_midi(&event).expect("NoteOff should convert");
    assert!(matches!(clap, ClapEvent::NoteOff(_)));
    let back = clap.to_midi().expect("round-trip should succeed");
    assert_eq!(back.frame_offset, 20);
    assert!(back.is_note_off());
    assert_eq!(back.note(), Some(72));
}

/// ControlChange falls through to CLAP's generic `Midi` (3-byte wire) event.
#[test]
fn test_control_change_roundtrip() {
    use tutti_midi_types::convert::midi1_cc_to_midi2;
    let event = MidiEvent::cc(0, 2, 74, midi1_cc_to_midi2(100)).with_frame_offset(5);
    let clap = ClapEvent::from_midi(&event).expect("CC should convert");
    // CC goes through the generic Midi event
    match clap {
        ClapEvent::Midi(e) => {
            assert_eq!(e.data[0], 0xB0 | 2, "CC status byte");
            assert_eq!(e.data[1], 74, "controller number");
            assert_eq!(e.data[2], 100, "CC value");
        }
        _ => panic!("Expected Midi event for ControlChange"),
    }
}

/// PitchBend falls through to CLAP's generic `Midi` event. The 14-bit
/// bend value is split across bytes 1 (LSB) and 2 (MSB).
#[test]
fn test_pitch_bend_roundtrip() {
    use tutti_midi_types::convert::midi1_pitch_bend_to_midi2;
    // MIDI-1 center = 8192 (0x2000).
    let event = MidiEvent::pitch_bend(0, 0, midi1_pitch_bend_to_midi2(8192));
    let clap = ClapEvent::from_midi(&event).expect("PitchBend should convert");
    match clap {
        ClapEvent::Midi(e) => {
            assert_eq!(e.data[0] & 0xF0, 0xE0, "PitchBend status nibble");
            // Recover 14-bit value: LSB | (MSB << 7)
            let value = (e.data[1] as u16) | ((e.data[2] as u16) << 7);
            assert_eq!(value, 8192, "PitchBend should round-trip to center");
        }
        _ => panic!("Expected Midi event for PitchBend"),
    }
}

/// ProgramChange is a 2-byte MIDI-1 event, passed through as generic `Midi`.
#[test]
fn test_program_change_roundtrip() {
    let event = MidiEvent::program_change(0, 9, 42, None).with_frame_offset(100);
    let clap = ClapEvent::from_midi(&event).expect("ProgramChange should convert");
    match clap {
        ClapEvent::Midi(e) => {
            assert_eq!(e.data[0], 0xC0 | 9, "ProgramChange status byte");
            assert_eq!(e.data[1], 42, "program number");
        }
        _ => panic!("Expected Midi event for ProgramChange"),
    }
}

/// Note events pushed through `InputEventList::add_midi` should appear in
/// the FFI event table.
#[test]
fn test_input_event_list_add_midi_counts_correctly() {
    let mut list = InputEventList::new();
    list.add_midi(&MidiEvent::note_on(0, 0, 60, 0x8000).with_frame_offset(0));
    list.add_midi(&MidiEvent::note_off(0, 0, 60, 0).with_frame_offset(100));

    let raw = list.as_raw();
    unsafe {
        let size_fn = (*raw).size.unwrap();
        assert_eq!(size_fn(raw), 2);

        let get_fn = (*raw).get.unwrap();
        let header0 = &*get_fn(raw, 0);
        assert_eq!(header0.time, 0);

        let header1 = &*get_fn(raw, 1);
        assert_eq!(header1.time, 100);

        // Out of bounds returns null
        assert!(get_fn(raw, 2).is_null());
    }
}

/// Output NoteOn events survive the FFI round-trip back through
/// `to_midi_events()`.
#[test]
fn test_output_event_list_push_note_on() {
    let mut list = OutputEventList::new();
    let raw = list.as_raw_mut();

    let note = clap_event_note {
        header: clap_event_header {
            size: std::mem::size_of::<clap_event_note>() as u32,
            time: 50,
            space_id: CLAP_CORE_EVENT_SPACE_ID,
            type_: CLAP_EVENT_NOTE_ON,
            flags: 0,
        },
        note_id: 1,
        port_index: 0,
        channel: 0,
        key: 64,
        velocity: 0.9,
    };

    unsafe {
        let push_fn = (*raw).try_push.unwrap();
        let ok = push_fn(raw as *const _, &note.header as *const _);
        assert!(ok);
    }

    assert_eq!(list.len(), 1);
    let midi_out = list.to_midi_events();
    assert_eq!(midi_out.len(), 1);
    let event = &midi_out[0];
    assert!(event.is_note_on());
    assert_eq!(event.note(), Some(64));
    assert_eq!(event.frame_offset, 50);
}

/// Output generic `Midi` (CC) events survive the FFI round-trip.
#[test]
fn test_output_event_list_push_midi_cc() {
    use clap_sys::events::clap_event_midi;
    let mut list = OutputEventList::new();
    let raw = list.as_raw_mut();

    let midi = clap_event_midi {
        header: clap_event_header {
            size: std::mem::size_of::<clap_event_midi>() as u32,
            time: 33,
            space_id: CLAP_CORE_EVENT_SPACE_ID,
            type_: CLAP_EVENT_MIDI,
            flags: 0,
        },
        port_index: 0,
        data: [0xB2, 74, 100],
    };

    unsafe {
        let push_fn = (*raw).try_push.unwrap();
        assert!(push_fn(raw as *const _, &midi.header as *const _));
    }

    let midi_out = list.to_midi_events();
    assert_eq!(midi_out.len(), 1);
    let event = &midi_out[0];
    assert_eq!(event.frame_offset, 33);
    // Round-tripping a CC through tutti-midi produces a UMP CV1 event;
    // decoding back to wire bytes yields the original CC payload.
    let (bytes, len) = event.to_midi1_bytes().expect("CC should round-trip");
    assert_eq!(len, 3);
    assert_eq!(bytes, [0xB2, 74, 100]);
}

/// Events added to the input list should sort by frame offset after
/// calling `sort_by_time()`.
#[test]
fn test_input_event_list_sort_by_time() {
    let mut list = InputEventList::new();
    list.add_midi(&MidiEvent::note_on(0, 0, 60, 0x8000).with_frame_offset(200));
    list.add_midi(&MidiEvent::note_on(0, 0, 64, 0x5000).with_frame_offset(50));
    list.add_midi(&MidiEvent::note_on(0, 0, 67, 0x7000).with_frame_offset(100));
    list.sort_by_time();

    let events = list.events();
    assert_eq!(events[0].header().time, 50);
    assert_eq!(events[1].header().time, 100);
    assert_eq!(events[2].header().time, 200);
}

#[test]
fn test_output_event_list_push_param_value() {
    let mut list = OutputEventList::new();
    let raw = list.as_raw_mut();

    let param = clap_event_param_value {
        header: clap_event_header {
            size: std::mem::size_of::<clap_event_param_value>() as u32,
            time: 0,
            space_id: CLAP_CORE_EVENT_SPACE_ID,
            type_: CLAP_EVENT_PARAM_VALUE,
            flags: 0,
        },
        param_id: 7,
        cookie: std::ptr::null_mut(),
        note_id: -1,
        port_index: -1,
        channel: -1,
        key: -1,
        value: 0.42,
    };

    unsafe {
        let push_fn = (*raw).try_push.unwrap();
        push_fn(raw as *const _, &param.header as *const _);
    }

    let changes = list.to_param_changes();
    assert_eq!(changes.queues.len(), 1);
    assert_eq!(changes.queues[0].param_id, 7);
    assert!((changes.queues[0].points[0].value - 0.42).abs() < 0.001);
}

// sort_by_time test removed alongside other MIDI-shape-dependent tests.

// ── Parameter changes grouping ──

#[test]
fn test_param_changes_through_input_list() {
    let mut changes = ParameterChanges::new();
    let mut q1 = ParameterQueue::new(1);
    q1.add_point(0, 0.0);
    q1.add_point(128, 1.0);
    changes.add_queue(q1);

    let mut q2 = ParameterQueue::new(2);
    q2.add_point(64, 0.5);
    changes.add_queue(q2);

    let mut list = InputEventList::new();
    // Empty range map → pass-through (this test only asserts event count, not
    // denormalized values; the denorm math is covered in the events unit tests).
    list.add_param_changes(&changes, &[]);

    assert_eq!(list.len(), 3);
}

// ── Note expression through event list ──

#[test]
fn test_note_expression_roundtrip_through_output_list() {
    let mut list = OutputEventList::new();
    let raw = list.as_raw_mut();

    let expr = clap_event_note_expression {
        header: clap_event_header {
            size: std::mem::size_of::<clap_event_note_expression>() as u32,
            time: 10,
            space_id: CLAP_CORE_EVENT_SPACE_ID,
            type_: CLAP_EVENT_NOTE_EXPRESSION,
            flags: 0,
        },
        expression_id: clap_sys::events::CLAP_NOTE_EXPRESSION_TUNING,
        note_id: 5,
        port_index: 0,
        channel: 0,
        key: 60,
        value: 0.25,
    };

    unsafe {
        let push_fn = (*raw).try_push.unwrap();
        push_fn(raw as *const _, &expr.header as *const _);
    }

    let expressions = list.to_note_expressions();
    assert_eq!(expressions.len(), 1);
    assert_eq!(expressions[0].expression_type, NoteExpressionType::Tuning);
    assert_eq!(expressions[0].note_id, 5);
    assert!((expressions[0].value - 0.25).abs() < 0.001);
}

// ── Stream I/O ──

#[test]
fn test_output_stream_write_and_read_back() {
    let mut stream = OutputStream::new();
    let raw = stream.as_raw();

    let data = b"CLAP state data here";
    unsafe {
        let write_fn = (*raw).write.unwrap();
        let written = write_fn(raw, data.as_ptr() as *const c_void, data.len() as u64);
        assert_eq!(written, data.len() as i64);
    }

    assert_eq!(stream.data(), data);
}

#[test]
fn test_input_stream_read() {
    let data = b"saved plugin state";
    let mut stream = InputStream::new(data);
    let raw = stream.as_raw();

    let mut buf = [0u8; 32];
    unsafe {
        let read_fn = (*raw).read.unwrap();
        let bytes_read = read_fn(raw, buf.as_mut_ptr() as *mut c_void, 32);
        assert_eq!(bytes_read, data.len() as i64);
        assert_eq!(&buf[..data.len()], data);
    }

    assert_eq!(stream.position(), data.len());
    assert_eq!(stream.remaining(), 0);
}

#[test]
fn test_input_stream_partial_reads() {
    let data = b"ABCDEFGHIJ";
    let mut stream = InputStream::new(data);
    let raw = stream.as_raw();

    let mut buf = [0u8; 4];
    unsafe {
        let read_fn = (*raw).read.unwrap();

        let n = read_fn(raw, buf.as_mut_ptr() as *mut c_void, 4);
        assert_eq!(n, 4);
        assert_eq!(&buf, b"ABCD");

        let n = read_fn(raw, buf.as_mut_ptr() as *mut c_void, 4);
        assert_eq!(n, 4);
        assert_eq!(&buf, b"EFGH");

        let n = read_fn(raw, buf.as_mut_ptr() as *mut c_void, 4);
        assert_eq!(n, 2);
        assert_eq!(&buf[..2], b"IJ");

        let n = read_fn(raw, buf.as_mut_ptr() as *mut c_void, 4);
        assert_eq!(n, 0);
    }
}

// ── Host ──

#[test]
fn test_clap_host_as_raw_not_null() {
    let host = ClapHost::default();
    assert!(!host.as_raw().is_null());
}

// ── Mixed event types through output ──

#[test]
fn test_output_list_filters_midi_from_mixed_events() {
    let mut list = OutputEventList::new();
    let raw = list.as_raw_mut();

    // Push a note on
    let note = clap_event_note {
        header: clap_event_header {
            size: std::mem::size_of::<clap_event_note>() as u32,
            time: 0,
            space_id: CLAP_CORE_EVENT_SPACE_ID,
            type_: CLAP_EVENT_NOTE_ON,
            flags: 0,
        },
        note_id: -1,
        port_index: 0,
        channel: 0,
        key: 60,
        velocity: 0.8,
    };

    // Push a param value
    let param = clap_event_param_value {
        header: clap_event_header {
            size: std::mem::size_of::<clap_event_param_value>() as u32,
            time: 10,
            space_id: CLAP_CORE_EVENT_SPACE_ID,
            type_: CLAP_EVENT_PARAM_VALUE,
            flags: 0,
        },
        param_id: 1,
        cookie: std::ptr::null_mut(),
        note_id: -1,
        port_index: -1,
        channel: -1,
        key: -1,
        value: 0.5,
    };

    unsafe {
        let push_fn = (*raw).try_push.unwrap();
        push_fn(raw as *const _, &note.header as *const _);
        push_fn(raw as *const _, &param.header as *const _);
    }

    assert_eq!(list.len(), 2);
    // to_param_changes should only return the param, not the note
    assert_eq!(list.to_param_changes().queues.len(), 1);
}

// ── Phase 1: New event types through output list FFI ──

#[test]
fn test_output_list_push_note_choke() {
    let mut list = OutputEventList::new();
    let raw = list.as_raw_mut();

    let note = clap_event_note {
        header: clap_event_header {
            size: std::mem::size_of::<clap_event_note>() as u32,
            time: 30,
            space_id: CLAP_CORE_EVENT_SPACE_ID,
            type_: CLAP_EVENT_NOTE_CHOKE,
            flags: 0,
        },
        note_id: 42,
        port_index: 0,
        channel: 2,
        key: 60,
        velocity: 0.0,
    };

    unsafe {
        let push_fn = (*raw).try_push.unwrap();
        assert!(push_fn(raw as *const _, &note.header as *const _));
    }

    assert_eq!(list.len(), 1);
    let events = list.events();
    match &events[0] {
        ClapEvent::NoteChoke(e) => {
            assert_eq!(e.header.time, 30);
            assert_eq!(e.note_id, 42);
            assert_eq!(e.channel, 2);
            assert_eq!(e.key, 60);
        }
        _ => panic!("Expected NoteChoke"),
    }
}

#[test]
fn test_output_list_push_note_end() {
    let mut list = OutputEventList::new();
    let raw = list.as_raw_mut();

    let note = clap_event_note {
        header: clap_event_header {
            size: std::mem::size_of::<clap_event_note>() as u32,
            time: 99,
            space_id: CLAP_CORE_EVENT_SPACE_ID,
            type_: CLAP_EVENT_NOTE_END,
            flags: 0,
        },
        note_id: 7,
        port_index: 0,
        channel: 0,
        key: 72,
        velocity: 0.0,
    };

    unsafe {
        let push_fn = (*raw).try_push.unwrap();
        assert!(push_fn(raw as *const _, &note.header as *const _));
    }

    assert_eq!(list.len(), 1);
    let events = list.events();
    match &events[0] {
        ClapEvent::NoteEnd(e) => {
            assert_eq!(e.header.time, 99);
            assert_eq!(e.note_id, 7);
            assert_eq!(e.key, 72);
        }
        _ => panic!("Expected NoteEnd"),
    }
}

#[test]
fn test_output_list_push_param_gesture_begin_end() {
    let mut list = OutputEventList::new();
    let raw = list.as_raw_mut();

    let begin = clap_event_param_gesture {
        header: clap_event_header {
            size: std::mem::size_of::<clap_event_param_gesture>() as u32,
            time: 0,
            space_id: CLAP_CORE_EVENT_SPACE_ID,
            type_: CLAP_EVENT_PARAM_GESTURE_BEGIN,
            flags: 0,
        },
        param_id: 5,
    };

    let end = clap_event_param_gesture {
        header: clap_event_header {
            size: std::mem::size_of::<clap_event_param_gesture>() as u32,
            time: 100,
            space_id: CLAP_CORE_EVENT_SPACE_ID,
            type_: CLAP_EVENT_PARAM_GESTURE_END,
            flags: 0,
        },
        param_id: 5,
    };

    unsafe {
        let push_fn = (*raw).try_push.unwrap();
        assert!(push_fn(raw as *const _, &begin.header as *const _));
        assert!(push_fn(raw as *const _, &end.header as *const _));
    }

    assert_eq!(list.len(), 2);

    let events = list.events();
    match &events[0] {
        ClapEvent::ParamGestureBegin(e) => {
            assert_eq!(e.header.time, 0);
            assert_eq!(e.param_id, 5);
        }
        _ => panic!("Expected ParamGestureBegin"),
    }
    match &events[1] {
        ClapEvent::ParamGestureEnd(e) => {
            assert_eq!(e.header.time, 100);
            assert_eq!(e.param_id, 5);
        }
        _ => panic!("Expected ParamGestureEnd"),
    }

    // Gesture events should not appear as param changes
    assert_eq!(list.to_param_changes().queues.len(), 0);
}

#[test]
fn test_output_list_push_unknown_event_returns_false() {
    let mut list = OutputEventList::new();
    let raw = list.as_raw_mut();

    let header = clap_event_header {
        size: std::mem::size_of::<clap_event_header>() as u32,
        time: 0,
        space_id: CLAP_CORE_EVENT_SPACE_ID,
        type_: 9999,
        flags: 0,
    };

    unsafe {
        let push_fn = (*raw).try_push.unwrap();
        assert!(!push_fn(raw as *const _, &header as *const _));
    }

    assert_eq!(list.len(), 0);
}

#[test]
fn test_output_list_push_null_event_returns_false() {
    let mut list = OutputEventList::new();
    let raw = list.as_raw_mut();

    unsafe {
        let push_fn = (*raw).try_push.unwrap();
        assert!(!push_fn(raw as *const _, std::ptr::null()));
    }

    assert_eq!(list.len(), 0);
}

// ── Transport constants ──

#[test]
fn test_transport_event_type_constant() {
    use clap_sys::events::CLAP_EVENT_TRANSPORT;
    // CLAP_EVENT_TRANSPORT is defined as 9 in the spec but we should use the constant
    assert_eq!(CLAP_EVENT_TRANSPORT, 9);
}

#[test]
fn test_transport_flags_are_distinct_bits() {
    use clap_sys::events::{
        CLAP_TRANSPORT_HAS_BEATS_TIMELINE, CLAP_TRANSPORT_HAS_SECONDS_TIMELINE,
        CLAP_TRANSPORT_HAS_TEMPO, CLAP_TRANSPORT_HAS_TIME_SIGNATURE, CLAP_TRANSPORT_IS_LOOP_ACTIVE,
        CLAP_TRANSPORT_IS_PLAYING, CLAP_TRANSPORT_IS_RECORDING,
    };

    let all_flags = [
        CLAP_TRANSPORT_HAS_TEMPO,
        CLAP_TRANSPORT_HAS_BEATS_TIMELINE,
        CLAP_TRANSPORT_HAS_SECONDS_TIMELINE,
        CLAP_TRANSPORT_HAS_TIME_SIGNATURE,
        CLAP_TRANSPORT_IS_PLAYING,
        CLAP_TRANSPORT_IS_RECORDING,
        CLAP_TRANSPORT_IS_LOOP_ACTIVE,
    ];

    // Verify all flags are distinct powers of 2
    for flag in &all_flags {
        assert!(flag.is_power_of_two(), "Flag {flag:#x} is not a power of 2");
    }

    // Verify no two flags are equal
    for i in 0..all_flags.len() {
        for j in (i + 1)..all_flags.len() {
            assert_ne!(all_flags[i], all_flags[j]);
        }
    }
}

#[test]
#[allow(clippy::assertions_on_constants)]
fn test_fixedpoint_factors_nonzero() {
    use clap_sys::fixedpoint::{CLAP_BEATTIME_FACTOR, CLAP_SECTIME_FACTOR};
    assert!(CLAP_BEATTIME_FACTOR > 0);
    assert!(CLAP_SECTIME_FACTOR > 0);
}

// ── Phase 5: Host state + extensions ──

#[test]
fn test_host_state_poll_clears_flag() {
    use std::sync::atomic::Ordering;

    let state = HostState::new();
    state
        .lifecycle
        .restart_requested
        .store(true, Ordering::Release);

    // First poll returns true and clears
    assert!(state.poll(&state.lifecycle.restart_requested));
    // Second poll returns false
    assert!(!state.poll(&state.lifecycle.restart_requested));
}

#[test]
fn test_host_state_all_flags_start_false() {
    use std::sync::atomic::Ordering;

    let state = HostState::new();
    assert!(!state.lifecycle.restart_requested.load(Ordering::Acquire));
    assert!(!state.lifecycle.process_requested.load(Ordering::Acquire));
    assert!(!state.lifecycle.callback_requested.load(Ordering::Acquire));
    assert!(!state.processing.latency_changed.load(Ordering::Acquire));
    assert!(!state.processing.tail_changed.load(Ordering::Acquire));
    assert!(!state.params.rescan_requested.load(Ordering::Acquire));
    assert!(!state.params.flush_requested.load(Ordering::Acquire));
    assert!(!state.audio_ports.changed.load(Ordering::Acquire));
    assert!(!state.notes.ports_changed.load(Ordering::Acquire));
    assert!(!state.processing.state_dirty.load(Ordering::Acquire));
    assert!(!state.gui.closed.load(Ordering::Acquire));
}

#[test]
fn test_host_state_main_thread_id_is_current() {
    let state = HostState::new();
    assert_eq!(state.main_thread_id, std::thread::current().id());
}

#[test]
fn test_clap_host_stores_host_data() {
    let host = ClapHost::default();
    let raw = host.as_raw();
    // host_data should point to the HostState
    assert!(!unsafe { (*raw).host_data }.is_null());
}

#[test]
fn host_gui_closed_was_destroyed_latches_already_destroyed() {
    // H5: gui.closed(was_destroyed = true) must record that the plugin already
    // tore its own editor down, so a later close_editor skips gui.destroy.
    // With was_destroyed = false the latch stays clear.
    use clap_sys::ext::gui::{clap_host_gui, CLAP_EXT_GUI};
    use std::sync::atomic::Ordering;

    let host = ClapHost::default();
    let raw = host.as_raw();
    let get_ext = unsafe { (*raw).get_extension.unwrap() };
    let gui_ext = unsafe { get_ext(raw, CLAP_EXT_GUI.as_ptr()) as *const clap_host_gui };
    assert!(!gui_ext.is_null());
    let closed = unsafe { (*gui_ext).closed.unwrap() };

    // was_destroyed = false: closed flag set, but no already_destroyed latch.
    unsafe { closed(raw, false) };
    assert!(host.state().gui.closed.load(Ordering::Acquire));
    assert!(
        !host.state().gui.already_destroyed.load(Ordering::Acquire),
        "was_destroyed=false must not latch already_destroyed"
    );

    // was_destroyed = true: latch set — close_editor will skip hide/destroy.
    unsafe { closed(raw, true) };
    assert!(
        host.state().gui.already_destroyed.load(Ordering::Acquire),
        "was_destroyed=true must latch already_destroyed"
    );

    // Emulate close_editor consuming the latch (swap → false).
    let was = host
        .state()
        .gui
        .already_destroyed
        .swap(false, Ordering::AcqRel);
    assert!(was, "latch was set");
    assert!(!host.state().gui.already_destroyed.load(Ordering::Acquire));
}

#[test]
fn test_host_get_extension_returns_non_null_for_supported() {
    use clap_sys::ext::audio_ports::CLAP_EXT_AUDIO_PORTS;
    use clap_sys::ext::gui::CLAP_EXT_GUI;
    use clap_sys::ext::latency::CLAP_EXT_LATENCY;
    use clap_sys::ext::log::CLAP_EXT_LOG;
    use clap_sys::ext::note_ports::CLAP_EXT_NOTE_PORTS;
    use clap_sys::ext::params::CLAP_EXT_PARAMS;
    use clap_sys::ext::state::CLAP_EXT_STATE;
    use clap_sys::ext::tail::CLAP_EXT_TAIL;
    use clap_sys::ext::thread_check::CLAP_EXT_THREAD_CHECK;

    let host = ClapHost::default();
    let raw = host.as_raw();
    let get_ext = unsafe { (*raw).get_extension.unwrap() };

    let supported = [
        CLAP_EXT_THREAD_CHECK,
        CLAP_EXT_LOG,
        CLAP_EXT_PARAMS,
        CLAP_EXT_STATE,
        CLAP_EXT_LATENCY,
        CLAP_EXT_TAIL,
        CLAP_EXT_GUI,
        CLAP_EXT_AUDIO_PORTS,
        CLAP_EXT_NOTE_PORTS,
    ];

    for ext_id in &supported {
        let ptr = unsafe { get_ext(raw, ext_id.as_ptr()) };
        assert!(!ptr.is_null(), "Extension {:?} returned null", ext_id);
    }
}

#[test]
fn test_host_get_extension_returns_null_for_unknown() {
    let host = ClapHost::default();
    let raw = host.as_raw();
    let get_ext = unsafe { (*raw).get_extension.unwrap() };

    let ptr = unsafe { get_ext(raw, c"clap.nonexistent".as_ptr()) };
    assert!(ptr.is_null());
}

#[test]
fn test_host_get_extension_returns_null_for_null_id() {
    let host = ClapHost::default();
    let raw = host.as_raw();
    let get_ext = unsafe { (*raw).get_extension.unwrap() };

    let ptr = unsafe { get_ext(raw, std::ptr::null()) };
    assert!(ptr.is_null());
}

#[test]
fn test_host_request_restart_sets_flag() {
    use std::sync::Arc;

    let state = Arc::new(HostState::new());
    let host = ClapHost::new(state.clone());
    let raw = host.as_raw();
    let request_restart = unsafe { (*raw).request_restart.unwrap() };

    assert!(!state.poll(&state.lifecycle.restart_requested));
    unsafe { request_restart(raw) };
    assert!(state.poll(&state.lifecycle.restart_requested));
}

#[test]
fn test_host_request_process_sets_flag() {
    use std::sync::Arc;

    let state = Arc::new(HostState::new());
    let host = ClapHost::new(state.clone());
    let raw = host.as_raw();
    let request_process = unsafe { (*raw).request_process.unwrap() };

    unsafe { request_process(raw) };
    assert!(state.poll(&state.lifecycle.process_requested));
}

#[test]
fn test_host_request_callback_sets_flag() {
    use std::sync::Arc;

    let state = Arc::new(HostState::new());
    let host = ClapHost::new(state.clone());
    let raw = host.as_raw();
    let request_callback = unsafe { (*raw).request_callback.unwrap() };

    unsafe { request_callback(raw) };
    assert!(state.poll(&state.lifecycle.callback_requested));
}

#[test]
fn test_host_thread_check_main_thread() {
    use clap_sys::ext::thread_check::clap_host_thread_check;

    let host = ClapHost::default();
    let raw = host.as_raw();
    let get_ext = unsafe { (*raw).get_extension.unwrap() };
    let tc_ptr = unsafe { get_ext(raw, c"clap.thread-check".as_ptr()) };
    let tc = unsafe { &*(tc_ptr as *const clap_host_thread_check) };

    // Current thread should be the main thread
    assert!(unsafe { tc.is_main_thread.unwrap()(raw) });
    // Current thread should NOT be audio thread
    assert!(!unsafe { tc.is_audio_thread.unwrap()(raw) });
}

#[test]
fn test_host_thread_check_audio_thread() {
    use clap_sys::ext::thread_check::clap_host_thread_check;
    use std::sync::Arc;

    let state = Arc::new(HostState::new());
    let state2 = Arc::clone(&state);
    let host = ClapHost::new(state);
    let raw = host.as_raw();
    let get_ext = unsafe { (*raw).get_extension.unwrap() };
    let tc_ptr = unsafe { get_ext(raw, c"clap.thread-check".as_ptr()) };
    let tc = unsafe { &*(tc_ptr as *const clap_host_thread_check) };

    let raw_val = raw as usize;
    let is_main = tc.is_main_thread.unwrap();
    let is_audio = tc.is_audio_thread.unwrap();

    // A random thread should NOT be the audio thread (no audio_thread_id set)
    let handle = std::thread::spawn(move || {
        let raw = raw_val as *const clap_sys::host::clap_host;
        let main = unsafe { is_main(raw) };
        let audio = unsafe { is_audio(raw) };
        (main, audio)
    });

    let (main, audio) = handle.join().unwrap();
    assert!(!main, "Other thread should not be main");
    assert!(
        !audio,
        "Random thread should not be audio (no audio_thread_id set)"
    );

    // Set the audio_thread_id to the current thread, then check
    state2
        .audio_thread_id
        .store(Some(std::sync::Arc::new(std::thread::current().id())));
    assert!(
        unsafe { is_audio(raw) },
        "Current thread should be audio after setting audio_thread_id"
    );
}

// ── Phase 8: Final tests ──

#[test]
fn test_param_value_event_construction() {
    let event = ClapEvent::param_value(42, 7, 0.75);
    let header = event.header();
    assert_eq!(header.time, 42);
    match &event {
        ClapEvent::ParamValue(e) => {
            assert_eq!(e.param_id, 7);
            assert!((e.value - 0.75).abs() < f64::EPSILON);
        }
        _ => panic!("Expected ParamValue"),
    }
}

#[test]
fn test_output_event_list_take_events() {
    use clap_sys::events::{CLAP_CORE_EVENT_SPACE_ID, CLAP_EVENT_NOTE_ON};

    let mut list = OutputEventList::new();
    let raw = list.as_raw_mut();

    let note = clap_event_note {
        header: clap_event_header {
            size: std::mem::size_of::<clap_event_note>() as u32,
            time: 0,
            space_id: CLAP_CORE_EVENT_SPACE_ID,
            type_: CLAP_EVENT_NOTE_ON,
            flags: 0,
        },
        note_id: -1,
        port_index: 0,
        channel: 0,
        key: 60,
        velocity: 0.8,
    };

    unsafe {
        let push_fn = (*raw).try_push.unwrap();
        push_fn(raw as *const _, &note.header as *const _);
    }

    assert_eq!(list.len(), 1);
    let taken = list.take_events();
    assert_eq!(taken.len(), 1);
    assert_eq!(list.len(), 0); // Should be empty after take
}

#[test]
fn test_input_event_list_from_events() {
    let events = vec![
        ClapEvent::param_value(10, 1, 0.5),
        ClapEvent::param_value(5, 2, 0.3),
    ];

    let mut list = tutti_clap_host::InputEventList::from_events(events);
    assert_eq!(list.len(), 2);

    list.sort_by_time();
    // After sorting, event at time 5 should be first
    let raw = list.as_raw();
    unsafe {
        let size_fn = (*raw).size.unwrap();
        assert_eq!(size_fn(raw), 2);

        let get_fn = (*raw).get.unwrap();
        let first = get_fn(raw, 0);
        assert_eq!((*first).time, 5);
        let second = get_fn(raw, 1);
        assert_eq!((*second).time, 10);
    }
}

#[test]
fn test_smallvec_parameter_queue() {
    let mut queue = ParameterQueue::new(42);
    // SmallVec<[ParameterPoint; 8]> should handle 8 points without heap allocation
    for i in 0..8 {
        queue.add_point(i, i as f64 * 0.1);
    }
    assert_eq!(queue.points.len(), 8);
    assert_eq!(queue.param_id, 42);
    assert!((queue.points[3].value - 0.3).abs() < f64::EPSILON);
    assert_eq!(queue.points[3].sample_offset, 3);
}

#[test]
fn test_state_context_enum() {
    use tutti_clap_host::StateContext;

    // Ensure the enum variants are distinct
    assert_ne!(StateContext::ForPreset, StateContext::ForProject);
    assert_ne!(StateContext::ForProject, StateContext::ForDuplicate);
    assert_ne!(StateContext::ForPreset, StateContext::ForDuplicate);
}

#[test]
fn test_audio_port_info_types() {
    use tutti_clap_host::{AudioPortFlags, AudioPortInfo, ChannelLayout};

    let port = AudioPortInfo {
        id: 0,
        name: "Main".to_string(),
        layout: ChannelLayout::Stereo,
        flags: AudioPortFlags::MAIN | AudioPortFlags::SUPPORTS_64BIT,
        in_place_pair_id: u32::MAX,
    };

    assert!(port.flags.contains(AudioPortFlags::MAIN));
    assert_eq!(port.layout, ChannelLayout::Stereo);
    assert_eq!(port.layout.count(), 2);
}

#[test]
fn test_note_port_info_types() {
    use tutti_clap_host::{NoteDialect, NoteDialects, NotePortInfo};

    let port = NotePortInfo {
        id: 0,
        name: "MIDI In".to_string(),
        supported_dialects: NoteDialects::CLAP | NoteDialects::MIDI,
        preferred_dialect: NoteDialect::Midi,
    };

    assert!(port.supported_dialects.contains(NoteDialects::MIDI));
    assert!(!port.supported_dialects.contains(NoteDialects::MIDI_MPE));
    assert_eq!(port.preferred_dialect, NoteDialect::Midi);
}

#[test]
fn test_audio_port_config_type() {
    use tutti_clap_host::AudioPortConfig;

    let config = AudioPortConfig {
        id: 1,
        name: "Stereo".to_string(),
        input_port_count: 1,
        output_port_count: 1,
        has_main_input: true,
        main_input_channel_count: 2,
        has_main_output: true,
        main_output_channel_count: 2,
    };

    assert_eq!(config.main_output_channel_count, 2);
    assert!(config.has_main_output);
}

#[test]
fn test_transport_info_builder() {
    use tutti_clap_host::TransportInfo;
    use tutti_plugin_types::{BeatsPerBar, NoteValue, TimeSignature};

    let transport = TransportInfo::new()
        .with_tempo(140.0)
        .with_playing(true)
        .with_recording(true)
        .with_time_signature(TimeSignature::new(BeatsPerBar::new(3), NoteValue::QUARTER))
        .with_position_beats(8.0, 3.5)
        .with_loop(true, 4.0, 16.0);

    assert!((transport.timing.tempo - 140.0).abs() < f64::EPSILON);
    assert!(transport.state.playing);
    assert!(transport.state.recording);
    assert_eq!(u32::from(transport.timing.signature.beats_per_bar()), 3);
    assert_eq!(u32::from(transport.timing.signature.note_value()), 4);
    assert!((transport.position.beats - 8.0).abs() < f64::EPSILON);
    assert!((transport.position.seconds - 3.5).abs() < f64::EPSILON);
    assert!(transport.state.cycle_active);
    assert!((transport.loop_region.start_beats - 4.0).abs() < f64::EPSILON);
    assert!((transport.loop_region.end_beats - 16.0).abs() < f64::EPSILON);
}

// ── Remaining plan items: voice info, note name, sysex, timer ──

#[test]
fn test_voice_info_type() {
    let info = VoiceInfo {
        voice_count: 16,
        voice_capacity: 32,
        supports_overlapping_notes: true,
    };
    assert_eq!(info.voice_count, 16);
    assert_eq!(info.voice_capacity, 32);
    assert!(info.supports_overlapping_notes);
}

#[test]
fn test_note_name_type() {
    let nn = NoteName {
        name: "C4".to_string(),
        port: 0,
        channel: -1,
        key: 60,
    };
    assert_eq!(nn.name, "C4");
    assert_eq!(nn.key, 60);
    assert_eq!(nn.channel, -1);
}

#[test]
fn test_output_list_push_midi_sysex() {
    use clap_sys::events::{
        clap_event_midi_sysex, CLAP_CORE_EVENT_SPACE_ID, CLAP_EVENT_MIDI_SYSEX,
    };

    let mut list = OutputEventList::new();
    let raw = list.as_raw_mut();

    let sysex_data: Vec<u8> = vec![0xF0, 0x7E, 0x7F, 0x09, 0x01, 0xF7];
    let sysex = clap_event_midi_sysex {
        header: clap_event_header {
            size: std::mem::size_of::<clap_event_midi_sysex>() as u32,
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
        let push_fn = (*raw).try_push.unwrap();
        assert!(push_fn(raw as *const _, &sysex.header as *const _));
    }

    assert_eq!(list.len(), 1);
    let events = list.events();
    match &events[0] {
        ClapEvent::MidiSysex { inner, _data } => {
            assert_eq!(inner.header.time, 25);
            assert_eq!(inner.port_index, 0);
            assert_eq!(_data, &sysex_data);
        }
        _ => panic!("Expected MidiSysex"),
    }
    // Sysex should not appear as regular MIDI events
    assert_eq!(list.to_midi_events().len(), 0);
}

#[test]
fn test_host_timer_support_extension_available() {
    use clap_sys::ext::timer_support::CLAP_EXT_TIMER_SUPPORT;

    let host = ClapHost::default();
    let raw = host.as_raw();
    let get_ext = unsafe { (*raw).get_extension.unwrap() };

    let ptr = unsafe { get_ext(raw, CLAP_EXT_TIMER_SUPPORT.as_ptr()) };
    assert!(!ptr.is_null());
}

#[test]
fn test_host_timer_register_unregister() {
    use clap_sys::ext::timer_support::{clap_host_timer_support, CLAP_EXT_TIMER_SUPPORT};
    use std::sync::Arc;

    let state = Arc::new(HostState::new());
    let host = ClapHost::new(state.clone());
    let raw = host.as_raw();
    let get_ext = unsafe { (*raw).get_extension.unwrap() };
    let ts_ptr = unsafe { get_ext(raw, CLAP_EXT_TIMER_SUPPORT.as_ptr()) };
    let ts = unsafe { &*(ts_ptr as *const clap_host_timer_support) };

    let mut timer_id: u32 = 0;
    let ok = unsafe { ts.register_timer.unwrap()(raw, 100, &mut timer_id) };
    assert!(ok);
    assert!(timer_id > 0);

    // Register a second timer — should get a different ID
    let mut timer_id2: u32 = 0;
    let ok = unsafe { ts.register_timer.unwrap()(raw, 200, &mut timer_id2) };
    assert!(ok);
    assert_ne!(timer_id, timer_id2);

    // Unregister first
    let ok = unsafe { ts.unregister_timer.unwrap()(raw, timer_id) };
    assert!(ok);

    // Unregister second
    let ok = unsafe { ts.unregister_timer.unwrap()(raw, timer_id2) };
    assert!(ok);

    // Unregister again should fail (already removed)
    let ok = unsafe { ts.unregister_timer.unwrap()(raw, timer_id) };
    assert!(!ok);
}

#[test]
fn test_needs_restart_non_clearing() {
    use std::sync::atomic::Ordering;

    let state = HostState::new();
    assert!(!state.lifecycle.restart_requested.load(Ordering::Acquire));

    state
        .lifecycle
        .restart_requested
        .store(true, Ordering::Release);

    // Non-clearing peek should return true without clearing
    assert!(state.lifecycle.restart_requested.load(Ordering::Acquire));
    assert!(state.lifecycle.restart_requested.load(Ordering::Acquire));

    // poll() should clear it
    assert!(state.poll(&state.lifecycle.restart_requested));
    assert!(!state.lifecycle.restart_requested.load(Ordering::Acquire));
}

// ── New extension type tests ──

#[test]
#[cfg(feature = "clap-extras")]
fn test_color_type() {
    use tutti_clap_host::Color;
    let c = Color {
        alpha: 255,
        red: 128,
        green: 64,
        blue: 32,
    };
    assert_eq!(c.red, 128);
    assert_eq!(c.alpha, 255);
}

#[test]
#[cfg(feature = "clap-extras")]
fn test_track_info_default() {
    use tutti_clap_host::TrackInfo;
    let info = TrackInfo::default();
    assert!(info.name.is_none());
    assert!(info.color.is_none());
    assert!(!info.is_master);
    assert!(!info.is_bus);
    assert!(!info.is_return_track);
}

#[test]
#[cfg(feature = "clap-extras")]
fn test_param_automation_state_variants() {
    use tutti_clap_host::ParamAutomationState;
    let states = [
        ParamAutomationState::None,
        ParamAutomationState::Present,
        ParamAutomationState::Playing,
        ParamAutomationState::Recording,
        ParamAutomationState::Overriding,
    ];
    assert_eq!(states.len(), 5);
    assert_ne!(ParamAutomationState::None, ParamAutomationState::Recording);
}

#[test]
#[cfg(feature = "clap-extras")]
fn test_remote_controls_page() {
    use tutti_clap_host::RemoteControlsPage;
    let page = RemoteControlsPage {
        section_name: "EQ".to_string(),
        page_id: 1,
        page_name: "Band 1".to_string(),
        param_ids: [10, 11, 12, 13, 14, 15, 16, 17],
        is_for_preset: false,
    };
    assert_eq!(page.param_ids.len(), 8);
    assert_eq!(page.section_name, "EQ");
}

#[test]
#[cfg(feature = "clap-extras")]
fn test_transport_request_variants() {
    use tutti_clap_host::TransportRequest;
    let req = TransportRequest::Jump {
        position_beats: 4.0,
    };
    assert_eq!(
        req,
        TransportRequest::Jump {
            position_beats: 4.0
        }
    );
    let req2 = TransportRequest::LoopRegion {
        start_beats: 0.0,
        duration_beats: 8.0,
    };
    assert_ne!(req, req2);
}

#[test]
#[cfg(feature = "clap-extras")]
fn test_context_menu_target() {
    use tutti_clap_host::ContextMenuTarget;
    let global = ContextMenuTarget::Global;
    let param = ContextMenuTarget::Param(42);
    assert_eq!(global, ContextMenuTarget::Global);
    assert_ne!(global, param);
}

#[test]
#[cfg(feature = "clap-extras")]
fn test_context_menu_item_variants() {
    use tutti_clap_host::ContextMenuItem;
    let items = [
        ContextMenuItem::Entry {
            label: "Cut".to_string(),
            is_enabled: true,
            action_id: 1,
        },
        ContextMenuItem::CheckEntry {
            label: "Mute".to_string(),
            is_enabled: true,
            is_checked: false,
            action_id: 2,
        },
        ContextMenuItem::Separator,
        ContextMenuItem::Title {
            title: "Options".to_string(),
            is_enabled: true,
        },
        ContextMenuItem::BeginSubmenu {
            label: "More".to_string(),
            is_enabled: true,
        },
        ContextMenuItem::EndSubmenu,
    ];
    assert_eq!(items.len(), 6);
}

#[test]
#[cfg(feature = "clap-extras")]
fn test_audio_port_config_request() {
    use tutti_clap_host::AudioPortConfigRequest;
    let req = AudioPortConfigRequest {
        is_input: false,
        port_index: 0,
        channel_count: 2,
        port_type: None,
    };
    assert!(!req.is_input);
    assert_eq!(req.channel_count, 2);
}

// ── New host extension vtable tests ──

#[test]
fn test_host_audio_ports_config_extension_available() {
    use clap_sys::ext::audio_ports_config::CLAP_EXT_AUDIO_PORTS_CONFIG;

    let host = ClapHost::default();
    let raw = host.as_raw();
    let get_ext = unsafe { (*raw).get_extension.unwrap() };
    let ptr = unsafe { get_ext(raw, CLAP_EXT_AUDIO_PORTS_CONFIG.as_ptr()) };
    assert!(!ptr.is_null());
}

#[test]
fn test_host_remote_controls_extension_available() {
    use clap_sys::ext::remote_controls::CLAP_EXT_REMOTE_CONTROLS;

    let host = ClapHost::default();
    let raw = host.as_raw();
    let get_ext = unsafe { (*raw).get_extension.unwrap() };
    let ptr = unsafe { get_ext(raw, CLAP_EXT_REMOTE_CONTROLS.as_ptr()) };
    assert!(!ptr.is_null());
}

#[test]
fn test_host_track_info_extension_available() {
    use clap_sys::ext::track_info::CLAP_EXT_TRACK_INFO;

    let host = ClapHost::default();
    let raw = host.as_raw();
    let get_ext = unsafe { (*raw).get_extension.unwrap() };
    let ptr = unsafe { get_ext(raw, CLAP_EXT_TRACK_INFO.as_ptr()) };
    assert!(!ptr.is_null());
}

#[test]
fn test_host_event_registry_extension_available() {
    use clap_sys::ext::event_registry::CLAP_EXT_EVENT_REGISTRY;

    let host = ClapHost::default();
    let raw = host.as_raw();
    let get_ext = unsafe { (*raw).get_extension.unwrap() };
    let ptr = unsafe { get_ext(raw, CLAP_EXT_EVENT_REGISTRY.as_ptr()) };
    assert!(!ptr.is_null());
}

#[test]
fn test_host_transport_control_extension_available() {
    use clap_sys::ext::draft::transport_control::CLAP_EXT_TRANSPORT_CONTROL;

    let host = ClapHost::default();
    let raw = host.as_raw();
    let get_ext = unsafe { (*raw).get_extension.unwrap() };
    let ptr = unsafe { get_ext(raw, CLAP_EXT_TRANSPORT_CONTROL.as_ptr()) };
    assert!(!ptr.is_null());
}

#[test]
fn test_host_context_menu_extension_available() {
    use clap_sys::ext::context_menu::CLAP_EXT_CONTEXT_MENU;

    let host = ClapHost::default();
    let raw = host.as_raw();
    let get_ext = unsafe { (*raw).get_extension.unwrap() };
    let ptr = unsafe { get_ext(raw, CLAP_EXT_CONTEXT_MENU.as_ptr()) };
    assert!(!ptr.is_null());
}

#[test]
fn test_host_track_info_get_returns_false_when_empty() {
    use clap_sys::ext::track_info::{clap_host_track_info, clap_track_info, CLAP_EXT_TRACK_INFO};

    let host = ClapHost::default();
    let raw = host.as_raw();
    let get_ext = unsafe { (*raw).get_extension.unwrap() };
    let ti_ptr = unsafe { get_ext(raw, CLAP_EXT_TRACK_INFO.as_ptr()) };
    let ti = unsafe { &*(ti_ptr as *const clap_host_track_info) };

    // No track info set yet — should return false
    let mut info: clap_track_info = unsafe { std::mem::zeroed() };
    let ok = unsafe { ti.get.unwrap()(raw, &mut info) };
    assert!(!ok);
}

#[test]
fn test_host_event_registry_query() {
    use clap_sys::ext::event_registry::{clap_host_event_registry, CLAP_EXT_EVENT_REGISTRY};
    use std::sync::Arc;

    let state = Arc::new(HostState::new());
    let host = ClapHost::new(state.clone());
    let raw = host.as_raw();
    let get_ext = unsafe { (*raw).get_extension.unwrap() };
    let er_ptr = unsafe { get_ext(raw, CLAP_EXT_EVENT_REGISTRY.as_ptr()) };
    let er = unsafe { &*(er_ptr as *const clap_host_event_registry) };

    let name = c"com.example.my-events";
    let mut space_id: u16 = 0;
    let ok = unsafe { er.query.unwrap()(raw, name.as_ptr(), &mut space_id) };
    assert!(ok);
    assert!(space_id >= 512);

    // Same name should return same ID
    let mut space_id2: u16 = 0;
    let ok = unsafe { er.query.unwrap()(raw, name.as_ptr(), &mut space_id2) };
    assert!(ok);
    assert_eq!(space_id, space_id2);

    // Different name should return different ID
    let name2 = c"com.other.events";
    let mut space_id3: u16 = 0;
    let ok = unsafe { er.query.unwrap()(raw, name2.as_ptr(), &mut space_id3) };
    assert!(ok);
    assert_ne!(space_id, space_id3);
}

#[test]
fn test_host_transport_control_callbacks_exist() {
    use clap_sys::ext::draft::transport_control::{
        clap_host_transport_control, CLAP_EXT_TRANSPORT_CONTROL,
    };

    let host = ClapHost::default();
    let raw = host.as_raw();
    let get_ext = unsafe { (*raw).get_extension.unwrap() };
    let tc_ptr = unsafe { get_ext(raw, CLAP_EXT_TRANSPORT_CONTROL.as_ptr()) };
    let tc = unsafe { &*(tc_ptr as *const clap_host_transport_control) };

    // All 11 callbacks should be present
    assert!(tc.request_start.is_some());
    assert!(tc.request_stop.is_some());
    assert!(tc.request_continue.is_some());
    assert!(tc.request_pause.is_some());
    assert!(tc.request_toggle_play.is_some());
    assert!(tc.request_jump.is_some());
    assert!(tc.request_loop_region.is_some());
    assert!(tc.request_toggle_loop.is_some());
    assert!(tc.request_enable_loop.is_some());
    assert!(tc.request_record.is_some());
    assert!(tc.request_toggle_record.is_some());
}

#[test]
fn test_host_audio_ports_config_rescan() {
    use clap_sys::ext::audio_ports_config::{
        clap_host_audio_ports_config, CLAP_EXT_AUDIO_PORTS_CONFIG,
    };
    use std::sync::atomic::Ordering;
    use std::sync::Arc;

    let state = Arc::new(HostState::new());
    let host = ClapHost::new(state.clone());
    let raw = host.as_raw();
    let get_ext = unsafe { (*raw).get_extension.unwrap() };
    let apc_ptr = unsafe { get_ext(raw, CLAP_EXT_AUDIO_PORTS_CONFIG.as_ptr()) };
    let apc = unsafe { &*(apc_ptr as *const clap_host_audio_ports_config) };

    assert!(!state.audio_ports.config_changed.load(Ordering::Acquire));
    unsafe { apc.rescan.unwrap()(raw) };
    assert!(state.audio_ports.config_changed.load(Ordering::Acquire));
}

#[test]
fn test_host_remote_controls_changed_and_suggest() {
    use clap_sys::ext::remote_controls::{clap_host_remote_controls, CLAP_EXT_REMOTE_CONTROLS};
    use std::sync::atomic::Ordering;
    use std::sync::Arc;

    let state = Arc::new(HostState::new());
    let host = ClapHost::new(state.clone());
    let raw = host.as_raw();
    let get_ext = unsafe { (*raw).get_extension.unwrap() };
    let rc_ptr = unsafe { get_ext(raw, CLAP_EXT_REMOTE_CONTROLS.as_ptr()) };
    let rc = unsafe { &*(rc_ptr as *const clap_host_remote_controls) };

    assert!(!state.remote_controls.changed.load(Ordering::Acquire));
    unsafe { rc.changed.unwrap()(raw) };
    assert!(state.remote_controls.changed.load(Ordering::Acquire));

    unsafe { rc.suggest_page.unwrap()(raw, 42) };
    // suggest_page stores internally — verified by the fact the callback didn't crash
}

// ── Phase 1: thread_pool, audio_ports_activation, extensible_audio_ports ──

#[test]
fn test_host_thread_pool_extension_available() {
    use clap_sys::ext::thread_pool::CLAP_EXT_THREAD_POOL;

    let host = ClapHost::default();
    let raw = host.as_raw();
    let get_ext = unsafe { (*raw).get_extension.unwrap() };
    let ptr = unsafe { get_ext(raw, CLAP_EXT_THREAD_POOL.as_ptr()) };
    assert!(!ptr.is_null());
}

#[test]
fn test_host_thread_pool_request_exec() {
    use clap_sys::ext::thread_pool::{clap_host_thread_pool, CLAP_EXT_THREAD_POOL};
    use std::sync::Arc;

    let state = Arc::new(HostState::new());
    let host = ClapHost::new(state.clone());
    let raw = host.as_raw();
    let get_ext = unsafe { (*raw).get_extension.unwrap() };
    let tp_ptr = unsafe { get_ext(raw, CLAP_EXT_THREAD_POOL.as_ptr()) };
    let tp = unsafe { &*(tp_ptr as *const clap_host_thread_pool) };

    let ok = unsafe { tp.request_exec.unwrap()(raw, 16) };
    assert!(ok);
    // Verify it stored the task count
    assert_eq!(
        state
            .processing
            .thread_pool_pending
            .load(std::sync::atomic::Ordering::Acquire),
        16
    );
}

// ── Phase 2: ambisonic, surround ──

#[test]
fn test_ambisonic_config_type() {
    use tutti_clap_host::{AmbisonicConfig, AmbisonicNormalization, AmbisonicOrdering};
    let config = AmbisonicConfig {
        ordering: AmbisonicOrdering::Acn,
        normalization: AmbisonicNormalization::Sn3d,
    };
    assert_eq!(config.ordering, AmbisonicOrdering::Acn);
    assert_ne!(config.normalization, AmbisonicNormalization::MaxN);
}

#[test]
fn test_ambisonic_ordering_variants() {
    use tutti_clap_host::AmbisonicOrdering;
    assert_ne!(AmbisonicOrdering::Fuma, AmbisonicOrdering::Acn);
}

#[test]
fn test_ambisonic_normalization_variants() {
    use tutti_clap_host::AmbisonicNormalization;
    let all = [
        AmbisonicNormalization::MaxN,
        AmbisonicNormalization::Sn3d,
        AmbisonicNormalization::N3d,
        AmbisonicNormalization::Sn2d,
        AmbisonicNormalization::N2d,
    ];
    assert_eq!(all.len(), 5);
    // All should be distinct
    for i in 0..all.len() {
        for j in (i + 1)..all.len() {
            assert_ne!(all[i], all[j]);
        }
    }
}

#[test]
fn test_surround_channel_from_position() {
    use tutti_clap_host::SurroundChannel;
    assert_eq!(
        SurroundChannel::from_position(0),
        Some(SurroundChannel::FrontLeft)
    );
    assert_eq!(
        SurroundChannel::from_position(3),
        Some(SurroundChannel::LowFrequency)
    );
    assert_eq!(
        SurroundChannel::from_position(17),
        Some(SurroundChannel::TopBackRight)
    );
    assert_eq!(SurroundChannel::from_position(18), None);
    assert_eq!(SurroundChannel::from_position(255), None);
}

#[test]
fn test_host_ambisonic_extension_available() {
    use clap_sys::ext::ambisonic::CLAP_EXT_AMBISONIC;

    let host = ClapHost::default();
    let raw = host.as_raw();
    let get_ext = unsafe { (*raw).get_extension.unwrap() };
    let ptr = unsafe { get_ext(raw, CLAP_EXT_AMBISONIC.as_ptr()) };
    assert!(!ptr.is_null());
}

#[test]
fn test_host_surround_extension_available() {
    use clap_sys::ext::surround::CLAP_EXT_SURROUND;

    let host = ClapHost::default();
    let raw = host.as_raw();
    let get_ext = unsafe { (*raw).get_extension.unwrap() };
    let ptr = unsafe { get_ext(raw, CLAP_EXT_SURROUND.as_ptr()) };
    assert!(!ptr.is_null());
}

#[test]
fn test_host_ambisonic_changed_callback() {
    use clap_sys::ext::ambisonic::{clap_host_ambisonic, CLAP_EXT_AMBISONIC};
    use std::sync::atomic::Ordering;
    use std::sync::Arc;

    let state = Arc::new(HostState::new());
    let host = ClapHost::new(state.clone());
    let raw = host.as_raw();
    let get_ext = unsafe { (*raw).get_extension.unwrap() };
    let amb_ptr = unsafe { get_ext(raw, CLAP_EXT_AMBISONIC.as_ptr()) };
    let amb = unsafe { &*(amb_ptr as *const clap_host_ambisonic) };

    assert!(!state.audio_ports.ambisonic_changed.load(Ordering::Acquire));
    unsafe { amb.changed.unwrap()(raw) };
    assert!(state.audio_ports.ambisonic_changed.load(Ordering::Acquire));
}

#[test]
fn test_host_surround_changed_callback() {
    use clap_sys::ext::surround::{clap_host_surround, CLAP_EXT_SURROUND};
    use std::sync::atomic::Ordering;
    use std::sync::Arc;

    let state = Arc::new(HostState::new());
    let host = ClapHost::new(state.clone());
    let raw = host.as_raw();
    let get_ext = unsafe { (*raw).get_extension.unwrap() };
    let sur_ptr = unsafe { get_ext(raw, CLAP_EXT_SURROUND.as_ptr()) };
    let sur = unsafe { &*(sur_ptr as *const clap_host_surround) };

    assert!(!state.audio_ports.surround_changed.load(Ordering::Acquire));
    unsafe { sur.changed.unwrap()(raw) };
    assert!(state.audio_ports.surround_changed.load(Ordering::Acquire));
}

// ── POSIX FD support tests (unix only) ──

#[cfg(unix)]
#[test]
fn test_host_posix_fd_extension_available() {
    use clap_sys::ext::posix_fd_support::CLAP_EXT_POSIX_FD_SUPPORT;

    let host = ClapHost::default();
    let raw = host.as_raw();
    let get_ext = unsafe { (*raw).get_extension.unwrap() };
    let ptr = unsafe { get_ext(raw, CLAP_EXT_POSIX_FD_SUPPORT.as_ptr()) };
    assert!(!ptr.is_null());
}

#[cfg(unix)]
#[test]
fn test_host_posix_fd_register_and_unregister() {
    use clap_sys::ext::posix_fd_support::{
        clap_host_posix_fd_support, CLAP_EXT_POSIX_FD_SUPPORT, CLAP_POSIX_FD_READ,
    };
    use std::sync::Arc;

    let state = Arc::new(HostState::new());
    let host = ClapHost::new(state.clone());
    let raw = host.as_raw();
    let get_ext = unsafe { (*raw).get_extension.unwrap() };
    let fd_ptr = unsafe { get_ext(raw, CLAP_EXT_POSIX_FD_SUPPORT.as_ptr()) };
    let fd_ext = unsafe { &*(fd_ptr as *const clap_host_posix_fd_support) };

    // Register a file descriptor
    assert!(unsafe { fd_ext.register_fd.unwrap()(raw, 42, CLAP_POSIX_FD_READ) });

    // Duplicate registration should fail
    assert!(!unsafe { fd_ext.register_fd.unwrap()(raw, 42, CLAP_POSIX_FD_READ) });

    // Verify it was stored
    {
        let fds = state.resources.posix_fds.lock().unwrap();
        assert_eq!(fds.len(), 1);
        assert_eq!(fds[0].fd, 42);
        assert_eq!(fds[0].flags, CLAP_POSIX_FD_READ);
    }

    // Unregister
    assert!(unsafe { fd_ext.unregister_fd.unwrap()(raw, 42) });

    // Verify removed
    {
        let fds = state.resources.posix_fds.lock().unwrap();
        assert_eq!(fds.len(), 0);
    }

    // Unregister again should fail
    assert!(!unsafe { fd_ext.unregister_fd.unwrap()(raw, 42) });
}

#[cfg(unix)]
#[test]
fn test_host_posix_fd_modify() {
    use clap_sys::ext::posix_fd_support::{
        clap_host_posix_fd_support, CLAP_EXT_POSIX_FD_SUPPORT, CLAP_POSIX_FD_READ,
        CLAP_POSIX_FD_WRITE,
    };
    use std::sync::Arc;

    let state = Arc::new(HostState::new());
    let host = ClapHost::new(state.clone());
    let raw = host.as_raw();
    let get_ext = unsafe { (*raw).get_extension.unwrap() };
    let fd_ptr = unsafe { get_ext(raw, CLAP_EXT_POSIX_FD_SUPPORT.as_ptr()) };
    let fd_ext = unsafe { &*(fd_ptr as *const clap_host_posix_fd_support) };

    // Modify non-existent fd should fail
    assert!(!unsafe { fd_ext.modify_fd.unwrap()(raw, 99, CLAP_POSIX_FD_WRITE) });

    // Register then modify
    assert!(unsafe { fd_ext.register_fd.unwrap()(raw, 99, CLAP_POSIX_FD_READ) });
    assert!(unsafe {
        fd_ext.modify_fd.unwrap()(raw, 99, CLAP_POSIX_FD_READ | CLAP_POSIX_FD_WRITE)
    });

    {
        let fds = state.resources.posix_fds.lock().unwrap();
        assert_eq!(fds[0].flags, CLAP_POSIX_FD_READ | CLAP_POSIX_FD_WRITE);
    }
}

#[cfg(unix)]
#[test]
#[cfg(feature = "clap-extras")]
fn test_posix_fd_flags_type() {
    use tutti_clap_host::PosixFdFlags;

    let flags = PosixFdFlags {
        read: true,
        write: false,
        error: true,
    };
    assert!(flags.read);
    assert!(!flags.write);
    assert!(flags.error);
    assert_eq!(flags, flags);
}

// ── Triggers extension tests ──

#[test]
#[cfg(feature = "clap-extras")]
fn test_trigger_info_type() {
    use tutti_clap_host::TriggerInfo;

    let info = TriggerInfo {
        id: 1,
        flags: 0x03,
        name: "My Trigger".to_string(),
        module: "triggers/main".to_string(),
    };
    assert_eq!(info.id, 1);
    assert_eq!(info.flags, 0x03);
    assert_eq!(info.name, "My Trigger");
    assert_eq!(info.module, "triggers/main");
}

#[test]
fn test_host_triggers_extension_available() {
    use clap_sys::ext::draft::triggers::CLAP_EXT_TRIGGERS;

    let host = ClapHost::default();
    let raw = host.as_raw();
    let get_ext = unsafe { (*raw).get_extension.unwrap() };
    let ptr = unsafe { get_ext(raw, CLAP_EXT_TRIGGERS.as_ptr()) };
    assert!(!ptr.is_null());
}

#[test]
fn test_host_triggers_rescan_callback() {
    use clap_sys::ext::draft::triggers::{clap_host_triggers, CLAP_EXT_TRIGGERS};
    use std::sync::atomic::Ordering;
    use std::sync::Arc;

    let state = Arc::new(HostState::new());
    let host = ClapHost::new(state.clone());
    let raw = host.as_raw();
    let get_ext = unsafe { (*raw).get_extension.unwrap() };
    let trig_ptr = unsafe { get_ext(raw, CLAP_EXT_TRIGGERS.as_ptr()) };
    let trig = unsafe { &*(trig_ptr as *const clap_host_triggers) };

    assert!(!state
        .resources
        .triggers_rescan_requested
        .load(Ordering::Acquire));
    unsafe { trig.rescan.unwrap()(raw, 0) };
    assert!(state
        .resources
        .triggers_rescan_requested
        .load(Ordering::Acquire));
}

// ── Tuning extension tests ──

#[test]
#[cfg(feature = "clap-extras")]
fn test_tuning_info_type() {
    use tutti_clap_host::TuningInfo;

    let info = TuningInfo {
        tuning_id: 42,
        name: "Just Intonation".to_string(),
        is_dynamic: false,
    };
    assert_eq!(info.tuning_id, 42);
    assert_eq!(info.name, "Just Intonation");
    assert!(!info.is_dynamic);
}

#[test]
fn test_host_tuning_extension_available() {
    use clap_sys::ext::draft::tuning::CLAP_EXT_TUNING;

    let host = ClapHost::default();
    let raw = host.as_raw();
    let get_ext = unsafe { (*raw).get_extension.unwrap() };
    let ptr = unsafe { get_ext(raw, CLAP_EXT_TUNING.as_ptr()) };
    assert!(!ptr.is_null());
}

#[test]
fn test_host_tuning_callbacks() {
    use clap_sys::ext::draft::tuning::{clap_host_tuning, CLAP_EXT_TUNING};

    let host = ClapHost::default();
    let raw = host.as_raw();
    let get_ext = unsafe { (*raw).get_extension.unwrap() };
    let tuning_ptr = unsafe { get_ext(raw, CLAP_EXT_TUNING.as_ptr()) };
    let tuning = unsafe { &*(tuning_ptr as *const clap_host_tuning) };

    // get_relative returns 0.0 (equal temperament)
    let rel = unsafe { tuning.get_relative.unwrap()(raw, 0, 0, 60, 0) };
    assert!((rel - 0.0).abs() < f64::EPSILON);

    // should_play returns true
    assert!(unsafe { tuning.should_play.unwrap()(raw, 0, 0, 60) });

    // get_tuning_count returns 0 (no tunings configured)
    assert_eq!(unsafe { tuning.get_tuning_count.unwrap()(raw) }, 0);
}

// ── Resource directory extension tests ──

#[test]
fn test_host_resource_directory_extension_available() {
    use clap_sys::ext::draft::resource_directory::CLAP_EXT_RESOURCE_DIRECTORY;

    let host = ClapHost::default();
    let raw = host.as_raw();
    let get_ext = unsafe { (*raw).get_extension.unwrap() };
    let ptr = unsafe { get_ext(raw, CLAP_EXT_RESOURCE_DIRECTORY.as_ptr()) };
    assert!(!ptr.is_null());
}

#[test]
fn test_host_resource_directory_callbacks() {
    use clap_sys::ext::draft::resource_directory::{
        clap_host_resource_directory, CLAP_EXT_RESOURCE_DIRECTORY,
    };

    let host = ClapHost::default();
    let raw = host.as_raw();
    let get_ext = unsafe { (*raw).get_extension.unwrap() };
    let res_ptr = unsafe { get_ext(raw, CLAP_EXT_RESOURCE_DIRECTORY.as_ptr()) };
    let res = unsafe { &*(res_ptr as *const clap_host_resource_directory) };

    // No directory configured, request should return false
    assert!(!unsafe { res.request_directory.unwrap()(raw, true) });
    assert!(!unsafe { res.request_directory.unwrap()(raw, false) });

    // Release should not panic even when nothing is configured
    unsafe { res.release_directory.unwrap()(raw, true) };
    unsafe { res.release_directory.unwrap()(raw, false) };
}

// ── Undo extension tests ──

#[test]
#[cfg(feature = "clap-extras")]
fn test_undo_delta_properties_type() {
    use tutti_clap_host::UndoDeltaProperties;

    let props = UndoDeltaProperties {
        has_delta: true,
        are_deltas_persistent: false,
        format_version: 1,
    };
    assert!(props.has_delta);
    assert!(!props.are_deltas_persistent);
    assert_eq!(props.format_version, 1);
}

#[test]
#[cfg(feature = "clap-extras")]
fn test_undo_change_type() {
    use tutti_clap_host::UndoChange;

    let change = UndoChange {
        name: "Set volume".to_string(),
        delta: vec![1, 2, 3, 4],
        delta_can_undo: true,
    };
    assert_eq!(change.name, "Set volume");
    assert_eq!(change.delta.len(), 4);
    assert!(change.delta_can_undo);
}

#[test]
fn test_host_undo_extension_available() {
    use clap_sys::ext::draft::undo::CLAP_EXT_UNDO;

    let host = ClapHost::default();
    let raw = host.as_raw();
    let get_ext = unsafe { (*raw).get_extension.unwrap() };
    let ptr = unsafe { get_ext(raw, CLAP_EXT_UNDO.as_ptr()) };
    assert!(!ptr.is_null());
}

#[test]
fn test_host_undo_begin_cancel_change() {
    use clap_sys::ext::draft::undo::{clap_host_undo, CLAP_EXT_UNDO};
    use std::sync::atomic::Ordering;
    use std::sync::Arc;

    let state = Arc::new(HostState::new());
    let host = ClapHost::new(state.clone());
    let raw = host.as_raw();
    let get_ext = unsafe { (*raw).get_extension.unwrap() };
    let undo_ptr = unsafe { get_ext(raw, CLAP_EXT_UNDO.as_ptr()) };
    let undo = unsafe { &*(undo_ptr as *const clap_host_undo) };

    assert!(!state.undo.in_progress.load(Ordering::Acquire));

    unsafe { undo.begin_change.unwrap()(raw) };
    assert!(state.undo.in_progress.load(Ordering::Acquire));

    unsafe { undo.cancel_change.unwrap()(raw) };
    assert!(!state.undo.in_progress.load(Ordering::Acquire));
}

#[test]
fn test_host_undo_change_made() {
    use clap_sys::ext::draft::undo::{clap_host_undo, CLAP_EXT_UNDO};
    use std::sync::Arc;

    let state = Arc::new(HostState::new());
    let host = ClapHost::new(state.clone());
    let raw = host.as_raw();
    let get_ext = unsafe { (*raw).get_extension.unwrap() };
    let undo_ptr = unsafe { get_ext(raw, CLAP_EXT_UNDO.as_ptr()) };
    let undo = unsafe { &*(undo_ptr as *const clap_host_undo) };

    let name = c"Set volume";
    let delta = [1u8, 2, 3, 4];
    unsafe {
        undo.begin_change.unwrap()(raw);
        undo.change_made.unwrap()(
            raw,
            name.as_ptr(),
            delta.as_ptr() as *const _,
            delta.len(),
            true,
        );
    }

    let changes = state.undo.changes.lock().unwrap();
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].name, "Set volume");
    assert_eq!(changes[0].delta, vec![1, 2, 3, 4]);
    assert!(changes[0].delta_can_undo);
}

#[test]
fn test_host_undo_request_undo_redo() {
    use clap_sys::ext::draft::undo::{clap_host_undo, CLAP_EXT_UNDO};
    use std::sync::atomic::Ordering;
    use std::sync::Arc;

    let state = Arc::new(HostState::new());
    let host = ClapHost::new(state.clone());
    let raw = host.as_raw();
    let get_ext = unsafe { (*raw).get_extension.unwrap() };
    let undo_ptr = unsafe { get_ext(raw, CLAP_EXT_UNDO.as_ptr()) };
    let undo = unsafe { &*(undo_ptr as *const clap_host_undo) };

    assert!(!state.undo.requested.load(Ordering::Acquire));
    assert!(!state.undo.redo_requested.load(Ordering::Acquire));

    unsafe { undo.request_undo.unwrap()(raw) };
    assert!(state.undo.requested.load(Ordering::Acquire));

    unsafe { undo.request_redo.unwrap()(raw) };
    assert!(state.undo.redo_requested.load(Ordering::Acquire));
}

#[test]
fn test_host_undo_wants_context_updates() {
    use clap_sys::ext::draft::undo::{clap_host_undo, CLAP_EXT_UNDO};
    use std::sync::atomic::Ordering;
    use std::sync::Arc;

    let state = Arc::new(HostState::new());
    let host = ClapHost::new(state.clone());
    let raw = host.as_raw();
    let get_ext = unsafe { (*raw).get_extension.unwrap() };
    let undo_ptr = unsafe { get_ext(raw, CLAP_EXT_UNDO.as_ptr()) };
    let undo = unsafe { &*(undo_ptr as *const clap_host_undo) };

    assert!(!state.undo.wants_context.load(Ordering::Acquire));

    unsafe { undo.set_wants_context_updates.unwrap()(raw, true) };
    assert!(state.undo.wants_context.load(Ordering::Acquire));

    unsafe { undo.set_wants_context_updates.unwrap()(raw, false) };
    assert!(!state.undo.wants_context.load(Ordering::Acquire));
}

// ── Note name host extension tests ──

#[test]
fn test_host_note_name_extension_available() {
    use clap_sys::ext::note_name::CLAP_EXT_NOTE_NAME;

    let host = ClapHost::default();
    let raw = host.as_raw();
    let get_ext = unsafe { (*raw).get_extension.unwrap() };
    let ptr = unsafe { get_ext(raw, CLAP_EXT_NOTE_NAME.as_ptr()) };
    assert!(!ptr.is_null());
}

#[test]
fn test_host_note_name_changed_callback() {
    use clap_sys::ext::note_name::{clap_host_note_name, CLAP_EXT_NOTE_NAME};
    use std::sync::atomic::Ordering;
    use std::sync::Arc;

    let state = Arc::new(HostState::new());
    let host = ClapHost::new(state.clone());
    let raw = host.as_raw();
    let get_ext = unsafe { (*raw).get_extension.unwrap() };
    let nn_ptr = unsafe { get_ext(raw, CLAP_EXT_NOTE_NAME.as_ptr()) };
    let nn = unsafe { &*(nn_ptr as *const clap_host_note_name) };

    assert!(!state.notes.names_changed.load(Ordering::Acquire));
    unsafe { nn.changed.unwrap()(raw) };
    assert!(state.notes.names_changed.load(Ordering::Acquire));
}

// ── Voice info host extension tests ──

#[test]
fn test_host_voice_info_extension_available() {
    use clap_sys::ext::voice_info::CLAP_EXT_VOICE_INFO;

    let host = ClapHost::default();
    let raw = host.as_raw();
    let get_ext = unsafe { (*raw).get_extension.unwrap() };
    let ptr = unsafe { get_ext(raw, CLAP_EXT_VOICE_INFO.as_ptr()) };
    assert!(!ptr.is_null());
}

#[test]
fn test_host_voice_info_changed_callback() {
    use clap_sys::ext::voice_info::{clap_host_voice_info, CLAP_EXT_VOICE_INFO};
    use std::sync::atomic::Ordering;
    use std::sync::Arc;

    let state = Arc::new(HostState::new());
    let host = ClapHost::new(state.clone());
    let raw = host.as_raw();
    let get_ext = unsafe { (*raw).get_extension.unwrap() };
    let vi_ptr = unsafe { get_ext(raw, CLAP_EXT_VOICE_INFO.as_ptr()) };
    let vi = unsafe { &*(vi_ptr as *const clap_host_voice_info) };

    assert!(!state.notes.voice_info_changed.load(Ordering::Acquire));
    unsafe { vi.changed.unwrap()(raw) };
    assert!(state.notes.voice_info_changed.load(Ordering::Acquire));
}

// ── Preset load host extension tests ──

#[test]
fn test_host_preset_load_extension_available() {
    use clap_sys::ext::preset_load::CLAP_EXT_PRESET_LOAD;

    let host = ClapHost::default();
    let raw = host.as_raw();
    let get_ext = unsafe { (*raw).get_extension.unwrap() };
    let ptr = unsafe { get_ext(raw, CLAP_EXT_PRESET_LOAD.as_ptr()) };
    assert!(!ptr.is_null());
}

#[test]
fn test_host_preset_load_loaded_callback() {
    use clap_sys::ext::preset_load::{clap_host_preset_load, CLAP_EXT_PRESET_LOAD};
    use std::sync::atomic::Ordering;
    use std::sync::Arc;

    let state = Arc::new(HostState::new());
    let host = ClapHost::new(state.clone());
    let raw = host.as_raw();
    let get_ext = unsafe { (*raw).get_extension.unwrap() };
    let pl_ptr = unsafe { get_ext(raw, CLAP_EXT_PRESET_LOAD.as_ptr()) };
    let pl = unsafe { &*(pl_ptr as *const clap_host_preset_load) };

    assert!(!state.processing.preset_loaded.load(Ordering::Acquire));

    let location = c"/path/to/preset.clap";
    unsafe {
        pl.loaded.unwrap()(raw, 0, location.as_ptr(), std::ptr::null());
    }
    assert!(state.processing.preset_loaded.load(Ordering::Acquire));
}

#[test]
fn test_host_preset_load_on_error_doesnt_panic() {
    use clap_sys::ext::preset_load::{clap_host_preset_load, CLAP_EXT_PRESET_LOAD};

    let host = ClapHost::default();
    let raw = host.as_raw();
    let get_ext = unsafe { (*raw).get_extension.unwrap() };
    let pl_ptr = unsafe { get_ext(raw, CLAP_EXT_PRESET_LOAD.as_ptr()) };
    let pl = unsafe { &*(pl_ptr as *const clap_host_preset_load) };

    let location = c"/path/to/preset.clap";
    let msg = c"File not found";
    unsafe {
        pl.on_error.unwrap()(
            raw,
            0,
            location.as_ptr(),
            std::ptr::null(),
            -1,
            msg.as_ptr(),
        );
    }
}

// ── ParamMod event FFI ──

#[test]
fn test_param_mod_event_ffi() {
    let event = clap_event_param_mod {
        header: clap_event_header {
            size: std::mem::size_of::<clap_event_param_mod>() as u32,
            time: 42,
            space_id: CLAP_CORE_EVENT_SPACE_ID,
            type_: CLAP_EVENT_PARAM_MOD,
            flags: 0,
        },
        param_id: 123,
        cookie: std::ptr::null_mut(),
        note_id: 7,
        port_index: 0,
        channel: 1,
        key: 60,
        amount: 0.75,
    };

    let clap_event = ClapEvent::ParamMod(event);

    // Verify header fields are accessible and correct
    let header = clap_event.header();
    assert_eq!(header.time, 42);
    assert_eq!(header.space_id, CLAP_CORE_EVENT_SPACE_ID);
    assert_eq!(header.type_, CLAP_EVENT_PARAM_MOD);
    assert_eq!(
        header.size,
        std::mem::size_of::<clap_event_param_mod>() as u32
    );

    // Verify data fields via pattern match
    match &clap_event {
        ClapEvent::ParamMod(e) => {
            assert_eq!(e.param_id, 123);
            assert_eq!(e.note_id, 7);
            assert_eq!(e.channel, 1);
            assert_eq!(e.key, 60);
            assert!((e.amount - 0.75).abs() < f64::EPSILON);
        }
        _ => panic!("Expected ParamMod"),
    }
}
