//! VST3 event types and bidirectional conversion to/from the Tutti
//! [`tutti_midi_types::MidiEvent`] UMP representation.
//!
//! VST3's native event list is richer than raw MIDI-1 wire data (typed
//! note-on/off/poly-pressure structs with `f32` velocity plus generic
//! `Data` events for CC / ProgramChange / ChannelPressure / PitchBend). The
//! [`Vst3Event::from_midi`] / [`Vst3Event::to_midi`] helpers bridge it to the
//! workspace's canonical [`MidiEvent`] UMP type — one `MidiEvent` maps to one
//! [`Vst3Event`] and round-trips losslessly for the MIDI-representable
//! variants.

//! # Narrowing casts are denied in this module
//!
//! This file is a boundary between our vocabulary and the VST3 C ABI, and every
//! bug it has had was a cast that silently changed a value's meaning: a `u32`
//! frame offset wrapping negative into an `i32` `sampleOffset`, and a release
//! velocity dropped on the way through. Both compiled without complaint.
//!
//! So truncating and sign-changing casts are denied here. Where a cast is
//! genuinely safe, the `#[allow(..., reason = "...")]` states why — the
//! justification is the point, not the lint. A cast nobody can justify is the
//! bug.
//!
//! Scoped to this module deliberately. Enabling these lints crate-wide produces
//! hundreds of warnings that get scrolled past, which is how the two above
//! survived review; a small denied surface that must be argued with is worth
//! more than a large warned one that is not read.
#![deny(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]

pub use tutti_midi_types::MidiEvent;

use tutti_plugin_types::{note_id_for, NoteExpressionType, NoteExpressionValue};

use tutti_midi_types::tutti_types::{CCNumber, MidiChannel, MidiGroup};
use vst3::Steinberg::Vst::Event_::EventTypes_;

/// Borrowed bundle of every input event stream staged into a VST3 plugin's
/// event list for one processing block: MIDI plus the four per-note-expressive
/// families (chords, scales, expression texts, integer expressions). Grouping
/// them keeps the `process` / `update_from_sources` signatures readable rather
/// than threading six parallel slices.
#[derive(Clone, Copy, Default)]
pub struct Vst3InputEvents<'a> {
    pub midi: &'a [MidiEvent],
    pub note_expressions: &'a [NoteExpressionValue],
    pub chords: &'a [ChordValue],
    pub scales: &'a [ScaleValue],
    pub expr_texts: &'a [NoteExpressionText],
    pub expr_ints: &'a [NoteExpressionIntValue],
}

impl Vst3InputEvents<'_> {
    /// True when at least one stream carries an event this block.
    pub fn is_empty(&self) -> bool {
        self.midi.is_empty()
            && self.note_expressions.is_empty()
            && self.chords.is_empty()
            && self.scales.is_empty()
            && self.expr_texts.is_empty()
            && self.expr_ints.is_empty()
    }
}

/// `type_` discriminant for note-on events.
#[allow(
    clippy::cast_possible_truncation,
    reason = "SDK event-type ordinals, all < 16"
)]
pub const K_NOTE_ON_EVENT: u16 = EventTypes_::kNoteOnEvent as u16;
/// `type_` discriminant for note-off events.
#[allow(
    clippy::cast_possible_truncation,
    reason = "SDK event-type ordinals, all < 16"
)]
pub const K_NOTE_OFF_EVENT: u16 = EventTypes_::kNoteOffEvent as u16;
/// `type_` discriminant for raw-data events (CC, pitch bend, program change, …).
#[allow(
    clippy::cast_possible_truncation,
    reason = "SDK event-type ordinals, all < 16"
)]
pub const K_DATA_EVENT: u16 = EventTypes_::kDataEvent as u16;
/// `type_` discriminant for poly-pressure events.
#[allow(
    clippy::cast_possible_truncation,
    reason = "SDK event-type ordinals, all < 16"
)]
pub const K_POLY_PRESSURE_EVENT: u16 = EventTypes_::kPolyPressureEvent as u16;
/// `type_` discriminant for note-expression value events.
#[allow(
    clippy::cast_possible_truncation,
    reason = "SDK event-type ordinals, all < 16"
)]
pub const K_NOTE_EXPRESSION_VALUE_EVENT: u16 = EventTypes_::kNoteExpressionValueEvent as u16;
/// `type_` discriminant for note-expression *text* events.
#[allow(
    clippy::cast_possible_truncation,
    reason = "SDK event-type ordinals, all < 16"
)]
pub const K_NOTE_EXPRESSION_TEXT_EVENT: u16 = EventTypes_::kNoteExpressionTextEvent as u16;
/// `type_` discriminant for chord events.
#[allow(
    clippy::cast_possible_truncation,
    reason = "SDK event-type ordinals, all < 16"
)]
pub const K_CHORD_EVENT: u16 = EventTypes_::kChordEvent as u16;
/// `type_` discriminant for scale events.
#[allow(
    clippy::cast_possible_truncation,
    reason = "SDK event-type ordinals, all < 16"
)]
pub const K_SCALE_EVENT: u16 = EventTypes_::kScaleEvent as u16;
/// `type_` discriminant for note-expression integer-value events.
#[allow(
    clippy::cast_possible_truncation,
    reason = "SDK event-type ordinals, all < 16"
)]
pub const K_NOTE_EXPRESSION_INT_VALUE_EVENT: u16 = EventTypes_::kNoteExpressionIntValueEvent as u16;
/// `type_` discriminant for legacy-MIDI-CC-out events (plugin → host, value 0xFFFF).
#[allow(
    clippy::cast_possible_truncation,
    reason = "SDK event-type ordinals, all < 16"
)]
pub const K_LEGACY_MIDI_CC_OUT_EVENT: u16 = EventTypes_::kLegacyMIDICCOutEvent as u16;
/// `DataEvent.type` subtype marking the payload as a MIDI SysEx message.
///
/// Cast explicitly like the discriminants above: the generated `vst3` bindings
/// give this constant `u32` on Unix but `i32` on Windows, so an uncast
/// initializer only compiles on one of them.
// The cast is a no-op on Unix (where the binding is already `u32`) and a real
// `i32 as u32` on Windows; allow the former's `unnecessary_cast` lint.
#[allow(clippy::unnecessary_cast)]
pub const K_DATA_TYPE_MIDI_SYSEX: u32 =
    vst3::Steinberg::Vst::DataEvent_::DataTypes_::kMidiSysEx as u32;

/// Flat Rust-facing header merging the `busIndex` / `sampleOffset` /
/// `ppqPosition` / `flags` / `type_` fields of `vst3::Steinberg::Vst::Event`
/// so callers can construct events literally.
#[derive(Debug, Clone, Copy, Default)]
pub struct EventHeader {
    /// Event bus index (0 for typical single-bus plugins).
    pub bus_index: i32,
    /// Frame offset within the current processing block.
    pub sample_offset: i32,
    /// Musical position in quarter notes, or 0 if unknown.
    pub ppq_position: f64,
    /// Flags bitfield (see VST3 `EventFlags`).
    pub flags: u16,
    /// One of the `K_*_EVENT` discriminants.
    pub event_type: u16,
}

/// Note-on event. Velocity is normalized to `0.0..=1.0`.
#[derive(Debug, Clone, Copy)]
pub struct NoteOnEvent {
    pub header: EventHeader,
    pub channel: i16,
    pub pitch: i16,
    /// Fractional tuning offset from 12-TET, in semitones.
    pub tuning: f32,
    /// Normalized velocity (0.0 – 1.0).
    pub velocity: f32,
    /// Note length in samples; 0 if unknown.
    pub length: i32,
    /// Plugin-assigned note id, or `-1` if channel/pitch-based.
    pub note_id: i32,
}

/// Note-off event. Velocity is normalized to `0.0..=1.0`.
#[derive(Debug, Clone, Copy)]
pub struct NoteOffEvent {
    pub header: EventHeader,
    pub channel: i16,
    pub pitch: i16,
    /// Normalized release velocity (0.0 – 1.0).
    pub velocity: f32,
    /// Plugin-assigned note id matching the originating note-on, or `-1`.
    pub note_id: i32,
    pub tuning: f32,
}

/// Generic raw-bytes event — used by VST3 for CC, pitch bend, program change,
/// channel pressure, and SysEx.
#[derive(Debug, Clone, Copy)]
pub struct DataEvent {
    pub header: EventHeader,
    /// Valid byte count in `bytes`.
    pub size: u32,
    /// Data subtype (e.g. `DataEvent::DataTypes::kMidiSysEx`).
    pub event_type: u32,
    /// Inline payload. Only the first `size` bytes are valid.
    pub bytes: [u8; 16],
}

/// Polyphonic pressure (per-note aftertouch).
#[derive(Debug, Clone, Copy)]
pub struct PolyPressureEvent {
    pub header: EventHeader,
    pub channel: i16,
    pub pitch: i16,
    /// Normalized pressure (0.0 – 1.0).
    pub pressure: f32,
    pub note_id: i32,
}

/// Per-note expression value. Specific to a note id and expression type
/// rather than a channel.
#[derive(Debug, Clone, Copy)]
pub struct NoteExpressionValueEvent {
    pub header: EventHeader,
    pub note_id: i32,
    /// VST3 `NoteExpressionTypeIDs`: 0=volume, 1=pan, 2=tuning, 3=vibrato,
    /// 4=expression, 5=brightness. See [`note_expression_type_to_id`].
    pub type_id: u32,
    /// 0.0 to 1.0, meaning depends on type_id.
    pub value: f64,
}

