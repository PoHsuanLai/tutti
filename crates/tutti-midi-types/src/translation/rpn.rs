//! MIDI 1.0 → MIDI 2.0 translation that needs *state*, per M2-104 Appendix D.
//!
//! [`normalize`](super::normalize) handles the stateless quirks (velocity-0 NoteOn → NoteOff,
//! per-message CV1 → CV2 promotion). Some MIDI 1.0 constructs, though, span
//! *several* messages and can only be translated by accumulating them:
//!
//! - **RPN / NRPN.** MIDI 1.0 expresses a Registered/Non-Registered Parameter as
//!   a run of CCs — select the parameter with CC 101/100 (RPN MSB/LSB) or CC
//!   99/98 (NRPN MSB/LSB), then send the value with CC 6/38 (Data Entry MSB/LSB).
//!   MIDI 2.0 has one dedicated message each (`Voice2::Registered`
//!   / `Voice2::Assignable`). [`Midi1ToMidi2Translator`] watches
//!   the CC stream per channel and, when a Data Entry completes a selected
//!   parameter, emits the single MIDI-2 controller message.
//!
//! A translator is stateful and per-endpoint; feed it every inbound CV1 packet.
//! Packets that aren't part of an (N)RPN run are promoted straight through via
//! [`normalize`](super::normalize), so a caller can treat it as "MIDI-1 in, MIDI-2 out".

use midi2::channel_voice1::ChannelVoice1;
use midi2::{Channeled, UmpMessage};
use tutti_ump::{Address, CCNumber, Message, MidiChannel, MidiGroup, U7, Ump, Voice2};

use crate::cc;

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
    /// and whether each has arrived since the last select.
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
const BANK_SELECT_LSB: CCNumber = CCNumber::of(32);

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
    #[must_use]
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
    pub fn translate(&mut self, ump: &Ump) -> Option<Ump> {
        let Ok(UmpMessage::ChannelVoice1(cv1)) = UmpMessage::try_from(ump.words()) else {
            // Not a MIDI-1 channel message — nothing to accumulate.
            return Some(super::normalize(ump));
        };
        let ChannelVoice1::ControlChange(m) = cv1 else {
            if let ChannelVoice1::ProgramChange(m) = cv1 {
                return Some(self.program_change(
                    ump,
                    u8::from(m.channel()),
                    u8::from(m.program()),
                ));
            }
            return Some(super::normalize(ump));
        };
        let channel = u8::from(m.channel()) as usize;
        // Wire boundary: midi2's `u7` is already in range, so the mask is a no-op.
        let control = CCNumber::masked(u8::from(m.control()));
        let value = u8::from(m.control_data());
        let state = &mut self.channels[channel & 0x0F];

        match control {
            cc::RPN_MSB => self.select(channel, true, ParamByte::Bank(value)),
            cc::RPN_LSB => self.select(channel, true, ParamByte::Index(value)),
            cc::NRPN_MSB => self.select(channel, false, ParamByte::Bank(value)),
            cc::NRPN_LSB => self.select(channel, false, ParamByte::Index(value)),
            cc::DATA_ENTRY => {
                state.data_msb = value;
                state.msb_set = true;
                self.emit(channel)
            }
            cc::DATA_ENTRY_LSB => {
                state.data_lsb = value;
                state.lsb_set = true;
                if state.msb_set {
                    self.emit(channel)
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
            _ => Some(super::normalize(ump)),
        }
    }

    /// A Program Change, carrying the bank latched on its channel if one was
    /// selected since the last Program Change (`ump_convert.c` 427-431: the
    /// flag is cleared, the MSB and LSB values are not).
    fn program_change(&mut self, ump: &Ump, channel: u8, program: u8) -> Ump {
        let state = &mut self.channels[usize::from(channel & 0x0F)];
        let bank = state
            .bank_set
            .then(|| (u16::from(state.bank_msb) << 7) | u16::from(state.bank_lsb));
        state.bank_set = false;
        // M2-104 Figure 64: Bank Valid is set exactly when a bank rides along.
        let bank = bank.map(|b| tutti_ump::Bank {
            msb: U7::new(((b >> 7) & 0x7F) as u8).unwrap_or_default(),
            lsb: U7::new((b & 0x7F) as u8).unwrap_or_default(),
        });
        Message::Midi2(
            Address {
                group: MidiGroup::masked(((ump.0[0] >> 24) & 0x0F) as u8),
                channel: MidiChannel::masked(channel),
            },
            Voice2::ProgramChange {
                program: U7::new(program & 0x7F).unwrap_or_default(),
                bank,
            },
        )
        .into()
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
    fn select(&mut self, channel: usize, registered: bool, byte: ParamByte) -> Option<Ump> {
        let state = &self.channels[channel & 0x0F];
        let flushed = if state.lsb_set && !state.msb_set {
            self.emit(channel)
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
    fn emit(&self, channel: usize) -> Option<Ump> {
        let state = &self.channels[channel & 0x0F];
        if !state.selected {
            return None;
        }
        // Widen the 14-bit MIDI-1 Data Entry (MSB<<7 | LSB) to 32 bits via the
        // spec Min-Center-Max upscale, so a MIDI-1 RPN value lands where its
        // MIDI-2 equivalent would.
        let data14 = ((state.data_msb as u16) << 7) | (state.data_lsb as u16);
        let data32 = super::scaling::midi1_pitch_bend_to_midi2(data14);
        let address = Address {
            group: MidiGroup::FIRST,
            channel: MidiChannel::masked(channel as u8),
        };
        let (n, registered) = if state.registered {
            (state.rpn, true)
        } else {
            (state.nrpn, false)
        };
        let bank = U7::new(n.bank & 0x7F).unwrap_or_default();
        let index = U7::new(n.index & 0x7F).unwrap_or_default();
        let voice = if registered {
            Voice2::Registered {
                bank,
                index,
                value: data32,
            }
        } else {
            Voice2::Assignable {
                bank,
                index,
                value: data32,
            }
        };
        Some(Message::Midi2(address, voice).into())
    }
}

#[cfg(test)]
mod tests;
