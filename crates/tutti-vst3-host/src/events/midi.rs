//! A UMP event to VST3 event and back.

use tutti_graph::UmpAt;
use tutti_midi_protocol::{
    bend_u32_to_signed_f32, u16_to_unit_f32, u32_to_unit_f32, unit_f32_to_u16, unit_f32_to_u32,
};
use tutti_midi_protocol::{midi1, narrow, widen, widen_message};
use tutti_plugin_types::PerNoteBendRanges;
use tutti_plugin_types::{
    NoteExpressionType, NoteExpressionValue, WireMidi, note_id_for, sysex_packets,
};
use tutti_ump::{
    Address, Attribute, Message, Midi1Bytes, Midi1Parser, MidiChannel, MidiGroup, NoteNumber,
    RegisteredPerNoteController, Ump, Unknown, Voice1, Voice2, decode as decode_ump,
};

use super::{
    Data, Event, K_DATA_TYPE_MIDI_SYSEX, NoteOff, NoteOn, Payload, PolyPressure, ToEvent,
    sample_offset,
};

/// VST3's tuning expression for a per-note bend of `bend` (signed, `[-1, 1]`)
/// over `range` semitones: `0.5 + semitones / 240` (`kTuningTypeID` spans -120
/// to +120 semitones over `[0, 1]`).
fn tuning_expression(bend: f32, range: f32) -> f64 {
    0.5 + f64::from(bend) * f64::from(range) / 240.0
}

/// Encodes UMP events (`(Offset, Ump)`) as VST3 [`Event`]s, keeping each channel's
/// per-note bend range ([`PerNoteBendRanges`]) across blocks.
#[derive(Debug, Default)]
pub struct MidiEncoder {
    ranges: PerNoteBendRanges,
}

impl MidiEncoder {
    /// The VST3 event for `event`, at the stream state the encoder holds.
    ///
    /// The packet is read through `tutti_ump::decode`. A MIDI 1.0 note or key pressure
    /// is widened ([`widen`]) and a MIDI 2.0 one is read as is, so velocity
    /// and pressure reach the plugin at their full bit width as `f32` rather
    /// than through 7-bit MIDI 1. A per-note pitch bend becomes a tuning
    /// expression over its channel's range, and a per-note bend sensitivity
    /// (RPN #00/07) sets that range and is consumed (`None`): VST3 has no
    /// event for it. Registered and assignable per-note controllers with a
    /// VST3 counterpart become expression events. A single-packet SysEx, and
    /// any other message with a MIDI-1 form, becomes a `Data` event.
    ///
    /// `None` is a message this host is a sink for, arm by arm in
    /// the encoder: VST3's event list has no raw UMP event, so a
    /// message with no VST3 event and no MIDI 1.0 form, including an
    /// [`Unknown`] packet, goes no further. A packet of a multi-packet SysEx
    /// is `None` too: the list reassembles it.
    ///
    /// The group is ignored: one plugin instance has one set of channels,
    /// whatever group a message arrives on.
    pub fn encode(&mut self, event: &UmpAt) -> Option<Event> {
        let (at, ump) = event;
        if self.ranges.observe(ump) {
            return None;
        }
        let offset = sample_offset(at.get());
        match decode_ump(*ump) {
            Ok(message) => self.message(offset, message),
            // A reserved or undefined packet. VST3 has no event that carries
            // raw words, so there is no way to forward it: this host is a
            // sink for it.
            Err(Unknown(_)) => None,
        }
    }

    fn message(&self, offset: i32, message: Message) -> Option<Event> {
        match message {
            Message::Midi1(address, voice) => self.voice1(offset, address.channel, voice),
            Message::Midi2(address, voice) => self.voice2(offset, address.channel, voice),
            Message::System(_, system) => Some(midi1_data(offset, system.bytes())),
            // VST3 wants a SysEx with its 0xF0 ... 0xF7 delimiters. A
            // single-packet message fits the inline bytes (at most 6 payload,
            // 8 framed); a packet of a multi-packet message cannot stand
            // alone, and the list reassembles those.
            Message::Sysex7(_, packet) => {
                let data = Data::try_from_iter(K_DATA_TYPE_MIDI_SYSEX, packet.framed()?)?;
                Some(Event::at(offset, Payload::Data(data)))
            }
            // Sinks: VST3 has no event for a clock, a stream message, flex
            // data or SysEx 8, and the Data event carries only MIDI 1.0 bytes.
            Message::Utility(_) | Message::Data128(..) | Message::Flex(..) | Message::Stream(_) => {
                None
            }
        }
    }

