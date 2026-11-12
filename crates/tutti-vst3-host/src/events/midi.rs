//! A UMP event to VST3 event and back.

use tutti_graph::UmpAt;
use tutti_midi_protocol::{
    Narrower, PitchBendSensitivity, u16_to_unit_f32, u32_to_unit_f32, unit_f32_to_u16,
    unit_f32_to_u32, widen, widen_bytes,
};
use tutti_plugin_types::{NoteExpressionType, NoteExpressionValue, WireMidi};
use tutti_ump::{
    Address, CCNumber, Message, Midi1Bytes, MidiChannel, MidiGroup, NoteId, NoteNumber, Rpn,
    SysexMessage, Ump, Voice1, Voice2,
};
use tutti_voicing::{LIVE_NOTES, NOTE_CONTROLS, NoteControl, NoteEvent, PerNoteState};

use super::{
    Data, Event, K_DATA_TYPE_MIDI_SYSEX, NoteOff, NoteOn, Payload, PolyPressure, ToEvent,
    sample_offset,
};

/// VST3's tuning expression for `semitones` from the note's key: `0.5 +
/// semitones / 240` (`kTuningTypeID` spans -120 to +120 semitones over
/// `[0, 1]`).
fn tuning_expression(semitones: f64) -> f64 {
    0.5 + semitones / 240.0
}

/// The most events one packet makes: a sensitivity change re-sends a bend for
/// each live note of every key of a channel, a Reset puts five values back for
/// each live note of one key, and a Note On is itself, the values its key held
/// and an attribute.
const MAX_EVENTS: usize = 128 * LIVE_NOTES + 5 * LIVE_NOTES + NOTE_CONTROLS + 16;

/// What a Per-Note Management Reset puts each expression of a note back to,
/// as VST3's normalized values: no tuning, unity volume, centred pan and
/// brightness. (Pressure is a `PolyPressure` of 0.)
const EXPRESSION_DEFAULTS: [(NoteExpressionType, f64); 4] = [
    (NoteExpressionType::Tuning, 0.5),
    (NoteExpressionType::Volume, 1.0),
    (NoteExpressionType::Pan, 0.5),
    (NoteExpressionType::Brightness, 0.5),
];

/// Encodes UMP events (`(Offset, Ump)`) as VST3 [`Event`]s, keeping the stream's
/// state across blocks: the per-note state of M2-104 section 7.4
/// ([`PerNoteState`]: each Group, channel and key's held values, the notes that
/// respond to them, each channel's bend sensitivity) and the MIDI 2.0 to MIDI
/// 1.0 state of the raw bytes sent ([`Narrower`]).
///
/// **A Group is an event bus**: its events carry `busIndex` of the Group, and
/// a Group the plugin has no event input bus for is dropped and counted
/// ([`groups_dropped`](Self::groups_dropped)), never folded onto another
/// Group's channels (M2-104 section 2.1.2).
#[derive(Debug)]
pub struct MidiEncoder {
    notes: PerNoteState,
    narrower: Narrower,
    /// The plugin's event input buses: a Group is the bus of its number.
    buses: usize,
    groups_dropped: u64,
    /// What the last [`encode`](Self::encode) made, reserved for the most one
    /// packet can.
    out: Vec<Event>,
}

impl Default for MidiEncoder {
    /// An encoder for a plugin with one event input bus.
    fn default() -> Self {
        Self::new(1)
    }
}

impl MidiEncoder {
    /// An encoder for a plugin with `buses` event input buses (at most 16, the
    /// Groups of MIDI 2.0; none is one: a plugin with no event bus is sent
    /// Group 0's events, as before). Every channel's per-note bend
    /// sensitivity starts at the MPE default, ±48 (RP-053). Allocates (about
    /// 270 KiB for each bus): off the audio thread.
    #[must_use]
    pub fn new(buses: usize) -> Self {
        let buses = buses.clamp(1, usize::from(MidiGroup::COUNT));
        Self {
            notes: PerNoteState::new(buses, PitchBendSensitivity::MPE_DEFAULT.semitones()),
            narrower: Narrower::new(),
            buses,
            groups_dropped: 0,
            out: Vec::with_capacity(MAX_EVENTS),
        }
    }

