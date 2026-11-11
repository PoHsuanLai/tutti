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
//! Signature (sections 6.1 to 6.1.2). The rest, from Start of Clip to End of
//! Clip, is the Sequence Data (section 7).
//!
//! Every UMP is preceded by a **Delta Clockstamp** (DCS) giving the ticks since
//! the previous event; the tick unit is declared once by the **DCTPQ** (Delta
//! Clockstamp Ticks Per Quarter Note). The stream is bracketed by Start of Clip
//! and End of Clip UMP Stream messages, and the DCS before End of Clip is the
//! clip's length.
//!
//! This module reads and writes a clip as absolute ticks and packets, with no
//! decoding past the structure: [`MidiFile`](crate::MidiFile) is the model
//! over it.

use std::vec::Vec;

use tutti_ump::{
    DeltaTicks, Flex, Message, Stream, Sysex7Packet, SysexStatus, Ump, Utility, decode,
    split_stream,
};

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
/// (sections 6.1 to 6.1.2), [`DuplicateStartOfClip`](Self::DuplicateStartOfClip)
/// (section 7) and [`TrailingData`](Self::TrailingData) (section 7.3).
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
    /// Tempo (or the DCTPQ, in a header with no Set Tempo). Section 6.1.2.
    #[error("clip header's Set Time Signature does not immediately follow the Set Tempo")]
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

