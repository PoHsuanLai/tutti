use super::*;
use crate::types::{
    ChordValue, EventHeader, K_NOTE_ON_EVENT, NoteExpressionIntValue, NoteExpressionText,
    NoteOnEvent, ScaleValue,
};
use tutti_midi_types::{CCNumber, MidiChannel, MidiGroup};
use tutti_plugin_types::NoteExpressionValue;

fn make_note_on() -> NoteOnEvent {
    NoteOnEvent {
        header: EventHeader {
            bus_index: 0,
            sample_offset: 0,
            ppq_position: 0.0,
            flags: 0,
            event_type: K_NOTE_ON_EVENT,
        },
        channel: 0,
        pitch: 60,
        tuning: 0.0,
        velocity: 0.8,
        length: 0,
        note_id: -1,
    }
}

/// A per-note bend sensitivity staged in one block sets the range a
/// later block's per-note bends are read at: the list keeps it.
///
/// Mutation (run): `Inner::clear` resets `bend_ranges` → the second
/// block's quarter bend reads 12 semitones (0.55), not 6 → fails.
#[test]
fn a_per_note_bend_range_outlives_its_block() {
    use tutti_midi_types::PitchBendSensitivity;
    use tutti_midi_types::convert::signed_f32_to_bend_u32;
    use tutti_midi_types::ump::{RPN_BANK_MPE, RPN_INDEX_PER_NOTE_PITCH_BEND_SENSITIVITY};

    let list = EventList::input();
    let sensitivity = [MidiEvent::registered_controller(
        MidiGroup::FIRST,
        MidiChannel::FIRST,
        RPN_BANK_MPE,
        RPN_INDEX_PER_NOTE_PITCH_BEND_SENSITIVITY,
        PitchBendSensitivity::from_semitones(24).to_rpn_bits(),
    )];
    list.update_from_sources(&Vst3InputEvents {
        midi: &sensitivity,
        ..Default::default()
    });
    assert_eq!(list.len(), 0, "the sensitivity stages nothing");

    let bend = [MidiEvent::per_note_pitch_bend(
        MidiGroup::FIRST,
        MidiChannel::FIRST,
        60,
        signed_f32_to_bend_u32(0.25),
    )];
    list.update_from_sources(&Vst3InputEvents {
        midi: &bend,
        ..Default::default()
    });
    let inner = list.inner.borrow();
    match inner.events.as_slice() {
        [Vst3Event::NoteExpression(e)] => {
            assert!((e.value - (0.5 + 6.0 / 240.0)).abs() < 1e-6, "{}", e.value);
        }
        other => panic!("expected one tuning expression, got {other:?}"),
    }
}

/// **VST3 and CLAP read one stream's per-note bends at the same range.**
/// The same two blocks go to both formats' event lists: a per-note bend
/// sensitivity of 24 on channel 1, then a quarter bend there and on
/// channel 2, and a block later another quarter bend on channel 1. Both
/// tell their plugin 6 semitones on channel 1 (in either block) and 12
/// on channel 2 (the ±48 default). VST3 carries semitones as
/// `0.5 + st / 240`, CLAP as semitones.
///
/// Mutation (run): CLAP back to the fixed ±48 (`from_midi` reading
/// `emitted_bend_range_semitones()`) → CLAP says 12 on channel 1 → fails.
/// Mutation (run): `InputEventList`'s `clear` resets its ranges → CLAP's
/// second block says 12 → fails.
#[test]
fn vst3_and_clap_bend_one_stream_by_the_same_semitones() {
    use tutti_clap_host::{ClapEvent, EventList as _, InputEventList};
    use tutti_midi_types::PitchBendSensitivity;
    use tutti_midi_types::convert::signed_f32_to_bend_u32;
    use tutti_midi_types::ump::{RPN_BANK_MPE, RPN_INDEX_PER_NOTE_PITCH_BEND_SENSITIVITY};

    let bend = |channel: u8| {
        MidiEvent::per_note_pitch_bend(
            MidiGroup::FIRST,
            MidiChannel::new(channel),
            60,
            signed_f32_to_bend_u32(0.25),
        )
    };
    let first = [
        MidiEvent::registered_controller(
            MidiGroup::FIRST,
            MidiChannel::FIRST,
            RPN_BANK_MPE,
            RPN_INDEX_PER_NOTE_PITCH_BEND_SENSITIVITY,
            PitchBendSensitivity::from_semitones(24).to_rpn_bits(),
        ),
        bend(0),
        bend(1),
    ];
    let second = [bend(0)];

    let vst3 = EventList::input();
    let mut clap = InputEventList::new();
    for (block, want) in [(&first[..], &[6.0, 12.0][..]), (&second[..], &[6.0][..])] {
        vst3.update_from_sources(&Vst3InputEvents {
            midi: block,
            ..Default::default()
        });
        clap.clear();
        clap.add_midi_events(block);

        let vst3_st: Vec<f64> = vst3
            .inner
            .borrow()
            .events
            .iter()
            .map(|e| match e {
                Vst3Event::NoteExpression(e) => (e.value - 0.5) * 240.0,
                other => panic!("VST3: expected a tuning, got {other:?}"),
            })
            .collect();
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
fn test_update_from_midi_counts_correctly() {
    let list = EventList::input();
    let midi_events = [
        MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x8000).with_frame_offset(0),
    ];
    list.update_from_midi(&midi_events);
    assert_eq!(list.len(), 1);
}

#[test]
fn test_clear_after_update_from_midi() {
    let list = EventList::input();
    let midi_events = [
        MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x8000).with_frame_offset(0),
        MidiEvent::note_off(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0).with_frame_offset(10),
    ];
    list.update_from_midi(&midi_events);
    list.clear();
    assert_eq!(list.len(), 0);
}

/// Plugin threads reading the list at once all get it: a read taken on
/// another thread is live (barriers hold it) while this one calls the
/// COM reads, and both succeed.
///
/// Mutation (run): `AudioThreadCell::try_acquire_reader` exclusive (a
/// compare-exchange from 0 to 1) → `getEventCount` answers 0 and
/// `getEvent` `kResultFalse` → fails.
#[test]
fn concurrent_reads_of_the_list_both_succeed() {
    use std::sync::Barrier;
    let list = EventList::input();
    list.update_from_midi(&[MidiEvent::note_on(
        MidiGroup::FIRST,
        MidiChannel::FIRST,
        60,
        0x8000,
    )]);
    let (taken, done) = (Barrier::new(2), Barrier::new(2));
    std::thread::scope(|scope| {
        scope.spawn(|| {
            let read = list.inner.borrow();
            taken.wait();
            done.wait();
            assert_eq!(read.events.len(), 1);
        });
        taken.wait();
        let ptr = list.to_com_ptr::<IEventList>().unwrap();
        // SAFETY: all-zero bytes is a valid value of this plain-data SDK struct.
        let mut out: Event = unsafe { std::mem::zeroed() };
        // SAFETY: the test calls a COM method on a live object it owns, with arguments valid
        // for the call.
        let count = unsafe { ptr.getEventCount() };
        // SAFETY: the test calls a COM method on a live object it owns, with arguments valid
        // for the call.
        let got = unsafe { ptr.getEvent(0, &mut out) };
        done.wait();
        assert_eq!(count, 1, "a second reader must see the list");
        assert_eq!(got, kResultOk);
    });
}

