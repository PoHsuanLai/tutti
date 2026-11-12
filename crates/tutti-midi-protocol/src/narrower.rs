//! The stateful MIDI 2.0 to MIDI 1.0 translation of M2-104 Appendix D.2: the
//! [`Narrower`] an output edge runs.

use crate::scale;
use crate::{midi1, narrow_bend};
use tutti_ump::{
    Address, Bank, CCNumber, Message, Midi1Bytes, MidiChannel, MidiGroup, U7, Ump, Voice1, Voice2,
};

/// Every channel of every group.
const SLOTS: usize = MidiGroup::COUNT as usize * MidiChannel::COUNT as usize;

/// The parameter a channel's receiver has selected, as this narrower last left
/// it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Selected {
    Registered { bank: U7, index: U7 },
    Assignable { bank: U7, index: U7 },
}

/// One channel's output state. `None` is "not known": the receiver may have
/// anything, so the next message sends the select or the bank in full.
#[derive(Clone, Copy, Debug, Default)]
struct Channel {
    selected: Option<Selected>,
    bank: Option<Bank>,
}

/// MIDI 2.0 in, MIDI 1.0 out: the translation an output edge runs, per group
/// and channel (M2-104 Appendix D.2). The inverse of [`Translator`](crate::Translator).
///
/// - **A Registered or Assignable Controller** is the four Control Changes of
///   D.2.3: "each message generates a sequence of four MIDI 1.0 Protocol
///   messages", CC 101 and 100 (RPN) or CC 99 and 98 (NRPN) selecting the
///   parameter, then CC 6 and CC 38, the top 14 bits of the 32-bit value. The
///   select is **skipped when the same parameter is already selected** on that
///   group and channel (MIDI 1.0 rule 6: a selected parameter stays selected
///   "until a new Parameter Number is chosen"), so a stream of values for one
///   parameter is a CC 6 and a CC 38 each.
/// - **The data narrows** by the rule the [`Translator`](crate::Translator) widened with. A
///   Registered Controller of bank 0 and index 0 to 31 is a fixed-point number
///   or a pair of fields, widened by zero-extension (M2-115 section 4.1:
///   "Translators shall use Zero-Extension Scaling on all Registered
///   Controller (RPN) Messages where the index (RPN LSB) is 0-31"), and
///   narrows **with rounding** (section 4.4: "When downscaling fixed-point
///   values, they should be rounded in the reduced resolution": add half the
///   scaled range, shift, clamp to 14 bits). Every other controller was scaled
///   Min-Center-Max and narrows by the plain right shift of section 3.4
///   (`narrow_value`), which returns the value it was widened from.
/// - **Null Function is not sent after a run.** D.2.3 gives four messages, not
///   five, and D.3.3 says the Null Function "is not translated"; the select
///   stays until the next one, which is what lets the select be skipped. A
///   Reset All Controllers (CC 121, RP-015) that passes through nulls the
///   receiver's selection, and this narrower forgets it on seeing one.
/// - **A Program Change with a bank** is CC 0, CC 32, then the Program Change
///   (D.2.4), but the bank is **kept across Program Changes**: the two Bank
///   Select messages are sent only when the bank differs from the one last
///   sent on the channel (MIDI 1.0 bank state is the receiver's, and a later
///   Program Change without a bank keeps it).
/// - **Every other message** is [`midi1()`](fn@midi1)'s: a Note On whose velocity narrows
///   to 0 is 1 (D.2.1), pressure and Control Change shift, and the messages
///   with no MIDI 1.0 form (D.2.8: relative and per-note controllers) send
///   nothing.
///
/// A Control Change that passes through and moves what this narrower tracks
/// (CC 101, 100, 99, 98, CC 0 or 32, or CC 121) makes it forget that state, so
/// the next controller selects in full. A receiver this narrower does not own
/// the whole stream of (a plugin that was reset, a device just opened) needs
/// [`reset`](Self::reset) too.
///
/// It allocates nothing and holds no clock. One narrower serves one output
/// for its life: two outputs through one would skip a select the second
/// receiver never saw.
///
/// ```
/// use tutti_midi_protocol::Narrower;
/// use tutti_ump::{Address, Message, MidiChannel, MidiGroup, U7, Voice2};
///
/// let address = Address { group: MidiGroup::FIRST, channel: MidiChannel::FIRST };
/// // Pitch bend sensitivity (RPN 0/0), 12 semitones, zero-extended.
/// let bend_range = Message::Midi2(address, Voice2::Registered {
///     bank: U7::of(0),
///     index: U7::of(0),
///     value: 12 << 25,
/// });
/// let mut narrower = Narrower::new();
/// let mut bytes = Vec::new();
/// narrower.feed(bend_range, |b| bytes.push(b.as_slice().to_vec()));
/// // CC 101, CC 100, CC 6 (12), CC 38 (0).
/// assert_eq!(bytes, [[0xB0, 101, 0], [0xB0, 100, 0], [0xB0, 6, 12], [0xB0, 38, 0]]);
/// ```
#[derive(Clone, Debug)]
pub struct Narrower {
    channels: [Channel; SLOTS],
}

