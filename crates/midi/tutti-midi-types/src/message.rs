//! [`MidiMessage`] — the app-facing decoded view of a [`MidiEvent`].
//!
//! Modeled on `std::net::IpAddr`: a [`MidiEvent`] is the wire form (a UMP packet
//! that may be MIDI 1.0 or 2.0 on the wire), and [`MidiMessage`] is the "just
//! tell me what it is" view an application matches on — the way `IpAddr` unifies
//! `Ipv4Addr`/`Ipv6Addr`. Like `IpAddr`, it carries the **common accessors**
//! ([`MidiMessage::note`], [`MidiMessage::velocity`], [`MidiMessage::channel`])
//! directly, so most code never has to `match` at all.
//!
//! Values are the MIDI 2.0 spec widths verbatim — 16-bit velocity, 32-bit
//! controllers/bend, per-note [`NoteId`] identity. No lossy narrowing happens
//! here; convert to `f32` at your DSP edge if you need to. Decode with
//! [`MidiEvent::message`]; it first runs [`normalize`](crate::normalize()), so a MIDI 1.0
//! event is already promoted to its MIDI 2.0 form before you see it.
//!
//! For message families this enum does not model (system real-time, SysEx, Flex
//! Data, UMP Stream, …) [`MidiMessage::Other`] is returned; reach for
//! `event.data_words()` + `midi2` when you need those.

use midi2::channel_voice2::ChannelVoice2 as Cv2;
use midi2::{Channeled, UmpMessage};

use crate::note_id::NoteId;
use crate::ump::MidiEvent;

/// Which per-note controller a [`MidiMessage::PerNoteController`] carries.
/// Registered controllers name a spec-defined function; assignable ones carry a
/// raw index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PerNoteController {
    /// A Registered Per-Note Controller identified by its spec bank/index.
    Registered { index: u8 },
    /// An Assignable Per-Note Controller identified by its raw index.
    Assignable { index: u8 },
}

/// A MIDI 2.0 note-on attribute (M2-104 §7.4.2): extra per-note data carried
/// alongside the note. tutti's re-export of `midi2`'s attribute so consumers
/// don't import `midi2` to preserve it across a decode/re-encode round-trip.
pub type NoteAttribute = midi2::channel_voice2::NoteAttribute;

/// A decoded MIDI 2.0 message — tutti's application-facing view of a
/// [`MidiEvent`]. See the [module docs](self). `#[non_exhaustive]` so added
/// message families never break an existing `match`.
///
/// Every variant carries `frame_offset` (the source event's sample-accurate
/// timing) so a decode → re-encode round-trip preserves timing; note variants
/// carry the note `attribute`. Unmodeled families keep their whole source event
/// in [`Other`](Self::Other), so [`MidiEvent::try_from`] can reconstruct any
/// message this view produced.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum MidiMessage {
    /// Note on. `velocity` is the full 16-bit MIDI 2.0 value.
    NoteOn {
        frame_offset: u32,
        id: NoteId,
        channel: u8,
        note: u8,
        velocity: u16,
        attribute: Option<NoteAttribute>,
    },
    /// Note off (also a MIDI 1.0 velocity-0 note-on, folded by `normalize`).
    NoteOff {
        frame_offset: u32,
        id: NoteId,
        channel: u8,
        note: u8,
        velocity: u16,
        attribute: Option<NoteAttribute>,
    },
    /// Polyphonic key pressure (per-note aftertouch). `pressure` is 32-bit.
    PolyPressure {
        frame_offset: u32,
        id: NoteId,
        channel: u8,
        note: u8,
        pressure: u32,
    },
    /// Control change. `value` is the full 32-bit MIDI 2.0 value.
    ControlChange {
        frame_offset: u32,
        channel: u8,
        index: u8,
        value: u32,
    },
    /// Program change, with an optional bank (MSB<<7 | LSB) when present.
    ProgramChange {
        frame_offset: u32,
        channel: u8,
        program: u8,
        bank: Option<u16>,
    },
    /// Channel (mono) aftertouch. `pressure` is 32-bit.
    ChannelPressure {
        frame_offset: u32,
        channel: u8,
        pressure: u32,
    },
    /// Channel pitch bend. `value` is 32-bit, bipolar around `0x8000_0000`.
    PitchBend {
        frame_offset: u32,
        channel: u8,
        value: u32,
    },
    /// Per-note pitch bend (MIDI 2.0). Addresses one voice by `id`.
    PerNotePitchBend {
        frame_offset: u32,
        id: NoteId,
        channel: u8,
        note: u8,
        value: u32,
    },
    /// Per-note controller (MIDI 2.0). `value` is 32-bit.
    PerNoteController {
        frame_offset: u32,
        id: NoteId,
        channel: u8,
        note: u8,
        controller: PerNoteController,
        value: u32,
    },
    /// Per-Note Management (MIDI 2.0): detach / reset the addressed note's
    /// controllers.
    PerNoteManagement {
        frame_offset: u32,
        id: NoteId,
        channel: u8,
        note: u8,
        detach: bool,
        reset: bool,
    },
    /// Any message family this view does not model (system, SysEx, Flex Data,
    /// UMP Stream, utility) — carries the whole source [`MidiEvent`] so it is
    /// never information-free. Inspect it via `event.data_words()` + `midi2`.
    Other(MidiEvent),
}

