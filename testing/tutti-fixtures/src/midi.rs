//! Test builders for single MIDI messages, as [`Ump`]s.
//!
//! Each builds a `tutti_ump::Message` and encodes it with `From<Message> for
//! Ump`, the one encoder; nothing here sets a bit by hand. They take raw
//! numbers where a test writes literals, and panic on a value the field type
//! refuses, so a test cannot build a message the wire could not carry.
//!
//! Production code builds the `Message` itself: these exist so a test reads
//! as the message it sends, not as the struct literal.

use tutti_midi_protocol::{MpeMode, MpeZone, MpeZoneConfig, MpeZones, widen};
use tutti_ump::{
    Address, Bank, CCNumber, Message, Midi1Parser, MidiChannel, MidiGroup, NoteNumber,
    RegisteredPerNoteController, Rpn, Sysex7Packets, System, U7, U14, Ump, Utility, Voice1, Voice2,
};

/// A MIDI 2.0 velocity at the midpoint of its 16-bit range: M2-115 section 3
/// widens a MIDI 1.0 velocity of 64 to `0x8000` (a velocity of 0 is `0`).
pub const MID_VELOCITY: u16 = 0x8000;

/// A receiver of the per-note bend sensitivity (RPN #00/07, M2-104 section
/// 7.4.13), for a test that checks what a stream tells one: each channel
/// starts at the MPE default, ±48 (RP-053), and [`observe`](Self::observe)
/// follows each message of the RPN. It reads the Group's channels as one
/// 16-channel space and ignores which Group a message is in.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BendRanges([f32; 16]);

impl Default for BendRanges {
    fn default() -> Self {
        Self([48.0; 16])
    }
}

impl BendRanges {
    /// The range on `channel`, in semitones.
    #[must_use]
    pub fn semitones(&self, channel: MidiChannel) -> f32 {
        self.0[usize::from(channel.get())]
    }

    /// Follows `ump`: a per-note bend sensitivity sets its channel's range, and
    /// `true` says it was one. Anything else leaves the ranges as they are.
    pub fn observe(&mut self, ump: &Ump) -> bool {
        let Ok(Message::Midi2(address, Voice2::Registered { bank, index, value })) =
            (*ump).decode()
        else {
            return false;
        };
        if Rpn::new(bank, index) != Rpn::PER_NOTE_PITCH_BEND_SENSITIVITY {
            return false;
        }
        self.0[usize::from(address.channel.get())] =
            tutti_midi_protocol::PitchBendSensitivity::from_rpn_bits(value).as_semitones_f32();
        true
    }
}

/// A MIDI 1.0 channel voice message in the first Group.
pub fn m1(channel: MidiChannel, v: Voice1) -> Ump {
    Message::Midi1(Address::first(channel), v).into()
}

/// A MIDI 2.0 channel voice message in the first Group.
pub fn m2(channel: MidiChannel, v: Voice2) -> Ump {
    voice(MidiGroup::FIRST, channel, v)
}

/// A MIDI 1.0 Note On in the first Group (a velocity of 0 is a note-off on
/// the wire, M1-4.2).
pub fn m1_note_on(channel: MidiChannel, note: NoteNumber, velocity: U7) -> Ump {
    m1(channel, Voice1::NoteOn { note, velocity })
}

/// A MIDI 1.0 Note Off in the first Group.
pub fn m1_note_off(channel: MidiChannel, note: NoteNumber, velocity: U7) -> Ump {
    m1(channel, Voice1::NoteOff { note, velocity })
}

/// A MIDI 1.0 Control Change in the first Group.
pub fn m1_cc(channel: MidiChannel, controller: CCNumber, value: U7) -> Ump {
    m1(channel, Voice1::ControlChange { controller, value })
}

/// The packet of the first Group that one MIDI 1.0 message's bytes make
/// (`Midi1Parser::message`: a status byte and its data, padding ignored), or
/// `None` for what it refuses.
pub fn from_midi1_bytes(bytes: &[u8]) -> Option<Ump> {
    Midi1Parser::message(MidiGroup::FIRST, bytes).map(Ump::from)
}

/// The packets of one SysEx7 message of `payload` (7-bit bytes, without
/// `F0`/`F7`) in the first Group. Panics on a byte with its top bit set or a
/// payload past `SYSEX7_MAX_PAYLOAD`.
pub fn sysex7_packets(payload: &[u8]) -> Vec<Ump> {
    Sysex7Packets::checked(MidiGroup::FIRST, payload)
        .expect("a payload of 7-bit bytes")
        .collect()
}