/// A `(start, len)` slice into an `EventList`'s UTF-16 text arena.
///
/// VST3's chord / scale / note-expression-text events carry a borrowed
/// `const TChar*` (UTF-16) that must outlive the `process` call. To keep
/// [`Vst3Event`] `Copy` (no owned heap per event), the string lives in a shared
/// arena owned by the `EventList` — written only while staging, cleared once
/// per block — and the event holds only this index into it. `len` counts `u16`
/// code units, excluding any terminator.
///
/// `DataEvent` needs no arena: its payload is already an inline `[u8; 16]` in
/// the event, and [`to_c_event`] points the C struct straight at it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TextRef {
    pub start: u32,
    pub len: u32,
}

/// Per-note text annotation (e.g. lyric, ornament name). `text` indexes the
/// owning event list's arena; `type_id` is a VST3 note-expression type id.
#[derive(Debug, Clone, Copy)]
pub struct NoteExpressionTextEvent {
    pub header: EventHeader,
    pub type_id: u32,
    pub note_id: i32,
    pub text: TextRef,
}

/// Current chord context: root + bass note (0..127), a degree `mask`, and a
/// display name in the arena. Drives chord-aware instruments / harmonizers.
#[derive(Debug, Clone, Copy)]
pub struct ChordEvent {
    pub header: EventHeader,
    pub root: i16,
    pub bass_note: i16,
    pub mask: i16,
    pub text: TextRef,
}

/// Current scale/key context: root (0..127) + a 12-bit `mask` of scale degrees
/// (bit 0 = C), and a display name in the arena.
#[derive(Debug, Clone, Copy)]
pub struct ScaleEvent {
    pub header: EventHeader,
    pub root: i16,
    pub mask: i16,
    pub text: TextRef,
}

/// Integer-valued per-note expression, the `i64` counterpart to
/// [`NoteExpressionValueEvent`] (used for stepped / enumerated dimensions).
#[derive(Debug, Clone, Copy)]
pub struct NoteExpressionIntValueEvent {
    pub header: EventHeader,
    pub type_id: u32,
    pub note_id: i32,
    pub value: i64,
}

/// Legacy MIDI CC emitted by the plugin back to the host (arpeggiators, MIDI
/// effects). `control_number` is a `ControllerNumbers` index; `value2` carries
/// the second data byte for pitch-bend / poly-pressure. Output-only.
#[derive(Debug, Clone, Copy)]
pub struct LegacyMidiCcOutEvent {
    pub header: EventHeader,
    pub control_number: u8,
    pub channel: i8,
    pub value: i8,
    pub value2: i8,
}

/// Safe tagged-enum form of the VST3 `Event` union. See
/// [`Vst3Event::from_midi`] / [`Vst3Event::to_midi`] for round-trip MIDI
/// conversion. Text-bearing variants reference the owning event list's arena
/// (see [`TextRef`]) so the enum stays `Copy`.
#[derive(Debug, Clone, Copy)]
pub enum Vst3Event {
    NoteOn(NoteOnEvent),
    NoteOff(NoteOffEvent),
    Data(DataEvent),
    PolyPressure(PolyPressureEvent),
    NoteExpression(NoteExpressionValueEvent),
    NoteExpressionText(NoteExpressionTextEvent),
    Chord(ChordEvent),
    Scale(ScaleEvent),
    NoteExpressionInt(NoteExpressionIntValueEvent),
    LegacyMidiCcOut(LegacyMidiCcOutEvent),
}

impl Vst3Event {
    /// Frame offset within the current processing block, from the underlying
    /// [`EventHeader`].
    pub fn sample_offset(&self) -> i32 {
        self.header().sample_offset
    }

    /// The common [`EventHeader`] of any variant.
    pub fn header(&self) -> &EventHeader {
        match self {
            Vst3Event::NoteOn(e) => &e.header,
            Vst3Event::NoteOff(e) => &e.header,
            Vst3Event::Data(e) => &e.header,
            Vst3Event::PolyPressure(e) => &e.header,
            Vst3Event::NoteExpression(e) => &e.header,
            Vst3Event::NoteExpressionText(e) => &e.header,
            Vst3Event::Chord(e) => &e.header,
            Vst3Event::Scale(e) => &e.header,
            Vst3Event::NoteExpressionInt(e) => &e.header,
            Vst3Event::LegacyMidiCcOut(e) => &e.header,
        }
    }

    /// Encode a Tutti UMP [`MidiEvent`] as a [`Vst3Event`]. Notes and
    /// poly-pressure keep MIDI-2 full-width resolution; per-note messages become
    /// note-expression events; everything else becomes a MIDI-1 [`Vst3Event::Data`]
    /// frame. `None` only for messages with no MIDI-1 form and no per-note mapping.
    #[inline]
    pub fn from_midi(event: &MidiEvent) -> Option<Self> {
        vst3_event_from_midi(event)
    }

    /// Decode this [`Vst3Event`] into a Tutti UMP [`MidiEvent`], promoting any
    /// MIDI-1 payload to Channel Voice 2. `None` for non-MIDI events. The inverse
    /// of [`Vst3Event::from_midi`].
    #[inline]
    pub fn to_midi(&self) -> Option<MidiEvent> {
        vst3_to_midi_event(self)
    }
}