impl MidiMessage {
    /// Note number, for note / poly-pressure / per-note messages.
    #[inline]
    pub fn note(&self) -> Option<u8> {
        match self {
            Self::NoteOn { note, .. }
            | Self::NoteOff { note, .. }
            | Self::PolyPressure { note, .. }
            | Self::PerNotePitchBend { note, .. }
            | Self::PerNoteController { note, .. }
            | Self::PerNoteManagement { note, .. } => Some(*note),
            _ => None,
        }
    }

    /// Per-note identity, for note / poly-pressure / per-note messages. Two
    /// simultaneous same-pitch notes have distinct ids.
    #[inline]
    pub fn id(&self) -> Option<NoteId> {
        match self {
            Self::NoteOn { id, .. }
            | Self::NoteOff { id, .. }
            | Self::PolyPressure { id, .. }
            | Self::PerNotePitchBend { id, .. }
            | Self::PerNoteController { id, .. }
            | Self::PerNoteManagement { id, .. } => Some(*id),
            _ => None,
        }
    }

    /// Channel (0-15) for any channel-voice message. `None` for [`Other`].
    ///
    /// [`Other`]: Self::Other
    #[inline]
    pub fn channel(&self) -> Option<u8> {
        match self {
            Self::NoteOn { channel, .. }
            | Self::NoteOff { channel, .. }
            | Self::PolyPressure { channel, .. }
            | Self::ControlChange { channel, .. }
            | Self::ProgramChange { channel, .. }
            | Self::ChannelPressure { channel, .. }
            | Self::PitchBend { channel, .. }
            | Self::PerNotePitchBend { channel, .. }
            | Self::PerNoteController { channel, .. }
            | Self::PerNoteManagement { channel, .. } => Some(*channel),
            Self::Other(_) => None,
        }
    }

    /// Velocity (full 16-bit) for note-on / note-off. `None` otherwise.
    #[inline]
    pub fn velocity(&self) -> Option<u16> {
        match self {
            Self::NoteOn { velocity, .. } | Self::NoteOff { velocity, .. } => Some(*velocity),
            _ => None,
        }
    }

    /// Sample-accurate offset within the current audio block, for every message.
    #[inline]
    pub fn frame_offset(&self) -> u32 {
        match self {
            Self::NoteOn { frame_offset, .. }
            | Self::NoteOff { frame_offset, .. }
            | Self::PolyPressure { frame_offset, .. }
            | Self::ControlChange { frame_offset, .. }
            | Self::ProgramChange { frame_offset, .. }
            | Self::ChannelPressure { frame_offset, .. }
            | Self::PitchBend { frame_offset, .. }
            | Self::PerNotePitchBend { frame_offset, .. }
            | Self::PerNoteController { frame_offset, .. }
            | Self::PerNoteManagement { frame_offset, .. } => *frame_offset,
            Self::Other(ev) => ev.frame_offset,
        }
    }

    /// `true` for a note-on with non-zero velocity.
    #[inline]
    pub fn is_note_on(&self) -> bool {
        matches!(self, Self::NoteOn { velocity, .. } if *velocity > 0)
    }

    /// `true` for a note-off (including a folded velocity-0 note-on).
    #[inline]
    pub fn is_note_off(&self) -> bool {
        matches!(self, Self::NoteOff { .. })
            || matches!(self, Self::NoteOn { velocity: 0, .. })
    }
}

