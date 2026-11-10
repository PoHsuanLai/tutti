//! The stateful MIDI 1.0 to MIDI 2.0 translation of M2-104 Appendix D.3: the
//! [`Translator`] an input edge runs once.

use crate::scale;
use crate::voice::widen;
use tutti_ump::{Bank, CCNumber, Message, MidiChannel, MidiGroup, U7, Ump, Voice1, Voice2};

/// Bank Select MSB and LSB.
const BANK_MSB: u8 = 0;
const BANK_LSB: u8 = 32;
/// Data Entry MSB and LSB.
const DATA_MSB: u8 = 6;
const DATA_LSB: u8 = 38;
/// The parameter selects: NRPN LSB and MSB, RPN LSB and MSB.
const NRPN_LSB: u8 = 98;
const NRPN_MSB: u8 = 99;
const RPN_LSB: u8 = 100;
const RPN_MSB: u8 = 101;
/// Reset All Controllers.
const RESET_ALL: u8 = 121;
/// The byte that, in both halves of a parameter number, is the Null Function.
const NULL: u8 = 0x7F;
/// The registered controllers in bank 0 whose data is a pair of 7-bit fields,
/// not one 14-bit number: their data is zero-extended, not scaled (M2-115
/// section 4.1).
const ZERO_EXTENDED_INDICES: u8 = 32;

/// Every channel of every group.
const SLOTS: usize = MidiGroup::COUNT as usize * MidiChannel::COUNT as usize;

/// A parameter number, both halves.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Number {
    bank: U7,
    index: U7,
}

impl Number {
    fn is_null(self) -> bool {
        self.bank.get() == NULL && self.index.get() == NULL
    }
}

/// Which kind of parameter the channel last selected.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Kind {
    /// None selected, or the Null Function.
    #[default]
    None,
    Registered,
    Assignable,
}

/// One channel's assembly state.
#[derive(Clone, Copy, Debug, Default)]
struct Channel {
    /// The registered and the assignable number, each kept whole: a select of
    /// one half keeps the other half as last selected.
    rpn: Number,
    nrpn: Number,
    kind: Kind,
    /// The Data Entry MSB of the selected parameter, once one has arrived.
    msb: Option<U7>,
    /// A Data Entry MSB that has not been sent yet.
    pending: bool,
    /// A Data Entry LSB that arrived before any MSB of this parameter.
    early_lsb: Option<U7>,
    /// The Bank Select halves as last sent, kept across Program Changes.
    bank_msb: U7,
    bank_lsb: U7,
    /// Whether a Bank Select arrived since the last Program Change.
    bank_set: bool,
}

