//! The legacy MIDI CC a plugin sends to the host, as a [`WireMidi`].

use tutti_midi_protocol::widen;
use tutti_ump::{
    Address, CCNumber, Message, MidiChannel, MidiGroup, NoteNumber, U7, U14, Ump, Voice1,
};
use tutti_vst3_ffi::sdk_enum_i32;
use vst3::Steinberg::Vst::ControllerNumbers_::{kAfterTouch, kCtrlPolyPressure, kPitchBend};

use tutti_plugin_types::WireMidi;

use super::LegacyCcOut;

/// The low 7 bits of a VST3 `i8` data field.
fn data_byte(raw: i8) -> U7 {
    // Masked to 7 bits, so the fallback is never taken.
    U7::new(raw.cast_unsigned() & 0x7F).unwrap_or(U7::MAX)
}

/// The UMP message a [`LegacyCcOut`] stands for, as a MIDI 1.0 message widened
/// to MIDI 2.0 (M2-104 App. D.3). `controlNumber` is a VST3 `ControllerNumbers`
/// index: 0-127 are real CCs, and the synthetic slots above are pitch bend,
/// channel pressure and poly-pressure, with `value2` supplying the second data
/// byte for the first and last.
pub(super) fn decode(e: &LegacyCcOut, frame: u32) -> Option<WireMidi> {
    let address = Address {
        group: MidiGroup::FIRST,
        channel: MidiChannel::masked(e.channel as u8),
    };
    let v1 = data_byte(e.value);
    let v2 = data_byte(e.value2);
    let control = i32::from(e.controlNumber);

    let voice = if control == sdk_enum_i32(kPitchBend) {
        // 14-bit: LSB = value, MSB = value2.
        Voice1::PitchBend(U14::from_halves(v2, v1))
    } else if control == sdk_enum_i32(kAfterTouch) {
        Voice1::ChannelPressure(v1)
    } else if control == sdk_enum_i32(kCtrlPolyPressure) {
        // value = note, value2 = pressure.
        Voice1::PolyPressure {
            note: NoteNumber::new(v1.get())?,
            pressure: v2,
        }
    } else {
        Voice1::ControlChange {
            controller: CCNumber::new(e.controlNumber)?,
            value: v1,
        }
    };
    let ump = Ump::from(Message::Midi2(address, widen(voice)));
    Some(WireMidi { offset: frame, ump })
}