/// `len` 7-bit bytes, counting up from 1 and wrapping inside `0..=0x7F`, so
/// a payload of any length is valid SysEx data and neighbours differ.
pub fn sysex_payload(len: usize) -> Vec<u8> {
    (0..len).map(|i| ((i + 1) % 0x80) as u8).collect()
}

/// The `(channel, note, on)` of a Note On or Note Off packet of either
/// protocol, as its message type says (a MIDI 1.0 Note On with velocity 0 is
/// still `on` here: this reads the status, not the held-key meaning). `None`
/// for anything else.
pub fn key_of(ump: &Ump) -> Option<(MidiChannel, NoteNumber, bool)> {
    match (*ump).decode().ok()? {
        Message::Midi1(a, Voice1::NoteOn { note, .. })
        | Message::Midi2(a, Voice2::NoteOn { note, .. }) => Some((a.channel, note, true)),
        Message::Midi1(a, Voice1::NoteOff { note, .. })
        | Message::Midi2(a, Voice2::NoteOff { note, .. }) => Some((a.channel, note, false)),
        _ => None,
    }
}

fn voice(group: MidiGroup, channel: MidiChannel, v: Voice2) -> Ump {
    Message::Midi2(Address { group, channel }, v).into()
}

fn system(group: MidiGroup, s: System) -> Ump {
    Message::System(group, s).into()
}

/// A 7-bit velocity as the 16-bit value M2-115 section 3 gives it (a Note
/// Off's release velocity widens the same way, so `0` stays `0`).
pub fn wide_velocity(v: u8) -> u16 {
    let off = Voice1::NoteOff {
        note: NoteNumber::of(60),
        velocity: U7::of(v),
    };
    let Voice2::NoteOff { velocity, .. } = widen(off) else {
        unreachable!("a Note Off widens to a Note Off")
    };
    velocity
}

/// A 7-bit Control Change value as the 32-bit value M2-115 section 3 gives it.
pub fn wide_value(v: u8) -> u32 {
    let cc = Voice1::ControlChange {
        controller: CCNumber::MOD_WHEEL,
        value: U7::of(v),
    };
    let Voice2::ControlChange { value, .. } = widen(cc) else {
        unreachable!("a Control Change widens to a Control Change")
    };
    value
}

/// A 14-bit Pitch Bend as the 32-bit value M2-115 section 3 gives it.
pub fn wide_bend(v: u16) -> u32 {
    let Voice2::PitchBend(bend) = widen(Voice1::PitchBend(U14::of(v))) else {
        unreachable!("a Pitch Bend widens to a Pitch Bend")
    };
    bend
}

/// A MIDI 2.0 Note On, no attribute.
pub fn note_on(group: MidiGroup, channel: MidiChannel, n: u8, velocity: u16) -> Ump {
    voice(
        group,
        channel,
        Voice2::NoteOn {
            note: NoteNumber::of(n),
            velocity,
            attribute: tutti_ump::Attribute::None,
        },
    )
}

/// A MIDI 2.0 Note Off, no attribute.
pub fn note_off(group: MidiGroup, channel: MidiChannel, n: u8, velocity: u16) -> Ump {
    voice(
        group,
        channel,
        Voice2::NoteOff {
            note: NoteNumber::of(n),
            velocity,
            attribute: tutti_ump::Attribute::None,
        },
    )
}

/// A MIDI 2.0 Control Change.
pub fn cc(
    group: MidiGroup,
    channel: MidiChannel,
    controller: tutti_ump::CCNumber,
    value: u32,
) -> Ump {
    voice(group, channel, Voice2::ControlChange { controller, value })
}

/// A MIDI 2.0 Pitch Bend, `0x8000_0000` centred.
pub fn pitch_bend(group: MidiGroup, channel: MidiChannel, value: u32) -> Ump {
    voice(group, channel, Voice2::PitchBend(value))
}

/// A MIDI 2.0 Per-Note Pitch Bend.
pub fn per_note_pitch_bend(group: MidiGroup, channel: MidiChannel, n: u8, value: u32) -> Ump {
    voice(
        group,
        channel,
        Voice2::PerNotePitchBend {
            note: NoteNumber::of(n),
            value,
        },
    )
}