    fn voice2(&self, offset: i32, channel: MidiChannel, voice: Voice2) -> Option<Event> {
        let ch = channel.get();
        match voice {
            // A MIDI 2.0 Note On at velocity 0 is sent as a Note Off at
            // release velocity 0, though M2-104 §7.4.2 keeps it a Note On. A
            // widened MIDI 1.0 Note On at velocity 0 never gets here: it
            // arrives as a Note Off.
            Voice2::NoteOn {
                note, velocity: 0, ..
            } => Some(note_off(offset, channel, note, 0)),
            // The attribute is not read: the Note On's `tuning` stays 0.
            Voice2::NoteOn { note, velocity, .. } => Some(Event::at(
                offset,
                Payload::NoteOn(NoteOn::new(
                    ch.into(),
                    note.get().into(),
                    u16_to_unit_f32(velocity),
                    note_id_for(channel, note),
                )),
            )),
            Voice2::NoteOff { note, velocity, .. } => {
                // The release velocity, at its full width.
                Some(note_off(offset, channel, note, velocity))
            }
            Voice2::PolyPressure { note, value } => Some(Event::at(
                offset,
                Payload::PolyPressure(PolyPressure::new(
                    ch.into(),
                    note.get().into(),
                    u32_to_unit_f32(value),
                    note_id_for(channel, note),
                )),
            )),
            Voice2::PerNotePitchBend { note, value } => {
                let bend = bend_u32_to_signed_f32(value);
                NoteExpressionValue {
                    sample_offset: offset,
                    note_id: note_id_for(channel, note),
                    expression_type: NoteExpressionType::Tuning,
                    value: tuning_expression(bend, self.ranges.semitones(ch)),
                }
                .to_event()
            }
            Voice2::RegisteredPerNote {
                note,
                controller,
                value,
            } => {
                let expression_type = registered_controller_expression(controller)?;
                NoteExpressionValue {
                    sample_offset: offset,
                    note_id: note_id_for(channel, note),
                    expression_type,
                    value: f64::from(u32_to_unit_f32(value)),
                }
                .to_event()
            }
            Voice2::AssignablePerNote {
                note,
                controller,
                value,
            } => {
                let expression_type = per_note_controller_expression(controller)?;
                NoteExpressionValue {
                    sample_offset: offset,
                    note_id: note_id_for(channel, note),
                    expression_type,
                    value: f64::from(u32_to_unit_f32(value)),
                }
                .to_event()
            }
            // CC, channel pressure, channel pitch bend and program change
            // have no extra resolution VST3 can use: they ride the plugin's
            // parameter funnel (`CcRoute`) or land as a raw Data event. A
            // Program Change sends its program only: a bank is three messages,
            // staged by `bank_program`.
            Voice2::ControlChange { .. }
            | Voice2::ChannelPressure(_)
            | Voice2::PitchBend(_)
            | Voice2::ProgramChange { .. } => {
                narrow(voice).map(|voice| midi1_data(offset, voice.bytes(channel)))
            }
            // Sinks: VST3 has no event for an RPN or NRPN (the per-note bend
            // sensitivity is consumed in `encode`), and per-note management
            // has nothing to act on.
            Voice2::Registered { .. }
            | Voice2::Assignable { .. }
            | Voice2::RelativeRegistered { .. }
            | Voice2::RelativeAssignable { .. }
            | Voice2::PerNoteManagement { .. } => None,
        }
    }

    /// A MIDI 1.0 message as a VST3 event. A note, key pressure, controller,
    /// program, channel pressure or bend is read at MIDI 1.0 resolution and
    /// carried as it came; a note or key pressure is widened to the full width
    /// VST3's `f32` takes.
    fn voice1(&self, offset: i32, channel: MidiChannel, voice: Voice1) -> Option<Event> {
        match voice {
            Voice1::NoteOn { .. } | Voice1::NoteOff { .. } | Voice1::PolyPressure { .. } => {
                // Widened here, so a MIDI 1.0 Note On at velocity 0 is the Note
                // Off of M2-104 App. D.3.1, which `voice2` encodes. `encode`
                // handles a MIDI 2.0 velocity of 0 itself.
                self.voice2(offset, channel, widen(voice))
            }
            Voice1::ControlChange { .. }
            | Voice1::ProgramChange(_)
            | Voice1::ChannelPressure(_)
            | Voice1::PitchBend(_) => Some(midi1_data(offset, voice.bytes(channel))),
        }
    }
}

/// The three messages a MIDI 2.0 Program Change with a valid bank stands for
/// (M2-104 App. D): Bank Select MSB (CC 0), Bank Select LSB (CC 32), then the
/// Program Change. `None` for anything else.
pub(super) fn bank_program(ump: &Ump) -> Option<[Midi1Bytes; 3]> {
    let message = decode_ump(*ump).ok().filter(|message| {
        matches!(
            message,
            Message::Midi2(_, Voice2::ProgramChange { bank: Some(_), .. })
        )
    })?;
    let mut messages = midi1(message);
    Some([messages.next()?, messages.next()?, messages.next()?])
}

