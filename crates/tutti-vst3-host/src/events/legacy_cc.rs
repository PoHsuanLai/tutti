//! The legacy MIDI CC a plugin sends to the host, as a [`WireMidi`].

use tutti_midi_protocol::widen;
use tutti_ump::{
    Address, CCNumber, Message, MidiChannel, MidiGroup, NoteNumber, System, U7, U14, Ump, Voice1,
};
use tutti_vst3_ffi::sdk_enum_i32;
use vst3::Steinberg::Vst::ControllerNumbers_::{
    kAfterTouch, kCtrlPolyPressure, kCtrlProgramChange, kCtrlQuarterFrame, kPitchBend,
    kSystemActiveSensing, kSystemMidiClockContinue, kSystemMidiClockStart, kSystemMidiClockStop,
    kSystemSongPointer, kSystemSongSelect, kSystemTuneRequest,
};

use tutti_plugin_types::WireMidi;

use super::LegacyCcOut;

/// The low 7 bits of a VST3 `i8` data field.
fn data_byte(raw: i8) -> U7 {
    // Masked to 7 bits, so the fallback is never taken.
    U7::new(raw.cast_unsigned() & 0x7F).unwrap_or(U7::MAX)
}

/// The UMP message a [`LegacyCcOut`] stands for, on `group` (the event output
/// bus it left on), as a MIDI 1.0 message widened to MIDI 2.0 (M2-104 App.
/// D.3). `controlNumber` is a VST3 `ControllerNumbers` index (ivstmidicontrollers.h):
/// 0-127 are real CCs, and the slots above are pitch bend, channel pressure,
/// program change, poly-pressure and the System messages, with `value2`
/// supplying the second data byte for pitch bend, poly-pressure and song
/// position. Cable Select (135) has no MIDI message, and a number past 140 is
/// none: both are `None`.
pub(super) fn decode(e: &LegacyCcOut, frame: u32, group: MidiGroup) -> Option<WireMidi> {
    let address = Address::new(group, MidiChannel::masked(e.channel as u8));
    let v1 = data_byte(e.value);
    let v2 = data_byte(e.value2);
    let control = i32::from(e.controlNumber);
    let is = |number| control == sdk_enum_i32(number);

    let system = if is(kCtrlQuarterFrame) {
        Some(System::TimeCode(v1))
    } else if is(kSystemSongSelect) {
        Some(System::SongSelect(v1))
    } else if is(kSystemSongPointer) {
        // value = LSB, value2 = MSB.
        Some(System::SongPosition(U14::from_halves(v2, v1)))
    } else if is(kSystemTuneRequest) {
        Some(System::TuneRequest)
    } else if is(kSystemMidiClockStart) {
        Some(System::Start)
    } else if is(kSystemMidiClockContinue) {
        Some(System::Continue)
    } else if is(kSystemMidiClockStop) {
        Some(System::Stop)
    } else if is(kSystemActiveSensing) {
        Some(System::ActiveSensing)
    } else {
        None
    };
    if let Some(system) = system {
        let ump = Ump::from(Message::System(group, system));
        return Some(WireMidi { offset: frame, ump });
    }

    let voice = if is(kPitchBend) {
        // 14-bit: LSB = value, MSB = value2.
        Voice1::PitchBend(U14::from_halves(v2, v1))
    } else if is(kAfterTouch) {
        Voice1::ChannelPressure(v1)
    } else if is(kCtrlProgramChange) {
        Voice1::ProgramChange(v1)
    } else if is(kCtrlPolyPressure) {
        // value = note, value2 = pressure.
        Voice1::PolyPressure {
            note: NoteNumber::new(v1.get())?,
            pressure: v2,
        }
    } else {
        // 0-127 only: Cable Select (135) and the numbers above 140 have no
        // message, and `CCNumber` refuses them.
        Voice1::ControlChange {
            controller: CCNumber::new(e.controlNumber)?,
            value: v1,
        }
    };
    let ump = Ump::from(Message::Midi2(address, widen(voice)));
    Some(WireMidi { offset: frame, ump })
}