/// A MIDI 2.0 Program Change; `bank` is the 14-bit bank, MSB in bits 13-7.
pub fn program_change(
    group: MidiGroup,
    channel: MidiChannel,
    program: u8,
    bank: Option<u16>,
) -> Ump {
    let bank = bank.map(|b| Bank {
        msb: U7::of(((b >> 7) & 0x7F) as u8),
        lsb: U7::of((b & 0x7F) as u8),
    });
    voice(
        group,
        channel,
        Voice2::ProgramChange {
            program: U7::of(program),
            bank,
        },
    )
}

/// A MIDI 2.0 Channel Pressure.
pub fn channel_pressure(group: MidiGroup, channel: MidiChannel, value: u32) -> Ump {
    voice(group, channel, Voice2::ChannelPressure(value))
}

/// A MIDI 2.0 Poly Pressure.
pub fn poly_pressure(group: MidiGroup, channel: MidiChannel, n: u8, value: u32) -> Ump {
    voice(
        group,
        channel,
        Voice2::PolyPressure {
            note: NoteNumber::of(n),
            value,
        },
    )
}

/// A MIDI 2.0 Registered Controller (RPN).
pub fn registered_controller(
    group: MidiGroup,
    channel: MidiChannel,
    bank: u8,
    index: u8,
    value: u32,
) -> Ump {
    voice(
        group,
        channel,
        Voice2::Registered {
            bank: U7::of(bank),
            index: U7::of(index),
            value,
        },
    )
}

/// A MIDI 2.0 Registered Controller at a named address.
pub fn registered_rpn(group: MidiGroup, channel: MidiChannel, rpn: Rpn, value: u32) -> Ump {
    voice(
        group,
        channel,
        Voice2::Registered {
            bank: rpn.bank(),
            index: rpn.index(),
            value,
        },
    )
}

/// A MIDI 2.0 Assignable Controller (NRPN).
pub fn assignable_controller(
    group: MidiGroup,
    channel: MidiChannel,
    bank: u8,
    index: u8,
    value: u32,
) -> Ump {
    voice(
        group,
        channel,
        Voice2::Assignable {
            bank: U7::of(bank),
            index: U7::of(index),
            value,
        },
    )
}

/// A MIDI 2.0 Relative Registered Controller.
pub fn relative_registered_controller(
    group: MidiGroup,
    channel: MidiChannel,
    bank: u8,
    index: u8,
    delta: i32,
) -> Ump {
    voice(
        group,
        channel,
        Voice2::RelativeRegistered {
            bank: U7::of(bank),
            index: U7::of(index),
            delta,
        },
    )
}

/// A MIDI 2.0 Relative Assignable Controller.
pub fn relative_assignable_controller(
    group: MidiGroup,
    channel: MidiChannel,
    bank: u8,
    index: u8,
    delta: i32,
) -> Ump {
    voice(
        group,
        channel,
        Voice2::RelativeAssignable {
            bank: U7::of(bank),
            index: U7::of(index),
            delta,
        },
    )
}

/// A MIDI 2.0 per-note controller: registered (`registered`) or assignable,
/// `index` its controller number.
pub fn per_note_controller(
    group: MidiGroup,
    channel: MidiChannel,
    n: u8,
    index: u8,
    value: u32,
    registered: bool,
) -> Ump {
    let v = if registered {
        Voice2::RegisteredPerNote {
            note: NoteNumber::of(n),
            controller: RegisteredPerNoteController::from(index),
            value,
        }
    } else {
        Voice2::AssignablePerNote {
            note: NoteNumber::of(n),
            controller: index,
            value,
        }
    };
    voice(group, channel, v)
}

/// A MIDI 2.0 Per-Note Management.
pub fn per_note_management(
    group: MidiGroup,
    channel: MidiChannel,
    n: u8,
    detach: bool,
    reset: bool,
) -> Ump {
    voice(
        group,
        channel,
        Voice2::PerNoteManagement {
            note: NoteNumber::of(n),
            detach,
            reset,
        },
    )
}

/// Timing Clock.
pub fn timing_clock(group: MidiGroup) -> Ump {
    system(group, System::TimingClock)
}

/// Start.
pub fn start(group: MidiGroup) -> Ump {
    system(group, System::Start)
}

/// Continue.
pub fn continue_msg(group: MidiGroup) -> Ump {
    system(group, System::Continue)
}

/// Stop.
pub fn stop(group: MidiGroup) -> Ump {
    system(group, System::Stop)
}

/// Song Position Pointer, in MIDI beats.
pub fn song_position(group: MidiGroup, position: u16) -> Ump {
    system(group, System::SongPosition(U14::of(position)))
}

