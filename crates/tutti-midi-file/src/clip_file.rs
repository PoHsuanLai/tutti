//! MIDI 2.0 **Clip File** (M2-116) byte codec: the wire half under
//! [`MidiFile`](crate::MidiFile).
//!
//! A Clip File is one sequence of Universal MIDI Packets. Layout (M2-116
//! sections 4 to 7, all big-endian):
//!
//! ```text
//! ["SMF2CLIP" 8 bytes] [Set Profile On]* [DCS=0] [DCTPQ] [ (DCS * UMP)* ]
//!   [DCS] [StartOfClip] [ (DCS * UMP)* ] [DCS] [EndOfClip]
//! ```
//!
//! Everything from the file header up to the DCS of Start of Clip is the
//! **Clip Configuration Header** (M2-116 section 6): leading Set Profile On
//! messages (no DCS), the DCTPQ, and any UMP messages that configure a
//! receiver (Bank Select, Program Change, volume, System Exclusive), each
//! behind its own DCS. It holds at most one Set Tempo and one Set Time
//! Signature (sections 6.1 to 6.1.2), both at delta 0. The DCS before Start
//! of Clip is the header's last gap, and with the others "determines the total
//! length of time allocated to the Clip Configuration Header" (section 6.1): it
//! is kept ([`ParsedClip::header_end`]). The rest, from Start of Clip to End of
//! Clip, is the Sequence Data (section 7). No Property Exchange message is in
//! either (sections 6.4 and 7.2).
//!
//! Every UMP is governed by a **Delta Clockstamp** (DCS) giving the ticks since
//! the previous event: "The Delta Clockstamp message declares the time of all
//! following messages which occur before the next Delta Clockstamp message"
//! (section 3.2.2), so simultaneous events may share one. The writer gives each
//! event its own, except the parts of a Lyric or Ruby message, which share the
//! first's (see [`write`](fn@write)). The tick unit is declared once by the **DCTPQ** (Delta
//! Clockstamp Ticks Per Quarter Note). The stream is bracketed by Start of Clip
//! and End of Clip UMP Stream messages, and the DCS before End of Clip is the
//! clip's length.
//!
//! This module reads (a `winnow` grammar, in `read.rs`) and writes a clip as
//! absolute ticks and packets, with no decoding past the structure: [`MidiFile`](crate::MidiFile) is the model
//! over it.

use std::vec::Vec;

use tutti_ump::{
    DeltaTicks, Flex, FlexTextKind, Form, Message, Stream, SysexStatus, Ump, Utility, decode,
};

mod read;

pub(crate) use read::{check_header, leading_profile_len, parse};

/// The 8-byte file header: ASCII "SMF2CLIP" (M2-116 section 5).
pub(crate) const CLIP_FILE_MAGIC: [u8; 8] = *b"SMF2CLIP";

/// A clip as the wire states it: the tick unit, the events at absolute
/// ticks, and the tick the clip ends at.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ParsedClip {
    /// Ticks per quarter note (the DCTPQ), never zero.
    pub(crate) ticks_per_quarter: u16,
    /// The header's messages after the file header, as `(tick, packet)`: the
    /// leading Set Profile On packets at 0, then each message behind its DCS,
    /// ticks counted from the DCTPQ (the header's own clock, which runs
    /// before the sequence).
    pub(crate) header: Vec<(u64, Ump)>,
    /// Every packet between Start of Clip and End of Clip that is not itself
    /// structure (a DCS, a Null, a JR message, the DCTPQ), at its absolute
    /// tick, in file order.
    pub(crate) events: Vec<(u64, Ump)>,
    /// The tick of End of Clip: the clip's length.
    pub(crate) end: u64,
    /// The tick, on the header's clock, of Start of Clip: the header's last
    /// message's tick plus the Delta Clockstamp before Start of Clip.
    pub(crate) header_end: u64,
}