/// A read while the list is being written answers with the method's
/// failure value instead of panicking (a panic would unwind into the
/// plugin's frame and abort the process); once the write ends it reads.
///
/// Mutation (run): `getEvent` back on a panicking `borrow` → the panic
/// cannot unwind out of the COM thunk, and the test process aborts
/// (SIGABRT): the plugin-server crash this guards against.
#[test]
fn a_read_during_a_write_fails_at_the_com_level() {
    let list = EventList::input();
    list.update_from_midi(&[MidiEvent::note_on(
        MidiGroup::FIRST,
        MidiChannel::FIRST,
        60,
        0x8000,
    )]);
    let ptr = list.to_com_ptr::<IEventList>().unwrap();
    // SAFETY: all-zero bytes is a valid value of this plain-data SDK struct.
    let mut out: Event = unsafe { std::mem::zeroed() };
    {
        let _writing = list.inner.borrow_mut();
        // SAFETY: the test calls a COM method on a live object it owns, with arguments valid
        // for the call.
        assert_eq!(unsafe { ptr.getEventCount() }, 0);
        // SAFETY: the test calls a COM method on a live object it owns, with arguments valid
        // for the call.
        assert_eq!(unsafe { ptr.getEvent(0, &mut out) }, kResultFalse);
        out.r#type = K_NOTE_ON_EVENT;
        // SAFETY: the test calls a COM method on a live object it owns, with arguments valid
        // for the call.
        assert_eq!(unsafe { ptr.addEvent(&mut out) }, kResultFalse);
    }
    // SAFETY: the test calls a COM method on a live object it owns, with arguments valid for
    // the call.
    assert_eq!(unsafe { ptr.getEventCount() }, 1);
    // SAFETY: the test calls a COM method on a live object it owns, with arguments valid for
    // the call.
    assert_eq!(unsafe { ptr.getEvent(0, &mut out) }, kResultOk);
}

/// A `kDataEvent` of `data_type` as a plugin hands it to `addEvent`.
fn data_event(data_type: u32, sample_offset: i32, bytes: &[u8]) -> Event {
    // SAFETY: all-zero bytes is a valid value of this plain-data SDK struct.
    let mut e: Event = unsafe { std::mem::zeroed() };
    e.r#type = K_DATA_EVENT;
    e.sampleOffset = sample_offset;
    e.__field0.data = vst3::Steinberg::Vst::DataEvent {
        size: bytes.len() as u32,
        r#type: data_type,
        bytes: bytes.as_ptr(),
    };
    e
}

/// A `kMidiSysEx` data event as a plugin hands it to `addEvent`.
fn data_sysex(sample_offset: i32, bytes: &[u8]) -> Event {
    data_event(K_DATA_TYPE_MIDI_SYSEX, sample_offset, bytes)
}

/// Reassembles `events` into the complete messages they carry.
fn reassembled(events: &[MidiEvent]) -> Vec<Vec<u8>> {
    let mut asm = tutti_plugin_types::SysExAssembler::reserved();
    let mut done = Vec::new();
    for e in events {
        if let SysExStep::Complete(r) = asm.push(e) {
            done.push(asm.message(r).to_vec());
        }
    }
    done
}

/// **A SysEx a plugin emits arrives whole, whatever its length.** The
/// output list keeps every byte of it: capping it at the 16 inline bytes, or
/// at one UMP packet (6 bytes), would mean no message of 7 bytes or more ever
/// reached the host.
///
/// Mutation (run): drop the SysEx branch in `addEvent` so the event goes
/// through `from_c_event` (16 inline bytes) → the 40-byte and the 17-byte
/// messages come back cut or missing → fails.
#[test]
fn a_sysex_from_the_plugin_reaches_the_host_whole() {
    for len in [4usize, 7, 14, 16, 17, 40, 100] {
        let mut message = vec![0xF0];
        message.extend((0..len).map(|i| (i % 0x7F) as u8));
        message.push(0xF7);
        let list = EventList::output();
        let ptr = list.to_com_ptr::<IEventList>().unwrap();
        let mut e = data_sysex(11, &message);
        // SAFETY: the test calls a COM method on a live object it owns, with arguments valid
        // for the call.
        assert_eq!(unsafe { ptr.addEvent(&mut e) }, kResultOk, "{len} bytes");
        let mut pool = RtMidiEvents::new();
        list.fill_midi_events(&mut pool);
        assert!(!pool.is_overflowed());
        assert!(pool.iter().all(|m| m.frame_offset == 11));
        assert_eq!(reassembled(pool.as_slice()), vec![message], "{len} bytes");
        assert_eq!(list.dropped(), 0);
    }
}

/// A SysEx the output list cannot hold is dropped whole and counted; the
/// list keeps working for the next message.
///
/// Mutation (run): truncate to the arena's room instead of refusing →
/// the oversize message arrives cut (no `0xF7`) → fails.
#[test]
fn a_sysex_past_the_output_room_is_dropped_whole_and_counted() {
    let list = EventList::output();
    let ptr = list.to_com_ptr::<IEventList>().unwrap();
    let mut long = vec![0xF0];
    long.extend(std::iter::repeat_n(1u8, SYSEX_OUT_ROOM));
    long.push(0xF7);
    let mut e = data_sysex(0, &long);
    // SAFETY: the test calls a COM method on a live object it owns, with arguments valid for
    // the call.
    assert_eq!(unsafe { ptr.addEvent(&mut e) }, kResultFalse);
    assert_eq!(list.dropped(), 1);

    let short = [0xF0, 1, 2, 3, 4, 5, 6, 7, 8, 0xF7];
    let mut e = data_sysex(0, &short);
    // SAFETY: the test calls a COM method on a live object it owns, with arguments valid for
    // the call.
    assert_eq!(unsafe { ptr.addEvent(&mut e) }, kResultOk);
    let mut pool = RtMidiEvents::new();
    list.fill_midi_events(&mut pool);
    assert_eq!(reassembled(pool.as_slice()), vec![short.to_vec()]);
}