fn note_off(offset: i32, channel: MidiChannel, note: NoteNumber, velocity: u16) -> Event {
    Event::at(
        offset,
        Payload::NoteOff(NoteOff::new(
            channel.get().into(),
            note.get().into(),
            u16_to_unit_f32(velocity),
            note_id_for(channel, note),
        )),
    )
}

/// One MIDI-1 message as the Data event that carries it: its bytes padded to
/// three.
pub(super) fn midi1_data(sample_offset: i32, bytes: Midi1Bytes) -> Event {
    Event::at(
        sample_offset,
        Payload::Data(Data::inline(0, bytes.padded())),
    )
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

/// The same for a registered per-note controller. Volume, pan and brightness
/// (CC74, sound controller #5) have a VST3 counterpart; the rest are dropped.
fn registered_controller_expression(
    controller: RegisteredPerNoteController,
) -> Option<NoteExpressionType> {
    match controller {
        RegisteredPerNoteController::Volume => Some(NoteExpressionType::Volume),
        RegisteredPerNoteController::Pan => Some(NoteExpressionType::Pan),
        RegisteredPerNoteController::Brightness => Some(NoteExpressionType::Brightness),
        RegisteredPerNoteController::Modulation
        | RegisteredPerNoteController::Breath
        | RegisteredPerNoteController::Pitch
        | RegisteredPerNoteController::Balance
        | RegisteredPerNoteController::Expression
        | RegisteredPerNoteController::SoundVariation
        | RegisteredPerNoteController::Timbre
        | RegisteredPerNoteController::ReleaseTime
        | RegisteredPerNoteController::AttackTime
        | RegisteredPerNoteController::DecayTime
        | RegisteredPerNoteController::VibratoRate
        | RegisteredPerNoteController::VibratoDepth
        | RegisteredPerNoteController::VibratoDelay
        | RegisteredPerNoteController::SoundController10
        | RegisteredPerNoteController::ReverbSend
        | RegisteredPerNoteController::Effects2Depth
        | RegisteredPerNoteController::ChorusSend
        | RegisteredPerNoteController::Effects4Depth
        | RegisteredPerNoteController::Effects5Depth
        | RegisteredPerNoteController::Reserved(_) => None,
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

/// A VST3 `i16` pitch field as a note number (`None` is never taken: it is
/// masked to 7 bits).
fn note_number(raw: i16) -> Option<NoteNumber> {
    NoteNumber::new(midi_note(raw))
}

/// Group 1 and the VST3 channel field, the address of a decoded event.
fn address(channel: i16) -> Address {
    Address {
        group: MidiGroup::FIRST,
        channel: MidiChannel::masked(midi_channel(channel)),
    }
}

/// See [`EventExt::to_midi`](super::EventExt::to_midi).
pub(super) fn decode(event: &Event) -> Option<WireMidi> {
    let frame = event.sample_offset().max(0).cast_unsigned();
    // Notes and poly-pressure build native MIDI-2 Channel Voice events so the
    // plugin's f32 velocity and pressure keep their full 16 and 32-bit width.
    let built = match &event.payload {
        Payload::NoteOn(e) => Ump::from(Message::Midi2(
            address(e.channel),
            Voice2::NoteOn {
                note: note_number(e.pitch)?,
                velocity: unit_f32_to_u16(e.velocity),
                attribute: Attribute::None,
            },
        )),
        Payload::NoteOff(e) => Ump::from(Message::Midi2(
            address(e.channel),
            Voice2::NoteOff {
                note: note_number(e.pitch)?,
                velocity: unit_f32_to_u16(e.velocity),
                attribute: Attribute::None,
            },
        )),
        Payload::PolyPressure(e) => Ump::from(Message::Midi2(
            address(e.channel),
            Voice2::PolyPressure {
                note: note_number(e.pitch)?,
                value: unit_f32_to_u32(e.pressure),
            },
        )),
        Payload::Data(d) => {
            let bytes = d.payload();
            if bytes.len() < 2 {
                return None;
            }
            // The 0xF0 framing is the reliable SysEx signal: the kMidiSysEx subtype is 0,
            // as is the raw channel-voice fallback's. Strip the delimiters and rebuild a
            // single packet; a message too long for one packet is dropped, not truncated,
            // and so is one with a payload byte of 128 or more (malformed).
            if bytes[0] == 0xF0 {
                let mut packets = sysex_packets(bytes)?;
                if packets.len() != 1 {
                    return None;
                }
                return packets.next().map(|ump| WireMidi { offset: frame, ump });
            }
            // Widened to Channel Voice 2 here. System messages pass through unchanged.
            let message = Midi1Parser::message(MidiGroup::FIRST, bytes)?;
            return Some(WireMidi {
                offset: frame,
                ump: Ump::from(widen_message(message)),
            });
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
    Some(WireMidi {
        offset: frame,
        ump: built,
    })
}
