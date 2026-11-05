//! midi: part of the VST3 event vocabulary.

use super::*;

/// VST3's tuning expression for a per-note bend of `bend` (signed, `[-1, 1]`)
/// over a range of `range` semitones: `0.5 + semitones / 240`, the
/// `kTuningTypeID` convention (`[0, 1]` spans -120 to +120 semitones).
pub(super) fn tuning_expression(bend: f32, range: f32) -> f64 {
    0.5 + f64::from(bend) * f64::from(range) / 240.0
}

/// Encodes a Tutti UMP [`MidiEvent`] as a [`Vst3Event`], at the stream state
/// `ranges` holds: a per-note pitch bend is read over its channel's per-note
/// bend range ([`PerNoteBendRanges`], which a plugin's event list keeps across
/// blocks), and a per-note bend sensitivity (RPN #00/07) sets that range and
/// is consumed (`None`): VST3 has no event for it.
///
/// Notes and poly-pressure are matched on `midi2`'s Channel Voice 2 vocabulary
/// via [`tutti_midi_types::normalize`], so velocity / pressure arrive at the
/// plugin at the source event's full bit width (converted to VST3's `f32` 0..1
/// here) rather than re-quantized through 7-bit MIDI-1 — a MIDI-2 note-on keeps
/// its 16-bit velocity. MIDI-2 per-note pitch bend and per-note controllers
/// become [`Vst3Event::NoteExpression`] events bound to the matching voice via
/// [`note_id_for`]. Everything else (CC, channel-wide pitch bend, program change,
/// channel pressure, a single-packet SysEx) becomes a [`Vst3Event::Data`]
/// event, which is a MIDI-1 frame by definition, so that branch stays on the
/// byte form. Returns `None` for messages with no MIDI-1 byte representation
/// and no per-note mapping, and for a packet of a multi-packet SysEx, which
/// the event list reassembles into a [`Vst3Event::SysEx`].
/// Text sizes are bounded as in `to_c_event`.
///
/// **`cast_possible_wrap` is deliberately NOT allowed here.** Listing it to
/// cover the `i64` note-expression slot would silently re-permit the
/// `frame_offset as i32` wrap this module exists to prevent. The sign-changing
/// casts go through named helpers
/// (`reinterpret_expression_value`) that carry their own narrow allow, so the
/// deny still bites in the function body.
#[allow(
    clippy::cast_possible_truncation,
    reason = "text sizes bounded by MAX_EVENT_TEXT_LEN"
)]
pub fn vst3_event_from_midi(
    event: &MidiEvent,
    ranges: &mut PerNoteBendRanges,
) -> Option<Vst3Event> {
    use tutti_midi_types::convert::{bend_u32_to_signed_f32, u16_to_unit_f32, u32_to_unit_f32};
    use tutti_midi_types::midi2::channel_voice2::ChannelVoice2 as Cv2;
    use tutti_midi_types::midi2::{Channeled, UmpMessage};

    if ranges.observe(event) {
        return None;
    }

    // `frame_offset` is `u32`; VST3's `sampleOffset` is `i32`. A value past
    // `i32::MAX` wraps negative, and the plugin indexes its buffers with it —
    // an out-of-bounds read inside the plugin, not in host code. Saturating keeps
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
            // MIDI-2 per-note pitch bend → VST3 tuning expression: the bend
            // (signed, [-1, 1]) over the channel's per-note bend range, in
            // semitones, on VST3's ±120-semitone tuning scale.
            Cv2::PerNotePitchBend(m) => {
                let note = u8::from(m.note_number());
                let bend = bend_u32_to_signed_f32(m.pitch_bend_data());
                // Tuning is VST3-encodable, so `note_expression_to_vst3` is `Some`.
                return note_expression_to_vst3(&NoteExpressionValue {
                    sample_offset,
                    note_id: note_id_for(channel, note),
                    expression_type: NoteExpressionType::Tuning,
                    value: tuning_expression(bend, ranges.semitones(channel)),
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
    // message *with* its 0xF0 … 0xF7 delimiters. A single-packet (`SINGLE`)
    // message fits the inline `[u8; 16]` (≤ 6 bytes, ≤ 8 framed). A packet of
    // a multi-packet message cannot stand alone: the event list reassembles
    // those (`SysExAssembler`) and stages the whole message as
    // `Vst3Event::SysEx`, so here they map to nothing.
    if let Some((status, payload, n)) = event.sysex7_payload() {
        use tutti_midi_types::ump::SYSEX7_STATUS_SINGLE;
        if status != SYSEX7_STATUS_SINGLE {
            return None;
        }
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

    // Data event: the VST3 carrier for any 3-byte MIDI-1 message. VST3 defines
    // no Data subtype for plain channel-voice bytes (only kMidiSysEx), so the
    // subtype stays 0 — these are a fallback the plugin reads as raw bytes.
    let (bytes, _len) = event.to_midi1_bytes()?;
    Some(midi1_data_event(sample_offset, bytes))
}

/// One MIDI-1 message as the VST3 Data event that carries it (see the fallback
/// in [`vst3_event_from_midi`]).
pub fn midi1_data_event(sample_offset: i32, bytes: [u8; 3]) -> Vst3Event {
    let mut data = [0u8; 16];
    data[..3].copy_from_slice(&bytes);
    Vst3Event::Data(DataEvent {
        header: EventHeader {
            bus_index: 0,
            sample_offset,
            ppq_position: 0.0,
            flags: 0,
            event_type: K_DATA_EVENT,
        },
        size: 3,
        event_type: 0,
        bytes: data,
    })
}

/// Maps a MIDI-2 per-note controller index to the VST3 note-expression dimension
/// it corresponds to, or `None` when there's no standard counterpart.
///
/// Only the indices that align with a VST3 standard expression are forwarded;
/// the MIDI-2 per-note CC namespace is otherwise open-ended and has no general
/// VST3 expression equivalent. The chosen indices follow the usual MIDI CC
/// assignments (volume = 7, pan = 10, brightness = 74).
pub(super) fn per_note_controller_expression(index: u8) -> Option<NoteExpressionType> {
    match index {
        7 => Some(NoteExpressionType::Volume),
        10 => Some(NoteExpressionType::Pan),
        74 => Some(NoteExpressionType::Brightness),
        _ => None,
    }
}

/// Maps a MIDI-2 *registered* per-note controller (a spec-named
/// [`Controller`](tutti_midi_types::midi2::channel_voice2::Controller)) to the
/// VST3 note-expression dimension it corresponds to, with its raw 32-bit data.
///
/// Registered controllers carry spec meaning by *name*, not by a bare index, so
/// the enum is matched directly. Only the dimensions with a VST3 standard
/// expression counterpart are forwarded (Volume, Pan, Brightness = CC74 /
/// SoundController #5); the rest have no VST3 equivalent and are dropped.
pub(super) fn registered_controller_expression(
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

/// Decodes a [`Vst3Event`] into a Tutti UMP [`MidiEvent`].
///
/// Notes and poly-pressure go through [`tutti_midi_types::convert`], so the
/// plugin's `f32` velocity / pressure is preserved at MIDI-2's full bit width
/// instead of being squashed to 7 bits. A [`Vst3Event::Data`] carrying SysEx is
/// rebuilt as a UMP SysEx7 packet; other Data events decode from their raw
/// MIDI-1 bytes. A plugin-emitted [`Vst3Event::LegacyMidiCcOut`] is rebuilt as
/// the CC / pitch-bend / poly-pressure UMP message it stands for. Returns `None`
/// for the non-MIDI events (note-expression value/text/int, chord, scale), for
/// a reassembled [`Vst3Event::SysEx`] (input-only), for
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
pub(super) fn midi_channel(raw: i16) -> u8 {
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
pub(super) fn reinterpret_expression_value(raw: u64) -> i64 {
    raw as i64
}

/// A VST3 `i16` pitch field as a 7-bit MIDI note number.
#[allow(
    clippy::cast_possible_truncation,
    reason = "masked to 7 bits, so the narrowing cannot lose a bit the mask keeps"
)]
pub(super) fn midi_note(raw: i16) -> u8 {
    (raw as u8) & 0x7F
}

pub fn vst3_to_midi_event(event: &Vst3Event) -> Option<MidiEvent> {
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
        // Its bytes are in the event list's buffer, which this conversion
        // has no access to; the list drains it as SysEx7 packets.
        Vst3Event::SysEx(_) => return None,
        Vst3Event::NoteExpression(_)
        | Vst3Event::NoteExpressionText(_)
        | Vst3Event::Chord(_)
        | Vst3Event::Scale(_)
        | Vst3Event::NoteExpressionInt(_) => return None,
    };
    Some(built.with_frame_offset(frame))
}