/// **A SysEx is recognised by its data type, framed or not.** An unframed
/// message of 7 to 16 bytes is kept whole, not cut to a `Data` event the host
/// could not pack into one packet; a data event of another type is not SysEx
/// whatever its bytes; and a raw channel-voice message carried as data
/// (status byte first) stays a MIDI-1 message.
///
/// Mutations (run): detect by `size > 16 || bytes[0] == 0xF0` alone → the
/// unframed 7/10/16-byte messages are cut or missing, and the other-type
/// 40-byte event is staged as SysEx → fails; drop the `< 0x80` arm → the
/// unframed messages fail; drop the `0xF0`/`< 0x80` guard (type alone) →
/// the raw note-on becomes a SysEx7 packet and fails.
#[test]
fn a_sysex_is_detected_by_its_data_type() {
    for len in [7usize, 10, 16] {
        let payload: Vec<u8> = (1..=len as u8).collect();
        let list = EventList::output();
        let ptr = list.to_com_ptr::<IEventList>().unwrap();
        let mut e = data_sysex(5, &payload);
        // SAFETY: the test calls a COM method on a live object it owns, with arguments valid
        // for the call.
        assert_eq!(unsafe { ptr.addEvent(&mut e) }, kResultOk, "{len} bytes");
        let mut pool = RtMidiEvents::new();
        list.fill_midi_events(&mut pool);
        assert_eq!(reassembled(pool.as_slice()), vec![framed(&payload)]);
    }

    // Another data type, even one led by `0xF0` or longer than the inline
    // bytes, is not a SysEx and stages no SysEx bytes.
    let mut other = vec![0xF0];
    other.extend(std::iter::repeat_n(1u8, 38));
    other.push(0xF7);
    let list = EventList::output();
    let ptr = list.to_com_ptr::<IEventList>().unwrap();
    let mut e = data_event(K_DATA_TYPE_MIDI_SYSEX + 1, 0, &other);
    // SAFETY: the test calls a COM method on a live object it owns, with arguments valid for
    // the call.
    assert_eq!(unsafe { ptr.addEvent(&mut e) }, kResultOk);
    assert_eq!(list.len(), 1);
    assert!(list.inner.borrow().sysex_out.is_empty());
    let mut pool = RtMidiEvents::new();
    list.fill_midi_events(&mut pool);
    assert!(reassembled(pool.as_slice()).is_empty());

    // A raw note-on carried as data (status byte first) is still a
    // MIDI-1 message.
    let list = EventList::output();
    let ptr = list.to_com_ptr::<IEventList>().unwrap();
    let raw = [0x90u8, 60, 100];
    let mut e = data_sysex(0, &raw);
    // SAFETY: the test calls a COM method on a live object it owns, with arguments valid for
    // the call.
    assert_eq!(unsafe { ptr.addEvent(&mut e) }, kResultOk);
    assert!(list.inner.borrow().sysex_out.is_empty());
    let mut pool = RtMidiEvents::new();
    list.fill_midi_events(&mut pool);
    assert_eq!(pool.len(), 1);
    assert!(pool.as_slice()[0].sysex7_payload().is_none());
}