/// Convert our flat `Vst3Event` into the C `Event` struct the vst3 crate expects.
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
///   push-per-`getEvent` `SmallVec` scratch: it spilled to the heap on the 9th
///   push and moved the inline elements, dangling every pointer already handed
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
    reason = "arena offsets and text lengths are bounded by MAX_EVENT_TEXT_LEN"
)]
pub(crate) fn to_c_event<'a>(
    event: &'a Vst3Event,
    text_arena: &'a [u16],
) -> vst3::Steinberg::Vst::Event {
    let header = event.header();

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

/// Largest UTF-16 text we copy out of a plugin-supplied chord/scale/text event.
/// Plugin display names are short; this caps a malicious/garbage `textLen`.
const MAX_EVENT_TEXT_LEN: usize = 256;

/// Convert from the vst3 crate's tagged-union `Event` to our safe enum.
///
/// `text_arena` owns the UTF-16 for any chord / scale / note-expression-text
/// event decoded here (these arrive on a plugin's *output* event list); the
/// returned [`TextRef`] indexes the bytes appended to it. Pass the same arena
/// the resulting `Vst3Event` will be read against.
///
/// # Safety
///
/// `event.type_` must accurately label the variant stored in `__field0`.
#[allow(clippy::unnecessary_cast)]
pub(crate) unsafe fn from_c_event(
    event: &vst3::Steinberg::Vst::Event,
    text_arena: &mut smallvec::SmallVec<[u16; 256]>,
) -> Option<Vst3Event> {
    let header = EventHeader {
        bus_index: event.busIndex,
        sample_offset: event.sampleOffset,
        ppq_position: event.ppqPosition,
        flags: event.flags,
        event_type: event.r#type,
    };

    // Copy a plugin-supplied (ptr, len) UTF-16 string into the arena, returning
    // the TextRef. Null pointer or zero length yields an empty ref.
    #[allow(
        clippy::cast_possible_truncation,
        reason = "n is clamped to MAX_EVENT_TEXT_LEN and the arena holds at most \
                  that many per event, so both fit u32"
    )]
    let intern =
        |arena: &mut smallvec::SmallVec<[u16; 256]>, ptr: *const u16, len: usize| -> TextRef {
            let n = len.min(MAX_EVENT_TEXT_LEN);
            if ptr.is_null() || n == 0 {
                return TextRef::default();
            }
            let start = arena.len() as u32;
            arena.extend(std::slice::from_raw_parts(ptr, n).iter().copied());
            TextRef {
                start,
                len: n as u32,
            }
        };

    match event.r#type as u32 {
        t if t == EventTypes_::kNoteOnEvent as u32 => {
            let e = event.__field0.noteOn;
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
            let e = event.__field0.noteOff;
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
            let e = event.__field0.data;
            let mut bytes = [0u8; 16];
            if !e.bytes.is_null() && e.size > 0 {
                let copy_len = (e.size as usize).min(bytes.len());
                std::ptr::copy_nonoverlapping(e.bytes, bytes.as_mut_ptr(), copy_len);
            }
            Some(Vst3Event::Data(DataEvent {
                header,
                size: e.size.min(16),
                event_type: e.r#type,
                bytes,
            }))
        }
        t if t == EventTypes_::kPolyPressureEvent as u32 => {
            let e = event.__field0.polyPressure;
            Some(Vst3Event::PolyPressure(PolyPressureEvent {
                header,
                channel: e.channel,
                pitch: e.pitch,
                pressure: e.pressure,
                note_id: e.noteId,
            }))
        }
        t if t == EventTypes_::kNoteExpressionValueEvent as u32 => {
            let e = event.__field0.noteExpressionValue;
            Some(Vst3Event::NoteExpression(NoteExpressionValueEvent {
                header,
                note_id: e.noteId,
                type_id: e.typeId,
                value: e.value,
            }))
        }
        t if t == EventTypes_::kNoteExpressionTextEvent as u32 => {
            let e = event.__field0.noteExpressionText;
            let text = intern(text_arena, e.text, e.textLen as usize);
            Some(Vst3Event::NoteExpressionText(NoteExpressionTextEvent {
                header,
                type_id: e.typeId,
                note_id: e.noteId,
                text,
            }))
        }
        t if t == EventTypes_::kChordEvent as u32 => {
            let e = event.__field0.chord;
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
            let e = event.__field0.scale;
            let text = intern(text_arena, e.text, e.textLen as usize);
            Some(Vst3Event::Scale(ScaleEvent {
                header,
                root: e.root,
                mask: e.mask,
                text,
            }))
        }
        t if t == EventTypes_::kNoteExpressionIntValueEvent as u32 => {
            let e = event.__field0.noteExpressionIntValue;
            Some(Vst3Event::NoteExpressionInt(NoteExpressionIntValueEvent {
                header,
                type_id: e.typeId,
                note_id: e.noteId,
                value: reinterpret_expression_value(e.value),
            }))
        }
        t if t == EventTypes_::kLegacyMIDICCOutEvent as u32 => {
            let e = event.__field0.midiCCOut;
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

/// Encode a Tutti UMP [`MidiEvent`] as a [`Vst3Event`].
///
/// Notes and poly-pressure are matched on `midi2`'s Channel Voice 2 vocabulary
/// via [`tutti_midi_types::normalize`], so velocity / pressure arrive at the
/// plugin at the source event's full bit width (converted to VST3's `f32` 0..1
/// here) rather than re-quantized through 7-bit MIDI-1 — a MIDI-2 note-on keeps
/// its 16-bit velocity. MIDI-2 per-note pitch bend and per-note controllers
/// become [`Vst3Event::NoteExpression`] events bound to the matching voice via
/// [`note_id_for`]. Everything else (CC, channel-wide pitch bend, program change,
/// channel pressure, SysEx) becomes a [`Vst3Event::Data`] event, which is a
/// 3-byte MIDI-1 frame by definition, so that branch stays on the byte form.
/// Returns `None` only for messages with no MIDI-1 byte representation and no
/// per-note mapping.
/// Text sizes are bounded as in `to_c_event`.
///
/// **`cast_possible_wrap` is deliberately NOT allowed here.** An earlier version
/// of this attribute listed it, to cover the `i64` note-expression slot — and
/// that blanket allow silently re-permitted the `frame_offset as i32` wrap this
/// module exists to prevent, verified by reintroducing the bug and watching it
/// compile. The sign-changing casts now go through named helpers
/// (`reinterpret_expression_value`) that carry their own narrow allow, so the
/// deny still bites in the function body.
#[allow(
    clippy::cast_possible_truncation,
    reason = "text sizes bounded by MAX_EVENT_TEXT_LEN"
)]
pub(crate) fn vst3_event_from_midi(event: &MidiEvent) -> Option<Vst3Event> {
    use tutti_midi_types::convert::{bend_u32_to_signed_f32, u16_to_unit_f32, u32_to_unit_f32};
    use tutti_midi_types::midi2::channel_voice2::ChannelVoice2 as Cv2;
    use tutti_midi_types::midi2::{Channeled, UmpMessage};

    // `frame_offset` is `u32`; VST3's `sampleOffset` is `i32`. A value past
    // `i32::MAX` wraps negative, and the plugin indexes its buffers with it —
    // an out-of-bounds read inside the plugin, not in our code. Saturating keeps
    // it in range; the mirror path (`vst3_to_midi_event`) already guards with
    // `.max(0)` and this direction was left unguarded.
    let sample_offset = i32::try_from(event.frame_offset).unwrap_or(i32::MAX);
    let header = EventHeader {
        bus_index: 0,
        sample_offset,
        ppq_position: 0.0,
        flags: 0,
        event_type: 0,
    };

    // Notes, poly-pressure, and per-note expression get VST3's typed structs
    // with full-width f32 values; the rest fall through to a 3-byte Data event.
    let normalized = tutti_midi_types::normalize(event);
    if let Ok(UmpMessage::ChannelVoice2(cv2)) = UmpMessage::try_from(normalized.data_words()) {
        let channel = u8::from(cv2.channel());
        match cv2 {
            Cv2::NoteOn(m) => {
                let note = u8::from(m.note_number());
                return Some(Vst3Event::NoteOn(NoteOnEvent {
                    header: EventHeader {
                        event_type: K_NOTE_ON_EVENT,
                        ..header
                    },
                    channel: channel as i16,
                    pitch: note as i16,
                    tuning: 0.0,
                    velocity: u16_to_unit_f32(m.velocity()),
                    length: 0,
                    note_id: note_id_for(channel, note),
                }));
            }
            Cv2::NoteOff(m) => {
                let note = u8::from(m.note_number());
                return Some(Vst3Event::NoteOff(NoteOffEvent {
                    header: EventHeader {
                        event_type: K_NOTE_OFF_EVENT,
                        ..header
                    },
                    channel: channel as i16,
                    pitch: note as i16,
                    // VST3's NoteOffEvent.velocity is the normalized *release*
                    // velocity, and MIDI-2 note-off carries a real 16-bit one.
                    // Hardcoding 0.0 gave every release-velocity-sensitive
                    // instrument (piano and orchestral release layers) the
                    // minimum on every note-off. The note-on path beside this
                    // one already converts via `u16_to_unit_f32`.
                    velocity: u16_to_unit_f32(m.velocity()),
                    note_id: note_id_for(channel, note),
                    tuning: 0.0,
                }));
            }
            Cv2::KeyPressure(m) => {
                let note = u8::from(m.note_number());
                return Some(Vst3Event::PolyPressure(PolyPressureEvent {
                    header: EventHeader {
                        event_type: K_POLY_PRESSURE_EVENT,
                        ..header
                    },
                    channel: channel as i16,
                    pitch: note as i16,
                    pressure: u32_to_unit_f32(m.key_pressure_data()),
                    note_id: note_id_for(channel, note),
                }));
            }
            // MIDI-2 per-note pitch bend → VST3 tuning expression. Signed [-1, 1]
            // (center 0) maps to the unit [0, 1] (center 0.5 = no detune) VST3's
            // note-expression value convention uses.
            Cv2::PerNotePitchBend(m) => {
                let note = u8::from(m.note_number());
                let value = bend_u32_to_signed_f32(m.pitch_bend_data());
                // Tuning is VST3-encodable, so `note_expression_to_vst3` is `Some`.
                return note_expression_to_vst3(&NoteExpressionValue {
                    sample_offset,
                    note_id: note_id_for(channel, note),
                    expression_type: NoteExpressionType::Tuning,
                    value: (f64::from(value) + 1.0) / 2.0,
                });
            }
            // MIDI-2 per-note controllers map onto VST3 note-expression
            // dimensions for the indices that have a standard expression
            // counterpart; other per-note CCs have no VST3 equivalent and drop.
            // Both Registered and Assignable per-note controllers are handled;
            // the index namespace is the same 8-bit space.
            Cv2::RegisteredPerNoteController(m) => {
                let note = u8::from(m.note_number());
                return registered_controller_expression(m.controller()).and_then(
                    |(expression_type, data)| {
                        note_expression_to_vst3(&NoteExpressionValue {
                            sample_offset,
                            note_id: note_id_for(channel, note),
                            expression_type,
                            value: f64::from(u32_to_unit_f32(data)),
                        })
                    },
                );
            }
            Cv2::AssignablePerNoteController(m) => {
                let note = u8::from(m.note_number());
                let value = u32_to_unit_f32(m.controller_data());
                return per_note_controller_expression(m.index()).and_then(|expression_type| {
                    note_expression_to_vst3(&NoteExpressionValue {
                        sample_offset,
                        note_id: note_id_for(channel, note),
                        expression_type,
                        value: f64::from(value),
                    })
                });
            }
            // CC / channel pressure / channel pitch bend / program change carry
            // no extra resolution VST3 can use here — they ride the plugin's
            // parameter funnel (see `CcRoute`) or land as a raw Data event below.
            _ => {}
        }
    }

    // SysEx → DataEvent with the kMidiSysEx subtype. VST3 wants the complete
    // message *with* its 0xF0 … 0xF7 delimiters, carried inline in the flat
    // event's fixed `[u8; 16]` slot.
    //
    // Single-packet (`SINGLE`) SysEx is fully supported: the payload fits a UMP
    // type-0x3 packet (≤ 6 bytes → ≤ 8 with delimiters), well within 16 bytes.
    //
    // Multi-packet SysEx is a KNOWN LIMITATION, not silently swallowed:
    // reassembling Start/Continue/End fragments into one message requires an
    // owned, growable buffer that spans events — which would force
    // [`Vst3Event`] to heap-allocate and lose its `Copy` + no-alloc RT
    // guarantee (see `vst3_event_is_copy` and the RT no-alloc harness). Rather
    // than pay that cost for a rare case, we degrade explicitly:
    // - `START`  → forward the opening 0xF0 + first-packet bytes (no 0xF7; the
    //   message is deliberately left unterminated because the rest didn't fit).
    //   A plugin sees the message *begin*; it is not dropped without trace.
    // - `CONTINUE` / `END` → dropped, because a mid/tail fragment carries no
    //   0xF0 start and cannot stand alone as a VST3 Data event.
    //
    // TODO: if a plugin that streams large SysEx (firmware/sample dumps) turns
    // up, add a per-instance reassembly buffer *outside* the per-event converter
    // (in the block-scoped EventList) and emit one complete Data event on `END`.
    if let Some((status, payload, n)) = event.sysex7_payload() {
        use tutti_midi_types::ump::{SYSEX7_STATUS_SINGLE, SYSEX7_STATUS_START};
        if status == SYSEX7_STATUS_SINGLE {
            let mut bytes = [0u8; 16];
            bytes[0] = 0xF0;
            bytes[1..1 + n].copy_from_slice(&payload[..n]);
            bytes[1 + n] = 0xF7;
            return Some(Vst3Event::Data(DataEvent {
                header: EventHeader {
                    event_type: K_DATA_EVENT,
                    ..header
                },
                size: (n + 2) as u32,
                event_type: K_DATA_TYPE_MIDI_SYSEX,
                bytes,
            }));
        }
        if status == SYSEX7_STATUS_START {
            // Best-effort: forward the message opening so a multi-packet SysEx is
            // not dropped silently. Intentionally unterminated (no 0xF7) — the
            // continuation fragments can't be reassembled here (see above).
            let mut bytes = [0u8; 16];
            bytes[0] = 0xF0;
            bytes[1..1 + n].copy_from_slice(&payload[..n]);
            return Some(Vst3Event::Data(DataEvent {
                header: EventHeader {
                    event_type: K_DATA_EVENT,
                    ..header
                },
                size: (n + 1) as u32,
                event_type: K_DATA_TYPE_MIDI_SYSEX,
                bytes,
            }));
        }
        // CONTINUE / END fragments carry no 0xF0 start and can't stand alone.
        return None;
    }

    // Data event: the VST3 carrier for any 3-byte MIDI-1 message. VST3 defines
    // no Data subtype for plain channel-voice bytes (only kMidiSysEx), so the
    // subtype stays 0 — these are a fallback the plugin reads as raw bytes.
    let (bytes, _len) = event.to_midi1_bytes()?;
    let mut data = [0u8; 16];
    data[..3].copy_from_slice(&bytes);
    Some(Vst3Event::Data(DataEvent {
        header: EventHeader {
            event_type: K_DATA_EVENT,
            ..header
        },
        size: 3,
        event_type: 0,
        bytes: data,
    }))
}

