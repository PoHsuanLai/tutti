//! c event: part of the VST3 event vocabulary.

use super::*;

/// Converts the flat `Vst3Event` into the C `Event` struct the vst3 crate expects.
///
/// # Pointer lifetimes (the whole reason for the `'a` binding)
///
/// A VST3 `Event` is not self-contained: `DataEvent.bytes` and the chord /
/// scale / note-expression-text `text` fields are **borrowed pointers**, and a
/// plugin is entitled to read every one of them for the duration of the
/// `process` call in which it called `getEvent`. So the storage behind each
/// pointer must be stable for the whole block, not just until the next
/// `getEvent`.
///
/// Both pointers are therefore tied to `'a`, and `'a` is chosen to be the
/// borrow of the event list's per-block storage:
///
/// - `DataEvent.bytes` points **straight into `event`'s own inline `[u8; 16]`**,
///   which lives in the list's `events: Vec<Vst3Event>`. That Vec is filled
///   once per block by `update_from_sources` and only *read* by `getEvent`, so
///   the address is fixed for the block. (This deliberately replaced a
///   push-per-`getEvent` inline-then-heap scratch: it spilled to the heap on the
///   9th push and moved the inline elements, dangling every pointer already handed
///   to the plugin. Reserving capacity would not have fixed it — growth past
///   the reservation reallocates just the same. Borrowing the already-stable
///   event removes the copy, and with it the hazard.)
/// - `text_arena` owns the UTF-16 for text-bearing events; the event's
///   [`TextRef`] indexes into it and is resolved to a pointer here. It is
///   likewise interned at stage time and only read at `getEvent` time.
///
/// Text lengths are bounded by `MAX_EVENT_TEXT_LEN` (256) at intern time, so
/// arena offsets and lengths fit `u32`/`u16` regardless of pointer width, and
/// the VST3 struct fields they feed are exactly those widths.
#[allow(
    clippy::cast_possible_truncation,
    reason = "arena offsets and text lengths are bounded by MAX_EVENT_TEXT_LEN, \
              and a SysEx message by SYSEX_MESSAGE_CAPACITY"
)]
pub fn to_c_event<'a>(
    event: &'a Vst3Event,
    text_arena: &'a [u16],
    sysex: &'a [u8],
) -> vst3::Steinberg::Vst::Event {
    let header = event.header();

    // SAFETY: `Event` is plain data (integers, floats and a union of such
    // structs, plus raw pointers); all-zero bytes is a valid value, and the
    // header and the member the tag names are written below.
    let mut out: vst3::Steinberg::Vst::Event = unsafe { std::mem::zeroed() };
    out.busIndex = header.bus_index;
    out.sampleOffset = header.sample_offset;
    out.ppqPosition = header.ppq_position;
    out.flags = header.flags;
    out.r#type = header.event_type;

    // Resolve a TextRef to a (pointer, len) into the arena. A null pointer with
    // len 0 when the slice is out of range (defensive — staging keeps it valid).
    let resolve = |t: &TextRef| -> (*const u16, u16) {
        let start = t.start as usize;
        let end = start + t.len as usize;
        match text_arena.get(start..end) {
            Some(slice) => (slice.as_ptr(), t.len as u16),
            None => (std::ptr::null(), 0),
        }
    };

    match event {
        Vst3Event::NoteOn(e) => {
            out.__field0.noteOn = vst3::Steinberg::Vst::NoteOnEvent {
                channel: e.channel,
                pitch: e.pitch,
                tuning: e.tuning,
                velocity: e.velocity,
                length: e.length,
                noteId: e.note_id,
            };
        }
        Vst3Event::NoteOff(e) => {
            out.__field0.noteOff = vst3::Steinberg::Vst::NoteOffEvent {
                channel: e.channel,
                pitch: e.pitch,
                velocity: e.velocity,
                noteId: e.note_id,
                tuning: e.tuning,
            };
        }
        Vst3Event::Data(e) => {
            // Borrow the event's own inline bytes — stable for the whole block
            // (see the pointer-lifetime note on this function). No copy, no
            // scratch buffer, so no reallocation can move it out from under a
            // pointer already handed to the plugin.
            out.__field0.data = vst3::Steinberg::Vst::DataEvent {
                size: e.size,
                r#type: e.event_type,
                bytes: e.bytes.as_ptr(),
            };
        }
        Vst3Event::SysEx(e) => {
            // A null pointer with size 0 when the ref is out of range
            // (defensive: staging keeps it valid).
            let (bytes, size) = match sysex.get(e.bytes.range()) {
                Some(slice) => (slice.as_ptr(), slice.len() as u32),
                None => (std::ptr::null(), 0),
            };
            out.__field0.data = vst3::Steinberg::Vst::DataEvent {
                size,
                r#type: K_DATA_TYPE_MIDI_SYSEX,
                bytes,
            };
        }
        Vst3Event::PolyPressure(e) => {
            out.__field0.polyPressure = vst3::Steinberg::Vst::PolyPressureEvent {
                channel: e.channel,
                pitch: e.pitch,
                pressure: e.pressure,
                noteId: e.note_id,
            };
        }
        Vst3Event::NoteExpression(e) => {
            out.__field0.noteExpressionValue = vst3::Steinberg::Vst::NoteExpressionValueEvent {
                typeId: e.type_id,
                noteId: e.note_id,
                value: e.value,
            };
        }
        Vst3Event::NoteExpressionText(e) => {
            let (text, text_len) = resolve(&e.text);
            out.__field0.noteExpressionText = vst3::Steinberg::Vst::NoteExpressionTextEvent {
                typeId: e.type_id,
                noteId: e.note_id,
                textLen: text_len as u32,
                text,
            };
        }
        Vst3Event::Chord(e) => {
            let (text, text_len) = resolve(&e.text);
            out.__field0.chord = vst3::Steinberg::Vst::ChordEvent {
                root: e.root,
                bassNote: e.bass_note,
                mask: e.mask,
                textLen: text_len,
                text,
            };
        }
        Vst3Event::Scale(e) => {
            let (text, text_len) = resolve(&e.text);
            out.__field0.scale = vst3::Steinberg::Vst::ScaleEvent {
                root: e.root,
                mask: e.mask,
                textLen: text_len,
                text,
            };
        }
        Vst3Event::NoteExpressionInt(e) => {
            out.__field0.noteExpressionIntValue =
                vst3::Steinberg::Vst::NoteExpressionIntValueEvent {
                    typeId: e.type_id,
                    noteId: e.note_id,
                    value: e.value as u64,
                };
        }
        Vst3Event::LegacyMidiCcOut(e) => {
            out.__field0.midiCCOut = vst3::Steinberg::Vst::LegacyMIDICCOutEvent {
                controlNumber: e.control_number,
                channel: e.channel,
                value: e.value,
                value2: e.value2,
            };
        }
    }

    out
}

