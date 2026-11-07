//! The legacy MIDI CC a plugin sends to the host, as a `MidiEvent`.

use tutti_midi_types::convert::{midi1_cc_to_midi2, midi1_pitch_bend_to_midi2};
use tutti_ump::{CCNumber, MidiChannel, MidiGroup};
use tutti_vst3_ffi::sdk_enum_i32;
use vst3::Steinberg::Vst::ControllerNumbers_::{kAfterTouch, kCtrlPolyPressure, kPitchBend};

use super::{LegacyCcOut, MidiEvent};

/// The UMP message a [`LegacyCcOut`] stands for. `controlNumber` is a VST3
/// `ControllerNumbers` index: 0-127 are real CCs, and the synthetic slots above
/// are pitch bend, channel pressure and poly-pressure, with `value2` supplying
/// the second data byte for the first and last.
pub(super) fn decode(e: &LegacyCcOut, frame: u32) -> Option<MidiEvent> {
    let channel = MidiChannel::new((e.channel as u8) & 0x0F);
    let v1 = (e.value as u8) & 0x7F;
    let v2 = (e.value2 as u8) & 0x7F;
    let control = i32::from(e.controlNumber);

    let event = if control == sdk_enum_i32(kPitchBend) {
        // 14-bit: LSB = value, MSB = value2.
        let bend = u16::from(v1) | (u16::from(v2) << 7);
        MidiEvent::pitch_bend(MidiGroup::FIRST, channel, midi1_pitch_bend_to_midi2(bend))
    } else if control == sdk_enum_i32(kAfterTouch) {
        MidiEvent::channel_pressure(MidiGroup::FIRST, channel, midi1_cc_to_midi2(v1))
    } else if control == sdk_enum_i32(kCtrlPolyPressure) {
        // value = note, value2 = pressure.
        MidiEvent::poly_pressure(MidiGroup::FIRST, channel, v1, midi1_cc_to_midi2(v2))
    } else if (0..=127).contains(&control) {
        MidiEvent::cc(
            MidiGroup::FIRST,
            channel,
            CCNumber::new(e.controlNumber),
            midi1_cc_to_midi2(v1),
        )
    } else {
        return None;
    };
    Some(event.with_frame_offset(frame))
}