/// Map a MIDI-2 per-note controller index to the VST3 note-expression dimension
/// it corresponds to, or `None` when there's no standard counterpart.
///
/// Only the indices that align with a VST3 standard expression are forwarded;
/// the MIDI-2 per-note CC namespace is otherwise open-ended and has no general
/// VST3 expression equivalent. The chosen indices follow the usual MIDI CC
/// assignments (volume = 7, pan = 10, brightness = 74).
fn per_note_controller_expression(index: u8) -> Option<NoteExpressionType> {
    match index {
        7 => Some(NoteExpressionType::Volume),
        10 => Some(NoteExpressionType::Pan),
        74 => Some(NoteExpressionType::Brightness),
        _ => None,
    }
}

/// Map a MIDI-2 *registered* per-note controller (a spec-named
/// [`Controller`](tutti_midi_types::midi2::channel_voice2::Controller)) to the
/// VST3 note-expression dimension it corresponds to, with its raw 32-bit data.
///
/// Registered controllers carry spec meaning by *name*, not by a bare index, so
/// we match the enum directly. Only the dimensions with a VST3 standard
/// expression counterpart are forwarded (Volume, Pan, Brightness = CC74 /
/// SoundController #5); the rest have no VST3 equivalent and are dropped.
fn registered_controller_expression(
    controller: tutti_midi_types::midi2::channel_voice2::Controller,
) -> Option<(NoteExpressionType, u32)> {
    use tutti_midi_types::midi2::channel_voice2::Controller;
    match controller {
        Controller::Volume(data) => Some((NoteExpressionType::Volume, data)),
        Controller::Pan(data) => Some((NoteExpressionType::Pan, data)),
        Controller::Brightness(data) | Controller::SoundController { index: 5, data } => {
            Some((NoteExpressionType::Brightness, data))
        }
        _ => None,
    }
}

/// Decode a [`Vst3Event`] into a Tutti UMP [`MidiEvent`].
///
/// Notes and poly-pressure go through [`tutti_midi_types::encode`], so the
/// plugin's `f32` velocity / pressure is preserved at MIDI-2's full bit width
/// instead of being squashed to 7 bits. A [`Vst3Event::Data`] carrying SysEx is
/// rebuilt as a UMP SysEx7 packet; other Data events decode from their raw
/// MIDI-1 bytes. A plugin-emitted [`Vst3Event::LegacyMidiCcOut`] is rebuilt as
/// the CC / pitch-bend / poly-pressure UMP message it stands for. Returns `None`
/// for the non-MIDI events (note-expression value/text/int, chord, scale), for
/// `Data` payloads shorter than 2 bytes, and for a SysEx too long for one UMP
/// packet.
/// A VST3 `i16` channel field as a 4-bit MIDI channel.
///
/// VST3 stores channel and pitch as `i16`; MIDI wants 4 and 7 bits. The mask is
/// what makes the narrowing lossless, so it lives here with the cast rather
/// than beside each call — three arms shared the same two-token idiom, and an
/// `#[allow]` per argument is not expressible on stable Rust anyway (attributes
/// on expressions are unstable, rust#15701).
#[allow(
    clippy::cast_possible_truncation,
    reason = "masked to 4 bits, so the narrowing cannot lose a bit the mask keeps"
)]
fn midi_channel(raw: i16) -> u8 {
    (raw as u8) & 0x0F
}

/// MIDI-2's unsigned 64-bit note-expression payload in VST3's signed slot.
///
/// `NoteExpressionIntValueEvent::value` is `i64` and MIDI-2 carries the same 64
/// bits unsigned. Reinterpreting is the intended round-trip — the bits are
/// preserved and the receiving plugin reads them back through the same slot —
/// so this is a rename, not a narrowing.
#[allow(
    clippy::cast_possible_wrap,
    reason = "same 64 bits in VST3's signed slot; the round-trip restores them"
)]
fn reinterpret_expression_value(raw: u64) -> i64 {
    raw as i64
}

/// A VST3 `i16` pitch field as a 7-bit MIDI note number.
#[allow(
    clippy::cast_possible_truncation,
    reason = "masked to 7 bits, so the narrowing cannot lose a bit the mask keeps"
)]
fn midi_note(raw: i16) -> u8 {
    (raw as u8) & 0x7F
}

pub(crate) fn vst3_to_midi_event(event: &Vst3Event) -> Option<MidiEvent> {
    use tutti_midi_types::convert::{unit_f32_to_u16, unit_f32_to_u32};

    let frame = event.sample_offset().max(0) as u32;
    // Notes and poly-pressure build native MIDI-2 Channel Voice events so the
    // plugin's f32 velocity / pressure is preserved at full 16/32-bit width.
    let built = match event {
        Vst3Event::NoteOn(e) => MidiEvent::note_on(
            MidiGroup::FIRST,
            MidiChannel::new(midi_channel(e.channel)),
            midi_note(e.pitch),
            unit_f32_to_u16(e.velocity),
        ),
        Vst3Event::NoteOff(e) => MidiEvent::note_off(
            MidiGroup::FIRST,
            MidiChannel::new(midi_channel(e.channel)),
            midi_note(e.pitch),
            // Mirrors the host->plugin direction: the plugin's normalized
            // release velocity, not a hardcoded 0.
            unit_f32_to_u16(e.velocity),
        ),
        Vst3Event::PolyPressure(e) => MidiEvent::poly_pressure(
            MidiGroup::FIRST,
            MidiChannel::new(midi_channel(e.channel)),
            midi_note(e.pitch),
            unit_f32_to_u32(e.pressure),
        ),
        Vst3Event::Data(e) => {
            if e.size < 2 {
                return None;
            }
            let bytes = &e.bytes[..e.size as usize];
            // SysEx Data event → UMP SysEx7. The 0xF0 framing is the reliable
            // signal (the kMidiSysEx subtype is 0, indistinguishable from the
            // raw-channel-voice fallback's subtype). Strip the 0xF0 … 0xF7
            // delimiters and rebuild a single packet — the inverse of the input
            // path, which only emits self-contained ≤ 6-byte SysEx. Anything
            // longer can't be one UMP event, so it's dropped, not truncated.
            if bytes[0] == 0xF0 {
                let inner = bytes.strip_prefix(&[0xF0]).unwrap_or(bytes);
                let inner = inner.strip_suffix(&[0xF7]).unwrap_or(inner);
                return MidiEvent::sysex7_single(MidiGroup::FIRST, inner)
                    .map(|m| m.with_frame_offset(frame));
            }
            // Promote the MIDI-1 bytes to Channel Voice 2 at this edge, so the
            // engine sees one vocabulary regardless of source — matching the
            // hardware input path. System messages pass through unchanged.
            return MidiEvent::from_midi1_bytes(frame, bytes)
                .map(|e| tutti_midi_types::normalize(&e));
        }
        Vst3Event::LegacyMidiCcOut(e) => {
            return legacy_cc_to_midi(e, frame);
        }
        // Not channel-voice MIDI messages.
        Vst3Event::NoteExpression(_)
        | Vst3Event::NoteExpressionText(_)
        | Vst3Event::Chord(_)
        | Vst3Event::Scale(_)
        | Vst3Event::NoteExpressionInt(_) => return None,
    };
    Some(built.with_frame_offset(frame))
}

