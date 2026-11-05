//! The legacy MIDI CC output event and its conversion to a `MidiEvent`.

use super::*;

/// Legacy MIDI CC emitted by the plugin back to the host (arpeggiators, MIDI
/// effects). `control_number` is a `ControllerNumbers` index; `value2` carries
/// the second data byte for pitch-bend / poly-pressure. Output-only.
#[derive(Debug, Clone, Copy)]
pub struct LegacyMidiCcOutEvent {
    /// Bus, frame offset and event-type discriminant.
    pub header: EventHeader,
    /// A VST3 `ControllerNumbers` index, not always a plain CC number: the
    /// enum's upper values stand for pitch bend, aftertouch and program change.
    pub control_number: u8,
    /// Zero-based MIDI channel.
    pub channel: i8,
    /// First data byte, 0..=127.
    pub value: i8,
    /// Second data byte, used by pitch-bend and poly-pressure; 0 otherwise.
    pub value2: i8,
}

/// Rebuilds the UMP MIDI message a plugin-emitted [`LegacyMidiCcOutEvent`] stands
/// for. `control_number` is a VST3 `ControllerNumbers` index: 0-127 are real
/// CCs; the synthetic slots map to their channel-voice messages, with `value2`
/// supplying the second data byte for pitch bend and poly-pressure.
/// `ControllerNumbers_` variants are SDK ordinals below 130, and `cn` is
/// range-checked against `0..=127` before the narrowing that reaches MIDI.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    reason = "SDK controller ordinals < 130; cn is range-checked before narrowing"
)]
pub(super) fn legacy_cc_to_midi(e: &LegacyMidiCcOutEvent, frame: u32) -> Option<MidiEvent> {
    use crate::sdk::sdk_enum_i32;
    use tutti_midi_types::convert::{midi1_cc_to_midi2, midi1_pitch_bend_to_midi2};
    use vst3::Steinberg::Vst::ControllerNumbers_::{kAfterTouch, kCtrlPolyPressure, kPitchBend};

    let channel = (e.channel as u8) & 0x0F;
    let v1 = (e.value as u8) & 0x7F;
    let v2 = (e.value2 as u8) & 0x7F;
    let cn = e.control_number as i32;

    let ev = if cn == sdk_enum_i32(kPitchBend) {
        // 14-bit: LSB = value, MSB = value2.
        let bend14 = (v1 as u16) | ((v2 as u16) << 7);
        MidiEvent::pitch_bend(
            MidiGroup::FIRST,
            MidiChannel::new(channel),
            midi1_pitch_bend_to_midi2(bend14),
        )
    } else if cn == sdk_enum_i32(kAfterTouch) {
        MidiEvent::channel_pressure(
            MidiGroup::FIRST,
            MidiChannel::new(channel),
            midi1_cc_to_midi2(v1),
        )
    } else if cn == sdk_enum_i32(kCtrlPolyPressure) {
        // value = note, value2 = pressure.
        MidiEvent::poly_pressure(
            MidiGroup::FIRST,
            MidiChannel::new(channel),
            v1,
            midi1_cc_to_midi2(v2),
        )
    } else if (0..=127).contains(&cn) {
        MidiEvent::cc(
            MidiGroup::FIRST,
            MidiChannel::new(channel),
            // Wire boundary: `cn` is a VST3 `control_number`, already narrowed
            // to `0..=127` by the arm's guard, so the mask is a no-op.
            CCNumber::new(cn as u8),
            midi1_cc_to_midi2(v1),
        )
    } else {
        return None;
    };
    Some(ev.with_frame_offset(frame))
}