    /// Channel voice messages dropped for a Group the plugin has no event
    /// input bus for, since the encoder was made: each whole (a note, a
    /// controller, a per-note message alike). System Real Time and Utility
    /// messages name no Group's channels and are not counted.
    #[must_use]
    pub fn groups_dropped(&self) -> u64 {
        self.groups_dropped
    }

    /// The plugin forgot its notes and its MIDI 1.0 state (it was stopped or
    /// reset): drops what was held per note and forgets which RPN, NRPN and bank
    /// each channel has had selected, so the next controller selects in full.
    /// Never allocates.
    pub fn reset(&mut self) {
        self.notes.clear();
        self.narrower.reset();
    }

    /// The VST3 events `event` stands for, valid until the next call, at the
    /// stream state the encoder holds.
    ///
    /// The packet is read through `Ump::decode`. A MIDI 1.0 note or key
    /// pressure is widened ([`widen`]) and a MIDI 2.0 one is read as is, so
    /// velocity and pressure reach the plugin at their full bit width as `f32`
    /// rather than through 7-bit MIDI 1.
    ///
    /// **Notes and per-note controllers** are the encoder's [`PerNoteState`]:
    /// a Note On is a `NoteOn` with a note id of its own, then the values its
    /// Group, channel and key held before it as expression events (a pressure as
    /// a `PolyPressure`), then its Attribute Pitch; a per-note controller reaches
    /// every note of its key that has not been detached, and a Per-Note
    /// Management Detach or Reset ends or resets them. A Note Off names no id
    /// (VST3's `-1`): the plugin matches it by channel and pitch. A per-note
    /// bend, a Registered Per-Note Controller #3 Pitch and an Attribute Pitch
    /// are one tuning expression in semitones: the note's sounding pitch
    /// ([`PerNoteState`]'s sum of the told pitch and the bend over the
    /// channel's RPN #00/07 sensitivity, consumed: VST3 has no event for it)
    /// relative to the key, sent when it changes; a Reset puts it back to 0 and a
    /// note told nothing has none. A Note On at MIDI 2.0
    /// velocity 0 goes at the smallest velocity above 0, which VST3 plugins do
    /// not read as a release.
    ///
    /// **Every other message with a MIDI 1.0 form** (controllers, channel pitch
    /// bend and pressure, program change, an RPN or NRPN) is a `Data` event of
    /// MIDI 1.0 bytes through the [`Narrower`]: an RPN or NRPN is the Control
    /// Changes of M2-104 Appendix D.2.3, and a bank is sent only when it
    /// changes. A single-packet SysEx becomes a `Data` event too.
    ///
    /// The result is empty for a message this host is a sink for, arm by arm in
    /// the encoder: VST3's event list has no raw UMP event, so a message with
    /// no VST3 event and no MIDI 1.0 form, including an unknown packet, goes no
    /// further. A packet of a multi-packet SysEx is empty too: the list
    /// reassembles it. The several events of one packet are one unit (an RPN's
    /// select and data, a bank and its program): a caller that cannot stage
    /// them all drops them all and tells the encoder
    /// ([`forget_selection`](Self::forget_selection)).
    pub fn encode(&mut self, event: &UmpAt) -> &[Event] {
        let (at, ump) = event;
        self.out.clear();
        let offset = sample_offset(at.get());
        // A reserved or undefined packet has no VST3 event that carries raw
        // words, so there is no way to forward it: this host is a sink for it.
        if let Ok(message) = (*ump).decode() {
            self.message(offset, message);
        }
        &self.out
    }

    /// The caller dropped what [`encode`](Self::encode) made: the plugin never
    /// heard its RPN or bank select, so the next controller must select in full.
    pub fn forget_selection(&mut self) {
        self.narrower.reset();
    }