/// Rebuild the UMP MIDI message a plugin-emitted [`LegacyMidiCcOutEvent`] stands
/// for. `control_number` is a VST3 `ControllerNumbers` index: 0-127 are real
/// CCs; the synthetic slots map to their channel-voice messages, with `value2`
/// supplying the second data byte for pitch bend and poly-pressure.
/// `ControllerNumbers_` variants are SDK ordinals below 130, and `cn` is
/// range-checked against `0..=127` before the narrowing that reaches MIDI.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    reason = "SDK controller ordinals < 130; cn is range-checked before narrowing"
)]
fn legacy_cc_to_midi(e: &LegacyMidiCcOutEvent, frame: u32) -> Option<MidiEvent> {
    use tutti_midi_types::convert::{midi1_cc_to_midi2, midi1_pitch_bend_to_midi2};
    use vst3::Steinberg::Vst::ControllerNumbers_::{kAfterTouch, kCtrlPolyPressure, kPitchBend};

    let channel = (e.channel as u8) & 0x0F;
    let v1 = (e.value as u8) & 0x7F;
    let v2 = (e.value2 as u8) & 0x7F;
    let cn = e.control_number as i32;

    let ev = if cn == kPitchBend as i32 {
        // 14-bit: LSB = value, MSB = value2.
        let bend14 = (v1 as u16) | ((v2 as u16) << 7);
        MidiEvent::pitch_bend(
            MidiGroup::FIRST,
            MidiChannel::new(channel),
            midi1_pitch_bend_to_midi2(bend14),
        )
    } else if cn == kAfterTouch as i32 {
        MidiEvent::channel_pressure(
            MidiGroup::FIRST,
            MidiChannel::new(channel),
            midi1_cc_to_midi2(v1),
        )
    } else if cn == kCtrlPolyPressure as i32 {
        // value = note, value2 = pressure.
        MidiEvent::poly_pressure(
            MidiGroup::FIRST,
            MidiChannel::new(channel),
            v1,
            midi1_cc_to_midi2(v2),
        )
    } else if (0..=127).contains(&cn) {
        MidiEvent::cc(
            MidiGroup::FIRST,
            MidiChannel::new(channel),
            // Wire boundary: `cn` is a VST3 `control_number`, already narrowed
            // to `0..=127` by the arm's guard, so the mask is a no-op.
            CCNumber::new(cn as u8),
            midi1_cc_to_midi2(v1),
        )
    } else {
        return None;
    };
    Some(ev.with_frame_offset(frame))
}

/// Encode a [`NoteExpressionType`] as the integer `typeId` VST3 uses on the
/// wire.
///
/// The ids are the `NoteExpressionTypeIDs` enumerators from
/// `ivstnoteexpression.h`: `kVolumeTypeID = 0`, `kPanTypeID = 1`,
/// `kTuningTypeID = 2`, `kVibratoTypeID = 3`, `kExpressionTypeID = 4`,
/// `kBrightnessTypeID = 5`. Note that **Expression comes before Brightness** —
/// they are not in enum-declaration order here.
///
/// **Partial:** VST3 has no standard note-expression `typeId` for CLAP's
/// [`Pressure`](NoteExpressionType::Pressure) (poly aftertouch rides the
/// `kPolyPressureEvent` path instead), so that returns `None`. Callers must
/// handle the `None` (skip the event) rather than substitute a different
/// dimension.
pub fn note_expression_type_to_id(ty: NoteExpressionType) -> Option<u32> {
    use vst3::Steinberg::Vst::NoteExpressionTypeIDs_ as Ids;
    #[allow(clippy::unnecessary_cast)]
    match ty {
        NoteExpressionType::Volume => Some(Ids::kVolumeTypeID as u32),
        NoteExpressionType::Pan => Some(Ids::kPanTypeID as u32),
        NoteExpressionType::Tuning => Some(Ids::kTuningTypeID as u32),
        NoteExpressionType::Vibrato => Some(Ids::kVibratoTypeID as u32),
        NoteExpressionType::Expression => Some(Ids::kExpressionTypeID as u32),
        NoteExpressionType::Brightness => Some(Ids::kBrightnessTypeID as u32),
        NoteExpressionType::Pressure => None,
        // A custom id came from a VST3 plugin in the first place, so it goes
        // back out unchanged. Text and phoneme ids are screened here rather
        // than trusted: those slots carry a string, and emitting one as a
        // value event would hand the plugin a malformed event.
        NoteExpressionType::Custom(id) => (!is_text_type_id(id)).then_some(id),
    }
}

/// Whether `id` is one of the two VST3 note-expression slots that carry text
/// rather than a `f64` value.
///
/// `kTextTypeID` / `kPhonemeTypeID` ride `NoteExpressionTextEvent`, a different
/// event struct. They must not round-trip through the value path in either
/// direction.
fn is_text_type_id(id: u32) -> bool {
    use vst3::Steinberg::Vst::NoteExpressionTypeIDs_ as Ids;
    #[allow(clippy::unnecessary_cast)]
    {
        id == Ids::kTextTypeID as u32 || id == Ids::kPhonemeTypeID as u32
    }
}

/// Decode a VST3 `typeId` into a [`NoteExpressionType`].
///
/// The six standard value dimensions map to their named variants; anything else
/// becomes [`Custom`](NoteExpressionType::Custom) carrying the id, because VST3
/// reserves `kCustomStart` upward for plugin-defined dimensions declared through
/// `INoteExpressionController`. Those used to return `None` and be dropped by
/// the caller, which made a plugin's own expression dimensions invisible.
///
/// Still `None` for `kTextTypeID` / `kPhonemeTypeID`: those slots carry a string
/// on `NoteExpressionTextEvent`, so there is no `f64` value to decode and
/// admitting them here would invent one.
///
/// `Pressure` is never produced — VST3 has no id for it.
pub fn note_expression_type_from_id(id: u32) -> Option<NoteExpressionType> {
    use vst3::Steinberg::Vst::NoteExpressionTypeIDs_ as Ids;
    #[allow(clippy::unnecessary_cast)]
    match id {
        i if i == Ids::kVolumeTypeID as u32 => Some(NoteExpressionType::Volume),
        i if i == Ids::kPanTypeID as u32 => Some(NoteExpressionType::Pan),
        i if i == Ids::kTuningTypeID as u32 => Some(NoteExpressionType::Tuning),
        i if i == Ids::kVibratoTypeID as u32 => Some(NoteExpressionType::Vibrato),
        i if i == Ids::kExpressionTypeID as u32 => Some(NoteExpressionType::Expression),
        i if i == Ids::kBrightnessTypeID as u32 => Some(NoteExpressionType::Brightness),
        i if is_text_type_id(i) => None,
        other => Some(NoteExpressionType::Custom(other)),
    }
}

/// Stage a [`NoteExpressionValue`] into the tagged-enum [`Vst3Event`] form
/// accepted by the event-list code. Returns `None` for a dimension VST3
/// cannot encode (`Pressure`) — see [`note_expression_type_to_id`];
/// the caller drops the event rather than substituting a different dimension.
pub fn note_expression_to_vst3(value: &NoteExpressionValue) -> Option<Vst3Event> {
    let type_id = note_expression_type_to_id(value.expression_type)?;

    let header = EventHeader {
        bus_index: 0,
        sample_offset: value.sample_offset,
        ppq_position: 0.0,
        flags: 0,
        event_type: K_NOTE_EXPRESSION_VALUE_EVENT,
    };

    Some(Vst3Event::NoteExpression(NoteExpressionValueEvent {
        header,
        note_id: value.note_id,
        type_id,
        value: value.value,
    }))
}

/// Extract a [`NoteExpressionValue`] from a [`Vst3Event`], or `None` for any
/// non-expression variant or unrecognised `type_id`.
pub fn vst3_to_note_expression(event: &Vst3Event) -> Option<NoteExpressionValue> {
    match event {
        Vst3Event::NoteExpression(e) => {
            let expression_type = note_expression_type_from_id(e.type_id)?;
            Some(NoteExpressionValue {
                sample_offset: e.header.sample_offset,
                note_id: e.note_id,
                expression_type,
                value: e.value,
            })
        }
        _ => None,
    }
}

/// Owner-scratch alias for the UTF-16 text arena that backs chord / scale /
/// note-expression-text events' borrowed `text` pointers.
pub(crate) type TextArena = smallvec::SmallVec<[u16; 256]>;

/// Intern `text` into `arena` and return a [`TextRef`] addressing it.
/// As `to_c_event`: `MAX_EVENT_TEXT_LEN` bounds both the offset and the length.
#[allow(
    clippy::cast_possible_truncation,
    reason = "bounded by MAX_EVENT_TEXT_LEN"
)]
fn intern_utf16(arena: &mut TextArena, text: &[u16]) -> TextRef {
    let n = text.len().min(MAX_EVENT_TEXT_LEN);
    if n == 0 {
        return TextRef::default();
    }
    let start = arena.len() as u32;
    arena.extend_from_slice(&text[..n]);
    TextRef {
        start,
        len: n as u32,
    }
}

fn text_header(sample_offset: i32, event_type: u16) -> EventHeader {
    EventHeader {
        bus_index: 0,
        sample_offset,
        ppq_position: 0.0,
        flags: 0,
        event_type,
    }
}

