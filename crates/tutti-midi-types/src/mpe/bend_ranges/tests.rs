use super::*;
use crate::ump::RPN_INDEX_CHANNEL_PITCH_BEND_SENSITIVITY;
use tutti_fixtures::midi;
use tutti_ump::{MidiChannel, MidiGroup};

fn rpn(channel: u8, index: u8, semitones: u8) -> Ump {
    midi::registered_controller(
        MidiGroup::FIRST,
        MidiChannel::of(channel),
        RPN_BANK_MPE,
        index,
        PitchBendSensitivity::of(semitones).to_rpn_bits(),
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
    let note = midi::note_on(MidiGroup::FIRST, MidiChannel::of(5), 60, 0x8000);
    assert!(!ranges.observe(&channel_wide));
    assert!(!ranges.observe(&note));
    assert_eq!(ranges, PerNoteBendRanges::default());

    assert!(ranges.observe(&rpn(5, RPN_INDEX_PER_NOTE_PITCH_BEND_SENSITIVITY, 12)));
    for channel in 0..16 {
        let want = if channel == 5 { 12.0 } else { 48.0 };
        assert_eq!(ranges.semitones(channel), want, "channel {channel}");
    }
}