/// **`getEvent` on an output list reads a SysEx back from the output
/// list's own buffer**, not the input list's reassembly buffer (which an
/// output list has none of).
///
/// Mutation (run): `getEvent` passing `inner.sysex.bytes()` → the event
/// reads back with size 0 and a null pointer → fails.
#[test]
fn get_event_reads_an_output_lists_sysex_back() {
    let list = EventList::output();
    let ptr = list.to_com_ptr::<IEventList>().unwrap();
    let message: Vec<u8> = framed(&sysex_payload(40));
    let mut e = data_sysex(9, &message);
    // SAFETY: all-zero bytes is a valid value of this plain-data SDK struct.
    assert_eq!(unsafe { ptr.addEvent(&mut e) }, kResultOk);
    // SAFETY: all-zero bytes is a valid value of this plain-data SDK struct.
    let mut back: Event = unsafe { std::mem::zeroed() };
    // SAFETY: the test wrote this union member itself, with the tag that names it.
    assert_eq!(unsafe { ptr.getEvent(0, &mut back) }, kResultOk);
    assert_eq!(back.sampleOffset, 9);
    // SAFETY: the test wrote this union member itself, with the tag that names it.
    let d = unsafe { back.__field0.data };
    assert_eq!(d.r#type, K_DATA_TYPE_MIDI_SYSEX);
    // SAFETY: the test calls a COM method on a live object it owns, with arguments valid for
    // the call.
    let read = unsafe { std::slice::from_raw_parts(d.bytes, d.size as usize) };
    assert_eq!(read, &message[..]);
}

/// **A SysEx the host's MIDI pool has no room for is dropped whole and
/// counted in `dropped`** (and latches the pool's `is_overflowed`); the
/// message before it arrives intact.
///
/// Mutation (run): drop `inner.dropped += refused` in `fill_midi_events`
/// → `dropped()` stays 0 → fails.
#[test]
fn a_sysex_past_the_midi_pool_is_counted_in_dropped() {
    let list = EventList::output();
    let ptr = list.to_com_ptr::<IEventList>().unwrap();
    // The first takes all but one of the pool's packets, the second
    // needs two; both fit the arena.
    let first = vec![1u8; (RT_MIDI_CAPACITY - 1) * 6 - 2];
    let second = vec![2u8; 7];
    for m in [&first, &second] {
        let mut e = data_sysex(0, m);
        // SAFETY: the test calls a COM method on a live object it owns, with arguments valid
        // for the call.
        assert_eq!(unsafe { ptr.addEvent(&mut e) }, kResultOk);
    }
    assert_eq!(list.dropped(), 0);
    let mut pool = RtMidiEvents::new();
    list.fill_midi_events(&mut pool);
    assert!(pool.is_overflowed());
    assert_eq!(list.dropped(), 1);
    assert_eq!(reassembled(pool.as_slice()), vec![framed(&first)]);
}

/// **The plugin-SysEx path is allocation-free**: a 200-byte message
/// through `addEvent`, then more until the list refuses one, then
/// `fill_midi_events` over what it holds.
///
/// Mutation (run): a `Vec` copy of the message in `stage_plugin_sysex`
/// (`let _v = std::slice::from_raw_parts(bytes, size).to_vec();`), and one
/// in `fill_midi_events`' SysEx arm → `assert_no_alloc` aborts.
#[test]
fn the_plugin_sysex_path_is_allocation_free() {
    let list = EventList::output();
    let ptr = list.to_com_ptr::<IEventList>().unwrap();
    let message = framed(&sysex_payload(200));
    let mut e = data_sysex(1, &message);
    let mut pool = RtMidiEvents::new();
    let mut refused = false;
    assert_no_alloc::assert_no_alloc(|| {
        // SAFETY: the test calls a COM method on a live object it owns, with arguments valid
        // for the call.
        assert_eq!(unsafe { ptr.addEvent(&mut e) }, kResultOk);
        for _ in 0..SYSEX_OUT_ROOM / 200 + 2 {
            // SAFETY: the test calls a COM method on a live object it owns, with arguments
            // valid for the call.
            refused |= unsafe { ptr.addEvent(&mut e) } == kResultFalse;
        }
        list.fill_midi_events(&mut pool);
    });
    assert!(refused, "the arena refused one inside the gate");
    assert!(!pool.is_empty());
    assert!(list.dropped() > 0);
}

#[test]
fn test_get_event_valid() {
    let list = EventList::input();
    list.inner
        .borrow_mut()
        .events
        .push(Vst3Event::NoteOn(make_note_on()));
    let ptr = list.to_com_ptr::<IEventList>().unwrap();
    // SAFETY: all-zero bytes is a valid value of this plain-data SDK struct.
    let mut out: Event = unsafe { std::mem::zeroed() };
    // SAFETY: the test calls a COM method on a live object it owns, with arguments valid for
    // the call.
    let result = unsafe { ptr.getEvent(0, &mut out) };
    assert_eq!(result, kResultOk);
    assert_eq!(out.r#type, K_NOTE_ON_EVENT);
    // SAFETY: the test wrote this union member itself, with the tag that names it.
    unsafe {
        assert_eq!(out.__field0.noteOn.pitch, 60);
        assert!((out.__field0.noteOn.velocity - 0.8).abs() < 1e-6);
    }
}

/// Regression for the dangling `DataEvent.bytes` bug.
///
/// A plugin's normal pattern is `getEventCount()` then `getEvent(i)` for
/// every `i`, keeping each returned `Event` (and therefore each borrowed
/// `bytes` pointer) live for the rest of `process`. Copying each event's
/// bytes into an inline-then-heap scratch of 8 cleared once per *block*
/// breaks that: the 9th push spills the inline storage to the heap and moves
/// it, dangling every pointer already handed out. This test stages well past
/// the 8-element inline capacity and reads every pointer only *after* the
/// whole batch has been fetched — the exact order that reads freed memory.
///
/// Every channel-voice non-note MIDI message becomes a `Data` event, so 32
/// CCs is not a contrived input.
#[test]
fn data_event_bytes_stay_valid_after_fetching_every_event() {
    use tutti_midi_types::convert::midi1_cc_to_midi2;

    const N: usize = 32;
    let midi: Vec<MidiEvent> = (0..N)
        .map(|i| {
            MidiEvent::cc(
                MidiGroup::FIRST,
                MidiChannel::FIRST,
                CCNumber::BRIGHTNESS,
                midi1_cc_to_midi2(i as u8),
            )
            .with_frame_offset(i as u32)
        })
        .collect();

    let list = EventList::input();
    list.update_from_midi(&midi);

    let ptr = list.to_com_ptr::<IEventList>().unwrap();
    // SAFETY: the test calls a COM method on a live object it owns, with arguments valid for
    // the call.
    let count = unsafe { ptr.getEventCount() };
    assert_eq!(count as usize, N, "every CC should stage as a Data event");

    // First fetch them all, holding every Event (and its `bytes`
    // pointer) live — as a plugin does.
    let mut fetched: Vec<Event> = Vec::with_capacity(N);
    for i in 0..count {
        // SAFETY: all-zero bytes is a valid value of this plain-data SDK struct.
        let mut out: Event = unsafe { std::mem::zeroed() };
        // SAFETY: the test calls a COM method on a live object it owns, with arguments valid
        // for the call.
        assert_eq!(unsafe { ptr.getEvent(i, &mut out) }, kResultOk);
        fetched.push(out);
    }

    // Then dereference. Each event must still see its own
    // 3-byte MIDI-1 CC frame, not another event's bytes or freed memory.
    for (i, ev) in fetched.iter().enumerate() {
        // SAFETY: the test wrote this union member itself, with the tag that names it.
        let data = unsafe { ev.__field0.data };
        assert_eq!(data.size, 3, "event {i}");
        assert!(!data.bytes.is_null(), "event {i}");
        // SAFETY: the test calls a COM method on a live object it owns, with arguments valid
        // for the call.
        let bytes = unsafe { std::slice::from_raw_parts(data.bytes, 3) };
        assert_eq!(bytes[0], 0xB0, "event {i} status");
        assert_eq!(bytes[1], 74, "event {i} controller");
        assert_eq!(
            bytes[2], i as u8,
            "event {i} value — pointer aliased or stale"
        );
    }
}

/// The same hazard on the text side: chord / scale / text events hand out a
/// `text` pointer into the arena. Fetching every event first and reading
/// after must still resolve each name correctly.
#[test]
fn text_pointers_stay_valid_after_fetching_every_event() {
    let names: Vec<Vec<u16>> = (0..16)
        .map(|i| format!("Chord{i}").encode_utf16().collect())
        .collect();
    let chords: Vec<ChordValue> = names
        .iter()
        .enumerate()
        .map(|(i, text)| ChordValue {
            sample_offset: i as i32,
            root: 60 + i as i16,
            bass_note: 48,
            mask: 0,
            text: text.clone(),
        })
        .collect();

    let list = EventList::input();
    list.update_from_sources(&Vst3InputEvents {
        chords: &chords,
        ..Default::default()
    });

    let ptr = list.to_com_ptr::<IEventList>().unwrap();
    // SAFETY: the test calls a COM method on a live object it owns, with arguments valid for
    // the call.
    let count = unsafe { ptr.getEventCount() };
    assert_eq!(count as usize, names.len());

    let mut fetched: Vec<Event> = Vec::with_capacity(names.len());
    for i in 0..count {
        // SAFETY: all-zero bytes is a valid value of this plain-data SDK struct.
        let mut out: Event = unsafe { std::mem::zeroed() };
        // SAFETY: the test calls a COM method on a live object it owns, with arguments valid
        // for the call.
        assert_eq!(unsafe { ptr.getEvent(i, &mut out) }, kResultOk);
        fetched.push(out);
    }

    for (i, ev) in fetched.iter().enumerate() {
        // SAFETY: the test wrote this union member itself, with the tag that names it.
        let chord = unsafe { ev.__field0.chord };
        assert_eq!(chord.root, 60 + i as i16, "event {i}");
        // SAFETY: the test calls a COM method on a live object it owns, with arguments valid
        // for the call.
        let text = unsafe { std::slice::from_raw_parts(chord.text, chord.textLen as usize) };
        assert_eq!(text, names[i].as_slice(), "event {i} text");
    }
}

/// **The text arena never grows, so a text pointer already handed out
/// never moves.** One block stages more maximum-length chord names than
/// the arena's reserved room holds. Every event is fetched first and
/// read after, as a plugin does; the names that fit read back intact,
/// the ones past the room are staged with an empty text and counted, and
/// staging the block, the very first time included, allocates nothing.
///
/// Mutations (run), each fails: interning without the room check (the
/// arena grows past its reservation) → the over-room texts arrive and the
/// drop count is 0; not counting the refusal → the drop count;
/// `TextArena::new` reserving nothing → the first staging allocates in
/// the gate.
#[test]
fn a_full_text_arena_refuses_rather_than_moving_its_pointers() {
    use crate::types::TextArena;
    const LONG: usize = 256;
    let fits = TextArena::CAPACITY / LONG;
    let over = 3;
    let names: Vec<Vec<u16>> = (0..fits + over)
        .map(|i| vec![b'a' as u16 + (i % 26) as u16; LONG])
        .collect();
    let chords: Vec<ChordValue> = names
        .iter()
        .enumerate()
        .map(|(i, text)| ChordValue {
            sample_offset: i as i32,
            root: 0,
            bass_note: 0,
            mask: 0,
            text: text.clone(),
        })
        .collect();
    let sources = Vst3InputEvents {
        chords: &chords,
        ..Default::default()
    };

    let list = EventList::input();
    assert_no_alloc::assert_no_alloc(|| {
        list.update_from_sources(&sources);
    });
    assert_eq!(list.text_dropped(), over);

    let ptr = list.to_com_ptr::<IEventList>().unwrap();
    // SAFETY: the test calls a COM method on a live object it owns, with arguments valid for
    // the call.
    let count = unsafe { ptr.getEventCount() };
    assert_eq!(count as usize, fits + over, "every event is staged");
    let mut fetched: Vec<Event> = Vec::with_capacity(count as usize);
    for i in 0..count {
        // SAFETY: all-zero bytes is a valid value of this plain-data SDK struct.
        let mut out: Event = unsafe { std::mem::zeroed() };
        // SAFETY: the test calls a COM method on a live object it owns, with arguments valid
        // for the call.
        assert_eq!(unsafe { ptr.getEvent(i, &mut out) }, kResultOk);
        fetched.push(out);
    }
    for (i, ev) in fetched.iter().enumerate() {
        // SAFETY: the test wrote this union member itself, with the tag that names it.
        let chord = unsafe { ev.__field0.chord };
        if i < fits {
            // SAFETY: the test calls a COM method on a live object it owns, with arguments
            // valid for the call.
            let text = unsafe { std::slice::from_raw_parts(chord.text, chord.textLen as usize) };
            assert_eq!(text, names[i].as_slice(), "event {i} text");
        } else {
            assert_eq!(chord.textLen, 0, "event {i} is past the room");
        }
    }

    assert_no_alloc::assert_no_alloc(|| {
        list.update_from_sources(&sources);
    });
    assert_eq!(list.text_dropped(), 2 * over);
}

/// `getEvent` must reject out-of-range and negative indices rather than
/// panic on the cast.
#[test]
fn get_event_rejects_bad_indices() {
    let list = EventList::input();
    list.update_from_midi(&[MidiEvent::note_on(
        MidiGroup::FIRST,
        MidiChannel::FIRST,
        60,
        0x8000,
    )]);
    let ptr = list.to_com_ptr::<IEventList>().unwrap();
    // SAFETY: all-zero bytes is a valid value of this plain-data SDK struct.
    let mut out: Event = unsafe { std::mem::zeroed() };
    // SAFETY: the test calls a COM method on a live object it owns, with arguments valid for
    // the call.
    assert_eq!(unsafe { ptr.getEvent(-1, &mut out) }, kInvalidArgument);
    // SAFETY: the test calls a COM method on a live object it owns, with arguments valid for
    // the call.
    assert_eq!(unsafe { ptr.getEvent(1, &mut out) }, kInvalidArgument);
    // SAFETY: the test calls a COM method on a live object it owns, with arguments valid for
    // the call.
    assert_eq!(unsafe { ptr.getEvent(0, &mut out) }, kResultOk);
}

/// RT regression: `update_from_midi` / `clear` run every buffer;
/// they must reuse the pre-reserved Vec capacity without heap grow.
#[test]
fn update_from_midi_is_allocation_free_after_warmup() {
    let list = EventList::input();
    let events = [
        MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x8000).with_frame_offset(0),
        MidiEvent::note_off(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0).with_frame_offset(32),
        MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::new(1), 64, 0x6000).with_frame_offset(64),
        MidiEvent::note_off(MidiGroup::FIRST, MidiChannel::new(1), 64, 0).with_frame_offset(96),
    ];

    // Warm up — the first call may grow (the default Vec cap is 256,
    // so this shouldn't, but prime anyway).
    list.update_from_midi(&events);
    list.clear();

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..10_000 {
            list.update_from_midi(&events);
            list.clear();
        }
    });
}