/// Converts from the vst3 crate's tagged-union `Event` to the safe enum.
///
/// `text_arena` owns the UTF-16 for any chord / scale / note-expression-text
/// event decoded here (these arrive on a plugin's *output* event list); the
/// returned [`TextRef`] indexes the bytes appended to it. Pass the same arena
/// the resulting `Vst3Event` will be read against.
///
/// # Safety
///
/// `event.type_` must accurately label the variant stored in `__field0`. For a chord, scale or
/// note-expression-text event, its text pointer must be null or valid for reads of
/// `min(text length, MAX_EVENT_TEXT_LEN)` `u16`s (the `intern` closure builds a slice of that
/// length from it).
#[allow(
    clippy::unnecessary_cast,
    reason = "the SDK event ordinals are u32 on unix and c_int on Windows; each \
              cast is a no-op on one target and load-bearing on the other"
)]
pub unsafe fn from_c_event(
    event: &vst3::Steinberg::Vst::Event,
    text_arena: &mut TextArena,
) -> Option<Vst3Event> {
    let header = EventHeader {
        bus_index: event.busIndex,
        sample_offset: event.sampleOffset,
        ppq_position: event.ppqPosition,
        flags: event.flags,
        event_type: event.r#type,
    };

    // Copy a plugin-supplied (ptr, len) UTF-16 string into the arena, returning
    // the TextRef. Null pointer or zero length yields an empty ref; so does a
    // text the arena has no reserved room left for (`TextArena::intern`).
    let intern = |arena: &mut TextArena, ptr: *const u16, len: usize| -> TextRef {
        let n = len.min(MAX_EVENT_TEXT_LEN);
        if ptr.is_null() || n == 0 {
            return TextRef::default();
        }
        // SAFETY: `ptr` is non-null and valid for `n` `u16`s, by this fn's `# Safety`.
        arena.intern(unsafe { std::slice::from_raw_parts(ptr, n) })
    };

    match event.r#type as u32 {
        t if t == EventTypes_::kNoteOnEvent as u32 => {
            // SAFETY: this arm runs only for `kNoteOnEvent`, the tag that names `noteOn` as the live union member.
            let e = unsafe { event.__field0.noteOn };
            Some(Vst3Event::NoteOn(NoteOnEvent {
                header,
                channel: e.channel,
                pitch: e.pitch,
                tuning: e.tuning,
                velocity: e.velocity,
                length: e.length,
                note_id: e.noteId,
            }))
        }
        t if t == EventTypes_::kNoteOffEvent as u32 => {
            // SAFETY: this arm runs only for `kNoteOffEvent`, the tag that names `noteOff` as the live union member.
            let e = unsafe { event.__field0.noteOff };
            Some(Vst3Event::NoteOff(NoteOffEvent {
                header,
                channel: e.channel,
                pitch: e.pitch,
                velocity: e.velocity,
                note_id: e.noteId,
                tuning: e.tuning,
            }))
        }
        t if t == EventTypes_::kDataEvent as u32 => {
            // SAFETY: this arm runs only for `kDataEvent`, the tag that names `data` as the live union member.
            let e = unsafe { event.__field0.data };
            let mut bytes = [0u8; 16];
            if !e.bytes.is_null() && e.size > 0 {
                let copy_len = (e.size as usize).min(bytes.len());
                // SAFETY: `e.bytes` is non-null (checked above) and the ABI makes it valid for `e.size` bytes; at most 16 (the destination's length) are copied, and the regions are distinct.
                unsafe { std::ptr::copy_nonoverlapping(e.bytes, bytes.as_mut_ptr(), copy_len) };
            }
            Some(Vst3Event::Data(DataEvent {
                header,
                size: e.size.min(16),
                event_type: e.r#type,
                bytes,
            }))
        }
        t if t == EventTypes_::kPolyPressureEvent as u32 => {
            // SAFETY: this arm runs only for `kPolyPressureEvent`, the tag that names `polyPressure` as the live union member.
            let e = unsafe { event.__field0.polyPressure };
            Some(Vst3Event::PolyPressure(PolyPressureEvent {
                header,
                channel: e.channel,
                pitch: e.pitch,
                pressure: e.pressure,
                note_id: e.noteId,
            }))
        }
        t if t == EventTypes_::kNoteExpressionValueEvent as u32 => {
            // SAFETY: this arm runs only for `kNoteExpressionValueEvent`, the tag that names `noteExpressionValue` as the live union member.
            let e = unsafe { event.__field0.noteExpressionValue };
            Some(Vst3Event::NoteExpression(NoteExpressionValueEvent {
                header,
                note_id: e.noteId,
                type_id: e.typeId,
                value: e.value,
            }))
        }
        t if t == EventTypes_::kNoteExpressionTextEvent as u32 => {
            // SAFETY: this arm runs only for `kNoteExpressionTextEvent`, the tag that names `noteExpressionText` as the live union member.
            let e = unsafe { event.__field0.noteExpressionText };
            let text = intern(text_arena, e.text, e.textLen as usize);
            Some(Vst3Event::NoteExpressionText(NoteExpressionTextEvent {
                header,
                type_id: e.typeId,
                note_id: e.noteId,
                text,
            }))
        }
        t if t == EventTypes_::kChordEvent as u32 => {
            // SAFETY: this arm runs only for `kChordEvent`, the tag that names `chord` as the live union member.
            let e = unsafe { event.__field0.chord };
            let text = intern(text_arena, e.text, e.textLen as usize);
            Some(Vst3Event::Chord(ChordEvent {
                header,
                root: e.root,
                bass_note: e.bassNote,
                mask: e.mask,
                text,
            }))
        }
        t if t == EventTypes_::kScaleEvent as u32 => {
            // SAFETY: this arm runs only for `kScaleEvent`, the tag that names `scale` as the live union member.
            let e = unsafe { event.__field0.scale };
            let text = intern(text_arena, e.text, e.textLen as usize);
            Some(Vst3Event::Scale(ScaleEvent {
                header,
                root: e.root,
                mask: e.mask,
                text,
            }))
        }
        t if t == EventTypes_::kNoteExpressionIntValueEvent as u32 => {
            // SAFETY: this arm runs only for `kNoteExpressionIntValueEvent`, the tag that names `noteExpressionIntValue` as the live union member.
            let e = unsafe { event.__field0.noteExpressionIntValue };
            Some(Vst3Event::NoteExpressionInt(NoteExpressionIntValueEvent {
                header,
                type_id: e.typeId,
                note_id: e.noteId,
                value: reinterpret_expression_value(e.value),
            }))
        }
        t if t == EventTypes_::kLegacyMIDICCOutEvent as u32 => {
            // SAFETY: this arm runs only for `kLegacyMIDICCOutEvent`, the tag that names `midiCCOut` as the live union member.
            let e = unsafe { event.__field0.midiCCOut };
            Some(Vst3Event::LegacyMidiCcOut(LegacyMidiCcOutEvent {
                header,
                control_number: e.controlNumber,
                channel: e.channel,
                value: e.value,
                value2: e.value2,
            }))
        }
        _ => None,
    }
}