/// Host-facing per-note text annotation. `text` is UTF-16 (converted from the
/// IPC `String` at this boundary); staging interns it into the event arena.
#[derive(Debug, Clone)]
pub struct NoteExpressionText {
    pub sample_offset: i32,
    pub note_id: i32,
    pub type_id: u32,
    pub text: Vec<u16>,
}

impl NoteExpressionText {
    /// Stage into a [`Vst3Event`], interning the text into `arena`.
    pub fn to_vst3_event(&self, arena: &mut TextArena) -> Vst3Event {
        Vst3Event::NoteExpressionText(NoteExpressionTextEvent {
            header: text_header(self.sample_offset, K_NOTE_EXPRESSION_TEXT_EVENT),
            type_id: self.type_id,
            note_id: self.note_id,
            text: intern_utf16(arena, &self.text),
        })
    }
}

/// Host-facing chord context (see [`ChordEvent`]).
#[derive(Debug, Clone)]
pub struct ChordValue {
    pub sample_offset: i32,
    pub root: i16,
    pub bass_note: i16,
    pub mask: i16,
    pub text: Vec<u16>,
}

impl ChordValue {
    pub fn to_vst3_event(&self, arena: &mut TextArena) -> Vst3Event {
        Vst3Event::Chord(ChordEvent {
            header: text_header(self.sample_offset, K_CHORD_EVENT),
            root: self.root,
            bass_note: self.bass_note,
            mask: self.mask,
            text: intern_utf16(arena, &self.text),
        })
    }
}

/// Host-facing scale/key context (see [`ScaleEvent`]).
#[derive(Debug, Clone)]
pub struct ScaleValue {
    pub sample_offset: i32,
    pub root: i16,
    pub mask: i16,
    pub text: Vec<u16>,
}

impl ScaleValue {
    pub fn to_vst3_event(&self, arena: &mut TextArena) -> Vst3Event {
        Vst3Event::Scale(ScaleEvent {
            header: text_header(self.sample_offset, K_SCALE_EVENT),
            root: self.root,
            mask: self.mask,
            text: intern_utf16(arena, &self.text),
        })
    }
}

/// Host-facing integer-valued per-note expression (see
/// [`NoteExpressionIntValueEvent`]). No text, so no arena needed.
#[derive(Debug, Clone, Copy)]
pub struct NoteExpressionIntValue {
    pub sample_offset: i32,
    pub note_id: i32,
    pub type_id: u32,
    pub value: i64,
}

impl NoteExpressionIntValue {
    pub fn to_vst3_event(&self) -> Vst3Event {
        Vst3Event::NoteExpressionInt(NoteExpressionIntValueEvent {
            header: EventHeader {
                bus_index: 0,
                sample_offset: self.sample_offset,
                ppq_position: 0.0,
                flags: 0,
                event_type: K_NOTE_EXPRESSION_INT_VALUE_EVENT,
            },
            type_id: self.type_id,
            note_id: self.note_id,
            value: self.value,
        })
    }
}

/// Read a chord/scale/text/int event off an output list, resolving its
/// [`TextRef`] against `arena` into an owned UTF-16 `Vec`. Returns `None` for
/// any other variant.
pub fn vst3_to_chord(event: &Vst3Event, arena: &[u16]) -> Option<ChordValue> {
    match event {
        Vst3Event::Chord(e) => Some(ChordValue {
            sample_offset: e.header.sample_offset,
            root: e.root,
            bass_note: e.bass_note,
            mask: e.mask,
            text: resolve_text(&e.text, arena),
        }),
        _ => None,
    }
}

/// See [`vst3_to_chord`].
pub fn vst3_to_scale(event: &Vst3Event, arena: &[u16]) -> Option<ScaleValue> {
    match event {
        Vst3Event::Scale(e) => Some(ScaleValue {
            sample_offset: e.header.sample_offset,
            root: e.root,
            mask: e.mask,
            text: resolve_text(&e.text, arena),
        }),
        _ => None,
    }
}

/// See [`vst3_to_chord`].
pub fn vst3_to_note_expression_text(
    event: &Vst3Event,
    arena: &[u16],
) -> Option<NoteExpressionText> {
    match event {
        Vst3Event::NoteExpressionText(e) => Some(NoteExpressionText {
            sample_offset: e.header.sample_offset,
            note_id: e.note_id,
            type_id: e.type_id,
            text: resolve_text(&e.text, arena),
        }),
        _ => None,
    }
}

/// Read the integer per-note expression off an event, or `None` otherwise.
pub fn vst3_to_note_expression_int(event: &Vst3Event) -> Option<NoteExpressionIntValue> {
    match event {
        Vst3Event::NoteExpressionInt(e) => Some(NoteExpressionIntValue {
            sample_offset: e.header.sample_offset,
            note_id: e.note_id,
            type_id: e.type_id,
            value: e.value,
        }),
        _ => None,
    }
}

fn resolve_text(t: &TextRef, arena: &[u16]) -> Vec<u16> {
    let start = t.start as usize;
    let end = start + t.len as usize;
    arena
        .get(start..end)
        .map(|s| s.to_vec())
        .unwrap_or_default()
}