/// RT regression for the full-source path: staging MIDI + every other event
/// source (including arena-interned chord/scale/text) must stay
/// allocation-free once the events Vec and the text arena have warmed up.
#[test]
fn update_from_sources_is_allocation_free_after_warmup() {
    let list = EventList::input();
    let midi = [
        MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x8000).with_frame_offset(0),
        MidiEvent::note_off(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0).with_frame_offset(64),
    ];
    let note_expr = [NoteExpressionValue {
        sample_offset: 0,
        note_id: 0,
        expression_type: tutti_plugin_types::NoteExpressionType::Tuning,
        value: 0.5,
    }];
    let cmaj: Vec<u16> = "Cmaj7".encode_utf16().collect();
    let chords = [ChordValue {
        sample_offset: 0,
        root: 60,
        bass_note: 48,
        mask: 0,
        text: cmaj.clone(),
    }];
    let dorian: Vec<u16> = "D Dorian".encode_utf16().collect();
    let scales = [ScaleValue {
        sample_offset: 0,
        root: 62,
        mask: 0,
        text: dorian.clone(),
    }];
    let lyric: Vec<u16> = "la".encode_utf16().collect();
    let texts = [NoteExpressionText {
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

    let src = Vst3InputEvents {
        midi: &midi,
        note_expressions: &note_expr,
        chords: &chords,
        scales: &scales,
        expr_texts: &texts,
        expr_ints: &ints,
    };

    // Warm up every buffer (events Vec / text arena).
    list.update_from_sources(&src);
    list.clear();

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..10_000 {
            list.update_from_sources(&src);
            list.clear();
        }
    });
}