    fn message(&mut self, offset: i32, message: Message) {
        match message {
            Message::Midi1(address, voice) => match voice {
                // Widened here, so a MIDI 1.0 Note On at velocity 0 is the Note
                // Off of M2-104 App. D.3.1, which `voice2` encodes. `voice2`
                // handles a MIDI 2.0 velocity of 0 itself.
                Voice1::NoteOn { .. } | Voice1::NoteOff { .. } | Voice1::PolyPressure { .. } => {
                    self.voice2(offset, address, widen(voice));
                }
                Voice1::ControlChange { .. }
                | Voice1::ProgramChange(_)
                | Voice1::ChannelPressure(_)
                | Voice1::PitchBend(_) => self.bytes(offset, address, message),
            },
            Message::Midi2(address, voice) => self.voice2(offset, address, voice),
            Message::System(_, system) => self.out.push(midi1_data(offset, system.bytes())),
            // VST3 wants a SysEx with its 0xF0 ... 0xF7 delimiters. A
            // single-packet message fits the inline bytes (at most 6 payload,
            // 8 framed); a packet of a multi-packet message cannot stand
            // alone, and the list reassembles those.
            Message::Sysex7(_, packet) => {
                if let Some(data) = packet
                    .framed()
                    .and_then(|framed| Data::try_from_iter(K_DATA_TYPE_MIDI_SYSEX, framed))
                {
                    self.out.push(Event::at(offset, Payload::Data(data)));
                }
            }
            // Sinks: VST3 has no event for a clock, a stream message, flex
            // data or SysEx 8, and the Data event carries only MIDI 1.0 bytes.
            Message::Utility(_) | Message::Data128(..) | Message::Flex(..) | Message::Stream(_) => {
            }
        }
    }

    /// The event bus of `address`'s Group, or `None` (counted) when the plugin
    /// has none.
    fn bus_of(&mut self, address: Address) -> Option<i32> {
        let bus = address.group.index();
        if bus < self.buses {
            i32::try_from(bus).ok()
        } else {
            self.groups_dropped += 1;
            None
        }
    }

    /// `message` as `Data` events of MIDI 1.0 bytes on its Group's bus,
    /// through the narrower.
    fn bytes(&mut self, offset: i32, address: Address, message: Message) {
        let Some(bus) = self.bus_of(address) else {
            return;
        };
        let out = &mut self.out;
        self.narrower.feed(message, |bytes| {
            out.push(on_bus(midi1_data(offset, bytes), bus))
        });
    }

    fn voice2(&mut self, offset: i32, address: Address, voice: Voice2) {
        let sensitivity = matches!(
            voice,
            Voice2::Registered { bank, index, .. }
                if Rpn::new(bank, index) == Rpn::PER_NOTE_PITCH_BEND_SENSITIVITY
        );
        match voice {
            Voice2::NoteOn { .. }
            | Voice2::RegisteredPerNote { .. }
            | Voice2::AssignablePerNote { .. }
            | Voice2::PerNotePitchBend { .. }
            | Voice2::PolyPressure { .. }
            | Voice2::PerNoteManagement { .. } => self.per_note(offset, address, voice),
            Voice2::Registered { .. } if sensitivity => self.per_note(offset, address, voice),
            // The attribute of a Note Off is not read, and its release velocity
            // is kept at its full width. A Note Off names no note: VST3's `-1`,
            // matched by channel and pitch, as MIDI's is.
            Voice2::NoteOff { note, velocity, .. } => {
                if let Some(bus) = self.bus_of(address) {
                    let off = note_off(offset, address.channel, note, velocity);
                    self.out.push(on_bus(off, bus));
                }
            }
            // CC, channel pressure, channel pitch bend and program change have no
            // extra resolution VST3 can use: they ride the plugin's parameter
            // funnel (`CcRoute`) or land as raw Data events, as do an RPN or an
            // NRPN (the per-note bend sensitivity is consumed by `per_note`).
            // A Program Change with a bank is three messages. There is no event
            // for a relative controller; the narrower sends nothing for it.
            Voice2::ControlChange { .. }
            | Voice2::ChannelPressure(_)
            | Voice2::PitchBend(_)
            | Voice2::ProgramChange { .. }
            | Voice2::Registered { .. }
            | Voice2::Assignable { .. }
            | Voice2::RelativeRegistered { .. }
            | Voice2::RelativeAssignable { .. } => {
                self.bytes(offset, address, Message::Midi2(address, voice));
            }
        }
    }

