//! MIDI 1.0 → MIDI 2.0 translation that needs *state*, per M2-104 Appendix D.
//!
//! [`normalize`](super::normalize) handles the stateless quirks (velocity-0 NoteOn → NoteOff,
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
//! [`normalize`](super::normalize), so a caller can treat it as "MIDI-1 in, MIDI-2 out".

use midi2::channel_voice1::ChannelVoice1;
use midi2::{Channeled, UmpMessage};
use tutti_types::{CCNumber, MidiChannel, MidiGroup};

use crate::cc;
use crate::ump::MidiEvent;

/// Per-channel accumulator for an in-progress (N)RPN run.
#[derive(Clone, Copy, Debug, Default)]
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
    /// Whether Data Entry has arrived for the current parameter — i.e. there is
    /// a value worth flushing when the next parameter select ends this run.
    pending: bool,
}

/// The 0x7F value that, in *both* the MSB and LSB, forms the RPN/NRPN Null
/// Function — the idiom for closing an (N)RPN transaction.
const RPN_NULL: u8 = 0x7F;

/// Which half of the parameter number a select byte carries.
#[derive(Clone, Copy)]
enum ParamByte {
    Bank(u8),
    Index(u8),
}

/// Stateful MIDI 1.0 → MIDI 2.0 translator. One per endpoint/channel-group.
#[derive(Clone, Debug, Default)]
pub struct Midi1ToMidi2Translator {
    /// One accumulator per MIDI channel (0..16).
    channels: [ParamState; 16],
}

impl Midi1ToMidi2Translator {
    /// Builds a translator with every channel's (N)RPN accumulator empty.
    ///
    /// The state is per channel and persists across events, so one translator
    /// must serve one MIDI stream for its whole life — sharing it between two
    /// sources interleaves their Data Entry runs and mixes their parameters.
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one inbound event. Returns the MIDI-2 event(s) it translates to:
    /// - a completed (N)RPN Data Entry yields a single Registered/Assignable
    ///   Controller message;
    /// - a parameter-select or partial Data Entry is *absorbed* (returns `None` —
    ///   it only updates state);
    /// - anything else is promoted through [`normalize`](super::normalize).
    pub fn translate(&mut self, event: &MidiEvent) -> Option<MidiEvent> {
        let Ok(UmpMessage::ChannelVoice1(ChannelVoice1::ControlChange(m))) =
            UmpMessage::try_from(event.data_words())
        else {
            // Not a MIDI-1 CC — nothing to accumulate; promote straight through.
            return Some(super::normalize(event));
        };
        let channel = u8::from(m.channel()) as usize;
        // Wire boundary: midi2's `u7` is already in range, so the mask is a no-op.
        let control = CCNumber::new(u8::from(m.control()));
        let value = u8::from(m.control_data());
        let state = &mut self.channels[channel & 0x0F];

        match control {
            cc::RPN_MSB => self.select(channel, event.frame_offset, true, ParamByte::Bank(value)),
            cc::RPN_LSB => self.select(channel, event.frame_offset, true, ParamByte::Index(value)),
            cc::NRPN_MSB => self.select(channel, event.frame_offset, false, ParamByte::Bank(value)),
            cc::NRPN_LSB => {
                self.select(channel, event.frame_offset, false, ParamByte::Index(value))
            }
            cc::DATA_ENTRY => {
                state.data_msb = value;
                state.pending = true;
                self.emit(channel, event.frame_offset)
            }
            cc::DATA_ENTRY_LSB => {
                state.data_lsb = value;
                state.pending = true;
                self.emit(channel, event.frame_offset)
            }
            // A plain CC (not part of an (N)RPN run) promotes normally.
            _ => Some(super::normalize(event)),
        }
    }

    /// Handle one parameter-select byte (CC 98/99/100/101).
    ///
    /// Appendix D.3.3 lists a parameter select as an *emit trigger*: "a CC 98,
    /// 99, 100, and 101 is received, indicating the last RPN/NRPN message has
    /// ended and a new one has started." So this flushes any parameter still
    /// holding data before adopting the new selection — otherwise the previous
    /// run's `data_msb`/`data_lsb` leak into the next parameter's value.
    ///
    /// It also implements the Null Function rule: "RPN/NRPN Null Function, where
    /// both the MSB and LSB is set to 0x7F, is not translated." Null deselects,
    /// so a following Data Entry must not synthesize a controller message.
    fn select(
        &mut self,
        channel: usize,
        frame_offset: u32,
        registered: bool,
        byte: ParamByte,
    ) -> Option<MidiEvent> {
        // Flush the parameter this select ends, before its state is overwritten.
        let flushed = self.emit_pending(channel, frame_offset);

        let state = &mut self.channels[channel & 0x0F];
        state.registered = registered;
        match byte {
            ParamByte::Bank(v) => state.bank = v,
            ParamByte::Index(v) => state.index = v,
        }
        // Data Entry belongs to the parameter being selected, not the last one.
        state.data_msb = 0;
        state.data_lsb = 0;
        state.pending = false;
        // Null Function (bank and index both 0x7F) deselects rather than naming
        // a parameter — a following Data Entry has nothing to apply to.
        state.selected = !(state.bank == RPN_NULL && state.index == RPN_NULL);

        flushed
    }