impl Default for Narrower {
    fn default() -> Self {
        Self::new()
    }
}

impl Narrower {
    /// A narrower that knows nothing of any receiver: the first controller or
    /// bank on each channel is sent in full.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            channels: [Channel {
                selected: None,
                bank: None,
            }; SLOTS],
        }
    }

    /// Forgets every selection and bank, as at [`new`](Self::new): the next
    /// controller or bank sends its select or Bank Select again. Call it when
    /// the receiver may have lost its state (it was reset, or reopened).
    pub fn reset(&mut self) {
        *self = Self::new();
    }

    /// Calls `out` with each MIDI 1.0 message `message` stands for, in the
    /// order to send them: up to four for a controller, three for a Program
    /// Change with a bank, one for most others, none for one with no MIDI 1.0
    /// form (D.2.8) or that is not a channel or system message.
    pub fn feed(&mut self, packet: impl Into<Ump>, mut out: impl FnMut(Midi1Bytes)) {
        // A packet that does not decode has no MIDI 1.0 form.
        let Ok(message) = packet.into().decode() else {
            return;
        };
        let (address, voice) = match message {
            Message::Midi2(address, voice) => (address, voice),
            Message::Midi1(address, voice) => {
                self.observe(address, voice);
                midi1(message).for_each(&mut out);
                return;
            }
            other => {
                midi1(other).for_each(&mut out);
                return;
            }
        };
        let channel = &mut self.channels[address.index()];
        let mut cc = |controller: CCNumber, value: U7| {
            out(Voice1::ControlChange { controller, value }.bytes(address.channel));
        };
        match voice {
            Voice2::Registered { bank, index, value } => {
                let selected = Selected::Registered { bank, index };
                let data = if scale::zero_extended(bank, index) {
                    narrow_rounded(value)
                } else {
                    narrow_bend(value).get()
                };
                run(channel, selected, data, &mut cc);
            }
            Voice2::Assignable { bank, index, value } => {
                let selected = Selected::Assignable { bank, index };
                run(channel, selected, narrow_bend(value).get(), &mut cc);
            }
            Voice2::ProgramChange {
                program,
                bank: Some(bank),
            } => {
                if channel.bank != Some(bank) {
                    channel.bank = Some(bank);
                    cc(CCNumber::BANK_SELECT, bank.msb);
                    cc(CCNumber::BANK_SELECT_LSB, bank.lsb);
                }
                out(Voice1::ProgramChange(program).bytes(address.channel));
            }
            Voice2::ControlChange { controller, .. } => {
                forget(channel, controller);
                midi1(message).for_each(&mut out);
            }
            _ => midi1(message).for_each(&mut out),
        }
    }

    /// A MIDI 1.0 Control Change that goes by moves the receiver's state.
    fn observe(&mut self, address: Address, voice: Voice1) {
        if let Voice1::ControlChange { controller, .. } = voice {
            forget(&mut self.channels[address.index()], controller);
        }
    }
}

/// A Control Change that moved the select or the bank behind this narrower's
/// back: the next controller or bank is sent in full.
fn forget(channel: &mut Channel, controller: CCNumber) {
    match controller {
        CCNumber::RPN_MSB
        | CCNumber::RPN_LSB
        | CCNumber::NRPN_MSB
        | CCNumber::NRPN_LSB
        | CCNumber::RESET_ALL => channel.selected = None,
        CCNumber::BANK_SELECT | CCNumber::BANK_SELECT_LSB => channel.bank = None,
        _ => {}
    }
}

/// One controller: the select unless it is already selected, then the data.
fn run(channel: &mut Channel, selected: Selected, data: u16, cc: &mut impl FnMut(CCNumber, U7)) {
    if channel.selected != Some(selected) {
        channel.selected = Some(selected);
        let (bank, index, msb, lsb) = match selected {
            Selected::Registered { bank, index } => {
                (bank, index, CCNumber::RPN_MSB, CCNumber::RPN_LSB)
            }
            Selected::Assignable { bank, index } => {
                (bank, index, CCNumber::NRPN_MSB, CCNumber::NRPN_LSB)
            }
        };
        cc(msb, bank);
        cc(lsb, index);
    }
    cc(CCNumber::DATA_ENTRY, U7::masked((data >> 7) as u8));
    cc(CCNumber::DATA_ENTRY_LSB, U7::masked((data & 0x7F) as u8));
}

/// A 32-bit value as 14 bits, rounded (M2-115 section 4.4.1,
/// `scaleDownRounding(value, 32, 14)`): add half the scaled range, shift,
/// clamp to the 14-bit maximum.
fn narrow_rounded(value: u32) -> u16 {
    const SCALE_BITS: u32 = 32 - 14;
    let shifted = (u64::from(value) + (1 << (SCALE_BITS - 1))) >> SCALE_BITS;
    // `min` keeps 14 bits.
    shifted.min(0x3FFF) as u16
}
