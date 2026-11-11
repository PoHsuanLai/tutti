//! The event lists against a plugin's view of them: what `getEvent` hands a
//! plugin after the host stages a block (`Stager`), and what the host reads
//! after a plugin `addEvent`s (`drain_midi`).
//!
//! Here, not in the host's `src/`, because the plugin's side of these calls is
//! `unsafe` COM and the host crate forbids `unsafe`.

use tutti_fixtures::midi;
use tutti_graph::Offset;
use tutti_graph::UmpAt;
use tutti_plugin_types::{MIDI_STACK_CAPACITY, RT_MIDI_CAPACITY, RtMidiEvents, WireMidi};
use tutti_types::Samples;
use tutti_ump::{MidiChannel, MidiGroup, SysexStep};
use tutti_vst3_ffi::{
    EVENT_CAPACITY, EventKind, EventList, K_DATA_TYPE_MIDI_SYSEX, SYSEX_OUT_ROOM, TextArena,
};
use tutti_vst3_host::{InputEvents, Stager, Utf16Chord, drain_midi};
use vst3::Steinberg::Vst::{Event, IEventList, IEventListTrait};
use vst3::Steinberg::{kResultFalse, kResultOk};

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

fn count(list: &EventList) -> usize {
    list.with_events(|staged| staged.events.len())
}

/// A `kDataEvent` of `data_type` as a plugin hands it to `addEvent`.
fn data_event(data_type: u32, sample_offset: i32, bytes: &[u8]) -> Event {
    // SAFETY: all-zero bytes is a valid value of this plain-data SDK struct.
    let mut e: Event = unsafe { std::mem::zeroed() };
    e.r#type = EventKind::Data as u16;
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
fn reassembled(events: &[WireMidi]) -> Vec<Vec<u8>> {
    let mut asm = tutti_plugin_types::sysex_assembler();
    let mut done = Vec::new();
    for e in events {
        if let SysexStep::Complete(r) = asm.push(e.ump) {
            done.push(asm.message(r).bytes().to_vec());
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
        let mut pool = RtMidiEvents::new();
        drain_midi(&list, &mut pool);
        assert!(!pool.is_overflowed());
        assert!(pool.iter().all(|m| m.offset == 11));
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
    drain_midi(&list, &mut pool);
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
        drain_midi(&list, &mut pool);
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
    assert_eq!(count(&list), 1);
    let mut pool = RtMidiEvents::new();
    drain_midi(&list, &mut pool);
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
    let mut pool = RtMidiEvents::new();
    drain_midi(&list, &mut pool);
    assert_eq!(pool.len(), 1);
    assert!(!matches!(
        tutti_ump::decode(pool.as_slice()[0].ump),
        Ok(tutti_ump::Message::Sysex7(..))
    ));
}

/// **A SysEx the host's MIDI pool has no room for is dropped whole and
/// counted in `dropped`** (and latches the pool's `is_overflowed`); the
/// message before it arrives intact.
///
/// Mutation (run): drop `list.add_dropped(refused)` in `drain_midi`
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
    drain_midi(&list, &mut pool);
    assert!(pool.is_overflowed());
    assert_eq!(list.dropped(), 1);
    assert_eq!(reassembled(pool.as_slice()), vec![framed(&first)]);
}

/// **The plugin-SysEx path is allocation-free**: a 200-byte message
/// through `addEvent`, then more until the list refuses one, then
/// `drain_midi` over what it holds.
///
/// Mutation (run): a `Vec` copy of the message in `stage_plugin_sysex`
/// (`let _v = std::slice::from_raw_parts(bytes, size).to_vec();`), and one
/// in `drain_midi`'s SysEx arm → `assert_no_alloc` aborts.
#[test]
fn the_plugin_sysex_path_is_allocation_free() {
    let list = EventList::output();
    let ptr = list.to_com_ptr::<IEventList>().unwrap();
    let message = framed(&midi::sysex_payload(200));
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
        drain_midi(&list, &mut pool);
    });
    assert!(refused, "the arena refused one inside the gate");
    assert!(!pool.is_empty());
    assert!(list.dropped() > 0);
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
        .map(|i| vec![b'a' as u16 + (i % 26) as u16; LONG])
        .collect();
    let chords: Vec<Utf16Chord> = names
        .iter()
        .enumerate()
        .map(|(i, text)| Utf16Chord {
            sample_offset: i as i32,
            root: 0,
            bass_note: 0,
            mask: 0,
            text: text.clone(),
        })
        .collect();
    let sources = InputEvents {
        chords: &chords,
        ..Default::default()
    };

    let list = EventList::input();
    let mut stager = Stager::default();
    assert_no_alloc::assert_no_alloc(|| {
        assert!(stager.stage(&list, &sources));
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
        assert!(stager.stage(&list, &sources));
    });
    assert_eq!(list.text_dropped(), 2 * over);
}