    /// Emit the pending parameter if one has actually received Data Entry since
    /// it was selected. Unlike [`Self::emit`], a parameter that was selected but
    /// never given data produces nothing — selecting a parameter and moving on
    /// is not a value change.
    fn emit_pending(&mut self, channel: usize, frame_offset: u32) -> Option<MidiEvent> {
        if !self.channels[channel & 0x0F].pending {
            return None;
        }
        self.channels[channel & 0x0F].pending = false;
        self.emit(channel, frame_offset)
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
        let data32 = super::scaling::midi1_pitch_bend_to_midi2(data14);
        let ch = MidiChannel::new(channel as u8);
        let ev = if state.registered {
            MidiEvent::registered_controller(MidiGroup::FIRST, ch, state.bank, state.index, data32)
        } else {
            MidiEvent::assignable_controller(MidiGroup::FIRST, ch, state.bank, state.index, data32)
        };
        Some(ev.with_frame_offset(frame_offset))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use midi2::channel_voice2::ChannelVoice2;

    /// A MIDI *1.0* wire CC (status 0xB0 | channel) — the translator's input.
    fn cc_ev(channel: u8, control: CCNumber, value: u8) -> MidiEvent {
        // `.get()` is a wire boundary — a MIDI-1 status/data byte triple.
        MidiEvent::from_midi1_bytes(0, &[0xB0 | (channel & 0x0F), control.get(), value])
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
                assert_eq!(CCNumber::new(u8::from(m.control())), cc::MOD_WHEEL);
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

    /// Bank/index of a Registered or Assignable Controller, if that's what `ev` is.
    fn controller_param(ev: &MidiEvent) -> Option<(u8, u8, u32)> {
        match UmpMessage::try_from(ev.data_words()).ok()? {
            UmpMessage::ChannelVoice2(ChannelVoice2::RegisteredController(m)) => {
                Some((u8::from(m.bank()), u8::from(m.index()), m.controller_data()))
            }
            UmpMessage::ChannelVoice2(ChannelVoice2::AssignableController(m)) => {
                Some((u8::from(m.bank()), u8::from(m.index()), m.controller_data()))
            }
            _ => None,
        }
    }

    #[test]
    fn null_function_is_not_translated() {
        // Appendix D.3.3: "RPN/NRPN Null Function, where both the MSB and LSB is
        // set to 0x7F, is not translated." Null closes a transaction, so a Data
        // Entry after it must not synthesize a controller message.
        let mut t = Midi1ToMidi2Translator::new();
        assert!(t.translate(&cc_ev(0, cc::RPN_MSB, 0x7F)).is_none());
        assert!(t.translate(&cc_ev(0, cc::RPN_LSB, 0x7F)).is_none());
        assert!(
            t.translate(&cc_ev(0, cc::DATA_ENTRY, 64)).is_none(),
            "data entry after Null must not emit RPN 0x7F/0x7F"
        );

        // A real parameter after the Null still works.
        assert!(t.translate(&cc_ev(0, cc::RPN_MSB, 0)).is_none());
        assert!(t.translate(&cc_ev(0, cc::RPN_LSB, 2)).is_none());
        let out = t
            .translate(&cc_ev(0, cc::DATA_ENTRY, 64))
            .expect("real parameter emits");
        assert_eq!(controller_param(&out).map(|(b, i, _)| (b, i)), Some((0, 2)));
    }

    #[test]
    fn reselect_does_not_leak_data_into_the_next_parameter() {
        // Appendix D.3.3 names CC 98/99/100/101 as an emit trigger: the previous
        // run ends there. Without that, `data_msb` from parameter A survives into
        // parameter B and the next CC 38 emits a value built from both.
        let mut t = Midi1ToMidi2Translator::new();
        t.translate(&cc_ev(0, cc::RPN_MSB, 0));
        t.translate(&cc_ev(0, cc::RPN_LSB, 2));
        let first = t
            .translate(&cc_ev(0, cc::DATA_ENTRY, 64))
            .expect("parameter A emits");
        assert_eq!(
            controller_param(&first).map(|(b, i, _)| (b, i)),
            Some((0, 2))
        );

        // Selecting parameter B flushes A, then clears the accumulator.
        let flushed = t.translate(&cc_ev(0, cc::RPN_LSB, 1));
        assert_eq!(
            controller_param(flushed.as_ref().expect("select flushes A")).map(|(b, i, _)| (b, i)),
            Some((0, 2)),
            "the flush carries parameter A's identity, not B's"
        );

        // Now B's LSB alone: the value must not contain A's MSB of 64.
        let out = t
            .translate(&cc_ev(0, cc::DATA_ENTRY_LSB, 1))
            .expect("parameter B emits");
        let (bank, index, data) = controller_param(&out).expect("controller");
        assert_eq!((bank, index), (0, 1));
        // B's own 14-bit value: MSB never sent (0), LSB 1 — spelled out so the
        // contrast with A's stale MSB of 64 is visible.
        let (b_msb, b_lsb) = (0u16, 1u16);
        let expected = super::super::scaling::midi1_pitch_bend_to_midi2((b_msb << 7) | b_lsb);
        assert_eq!(
            data, expected,
            "value must be built from B's own data only (stale MSB 64 would give a much larger value)"
        );
    }

    #[test]
    fn a_bare_parameter_select_emits_nothing() {
        // Selecting a parameter and never sending Data Entry is not a value
        // change — the flush must stay silent.
        let mut t = Midi1ToMidi2Translator::new();
        assert!(t.translate(&cc_ev(0, cc::RPN_MSB, 0)).is_none());
        assert!(t.translate(&cc_ev(0, cc::RPN_LSB, 2)).is_none());
        assert!(
            t.translate(&cc_ev(0, cc::RPN_LSB, 1)).is_none(),
            "reselect with no data in flight emits nothing"
        );
    }
}