/// The kind and identity of a staged event, to read an order back: a
/// MIDI note by its pitch, a per-note expression by its note id, a
/// chord by its root, a scale by its root.
fn tag(e: &Vst3Event) -> (&'static str, i32) {
    match e {
        Vst3Event::NoteOn(n) => ("on", i32::from(n.pitch)),
        Vst3Event::NoteOff(n) => ("off", i32::from(n.pitch)),
        Vst3Event::NoteExpression(x) => ("expr", x.note_id),
        Vst3Event::NoteExpressionInt(x) => ("int", x.note_id),
        Vst3Event::Chord(c) => ("chord", i32::from(c.root)),
        Vst3Event::Scale(c) => ("scale", i32::from(c.root)),
        Vst3Event::NoteExpressionText(x) => ("text", x.note_id),
        other => panic!("unexpected event {other:?}"),
    }
}

fn staged(list: &EventList) -> Vec<(i32, (&'static str, i32))> {
    list.inner
        .borrow()
        .events
        .iter()
        .map(|e| (e.sample_offset(), tag(e)))
        .collect()
}

fn expr(offset: i32, note_id: i32) -> NoteExpressionValue {
    NoteExpressionValue {
        sample_offset: offset,
        note_id,
        expression_type: tutti_plugin_types::NoteExpressionType::Tuning,
        value: 0.5,
    }
}

fn chord(offset: i32, root: i16) -> ChordValue {
    ChordValue {
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
///
/// Mutations (run): `Inner::sort` back to
/// `self.events.sort_by_key(|e| e.sample_offset())` aborts in the gate,
/// as does `Inner::push` pushing past the room
/// (`self.events.push(event); return true;` first) when the block is
/// staged twice its room.
#[test]
fn a_full_block_from_every_source_stages_without_allocating() {
    use tutti_plugin_types::{HARMONY_CAPACITY, MIDI_STACK_CAPACITY};
    let midi: Vec<MidiEvent> = (0..MIDI_STACK_CAPACITY)
        .map(|i| {
            MidiEvent::note_on(
                MidiGroup::FIRST,
                MidiChannel::FIRST,
                (i % 128) as u8,
                0x8000,
            )
            .with_frame_offset(2 * i as u32)
        })
        .collect();
    let note_expressions: Vec<NoteExpressionValue> = (0..MIDI_STACK_CAPACITY as i32)
        .map(|i| expr(2 * i + 1, i))
        .collect();
    let backwards = |i: usize| (HARMONY_CAPACITY - i) as i32 * 100;
    let chords: Vec<ChordValue> = (0..HARMONY_CAPACITY)
        .map(|i| chord(backwards(i), i as i16))
        .collect();
    let scales: Vec<ScaleValue> = (0..HARMONY_CAPACITY)
        .map(|i| ScaleValue {
            sample_offset: backwards(i),
            root: i as i16,
            mask: 0,
            text: "D".encode_utf16().collect(),
        })
        .collect();
    let expr_texts: Vec<NoteExpressionText> = (0..HARMONY_CAPACITY)
        .map(|i| NoteExpressionText {
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
    let src = Vst3InputEvents {
        midi: &midi,
        note_expressions: &note_expressions,
        chords: &chords,
        scales: &scales,
        expr_texts: &expr_texts,
        expr_ints: &expr_ints,
    };

    let list = EventList::input();
    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..4 {
            list.update_from_sources(&src);
        }
    });
    assert_eq!(list.len(), EVENT_CAPACITY, "every event fits the room");
    assert_eq!(list.dropped(), 0);
    let offsets: Vec<i32> = staged(&list).iter().map(|(o, _)| *o).collect();
    assert!(offsets.is_sorted(), "in frame order: {offsets:?}");

    // Twice the room: the rest is dropped, counted, never grown into.
    let twice = Vst3InputEvents {
        midi: &[midi.as_slice(), midi.as_slice()].concat(),
        note_expressions: &[note_expressions.as_slice(), note_expressions.as_slice()].concat(),
        ..src
    };
    assert_no_alloc::assert_no_alloc(|| list.update_from_sources(&twice));
    assert_eq!(list.len(), EVENT_CAPACITY);
    let offered = 2 * (MIDI_STACK_CAPACITY + MIDI_STACK_CAPACITY) + 4 * HARMONY_CAPACITY;
    assert_eq!(list.dropped(), (offered - EVENT_CAPACITY) as u64);
}

/// **Events on one frame keep their source order**: the sources' order
/// (MIDI, then per-note expressions, integer expressions, chords), and
/// each source's own (a note-off before the note-on that reuses its key).
/// 330 events on ten frames, the expressions arriving on frames running
/// backwards, so the sort has many equal keys to move past each other.
///
/// Mutation (run): `Inner::sort` as `self.events.sort_unstable_by_key(..)`
/// fails here (equal frames transposed).
#[test]
fn events_on_one_frame_keep_their_source_order() {
    // 12 MIDI events a frame, alternating off/on, the pitch their arrival.
    let midi: Vec<MidiEvent> = (0..120u8)
        .map(|i| {
            let e = if i % 2 == 0 {
                MidiEvent::note_off(MidiGroup::FIRST, MidiChannel::FIRST, i, 0)
            } else {
                MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, i, 0x8000)
            };
            e.with_frame_offset(u32::from(i / 12))
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
    let chords: Vec<ChordValue> = (0..4).map(|j| chord(5, j)).collect();

    let list = EventList::input();
    list.update_from_sources(&Vst3InputEvents {
        midi: &midi,
        note_expressions: &note_expressions,
        chords: &chords,
        expr_ints: &expr_ints,
        ..Default::default()
    });

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
///
/// Mutations (run): `Inner::push` returning `false` for every event past
/// the room (no eviction) fails the note-off assertion; the
/// `self.dropped += 1` removed fails the count; `stage_with_text`
/// interning before its room check fails the arena assertion.
#[test]
fn past_the_room_note_offs_are_kept_and_the_rest_counted() {
    let ons = EVENT_CAPACITY;
    let midi: Vec<MidiEvent> = (0..ons)
        .map(|i| {
            MidiEvent::note_on(
                MidiGroup::FIRST,
                MidiChannel::FIRST,
                (i % 100) as u8,
                0x8000,
            )
            .with_frame_offset(i as u32)
        })
        .chain((0..10u8).map(|k| {
            MidiEvent::note_off(MidiGroup::FIRST, MidiChannel::FIRST, 100 + k, 0)
                .with_frame_offset(ons as u32)
        }))
        .collect();
    let chords = [chord(0, 1)];
    let src = Vst3InputEvents {
        midi: &midi,
        chords: &chords,
        ..Default::default()
    };

    let list = EventList::input();
    let capacity = list.inner.borrow().events.capacity();
    assert_no_alloc::assert_no_alloc(|| list.update_from_sources(&src));
    assert_eq!(
        list.inner.borrow().events.capacity(),
        capacity,
        "the list never grew"
    );
    assert_eq!(list.len(), EVENT_CAPACITY);
    assert_eq!(list.dropped(), 11, "ten note-ons gave way, and the chord");
    assert!(
        list.inner.borrow().text_arena.is_empty(),
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

/// **`addEvent` refuses past the room, counted**: the plugin's output list
/// holds `EVENT_CAPACITY` events and answers `kResultFalse` for the
/// next, without growing.
///
/// A refused chord spends none of the text arena.
///
/// Mutations (run): `addEvent`'s room check removed fails the arena
/// assertion (the chord's text is interned, then the event refused);
/// that and `Inner::push` growing past the room together abort in the
/// gate.
#[test]
fn add_event_refuses_past_the_room() {
    let list = EventList::input();
    let ptr = list.to_com_ptr::<IEventList>().unwrap();
    let mut on = to_c_event(&Vst3Event::NoteOn(make_note_on()), &[], &[]);
    let mut chord_arena = TextArena::new();
    let chord_event = chord(0, 1).to_vst3_event(&mut chord_arena);
    let mut chord_c = to_c_event(&chord_event, &chord_arena, &[]);
    let mut results = [0; 4];
    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..EVENT_CAPACITY - 1 {
            // SAFETY: the test calls a COM method on a live object it owns, with arguments
            // valid for the call.
            assert_eq!(unsafe { ptr.addEvent(&mut on) }, kResultOk);
        }
        for r in &mut results[..3] {
            // SAFETY: the test calls a COM method on a live object it owns, with arguments
            // valid for the call.
            *r = unsafe { ptr.addEvent(&mut on) };
        }
        // SAFETY: the test calls a COM method on a live object it owns, with arguments valid
        // for the call.
        results[3] = unsafe { ptr.addEvent(&mut chord_c) };
    });
    assert_eq!(
        results,
        [kResultOk, kResultFalse, kResultFalse, kResultFalse]
    );
    // SAFETY: the test calls a COM method on a live object it owns, with arguments valid for
    // the call.
    assert_eq!(unsafe { ptr.getEventCount() } as usize, EVENT_CAPACITY);
    assert_eq!(list.dropped(), 3);
    assert!(
        list.inner.borrow().text_arena.is_empty(),
        "the refused chord spent no text"
    );
}

/// **A dense expression block does not spend the harmony kinds' room.**
/// A full MIDI block, 300 per-note expressions (44 past their budget)
/// and four chords and scales: every chord and scale is staged, and the
/// 44 expressions past `NOTE_EXPRESSION_ROOM` are what is dropped,
/// counted.
///
/// Mutation (run): the budget removed (every expression `push`ed) fails
/// here: the expressions fill the room and the chords and scales are
/// dropped instead.
#[test]
fn note_expressions_past_their_budget_leave_the_harmony_room() {
    use tutti_plugin_types::{HARMONY_CAPACITY, MIDI_STACK_CAPACITY};
    let midi: Vec<MidiEvent> = (0..MIDI_STACK_CAPACITY)
        .map(|i| {
            MidiEvent::note_on(
                MidiGroup::FIRST,
                MidiChannel::FIRST,
                (i % 128) as u8,
                0x8000,
            )
            .with_frame_offset(i as u32)
        })
        .collect();
    let note_expressions: Vec<NoteExpressionValue> = (0..NOTE_EXPRESSION_ROOM as i32 + 44)
        .map(|j| expr(j, j))
        .collect();
    let chords: Vec<ChordValue> = (0..HARMONY_CAPACITY as i16)
        .map(|j| chord(j as i32, j))
        .collect();
    let scales: Vec<ScaleValue> = (0..HARMONY_CAPACITY as i16)
        .map(|j| ScaleValue {
            sample_offset: i32::from(j),
            root: j,
            mask: 0,
            text: "D".encode_utf16().collect(),
        })
        .collect();

    let list = EventList::input();
    assert_no_alloc::assert_no_alloc(|| {
        list.update_from_sources(&Vst3InputEvents {
            midi: &midi,
            note_expressions: &note_expressions,
            chords: &chords,
            scales: &scales,
            ..Default::default()
        })
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
///
/// Mutation (run): the sort key without the sign flip
/// (`e.sample_offset().cast_unsigned()`) puts -1 after every
/// non-negative offset and fails here.
#[test]
fn a_negative_offset_sorts_before_zero() {
    let midi = [
        MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x8000).with_frame_offset(0),
    ];
    let note_expressions = [expr(3, 1), expr(-1, 2)];
    let list = EventList::input();
    list.update_from_sources(&Vst3InputEvents {
        midi: &midi,
        note_expressions: &note_expressions,
        ..Default::default()
    });
    assert_eq!(
        staged(&list),
        vec![(-1, ("expr", 2)), (0, ("on", 60)), (3, ("expr", 1))]
    );
}

/// Every SysEx `kDataEvent` the plugin reads through `getEvent`, as
/// `(sampleOffset, bytes)`, its bytes read through the pointer it is
/// handed.
fn sysex_read_by_plugin(list: &EventList) -> Vec<(i32, Vec<u8>)> {
    use crate::types::K_DATA_TYPE_MIDI_SYSEX;
    let mut out = Vec::new();
    // SAFETY: all-zero bytes is a valid value of this plain-data SDK struct.
    unsafe {
        for i in 0..list.getEventCount() {
            let mut e: Event = std::mem::zeroed();
            assert_eq!(list.getEvent(i, &mut e), kResultOk);
            if e.r#type != K_DATA_EVENT {
                continue;
            }
            let d = e.__field0.data;
            if d.r#type != K_DATA_TYPE_MIDI_SYSEX {
                continue;
            }
            let bytes = std::slice::from_raw_parts(d.bytes, d.size as usize).to_vec();
            out.push((e.sampleOffset, bytes));
        }
    }
    out
}

/// A banked Program Change reaches the plugin as CC 0, CC 32 and PC, at one
/// offset, in that order, as `kDataEvent`s of raw MIDI-1 bytes.
///
/// With room for fewer than three, none is staged and all three are counted.
///
/// Mutation (run): `stage_midi` converting the event with
/// `vst3_event_from_midi` alone → only the PC → fails; pushing each message
/// without the room check → a partial bank in the full list → fails.
#[test]
fn a_banked_program_change_reaches_the_plugin_as_three_messages() {
    let pc = MidiEvent::program_change(MidiGroup::FIRST, MidiChannel::new(1), 9, Some(0x0085))
        .with_frame_offset(5);
    let list = EventList::input();
    assert_no_alloc::assert_no_alloc(|| list.update_from_midi(&[pc]));
    let mut seen = Vec::new();
    // SAFETY: all-zero bytes is a valid value of this plain-data SDK struct.
    unsafe {
        for i in 0..list.getEventCount() {
            let mut e: Event = std::mem::zeroed();
            assert_eq!(list.getEvent(i, &mut e), kResultOk);
            assert_eq!(e.r#type, K_DATA_EVENT);
            let d = e.__field0.data;
            let bytes = std::slice::from_raw_parts(d.bytes, d.size as usize).to_vec();
            seen.push((e.sampleOffset, bytes));
        }
    }
    assert_eq!(
        seen,
        vec![
            (5, vec![0xB1, 0, 1]),
            (5, vec![0xB1, 32, 5]),
            (5, vec![0xC1, 9, 0]),
        ]
    );

    // Room for two: nothing of the bank is staged, and all three are counted.
    let note = MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x8000);
    let mut midi = vec![note; EVENT_CAPACITY - 2];
    midi.push(pc);
    let full = EventList::input();
    full.update_from_midi(&midi);
    assert_eq!(full.len(), EVENT_CAPACITY - 2);
    assert_eq!(full.dropped(), 3);
}

fn sysex_packets(payload: &[u8], frame: u32) -> Vec<MidiEvent> {
    let mut out = Vec::new();
    MidiEvent::sysex7_fragments(MidiGroup::FIRST, payload, &mut out);
    out.into_iter()
        .map(|e| e.with_frame_offset(frame))
        .collect()
}

fn framed(payload: &[u8]) -> Vec<u8> {
    let mut v = vec![0xF0];
    v.extend_from_slice(payload);
    v.push(0xF7);
    v
}

fn sysex_payload(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i * 7 % 0x7F) as u8).collect()
}

/// **A multi-packet SysEx reaches the plugin as one complete message**,
/// `0xF0 … 0xF7`, at its `END` packet's frame, beside the block's other
/// events, and the staging does not allocate.
///
/// Mutation (run): `stage_midi` converting every event with
/// `vst3_event_from_midi` (no reassembly) → no SysEx event → fails.
#[test]
fn a_multi_packet_sysex_reaches_the_plugin_whole() {
    let body = sysex_payload(100);
    let mut midi = sysex_packets(&body, 5);
    let last = midi.len() - 1;
    midi[last].frame_offset = 9;
    midi.insert(
        3,
        MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x8000).with_frame_offset(7),
    );
    let list = EventList::input();
    assert_no_alloc::assert_no_alloc(|| {
        list.update_from_sources(&Vst3InputEvents {
            midi: &midi,
            ..Default::default()
        })
    });
    assert_eq!(sysex_read_by_plugin(&list), vec![(9, framed(&body))]);
    assert_eq!(list.len(), 2, "the note and the one SysEx");
    assert_eq!(list.sysex_dropped(), 0);
}

/// **A SysEx that spans blocks completes in the block of its `END`**;
/// the block holding only its start hands the plugin nothing of it.
///
/// Mutation (run): `Inner::clear` clearing the assembler's open run
/// (a `begin_block` that always clears) → the `END` finds no run → the
/// message never arrives → fails.
#[test]
fn a_sysex_spanning_blocks_completes_in_the_block_of_its_end() {
    let body = sysex_payload(MIDI_STACK_CAPACITY * 6 + 100);
    let midi = sysex_packets(&body, 0);
    assert!(midi.len() > MIDI_STACK_CAPACITY);
    let list = EventList::input();
    let (first, second) = midi.split_at(MIDI_STACK_CAPACITY);
    list.update_from_midi(first);
    assert!(sysex_read_by_plugin(&list).is_empty());
    // An empty block between them clears the list, keeping the run.
    list.clear();
    list.update_from_midi(second);
    assert_eq!(sysex_read_by_plugin(&list), vec![(0, framed(&body))]);
    assert_eq!(list.sysex_dropped(), 0);
}

/// **A SysEx past `SYSEX_MESSAGE_CAPACITY` is dropped whole and counted**
/// (never truncated), and the next one arrives intact.
///
/// Mutation (run): the assembler's capacity check removed → the
/// over-long message is delivered → fails.
#[test]
fn a_sysex_past_capacity_is_dropped_whole_and_counted() {
    use tutti_plugin_types::SYSEX_MESSAGE_CAPACITY;
    let long = sysex_payload(SYSEX_MESSAGE_CAPACITY - 1); // framed: one past
    let list = EventList::input();
    for block in sysex_packets(&long, 0).chunks(MIDI_STACK_CAPACITY) {
        list.update_from_midi(block);
        assert!(sysex_read_by_plugin(&list).is_empty());
    }
    assert_eq!(list.sysex_dropped(), 1);
    let next = sysex_payload(20);
    list.update_from_midi(&sysex_packets(&next, 2));
    assert_eq!(sysex_read_by_plugin(&list), vec![(2, framed(&next))]);
    assert_eq!(list.sysex_dropped(), 1);
}

/// **A `START` with no `END` before the next `START` is dropped and
/// counted**; the later message arrives whole.
///
/// Mutation (run): the assembler's `START` not abandoning the open run →
/// the two messages are spliced together and delivered → fails.
#[test]
fn a_start_without_its_end_before_the_next_start_is_dropped() {
    let a = sysex_payload(30);
    let b = sysex_payload(20);
    let mut midi = sysex_packets(&a, 0);
    midi.truncate(2); // START and one CONTINUE
    midi.extend(sysex_packets(&b, 4));
    let list = EventList::input();
    list.update_from_midi(&midi);
    assert_eq!(sysex_read_by_plugin(&list), vec![(4, framed(&b))]);
    assert_eq!(list.sysex_dropped(), 1);
}
