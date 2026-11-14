//! MIDI 1.0 bytes and the UMP that carries them.

use tutti_midi_protocol::midi1;
use tutti_ump::{
    Midi1Parser, MidiGroup, SYSEX7_MAX_PAYLOAD, SysexAssembler, SysexStep, Ump, parse_exact,
};

/// MIDI 1.0 bytes read as UMP on the first Group: a port is one cable.
///
/// Voice and system messages come out as MIDI 1.0 UMP (type 2 and type 1) and
/// System Exclusive as SysEx7 packets (type 3), which the ingest node widens
/// and reassembles.
#[derive(Clone, Debug)]
pub(in crate::backend) struct ByteIn {
    parser: Midi1Parser,
}

impl ByteIn {
    /// A reader at the start of a stream.
    pub(in crate::backend) fn new() -> Self {
        Self {
            parser: Midi1Parser::new(MidiGroup::FIRST),
        }
    }

    /// The message in a packed short message: the status byte, then its data
    /// bytes, low byte first, as the OS gives them with running status already
    /// expanded. `None` for a status that is not a message (a stray data byte).
    /// Does not touch a System Exclusive message in progress.
    pub(in crate::backend) fn short(&self, packed: u32) -> Option<Ump> {
        let [status, d1, d2, _] = packed.to_le_bytes();
        parse_exact(MidiGroup::FIRST, &[status, d1, d2]).map(Ump::from)
    }

    /// The packets of the next bytes of a stream that carries System
    /// Exclusive: a message longer than one buffer continues in the next call.
    pub(in crate::backend) fn long<'a>(
        &'a mut self,
        bytes: &'a [u8],
    ) -> impl Iterator<Item = Ump> + 'a {
        self.parser.feed_all(bytes).map(Ump::from)
    }
}

/// What to put on the wire for a UMP.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::backend) enum Wire<'a> {
    /// A short message, packed as the OS wants it: the status byte, then its
    /// data bytes, low byte first.
    Short(u32),
    /// A whole System Exclusive message, `0xF0` to `0xF7`.
    Long(&'a [u8]),
}

/// UMP written as MIDI 1.0 bytes.
///
/// The UMP are already MIDI 1.0 (the output narrowed them for a MIDI 1.0
/// endpoint). A message with no MIDI 1.0 form (a Flex Data or a Utility
/// message) is consumed and sends nothing. SysEx7 packets are joined into one
/// message, which is sent when its last packet arrives.
#[derive(Debug)]
pub(in crate::backend) struct ByteOut {
    sysex: SysexAssembler<Box<[u8]>>,
}

/// The longest message sent: the payload tutti carries, with its delimiters.
const MAX_MESSAGE: usize = SYSEX7_MAX_PAYLOAD + 2;

impl ByteOut {
    /// A writer with no message open.
    pub(in crate::backend) fn new() -> Self {
        // One lane, one packet at a time: each message is sent before the next
        // packet is pushed.
        let room = SysexAssembler::<()>::room_for(MAX_MESSAGE, 1, 1);
        Self {
            sysex: SysexAssembler::new(vec![0; room].into_boxed_slice(), MAX_MESSAGE, 1),
        }
    }

    /// Hands `send` what `ump` puts on the wire: nothing for the packet of a
    /// message not yet whole, one message for a short message or the packet
    /// that completes a SysEx, up to three for a Program Change with a bank.
    /// Stops at the first `Err` `send` returns.
    pub(in crate::backend) fn encode<E>(
        &mut self,
        ump: Ump,
        mut send: impl FnMut(Wire<'_>) -> Result<(), E>,
    ) -> Result<(), E> {
        self.sysex.begin_block();
        match self.sysex.push(ump) {
            SysexStep::Held => Ok(()),
            SysexStep::Complete(done) => send(Wire::Long(self.sysex.message(done).bytes())),
            SysexStep::NotSysex => midi1(ump).try_for_each(|m| {
                let [status, d1, d2] = m.padded();
                send(Wire::Short(u32::from_le_bytes([status, d1, d2, 0])))
            }),
        }
    }
}