pub(crate) fn parse(bytes: &[u8]) -> Result<ParsedClip, ClipFileError> {
    if bytes.len() < 8 || bytes[..8] != CLIP_FILE_MAGIC {
        return Err(ClipFileError::BadMagic);
    }
    let words = words_of(&bytes[8..]).ok_or(ClipFileError::Unaligned)?;

    let mut ticks_per_quarter = None;
    let mut events = Vec::new();
    // The delta declared by the most recent DCS, and the total closed off by
    // NOOP restarts before it. See the DCS/NOOP arms below.
    let mut pending_delta: u32 = 0;
    let mut banked_delta: u32 = 0;
    // Where the sequence is: the tick the last event (or the start) sits at.
    let mut tick: u64 = 0;
    // Structure order (M2-116 sections 6 and 7). `content` is whether any
    // message other than a DCS, a Null or a JR message has come: the DCTPQ
    // must not follow one, and `started` is whether Start of Clip has.
    let mut content = false;
    let mut started = false;
    let mut end = None;
    // The header: Set Profile On packets before the DCTPQ (counted by `pre`),
    // then what follows the DCTPQ up to Start of Clip.
    let mut header: Vec<(u64, Ump)> = Vec::new();
    let mut pre = 0;
    let mut profile = ProfileScan::default();

    // Words consumed so far: `split_stream` ends at a truncated packet without
    // saying so, and the words it left are what `taken` falls short of.
    let mut taken = 0;
    for ev in split_stream(&words) {
        taken += ev.words().len();

        // Classify by message. An undefined status of a Utility message
        // (JR clock and timestamp included) is ignored, as the spec allows a
        // reader to; any other packet that does not decode is an event, kept
        // as its words.
        match decode(ev) {
            Ok(Message::Utility(utility)) => {
                match utility {
                    // A DCS is the count since the last event or Null message:
                    // it *replaces* the current count rather than adding to it
                    // (M2-116 section 3.2.2). Only `banked_delta`, closed off by
                    // a NOOP below, carries over from earlier restarts.
                    Utility::DeltaClockstamp(ticks) => pending_delta = ticks.get(),
                    Utility::TicksPerQuarterNote(ticks) => {
                        if ticks_per_quarter.is_some() {
                            return Err(ClipFileError::DuplicateDctpq);
                        }
                        if content {
                            return Err(ClipFileError::DctpqNotFirst);
                        }
                        ticks_per_quarter = Some(ticks);
                        // The DCTPQ's DCS is 0 and starts the header's clock.
                        tick = 0;
                        pending_delta = 0;
                        banked_delta = 0;
                    }
                    // NOOP = the Null message that restarts the delta count
                    // (M2-116 section 3.2.2). Bank what the preceding DCS
                    // declared; the next DCS counts from here.
                    Utility::Noop => {
                        banked_delta = banked_delta.saturating_add(pending_delta);
                        pending_delta = 0;
                    }
                    // JR clock / timestamp: not part of a clip's sequence.
                    Utility::JrClock(_) | Utility::JrTimestamp(_) => {}
                }
                continue;
            }
            Err(_) if ev.message_type() == 0x0 => continue,
            Ok(Message::Stream(Stream::StartOfClip)) => {
                if started {
                    return Err(ClipFileError::DuplicateStartOfClip);
                }
                header_rules(&header, pre)?;
                started = true;
                content = true;
                tick = 0;
                // The DCS before it is not an event's delta: the sequence
                // starts at tick 0 however the file spells it.
                pending_delta = 0;
                banked_delta = 0;
                continue; // structural
            }
            Ok(Message::Stream(Stream::EndOfClip)) => {
                // The DCS before End of Clip is the clip's length (all of the
                // delta since the last event, restarts included).
                end = Some(tick + u64::from(banked_delta) + u64::from(pending_delta));
                // Section 7.3: "A MIDI Clip File shall not have any data
                // following the End of Clip message." There is no multi-clip
                // clip file, so anything after this is corruption rather than
                // a second clip.
                if taken < words.len() {
                    return Err(ClipFileError::TrailingData);
                }
                break;
            }
            _ => {}
        }
        // Before the DCTPQ only Set Profile On messages are allowed (section
        // 6), with no DCS; anything else is content the DCTPQ must not follow (a
        // Set Profile On after such content is kept, and the DCTPQ's arrival
        // names the error).
        if ticks_per_quarter.is_none() {
            if decode(ev).is_ok_and(|m| profile.accepts(&m)) {
                header.push((0, ev));
                pre += 1;
            } else {
                content = true;
            }
            continue;
        }
        // After the DCTPQ and before Start of Clip: a header message, on the
        // header's own clock.
        if !started {
            tick += u64::from(banked_delta) + u64::from(pending_delta);
            header.push((tick, ev));
            pending_delta = 0;
            banked_delta = 0;
            continue;
        }
        content = true;
        // Its delta is the current count plus everything banked by NOOP
        // restarts since the previous event. A u64 sum: two saturated u32
        // deltas must not wrap.
        tick += u64::from(banked_delta) + u64::from(pending_delta);
        events.push((tick, ev));
        pending_delta = 0;
        banked_delta = 0;
    }
    if taken < words.len() {
        return Err(ClipFileError::Truncated);
    }

    // Order matters: report the *earliest* structural thing that is missing, so
    // a caller sees the first reason the file is unusable rather than the last.
    let ticks_per_quarter = ticks_per_quarter.ok_or(ClipFileError::MissingDctpq)?;
    // Declared, but unusable: every beat this file yields is a tick count
    // divided by this.
    if ticks_per_quarter == 0 {
        return Err(ClipFileError::ZeroDctpq);
    }
    if !started {
        return Err(ClipFileError::MissingStartOfClip);
    }
    let end = end.ok_or(ClipFileError::MissingEndOfClip)?;

    Ok(ParsedClip {
        ticks_per_quarter,
        header,
        events,
        end,
    })
}

/// Tracks whether the packets it is fed are a Set Profile On message (MIDI-CI
/// sub-ID 2 `0x22`, a Universal SysEx `7E <device> 0D 22 ...`), packet by
/// packet: a message's first packet says what it is, and its Continue and End
/// packets belong to it.
#[derive(Default)]
struct ProfileScan {
    open: bool,
}

impl ProfileScan {
    fn accepts(&mut self, message: &Message) -> bool {
        let Message::Sysex7(_, packet) = message else {
            self.open = false;
            return false;
        };
        let is_set_profile_on = |p: &Sysex7Packet| matches!(p.data(), [0x7E, _, 0x0D, 0x22, ..]);
        match packet.status() {
            SysexStatus::Complete => {
                self.open = false;
                is_set_profile_on(packet)
            }
            SysexStatus::Start => {
                self.open = is_set_profile_on(packet);
                self.open
            }
            SysexStatus::Continue => self.open,
            SysexStatus::End => std::mem::take(&mut self.open),
        }
    }
}

