use super::*;
use crate::types::{Chord, ExpressionText, NoteOff, NoteOn};
use vst3::Steinberg::Vst::Event as CEvent;

fn note_on(pitch: i16, sample_offset: i32) -> Event {
    Event::at(
        sample_offset,
        Payload::NoteOn(NoteOn::new(0, pitch, 0.8, -1)),
    )
}

fn note_off(pitch: i16, sample_offset: i32) -> Event {
    Event::at(
        sample_offset,
        Payload::NoteOff(NoteOff::new(0, pitch, 0.0, -1)),
    )
}

fn cc(value: u8, sample_offset: i32) -> Event {
    let data = Data::inline(0, [0xB0, 74, value]);
    Event::at(sample_offset, Payload::Data(data))
}

/// Stages `events` as one block.
fn stage(list: &EventList, events: impl IntoIterator<Item = Event>) {
    let mut staging = list.begin();
    for event in events {
        staging.push(event);
    }
    staging.finish();
}

/// An all-zero SDK event, for the plugin side of a call.
fn zeroed() -> CEvent {
    crate::sdk::zeroed()
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
    stage(&list, [note_on(60, 0)]);
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
        let mut out = zeroed();
        // SAFETY: the test calls a COM method on a live object it owns, with arguments valid
        // for the call.
        let count = unsafe { ptr.getEventCount() };
        // SAFETY: as above.
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
    stage(&list, [note_on(60, 0)]);
    let ptr = list.to_com_ptr::<IEventList>().unwrap();
    let mut out = zeroed();
    {
        let _writing = list.inner.borrow_mut();
        // SAFETY: the test calls a COM method on a live object it owns, with arguments valid
        // for the call.
        assert_eq!(unsafe { ptr.getEventCount() }, 0);
        // SAFETY: as above.
        assert_eq!(unsafe { ptr.getEvent(0, &mut out) }, kResultFalse);
        out.r#type = EventKind::NoteOn as u16;
        // SAFETY: as above.
        assert_eq!(unsafe { ptr.addEvent(&mut out) }, kResultFalse);
    }
    // SAFETY: as above.
    assert_eq!(unsafe { ptr.getEventCount() }, 1);
    // SAFETY: as above.
    assert_eq!(unsafe { ptr.getEvent(0, &mut out) }, kResultOk);
}

/// A `kDataEvent` of `data_type` as a plugin hands it to `addEvent`.
fn data_event(data_type: u32, sample_offset: i32, bytes: &[u8]) -> CEvent {
    let mut e = zeroed();
    e.r#type = EventKind::Data as u16;
    e.sampleOffset = sample_offset;
    e.__field0.data = vst3::Steinberg::Vst::DataEvent {
        size: u32::try_from(bytes.len()).unwrap(),
        r#type: data_type,
        bytes: bytes.as_ptr(),
    };
    e
}

