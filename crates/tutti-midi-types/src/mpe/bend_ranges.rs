//! The per-note pitch-bend range each channel of a stream has set.

use tutti_midi_protocol::PitchBendSensitivity;
use tutti_ump::Ump;
use tutti_ump::{Rpn, U7};

/// The per-note pitch-bend range of each MIDI channel: what a per-note pitch
/// bend's full scale means, in semitones, for a receiver that turns one into
/// a tuning (a plugin host's note expression).
///
/// Each channel starts at the MPE default, ±48 (RP-053), which the engine's
/// own synth assumes too, and follows the stream's "Sensitivity of Per-Note
/// Pitch Bend" (RPN #00/07, M2-104 §7.4.13), which a sender sets per channel.
/// The range is the stream's state, not a block's: a receiver keeps one for
/// as long as it reads the stream ([`observe`](Self::observe) each packet in
/// order).
///
/// Keyed by channel alone, ignoring the UMP group: per-note identity is keyed
/// the same way downstream (a plugin host's note id is channel and note), so
/// a stream that spreads one instrument over several groups shares one range
/// per channel number.
///
/// A fork of a live graph (an offline export) starts every channel back at
/// ±48: it renders its own stream, not the live MIDI that set the ranges, so
/// a range it needs must be in that stream.
///
/// # Examples
///
/// ```
/// use tutti_fixtures::midi::registered_rpn;
/// use tutti_midi_protocol::PitchBendSensitivity;
/// use tutti_midi_types::PerNoteBendRanges;
/// use tutti_ump::{MidiChannel, MidiGroup, Rpn};
///
/// let mut ranges = PerNoteBendRanges::default();
/// assert_eq!(ranges.semitones(3), 48.0);
/// let rpn = registered_rpn(
///     MidiGroup::FIRST,
///     MidiChannel::of(3),
///     Rpn::PER_NOTE_PITCH_BEND_SENSITIVITY,
///     PitchBendSensitivity::of(24).to_rpn_bits(),
/// );
/// assert!(ranges.observe(&rpn));
/// assert_eq!(ranges.semitones(3), 24.0);
/// assert_eq!(ranges.semitones(4), 48.0);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PerNoteBendRanges([PitchBendSensitivity; 16]);

impl Default for PerNoteBendRanges {
    /// Every channel at the MPE default, ±48.
    fn default() -> Self {
        Self([PitchBendSensitivity::MPE_DEFAULT; 16])
    }
}

impl PerNoteBendRanges {
    /// The range on `channel` (0-indexed; the low four bits are read).
    pub fn get(&self, channel: u8) -> PitchBendSensitivity {
        self.0[usize::from(channel & 0x0F)]
    }

    /// The range on `channel`, in semitones.
    pub fn semitones(&self, channel: u8) -> f32 {
        self.get(channel).as_semitones_f32()
    }

    /// Follow `ump`: a per-note bend sensitivity (RPN #00/07) sets its
    /// channel's range, and `true` says it was one. A receiver that has no
    /// event of its own for the message (no plugin format has one) consumes
    /// it there. Anything else leaves the ranges as they are. No allocation.
    pub fn observe(&mut self, ump: &Ump) -> bool {
        match per_note_bend_sensitivity(ump) {
            Some((channel, range)) => {
                self.0[usize::from(channel & 0x0F)] = range;
                true
            }
            None => false,
        }
    }
}

/// The channel and range of a per-note bend sensitivity message (RPN
/// #00/07), or `None` for any other packet.
fn per_note_bend_sensitivity(ump: &Ump) -> Option<(u8, PitchBendSensitivity)> {
    use midi2::channel_voice2::ChannelVoice2;
    use midi2::{Channeled, UmpMessage};
    let UmpMessage::ChannelVoice2(ChannelVoice2::RegisteredController(m)) =
        UmpMessage::try_from(ump.words()).ok()?
    else {
        return None;
    };
    // A midi2 bank and index are 7-bit, so `U7::new` never refuses.
    let rpn = Rpn::new(U7::new(u8::from(m.bank()))?, U7::new(u8::from(m.index()))?);
    (rpn == Rpn::PER_NOTE_PITCH_BEND_SENSITIVITY).then(|| {
        (
            u8::from(m.channel()),
            PitchBendSensitivity::from_rpn_bits(m.controller_data()),
        )
    })
}

#[cfg(test)]
mod tests;