/// How many packets at the head of `header` are Set Profile On messages at
/// tick 0: the ones a write puts before the DCTPQ, where they carry no DCS.
pub(crate) fn leading_profile_len(header: &[(u64, Ump)]) -> usize {
    let mut scan = ProfileScan::default();
    header
        .iter()
        .take_while(|(tick, ump)| *tick == 0 && decode(*ump).is_ok_and(|m| scan.accepts(&m)))
        .count()
}

/// The header's own rules (M2-116 sections 6.1 to 6.1.2) over `header`, whose
/// first `pre` packets precede the DCTPQ: at most one Set Tempo and one Set
/// Time Signature; the Set Tempo first after the DCTPQ at delta 0; the Set
/// Time Signature right behind it (first, if there is no Set Tempo).
///
/// # Errors
///
/// The [`ClipFileError`] naming the rule broken.
pub(crate) fn header_rules(header: &[(u64, Ump)], pre: usize) -> Result<(), ClipFileError> {
    let (mut tempo, mut signature) = (false, false);
    for (i, (tick, ump)) in header[pre..].iter().enumerate() {
        match decode(*ump) {
            Ok(Message::Flex(_, Flex::SetTempo(_))) => {
                if tempo {
                    return Err(ClipFileError::DuplicateHeaderTempo);
                }
                if i != 0 || *tick != 0 {
                    return Err(ClipFileError::MisplacedHeaderTempo);
                }
                tempo = true;
            }
            Ok(Message::Flex(_, Flex::SetTimeSignature(_))) => {
                if signature {
                    return Err(ClipFileError::DuplicateHeaderTimeSignature);
                }
                if i != usize::from(tempo) {
                    return Err(ClipFileError::MisplacedHeaderTimeSignature);
                }
                signature = true;
            }
            _ => {}
        }
    }
    Ok(())
}

/// Serializes a clip: the leading Set Profile On packets (no DCS), the DCTPQ,
/// the rest of the header, Start of Clip, each event behind its Delta
/// Clockstamp, and End of Clip behind the DCS that makes the clip `end` ticks
/// long. `header` and `events` are absolute ticks in order (the header's on
/// its own clock), `events` none past `end`, no two more than `u32::MAX` apart
/// (the caller checks; [`MidiFile`] does).
///
/// [`MidiFile`]: crate::MidiFile
pub(crate) fn write(
    ticks_per_quarter: u16,
    header: &[(u64, Ump)],
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

    // Start of Clip (preceded by a zero DCS: the sequence begins here).
    push_words(&mut out, delta_clockstamp(0).words());
    push_words(&mut out, start_of_clip().words());

    // The timed event stream: each UMP preceded by its delta clockstamp(s).
    // A DCS field is only 20 bits, so a delta beyond `DCS_MAX` is expressed as
    // DCS*NOOP restarts (M2-116 section 3.2.2): see `push_delta`.
    let mut at = 0u64;
    for &(tick, ump) in events {
        push_delta(&mut out, tick.saturating_sub(at));
        push_words(&mut out, ump.words());
        at = tick;
    }

    // End of Clip, behind the DCS that is the clip's length.
    push_delta(&mut out, end.saturating_sub(at));
    push_words(&mut out, end_of_clip().words());
    out
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

/// The big-endian 32-bit words of `bytes`, `None` if it is not a whole number
/// of them.
fn words_of(bytes: &[u8]) -> Option<Vec<u32>> {
    // `as_chunks`: the remainder is empty once the length is a multiple of 4,
    // and each chunk arrives as the `[u8; 4]` `from_be_bytes` takes.
    let (chunks, rest) = bytes.as_chunks::<4>();
    rest.is_empty()
        .then(|| chunks.iter().map(|c| u32::from_be_bytes(*c)).collect())
}
