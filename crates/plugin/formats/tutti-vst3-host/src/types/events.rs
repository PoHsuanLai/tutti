//! VST3 event types and bidirectional conversion to/from the Tutti
//! [`tutti_midi_types::MidiEvent`] UMP representation.
//!
//! VST3's native event list is richer than raw MIDI-1 wire data (typed
//! note-on/off/poly-pressure structs with `f32` velocity plus generic
//! `Data` events for CC / ProgramChange / ChannelPressure / PitchBend). The
//! [`vst3_event_from_midi`] / [`vst3_to_midi_event`] helpers bridge it to the
//! workspace's canonical [`MidiEvent`] UMP type — one `MidiEvent` maps to one
//! [`Vst3Event`] and round-trips losslessly for the MIDI-representable
//! variants.

pub use tutti_midi_types::MidiEvent;

use tutti_plugin_types::{note_id_for, NoteExpressionType, NoteExpressionValue};

use vst3::Steinberg::Vst::Event_::EventTypes_;

/// `type_` discriminant for note-on events.
pub const K_NOTE_ON_EVENT: u16 = EventTypes_::kNoteOnEvent as u16;
/// `type_` discriminant for note-off events.
pub const K_NOTE_OFF_EVENT: u16 = EventTypes_::kNoteOffEvent as u16;
/// `type_` discriminant for raw-data events (CC, pitch bend, program change, …).
pub const K_DATA_EVENT: u16 = EventTypes_::kDataEvent as u16;
/// `type_` discriminant for poly-pressure events.
pub const K_POLY_PRESSURE_EVENT: u16 = EventTypes_::kPolyPressureEvent as u16;
/// `type_` discriminant for note-expression value events.
pub const K_NOTE_EXPRESSION_VALUE_EVENT: u16 = EventTypes_::kNoteExpressionValueEvent as u16;
/// `type_` discriminant for note-expression *text* events.
pub const K_NOTE_EXPRESSION_TEXT_EVENT: u16 = EventTypes_::kNoteExpressionTextEvent as u16;
/// `type_` discriminant for chord events.
pub const K_CHORD_EVENT: u16 = EventTypes_::kChordEvent as u16;
/// `type_` discriminant for scale events.
pub const K_SCALE_EVENT: u16 = EventTypes_::kScaleEvent as u16;
/// `type_` discriminant for note-expression integer-value events.
pub const K_NOTE_EXPRESSION_INT_VALUE_EVENT: u16 = EventTypes_::kNoteExpressionIntValueEvent as u16;
/// `type_` discriminant for legacy-MIDI-CC-out events (plugin → host, value 0xFFFF).
pub const K_LEGACY_MIDI_CC_OUT_EVENT: u16 = EventTypes_::kLegacyMIDICCOutEvent as u16;
/// `DataEvent.type` subtype marking the payload as a MIDI SysEx message.
pub const K_DATA_TYPE_MIDI_SYSEX: u32 = vst3::Steinberg::Vst::DataEvent_::DataTypes_::kMidiSysEx;

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
    /// 0=volume, 1=pan, 2=tuning, 3=vibrato, 4=brightness.
    pub type_id: u32,
    /// 0.0 to 1.0, meaning depends on type_id.
    pub value: f64,
}

/// A `(start, len)` slice into an `EventList`'s UTF-16 text arena.
///
/// VST3's chord / scale / note-expression-text events carry a borrowed
/// `const TChar*` (UTF-16) that must outlive the `process` call. To keep
/// [`Vst3Event`] `Copy` (no owned heap per event), the string lives in a shared
/// arena owned by the `EventList` — cleared each block, like the `DataEvent`
/// byte scratch — and the event holds only this index into it. `len` counts
/// `u16` code units, excluding any terminator.
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
/// [`vst3_event_from_midi`] / [`vst3_to_midi_event`] for round-trip MIDI
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
}