/// MIDI round-trip tests through `Vst3Event::from_midi` + `Vst3Event::to_midi`.
///
/// Tests build SDK structs by hand, so they repeat the same bounded narrowings
/// the production code justifies above (SDK ordinals, text sizes clamped to
/// `MAX_EVENT_TEXT_LEN`). Allowed at module scope rather than per case: a test
/// that overflows one of these fails loudly on its own assertion, so the lint
/// adds nothing here, and the deny is kept meaningful by staying tight in the
/// code that actually crosses the ABI.
#[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
#[cfg(test)]
mod tests {
    use super::*;
    use tutti_midi_types::convert::{bend_u32_to_signed_f32, u16_to_unit_f32, u32_to_unit_f32};
    use tutti_midi_types::midi2::channel_voice2::ChannelVoice2 as Cv2;
    use tutti_midi_types::midi2::{Channeled, UmpMessage};

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
        let vst3 = vst3_event_from_midi(&event).expect("CC -> Data");
        assert!(
            matches!(vst3, Vst3Event::Data(_)),
            "CC should be a Data event"
        );
        let back = vst3_to_midi_event(&vst3).expect("cc decodes");
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
        let vst3 = vst3_event_from_midi(&event).expect("clock -> Data");
        let back = vst3_to_midi_event(&vst3).expect("clock decodes");
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
        let event = MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::new(3), 60, 0x8000)
            .with_frame_offset(5);
        let vst3 = vst3_event_from_midi(&event).expect("NoteOn should convert");
        match &vst3 {
            Vst3Event::NoteOn(e) => {
                assert_eq!(e.channel, 3);
                assert_eq!(e.pitch, 60);
                assert_eq!(e.header.sample_offset, 5);
                assert!(e.velocity > 0.0, "expected non-zero velocity");
            }
            _ => panic!("expected NoteOn variant"),
        }

        let back = vst3_to_midi_event(&vst3).expect("round-trip");
        assert!(back.is_note_on());
        assert_eq!(back.note(), Some(60));
        assert_eq!(back.frame_offset, 5);
    }

    #[test]
    fn note_off_lands_in_note_off_variant() {
        const RELEASE: u16 = 0x4000;
        let event = MidiEvent::note_off(MidiGroup::FIRST, MidiChannel::FIRST, 72, RELEASE)
            .with_frame_offset(10);
        let vst3 = vst3_event_from_midi(&event).expect("NoteOff should convert");
        match &vst3 {
            Vst3Event::NoteOff(e) => {
                assert_eq!(e.pitch, 72);
                assert_eq!(e.header.sample_offset, 10);
                // The test always passed a real release velocity here and
                // asserted only pitch and offset, so it stayed green while both
                // conversion directions hardcoded zero.
                assert!(
                    e.velocity > 0.0,
                    "release velocity was dropped on the way to the plugin — \
                     release-layer instruments get the minimum on every note-off"
                );
            }
            _ => panic!("expected NoteOff variant"),
        }

        let back = vst3_to_midi_event(&vst3).expect("round-trip");
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
    /// a bare `as i32` turns a large offset into a negative one. The plugin
    /// indexes its own buffers with that value, so the out-of-bounds access
    /// happens inside the plugin where nothing here can catch it. The mirror
    /// path already guarded with `.max(0)`; this direction did not.
    #[test]
    fn a_huge_frame_offset_saturates_rather_than_going_negative() {
        let event = MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x8000)
            .with_frame_offset(u32::MAX);
        let vst3 = vst3_event_from_midi(&event).expect("NoteOn should convert");
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
        let vst3 = vst3_event_from_midi(&event).expect("PolyPressure should convert");
        assert!(matches!(vst3, Vst3Event::PolyPressure(_)));
        let back = vst3_to_midi_event(&vst3).expect("round-trip");
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
        let vst3 = vst3_event_from_midi(&event).expect("CC should convert");
        match &vst3 {
            Vst3Event::Data(e) => {
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
        let vst3 = vst3_event_from_midi(&event).expect("PitchBend should convert");
        match &vst3 {
            Vst3Event::Data(e) => {
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
        let vst3 = vst3_event_from_midi(&event).expect("ProgramChange should convert");
        match &vst3 {
            Vst3Event::Data(e) => {
                assert_eq!(e.size, 3);
                assert_eq!(e.bytes[0], 0xC0 | 9);
                assert_eq!(e.bytes[1], 42);
            }
            _ => panic!("expected Data variant for ProgramChange"),
        }
    }

    /// The note-expression `typeId` table, asserted against the **absolute
    /// numeric ids from `ivstnoteexpression.h`** — deliberately hardcoded here
    /// rather than read back from `note_expression_type_to_id`, because a test
    /// that compares the table to itself passes with a wrong table (which is
    /// exactly how `Brightness => 4` — really `kExpressionTypeID` — shipped).
    ///
    /// `kVolumeTypeID = 0`, `kPanTypeID = 1`, `kTuningTypeID = 2`,
    /// `kVibratoTypeID = 3`, `kExpressionTypeID = 4`, `kBrightnessTypeID = 5`.
    #[test]
    fn note_expression_type_ids_match_the_vst3_spec() {
        assert_eq!(
            note_expression_type_to_id(NoteExpressionType::Volume),
            Some(0)
        );
        assert_eq!(note_expression_type_to_id(NoteExpressionType::Pan), Some(1));
        assert_eq!(
            note_expression_type_to_id(NoteExpressionType::Tuning),
            Some(2)
        );
        assert_eq!(
            note_expression_type_to_id(NoteExpressionType::Vibrato),
            Some(3)
        );
        assert_eq!(
            note_expression_type_to_id(NoteExpressionType::Expression),
            Some(4),
            "kExpressionTypeID is 4 — NOT Brightness"
        );
        assert_eq!(
            note_expression_type_to_id(NoteExpressionType::Brightness),
            Some(5),
            "kBrightnessTypeID is 5"
        );
        // VST3 has no standard note-expression id for poly pressure; it rides
        // the kPolyPressureEvent path instead.
        assert_eq!(
            note_expression_type_to_id(NoteExpressionType::Pressure),
            None
        );
    }

    /// The decoder must mirror the same absolute ids, so an incoming 5 is
    /// Brightness (not dropped) and an incoming 4 is Expression (not
    /// mislabelled Brightness).
    #[test]
    fn note_expression_type_from_id_matches_the_vst3_spec() {
        assert_eq!(
            note_expression_type_from_id(0),
            Some(NoteExpressionType::Volume)
        );
        assert_eq!(
            note_expression_type_from_id(1),
            Some(NoteExpressionType::Pan)
        );
        assert_eq!(
            note_expression_type_from_id(2),
            Some(NoteExpressionType::Tuning)
        );
        assert_eq!(
            note_expression_type_from_id(3),
            Some(NoteExpressionType::Vibrato)
        );
        assert_eq!(
            note_expression_type_from_id(4),
            Some(NoteExpressionType::Expression)
        );
        assert_eq!(
            note_expression_type_from_id(5),
            Some(NoteExpressionType::Brightness)
        );
        // kTextTypeID (6) / kPhonemeTypeID (7) carry text, not a value.
        assert_eq!(note_expression_type_from_id(6), None);
        assert_eq!(note_expression_type_from_id(7), None);
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
            let id = note_expression_type_to_id(ty).expect("VST3-encodable");
            assert_eq!(
                note_expression_type_from_id(id),
                Some(ty),
                "{ty:?} @ id {id}"
            );
        }
    }

    /// A plugin-defined `typeId` survives instead of being dropped.
    ///
    /// VST3 reserves `kCustomStart` (100000) upward for dimensions a plugin
    /// declares through `INoteExpressionController`. The decoder used to return
    /// `None` for those and the caller discarded the event, so a plugin whose
    /// expressiveness is entirely custom looked silent.
    #[test]
    fn a_plugin_defined_type_id_is_carried_not_dropped() {
        for id in [100_000, 100_001, 8, 12345, u32::MAX] {
            assert_eq!(
                note_expression_type_from_id(id),
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
            let ty = note_expression_type_from_id(id).expect("carried");
            assert_eq!(note_expression_type_to_id(ty), Some(id));
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
                note_expression_type_from_id(id),
                None,
                "id {id} carries text, not a value"
            );
            assert_eq!(
                note_expression_type_to_id(NoteExpressionType::Custom(id)),
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
        let staged = note_expression_to_vst3(&expr).expect("custom id is encodable");
        match &staged {
            Vst3Event::NoteExpression(e) => assert_eq!(e.type_id, 100_042),
            other => panic!("expected NoteExpression, got {other:?}"),
        }
        let back = vst3_to_note_expression(&staged).expect("decodes back");
        assert_eq!(back.expression_type, NoteExpressionType::Custom(100_042));
        assert_eq!(back.value, 0.75);
        assert_eq!(back.note_id, 3);
    }

    /// `Expression` is a real VST3 dimension (id 4) and must survive staging
    /// into an event and reading back out — it used to be hardcoded `None`
    /// while Brightness consumed its id.
    #[test]
    fn expression_dimension_stages_and_reads_back() {
        let expr = NoteExpressionValue {
            sample_offset: 3,
            note_id: note_id_for(2, 64),
            expression_type: NoteExpressionType::Expression,
            value: 0.25,
        };
        let ev = note_expression_to_vst3(&expr).expect("Expression is VST3-encodable (id 4)");
        match &ev {
            Vst3Event::NoteExpression(e) => assert_eq!(e.type_id, 4),
            other => panic!("expected NoteExpression, got {other:?}"),
        }
        let back = vst3_to_note_expression(&ev).expect("decodes");
        assert_eq!(back.expression_type, NoteExpressionType::Expression);
        assert_eq!(back.note_id, expr.note_id);
        assert_eq!(back.sample_offset, 3);
    }

    #[test]
    fn note_expression_is_not_a_midi_event() {
        let expr = NoteExpressionValue {
            sample_offset: 0,
            note_id: 1,
            expression_type: NoteExpressionType::Tuning,
            value: 0.5,
        };
        let vst3 = note_expression_to_vst3(&expr).expect("Tuning is VST3-encodable");
        assert!(vst3_to_midi_event(&vst3).is_none());
    }

    #[test]
    fn data_event_with_truncated_size_fails_gracefully() {
        let e = DataEvent {
            header: EventHeader {
                bus_index: 0,
                sample_offset: 0,
                ppq_position: 0.0,
                flags: 0,
                event_type: K_DATA_EVENT,
            },
            size: 1,
            event_type: 0,
            bytes: [0xB0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
        };
        assert!(vst3_to_midi_event(&Vst3Event::Data(e)).is_none());
    }

    #[test]
    fn note_on_and_off_share_a_derived_note_id() {
        // A note-on and its note-off for the same (channel, note) must carry the
        // same VST3 noteId so per-note events bind to the same voice.
        let on = vst3_event_from_midi(&MidiEvent::note_on(
            MidiGroup::FIRST,
            MidiChannel::new(5),
            67,
            0x8000,
        ))
        .unwrap();
        let off = vst3_event_from_midi(&MidiEvent::note_off(
            MidiGroup::FIRST,
            MidiChannel::new(5),
            67,
            0,
        ))
        .unwrap();
        let expected = note_id_for(5, 67);
        match (on, off) {
            (Vst3Event::NoteOn(a), Vst3Event::NoteOff(b)) => {
                assert_eq!(a.note_id, expected);
                assert_eq!(b.note_id, expected);
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
        let vst3 = vst3_event_from_midi(&event).expect("per-note bend should map");
        let expr = vst3_to_note_expression(&vst3).expect("is a note expression");
        assert_eq!(expr.expression_type, NoteExpressionType::Tuning);
        assert_eq!(expr.note_id, note_id_for(5, 67));
        assert_eq!(expr.sample_offset, 12);
        assert!((expr.value - 0.5).abs() < 0.01, "center bend → 0.5");
        // It is NOT a MIDI channel-voice event.
        assert!(vst3_to_midi_event(&vst3).is_none());
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
        match vst3_event_from_midi(&known) {
            Some(Vst3Event::NoteExpression(e)) => {
                // Asserted against the SPEC's absolute id (kBrightnessTypeID = 5),
                // not against `note_expression_type_to_id` — comparing the table
                // to itself is what let the off-by-one at index 4 ship.
                assert_eq!(e.type_id, 5, "CC74 → kBrightnessTypeID");
                assert_eq!(e.note_id, note_id_for(0, 60));
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
        assert!(vst3_event_from_midi(&unknown).is_none());
    }

    #[test]
    fn single_packet_sysex_round_trips_through_data_event() {
        // A short SysEx (identity request) → VST3 Data event with the SysEx
        // subtype and 0xF0 … 0xF7 framing, then back to the same UMP payload.
        let payload = [0x7E, 0x7F, 0x06, 0x01];
        let event = MidiEvent::sysex7_single(MidiGroup::FIRST, &payload)
            .unwrap()
            .with_frame_offset(7);
        let vst3 = vst3_event_from_midi(&event).expect("SysEx should map to Data");
        match &vst3 {
            Vst3Event::Data(e) => {
                assert_eq!(e.event_type, K_DATA_TYPE_MIDI_SYSEX);
                assert_eq!(e.size as usize, payload.len() + 2);
                assert_eq!(e.bytes[0], 0xF0);
                assert_eq!(e.bytes[1..1 + payload.len()], payload);
                assert_eq!(e.bytes[1 + payload.len()], 0xF7);
                assert_eq!(e.header.sample_offset, 7);
            }
            _ => panic!("expected Data variant for SysEx"),
        }

        let back = vst3_to_midi_event(&vst3).expect("SysEx Data round-trips");
        let (status, bytes, n) = back.sysex7_payload().expect("decodes as SysEx7");
        assert_eq!(status, tutti_midi_types::ump::SYSEX7_STATUS_SINGLE);
        assert_eq!(&bytes[..n], &payload);
        assert_eq!(back.frame_offset, 7);
    }

    #[test]
    fn fragmented_sysex_start_forwards_opening_rest_dropped() {
        // A multi-packet SysEx (>6 bytes) produces Start/Continue/End fragments.
        // We can't reassemble across events without breaking the Copy/no-alloc
        // event invariant, so the host degrades explicitly (not silently):
        // the START fragment forwards the message opening (0xF0 + first bytes,
        // deliberately unterminated), while CONTINUE/END — which carry no 0xF0
        // start — are dropped.
        use tutti_midi_types::ump::SYSEX7_STATUS_START;

        let mut frags = Vec::new();
        MidiEvent::sysex7_fragments(MidiGroup::FIRST, &[1, 2, 3, 4, 5, 6, 7, 8], &mut frags);
        assert!(frags.len() > 1);

        for frag in &frags {
            let (status, payload, n) = frag.sysex7_payload().expect("is a sysex7 packet");
            match vst3_event_from_midi(frag) {
                Some(Vst3Event::Data(d)) => {
                    // Only the START fragment forwards, as the message opening.
                    assert_eq!(status, SYSEX7_STATUS_START);
                    assert_eq!(d.event_type, K_DATA_TYPE_MIDI_SYSEX);
                    assert_eq!(d.bytes[0], 0xF0);
                    assert_eq!(&d.bytes[1..1 + n], &payload[..n]);
                    // Unterminated: no 0xF7 appended (the rest didn't fit).
                    assert_eq!(d.size, (n + 1) as u32);
                }
                Some(other) => panic!("unexpected non-Data event: {other:?}"),
                None => {
                    // CONTINUE / END fragments are dropped — never START.
                    assert_ne!(status, SYSEX7_STATUS_START);
                }
            }
        }
    }

    #[test]
    fn note_on_velocity_round_trips_at_full_width() {
        // A MIDI-2 note-on carrying a value not representable in 7 bits keeps
        // (close to) its width through the f32 VST3 struct on the way back out,
        // rather than collapsing to a 7-bit grid point.
        let velocity_u16 = 0x9123; // not a multiple of the 7-bit step
        let event = MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, velocity_u16);
        let vst3 = vst3_event_from_midi(&event).expect("converts");
        let back = vst3_to_midi_event(&vst3).expect("round-trips");
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

    // ── chord / scale / text / int + legacy-cc-out ───────────────────────────

    /// `Vst3Event` must stay `Copy` even with the text-bearing variants (they
    /// hold a `TextRef` index, not an owned string).
    #[test]
    fn vst3_event_is_copy() {
        fn assert_copy<T: Copy>() {}
        assert_copy::<Vst3Event>();
    }

    /// Round-trip a host chord value: intern into an arena via `to_vst3_event`,
    /// encode to a C `Event` (`to_c_event` reads the arena), decode back
    /// (`from_c_event` re-interns into its own arena), and read it out.
    #[test]
    fn chord_round_trips_through_c_event_with_text() {
        let name: Vec<u16> = "Cmaj7".encode_utf16().collect();
        let host = ChordValue {
            sample_offset: 12,
            root: 60,
            bass_note: 48,
            mask: 0b100_1001_0001,
            text: name.clone(),
        };

        let mut in_arena: TextArena = Default::default();
        let ev = host.to_vst3_event(&mut in_arena);
        assert_eq!(ev.header().event_type, K_CHORD_EVENT);

        // Encode to the C struct, pointing into the staging arena.
        let c = to_c_event(&ev, &in_arena);
        assert_eq!(c.r#type, K_CHORD_EVENT);
        unsafe {
            assert_eq!(c.__field0.chord.root, 60);
            assert_eq!(c.__field0.chord.textLen as usize, name.len());
            assert!(!c.__field0.chord.text.is_null());
        }

        // Decode back (plugin → host direction): re-interns into out_arena.
        let mut out_arena = smallvec::SmallVec::new();
        let decoded = unsafe { from_c_event(&c, &mut out_arena) }.expect("decodes");
        let back = vst3_to_chord(&decoded, &out_arena).expect("is a chord");
        assert_eq!(back.root, 60);
        assert_eq!(back.bass_note, 48);
        assert_eq!(back.mask, host.mask);
        assert_eq!(back.text, name);
        assert_eq!(back.sample_offset, 12);
    }

    #[test]
    fn scale_and_text_round_trip_through_c_event() {
        let scale_name: Vec<u16> = "D Dorian".encode_utf16().collect();
        let scale = ScaleValue {
            sample_offset: 0,
            root: 62,
            mask: 0x5ab5,
            text: scale_name.clone(),
        };
        let text_str: Vec<u16> = "staccato".encode_utf16().collect();
        let expr_text = NoteExpressionText {
            sample_offset: 4,
            note_id: note_id_for(2, 64),
            type_id: 7,
            text: text_str.clone(),
        };

        let mut in_arena: TextArena = Default::default();
        let scale_ev = scale.to_vst3_event(&mut in_arena);
        let text_ev = expr_text.to_vst3_event(&mut in_arena);

        let mut out = smallvec::SmallVec::new();
        for (ev, expect_scale) in [(scale_ev, true), (text_ev, false)] {
            let c = to_c_event(&ev, &in_arena);
            let decoded = unsafe { from_c_event(&c, &mut out) }.expect("decodes");
            if expect_scale {
                let s = vst3_to_scale(&decoded, &out).expect("scale");
                assert_eq!(s.root, 62);
                assert_eq!(s.mask, 0x5ab5);
                assert_eq!(s.text, scale_name);
            } else {
                let t = vst3_to_note_expression_text(&decoded, &out).expect("text");
                assert_eq!(t.note_id, note_id_for(2, 64));
                assert_eq!(t.type_id, 7);
                assert_eq!(t.text, text_str);
            }
        }
    }

    #[test]
    fn note_expression_int_round_trips() {
        let host = NoteExpressionIntValue {
            sample_offset: 9,
            note_id: note_id_for(1, 50),
            type_id: 3,
            value: -1234,
        };
        let ev = host.to_vst3_event();
        assert_eq!(ev.header().event_type, K_NOTE_EXPRESSION_INT_VALUE_EVENT);
        let c = to_c_event(&ev, &[]);
        let mut arena = smallvec::SmallVec::new();
        let decoded = unsafe { from_c_event(&c, &mut arena) }.expect("decodes");
        let back = vst3_to_note_expression_int(&decoded).expect("int expr");
        assert_eq!(back.value, -1234);
        assert_eq!(back.note_id, note_id_for(1, 50));
        assert_eq!(back.type_id, 3);
        // Not a MIDI message.
        assert!(vst3_to_midi_event(&decoded).is_none());
    }

    /// A plugin-emitted legacy-MIDI-CC-out event decodes to the CC / pitch-bend
    /// / poly-pressure MIDI message it stands for.
    #[test]
    fn legacy_cc_out_decodes_to_midi() {
        use vst3::Steinberg::Vst::ControllerNumbers_::{kAfterTouch, kPitchBend};

        // Plain CC 74 = 100 on channel 3.
        let cc = Vst3Event::LegacyMidiCcOut(LegacyMidiCcOutEvent {
            header: EventHeader {
                bus_index: 0,
                sample_offset: 5,
                ppq_position: 0.0,
                flags: 0,
                event_type: K_LEGACY_MIDI_CC_OUT_EVENT,
            },
            control_number: 74,
            channel: 3,
            value: 100,
            value2: 0,
        });
        let m = vst3_to_midi_event(&cc).expect("CC decodes");
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
        let pb = Vst3Event::LegacyMidiCcOut(LegacyMidiCcOutEvent {
            header: EventHeader {
                bus_index: 0,
                sample_offset: 0,
                ppq_position: 0.0,
                flags: 0,
                event_type: K_LEGACY_MIDI_CC_OUT_EVENT,
            },
            control_number: kPitchBend as u8,
            channel: 0,
            value: 0,
            value2: 0x40,
        });
        let pb_m = vst3_to_midi_event(&pb).unwrap();
        match UmpMessage::try_from(pb_m.data_words()).expect("UMP") {
            UmpMessage::ChannelVoice2(Cv2::ChannelPitchBend(msg)) => {
                assert!(bend_u32_to_signed_f32(msg.pitch_bend_data()).abs() < 0.01)
            }
            other => panic!("expected PitchBend, got {other:?}"),
        }

        // Aftertouch (channel pressure).
        let at = Vst3Event::LegacyMidiCcOut(LegacyMidiCcOutEvent {
            header: EventHeader {
                bus_index: 0,
                sample_offset: 0,
                ppq_position: 0.0,
                flags: 0,
                event_type: K_LEGACY_MIDI_CC_OUT_EVENT,
            },
            control_number: kAfterTouch as u8,
            channel: 1,
            value: 127,
            value2: 0,
        });
        let at_m = vst3_to_midi_event(&at).unwrap();
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
        let mut arena: TextArena = Default::default();
        let chord = ChordValue {
            sample_offset: 0,
            root: 60,
            bass_note: 60,
            mask: 0,
            text: vec![],
        }
        .to_vst3_event(&mut arena);
        assert!(vst3_to_midi_event(&chord).is_none());
    }
}
