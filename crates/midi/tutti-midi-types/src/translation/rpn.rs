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

/// The registered (RPN) or non-registered (NRPN) parameter last selected.
#[derive(Clone, Copy, Debug, Default)]
struct ParamNumber {
    /// Selected parameter MSB / LSB (CC 101/100 for RPN, 99/98 for NRPN).
    bank: u8,
    index: u8,
}

impl ParamNumber {
    fn is_null(self) -> bool {
        self.bank == RPN_NULL && self.index == RPN_NULL
    }
}

/// Per-channel accumulator for an in-progress (N)RPN run, and the latched bank
/// for the next Program Change.
#[derive(Clone, Copy, Debug, Default)]
struct ParamState {
    /// RPN and NRPN keep their own numbers: selecting half of one never reads
    /// the other's.
    rpn: ParamNumber,
    nrpn: ParamNumber,
    /// Data Entry MSB / LSB (CC 6 / 38) accumulated for the current parameter,
    /// and whether each has arrived since the last emit.
    data_msb: u8,
    data_lsb: u8,
    msb_set: bool,
    lsb_set: bool,
    /// Whether the currently-selected parameter is registered (RPN) or not (NRPN).
    registered: bool,
    /// Whether a parameter has been selected (so a Data Entry is meaningful).
    selected: bool,
    /// The last Bank Select MSB / LSB (CC 0 / 32), kept across Program Changes;
    /// `bank_set` is whether either arrived since the last Program Change.
    bank_msb: u8,
    bank_lsb: u8,
    bank_set: bool,
}

/// The 0x7F value that, in *both* the MSB and LSB, forms the RPN/NRPN Null
/// Function — the idiom for closing an (N)RPN transaction.
const RPN_NULL: u8 = 0x7F;

