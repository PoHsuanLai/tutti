//! MIDI 1.0 → MIDI 2.0 translation that needs *state*, per M2-104 Appendix D.
//!
//! [`crate::normalize`] handles the stateless quirks (velocity-0 NoteOn → NoteOff,
//! per-message CV1 → CV2 promotion). Some MIDI 1.0 constructs, though, span
//! *several* messages and can only be translated by accumulating them:
//!
//! - **RPN / NRPN.** MIDI 1.0 expresses a Registered/Non-Registered Parameter as
//!   a run of CCs — select the parameter with CC 101/100 (RPN MSB/LSB) or CC
//!   99/98 (NRPN MSB/LSB), then send the value with CC 6/38 (Data Entry MSB/LSB).
//!   MIDI 2.0 has one dedicated message each ([`MidiEvent::registered_controller`]
//!   / [`MidiEvent::assignable_controller`]). [`Midi1ToMidi2Translator`] watches
//!   the CC stream per channel and, when a Data Entry completes a selected
//!   parameter, emits the single MIDI-2 controller message.
//!
//! A translator is stateful and per-endpoint; feed it every inbound CV1 event.
//! Events that aren't part of an (N)RPN run are promoted straight through via
//! [`crate::normalize`], so a caller can treat it as "MIDI-1 in, MIDI-2 out".

use midi2::channel_voice1::ChannelVoice1;
use midi2::{Channeled, UmpMessage};

use crate::cc;
use crate::ump::MidiEvent;

/// Per-channel accumulator for an in-progress (N)RPN run.
#[derive(Clone, Copy, Default)]
struct ParamState {
    /// Selected parameter MSB / LSB (CC 101/100 for RPN, 99/98 for NRPN).
    bank: u8,
    index: u8,
    /// Data Entry MSB / LSB (CC 6 / 38) accumulated for the current parameter.
    data_msb: u8,
    data_lsb: u8,
    /// Whether the currently-selected parameter is registered (RPN) or not (NRPN).
    registered: bool,
    /// Whether a parameter has been selected (so a Data Entry is meaningful).
    selected: bool,
}

/// Stateful MIDI 1.0 → MIDI 2.0 translator. One per endpoint/channel-group.
#[derive(Clone, Default)]
pub struct Midi1ToMidi2Translator {
    /// One accumulator per MIDI channel (0..16).
    channels: [ParamState; 16],
}