/// A Utility NOOP.
pub fn noop() -> Ump {
    Message::Utility(Utility::Noop).into()
}

/// A Utility JR Timestamp.
pub fn jr_timestamp(timestamp: u16) -> Ump {
    Message::Utility(Utility::JrTimestamp(timestamp)).into()
}

/// A Utility JR Clock.
pub fn jr_clock(time: u16) -> Ump {
    Message::Utility(Utility::JrClock(time)).into()
}

/// An MPE lower zone of `members` member channels as an [`MpeMode`]. Panics
/// outside `1..=15`, which RP-053 gives no zone.
pub fn lower_zone(members: u8) -> MpeMode {
    zones(Some(members), None)
}

/// An MPE upper zone of `members` member channels as an [`MpeMode`].
pub fn upper_zone(members: u8) -> MpeMode {
    zones(None, Some(members))
}

/// Both MPE zones as an [`MpeMode`]. Panics when the two would share a member
/// channel (more than 14 members between them).
pub fn dual_zones(lower: u8, upper: u8) -> MpeMode {
    zones(Some(lower), Some(upper))
}

fn zones(lower: Option<u8>, upper: Option<u8>) -> MpeMode {
    let config = |zone, n: u8| {
        MpeZoneConfig::new(zone, n).unwrap_or_else(|| panic!("{n} is not an MPE member count"))
    };
    MpeMode::Zones(
        MpeZones::new(
            lower.map(|n| config(MpeZone::Lower, n)),
            upper.map(|n| config(MpeZone::Upper, n)),
        )
        .expect("zones that RP-053 allows"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // Pins the MIDI 1.0 builders to M1-4.2 / M2-104 figures: type 2, group 0,
    // status nibble, channel, then the two data bytes.
    #[test]
    fn the_midi1_builders_have_the_spec_layout() {
        let (c, n) = (MidiChannel::of(0), NoteNumber::of(60));
        assert_eq!(m1_note_on(c, n, U7::of(100)).0, [0x2090_3C64, 0, 0, 0]);
        assert_eq!(
            m1_note_off(MidiChannel::of(3), n, U7::of(0)).0,
            [0x2083_3C00, 0, 0, 0]
        );
        assert_eq!(
            m1_cc(MidiChannel::of(2), CCNumber::VOLUME, U7::of(100)).0,
            [0x20B2_0764, 0, 0, 0]
        );
        assert_eq!(
            from_midi1_bytes(&[0x90, 60, 100]),
            Some(m1_note_on(c, n, U7::of(100)))
        );
        assert_eq!(from_midi1_bytes(&[0xF0, 1, 0xF7]), None);
        assert_eq!(MID_VELOCITY, wide_velocity(64));
        assert_eq!(
            key_of(&m1_note_off(MidiChannel::of(3), n, U7::of(0))),
            Some((MidiChannel::of(3), n, false))
        );
        assert_eq!(
            key_of(&note_on(MidiGroup::FIRST, c, 61, 1)),
            Some((c, NoteNumber::of(61), true))
        );
        assert_eq!(key_of(&timing_clock(MidiGroup::FIRST)), None);
        assert_eq!(&sysex_payload(3), &[1, 2, 3]);
        assert_eq!(sysex_payload(130)[126..], [0x7F, 0, 1, 2]);
        // M2-104 Table 18: a payload of 6 or fewer bytes is one Complete packet.
        assert_eq!(sysex7_packets(&[1, 2, 3]).len(), 1);
        assert_eq!(sysex7_packets(&sysex_payload(7)).len(), 2);
    }

    // Pins the encoder against M2-104's figures for the two messages most
    // tests send, so a helper that built the wrong message would fail here
    // rather than make every caller agree with it.
    #[test]
    fn note_on_and_off_have_the_spec_layout() {
        let g = MidiGroup::of(2);
        let c = MidiChannel::of(5);
        // M2-104 §7.4.2: type 4, group, status 9 / 8, channel; note, attr type;
        // velocity in the high half of word 1.
        assert_eq!(
            note_on(g, c, 60, 0x8000).0,
            [0x4295_3C00, 0x8000_0000, 0, 0]
        );
        assert_eq!(
            note_off(g, c, 60, 0x1234).0,
            [0x4285_3C00, 0x1234_0000, 0, 0]
        );
    }

    #[test]
    #[should_panic(expected = "a NoteNumber is 7 bits")]
    fn an_out_of_range_note_is_refused() {
        let _ = note_on(MidiGroup::FIRST, MidiChannel::FIRST, 128, 1);
    }
}