/// Bank Select LSB (CC 32).
const BANK_SELECT_LSB: CCNumber = CCNumber::new(32);

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
    /// Builds a translator with every channel's accumulators empty.
    ///
    /// The state is per channel and persists across events, so one translator
    /// must serve one MIDI stream for its whole life — sharing it between two
    /// sources interleaves their Data Entry runs and mixes their parameters.
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds one inbound event. Returns the MIDI-2 event it translates to:
    /// - a Data Entry on a selected (N)RPN yields one Registered/Assignable
    ///   Controller message with the value so far: CC 6 emits at once with
    ///   the LSB as last sent (0 if never), and a later CC 38 emits the
    ///   refined value. A CC 38 with no CC 6 before it emits nothing: its
    ///   value would have an MSB that never arrived. If a select ends such a
    ///   run, it is emitted then, MSB 0 (`ump_convert.c` flushes a partial run
    ///   the same way; it also waits for both halves before the first emit,
    ///   which this does not, so a CC 6 alone is heard without waiting for the
    ///   next select);
    /// - a parameter select, or a Data Entry with none selected, is *absorbed*
    ///   (returns `None`: it only updates state);
    /// - Bank Select (CC 0 / 32) is absorbed and attached to the channel's next
    ///   Program Change, whose bank is then valid. As in `ump_convert.c`
    ///   (408-413, 427-431), the last MSB and LSB are kept across Program
    ///   Changes, so a later lone CC 32 pairs with the earlier CC 0, and a lone
    ///   half pairs with the other as last sent (0 if never);
    /// - anything else, Data Increment and Decrement included (`ump_convert.c`
    ///   passes them through as CCs), is promoted through
    ///   [`normalize`](super::normalize).
    pub fn translate(&mut self, event: &MidiEvent) -> Option<MidiEvent> {
        let Ok(UmpMessage::ChannelVoice1(cv1)) = UmpMessage::try_from(event.data_words()) else {
            // Not a MIDI-1 channel message — nothing to accumulate.
            return Some(super::normalize(event));
        };
        let ChannelVoice1::ControlChange(m) = cv1 else {
            if let ChannelVoice1::ProgramChange(m) = cv1 {
                return Some(self.program_change(
                    event,
                    u8::from(m.channel()),
                    u8::from(m.program()),
                ));
            }
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
                state.msb_set = true;
                self.emit(channel, event.frame_offset)
            }
            cc::DATA_ENTRY_LSB => {
                state.data_lsb = value;
                state.lsb_set = true;
                if state.msb_set {
                    self.emit(channel, event.frame_offset)
                } else {
                    None
                }
            }
            // `ump_convert.c` 408-413: latch and absorb.
            cc::BANK_SELECT => {
                state.bank_msb = value;
                state.bank_set = true;
                None
            }
            BANK_SELECT_LSB => {
                state.bank_lsb = value;
                state.bank_set = true;
                None
            }
            // A plain CC (not part of an (N)RPN run) promotes normally.
            _ => Some(super::normalize(event)),
        }
    }

    /// A Program Change, carrying the bank latched on its channel if one was
    /// selected since the last Program Change (`ump_convert.c` 427-431: the
    /// flag is cleared, the MSB and LSB values are not).
    fn program_change(&mut self, event: &MidiEvent, channel: u8, program: u8) -> MidiEvent {
        let state = &mut self.channels[usize::from(channel & 0x0F)];
        let bank = state
            .bank_set
            .then(|| (u16::from(state.bank_msb) << 7) | u16::from(state.bank_lsb));
        state.bank_set = false;
        MidiEvent::program_change(event.group(), MidiChannel::new(channel), program, bank)
            .with_frame_offset(event.frame_offset)
    }

    /// Handle one parameter-select byte (CC 98/99/100/101).
    ///
    /// Appendix D.3.3 lists a parameter select as an *emit trigger*: "a CC 98,
    /// 99, 100, and 101 is received, indicating the last RPN/NRPN message has
    /// ended and a new one has started." A run with only a CC 38 is emitted
    /// here (every other value went out with its Data Entry), before the new
    /// number is adopted; the accumulated data is cleared so it never leaks
    /// into the next parameter.
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
        let state = &self.channels[channel & 0x0F];
        let flushed = if state.lsb_set && !state.msb_set {
            self.emit(channel, frame_offset)
        } else {
            None
        };
        let state = &mut self.channels[channel & 0x0F];
        state.registered = registered;
        let number = if registered {
            &mut state.rpn
        } else {
            &mut state.nrpn
        };
        match byte {
            ParamByte::Bank(v) => number.bank = v,
            ParamByte::Index(v) => number.index = v,
        }
        let null = number.is_null();
        state.data_msb = 0;
        state.data_lsb = 0;
        state.msb_set = false;
        state.lsb_set = false;
        state.selected = !null;
        flushed
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
            let n = state.rpn;
            MidiEvent::registered_controller(MidiGroup::FIRST, ch, n.bank, n.index, data32)
        } else {
            let n = state.nrpn;
            MidiEvent::assignable_controller(MidiGroup::FIRST, ch, n.bank, n.index, data32)
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

    // Each Data Entry already emitted its value, so the select that ends a run
    // has nothing to flush: the earlier version of this test expected a
    // second, identical message for parameter A there, a duplicate of the one
    // the Data Entry sent.
    //
    // Mutation: stop clearing `data_msb` in `select` and B's value carries A's
    // MSB of 64 → fails. Re-emit on select and `flushed` is `Some` → fails.
    #[test]
    fn reselect_emits_nothing_and_does_not_leak_data_into_the_next_parameter() {
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

        assert!(
            t.translate(&cc_ev(0, cc::RPN_LSB, 1)).is_none(),
            "A was emitted by its Data Entry; the select adds nothing"
        );

        // B's LSB waits for its MSB; B's MSB of 0 then completes a value that
        // must not contain A's MSB of 64.
        assert!(t.translate(&cc_ev(0, cc::DATA_ENTRY_LSB, 1)).is_none());
        let out = t
            .translate(&cc_ev(0, cc::DATA_ENTRY, 0))
            .expect("parameter B emits");
        let (bank, index, data) = controller_param(&out).expect("controller");
        assert_eq!((bank, index), (0, 1));
        let expected = super::super::scaling::midi1_pitch_bend_to_midi2(1);
        assert_eq!(data, expected, "value is built from B's own data only");
    }

    // The whole run of the old duplicate probe: CC101=0, CC100=0, CC6=12,
    // CC38=0, CC101=127 gives one event per Data Entry (two), none on select.
    // Mutation: emit on select when data was seen → a third event.
    #[test]
    fn a_completed_value_is_emitted_once_per_data_entry() {
        let mut t = Midi1ToMidi2Translator::new();
        let mut n = 0;
        for (c, v) in [
            (cc::RPN_MSB, 0),
            (cc::RPN_LSB, 0),
            (cc::DATA_ENTRY, 12),
            (cc::DATA_ENTRY_LSB, 0),
            (cc::RPN_MSB, 127),
        ] {
            n += usize::from(t.translate(&cc_ev(0, c, v)).is_some());
        }
        assert_eq!(n, 2);
    }

    // A CC 38 before any CC 6 is not a value yet: no transient MSB-0 event.
    // Mutation: emit on CC 38 regardless of `msb_set` and the first assert fails.
    #[test]
    fn a_data_entry_lsb_before_its_msb_waits() {
        let mut t = Midi1ToMidi2Translator::new();
        t.translate(&cc_ev(0, cc::RPN_MSB, 0));
        t.translate(&cc_ev(0, cc::RPN_LSB, 2));
        assert!(t.translate(&cc_ev(0, cc::DATA_ENTRY_LSB, 5)).is_none());
        let out = t.translate(&cc_ev(0, cc::DATA_ENTRY, 64)).expect("emits");
        let want = super::super::scaling::midi1_pitch_bend_to_midi2((64 << 7) | 5);
        assert_eq!(controller_param(&out).map(|(_, _, d)| d), Some(want));
        // A run left with only the LSB is emitted when the next select ends it.
        let mut t = Midi1ToMidi2Translator::new();
        t.translate(&cc_ev(0, cc::RPN_MSB, 0));
        t.translate(&cc_ev(0, cc::RPN_LSB, 2));
        t.translate(&cc_ev(0, cc::DATA_ENTRY_LSB, 5));
        let flushed = t.translate(&cc_ev(0, cc::RPN_MSB, 127)).expect("flushed");
        let want = super::super::scaling::midi1_pitch_bend_to_midi2(5);
        assert_eq!(controller_param(&flushed), Some((0, 2, want)));
    }

    // After RPN 0/6, a lone NRPN MSB must not take the RPN's LSB.
    // Mutation: share one bank/index pair between RPN and NRPN → index 6.
    #[test]
    fn rpn_and_nrpn_keep_separate_numbers() {
        let mut t = Midi1ToMidi2Translator::new();
        t.translate(&cc_ev(0, cc::RPN_MSB, 0));
        t.translate(&cc_ev(0, cc::RPN_LSB, 6));
        t.translate(&cc_ev(0, cc::NRPN_MSB, 5));
        let out = t.translate(&cc_ev(0, cc::DATA_ENTRY, 1)).expect("emits");
        assert_eq!(controller_param(&out).map(|(b, i, _)| (b, i)), Some((5, 0)));
        // And the RPN number is intact when re-selected by one half.
        t.translate(&cc_ev(0, cc::RPN_MSB, 0));
        let out = t.translate(&cc_ev(0, cc::DATA_ENTRY, 1)).expect("emits");
        assert_eq!(controller_param(&out).map(|(b, i, _)| (b, i)), Some((0, 6)));
    }

    // Mutation: do not `take` the latch and the second Program Change keeps the
    // bank; do not latch and the first has none; use FIRST for the channel and
    // the ch 5 assert fails.
    #[test]
    fn bank_select_attaches_to_the_next_program_change_only() {
        let mut t = Midi1ToMidi2Translator::new();
        let pc = |ch: u8, p: u8| MidiEvent::from_midi1_bytes(0, &[0xC0 | ch, p]).unwrap();
        assert!(t.translate(&cc_ev(5, cc::BANK_SELECT, 3)).is_none());
        // Another channel's program change does not take it.
        let other = t.translate(&pc(1, 7)).unwrap();
        assert_eq!(other.data_words(), pc_words(1, 7, None));
        // MSB alone: the LSB half is 0.
        let with = t.translate(&pc(5, 8)).unwrap();
        assert_eq!(with.data_words(), pc_words(5, 8, Some(3 << 7)));
        let after = t.translate(&pc(5, 9)).unwrap();
        assert_eq!(after.data_words(), pc_words(5, 9, None));
        // The values are kept: a lone LSB pairs with the earlier MSB
        // (`ump_convert.c` clears only the flag). Mutation: clear the
        // values on a Program Change and this reads bank 6.
        assert!(t.translate(&cc_ev(5, CCNumber::new(32), 6)).is_none());
        let again = t.translate(&pc(5, 10)).unwrap();
        assert_eq!(again.data_words(), pc_words(5, 10, Some((3 << 7) | 6)));
    }

    fn pc_words(ch: u8, p: u8, bank: Option<u16>) -> Vec<u32> {
        MidiEvent::program_change(MidiGroup::FIRST, MidiChannel::new(ch), p, bank)
            .data_words()
            .to_vec()
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