/// Why a byte stream failed to parse as a MIDI Clip File (M2-116). Distinguishes
/// "this isn't a clip file" from "this clip file is malformed" so a caller can
/// react differently (e.g. try another importer vs. report corruption).
///
/// The structure rules are one variant each: [`DctpqNotFirst`](Self::DctpqNotFirst)
/// and [`DuplicateDctpq`](Self::DuplicateDctpq) (section 6),
/// [`DuplicateHeaderTempo`](Self::DuplicateHeaderTempo),
/// [`MisplacedHeaderTempo`](Self::MisplacedHeaderTempo),
/// [`DuplicateHeaderTimeSignature`](Self::DuplicateHeaderTimeSignature) and
/// [`MisplacedHeaderTimeSignature`](Self::MisplacedHeaderTimeSignature)
/// (sections 6.1 to 6.1.2), [`DctpqDelta`](Self::DctpqDelta) and
/// [`ProfileOnDelta`](Self::ProfileOnDelta) (sections 3.2.1 and 6),
/// [`PropertyExchange`](Self::PropertyExchange) (sections 6.4 and 7.2),
/// [`DuplicateStartOfClip`](Self::DuplicateStartOfClip) (section 7) and
/// [`TrailingData`](Self::TrailingData) (section 7.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ClipFileError {
    /// Fewer than 8 bytes, or the leading 8 bytes are not `SMF2CLIP`.
    #[error("not a MIDI Clip File (bad or missing SMF2CLIP magic)")]
    BadMagic,
    /// The body length is not a whole number of 32-bit UMP words.
    #[error("clip body is not 32-bit-word aligned")]
    Unaligned,
    /// A UMP claims more words than remain in the stream.
    #[error("clip file is truncated mid-message")]
    Truncated,
    /// No DCTPQ (tick-unit declaration) in the file.
    #[error("clip file has no DCTPQ tick-unit declaration")]
    MissingDctpq,
    /// The DCTPQ declared zero ticks per quarter note.
    ///
    /// Distinct from [`MissingDctpq`](Self::MissingDctpq) because the file did
    /// declare one: it is unusable rather than absent, and a caller reporting
    /// corruption wants to say which. Every beat in the file is a tick count
    /// divided by this.
    #[error("clip file declares zero ticks per quarter note")]
    ZeroDctpq,
    /// A message other than a Set Profile On comes before the DCTPQ. M2-116
    /// section 6: the DCTPQ follows the Set Profile On messages, or the file
    /// header if there are none.
    #[error("clip file has a message before its DCTPQ")]
    DctpqNotFirst,
    /// The DCTPQ has no Delta Clockstamp right before it, or one that is not 0.
    /// Section 3.2.1 and section 6: "The DCTPQ shall have a preceding Delta
    /// Clockstamp (see Section 3.2.2) with Number of Ticks Since Last Event set
    /// to 0x00000."
    #[error("clip file's DCTPQ has no Delta Clockstamp of 0 before it")]
    DctpqDelta,
    /// A Set Profile On message has a Delta Clockstamp before it. Section 6:
    /// "Set Profile On messages shall not have a prepended Delta Clockstamp
    /// (DCS)."
    #[error("clip file's Set Profile On has a Delta Clockstamp before it")]
    ProfileOnDelta,
    /// A MIDI-CI Property Exchange message (Universal System Exclusive `7E`,
    /// sub-ID#1 `0D`, sub-ID#2 `30` to `3F`). Section 6.4: "Property Data for PE
    /// Resources shall not be included in a MIDI Clip File"; section 7.2: the
    /// sequence data holds "any MIDI messages in UMP Format, except Property
    /// Exchange messages".
    #[error("clip file holds a Property Exchange message")]
    PropertyExchange,
    /// A second DCTPQ. The tick unit is declared once.
    #[error("clip file declares its DCTPQ more than once")]
    DuplicateDctpq,
    /// No Start of Clip message. M2-116 section 7: "A Clip Sequence Data shall
    /// include one Start of Clip message ... as the first UMP message."
    #[error("clip file has no Start of Clip message")]
    MissingStartOfClip,
    /// A second Start of Clip. Section 7 asks for one.
    #[error("clip file has more than one Start of Clip message")]
    DuplicateStartOfClip,
    /// A second Set Tempo in the header. Section 6.1: the header holds at
    /// most one.
    #[error("clip header has more than one Set Tempo")]
    DuplicateHeaderTempo,
    /// A second Set Time Signature in the header. Section 6.1.2: at most one.
    #[error("clip header has more than one Set Time Signature")]
    DuplicateHeaderTimeSignature,
    /// The header's Set Tempo is not the first message after the DCTPQ, or its
    /// Delta Clockstamp is not 0. Section 6.1.1: "shall" be first, with a DCS
    /// of 0 or sharing the DCTPQ's.
    #[error("clip header's Set Tempo is not first after the DCTPQ at delta 0")]
    MisplacedHeaderTempo,
    /// The header's Set Time Signature does not immediately follow the Set
    /// Tempo (or the DCTPQ, in a header with no Set Tempo), or its Delta
    /// Clockstamp is not 0. Section 6.1.2: it "shall use the Delta Clockstamp
    /// which precedes the DCTPQ message or shall have a preceding Delta
    /// Clockstamp with Number of Ticks Since Last Event set to 0x00000".
    #[error("clip header's Set Time Signature is not first after the Set Tempo at delta 0")]
    MisplacedHeaderTimeSignature,
    /// No End of Clip message. Section 7: "A Clip Sequence Data shall include
    /// one End of Clip message as the last UMP message."
    #[error("clip file has no End of Clip message")]
    MissingEndOfClip,
    /// Bytes follow the End of Clip, which must be last. Section 7.3: "A MIDI
    /// Clip File shall not have any data following the End of Clip message."
    /// There is no multi-clip clip file: that is what the MIDI Container File
    /// is for.
    #[error("clip file has data following the End of Clip message")]
    TrailingData,
}

