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

use tutti_midi_types::{decode, SemanticEvent};

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

/// Safe tagged-enum form of the VST3 `Event` union. See
/// [`vst3_event_from_midi`] / [`vst3_to_midi_event`] for round-trip MIDI
/// conversion.
#[derive(Debug, Clone, Copy)]
pub enum Vst3Event {
    NoteOn(NoteOnEvent),
    NoteOff(NoteOffEvent),
    Data(DataEvent),
    PolyPressure(PolyPressureEvent),
    NoteExpression(NoteExpressionValueEvent),
}

impl Vst3Event {
    /// Frame offset within the current processing block, from the underlying
    /// [`EventHeader`].
    pub fn sample_offset(&self) -> i32 {
        match self {
            Vst3Event::NoteOn(e) => e.header.sample_offset,
            Vst3Event::NoteOff(e) => e.header.sample_offset,
            Vst3Event::Data(e) => e.header.sample_offset,
            Vst3Event::PolyPressure(e) => e.header.sample_offset,
            Vst3Event::NoteExpression(e) => e.header.sample_offset,
        }
    }
}

/// Convert our flat `Vst3Event` into the C `Event` struct the vst3 crate expects.
///
/// `data_storage` acts as an owner for the `DataEvent.bytes` pointer: when a
/// `Data` event is encoded, the buffer is pushed into `data_storage` and the
/// event's `bytes` field points at the most-recently-pushed slot. Callers must
/// keep `data_storage` alive at least as long as the returned `Event` is used.
pub(crate) fn to_c_event(
    event: &Vst3Event,
    data_storage: &mut smallvec::SmallVec<[[u8; 16]; 8]>,
) -> vst3::Steinberg::Vst::Event {
    let header = match event {
        Vst3Event::NoteOn(e) => &e.header,
        Vst3Event::NoteOff(e) => &e.header,
        Vst3Event::Data(e) => &e.header,
        Vst3Event::PolyPressure(e) => &e.header,
        Vst3Event::NoteExpression(e) => &e.header,
    };

    let mut out: vst3::Steinberg::Vst::Event = unsafe { std::mem::zeroed() };
    out.busIndex = header.bus_index;
    out.sampleOffset = header.sample_offset;
    out.ppqPosition = header.ppq_position;
    out.flags = header.flags;
    out.r#type = header.event_type;

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
    }

    out
}

/// Convert from the vst3 crate's tagged-union `Event` to our safe enum.
///
/// # Safety
///
/// `event.type_` must accurately label the variant stored in `__field0`.
#[allow(clippy::unnecessary_cast)]
pub(crate) unsafe fn from_c_event(event: &vst3::Steinberg::Vst::Event) -> Option<Vst3Event> {
    let header = EventHeader {
        bus_index: event.busIndex,
        sample_offset: event.sampleOffset,
        ppq_position: event.ppqPosition,
        flags: event.flags,
        event_type: event.r#type,
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
        _ => None,
    }
}

