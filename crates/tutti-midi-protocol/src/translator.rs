//! The stateful MIDI 1.0 to MIDI 2.0 translation of M2-104 Appendix D.3: the
//! [`Translator`] an input edge runs once.

use crate::scale;
use crate::voice::widen;
use tutti_ump::{
    Address, Attribute, Bank, CCNumber, Message, MidiChannel, MidiGroup, U7, Ump, Voice1, Voice2,
};

/// The byte that, in both halves of a parameter number, is the Null Function.
const NULL: u8 = 0x7F;

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
    /// The low 7 bits a High Resolution Velocity Prefix (CC 88) gave the next
    /// Note On or Note Off on this channel.
    velocity_low: Option<U7>,
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
///   passes on as a Control Change, drops what the run had not sent and sets
///   both halves of both numbers to null (127), so a later select of one half
///   pairs with a null other.
/// - **A High Resolution Velocity Prefix** (CC 88, CA-031) is absorbed and
///   becomes the low 7 bits of the next Note On or Note Off's 14-bit velocity
///   on that channel (the Note's byte is the high 7), scaled Min-Center-Max to
///   the 16 bits; the prefix is then cleared. A Note On at velocity 0 is the
///   Note Off it always was, and the prefix has no effect on it. M2-104 section
///   7.4.6 says CC 88 "shall not be used for High Resolution Velocity" in the
///   MIDI 2.0 Protocol: this is the MIDI 1.0 input edge, which is the one
///   place a prefix can arrive, so the fold is here and a CC 88 never reaches
///   a receiver.
/// - **Bank Select** (CC 0 and 32) is absorbed and rides on the channel's next
///   Program Change as its bank. The halves are kept across Program Changes,
///   so a lone CC 32 pairs with the CC 0 last sent (0 if never); a Program
///   Change with no Bank Select since the one before carries no bank.
/// - **Every other packet** passes through unchanged: a MIDI 2.0 message (a
///   Note On at velocity 0 stays a Note On), system, SysEx, Flex Data, UMP
///   Stream, utility, and a packet no definition covers. Data Increment and
///   Decrement (CC 96 and 97) stay Control Changes: M2-104 D.3.3 says they
///   "are translated to Control Change messages in the MIDI 2.0 Protocol. They
///   have no RPN/NRPN related function in the MIDI 2.0 Protocol. They should
///   not be translated to MIDI 2.0 Relative Registered Controller and
///   Relative Assignable Controller Messages." (RP-018's rule, that a MIDI 1.0
///   receiver applies them to the selected parameter, has no receiver in
///   tutti: every one takes MIDI 2.0.)
///
/// A CC 6 with no CC 38 after it waits for a trigger, which a stream that ends
/// never sends: [`flush`](Self::flush) sends what is still waiting. A clip's
/// loader calls it once the clip is read, so a file that sets a range with a CC
/// 6 alone and nothing after keeps it. A live input has no end, and the
/// "short timeout after receiving a CC 6" D.3.3 allows ("some translators may
/// wish to") is not built: it would need a clock here, and a caller that has
/// one can call `flush` itself.
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
///         controller: CCNumber::of(controller),
///         value: U7::of(value),
///     }))
/// };
/// let mut translator = Translator::new();
/// let mut out = Vec::new();
/// // Pitch bend sensitivity, 12 semitones: RPN 0/0, Data Entry 12 then 0.
/// for ump in [cc(101, 0), cc(100, 0), cc(6, 12), cc(38, 0)] {
///     out.extend(translator.translate(ump));
/// }
/// assert_eq!(out.len(), 1);
/// let Ok(Message::Midi2(_, Voice2::Registered { value, .. })) = out[0].decode() else {
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
        const ZERO: U7 = U7::of(0);
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
            velocity_low: None,
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

    /// Sends every run still waiting for a trigger, as its select, CC 6 or CC
    /// 38 would have, calling `out` with the channel's address and the packet
    /// (a CC 6 with no CC 38 is sent at LSB 0, a CC 38 with no CC 6 at MSB 0).
    /// Nothing else changes: the selections stay.
    pub fn flush(&mut self, mut out: impl FnMut(Address, Ump)) {
        let addresses = MidiGroup::all()
            .flat_map(|group| MidiChannel::all().map(move |channel| Address { group, channel }));
        for (address, channel) in addresses.zip(self.channels.iter_mut()) {
            flush(channel, &mut |voice| {
                out(address, Message::Midi2(address, voice).into());
            });
        }
    }

    /// The end of the stream: [`flush`](Self::flush), then the translator is
    /// gone. The offline caller's door (a clip's loader, which reads a whole
    /// stream and has no later packet to trigger a waiting run).
    pub fn finish(mut self, out: impl FnMut(Address, Ump)) {
        self.flush(out);
    }

    /// Translates one packet: `None` for an absorbed select, Data Entry or
    /// Bank Select that ends no run, and the packet it becomes otherwise (a
    /// select or a CC 6 that ends a run sends that run's controller, and is
    /// itself absorbed). Never more than one packet: a clip's loader puts the
    /// result in its source's place. A `Message` is accepted as its packet.
    pub fn translate(&mut self, packet: impl Into<Ump>) -> Option<Ump> {
        let ump = packet.into();
        let Ok(Message::Midi1(address, voice)) = ump.decode() else {
            return Some(ump);
        };
        let channel = &mut self.channels[address.index()];
        let mut sent = None;
        let mut send = |voice: Voice2| sent = Some(Message::Midi2(address, voice).into());
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
            Voice1::NoteOn { note, velocity } if velocity.get() != 0 => send(Voice2::NoteOn {
                note,
                velocity: channel.velocity(velocity),
                attribute: Attribute::None,
            }),
            Voice1::NoteOff { note, velocity } => send(Voice2::NoteOff {
                note,
                velocity: channel.velocity(velocity),
                attribute: Attribute::None,
            }),
            other => {
                // A Note On at velocity 0 is a Note Off, and the prefix has no
                // effect on it (CA-031), but the Note On/Off it was for is parsed.
                if matches!(other, Voice1::NoteOn { .. }) {
                    channel.velocity_low = None;
                }
                send(widen(other));
            }
        }
        sent
    }
}