    /// A message the per-note state reads: a Note On, a per-note controller,
    /// Per-Note Management or the bend sensitivity.
    fn per_note(&mut self, offset: i32, address: Address, voice: Voice2) {
        let Some(bus) = self.bus_of(address) else {
            return;
        };
        let channel = address.channel;
        let (note, velocity) = match voice {
            Voice2::NoteOn { note, velocity, .. } => (Some(note), Some(velocity)),
            Voice2::RegisteredPerNote { note, .. }
            | Voice2::AssignablePerNote { note, .. }
            | Voice2::PerNotePitchBend { note, .. }
            | Voice2::PolyPressure { note, .. }
            | Voice2::PerNoteManagement { note, .. } => (Some(note), None),
            _ => (None, None),
        };
        for event in self.notes.step(address, &voice) {
            let made = match *event {
                NoteEvent::Started { id, note, .. } => {
                    // A MIDI 2.0 Note On at velocity 0 is a Note On (M2-104
                    // §7.4.2: velocity 0 "does not function as a Note Off"). VST3
                    // plugins, and the SDK's own MIDI conversion, read a NoteOnEvent
                    // at velocity 0 as a release, so it goes at the smallest
                    // velocity the field carries above 0: M2-104 D.2's rule for a
                    // note-on that cannot be 0 (velocity 0 -> 1). CLAP carries the
                    // 0. A widened MIDI 1.0 Note On at velocity 0 never gets here:
                    // it arrives as a Note Off.
                    let velocity = velocity.map_or(1, |v| v.max(1));
                    self.out.push(on_bus(
                        note_on(offset, channel, note, velocity, plugin_note_id(id)),
                        bus,
                    ));
                    continue;
                }
                // A Pitch is the note's sounding pitch, the told pitch with its
                // bend already in it: VST3's one tuning is its distance from the
                // key.
                NoteEvent::Control { id, note, control } => {
                    control_event(offset, channel, note, id, &control)
                }
                NoteEvent::Reset { id } => {
                    for (expression_type, value) in EXPRESSION_DEFAULTS {
                        let reset = NoteExpressionValue {
                            sample_offset: offset,
                            note_id: plugin_note_id(id),
                            expression_type,
                            value,
                        }
                        .to_event();
                        self.out.extend(reset.map(|e| on_bus(e, bus)));
                    }
                    if let Some(key) = note {
                        self.out
                            .push(on_bus(pressure_event(offset, channel, key, id, 0.0), bus));
                    }
                    continue;
                }
            };
            self.out.extend(made.map(|e| on_bus(e, bus)));
        }
    }
}

/// `event` on event bus `bus`.
fn on_bus(mut event: Event, bus: i32) -> Event {
    event.header.bus_index = bus;
    event
}

/// The note id a plugin is told for `id`. A minted id past `i32::MAX` (after two
/// billion notes) has no `i32` form and is VST3's `-1` "unspecified": the plugin
/// falls back to channel and pitch.
fn plugin_note_id(id: NoteId) -> i32 {
    i32::try_from(id.raw()).unwrap_or(-1)
}

/// A `PolyPressure` of the note `id` on `note`.
fn pressure_event(
    offset: i32,
    channel: MidiChannel,
    note: NoteNumber,
    id: NoteId,
    pressure: f32,
) -> Event {
    Event::at(
        offset,
        Payload::PolyPressure(PolyPressure::new(
            channel.get().into(),
            note.get().into(),
            pressure,
            plugin_note_id(id),
        )),
    )
}

/// The event a per-note controller is for the note `id`, or `None` when VST3
/// has no event for it. A pressure is a `PolyPressure` of `note`, a Pitch is a
/// tuning relative to `note`, and a Registered or Assignable controller with a
/// VST3 expression is that expression.
fn control_event(
    offset: i32,
    channel: MidiChannel,
    note: NoteNumber,
    id: NoteId,
    control: &NoteControl,
) -> Option<Event> {
    let expression = |expression_type, value| {
        NoteExpressionValue {
            sample_offset: offset,
            note_id: plugin_note_id(id),
            expression_type,
            value,
        }
        .to_event()
    };
    match *control {
        NoteControl::Pitch(pitch) => expression(
            NoteExpressionType::Tuning,
            tuning_expression(f64::from(pitch.note_number()) - f64::from(note.get())),
        ),
        NoteControl::Pressure(value) => Some(pressure_event(
            offset,
            channel,
            note,
            id,
            u32_to_unit_f32(value),
        )),
        NoteControl::Registered(controller, value) => expression(
            per_note_controller_expression(controller.number())?,
            f64::from(u32_to_unit_f32(value)),
        ),
        NoteControl::Assignable(index, value) => expression(
            per_note_controller_expression(index)?,
            f64::from(u32_to_unit_f32(value)),
        ),
    }
}