impl Midi1ToMidi2Translator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one inbound event. Returns the MIDI-2 event(s) it translates to:
    /// - a completed (N)RPN Data Entry yields a single Registered/Assignable
    ///   Controller message;
    /// - a parameter-select or partial Data Entry is *absorbed* (returns `None` —
    ///   it only updates state);
    /// - anything else is promoted through [`crate::normalize`].
    pub fn translate(&mut self, event: &MidiEvent) -> Option<MidiEvent> {
        let Ok(UmpMessage::ChannelVoice1(ChannelVoice1::ControlChange(m))) =
            UmpMessage::try_from(event.data_words())
        else {
            // Not a MIDI-1 CC — nothing to accumulate; promote straight through.
            return Some(crate::normalize(event));
        };
        let channel = u8::from(m.channel()) as usize;
        let control = u8::from(m.control());
        let value = u8::from(m.control_data());
        let state = &mut self.channels[channel & 0x0F];

        match control {
            cc::RPN_MSB => {
                state.bank = value;
                state.registered = true;
                state.selected = true;
                None
            }
            cc::RPN_LSB => {
                state.index = value;
                state.registered = true;
                state.selected = true;
                None
            }
            cc::NRPN_MSB => {
                state.bank = value;
                state.registered = false;
                state.selected = true;
                None
            }
            cc::NRPN_LSB => {
                state.index = value;
                state.registered = false;
                state.selected = true;
                None
            }
            cc::DATA_ENTRY => {
                state.data_msb = value;
                self.emit(channel, event.frame_offset)
            }
            cc::DATA_ENTRY_LSB => {
                state.data_lsb = value;
                self.emit(channel, event.frame_offset)
            }
            // A plain CC (not part of an (N)RPN run) promotes normally.
            _ => Some(crate::normalize(event)),
        }
    }

    /// Emit the MIDI-2 controller message for the channel's currently-selected
    /// parameter and accumulated data. `None` if no parameter is selected.
    fn emit(&self, channel: usize, frame_offset: u32) -> Option<MidiEvent> {
        let state = &self.channels[channel & 0x0F];
        if !state.selected {
            return None;
        }
        // Widen the 14-bit MIDI-1 Data Entry (MSB<<7 | LSB) to 32 bits via the
        // spec Min-Center-Max upscale, so a MIDI-1 RPN value lands where its
        // MIDI-2 equivalent would.
        let data14 = ((state.data_msb as u16) << 7) | (state.data_lsb as u16);
        let data32 = crate::convert::midi1_pitch_bend_to_midi2(data14);
        let ch = channel as u8;
        let ev = if state.registered {
            MidiEvent::registered_controller(0, ch, state.bank, state.index, data32)
        } else {
            MidiEvent::assignable_controller(0, ch, state.bank, state.index, data32)
        };
        Some(ev.with_frame_offset(frame_offset))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use midi2::channel_voice2::ChannelVoice2;

    /// A MIDI *1.0* wire CC (status 0xB0 | channel) — the translator's input.
    fn cc_ev(channel: u8, control: u8, value: u8) -> MidiEvent {
        MidiEvent::from_midi1_bytes(0, &[0xB0 | (channel & 0x0F), control, value])
            .expect("valid MIDI-1 CC")
    }

    #[test]
    fn rpn_run_translates_to_registered_controller() {
        let mut t = Midi1ToMidi2Translator::new();
        // Select RPN 0x00 / 0x06 (the MPE MCM), then Data Entry MSB = 10.
        assert!(t.translate(&cc_ev(3, cc::RPN_MSB, 0x00)).is_none());
        assert!(t.translate(&cc_ev(3, cc::RPN_LSB, 0x06)).is_none());
        let out = t
            .translate(&cc_ev(3, cc::DATA_ENTRY, 10))
            .expect("emits on data entry");
        match UmpMessage::try_from(out.data_words()).unwrap() {
            UmpMessage::ChannelVoice2(ChannelVoice2::RegisteredController(m)) => {
                assert_eq!(u8::from(m.channel()), 3);
                assert_eq!(u8::from(m.bank()), 0x00);
                assert_eq!(u8::from(m.index()), 0x06);
            }
            other => panic!("expected RegisteredController, got {other:?}"),
        }
    }

    #[test]
    fn nrpn_run_translates_to_assignable_controller() {
        let mut t = Midi1ToMidi2Translator::new();
        assert!(t.translate(&cc_ev(0, cc::NRPN_MSB, 0x12)).is_none());
        assert!(t.translate(&cc_ev(0, cc::NRPN_LSB, 0x34)).is_none());
        let out = t.translate(&cc_ev(0, cc::DATA_ENTRY, 64)).expect("emits");
        match UmpMessage::try_from(out.data_words()).unwrap() {
            UmpMessage::ChannelVoice2(ChannelVoice2::AssignableController(m)) => {
                assert_eq!(u8::from(m.bank()), 0x12);
                assert_eq!(u8::from(m.index()), 0x34);
            }
            other => panic!("expected AssignableController, got {other:?}"),
        }
    }

    #[test]
    fn plain_cc_promotes_straight_through() {
        let mut t = Midi1ToMidi2Translator::new();
        let out = t
            .translate(&cc_ev(1, cc::MOD_WHEEL, 100))
            .expect("plain CC promotes");
        match UmpMessage::try_from(out.data_words()).unwrap() {
            UmpMessage::ChannelVoice2(ChannelVoice2::ControlChange(m)) => {
                assert_eq!(u8::from(m.control()), cc::MOD_WHEEL);
            }
            other => panic!("expected CV2 ControlChange, got {other:?}"),
        }
    }

    #[test]
    fn note_on_promotes_straight_through() {
        let mut t = Midi1ToMidi2Translator::new();
        let midi1 = MidiEvent::from_midi1_bytes(0, &[0x93, 60, 100]).unwrap();
        let out = t.translate(&midi1).expect("note promotes");
        assert!(matches!(
            UmpMessage::try_from(out.data_words()).unwrap(),
            UmpMessage::ChannelVoice2(ChannelVoice2::NoteOn(_))
        ));
    }
}