impl MidiEvent {
    /// Decode into the app-facing [`MidiMessage`] view. Runs [`normalize`](crate::normalize())
    /// first, so a MIDI 1.0 channel-voice event arrives already promoted to its
    /// MIDI 2.0 form. Anything this view doesn't model yields
    /// [`MidiMessage::Other`] — use [`data_words`](Self::data_words) + `midi2`
    /// for those.
    pub fn message(&self) -> MidiMessage {
        let ev = crate::normalize(self);
        let frame_offset = ev.frame_offset;
        let Ok(UmpMessage::ChannelVoice2(cv2)) = UmpMessage::try_from(ev.data_words()) else {
            // Preserve the *original* event verbatim so a re-encode is exact.
            return MidiMessage::Other(*self);
        };
        let channel = u8::from(cv2.channel());
        let note_id = |note: u8| NoteId::from_channel_note(channel, note);

        match cv2 {
            Cv2::NoteOn(m) => {
                let note = u8::from(m.note_number());
                MidiMessage::NoteOn {
                    frame_offset,
                    id: note_id(note),
                    channel,
                    note,
                    velocity: m.velocity(),
                    attribute: m.attribute(),
                }
            }
            Cv2::NoteOff(m) => {
                let note = u8::from(m.note_number());
                MidiMessage::NoteOff {
                    frame_offset,
                    id: note_id(note),
                    channel,
                    note,
                    velocity: m.velocity(),
                    attribute: m.attribute(),
                }
            }
            Cv2::KeyPressure(m) => {
                let note = u8::from(m.note_number());
                MidiMessage::PolyPressure {
                    frame_offset,
                    id: note_id(note),
                    channel,
                    note,
                    pressure: m.key_pressure_data(),
                }
            }
            Cv2::ControlChange(m) => MidiMessage::ControlChange {
                frame_offset,
                channel,
                index: u8::from(m.control()),
                value: m.control_change_data(),
            },
            Cv2::ProgramChange(m) => MidiMessage::ProgramChange {
                frame_offset,
                channel,
                program: u8::from(m.program()),
                bank: m.bank().map(u16::from),
            },
            Cv2::ChannelPressure(m) => MidiMessage::ChannelPressure {
                frame_offset,
                channel,
                pressure: m.channel_pressure_data(),
            },
            Cv2::ChannelPitchBend(m) => MidiMessage::PitchBend {
                frame_offset,
                channel,
                value: m.pitch_bend_data(),
            },
            Cv2::PerNotePitchBend(m) => {
                let note = u8::from(m.note_number());
                MidiMessage::PerNotePitchBend {
                    frame_offset,
                    id: note_id(note),
                    channel,
                    note,
                    value: m.pitch_bend_data(),
                }
            }
            Cv2::RegisteredPerNoteController(m) => {
                let note = u8::from(m.note_number());
                let (index, value) = controller_index_and_data(m.controller());
                MidiMessage::PerNoteController {
                    frame_offset,
                    id: note_id(note),
                    channel,
                    note,
                    controller: PerNoteController::Registered { index },
                    value,
                }
            }
            Cv2::AssignablePerNoteController(m) => {
                let note = u8::from(m.note_number());
                MidiMessage::PerNoteController {
                    frame_offset,
                    id: note_id(note),
                    channel,
                    note,
                    controller: PerNoteController::Assignable { index: m.index() },
                    value: m.controller_data(),
                }
            }
            Cv2::PerNoteManagement(m) => {
                let note = u8::from(m.note_number());
                MidiMessage::PerNoteManagement {
                    frame_offset,
                    id: note_id(note),
                    channel,
                    note,
                    detach: m.detach(),
                    reset: m.reset(),
                }
            }
            _ => MidiMessage::Other(*self),
        }
    }
}

/// A [`MidiMessage`] variant that this engine cannot re-encode to a
/// [`MidiEvent`]. Every variant `MidiEvent::message` produces *can* be
/// re-encoded, so this only arises for hand-built messages using a per-note
/// controller index or program bank outside the encodable range.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UnencodableMessage;

impl core::fmt::Display for UnencodableMessage {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("MidiMessage cannot be encoded to a MidiEvent")
    }
}

impl std::error::Error for UnencodableMessage {}

impl TryFrom<MidiMessage> for MidiEvent {
    type Error = UnencodableMessage;

