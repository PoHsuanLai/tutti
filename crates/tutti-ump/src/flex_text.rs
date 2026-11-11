//! A text as Flex Data packets (M2-104 section 7.5.9): the writer of the
//! Flex text format, the way [`sysex7_packets`](crate::sysex7_packets) is the
//! writer of SysEx7.

use std::iter::FusedIterator;

use crate::{Flex, FlexAddress, FlexTextKind, Form, Message, MidiGroup, Ump};

/// The bytes one Flex text packet carries.
const BYTES_PER_PACKET: usize = 12;

/// The Flex text packets of `text`, `None` if it holds a NUL.
///
/// A text of up to 12 bytes is one `Complete` packet (an empty one is a
/// `Complete` of zeros). A longer one is a `Start`, any number of `Continue`,
/// then an `End`, each carrying 12 bytes but the last, which carries one to 12
/// and is padded with zeros. A packet may end inside a multi-byte character:
/// the reader joins the bytes before it decodes them. A NUL is refused
/// because zero is the padding: a reader cannot tell it from the end of the
/// text. Nothing is allocated, and the packets carry no timing.
///
/// ```
/// use tutti_ump::{
///     Flex, FlexAddress, FlexTextKind, Form, Message, MidiGroup, decode, flex_text_packets,
/// };
///
/// let mut packets =
///     flex_text_packets(MidiGroup::of(0), FlexAddress::Group, FlexTextKind::Lyrics, "thirteen byte")
///         .unwrap();
/// assert_eq!(packets.len(), 2);
/// let Ok(Message::Flex(_, Flex::Text { form, .. })) = decode(packets.next().unwrap()) else {
///     unreachable!()
/// };
/// assert_eq!(form, Form::Start);
/// ```
#[must_use]
pub fn flex_text_packets(
    group: MidiGroup,
    address: FlexAddress,
    kind: FlexTextKind,
    text: &str,
) -> Option<FlexTextPackets<'_>> {
    (!text.as_bytes().contains(&0)).then_some(FlexTextPackets {
        group,
        address,
        kind,
        rest: text.as_bytes(),
        started: false,
    })
}

/// The packets of one text; see [`flex_text_packets`].
#[derive(Clone, Debug)]
pub struct FlexTextPackets<'a> {
    group: MidiGroup,
    address: FlexAddress,
    kind: FlexTextKind,
    rest: &'a [u8],
    /// Whether a packet has been yielded, so an empty text is one packet.
    started: bool,
}

impl Iterator for FlexTextPackets<'_> {
    type Item = Ump;

    fn next(&mut self) -> Option<Ump> {
        if self.started && self.rest.is_empty() {
            return None;
        }
        let form = match (self.started, self.rest.len() > BYTES_PER_PACKET) {
            (false, false) => Form::Complete,
            (false, true) => Form::Start,
            (true, true) => Form::Continue,
            (true, false) => Form::End,
        };
        let (chunk, rest) = self.rest.split_at(self.rest.len().min(BYTES_PER_PACKET));
        self.rest = rest;
        self.started = true;
        let mut bytes = [0u8; BYTES_PER_PACKET];
        bytes[..chunk.len()].copy_from_slice(chunk);
        Some(Ump::from(Message::Flex(
            self.group,
            Flex::Text {
                address: self.address,
                kind: self.kind,
                form,
                bytes,
            },
        )))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let n = match (self.started, self.rest.len()) {
            (true, 0) => 0,
            (_, len) => len.div_ceil(BYTES_PER_PACKET).max(1),
        };
        (n, Some(n))
    }
}

impl ExactSizeIterator for FlexTextPackets<'_> {}
impl FusedIterator for FlexTextPackets<'_> {}
