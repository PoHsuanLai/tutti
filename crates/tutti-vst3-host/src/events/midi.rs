//! `MidiEvent` to VST3 event and back.

use tutti_midi_types::PerNoteBendRanges;
use tutti_midi_types::convert::{
    bend_u32_to_signed_f32, u16_to_unit_f32, u32_to_unit_f32, unit_f32_to_u16, unit_f32_to_u32,
};
use tutti_midi_types::midi2::channel_voice2::{ChannelVoice2 as Cv2, Controller};
use tutti_midi_types::midi2::{Channeled, UmpMessage};
use tutti_midi_types::ump::SYSEX7_STATUS_SINGLE;
use tutti_plugin_types::{NoteExpressionType, NoteExpressionValue, note_id_for};
use tutti_ump::{MidiChannel, MidiGroup};

use super::{
    Data, Event, K_DATA_TYPE_MIDI_SYSEX, MidiEvent, NoteOff, NoteOn, Payload, PolyPressure,
    ToEvent, sample_offset,
};

/// VST3's tuning expression for a per-note bend of `bend` (signed, `[-1, 1]`)
/// over `range` semitones: `0.5 + semitones / 240` (`kTuningTypeID` spans -120
/// to +120 semitones over `[0, 1]`).
fn tuning_expression(bend: f32, range: f32) -> f64 {
    0.5 + f64::from(bend) * f64::from(range) / 240.0
}

/// Encodes UMP [`MidiEvent`]s as VST3 [`Event`]s, keeping each channel's
/// per-note bend range ([`PerNoteBendRanges`]) across blocks.
#[derive(Debug, Default)]
pub struct MidiEncoder {
    ranges: PerNoteBendRanges,
}

impl MidiEncoder {
    /// The VST3 event for `event`, at the stream state the encoder holds.
    ///
    /// Notes and poly-pressure are matched on Channel Voice 2 via
    /// [`tutti_midi_types::normalize`], so velocity and pressure reach the
    /// plugin at their full bit width as `f32` rather than through 7-bit MIDI 1.
    /// A per-note pitch bend becomes a tuning expression over its channel's
    /// range, and a per-note bend sensitivity (RPN #00/07) sets that range and
    /// is consumed (`None`): VST3 has no event for it. Registered and
    /// assignable per-note controllers with a VST3 counterpart become
    /// expression events. A single-packet SysEx, and any other message with a
    /// MIDI-1 form, becomes a `Data` event. `None` for messages with neither,
    /// and for a packet of a multi-packet SysEx, which the list reassembles.
    pub fn encode(&mut self, event: &MidiEvent) -> Option<Event> {
        if self.ranges.observe(event) {
            return None;
        }

        let offset = sample_offset(event.frame_offset);
        let normalized = tutti_midi_types::normalize(event);
        if let Ok(UmpMessage::ChannelVoice2(cv2)) = UmpMessage::try_from(normalized.data_words()) {
            let channel = u8::from(cv2.channel());
            match cv2 {
                Cv2::NoteOn(m) => {
                    let note = u8::from(m.note_number());
                    return Some(Event::at(
                        offset,
                        Payload::NoteOn(NoteOn::new(
                            channel.into(),
                            note.into(),
                            u16_to_unit_f32(m.velocity()),
                            note_id_for(channel, note),
                        )),
                    ));
                }
                Cv2::NoteOff(m) => {
                    let note = u8::from(m.note_number());
                    return Some(Event::at(
                        offset,
                        Payload::NoteOff(NoteOff::new(
                            channel.into(),
                            note.into(),
                            // The release velocity, at its full width.
                            u16_to_unit_f32(m.velocity()),
                            note_id_for(channel, note),
                        )),
                    ));
                }
                Cv2::KeyPressure(m) => {
                    let note = u8::from(m.note_number());
                    return Some(Event::at(
                        offset,
                        Payload::PolyPressure(PolyPressure::new(
                            channel.into(),
                            note.into(),
                            u32_to_unit_f32(m.key_pressure_data()),
                            note_id_for(channel, note),
                        )),
                    ));
                }
                Cv2::PerNotePitchBend(m) => {
                    let note = u8::from(m.note_number());
                    let bend = bend_u32_to_signed_f32(m.pitch_bend_data());
                    return NoteExpressionValue {
                        sample_offset: offset,
                        note_id: note_id_for(channel, note),
                        expression_type: NoteExpressionType::Tuning,
                        value: tuning_expression(bend, self.ranges.semitones(channel)),
                    }
                    .to_event();
                }
                Cv2::RegisteredPerNoteController(m) => {
                    let note = u8::from(m.note_number());
                    return registered_controller_expression(m.controller()).and_then(
                        |(expression_type, data)| {
                            NoteExpressionValue {
                                sample_offset: offset,
                                note_id: note_id_for(channel, note),
                                expression_type,
                                value: f64::from(u32_to_unit_f32(data)),
                            }
                            .to_event()
                        },
                    );
                }
                Cv2::AssignablePerNoteController(m) => {
                    let note = u8::from(m.note_number());
                    let value = u32_to_unit_f32(m.controller_data());
                    return per_note_controller_expression(m.index()).and_then(|expression_type| {
                        NoteExpressionValue {
                            sample_offset: offset,
                            note_id: note_id_for(channel, note),
                            expression_type,
                            value: f64::from(value),
                        }
                        .to_event()
                    });
                }
                // CC, channel pressure, channel pitch bend and program change have no
                // extra resolution VST3 can use: they ride the plugin's parameter funnel
                // (`CcRoute`) or land as a raw Data event below.
                _ => {}
            }
        }

        // VST3 wants a SysEx with its 0xF0 ... 0xF7 delimiters. A single-packet
        // message fits the inline bytes (at most 6 payload, 8 framed); a packet
        // of a multi-packet message cannot stand alone.
        if let Some((status, payload, n)) = event.sysex7_payload() {
            if status != SYSEX7_STATUS_SINGLE {
                return None;
            }
            let mut framed = [0u8; Data::CAPACITY];
            framed[0] = 0xF0;
            framed[1..=n].copy_from_slice(&payload[..n]);
            framed[n + 1] = 0xF7;
            let data = Data::new(K_DATA_TYPE_MIDI_SYSEX, &framed[..n + 2])?;
            return Some(Event::at(offset, Payload::Data(data)));
        }

        // The VST3 carrier for any 3-byte MIDI-1 message: a Data event of subtype 0
        // (the only subtype the SDK defines is SysEx).
        let (bytes, _len) = event.to_midi1_bytes()?;
        Some(midi1_data(offset, bytes))
    }
}