/// Convert our flat `Vst3Event` into the C `Event` struct the vst3 crate expects.
///
/// Two owner-scratch buffers back the borrowed pointers in the C structs and
/// must outlive the returned `Event`:
/// - `data_storage` owns the `DataEvent.bytes` slot (one push per `Data` event).
/// - `text_arena` owns the UTF-16 text for chord / scale / note-expression-text
///   events; the event's [`TextRef`] indexes into it, resolved to a pointer here.
pub(crate) fn to_c_event(
    event: &Vst3Event,
    data_storage: &mut smallvec::SmallVec<[[u8; 16]; 8]>,
    text_arena: &[u16],
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
            data_storage.push(e.bytes);
            let slot = data_storage.last().expect("just pushed");
            out.__field0.data = vst3::Steinberg::Vst::DataEvent {
                size: e.size,
                r#type: e.event_type,
                bytes: slot.as_ptr(),
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
    let intern = |arena: &mut smallvec::SmallVec<[u16; 256]>,
                  ptr: *const u16,
                  len: usize|
     -> TextRef {
        let n = len.min(MAX_EVENT_TEXT_LEN);
        if ptr.is_null() || n == 0 {
            return TextRef::default();
        }
        let start = arena.len() as u32;
        arena.extend(std::slice::from_raw_parts(ptr, n).iter().copied());
        TextRef { start, len: n as u32 }
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
                value: e.value as i64,
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
pub fn vst3_event_from_midi(event: &MidiEvent) -> Option<Vst3Event> {
    use tutti_midi_types::convert::{bend_u32_to_signed_f32, u16_to_unit_f32, u32_to_unit_f32};
    use tutti_midi_types::midi2::channel_voice2::ChannelVoice2 as Cv2;
    use tutti_midi_types::midi2::{Channeled, UmpMessage};

    let sample_offset = event.frame_offset as i32;
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
                    velocity: 0.0,
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
    // message *with* its 0xF0 … 0xF7 delimiters. We forward a single-packet
    // SysEx (the payload fits a UMP type-0x3 packet, ≤ 6 bytes → ≤ 8 with
    // delimiters, well within the 16-byte buffer). Multi-packet SysEx needs
    // cross-event reassembly that doesn't belong in a per-event converter, so
    // only the self-contained `SINGLE` packet is mapped here.
    if let Some((status, payload, n)) = event.sysex7_payload() {
        if status == tutti_midi_types::ump::SYSEX7_STATUS_SINGLE {
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
        // Start/Continue/End fragments can't stand alone as a VST3 Data event.
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
pub fn vst3_to_midi_event(event: &Vst3Event) -> Option<MidiEvent> {
    use tutti_midi_types::convert::{unit_f32_to_u16, unit_f32_to_u32};

    let frame = event.sample_offset().max(0) as u32;
    // Notes and poly-pressure build native MIDI-2 Channel Voice events so the
    // plugin's f32 velocity / pressure is preserved at full 16/32-bit width.
    let built = match event {
        Vst3Event::NoteOn(e) => MidiEvent::note_on(
            0,
            (e.channel as u8) & 0x0F,
            (e.pitch as u8) & 0x7F,
            unit_f32_to_u16(e.velocity),
        ),
        Vst3Event::NoteOff(e) => {
            MidiEvent::note_off(0, (e.channel as u8) & 0x0F, (e.pitch as u8) & 0x7F, 0)
        }
        Vst3Event::PolyPressure(e) => MidiEvent::poly_pressure(
            0,
            (e.channel as u8) & 0x0F,
            (e.pitch as u8) & 0x7F,
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
                return MidiEvent::sysex7_single(0, inner).map(|m| m.with_frame_offset(frame));
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
fn legacy_cc_to_midi(e: &LegacyMidiCcOutEvent, frame: u32) -> Option<MidiEvent> {
    use vst3::Steinberg::Vst::ControllerNumbers_::{
        kAfterTouch, kCtrlPolyPressure, kPitchBend,
    };
    use tutti_midi_types::convert::{midi1_cc_to_midi2, midi1_pitch_bend_to_midi2};

    let channel = (e.channel as u8) & 0x0F;
    let v1 = (e.value as u8) & 0x7F;
    let v2 = (e.value2 as u8) & 0x7F;
    let cn = e.control_number as i32;

    let ev = if cn == kPitchBend as i32 {
        // 14-bit: LSB = value, MSB = value2.
        let bend14 = (v1 as u16) | ((v2 as u16) << 7);
        MidiEvent::pitch_bend(0, channel, midi1_pitch_bend_to_midi2(bend14))
    } else if cn == kAfterTouch as i32 {
        MidiEvent::channel_pressure(0, channel, midi1_cc_to_midi2(v1))
    } else if cn == kCtrlPolyPressure as i32 {
        // value = note, value2 = pressure.
        MidiEvent::poly_pressure(0, channel, v1, midi1_cc_to_midi2(v2))
    } else if (0..=127).contains(&cn) {
        MidiEvent::cc(0, channel, cn as u8, midi1_cc_to_midi2(v1))
    } else {
        return None;
    };
    Some(ev.with_frame_offset(frame))
}

/// Encode a [`NoteExpressionType`] as the integer `typeId` VST3 uses on the
/// wire. **Partial:** VST3 has no note-expression `typeId` for CLAP's
/// [`Pressure`](NoteExpressionType::Pressure) /
/// [`Expression`](NoteExpressionType::Expression), so those return `None`.
/// Callers must handle the `None` (skip the event) rather than substitute a
/// different dimension.
pub fn note_expression_type_to_id(ty: NoteExpressionType) -> Option<u32> {
    match ty {
        NoteExpressionType::Volume => Some(0),
        NoteExpressionType::Pan => Some(1),
        NoteExpressionType::Tuning => Some(2),
        NoteExpressionType::Vibrato => Some(3),
        NoteExpressionType::Brightness => Some(4),
        NoteExpressionType::Pressure | NoteExpressionType::Expression => None,
    }
}

/// Decode a VST3 `typeId` back into a [`NoteExpressionType`]; `None` for
/// unknown ids. VST3 only emits the five it can encode, so this never yields
/// `Pressure`/`Expression`.
pub fn note_expression_type_from_id(id: u32) -> Option<NoteExpressionType> {
    match id {
        0 => Some(NoteExpressionType::Volume),
        1 => Some(NoteExpressionType::Pan),
        2 => Some(NoteExpressionType::Tuning),
        3 => Some(NoteExpressionType::Vibrato),
        4 => Some(NoteExpressionType::Brightness),
        _ => None,
    }
}

/// Stage a [`NoteExpressionValue`] into the tagged-enum [`Vst3Event`] form
/// accepted by the event-list code. Returns `None` for a dimension VST3
/// cannot encode (Pressure/Expression) — see [`note_expression_type_to_id`];
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
fn intern_utf16(arena: &mut TextArena, text: &[u16]) -> TextRef {
    let n = text.len().min(MAX_EVENT_TEXT_LEN);
    if n == 0 {
        return TextRef::default();
    }
    let start = arena.len() as u32;
    arena.extend_from_slice(&text[..n]);
    TextRef { start, len: n as u32 }
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
pub fn vst3_to_note_expression_text(event: &Vst3Event, arena: &[u16]) -> Option<NoteExpressionText> {
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
    arena.get(start..end).map(|s| s.to_vec()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    //! MIDI round-trip tests through `vst3_event_from_midi` + `vst3_to_midi_event`.

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

        let event = MidiEvent::cc(0, 1, 74, midi1_cc_to_midi2(100));
        let vst3 = vst3_event_from_midi(&event).expect("CC -> Data");
        assert!(matches!(vst3, Vst3Event::Data(_)), "CC should be a Data event");
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
        let event = MidiEvent::timing_clock(0);
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
        let event = MidiEvent::note_on(0, 3, 60, 0x8000).with_frame_offset(5);
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
        let event = MidiEvent::note_off(0, 0, 72, 0x4000).with_frame_offset(10);
        let vst3 = vst3_event_from_midi(&event).expect("NoteOff should convert");
        match &vst3 {
            Vst3Event::NoteOff(e) => {
                assert_eq!(e.pitch, 72);
                assert_eq!(e.header.sample_offset, 10);
            }
            _ => panic!("expected NoteOff variant"),
        }

        let back = vst3_to_midi_event(&vst3).expect("round-trip");
        assert!(back.is_note_off());
        assert_eq!(back.note(), Some(72));
    }

    #[test]
    fn poly_pressure_lands_in_poly_pressure_variant() {
        use tutti_midi_types::convert::midi1_cc_to_midi2;
        let event = MidiEvent::poly_pressure(0, 1, 60, midi1_cc_to_midi2(100)).with_frame_offset(0);
        let vst3 = vst3_event_from_midi(&event).expect("PolyPressure should convert");
        assert!(matches!(vst3, Vst3Event::PolyPressure(_)));
        let back = vst3_to_midi_event(&vst3).expect("round-trip");
        assert_eq!(back.note(), Some(60));
    }

    #[test]
    fn cc_falls_through_to_data_event() {
        use tutti_midi_types::convert::midi1_cc_to_midi2;
        let event = MidiEvent::cc(0, 2, 74, midi1_cc_to_midi2(100));
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
        let event = MidiEvent::pitch_bend(0, 0, midi1_pitch_bend_to_midi2(8192));
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
        let event = MidiEvent::program_change(0, 9, 42, None);
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
        let on = vst3_event_from_midi(&MidiEvent::note_on(0, 5, 67, 0x8000)).unwrap();
        let off = vst3_event_from_midi(&MidiEvent::note_off(0, 5, 67, 0)).unwrap();
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
        let event =
            MidiEvent::per_note_pitch_bend(0, 5, 67, midi1_pitch_bend_to_midi2(8192))
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
        let known =
            MidiEvent::per_note_controller(0, 0, 60, 74, midi1_cc_to_midi2(100), false);
        match vst3_event_from_midi(&known) {
            Some(Vst3Event::NoteExpression(e)) => {
                assert_eq!(
                    Some(e.type_id),
                    note_expression_type_to_id(NoteExpressionType::Brightness)
                );
                assert_eq!(e.note_id, note_id_for(0, 60));
            }
            other => panic!("expected Brightness note expression, got {other:?}"),
        }
        // An index with no VST3 expression counterpart is dropped.
        let unknown =
            MidiEvent::per_note_controller(0, 0, 60, 33, midi1_cc_to_midi2(100), false);
        assert!(vst3_event_from_midi(&unknown).is_none());
    }

    #[test]
    fn single_packet_sysex_round_trips_through_data_event() {
        // A short SysEx (identity request) → VST3 Data event with the SysEx
        // subtype and 0xF0 … 0xF7 framing, then back to the same UMP payload.
        let payload = [0x7E, 0x7F, 0x06, 0x01];
        let event = MidiEvent::sysex7_single(0, &payload)
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
    fn fragmented_sysex_is_not_forwarded() {
        // A multi-packet SysEx (>6 bytes) produces Start/Continue/End fragments;
        // none stand alone as a VST3 Data event, so each is dropped.
        let mut frags = Vec::new();
        MidiEvent::sysex7_fragments(0, &[1, 2, 3, 4, 5, 6, 7, 8], &mut frags);
        assert!(frags.len() > 1);
        for frag in &frags {
            assert!(vst3_event_from_midi(frag).is_none());
        }
    }

    #[test]
    fn note_on_velocity_round_trips_at_full_width() {
        // A MIDI-2 note-on carrying a value not representable in 7 bits keeps
        // (close to) its width through the f32 VST3 struct on the way back out,
        // rather than collapsing to a 7-bit grid point.
        let velocity_u16 = 0x9123; // not a multiple of the 7-bit step
        let event = MidiEvent::note_on(0, 0, 60, velocity_u16);
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
        let mut data_scratch = smallvec::SmallVec::new();
        let c = to_c_event(&ev, &mut data_scratch, &in_arena);
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

        let mut scratch = smallvec::SmallVec::new();
        let mut out = smallvec::SmallVec::new();
        for (ev, expect_scale) in [(scale_ev, true), (text_ev, false)] {
            let c = to_c_event(&ev, &mut scratch, &in_arena);
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
        let mut scratch = smallvec::SmallVec::new();
        let c = to_c_event(&ev, &mut scratch, &[]);
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