fn note_on(
    offset: i32,
    channel: MidiChannel,
    note: NoteNumber,
    velocity: u16,
    note_id: i32,
) -> Event {
    Event::at(
        offset,
        Payload::NoteOn(NoteOn::new(
            channel.get().into(),
            note.get().into(),
            u16_to_unit_f32(velocity),
            note_id,
        )),
    )
}

/// A note-off naming no note (`-1`).
fn note_off(offset: i32, channel: MidiChannel, note: NoteNumber, velocity: u16) -> Event {
    Event::at(
        offset,
        Payload::NoteOff(NoteOff::new(
            channel.get().into(),
            note.get().into(),
            u16_to_unit_f32(velocity),
            -1,
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

/// The per-note controllers with a VST3 note expression, and the expression
/// each is: the usual CC assignments, volume 7, pan 10, brightness 74 (CC74,
/// sound controller #5). The one table the assignable and the registered
/// per-note controllers share.
const PER_NOTE_EXPRESSIONS: [(CCNumber, NoteExpressionType); 3] = [
    (CCNumber::VOLUME, NoteExpressionType::Volume),
    (CCNumber::PAN, NoteExpressionType::Pan),
    (CCNumber::BRIGHTNESS, NoteExpressionType::Brightness),
];

/// The VST3 note-expression dimension a per-note controller number
/// corresponds to ([`PER_NOTE_EXPRESSIONS`]); the rest are dropped.
fn per_note_controller_expression(number: u8) -> Option<NoteExpressionType> {
    PER_NOTE_EXPRESSIONS
        .iter()
        .find(|(controller, _)| controller.get() == number)
        .map(|&(_, expression)| expression)
}

/// A VST3 `i16` channel field as a 4-bit MIDI channel.
#[allow(
    clippy::cast_possible_truncation,
    reason = "masked to 4 bits, so the narrowing cannot lose a bit the mask keeps"
)]
pub(super) fn midi_channel(raw: i16) -> MidiChannel {
    MidiChannel::masked(raw as u8)
}

/// A VST3 `i16` pitch field as a 7-bit note number.
#[allow(
    clippy::cast_possible_truncation,
    reason = "masked to 7 bits, so the narrowing cannot lose a bit the mask keeps"
)]
fn note_number(raw: i16) -> NoteNumber {
    NoteNumber::masked(raw as u8)
}

/// Group 1 and the VST3 channel field, the address of a decoded event.
fn address(channel: i16) -> Address {
    Address::first(midi_channel(channel))
}

/// See [`EventExt::to_midi`](super::EventExt::to_midi).
pub(super) fn decode(event: &Event) -> Option<WireMidi> {
    let frame = event.sample_offset().max(0).cast_unsigned();
    // Notes and poly-pressure build native MIDI-2 Channel Voice events so the
    // plugin's f32 velocity and pressure keep their full 16 and 32-bit width.
    let built = match &event.payload {
        Payload::NoteOn(e) => Ump::from(Message::Midi2(
            address(e.channel),
            Voice2::note_on(note_number(e.pitch), unit_f32_to_u16(e.velocity)),
        )),
        Payload::NoteOff(e) => Ump::from(Message::Midi2(
            address(e.channel),
            Voice2::note_off(note_number(e.pitch), unit_f32_to_u16(e.velocity)),
        )),
        Payload::PolyPressure(e) => Ump::from(Message::Midi2(
            address(e.channel),
            Voice2::PolyPressure {
                note: note_number(e.pitch),
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
                let mut packets = SysexMessage::new(bytes)?.packets(MidiGroup::FIRST);
                if packets.len() != 1 {
                    return None;
                }
                return packets.next().map(|ump| WireMidi { offset: frame, ump });
            }
            // Widened to Channel Voice 2 here. System messages pass through unchanged.
            return Some(WireMidi {
                offset: frame,
                ump: widen_bytes(MidiGroup::FIRST, bytes)?,
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