/// Every SysEx `kDataEvent` the plugin reads through `getEvent`, as
/// `(sampleOffset, bytes)`, its bytes read through the pointer it is
/// handed.
fn sysex_read_by_plugin(list: &EventList) -> Vec<(i32, Vec<u8>)> {
    let mut out = Vec::new();
    // SAFETY: all-zero bytes is a valid value of this plain-data SDK struct.
    unsafe {
        for i in 0..list.getEventCount() {
            let mut e: Event = std::mem::zeroed();
            assert_eq!(list.getEvent(i, &mut e), kResultOk);
            if e.r#type != EventKind::Data as u16 {
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
/// Mutation (run): `Stager::stage_midi` encoding the event with
/// `MidiEncoder::encode` alone → only the PC → fails; pushing each message
/// without the room check → a partial bank in the full list → fails.
#[test]
fn a_banked_program_change_reaches_the_plugin_as_three_messages() {
    let pc = (
        at(5),
        midi::program_change(MidiGroup::FIRST, MidiChannel::of(1), 9, Some(0x0085)),
    );
    let list = EventList::input();
    let mut stager = Stager::default();
    assert_no_alloc::assert_no_alloc(|| stager.stage(&list, &midi_only(&[pc])));
    let mut seen = Vec::new();
    // SAFETY: all-zero bytes is a valid value of this plain-data SDK struct.
    unsafe {
        for i in 0..list.getEventCount() {
            let mut e: Event = std::mem::zeroed();
            assert_eq!(list.getEvent(i, &mut e), kResultOk);
            assert_eq!(e.r#type, EventKind::Data as u16);
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
    let note = (
        Offset::ZERO,
        midi::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, midi::MID_VELOCITY),
    );
    let mut midi = vec![note; EVENT_CAPACITY - 2];
    midi.push(pc);
    let full = EventList::input();
    let mut stager = Stager::default();
    assert!(stager.stage(&full, &midi_only(&midi)));
    assert_eq!(count(&full), EVENT_CAPACITY - 2);
    assert_eq!(full.dropped(), 3);
}

fn sysex_packets(payload: &[u8], frame: u32) -> Vec<UmpAt> {
    midi::sysex7_packets(payload)
        .into_iter()
        .map(|ump| (at(frame), ump))
        .collect()
}

fn framed(payload: &[u8]) -> Vec<u8> {
    tutti_ump::frame(payload).collect()
}

/// **A multi-packet SysEx reaches the plugin as one complete message**,
/// `0xF0 … 0xF7`, at its `END` packet's frame, beside the block's other
/// events, and the staging does not allocate.
///
/// Mutation (run): `Stager::stage_midi` encoding every event with
/// `MidiEncoder::encode` (no reassembly) → no SysEx event → fails.
#[test]
fn a_multi_packet_sysex_reaches_the_plugin_whole() {
    let body = midi::sysex_payload(100);
    let mut midi = sysex_packets(&body, 5);
    let last = midi.len() - 1;
    midi[last].0 = at(9);
    // On another Group: a note on the SysEx's own Group would end the message
    // (M2-104 section 7.7.1).
    midi.insert(
        3,
        (
            at(7),
            midi::note_on(MidiGroup::of(1), MidiChannel::FIRST, 60, midi::MID_VELOCITY),
        ),
    );
    let list = EventList::input();
    let mut stager = Stager::default();
    assert_no_alloc::assert_no_alloc(|| {
        stager.stage(
            &list,
            &InputEvents {
                midi: &midi,
                ..Default::default()
            },
        )
    });
    assert_eq!(sysex_read_by_plugin(&list), vec![(9, framed(&body))]);
    assert_eq!(count(&list), 2, "the note and the one SysEx");
    assert_eq!(list.sysex_dropped(), 0);
}

/// **A SysEx that spans blocks completes in the block of its `END`**;
/// the block holding only its start hands the plugin nothing of it.
///
/// Mutation (run): `EventList::begin` clearing the assembler's open run
/// (a `begin_block` that always clears) → the `END` finds no run → the
/// message never arrives → fails.
#[test]
fn a_sysex_spanning_blocks_completes_in_the_block_of_its_end() {
    let body = midi::sysex_payload(MIDI_STACK_CAPACITY * 6 + 100);
    let midi = sysex_packets(&body, 0);
    assert!(midi.len() > MIDI_STACK_CAPACITY);
    let list = EventList::input();
    let mut stager = Stager::default();
    let (first, second) = midi.split_at(MIDI_STACK_CAPACITY);
    assert!(stager.stage(&list, &midi_only(first)));
    assert!(sysex_read_by_plugin(&list).is_empty());
    // An empty block between them clears the list, keeping the run.
    assert!(list.clear());
    assert!(stager.stage(&list, &midi_only(second)));
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
    let long = midi::sysex_payload(SYSEX_MESSAGE_CAPACITY - 1); // framed: one past
    let list = EventList::input();
    let mut stager = Stager::default();
    for block in sysex_packets(&long, 0).chunks(MIDI_STACK_CAPACITY) {
        assert!(stager.stage(&list, &midi_only(block)));
        assert!(sysex_read_by_plugin(&list).is_empty());
    }
    assert_eq!(list.sysex_dropped(), 1);
    let next = midi::sysex_payload(20);
    assert!(stager.stage(&list, &midi_only(&sysex_packets(&next, 2))));
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
    let a = midi::sysex_payload(30);
    let b = midi::sysex_payload(20);
    let mut midi = sysex_packets(&a, 0);
    midi.truncate(2); // START and one CONTINUE
    midi.extend(sysex_packets(&b, 4));
    let list = EventList::input();
    let mut stager = Stager::default();
    assert!(stager.stage(&list, &midi_only(&midi)));
    assert_eq!(sysex_read_by_plugin(&list), vec![(4, framed(&b))]);
    assert_eq!(list.sysex_dropped(), 1);
}