/// One MIDI-1 message as the Data event that carries it.
pub(super) fn midi1_data(sample_offset: i32, bytes: [u8; 3]) -> Event {
    Event::at(sample_offset, Payload::Data(Data::inline(0, bytes)))
}

/// The VST3 note-expression dimension a MIDI-2 assignable per-note controller
/// corresponds to: the usual CC assignments, volume 7, pan 10, brightness 74.
fn per_note_controller_expression(index: u8) -> Option<NoteExpressionType> {
    match index {
        7 => Some(NoteExpressionType::Volume),
        10 => Some(NoteExpressionType::Pan),
        74 => Some(NoteExpressionType::Brightness),
        _ => None,
    }
}

/// The same for a registered per-note controller, with its raw 32-bit data.
/// Volume, pan and brightness (CC74, sound controller #5) have a VST3
/// counterpart; the rest are dropped.
fn registered_controller_expression(controller: Controller) -> Option<(NoteExpressionType, u32)> {
    match controller {
        Controller::Volume(data) => Some((NoteExpressionType::Volume, data)),
        Controller::Pan(data) => Some((NoteExpressionType::Pan, data)),
        Controller::Brightness(data) | Controller::SoundController { index: 5, data } => {
            Some((NoteExpressionType::Brightness, data))
        }
        _ => None,
    }
}

/// A VST3 `i16` channel field as a 4-bit MIDI channel.
#[allow(
    clippy::cast_possible_truncation,
    reason = "masked to 4 bits, so the narrowing cannot lose a bit the mask keeps"
)]
pub(super) fn midi_channel(raw: i16) -> u8 {
    (raw as u8) & 0x0F
}

/// A VST3 `i16` pitch field as a 7-bit MIDI note number.
#[allow(
    clippy::cast_possible_truncation,
    reason = "masked to 7 bits, so the narrowing cannot lose a bit the mask keeps"
)]
fn midi_note(raw: i16) -> u8 {
    (raw as u8) & 0x7F
}

/// See [`EventExt::to_midi`](super::EventExt::to_midi).
pub(super) fn decode(event: &Event) -> Option<MidiEvent> {
    let frame = event.sample_offset().max(0).cast_unsigned();
    // Notes and poly-pressure build native MIDI-2 Channel Voice events so the
    // plugin's f32 velocity and pressure keep their full 16 and 32-bit width.
    let built = match &event.payload {
        Payload::NoteOn(e) => MidiEvent::note_on(
            MidiGroup::FIRST,
            MidiChannel::masked(midi_channel(e.channel)),
            midi_note(e.pitch),
            unit_f32_to_u16(e.velocity),
        ),
        Payload::NoteOff(e) => MidiEvent::note_off(
            MidiGroup::FIRST,
            MidiChannel::masked(midi_channel(e.channel)),
            midi_note(e.pitch),
            unit_f32_to_u16(e.velocity),
        ),
        Payload::PolyPressure(e) => MidiEvent::poly_pressure(
            MidiGroup::FIRST,
            MidiChannel::masked(midi_channel(e.channel)),
            midi_note(e.pitch),
            unit_f32_to_u32(e.pressure),
        ),
        Payload::Data(d) => {
            let bytes = d.payload();
            if bytes.len() < 2 {
                return None;
            }
            // The 0xF0 framing is the reliable SysEx signal: the kMidiSysEx subtype is 0,
            // as is the raw channel-voice fallback's. Strip the delimiters and rebuild a
            // single packet; a message too long for one packet is dropped, not truncated.
            if bytes[0] == 0xF0 {
                let inner = bytes.strip_prefix(&[0xF0]).unwrap_or(bytes);
                let inner = inner.strip_suffix(&[0xF7]).unwrap_or(inner);
                return MidiEvent::sysex7_single(MidiGroup::FIRST, inner)
                    .map(|m| m.with_frame_offset(frame));
            }
            // Promoted to Channel Voice 2 here, so the engine sees one vocabulary
            // whatever the source. System messages pass through unchanged.
            return MidiEvent::from_midi1_bytes(frame, bytes)
                .map(|e| tutti_midi_types::normalize(&e));
        }
        Payload::LegacyCcOut(e) => return super::legacy_cc::decode(e, frame),
        // A reassembled SysEx's bytes are in the list's buffer, which the list drains.
        Payload::SysEx(_)
        | Payload::ExpressionValue(_)
        | Payload::ExpressionText(_)
        | Payload::Chord(_)
        | Payload::Scale(_)
        | Payload::ExpressionInt(_) => return None,
    };
    Some(built.with_frame_offset(frame))
}