/// A `kMidiSysEx` data event as a plugin hands it to `addEvent`.
fn data_sysex(sample_offset: i32, bytes: &[u8]) -> CEvent {
    data_event(K_DATA_TYPE_MIDI_SYSEX, sample_offset, bytes)
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

/// **A SysEx a plugin emits arrives whole, whatever its length.** The
/// output list keeps every byte of it: capping it at the 16 inline bytes, or
/// at one UMP packet (6 bytes), would mean no message of 7 bytes or more ever
/// reached the host.
///
/// Mutation (run): drop the SysEx branch in `addEvent` so the event goes
/// through `Event::from_c` (16 inline bytes) → the 40-byte and the 17-byte
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
        assert_eq!(list.staged_sysex(), vec![message], "{len} bytes");
        list.with_events(|staged| assert_eq!(staged.events[0].sample_offset(), 11));
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
    // SAFETY: as above.
    assert_eq!(unsafe { ptr.addEvent(&mut e) }, kResultOk);
    assert_eq!(list.staged_sysex(), vec![short.to_vec()]);
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
/// the raw note-on becomes a SysEx and fails.
#[test]
fn a_sysex_is_detected_by_its_data_type() {
    for len in [7usize, 10, 16] {
        let payload: Vec<u8> = (1..=u8::try_from(len).unwrap()).collect();
        let list = EventList::output();
        let ptr = list.to_com_ptr::<IEventList>().unwrap();
        let mut e = data_sysex(5, &payload);
        // SAFETY: the test calls a COM method on a live object it owns, with arguments valid
        // for the call.
        assert_eq!(unsafe { ptr.addEvent(&mut e) }, kResultOk, "{len} bytes");
        assert_eq!(list.staged_sysex(), vec![payload], "{len} bytes");
    }

    // Another data type, even one led by `0xF0` or longer than the inline
    // bytes, is not a SysEx and stages no SysEx bytes.
    let mut other = vec![0xF0];
    other.extend(std::iter::repeat_n(1u8, 38));
    other.push(0xF7);
    let list = EventList::output();
    let ptr = list.to_com_ptr::<IEventList>().unwrap();
    let mut e = data_event(K_DATA_TYPE_MIDI_SYSEX + 1, 0, &other);
    // SAFETY: as above.
    assert_eq!(unsafe { ptr.addEvent(&mut e) }, kResultOk);
    assert_eq!(list.len(), 1);
    assert!(list.inner.borrow().sysex_out.is_empty());
    assert!(list.staged_sysex().is_empty());

    // A raw note-on carried as data (status byte first) is still a MIDI-1 message.
    let list = EventList::output();
    let ptr = list.to_com_ptr::<IEventList>().unwrap();
    let raw = [0x90u8, 60, 100];
    let mut e = data_sysex(0, &raw);
    // SAFETY: as above.
    assert_eq!(unsafe { ptr.addEvent(&mut e) }, kResultOk);
    assert!(list.inner.borrow().sysex_out.is_empty());
    list.with_events(|staged| match staged.events {
        [
            Event {
                payload: Payload::Data(d),
                ..
            },
        ] => assert_eq!(d.payload(), &raw),
        other => panic!("expected one Data event, got {other:?}"),
    });
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
    // SAFETY: the test calls a COM method on a live object it owns, with arguments valid for
    // the call.
    assert_eq!(unsafe { ptr.addEvent(&mut e) }, kResultOk);
    let mut back = zeroed();
    // SAFETY: as above.
    assert_eq!(unsafe { ptr.getEvent(0, &mut back) }, kResultOk);
    assert_eq!(back.sampleOffset, 9);
    // SAFETY: `getEvent` wrote the union member the tag names.
    let d = unsafe { back.__field0.data };
    assert_eq!(d.r#type, K_DATA_TYPE_MIDI_SYSEX);
    // SAFETY: the list's buffer holds `size` bytes at `bytes`, and is not touched.
    let read = unsafe { std::slice::from_raw_parts(d.bytes, d.size as usize) };
    assert_eq!(read, &message[..]);
}

/// **The plugin-SysEx path is allocation-free**: a 200-byte message
/// through `addEvent`, then more until the list refuses one.
///
/// Mutation (run): a `Vec` copy of the message in `stage_plugin_sysex`
/// (`let _v = std::slice::from_raw_parts(bytes, size).to_vec();`) →
/// `assert_no_alloc` aborts.
#[test]
fn the_plugin_sysex_path_is_allocation_free() {
    let list = EventList::output();
    let ptr = list.to_com_ptr::<IEventList>().unwrap();
    let message = framed(&sysex_payload(200));
    let mut e = data_sysex(1, &message);
    let mut refused = false;
    assert_no_alloc::assert_no_alloc(|| {
        // SAFETY: the test calls a COM method on a live object it owns, with arguments valid
        // for the call.
        assert_eq!(unsafe { ptr.addEvent(&mut e) }, kResultOk);
        for _ in 0..SYSEX_OUT_ROOM / 200 + 2 {
            // SAFETY: as above.
            refused |= unsafe { ptr.addEvent(&mut e) } == kResultFalse;
        }
    });
    assert!(refused, "the arena refused one inside the gate");
    assert!(list.dropped() > 0);
}

#[test]
fn test_get_event_valid() {
    let list = EventList::input();
    stage(&list, [note_on(60, 0)]);
    let ptr = list.to_com_ptr::<IEventList>().unwrap();
    let mut out = zeroed();
    // SAFETY: the test calls a COM method on a live object it owns, with arguments valid for
    // the call.
    let result = unsafe { ptr.getEvent(0, &mut out) };
    assert_eq!(result, kResultOk);
    assert_eq!(EventKind::try_from(out.r#type), Ok(EventKind::NoteOn));
    // SAFETY: the tag names this union member.
    unsafe {
        assert_eq!(out.__field0.noteOn.pitch, 60);
        assert!((out.__field0.noteOn.velocity - 0.8).abs() < 1e-6);
    }
}

/// Fetches every event of `list` first, holding each (and its pointers) live
/// as a plugin does.
fn fetch_all(list: &ComWrapper<EventList>) -> Vec<CEvent> {
    let ptr = list.to_com_ptr::<IEventList>().unwrap();
    // SAFETY: the test calls a COM method on a live object it owns, with arguments valid for
    // the call.
    let count = unsafe { ptr.getEventCount() };
    (0..count)
        .map(|i| {
            let mut out = zeroed();
            // SAFETY: as above.
            assert_eq!(unsafe { ptr.getEvent(i, &mut out) }, kResultOk);
            out
        })
        .collect()
}

/// A plugin calls `getEventCount()` then `getEvent(i)` for every `i`, keeping
/// each returned `Event` (and its borrowed `bytes` pointer) live for the rest of
/// `process`. Staged well past any small inline scratch, every pointer must
/// still read its own event's bytes after the whole batch has been fetched.
///
/// Mutation (run): `getEvent` ignoring the index (`events.get(i * 0)`) → every
/// event reads event 0's bytes → fails (as does the text test below).
#[test]
fn data_event_bytes_stay_valid_after_fetching_every_event() {
    const N: u8 = 32;
    let list = EventList::input();
    stage(&list, (0..N).map(|i| cc(i, i32::from(i))));

    let fetched = fetch_all(&list);
    assert_eq!(fetched.len(), usize::from(N), "every CC is staged");

    // Then dereference. Each event must still see its own 3-byte frame, not
    // another event's bytes or freed memory.
    for (i, ev) in fetched.iter().enumerate() {
        // SAFETY: the tag names this union member.
        let data = unsafe { ev.__field0.data };
        assert_eq!(data.size, 3, "event {i}");
        assert!(!data.bytes.is_null(), "event {i}");
        // SAFETY: the event's inline bytes, live in the list.
        let bytes = unsafe { std::slice::from_raw_parts(data.bytes, 3) };
        assert_eq!(bytes[0], 0xB0, "event {i} status");
        assert_eq!(bytes[1], 74, "event {i} controller");
        assert_eq!(
            usize::from(bytes[2]),
            i,
            "event {i} value: pointer aliased or stale"
        );
    }
}

fn utf16(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

/// A chord whose name is interned into the block's arena.
fn chord_event(staging: &mut Staging<'_>, root: i16, name: &[u16]) -> Event {
    let text = staging.intern(name);
    Event::at(
        i32::from(root),
        Payload::Chord(Chord {
            root,
            bass_note: 48,
            mask: 0,
            text,
        }),
    )
}

/// The same hazard on the text side: chord / scale / text events hand out a
/// `text` pointer into the arena. Fetching every event first and reading
/// after must still resolve each name correctly.
///
/// Mutation (run): `getEvent` ignoring the index (`events.get(i * 0)`) → every
/// chord reads the first root and name → fails.
#[test]
fn text_pointers_stay_valid_after_fetching_every_event() {
    let names: Vec<Vec<u16>> = (0..16).map(|i| utf16(&format!("Chord{i}"))).collect();
    let list = EventList::input();
    let mut staging = list.begin();
    for (i, name) in names.iter().enumerate() {
        let event = chord_event(&mut staging, 60 + i as i16, name);
        staging.push(event);
    }
    staging.finish();

    let fetched = fetch_all(&list);
    assert_eq!(fetched.len(), names.len());
    for (i, ev) in fetched.iter().enumerate() {
        // SAFETY: the tag names this union member.
        let chord = unsafe { ev.__field0.chord };
        assert_eq!(chord.root, 60 + i as i16, "event {i}");
        // SAFETY: the list's arena holds `textLen` units at `text`, and is not touched.
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
    const LONG: usize = 256;
    let fits = TextArena::CAPACITY / LONG;
    let over = 3;
    let names: Vec<Vec<u16>> = (0..fits + over)
        .map(|i| vec![u16::from(b'a') + (i % 26) as u16; LONG])
        .collect();
    let stage_names = |list: &EventList| {
        let mut staging = list.begin();
        for (i, name) in names.iter().enumerate() {
            let event = chord_event(&mut staging, 0, name);
            staging.push(Event::at(i as i32, event.payload));
        }
        staging.finish();
    };

    let list = EventList::input();
    assert_no_alloc::assert_no_alloc(|| stage_names(&list));
    assert_eq!(list.text_dropped(), over);

    let fetched = fetch_all(&list);
    assert_eq!(fetched.len(), fits + over, "every event is staged");
    for (i, ev) in fetched.iter().enumerate() {
        // SAFETY: the tag names this union member.
        let chord = unsafe { ev.__field0.chord };
        if i < fits {
            // SAFETY: the list's arena holds `textLen` units at `text`, and is not touched.
            let text = unsafe { std::slice::from_raw_parts(chord.text, chord.textLen as usize) };
            assert_eq!(text, names[i].as_slice(), "event {i} text");
        } else {
            assert_eq!(chord.textLen, 0, "event {i} is past the room");
        }
    }

    assert_no_alloc::assert_no_alloc(|| stage_names(&list));
    assert_eq!(list.text_dropped(), 2 * over);
}

/// `getEvent` must reject out-of-range and negative indices rather than
/// panic on the cast.
#[test]
fn get_event_rejects_bad_indices() {
    let list = EventList::input();
    stage(&list, [note_on(60, 0)]);
    let ptr = list.to_com_ptr::<IEventList>().unwrap();
    let mut out = zeroed();
    // SAFETY: the test calls a COM method on a live object it owns, with arguments valid for
    // the call.
    assert_eq!(unsafe { ptr.getEvent(-1, &mut out) }, kInvalidArgument);
    // SAFETY: as above.
    assert_eq!(unsafe { ptr.getEvent(1, &mut out) }, kInvalidArgument);
    // SAFETY: as above.
    assert_eq!(unsafe { ptr.getEvent(0, &mut out) }, kResultOk);
}

/// Staging and `clear` run every block; they reuse the reserved capacity.
///
/// Mutation (run): a `Vec::with_capacity(8)` in `Staging::push` → `assert_no_alloc`
/// aborts.
#[test]
fn staging_is_allocation_free_after_warmup() {
    let list = EventList::input();
    let events = [
        note_on(60, 0),
        note_off(60, 32),
        note_on(64, 64),
        note_off(64, 96),
    ];
    stage(&list, events);
    list.clear();

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..10_000 {
            stage(&list, events);
            list.clear();
        }
    });
}

/// Mutation (run): `Inner::clear` not clearing `events` → the count stays 2 → fails.
#[test]
fn clear_empties_a_staged_block() {
    let list = EventList::input();
    stage(&list, [note_on(60, 0), note_off(60, 10)]);
    assert_eq!(list.len(), 2);
    list.clear();
    assert_eq!(list.len(), 0);
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
    let list = EventList::input();
    stage(&list, [note_on(60, 3), note_on(61, -1), note_on(62, 0)]);
    let offsets: Vec<i32> =
        list.with_events(|staged| staged.events.iter().map(Event::sample_offset).collect());
    assert_eq!(offsets, [-1, 0, 3]);
}

/// Past the room, a note-off is kept and a note-on gives way, and every
/// loss is counted once; the list never grows.
///
/// Mutations (run): `Inner::push` returning `false` for every event past
/// the room (no eviction) fails the note-off assertion; the
/// `self.dropped += 1` removed fails the count.
#[test]
fn past_the_room_a_note_off_evicts_and_the_rest_are_counted() {
    let list = EventList::input();
    let mut staging = list.begin();
    for i in 0..EVENT_CAPACITY {
        assert!(staging.push(note_on(i16::try_from(i % 100).unwrap(), 0)));
    }
    assert_eq!(staging.room(), 0);
    assert!(
        !staging.push(note_on(1, 0)),
        "a note-on past the room is dropped"
    );
    for k in 0..10 {
        assert!(staging.push(note_off(100 + k, 0)), "a note-off is kept");
    }
    staging.finish();
    assert_eq!(list.len(), EVENT_CAPACITY);
    assert_eq!(
        list.dropped(),
        11,
        "one note-on past the room, ten that gave way"
    );
    let off_pitches: Vec<i16> = list.with_events(|staged| {
        staged
            .events
            .iter()
            .filter_map(|e| match &e.payload {
                Payload::NoteOff(o) => Some(o.pitch),
                _ => None,
            })
            .collect()
    });
    assert_eq!(off_pitches, (100..110).collect::<Vec<_>>());
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
    let mut on = note_on(60, 0).to_c(&[], &[]);
    let mut chord_arena = TextArena::new();
    let text = chord_arena.intern(&utf16("C"));
    let chord = Event::at(
        0,
        Payload::Chord(Chord {
            root: 1,
            bass_note: 0,
            mask: 0,
            text,
        }),
    );
    let mut chord_c = chord.to_c(&chord_arena, &[]);
    let mut results = [0; 4];
    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..EVENT_CAPACITY - 1 {
            // SAFETY: the test calls a COM method on a live object it owns, with arguments
            // valid for the call.
            assert_eq!(unsafe { ptr.addEvent(&mut on) }, kResultOk);
        }
        for r in &mut results[..3] {
            // SAFETY: as above.
            *r = unsafe { ptr.addEvent(&mut on) };
        }
        // SAFETY: as above.
        results[3] = unsafe { ptr.addEvent(&mut chord_c) };
    });
    assert_eq!(
        results,
        [kResultOk, kResultFalse, kResultFalse, kResultFalse]
    );
    // SAFETY: as above.
    assert_eq!(unsafe { ptr.getEventCount() } as usize, EVENT_CAPACITY);
    assert_eq!(list.dropped(), 3);
    assert!(
        list.inner.borrow().text_arena.is_empty(),
        "the refused chord spent no text"
    );
}

/// A text event the plugin adds is copied into the arena, whatever the
/// pointer's lifetime.
///
/// Mutation (run): `from_c` interning nothing (an empty `TextRef`) → the text
/// reads back empty → fails.
#[test]
fn a_plugin_text_is_copied_into_the_arena() {
    let list = EventList::output();
    let ptr = list.to_com_ptr::<IEventList>().unwrap();
    let name = utf16("lyric");
    let mut e = zeroed();
    e.r#type = EventKind::NoteExpressionText as u16;
    e.__field0.noteExpressionText = vst3::Steinberg::Vst::NoteExpressionTextEvent {
        typeId: 7,
        noteId: 3,
        textLen: u32::try_from(name.len()).unwrap(),
        text: name.as_ptr(),
    };
    // SAFETY: the test calls a COM method on a live object it owns, with arguments valid for
    // the call; `name` outlives it.
    assert_eq!(unsafe { ptr.addEvent(&mut e) }, kResultOk);
    drop(name);
    list.with_events(|staged| match staged.events {
        [
            Event {
                payload: Payload::ExpressionText(ExpressionText { text, .. }),
                ..
            },
        ] => assert_eq!(text.slice(staged.text), Some(utf16("lyric").as_slice())),
        other => panic!("expected one text event, got {other:?}"),
    });
}

/// **A plugin thread inside a list call makes the host skip, not panic.** The
/// audio thread's `try_begin` and `try_with_events` used to `borrow`, which
/// panics when a plugin thread is inside `getEventCount` or `getEvent`.
///
/// Mutation: make `try_begin` use `borrow_mut` again -> the first call panics.
#[test]
fn a_list_in_use_by_the_plugin_is_skipped_and_counted() {
    let list = EventList::input();
    {
        let _plugin_reading = list.inner.borrow();
        assert!(list.try_begin().is_none());
        list.clear();
        list.add_dropped(1);
        assert_eq!(list.busy(), 3);
    }
    let staging = list.try_begin().expect("free again");
    assert!(list.try_with_events(|_| ()).is_none());
    drop(staging);
    assert_eq!(list.dropped(), 0, "the skipped add_dropped did not count");
    assert!(list.try_with_events(|_| ()).is_some());
}