/// MIDI 1.0 in, MIDI 2.0 out: the translation an input edge runs once, so
/// what follows it carries MIDI 2.0 channel voice messages only (design 017
/// §4).
///
/// State is kept per group and channel, and the group is kept. What it does
/// (M2-104 Appendix D.3):
///
/// - **A stateless message** is [`widen`]ed: velocity, pressure, Control
///   Change and Pitch Bend values scaled Min-Center-Max (M2-115 section 3),
///   and a Note On at velocity 0 is a Note Off at release velocity `0x8000`.
/// - **An RPN or NRPN run** (CC 101 and 100, or 99 and 98, then Data Entry CC
///   6 and 38) is one Registered or Assignable Controller. The parameter
///   selects and the Data Entry are absorbed; the controller is sent on the
///   triggers of D.3.3 only: a CC 38, a later CC 6, or a new parameter select,
///   so a receiver never sees a value built on an LSB from an earlier run. A
///   CC 6 waits for one of them, with an LSB of 0 if none follows; a CC 38
///   before any CC 6 waits for the CC 6 and is sent with it. The data is the
///   14-bit `MSB << 7 | LSB`, zero-extended to 32 bits for registered
///   controllers 0 to 31 of bank 0 (M2-115 section 4.1: their halves are two
///   fields, pitch bend sensitivity's semitones and cents for one) and
///   scaled Min-Center-Max for every other. A select of one half keeps the
///   other half as last selected, separately for RPN and NRPN. The Null
///   Function (`7F 7F`) ends the run and is not translated: Data Entry after
///   it is dropped. So does Reset All Controllers (CC 121, RP-015), which
///   passes on as a Control Change and drops what the run had not sent.
/// - **Bank Select** (CC 0 and 32) is absorbed and rides on the channel's next
///   Program Change as its bank. The halves are kept across Program Changes,
///   so a lone CC 32 pairs with the CC 0 last sent (0 if never); a Program
///   Change with no Bank Select since the one before carries no bank.
/// - **Every other packet** passes through unchanged: a MIDI 2.0 message (a
///   Note On at velocity 0 stays a Note On), system, SysEx, Flex Data, UMP
///   Stream, utility, and a packet no definition covers. Data Increment and
///   Decrement (CC 96 and 97) stay Control Changes.
///
/// It allocates nothing and holds no clock. One translator serves one input
/// stream for its life: two streams through one would interleave their runs.
///
/// ```
/// use tutti_midi_protocol::Translator;
/// use tutti_ump::{Address, CCNumber, Message, MidiChannel, MidiGroup, U7, Ump, Voice1, Voice2};
///
/// let address = Address { group: MidiGroup::FIRST, channel: MidiChannel::FIRST };
/// let cc = |controller: u8, value: u8| {
///     Ump::from(Message::Midi1(address, Voice1::ControlChange {
///         controller: CCNumber::new(controller).unwrap(),
///         value: U7::new(value).unwrap(),
///     }))
/// };
/// let mut translator = Translator::new();
/// let mut out = Vec::new();
/// // Pitch bend sensitivity, 12 semitones: RPN 0/0, Data Entry 12 then 0.
/// for ump in [cc(101, 0), cc(100, 0), cc(6, 12), cc(38, 0)] {
///     translator.feed(ump, |u| out.push(u));
/// }
/// assert_eq!(out.len(), 1);
/// let Ok(Message::Midi2(_, Voice2::Registered { value, .. })) = tutti_ump::decode(out[0]) else {
///     unreachable!()
/// };
/// // Zero-extended: the semitones are the top 7 bits.
/// assert_eq!(value, 12 << 25);
/// ```
#[derive(Clone, Debug)]
pub struct Translator {
    channels: [Channel; SLOTS],
}

impl Default for Translator {
    fn default() -> Self {
        Self::new()
    }
}

impl Translator {
    /// A translator with nothing selected on any channel.
    #[must_use]
    pub const fn new() -> Self {
        const ZERO: U7 = match U7::new(0) {
            Some(zero) => zero,
            None => panic!("0 is a 7-bit value"),
        };
        const EMPTY: Channel = Channel {
            rpn: Number {
                bank: ZERO,
                index: ZERO,
            },
            nrpn: Number {
                bank: ZERO,
                index: ZERO,
            },
            kind: Kind::None,
            msb: None,
            pending: false,
            early_lsb: None,
            bank_msb: ZERO,
            bank_lsb: ZERO,
            bank_set: false,
        };
        Self {
            channels: [EMPTY; SLOTS],
        }
    }

    /// Forgets every selection, Data Entry and Bank Select, as at
    /// [`new`](Self::new). Nothing pending is sent.
    pub fn reset(&mut self) {
        *self = Self::new();
    }

    /// Translates one packet, calling `out` for the packet it becomes: not at
    /// all for an absorbed select, Data Entry or Bank Select that ends no run,
    /// and once otherwise (a select or a CC 6 that ends a run sends that run's
    /// controller, and is itself absorbed). Never more than once: a clip's
    /// loader puts the result in its source's place.
    pub fn feed(&mut self, ump: Ump, mut out: impl FnMut(Ump)) {
        let Ok(Message::Midi1(address, voice)) = tutti_ump::decode(ump) else {
            out(ump);
            return;
        };
        let slot = usize::from(address.group.get()) * usize::from(MidiChannel::COUNT)
            + usize::from(address.channel.get());
        let channel = &mut self.channels[slot];
        let mut send = |voice: Voice2| out(Message::Midi2(address, voice).into());
        match voice {
            Voice1::ControlChange { controller, value } => {
                controller_change(channel, controller, value, &mut send);
            }
            Voice1::ProgramChange(program) => {
                let bank = channel.bank_set.then_some(Bank {
                    msb: channel.bank_msb,
                    lsb: channel.bank_lsb,
                });
                channel.bank_set = false;
                send(Voice2::ProgramChange { program, bank });
            }
            other => send(widen(other)),
        }
    }
}