/// Encode a Tutti UMP [`MidiEvent`] as a [`Vst3Event`].
///
/// Notes and poly-pressure decode through [`tutti_midi_types::decode`] into
/// VST3's typed structs, so velocity / pressure arrive at the plugin at the
/// source event's full bit width (`f32` 0..1) rather than re-quantized through
/// 7-bit MIDI-1 — a MIDI-2 note-on keeps its 16-bit velocity. Everything else
/// (CC, pitch bend, program change, channel pressure, SysEx) becomes a
/// [`Vst3Event::Data`] event, which is a 3-byte MIDI-1 frame by definition, so
/// that branch stays on the byte form. Returns `None` only for messages with
/// no MIDI-1 byte representation and no semantic note/pressure mapping.
pub fn vst3_event_from_midi(event: &MidiEvent) -> Option<Vst3Event> {
    let sample_offset = event.frame_offset as i32;
    let header = EventHeader {
        bus_index: 0,
        sample_offset,
        ppq_position: 0.0,
        flags: 0,
        event_type: 0,
    };

    // Notes and poly-pressure get VST3's typed structs with full-width f32
    // values; the rest fall through to a 3-byte Data event below.
    match decode(event) {
        Some(SemanticEvent::NoteOn {
            channel,
            note,
            velocity,
        }) => {
            return Some(Vst3Event::NoteOn(NoteOnEvent {
                header: EventHeader {
                    event_type: K_NOTE_ON_EVENT,
                    ..header
                },
                channel: channel as i16,
                pitch: note as i16,
                tuning: 0.0,
                velocity,
                length: 0,
                note_id: -1,
            }));
        }
        Some(SemanticEvent::NoteOff { channel, note }) => {
            return Some(Vst3Event::NoteOff(NoteOffEvent {
                header: EventHeader {
                    event_type: K_NOTE_OFF_EVENT,
                    ..header
                },
                channel: channel as i16,
                pitch: note as i16,
                velocity: 0.0,
                note_id: -1,
                tuning: 0.0,
            }));
        }
        Some(SemanticEvent::KeyPressure {
            channel,
            note,
            value,
        }) => {
            return Some(Vst3Event::PolyPressure(PolyPressureEvent {
                header: EventHeader {
                    event_type: K_POLY_PRESSURE_EVENT,
                    ..header
                },
                channel: channel as i16,
                pitch: note as i16,
                pressure: value,
                note_id: -1,
            }));
        }
        // CC / channel pressure / pitch bend / program change carry no extra
        // resolution VST3 can use here — they ride the plugin's parameter
        // funnel (see `CcRoute`) or land as a raw Data event. Per-note
        // controllers/bends would need a note-id allocation scheme to bind to
        // a VST3 voice (our note-ons use note_id = -1), so they aren't mapped
        // to note-expression yet — they fall through to the byte form, which
        // drops them (no MIDI-1 encoding) rather than misattributing a voice.
        _ => {}
    }

    // Data event: the VST3 carrier for any 3-byte MIDI-1 message.
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

/// Decode a [`Vst3Event`] into a Tutti UMP [`MidiEvent`].
///
/// Returns `None` for [`Vst3Event::NoteExpression`] (not representable as
/// MIDI) and for [`Vst3Event::Data`] payloads shorter than 2 bytes.
pub fn vst3_to_midi_event(event: &Vst3Event) -> Option<MidiEvent> {
    match event {
        Vst3Event::NoteOn(e) => {
            let velocity_u7 = (e.velocity * 127.0).clamp(0.0, 127.0) as u8;
            let status = 0x90 | ((e.channel as u8) & 0x0F);
            let frame = e.header.sample_offset.max(0) as u32;
            MidiEvent::from_midi1_bytes(frame, &[status, e.pitch as u8 & 0x7F, velocity_u7])
        }
        Vst3Event::NoteOff(e) => {
            let velocity_u7 = (e.velocity * 127.0).clamp(0.0, 127.0) as u8;
            let status = 0x80 | ((e.channel as u8) & 0x0F);
            let frame = e.header.sample_offset.max(0) as u32;
            MidiEvent::from_midi1_bytes(frame, &[status, e.pitch as u8 & 0x7F, velocity_u7])
        }
        Vst3Event::PolyPressure(e) => {
            let pressure_u7 = (e.pressure * 127.0).clamp(0.0, 127.0) as u8;
            let status = 0xA0 | ((e.channel as u8) & 0x0F);
            let frame = e.header.sample_offset.max(0) as u32;
            MidiEvent::from_midi1_bytes(frame, &[status, e.pitch as u8 & 0x7F, pressure_u7])
        }
        Vst3Event::Data(e) => {
            if e.size < 2 {
                return None;
            }
            let frame = e.header.sample_offset.max(0) as u32;
            MidiEvent::from_midi1_bytes(frame, &e.bytes[..e.size as usize])
        }
        Vst3Event::NoteExpression(_) => None,
    }
}

/// VST3-standard note-expression dimensions carried on
/// [`NoteExpressionValueEvent`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoteExpressionType {
    /// Volume expression (0.0 = -inf dB, 0.5 = 0dB, 1.0 = +6dB).
    Volume,
    /// Pan expression (0.0 = left, 0.5 = center, 1.0 = right).
    Pan,
    /// Tuning in semitones (-120 to +120 mapped to 0.0-1.0).
    Tuning,
    /// Vibrato intensity (0.0 = none, 1.0 = max).
    Vibrato,
    /// Brightness/filter cutoff (0.0 = dark, 1.0 = bright).
    Brightness,
}

impl NoteExpressionType {
    /// Encode as the integer `typeId` VST3 uses on the wire.
    pub fn to_type_id(self) -> u32 {
        match self {
            NoteExpressionType::Volume => 0,
            NoteExpressionType::Pan => 1,
            NoteExpressionType::Tuning => 2,
            NoteExpressionType::Vibrato => 3,
            NoteExpressionType::Brightness => 4,
        }
    }

    /// Decode a VST3 `typeId` back to an enum value; `None` for unknown ids.
    pub fn from_type_id(id: u32) -> Option<Self> {
        match id {
            0 => Some(NoteExpressionType::Volume),
            1 => Some(NoteExpressionType::Pan),
            2 => Some(NoteExpressionType::Tuning),
            3 => Some(NoteExpressionType::Vibrato),
            4 => Some(NoteExpressionType::Brightness),
            _ => None,
        }
    }
}

/// Host-facing note-expression sample. Paired with a note id so the plugin
/// applies it to a specific active voice.
#[derive(Debug, Clone, Copy)]
pub struct NoteExpressionValue {
    /// Frame offset within the current processing block.
    pub sample_offset: i32,
    /// Note id returned by the originating note-on.
    pub note_id: i32,
    /// Which expression dimension this sample drives.
    pub expression_type: NoteExpressionType,
    /// 0.0 to 1.0
    pub value: f64,
}

impl NoteExpressionValue {
    /// Convert to the tagged-enum [`Vst3Event`] form accepted by the event
    /// list staging code.
    pub fn to_vst3_event(&self) -> Vst3Event {
        let header = EventHeader {
            bus_index: 0,
            sample_offset: self.sample_offset,
            ppq_position: 0.0,
            flags: 0,
            event_type: K_NOTE_EXPRESSION_VALUE_EVENT,
        };

        Vst3Event::NoteExpression(NoteExpressionValueEvent {
            header,
            note_id: self.note_id,
            type_id: self.expression_type.to_type_id(),
            value: self.value,
        })
    }
}

/// Extract a [`NoteExpressionValue`] from a [`Vst3Event`], or `None` for any
/// non-expression variant or unrecognised `type_id`.
pub fn vst3_to_note_expression(event: &Vst3Event) -> Option<NoteExpressionValue> {
    match event {
        Vst3Event::NoteExpression(e) => {
            let expression_type = NoteExpressionType::from_type_id(e.type_id)?;
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

#[cfg(test)]
mod tests {
    //! MIDI round-trip tests through `vst3_event_from_midi` + `vst3_to_midi_event`.

    use super::*;

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
        let vst3 = expr.to_vst3_event();
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
}
