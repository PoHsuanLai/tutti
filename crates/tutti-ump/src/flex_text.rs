//! A text as Flex Data packets (M2-104 section 7.5.9): the writer of the
//! Flex text format, the way [`SysexMessage::packets`](crate::SysexMessage::packets) is the
//! writer of SysEx7.

use core::iter::FusedIterator;

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
///     Flex, FlexAddress, FlexTextKind, Form, Message, MidiGroup, flex_text_packets,
/// };
///
/// let mut packets =
///     flex_text_packets(MidiGroup::of(0), FlexAddress::Group, FlexTextKind::Lyrics, "thirteen byte")
///         .unwrap();
/// assert_eq!(packets.len(), 2);
/// let Ok(Message::Flex(_, Flex::Text { form, .. })) = packets.next().unwrap().decode() else {
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

/// How many Groups there are, so how many messages can be open at once.
const GROUPS: usize = 16;

/// What the assembler holds for one Group.
#[derive(Clone, Copy, Debug)]
enum Run {
    Idle,
    /// A message in `lane`, `len` bytes so far.
    Assembling {
        lane: usize,
        len: usize,
        address: FlexAddress,
        kind: FlexTextKind,
    },
    /// A message that was dropped or found invalid: its remaining packets
    /// (those of the same message) are skipped up to its `End`.
    Discarding {
        address: FlexAddress,
        kind: FlexTextKind,
    },
}

/// A text message the [`FlexTextAssembler`] completed: where it came from and
/// its text, without the zero padding.
///
/// It borrows the assembler and is valid until the next
/// [`push`](FlexTextAssembler::push).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FlexText<'a> {
    /// The Group it arrived on.
    pub group: MidiGroup,
    /// The Channel or Group it is addressed to.
    pub address: FlexAddress,
    /// Which text it is.
    pub kind: FlexTextKind,
    /// The text: UTF-8, with no Byte Order Mark. An empty one is the
    /// all-zero Complete message that Lyrics and Ruby use as a melisma
    /// (M2-104 sections 7.5.10.1 and 7.5.12.1).
    pub text: &'a str,
}

/// What [`FlexTextAssembler::push`] made of one UMP.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FlexTextStep<'a> {
    /// Not a Flex text packet: for the caller to read as usual.
    NotText,
    /// A Flex text packet, taken in; the message is not complete (or was
    /// dropped, and counted).
    Held,
    /// A message completed.
    Done(FlexText<'a>),
}

/// Reassembles the multi-packet Flex text messages (M2-104 section 7.5.9) that
/// [`flex_text_packets`] writes: the reader of the Flex text format, as
/// [`SysexAssembler`](crate::SysexAssembler) is of SysEx7. It holds no
/// allocation after [`new`](Self::new): its room is the buffer the caller gives.
///
/// # Rules (M2-104 sections 7.5.1, 7.5.2 and 7.5.9)
///
/// - **A message is a Complete packet, or a Start, any number of Continue and
///   an End,** all of one Group, one address and one text kind (Status Bank
///   and Status of Table 16).
/// - **One message is open per Group.** Section 7.5.2 lets nothing but the
///   message's own Continue packets, System Real Time messages and JR Clock
///   messages between its Start and End, so any other UMP of the Group, and a
///   Start or a packet of another message, ends the open one: it is dropped,
///   counted. At most `lanes` Groups are open at once (each has a slot of
///   `max_text` bytes in the buffer).
/// - **The text is UTF-8 without a Byte Order Mark,** checked when the message
///   is whole (a packet may end inside a character). The zero padding is the
///   end of the text: `0x00` may occur only in a Complete or End packet, and
///   from its first zero the rest of the packet is zero. A message that
///   breaks any of that is not delivered and is counted in
///   [`invalid`](Self::invalid), once. (Section 7.5.10 also calls a zero in a
///   Continue packet a melisma: that line contradicts 7.5.9 and the
///   Start rule beside it, and a melisma is the all-zero Complete packet;
///   this reads 7.5.9.)
/// - **Over capacity the whole message is dropped,** never cut: past
///   `max_text` bytes, or with no free lane. Its remaining packets are skipped
///   up to its End, and it is counted in [`dropped`](Self::dropped).
/// - A Continue or End with no open message (its Start was never seen or was
///   already counted as lost) is skipped.
///
/// ```
/// use tutti_ump::{
///     FlexAddress, FlexTextAssembler, FlexTextKind, FlexTextStep, MidiGroup, flex_text_packets,
/// };
///
/// let mut assembler = FlexTextAssembler::new([0u8; 64], 64, 1);
/// let group = MidiGroup::of(0);
/// let packets = flex_text_packets(group, FlexAddress::Group, FlexTextKind::Lyrics, "thirty-one bytes of text, in all").unwrap();
/// let mut done = None;
/// for ump in packets {
///     if let FlexTextStep::Done(text) = assembler.push(ump) {
///         done = Some(text.text.to_owned());
///     }
/// }
/// assert_eq!(done.as_deref(), Some("thirty-one bytes of text, in all"));
/// ```
#[derive(Debug)]
pub struct FlexTextAssembler<B> {
    buf: B,
    lanes: usize,
    max_text: usize,
    runs: [Run; GROUPS],
    /// A Complete packet's text, which has no lane.
    single: [u8; BYTES_PER_PACKET],
    dropped: u64,
    invalid: u64,
}

