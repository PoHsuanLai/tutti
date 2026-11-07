//! The stateless `Voice1` to `Voice2` widening.

use crate::scale;
use tutti_ump::{Attribute, Voice1, Voice2};

/// The release velocity a MIDI 1.0 Note On at velocity 0 becomes (M2-104
/// Appendix D.3.1).
const NOTE_ON_ZERO_RELEASE: u16 = 0x8000;

/// The MIDI 2.0 message that means what `voice` means (M2-104 Appendix D.3,
/// M2-115 section 3). The caller keeps the [`Address`](tutti_ump::Address)
/// unchanged.
///
/// Transitional for a consumer: see the crate docs. Control Changes that
/// would need RPN or Bank Select assembly widen by value as plain Control
/// Changes, and a Program Change has no bank.
#[inline]
#[must_use]
pub fn widen(voice: Voice1) -> Voice2 {
    match voice {
        Voice1::NoteOn { note, velocity } if velocity.get() == 0 => Voice2::NoteOff {
            note,
            velocity: NOTE_ON_ZERO_RELEASE,
            attribute: Attribute::None,
        },
        Voice1::NoteOn { note, velocity } => Voice2::NoteOn {
            note,
            velocity: scale::velocity(velocity.get()),
            attribute: Attribute::None,
        },
        Voice1::NoteOff { note, velocity } => Voice2::NoteOff {
            note,
            velocity: scale::velocity(velocity.get()),
            attribute: Attribute::None,
        },
        Voice1::PolyPressure { note, pressure } => Voice2::PolyPressure {
            note,
            value: scale::value(pressure.get()),
        },
        Voice1::ControlChange { controller, value } => Voice2::ControlChange {
            controller,
            value: scale::value(value.get()),
        },
        Voice1::ProgramChange(program) => Voice2::ProgramChange {
            program,
            bank: None,
        },
        Voice1::ChannelPressure(pressure) => Voice2::ChannelPressure(scale::value(pressure.get())),
        Voice1::PitchBend(bend) => Voice2::PitchBend(scale::bend(bend.get())),
    }
}
