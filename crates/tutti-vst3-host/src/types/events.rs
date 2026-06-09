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

use tutti_midi_types::{decode, encode, SemanticEvent};

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

/// Deterministic VST3 `noteId` for a `(channel, note)` pair.
///
/// VST3 addresses per-note events (note-on/off and note expression) by a host-
/// chosen `noteId` token that the plugin echoes back. We don't track live
/// voices, so we derive a stable id from the channel and note instead: a note-on
/// and any per-note expression for the *same* `(channel, note)` compute the same
/// id and therefore bind to the same VST3 voice. The mapping is a bijection into
/// `0..2048`, comfortably inside `i32`.
///
/// This is distinct from the spec's "use `noteId = -1` for channel/pitch
/// matching" fallback: that fallback only covers note-on/off, *not* note
/// expression — note-expression events carry no channel/pitch, only a `noteId`,
/// so they have no voice to attach to unless the host assigns real ids.
#[inline]
pub fn note_id_for(channel: u8, note: u8) -> i32 {
    (channel as i32) * 128 + (note as i32)
}

/// Encode a Tutti UMP [`MidiEvent`] as a [`Vst3Event`].
///
/// Notes and poly-pressure decode through [`tutti_midi_types::decode`] into
/// VST3's typed structs, so velocity / pressure arrive at the plugin at the
/// source event's full bit width (`f32` 0..1) rather than re-quantized through
/// 7-bit MIDI-1 — a MIDI-2 note-on keeps its 16-bit velocity. MIDI-2 per-note
/// pitch bend and per-note controllers become [`Vst3Event::NoteExpression`]
/// events bound to the matching voice via [`note_id_for`]. Everything else (CC,
/// channel-wide pitch bend, program change, channel pressure, SysEx) becomes a
/// [`Vst3Event::Data`] event, which is a 3-byte MIDI-1 frame by definition, so
/// that branch stays on the byte form. Returns `None` only for messages with no
/// MIDI-1 byte representation and no semantic mapping.
pub fn vst3_event_from_midi(event: &MidiEvent) -> Option<Vst3Event> {
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
                note_id: note_id_for(channel, note),
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
                note_id: note_id_for(channel, note),
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
                note_id: note_id_for(channel, note),
            }));
        }
        // MIDI-2 per-note pitch bend → VST3 tuning expression. Signed [-1, 1]
        // (center 0) maps to the unit [0, 1] (center 0.5 = no detune) VST3's
        // note-expression value convention uses.
        Some(SemanticEvent::PerNotePitchBend {
            channel,
            note,
            value,
        }) => {
            return Some(
                NoteExpressionValue {
                    sample_offset,
                    note_id: note_id_for(channel, note),
                    expression_type: NoteExpressionType::Tuning,
                    value: (value as f64 + 1.0) / 2.0,
                }
                .to_vst3_event(),
            );
        }
        // MIDI-2 per-note controllers map onto VST3 note-expression dimensions
        // for the indices that have a standard expression counterpart; other
        // per-note CCs have no VST3 expression equivalent and are dropped.
        Some(SemanticEvent::PerNoteController {
            channel,
            note,
            index,
            value,
        }) => {
            return per_note_controller_expression(index).map(|expression_type| {
                NoteExpressionValue {
                    sample_offset,
                    note_id: note_id_for(channel, note),
                    expression_type,
                    value: value as f64,
                }
                .to_vst3_event()
            });
        }
        // CC / channel pressure / channel pitch bend / program change carry no
        // extra resolution VST3 can use here — they ride the plugin's parameter
        // funnel (see `CcRoute`) or land as a raw Data event below.
        _ => {}
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

/// Decode a [`Vst3Event`] into a Tutti UMP [`MidiEvent`].
///
/// Notes and poly-pressure go through [`tutti_midi_types::encode`], so the
/// plugin's `f32` velocity / pressure is preserved at MIDI-2's full bit width
/// instead of being squashed to 7 bits. A [`Vst3Event::Data`] carrying SysEx is
/// rebuilt as a UMP SysEx7 packet; other Data events decode from their raw
/// MIDI-1 bytes. Returns `None` for [`Vst3Event::NoteExpression`] (not a
/// channel-voice MIDI message), for `Data` payloads shorter than 2 bytes, and
/// for a SysEx too long to fit a single UMP packet.
pub fn vst3_to_midi_event(event: &Vst3Event) -> Option<MidiEvent> {
    let frame = event.sample_offset().max(0) as u32;
    let semantic = match event {
        Vst3Event::NoteOn(e) => SemanticEvent::NoteOn {
            channel: (e.channel as u8) & 0x0F,
            note: (e.pitch as u8) & 0x7F,
            velocity: e.velocity,
        },
        Vst3Event::NoteOff(e) => SemanticEvent::NoteOff {
            channel: (e.channel as u8) & 0x0F,
            note: (e.pitch as u8) & 0x7F,
        },
        Vst3Event::PolyPressure(e) => SemanticEvent::KeyPressure {
            channel: (e.channel as u8) & 0x0F,
            note: (e.pitch as u8) & 0x7F,
            value: e.pressure,
        },
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
            return MidiEvent::from_midi1_bytes(frame, bytes);
        }
        Vst3Event::NoteExpression(_) => return None,
    };
    Some(encode(&semantic).with_frame_offset(frame))
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
                assert_eq!(e.type_id, NoteExpressionType::Brightness.to_type_id());
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
        let dec = tutti_midi_types::decode(&back).expect("decodes");
        match dec {
            SemanticEvent::NoteOn { velocity, .. } => {
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
}