impl<B: AsRef<[u8]> + AsMut<[u8]>> FlexTextAssembler<B> {
    /// An assembler over `buf` for texts of up to `max_text` bytes, with up to
    /// `lanes` Groups open at once (at most sixteen). It needs `lanes *
    /// max_text` bytes: as many lanes as `buf` holds are used. An empty buffer
    /// drops (and counts) every message of more than one packet.
    #[must_use]
    pub fn new(buf: B, max_text: usize, lanes: usize) -> Self {
        let fit = buf.as_ref().len().checked_div(max_text).unwrap_or(0);
        Self {
            buf,
            lanes: lanes.min(GROUPS).min(fit),
            max_text,
            runs: [Run::Idle; GROUPS],
            single: [0; BYTES_PER_PACKET],
            dropped: 0,
            invalid: 0,
        }
    }

    /// The buffer length that holds `lanes` open texts of `max_text` bytes.
    #[must_use]
    pub const fn room_for(max_text: usize, lanes: usize) -> usize {
        max_text * lanes
    }

    /// Take in one UMP of the stream, whatever it is. Never allocates.
    pub fn push(&mut self, ump: Ump) -> FlexTextStep<'_> {
        let Ok(Message::Flex(
            group,
            Flex::Text {
                address,
                kind,
                form,
                bytes,
            },
        )) = ump.decode()
        else {
            self.other(ump);
            return FlexTextStep::NotText;
        };
        let g = group.index();
        let same = |a: FlexAddress, k: FlexTextKind| a == address && k == kind;
        match form {
            Form::Complete => {
                self.end_run(g);
                self.complete_single(group, address, kind, &bytes)
            }
            Form::Start => {
                self.end_run(g);
                self.start(g, address, kind, &bytes);
                FlexTextStep::Held
            }
            Form::Continue | Form::End => match self.runs[g] {
                Run::Assembling {
                    lane,
                    len,
                    address: a,
                    kind: k,
                } if same(a, k) => {
                    if form == Form::Continue {
                        self.extend(g, lane, len, &bytes, false);
                        FlexTextStep::Held
                    } else {
                        self.finish(group, lane, len, &bytes)
                    }
                }
                Run::Discarding {
                    address: a,
                    kind: k,
                } if same(a, k) => {
                    if form == Form::End {
                        self.runs[g] = Run::Idle;
                    }
                    FlexTextStep::Held
                }
                // Another message's packet interrupts the open one.
                Run::Assembling { .. } | Run::Discarding { .. } => {
                    self.end_run(g);
                    FlexTextStep::Held
                }
                Run::Idle => FlexTextStep::Held,
            },
        }
    }

    /// An upstream queue dropped packets after the ones just pushed: drop
    /// every open message, counted, so a later packet is not spliced onto one.
    pub fn lost_tail(&mut self) {
        for g in 0..GROUPS {
            self.end_run(g);
        }
    }

    /// The consumer was reset: drop the open messages (counted).
    pub fn reset(&mut self) {
        self.lost_tail();
    }

    /// Messages dropped whole since this assembler was made: longer than
    /// `max_text`, with no free lane, interrupted before their End, cut by
    /// [`lost_tail`](Self::lost_tail) or open at a [`reset`](Self::reset).
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// Messages refused since this assembler was made for breaking the text
    /// format: a zero where only text may be, a non-zero after the padding
    /// began, text that is not UTF-8, or one that begins with a Byte Order
    /// Mark.
    #[must_use]
    pub fn invalid(&self) -> u64 {
        self.invalid
    }

    /// A UMP that is not a Flex text packet. It ends its Group's open message
    /// unless it is a System Real Time or a JR Clock message (section 7.5.2).
    fn other(&mut self, ump: Ump) {
        let Some(group) = ump.group() else { return };
        let real_time = ump.message_type() == 0x1 && (ump.0[0] >> 16) & 0xFF >= 0xF8;
        if !real_time {
            self.end_run(group.index());
        }
    }

    /// The Group's open message ends without its End: counted if it was being
    /// assembled (a discarded one was counted when it was dropped).
    fn end_run(&mut self, g: usize) {
        if matches!(self.runs[g], Run::Assembling { .. }) {
            self.dropped += 1;
        }
        self.runs[g] = Run::Idle;
    }

    fn free_lane(&self) -> Option<usize> {
        (0..self.lanes).find(|&lane| {
            !self
                .runs
                .iter()
                .any(|run| matches!(run, Run::Assembling { lane: l, .. } if *l == lane))
        })
    }

    fn start(&mut self, g: usize, address: FlexAddress, kind: FlexTextKind, bytes: &[u8; 12]) {
        if bytes.contains(&0) {
            // Zero is the padding, and only an End or a Complete has any.
            self.invalid += 1;
            self.runs[g] = Run::Discarding { address, kind };
            return;
        }
        let Some(lane) = self.free_lane() else {
            self.dropped += 1;
            self.runs[g] = Run::Discarding { address, kind };
            return;
        };
        self.runs[g] = Run::Assembling {
            lane,
            len: 0,
            address,
            kind,
        };
        self.extend(g, lane, 0, bytes, false);
    }

    /// Append a Start or Continue packet (all 12 bytes text) to the message of
    /// `len` bytes in `lane`. `last` says the bytes are an End's, whose zeros
    /// are checked by the caller.
    fn extend(&mut self, g: usize, lane: usize, len: usize, bytes: &[u8], last: bool) -> bool {
        let Run::Assembling { address, kind, .. } = self.runs[g] else {
            return false;
        };
        if !last && bytes.contains(&0) {
            self.invalid += 1;
            self.runs[g] = Run::Discarding { address, kind };
            return false;
        }
        if len + bytes.len() > self.max_text {
            self.dropped += 1;
            self.runs[g] = Run::Discarding { address, kind };
            return false;
        }
        let at = lane * self.max_text + len;
        self.buf.as_mut()[at..at + bytes.len()].copy_from_slice(bytes);
        self.runs[g] = Run::Assembling {
            lane,
            len: len + bytes.len(),
            address,
            kind,
        };
        true
    }

    /// The text of a packet's bytes: up to the first zero, the rest zero.
    fn padded(bytes: &[u8]) -> Option<&[u8]> {
        let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
        bytes[end..].iter().all(|&b| b == 0).then(|| &bytes[..end])
    }

    fn finish(
        &mut self,
        group: MidiGroup,
        lane: usize,
        len: usize,
        bytes: &[u8; 12],
    ) -> FlexTextStep<'_> {
        let g = group.index();
        let Run::Assembling { address, kind, .. } = self.runs[g] else {
            return FlexTextStep::Held;
        };
        let Some(tail) = Self::padded(bytes) else {
            self.invalid += 1;
            self.runs[g] = Run::Idle;
            return FlexTextStep::Held;
        };
        let tail = tail.len();
        if !self.extend(g, lane, len, &bytes[..tail], true) {
            self.runs[g] = Run::Idle;
            return FlexTextStep::Held;
        }
        self.runs[g] = Run::Idle;
        let total = len + tail;
        self.deliver(group, address, kind, Some(lane), total)
    }

    fn complete_single(
        &mut self,
        group: MidiGroup,
        address: FlexAddress,
        kind: FlexTextKind,
        bytes: &[u8; 12],
    ) -> FlexTextStep<'_> {
        let Some(text) = Self::padded(bytes) else {
            self.invalid += 1;
            return FlexTextStep::Held;
        };
        let len = text.len();
        self.single[..len].copy_from_slice(text);
        self.deliver(group, address, kind, None, len)
    }

    /// Check the text of `len` bytes in `lane` (`None`: the Complete packet's)
    /// and hand it on.
    fn deliver(
        &mut self,
        group: MidiGroup,
        address: FlexAddress,
        kind: FlexTextKind,
        lane: Option<usize>,
        len: usize,
    ) -> FlexTextStep<'_> {
        let bytes = match lane {
            None => &self.single[..len],
            Some(lane) => &self.buf.as_ref()[lane * self.max_text..][..len],
        };
        match core::str::from_utf8(bytes) {
            Ok(text) if !text.starts_with('\u{FEFF}') => FlexTextStep::Done(FlexText {
                group,
                address,
                kind,
                text,
            }),
            _ => {
                self.invalid += 1;
                FlexTextStep::Held
            }
        }
    }
}
