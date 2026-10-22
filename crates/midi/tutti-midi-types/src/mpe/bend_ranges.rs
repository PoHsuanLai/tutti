//! The per-note pitch-bend range each channel of a stream has set.

use super::PitchBendSensitivity;
use crate::ump::{MidiEvent, RPN_BANK_MPE, RPN_INDEX_PER_NOTE_PITCH_BEND_SENSITIVITY};

/// The per-note pitch-bend range of each MIDI channel: what a per-note pitch
/// bend's full scale means, in semitones, for a receiver that turns one into
/// a tuning (a plugin host's note expression).
///
/// Each channel starts at the MPE default, ±48 (RP-053), which the engine's
/// own synth assumes too, and follows the stream's "Sensitivity of Per-Note
/// Pitch Bend" (RPN #00/07, M2-104 §7.4.13), which a sender sets per channel.
/// The range is the stream's state, not a block's: a receiver keeps one for
/// as long as it reads the stream ([`observe`](Self::observe) each event in
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
/// use tutti_midi_types::ump::{RPN_BANK_MPE, RPN_INDEX_PER_NOTE_PITCH_BEND_SENSITIVITY};
/// use tutti_midi_types::{MidiChannel, MidiEvent, MidiGroup, PerNoteBendRanges, PitchBendSensitivity};
///
/// let mut ranges = PerNoteBendRanges::default();
/// assert_eq!(ranges.semitones(3), 48.0);
/// let rpn = MidiEvent::registered_controller(
///     MidiGroup::FIRST,
///     MidiChannel::new(3),
///     RPN_BANK_MPE,
///     RPN_INDEX_PER_NOTE_PITCH_BEND_SENSITIVITY,
///     PitchBendSensitivity::from_semitones(24).to_rpn_bits(),
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

    /// Follow `event`: a per-note bend sensitivity (RPN #00/07) sets its
    /// channel's range, and `true` says it was one. A receiver that has no
    /// event of its own for the message (no plugin format has one) consumes
    /// it there. Anything else leaves the ranges as they are. No allocation.
    pub fn observe(&mut self, event: &MidiEvent) -> bool {
        match per_note_bend_sensitivity(event) {
            Some((channel, range)) => {
                self.0[usize::from(channel & 0x0F)] = range;
                true
            }
            None => false,
        }
    }
}

/// The channel and range of a per-note bend sensitivity message (RPN
/// #00/07), or `None` for any other event.
fn per_note_bend_sensitivity(event: &MidiEvent) -> Option<(u8, PitchBendSensitivity)> {
    use midi2::channel_voice2::ChannelVoice2;
    use midi2::{Channeled, UmpMessage};
    let UmpMessage::ChannelVoice2(ChannelVoice2::RegisteredController(m)) =
        UmpMessage::try_from(event.data_words()).ok()?
    else {
        return None;
    };
    let ours = u8::from(m.bank()) == RPN_BANK_MPE
        && u8::from(m.index()) == RPN_INDEX_PER_NOTE_PITCH_BEND_SENSITIVITY;
    ours.then(|| {
        (
            u8::from(m.channel()),
            PitchBendSensitivity::from_rpn_bits(m.controller_data()),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ump::RPN_INDEX_CHANNEL_PITCH_BEND_SENSITIVITY;
    use crate::{MidiChannel, MidiGroup};

    fn rpn(channel: u8, index: u8, semitones: u8) -> MidiEvent {
        MidiEvent::registered_controller(
            MidiGroup::FIRST,
            MidiChannel::new(channel),
            RPN_BANK_MPE,
            index,
            PitchBendSensitivity::from_semitones(semitones).to_rpn_bits(),
        )
    }

    /// Only RPN #00/07 sets a range, and only on its own channel; the
    /// channel-wide sensitivity (#00/00) and a note pass by untouched.
    ///
    /// Mutation (run): `per_note_bend_sensitivity` drops the index compare →
    /// the #00/00 message is consumed and sets channel 5 → fails.
    /// Mutation (run): `observe` writes channel 0 instead of the event's →
    /// channel 5 stays at 48 → fails.
    #[test]
    fn only_the_per_note_sensitivity_sets_a_channels_range() {
        let mut ranges = PerNoteBendRanges::default();
        let channel_wide = rpn(5, RPN_INDEX_CHANNEL_PITCH_BEND_SENSITIVITY, 2);
        let note = MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::new(5), 60, 0x8000);
        assert!(!ranges.observe(&channel_wide));
        assert!(!ranges.observe(&note));
        assert_eq!(ranges, PerNoteBendRanges::default());

        assert!(ranges.observe(&rpn(5, RPN_INDEX_PER_NOTE_PITCH_BEND_SENSITIVITY, 12)));
        for channel in 0..16 {
            let want = if channel == 5 { 12.0 } else { 48.0 };
            assert_eq!(ranges.semitones(channel), want, "channel {channel}");
        }
    }
}