/// Whether `message` starts a MIDI-CI Property Exchange message: a System
/// Exclusive 7 packet, first of its message, that begins `7E <device> 0D` and a
/// sub-ID#2 of `30` to `3F` (M2-101 Table 5: "Property Exchange Messages"). The
/// packets after the first are not told apart, as they say nothing of what the
/// message is; refusing the first refuses the message.
pub(crate) fn property_exchange(message: &Message) -> bool {
    let Message::Sysex7(_, packet) = message else {
        return false;
    };
    matches!(packet.status(), SysexStatus::Complete | SysexStatus::Start)
        && matches!(packet.data(), [0x7E, _, 0x0D, 0x30..=0x3F, ..])
}

/// Serializes a clip: the leading Set Profile On packets (no DCS), the DCTPQ,
/// the rest of the header, Start of Clip, each event behind its Delta
/// Clockstamp, and End of Clip behind the DCS that makes the clip `end` ticks
/// long. `header` and `events` are absolute ticks in order (the header's on
/// its own clock), `events` none past `end`, no two more than `u32::MAX` apart
/// (the caller checks; [`MidiFile`] does), and the header ends at `header_end`,
/// not before its last message: the Delta Clockstamp before Start of Clip is
/// the gap that makes it so.
///
/// **One Delta Clockstamp for the parts of a Lyric or Ruby message.**
/// M2-116 section 3 says "A Delta Clockstamp message precedes every UMP", and
/// M2-104 sections 7.5.10 and 7.5.12 say that "When placed in a MIDI Clip File,
/// all parts of the Lyric Data Message shall be placed at the same time, sharing
/// a single Delta Clock Stamp" (the Ruby Data Message likewise). They agree
/// once the Clip File's own definition of a DCS is read: "The Delta Clockstamp
/// message declares the time of all following messages which occur before the
/// next Delta Clockstamp message" (M2-116 section 3.2.2; M2-104 section 7.2.3.2:
/// "Simultaneous events may share a single Delta Clockstamp"), so a DCS precedes
/// every UMP in that it governs it. A zero DCS between the parts would be a
/// second Delta Clock Stamp, not a shared one. So the parts after the first are
/// written with none, and every other event keeps its own.
///
/// [`MidiFile`]: crate::MidiFile
pub(crate) fn write(
    ticks_per_quarter: u16,
    header: &[(u64, Ump)],
    header_end: u64,
    events: &[(u64, Ump)],
    end: u64,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + (header.len() + events.len() + 4) * 8);
    out.extend_from_slice(&CLIP_FILE_MAGIC);

    // Set Profile On messages come first and carry no DCS (section 6).
    let pre = leading_profile_len(header);
    for (_, ump) in &header[..pre] {
        push_words(&mut out, ump.words());
    }

    // DCTPQ declares the tick unit (preceded by a zero DCS per section 3.2.1).
    push_words(&mut out, delta_clockstamp(0).words());
    push_words(&mut out, dctpq(ticks_per_quarter).words());

    // The rest of the header, each message behind its DCS.
    let mut at = 0u64;
    for &(tick, ump) in &header[pre..] {
        push_delta(&mut out, tick.saturating_sub(at));
        push_words(&mut out, ump.words());
        at = tick;
    }

    // Start of Clip, behind the DCS that ends the header's allotted time.
    push_delta(&mut out, header_end.saturating_sub(at));
    push_words(&mut out, start_of_clip().words());

    // The timed event stream: each UMP preceded by its delta clockstamp(s).
    // A DCS field is only 20 bits, so a delta beyond `DCS_MAX` is expressed as
    // DCS*NOOP restarts (M2-116 section 3.2.2): see `push_delta`.
    let mut at = 0u64;
    let mut previous: Option<Ump> = None;
    for &(tick, ump) in events {
        let shares = tick == at && previous.is_some_and(|p| continues_text(p, ump));
        if !shares {
            push_delta(&mut out, tick.saturating_sub(at));
        }
        push_words(&mut out, ump.words());
        at = tick;
        previous = Some(ump);
    }

    // End of Clip, behind the DCS that is the clip's length.
    push_delta(&mut out, end.saturating_sub(at));
    push_words(&mut out, end_of_clip().words());
    out
}