impl Channel {
    /// The 16-bit velocity of a Note On or Off whose 7 high bits are `high`:
    /// with a pending High Resolution Velocity Prefix the 14-bit velocity
    /// (`high << 7 | prefix`) scaled Min-Center-Max; the prefix is then
    /// cleared ("After the standard Note On or Note Off message has been
    /// parsed, the lower 7 bits of the receiver's 14-bit velocity register
    /// should be cleared", CA-031).
    fn velocity(&mut self, high: U7) -> u16 {
        match self.velocity_low.take() {
            Some(low) => {
                scale::upscale((u32::from(high.get()) << 7) | u32::from(low.get()), 14, 16) as u16
            }
            None => scale::velocity(high.get()),
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
    match controller {
        CCNumber::RPN_MSB | CCNumber::RPN_LSB | CCNumber::NRPN_MSB | CCNumber::NRPN_LSB => {
            flush(channel, send);
            let registered = matches!(controller, CCNumber::RPN_MSB | CCNumber::RPN_LSB);
            let number = if registered {
                &mut channel.rpn
            } else {
                &mut channel.nrpn
            };
            if matches!(controller, CCNumber::RPN_MSB | CCNumber::NRPN_MSB) {
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
        CCNumber::DATA_ENTRY if channel.kind != Kind::None => {
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
        CCNumber::DATA_ENTRY_LSB if channel.kind != Kind::None => {
            if channel.msb.is_some() {
                emit(channel, Some(value), send);
            } else {
                channel.early_lsb = Some(value);
            }
        }
        // Data Entry with nothing selected, or after the Null Function.
        CCNumber::DATA_ENTRY | CCNumber::DATA_ENTRY_LSB => {}
        // RP-015: Reset All Controllers sets the RPN and NRPN to null. What a
        // run had not sent is dropped with it (sending it as well would make
        // two packets of one); the message itself goes on.
        CCNumber::RESET_ALL => {
            let null = Number {
                bank: U7::of(NULL),
                index: U7::of(NULL),
            };
            channel.rpn = null;
            channel.nrpn = null;
            channel.kind = Kind::None;
            channel.msb = None;
            channel.pending = false;
            channel.early_lsb = None;
            send(widen(Voice1::ControlChange { controller, value }));
        }
        // CA-031: Bn 58 vv is the lower 7 bits of the next Note On or Note Off's
        // velocity on this channel. It is the MIDI 1.0 spelling of what the
        // 16-bit velocity carries, so it is absorbed (M2-104 section 7.4.6: "In
        // MIDI 2.0 Protocol, Control Change 88 shall not be used for High
        // Resolution Velocity").
        CCNumber::VELOCITY_PREFIX => channel.velocity_low = Some(value),
        CCNumber::BANK_SELECT => {
            channel.bank_msb = value;
            channel.bank_set = true;
        }
        CCNumber::BANK_SELECT_LSB => {
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
            let value = if scale::zero_extended(bank, index) {
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