/// A Control Change on `channel`: absorbed into a run or a bank, or widened.
fn controller_change(
    channel: &mut Channel,
    controller: CCNumber,
    value: U7,
    send: &mut impl FnMut(Voice2),
) {
    match controller.get() {
        RPN_MSB | RPN_LSB | NRPN_MSB | NRPN_LSB => {
            flush(channel, send);
            let registered = matches!(controller.get(), RPN_MSB | RPN_LSB);
            let number = if registered {
                &mut channel.rpn
            } else {
                &mut channel.nrpn
            };
            if matches!(controller.get(), RPN_MSB | NRPN_MSB) {
                number.bank = value;
            } else {
                number.index = value;
            }
            channel.kind = match (number.is_null(), registered) {
                (true, _) => Kind::None,
                (false, true) => Kind::Registered,
                (false, false) => Kind::Assignable,
            };
            channel.msb = None;
            channel.pending = false;
            channel.early_lsb = None;
        }
        DATA_MSB if channel.kind != Kind::None => {
            // A later CC 6 ends the run before it.
            if channel.pending {
                emit(channel, None, send);
            }
            channel.msb = Some(value);
            match channel.early_lsb.take() {
                Some(lsb) => emit(channel, Some(lsb), send),
                None => channel.pending = true,
            }
        }
        DATA_LSB if channel.kind != Kind::None => {
            if channel.msb.is_some() {
                emit(channel, Some(value), send);
            } else {
                channel.early_lsb = Some(value);
            }
        }
        // Data Entry with nothing selected, or after the Null Function.
        DATA_MSB | DATA_LSB => {}
        // RP-015: Reset All Controllers sets the RPN and NRPN to null. What a
        // run had not sent is dropped with it (sending it as well would make
        // two packets of one); the message itself goes on.
        RESET_ALL => {
            channel.kind = Kind::None;
            channel.msb = None;
            channel.pending = false;
            channel.early_lsb = None;
            send(widen(Voice1::ControlChange { controller, value }));
        }
        BANK_MSB => {
            channel.bank_msb = value;
            channel.bank_set = true;
        }
        BANK_LSB => {
            channel.bank_lsb = value;
            channel.bank_set = true;
        }
        _ => send(widen(Voice1::ControlChange { controller, value })),
    }
}

/// Sends what a run that is ending has not sent: a CC 6 with no CC 38 after
/// it (LSB 0), or a CC 38 that never had a CC 6 (MSB 0).
fn flush(channel: &mut Channel, send: &mut impl FnMut(Voice2)) {
    if channel.kind == Kind::None {
        return;
    }
    if channel.pending {
        emit(channel, None, send);
    } else if let Some(lsb) = channel.early_lsb.take() {
        emit(channel, Some(lsb), send);
    }
}

/// Sends the selected parameter at the channel's MSB and `lsb`.
///
/// An MSB that never arrived is 0.
fn emit(channel: &mut Channel, lsb: Option<U7>, send: &mut impl FnMut(Voice2)) {
    channel.pending = false;
    let half = |v: Option<U7>| u32::from(v.map_or(0, U7::get));
    let data = (half(channel.msb) << 7) | half(lsb);
    let voice = match channel.kind {
        Kind::None => return,
        Kind::Registered => {
            let Number { bank, index } = channel.rpn;
            let value = if bank.get() == 0 && index.get() < ZERO_EXTENDED_INDICES {
                data << 18
            } else {
                scale::upscale(data, 14, 32)
            };
            Voice2::Registered { bank, index, value }
        }
        Kind::Assignable => {
            let Number { bank, index } = channel.nrpn;
            Voice2::Assignable {
                bank,
                index,
                value: scale::upscale(data, 14, 32),
            }
        }
    };
    send(voice);
}