/// Whether `next` is a Lyric or Ruby packet that continues the message `previous`
/// is a part of: the same kind, Group and address, `previous` a Start or a
/// Continue and `next` a Continue or an End.
fn continues_text(previous: Ump, next: Ump) -> bool {
    let (
        Ok(Message::Flex(
            group_p,
            Flex::Text {
                address: addr_p,
                kind: kind_p,
                form: form_p,
                ..
            },
        )),
        Ok(Message::Flex(
            group_n,
            Flex::Text {
                address: addr_n,
                kind: kind_n,
                form: form_n,
                ..
            },
        )),
    ) = (decode(previous), decode(next))
    else {
        return false;
    };
    matches!(kind_p, FlexTextKind::Lyrics | FlexTextKind::Ruby)
        && (group_p, addr_p, kind_p) == (group_n, addr_n, kind_n)
        && matches!(form_p, Form::Start | Form::Continue)
        && matches!(form_n, Form::Continue | Form::End)
}

// --- helpers ---------------------------------------------------------------

fn push_words(out: &mut Vec<u8>, words: &[u32]) {
    for w in words {
        out.extend_from_slice(&w.to_be_bytes());
    }
}

/// Widest value a single 20-bit Delta Clockstamp field can carry.
const DCS_MAX: u64 = 0x000F_FFFF;

fn delta_clockstamp(ticks: u64) -> Ump {
    Ump::from(Message::Utility(Utility::DeltaClockstamp(
        DeltaTicks::masked((ticks & DCS_MAX) as u32),
    )))
}

/// Emit `ticks` as Delta Clockstamps, restarting the count with a NOOP each
/// time a gap exceeds the 20-bit DCS field.
///
/// M2-116 section 3.2.2 (identically M2-104-UM section 7.2.3.2): "If no MIDI
/// message has occurred during the previous 1,048,575 ticks, then the
/// application which creates the MIDI Clip File shall insert a Delta Clockstamp
/// followed by a Null message to restart the delta time count. Then the next
/// Delta Clockstamp in the file declares the ticks since the previous Null
/// message."
///
/// So consecutive DCSs do **not** accumulate: each one is the count since the
/// last event, and a NOOP is what makes a long gap expressible as a sequence of
/// restarts. `0` still emits exactly one (zero) DCS.
fn push_delta(out: &mut Vec<u8>, mut ticks: u64) {
    while ticks > DCS_MAX {
        push_words(out, delta_clockstamp(DCS_MAX).words());
        push_words(out, Ump::from(Message::Utility(Utility::Noop)).words());
        ticks -= DCS_MAX;
    }
    push_words(out, delta_clockstamp(ticks).words());
}

fn dctpq(ticks_per_quarter: u16) -> Ump {
    Ump::from(Message::Utility(Utility::TicksPerQuarterNote(
        ticks_per_quarter,
    )))
}

fn start_of_clip() -> Ump {
    Ump::from(Message::Stream(Stream::StartOfClip))
}

fn end_of_clip() -> Ump {
    Ump::from(Message::Stream(Stream::EndOfClip))
}
