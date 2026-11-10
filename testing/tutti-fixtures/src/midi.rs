//! Test builders for single MIDI messages, as [`Ump`]s.
//!
//! Each builds a `tutti_ump::Message` and encodes it with `From<Message> for
//! Ump`, the one encoder; nothing here sets a bit by hand. They take raw
//! numbers where a test writes literals, and panic on a value the field type
//! refuses, so a test cannot build a message the wire could not carry.
//!
//! Production code builds the `Message` itself: these exist so a test reads
//! as the message it sends, not as the struct literal.

use tutti_ump::{
    Address, Bank, Message, MidiChannel, MidiGroup, NoteNumber, RegisteredPerNoteController,
    System, U7, U14, Ump, Utility, Voice2,
};

fn note(n: u8) -> NoteNumber {
    NoteNumber::new(n).unwrap_or_else(|| panic!("note {n} out of range"))
}

fn u7(v: u8) -> U7 {
    U7::new(v).unwrap_or_else(|| panic!("{v} is not 7-bit"))
}

fn voice(group: MidiGroup, channel: MidiChannel, v: Voice2) -> Ump {
    Message::Midi2(Address { group, channel }, v).into()
}

fn system(group: MidiGroup, s: System) -> Ump {
    Message::System(group, s).into()
}

/// A MIDI 2.0 Note On, no attribute.
pub fn note_on(group: MidiGroup, channel: MidiChannel, n: u8, velocity: u16) -> Ump {
    voice(
        group,
        channel,
        Voice2::NoteOn {
            note: note(n),
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
            note: note(n),
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
            note: note(n),
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
        msb: u7(((b >> 7) & 0x7F) as u8),
        lsb: u7((b & 0x7F) as u8),
    });
    voice(
        group,
        channel,
        Voice2::ProgramChange {
            program: u7(program),
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
            note: note(n),
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
            bank: u7(bank),
            index: u7(index),
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
            bank: u7(bank),
            index: u7(index),
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
            bank: u7(bank),
            index: u7(index),
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
            bank: u7(bank),
            index: u7(index),
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
            note: note(n),
            controller: RegisteredPerNoteController::from(index),
            value,
        }
    } else {
        Voice2::AssignablePerNote {
            note: note(n),
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
            note: note(n),
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
    let p = U14::new(position).unwrap_or_else(|| panic!("{position} is not 14-bit"));
    system(group, System::SongPosition(p))
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

#[cfg(test)]
mod tests {
    use super::*;

    // Pins the encoder against M2-104's figures for the two messages most
    // tests send, so a helper that built the wrong message would fail here
    // rather than make every caller agree with it. Mutation: swap NoteOn's
    // status nibble for NoteOff's in `note_on` and the first assert fails.
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
    #[should_panic(expected = "note 128 out of range")]
    fn an_out_of_range_note_is_refused() {
        let _ = note_on(MidiGroup::FIRST, MidiChannel::FIRST, 128, 1);
    }
}