    /// Re-encode a [`MidiMessage`] back to its wire [`MidiEvent`], preserving the
    /// frame offset (and note attribute where present). Every message produced by
    /// [`MidiEvent::message`] round-trips; the error case only occurs for
    /// hand-built controller/bank values with no MIDI-2 encoding.
    fn try_from(msg: MidiMessage) -> Result<Self, Self::Error> {
        let ev = match msg {
            MidiMessage::NoteOn {
                channel,
                note,
                velocity,
                attribute,
                ..
            } => note_with_attribute(true, channel, note, velocity, attribute),
            MidiMessage::NoteOff {
                channel,
                note,
                velocity,
                attribute,
                ..
            } => note_with_attribute(false, channel, note, velocity, attribute),
            MidiMessage::PolyPressure {
                channel,
                note,
                pressure,
                ..
            } => MidiEvent::poly_pressure(0, channel, note, pressure),
            MidiMessage::ControlChange {
                channel,
                index,
                value,
                ..
            } => MidiEvent::cc(0, channel, index, value),
            MidiMessage::ProgramChange {
                channel,
                program,
                bank,
                ..
            } => MidiEvent::program_change(0, channel, program, bank),
            MidiMessage::ChannelPressure {
                channel, pressure, ..
            } => MidiEvent::channel_pressure(0, channel, pressure),
            MidiMessage::PitchBend { channel, value, .. } => {
                MidiEvent::pitch_bend(0, channel, value)
            }
            MidiMessage::PerNotePitchBend {
                channel,
                note,
                value,
                ..
            } => MidiEvent::per_note_pitch_bend(0, channel, note, value),
            MidiMessage::PerNoteController {
                channel,
                note,
                controller,
                value,
                ..
            } => {
                let (index, registered) = match controller {
                    PerNoteController::Registered { index } => (index, true),
                    PerNoteController::Assignable { index } => (index, false),
                };
                MidiEvent::per_note_controller(0, channel, note, index, value, registered)
            }
            MidiMessage::PerNoteManagement {
                channel,
                note,
                detach,
                reset,
                ..
            } => MidiEvent::per_note_management(0, channel, note, detach, reset),
            // The source event was preserved verbatim.
            MidiMessage::Other(ev) => return Ok(ev),
        };
        Ok(ev.with_frame_offset(msg.frame_offset()))
    }
}

/// Build a note-on/off [`MidiEvent`], re-applying a note `attribute` if present.
fn note_with_attribute(
    on: bool,
    channel: u8,
    note: u8,
    velocity: u16,
    attribute: Option<NoteAttribute>,
) -> MidiEvent {
    use midi2::channel_voice2::{NoteOff, NoteOn};
    use midi2::prelude::*;
    let mut words = [0u32; 4];
    if on {
        let mut m = NoteOn::<[u32; 2]>::new();
        m.set_channel(u4::new(channel & 0x0F));
        m.set_note_number(u7::new(note & 0x7F));
        m.set_velocity(velocity);
        if let Some(attr) = attribute {
            m.set_attribute(Some(attr));
        }
        words[..2].copy_from_slice(m.data());
    } else {
        let mut m = NoteOff::<[u32; 2]>::new();
        m.set_channel(u4::new(channel & 0x0F));
        m.set_note_number(u7::new(note & 0x7F));
        m.set_velocity(velocity);
        if let Some(attr) = attribute {
            m.set_attribute(Some(attr));
        }
        words[..2].copy_from_slice(m.data());
    }
    MidiEvent::from_ump(0, &words[..2])
}

/// Split a registered per-note `Controller` into its spec index and 32-bit data
/// (M2-104 §7.4.10). Mirrors midi2's own index assignment.
fn controller_index_and_data(c: midi2::channel_voice2::Controller) -> (u8, u32) {
    use midi2::channel_voice2::Controller;
    match c {
        Controller::Modulation(d) => (1, d),
        Controller::Breath(d) => (2, d),
        Controller::Pitch7_25(v) => (3, v.to_bits()),
        Controller::Volume(d) => (7, d),
        Controller::Balance(d) => (8, d),
        Controller::Pan(d) => (10, d),
        Controller::Expression(d) => (11, d),
        Controller::SoundVariation(d) => (70, d),
        Controller::Timbre(d) => (71, d),
        Controller::ReleaseTime(d) => (72, d),
        Controller::AttackTime(d) => (73, d),
        Controller::Brightness(d) => (74, d),
        Controller::DecayTime(d) => (75, d),
        Controller::VebratoRate(d) => (76, d),
        Controller::VebratoDepth(d) => (77, d),
        Controller::VebratoDelay(d) => (78, d),
        Controller::ReverbSendLevel(d) => (91, d),
        Controller::ChorusSendLevel(d) => (93, d),
        Controller::SoundController { index, data } => (69 + index, data),
        Controller::EffectDepth { index, data } => (90 + index, data),
        Controller::Undefined(d) => (0, d),
        // `Controller` is #[non_exhaustive]; a future variant surfaces as index 0.
        _ => (0, 0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn note_on_decodes_full_width() {
        let msg = MidiEvent::note_on(0, 3, 60, 0xC000).message();
        assert!(msg.is_note_on());
        assert_eq!(msg.note(), Some(60));
        assert_eq!(msg.channel(), Some(3));
        assert_eq!(msg.velocity(), Some(0xC000)); // full 16-bit, not narrowed
    }

    #[test]
    fn midi1_note_on_promotes_and_decodes() {
        // A MIDI 1.0 wire note-on decodes the same way — protocol is invisible here.
        let ev = MidiEvent::from_midi1_bytes(0, &[0x93, 60, 100]).unwrap();
        let msg = ev.message();
        assert!(msg.is_note_on());
        assert_eq!(msg.channel(), Some(3));
        assert_eq!(msg.note(), Some(60));
    }

    #[test]
    fn velocity_zero_note_on_reads_as_note_off() {
        let ev = MidiEvent::from_midi1_bytes(0, &[0x90, 60, 0]).unwrap();
        let msg = ev.message();
        assert!(msg.is_note_off());
        assert!(!msg.is_note_on());
    }

    #[test]
    fn control_change_carries_32bit_value() {
        let msg = MidiEvent::cc(0, 5, 74, 0xDEAD_BEEF).message();
        match msg {
            MidiMessage::ControlChange { channel, index, value, .. } => {
                assert_eq!(channel, 5);
                assert_eq!(index, 74);
                assert_eq!(value, 0xDEAD_BEEF);
            }
            other => panic!("expected ControlChange, got {other:?}"),
        }
    }

    #[test]
    fn same_pitch_different_channel_have_distinct_ids() {
        let a = MidiEvent::note_on(0, 1, 60, 0x8000).message().id().unwrap();
        let b = MidiEvent::note_on(0, 2, 60, 0x8000).message().id().unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn per_note_pitch_bend_addresses_one_note() {
        let msg = MidiEvent::per_note_pitch_bend(0, 0, 64, 0x9000_0000).message();
        match msg {
            MidiMessage::PerNotePitchBend { note, value, .. } => {
                assert_eq!(note, 64);
                assert_eq!(value, 0x9000_0000);
            }
            other => panic!("expected PerNotePitchBend, got {other:?}"),
        }
    }

    #[test]
    fn system_message_is_other_carrying_its_event() {
        let clock = MidiEvent::timing_clock(0);
        let msg = clock.message();
        assert_eq!(msg, MidiMessage::Other(clock));
        assert_eq!(msg.channel(), None);
    }

    #[test]
    fn round_trip_preserves_frame_offset() {
        // The view must not drop sample-accurate timing (project round-trip invariant).
        let ev = MidiEvent::cc(0, 5, 74, 0xABCD_1234).with_frame_offset(137);
        let msg = ev.message();
        assert_eq!(msg.frame_offset(), 137);
        let back = MidiEvent::try_from(msg).expect("re-encodable");
        assert_eq!(back, ev);
    }

    #[test]
    fn round_trip_preserves_note_attribute() {
        use midi2::channel_voice2::NoteAttribute;
        use midi2::num::Fixed7_9;
        // A note-on carrying a Pitch7_9 attribute must survive decode → re-encode.
        let mut on = midi2::channel_voice2::NoteOn::<[u32; 2]>::new();
        {
            use midi2::prelude::*;
            on.set_channel(u4::new(3));
            on.set_note_number(u7::new(60));
            on.set_velocity(0x8000);
            on.set_attribute(Some(NoteAttribute::Pitch7_9(Fixed7_9::from_bits(0x1234))));
        }
        let ev = MidiEvent::from_ump(0, {
            use midi2::Data;
            on.data()
        });
        let msg = ev.message();
        match msg {
            MidiMessage::NoteOn { attribute, .. } => {
                assert_eq!(
                    attribute,
                    Some(NoteAttribute::Pitch7_9(Fixed7_9::from_bits(0x1234)))
                );
            }
            other => panic!("expected NoteOn, got {other:?}"),
        }
        let back = MidiEvent::try_from(msg).expect("re-encodable");
        assert_eq!(back, ev);
    }

    #[test]
    fn round_trip_other_is_exact() {
        // An unmodeled message re-encodes byte-for-byte via the preserved event.
        let ev = MidiEvent::timing_clock(0).with_frame_offset(42);
        let back = MidiEvent::try_from(ev.message()).expect("Other round-trips");
        assert_eq!(back, ev);
    }
}
