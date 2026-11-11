//! The reader: SMF bytes into [`SmfParts`], built from `winnow` combinators.
//!
//! # Shape
//!
//! Every parser is `fn(&mut &[u8]) -> Res<T>` (or a function returning one),
//! composed with `?` and winnow's combinators; there is no cursor type and no
//! index arithmetic. Two passes, in this order:
//!
//! 1. **Framing** ([`chunk`], [`smf`]): the header chunk, then chunks of
//!    `type, length, body` until the input ends. A chunk cut off anywhere is
//!    [`Fault::Truncated`]; an unknown chunk type is skipped (SMF 1.0 section
//!    2: "your programs should expect alien chunks and treat them as if they
//!    weren't there").
//! 2. **Tracks** ([`track`]): each `MTrk` body into events.
//!
//! # Running status is a value
//!
//! [`event`] takes the running status and returns the next one beside the
//! event; [`steps`] threads it with `successors`. Nothing hides the status in
//! the input type, so one event is a function of its bytes and the status
//! before it. SMF 1.0 section 3 (Track Chunks): a channel event may omit its
//! status byte when it equals the previous event's, and a sysex or meta event
//! cancels the running status. Real-time and system-common statuses are not in
//! a file at all (section 3.4: `F1` to `F6` and `F8` to `FE` are undefined
//! there), so they are refused.
//!
//! # What decodes a message
//!
//! The combinators frame the file; a channel message is `tutti-ump`'s: its
//! data-byte count is `Voice1::data_len` and the status and data bytes become
//! a message through `Midi1Parser::message`, the one-message decode. Track
//! bytes are never fed to `Midi1Parser::feed` as a stream: in a file the delta
//! bytes would read as running-status data and `FF` (meta) as a real-time
//! Reset, so the streaming parser is wrong for file framing.
//!
//! # What this reads strictly
//!
//! A chunk that runs past the end, fewer tracks than the header declares
//! ([`Fault::Truncated`]); more tracks than declared, a format 0 file without
//! exactly one track, a second header chunk, a data byte with its top bit set,
//! a running status with none active, a variable-length quantity over four
//! bytes, an event cut off inside its chunk, a zero division
//! ([`Fault::Malformed`]); and an SMPTE division ([`Fault::Timecode`]). A track
//! with no End of Track ends at its last event, and an End of Track that is
//! not the track's last event is refused (SMF 1.0 section 3.3: it "must occur
//! as the last event in the track").

use std::iter::successors;

use winnow::Parser;
use winnow::binary::{be_u16, be_u32, length_take, u8 as byte};
use winnow::combinator::{alt, dispatch, eof, fail, peek, repeat, repeat_till};
use winnow::error::ContextError;
use winnow::token::{literal, take};

use tutti_ump::{Midi1Parser, MidiGroup, Voice1};

use super::SmfParts;
use crate::file::{FileEvent, Meta, SmfFormat, TextKind, Tick, Track};
use crate::{Error, Result};

/// Why a parser refused, carried as winnow context and mapped to [`enum@Error`]
/// once, in [`read`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fault {
    /// The input ended inside a chunk, or before the declared tracks.
    Truncated,
    /// SMPTE division: no beat grid.
    Timecode,
    /// Not a well-formed SMF, and what.
    Malformed(&'static str),
}

type Res<T> = std::result::Result<T, ContextError<Fault>>;

/// The running status: the last channel status byte, if no sysex or meta
/// event has cancelled it.
type Status = Option<u8>;

fn malformed(what: &'static str) -> Fault {
    Fault::Malformed(what)
}

/// A parser that always refuses, for `what`.
fn refuse<'a, T>(what: &'static str) -> impl Parser<&'a [u8], T, ContextError<Fault>> {
    fail.context(malformed(what))
}

// ---- primitives ------------------------------------------------------------

/// A variable-length quantity (SMF 1.0 section 1.1): seven bits a byte,
/// most significant first, the top bit set on every byte but the last, and at
/// most four bytes (28 bits).
fn vlq(input: &mut &[u8]) -> Res<u32> {
    (
        repeat(0..=3, byte.verify(|b| b & 0x80 != 0))
            .fold(|| 0u32, |acc, b| acc << 7 | u32::from(b & 0x7F)),
        byte.verify(|b| b & 0x80 == 0),
    )
        .map(|(high, last)| high << 7 | u32::from(last))
        .context(malformed("variable-length quantity"))
        .parse_next(input)
}

/// A length (a `u32` or a vlq) and that many bytes.
fn counted<'a>(
    length: impl Parser<&'a [u8], u32, ContextError<Fault>>,
) -> impl Parser<&'a [u8], &'a [u8], ContextError<Fault>> {
    length_take(length.map(|n| n as usize))
}

// ---- framing ---------------------------------------------------------------

/// What the header chunk says.
struct Header {
    format: SmfFormat,
    tracks: u16,
    ticks_per_quarter: u16,
}

/// The division word: ticks per quarter note, or (top bit set) an SMPTE
/// frame rate and ticks per frame (SMF 1.0 section 2.1), which is refused.
fn division(input: &mut &[u8]) -> Res<u16> {
    let raw = be_u16.parse_next(input)?;
    let [high, _] = raw.to_be_bytes();
    // With the top bit set, the high byte is the negated frame rate as an i8.
    match (raw, i8::from_be_bytes([high])) {
        (0, _) => refuse("zero ticks-per-beat").parse_next(input),
        (_, -24 | -25 | -29 | -30) => fail.context(Fault::Timecode).parse_next(input),
        (_, ..0) => refuse("invalid SMPTE frame rate").parse_next(input),
        (raw, _) => Ok(raw),
    }
}

/// The header chunk's body: format, track count, division. Bytes after the
/// first six are ignored (section 2.1: a reader skips what a later revision
/// adds).
fn header_body(input: &mut &[u8]) -> Res<Header> {
    let format = be_u16
        .verify_map(|f| match f {
            0 => Some(SmfFormat::SingleTrack),
            1 => Some(SmfFormat::Parallel),
            2 => Some(SmfFormat::Sequential),
            _ => None,
        })
        .context(malformed("unknown format"))
        .parse_next(input)?;
    let tracks = be_u16.parse_next(input)?;
    let ticks_per_quarter = division.parse_next(input)?;
    Ok(Header {
        format,
        tracks,
        ticks_per_quarter,
    })
}

/// The header chunk: `MThd`, a length of at least six, the body.
fn header(input: &mut &[u8]) -> Res<Header> {
    literal(b"MThd")
        .context(malformed("missing header chunk"))
        .parse_next(input)?;
    let mut body = counted(be_u32)
        .context(Fault::Truncated)
        .parse_next(input)?;
    header_body
        .context(malformed("header chunk shorter than six bytes"))
        .parse_next(&mut body)
}

/// A chunk after the header: a track's body to decode later, or one skipped.
enum Chunk<'a> {
    Track(&'a [u8]),
    Alien,
}

/// `type, length, body`, framed and not yet read.
fn chunk<'a>(input: &mut &'a [u8]) -> Res<Chunk<'a>> {
    let (kind, body) = (take(4usize), counted(be_u32))
        .context(Fault::Truncated)
        .parse_next(input)?;
    match kind {
        b"MTrk" => Ok(Chunk::Track(body)),
        b"MThd" => refuse("a second header chunk").parse_next(input),
        _ => Ok(Chunk::Alien),
    }
}

/// The whole file, framed: the header and the bodies of its `MTrk` chunks.
fn smf<'a>(input: &mut &'a [u8]) -> Res<(Header, Vec<&'a [u8]>)> {
    let header = header.parse_next(input)?;
    let (chunks, _): (Vec<_>, _) = repeat_till(0.., chunk, eof).parse_next(input)?;
    let tracks = chunks
        .into_iter()
        .filter_map(|c| match c {
            Chunk::Track(body) => Some(body),
            Chunk::Alien => None,
        })
        .collect();
    Ok((header, tracks))
}

// ---- events ----------------------------------------------------------------

/// The data bytes of a channel status (how many is `Voice1::data_len`'s) as a
/// message on group 0. `Midi1Parser::message` owns the status bit layout.
fn channel<'a>(status: u8) -> impl Parser<&'a [u8], FileEvent, ContextError<Fault>> {
    let count = Voice1::data_len(status).map_or(0, usize::from);
    take(count)
        .verify_map(move |data: &[u8]| {
            let bytes = match *data {
                [a] => [status, a, 0],
                [a, b] => [status, a, b],
                _ => return None,
            };
            Midi1Parser::message(MidiGroup::FIRST, &bytes).map(FileEvent::Message)
        })
        .context(malformed("channel message data"))
}

/// A meta event's type byte and data as the model's [`Meta`]. A fixed-size
/// event with more data than it defines is read for the bytes it defines and
/// keeps the rest as `extra` (SMF 1.0 section 3.3: "they shouldn't be surprised
/// if it's bigger than they expected. If so, they must ignore everything past
/// what they know about"). Anything shorter than the typed shape stays
/// `Other`, so no byte is lost.
fn meta_of(kind: u8, data: &[u8]) -> Meta {
    let text = |kind| Meta::Text {
        kind,
        bytes: data.into(),
    };
    match (kind, data) {
        (0x01, _) => text(TextKind::Text),
        (0x02, _) => text(TextKind::Copyright),
        (0x03, _) => text(TextKind::TrackName),
        (0x04, _) => text(TextKind::InstrumentName),
        (0x05, _) => text(TextKind::Lyric),
        (0x06, _) => text(TextKind::Marker),
        (0x07, _) => text(TextKind::CuePoint),
        (0x08, _) => text(TextKind::ProgramName),
        (0x09, _) => text(TextKind::DeviceName),
        (0x51, &[a, b, c, ref extra @ ..]) => Meta::Tempo {
            micros_per_quarter: u32::from_be_bytes([0, a, b, c]),
            extra: extra.into(),
        },
        (
            0x58,
            &[
                numerator,
                denominator_power,
                clocks_per_click,
                thirty_seconds_per_quarter,
                ref extra @ ..,
            ],
        ) => Meta::TimeSignature {
            numerator,
            denominator_power,
            clocks_per_click,
            thirty_seconds_per_quarter,
            extra: extra.into(),
        },
        (0x59, &[sharps, minor @ (0 | 1), ref extra @ ..]) => Meta::KeySignature {
            sharps: i8::from_be_bytes([sharps]),
            minor: minor == 1,
            extra: extra.into(),
        },
        _ => Meta::Other {
            kind,
            data: data.into(),
        },
    }
}

/// A meta event after its `FF`: type, length, data.
fn meta(input: &mut &[u8]) -> Res<FileEvent> {
    (byte, counted(vlq))
        .map(|(kind, data)| FileEvent::Meta(meta_of(kind, data)))
        .parse_next(input)
}

/// One event after its delta, given the running status: the event and the
/// status after it. A status byte is read when the next byte has its top bit
/// set; otherwise the running one stands in and the byte is data.
fn event<'a>(
    running: Status,
) -> impl Parser<&'a [u8], (u32, FileEvent, Status), ContextError<Fault>> {
    let status = alt((
        byte.verify(|b| b & 0x80 != 0),
        peek(byte.verify(|b| b & 0x80 == 0)).verify_map(move |_| running),
    ))
    .context(malformed("data byte with no running status"));
    (vlq, status).flat_map(|(delta, status)| {
        dispatch! { move |_: &mut &'a [u8]| Ok::<u8, ContextError<Fault>>(status);
            0xFF => meta.map(|e| (e, None)),
            0xF0 => counted(vlq).map(|d| (FileEvent::SysEx(d.into()), None)),
            0xF7 => counted(vlq).map(|d| (FileEvent::Escape(d.into()), None)),
            0x80..=0xEF => channel(status).map(move |e| (e, Some(status))),
            _ => refuse("system common or real-time status in a file"),
        }
        .map(move |(event, status)| (delta, event, status))
    })
}

/// One step of a track: the event at `delta`, the bytes after it, and the
/// status after it. `None` at the end of the bytes.
struct Step<'a> {
    delta: u32,
    event: FileEvent,
    rest: &'a [u8],
    status: Status,
}

fn step(input: &[u8], running: Status) -> Res<Option<Step<'_>>> {
    alt((eof.value(None), event(running).map(Some)))
        .parse_peek(input)
        .map(|(rest, parsed)| {
            parsed.map(|(delta, event, status)| Step {
                delta,
                event,
                rest,
                status,
            })
        })
}

/// Every step of a track body, an error ending the sequence.
fn steps(body: &[u8]) -> impl Iterator<Item = Res<Step<'_>>> {
    successors(Some(step(body, None)), |prev| match prev {
        Ok(Some(s)) => Some(step(s.rest, s.status)),
        _ => None,
    })
    .map_while(std::result::Result::transpose)
}

/// A track body: its events at absolute ticks, and its end. The last event,
/// if it is an End of Track (`FF 2F 00`), is the track's end and not an event.
fn track(body: &[u8]) -> Res<Track> {
    let mut events = steps(body)
        .scan(0u64, |tick, step| {
            Some(step.map(|s| {
                *tick += u64::from(s.delta);
                (Tick(*tick), s.event)
            }))
        })
        .collect::<Res<Vec<_>>>()?;
    let is_end = |e: &FileEvent| matches!(e, FileEvent::Meta(Meta::Other { kind: 0x2F, data }) if data.is_empty());
    // SMF 1.0 section 3.3: "An end-of-track event must occur as the last
    // event in the track." One before the last is refused, not read as the
    // track's end: what follows it would be dropped, and the model is lossless.
    let last = events.len().saturating_sub(1);
    if events.iter().take(last).any(|(_, e)| is_end(e)) {
        return Err(
            refuse::<Track>("End of Track that is not the last event in its track")
                .parse_peek(body)
                .unwrap_err(),
        );
    }
    let end = match events.last() {
        Some(&(tick, ref e)) if is_end(e) => {
            events.pop();
            tick
        }
        Some(&(tick, _)) => tick,
        None => Tick(0),
    };
    Ok(Track {
        header: Vec::new(),
        header_end: Tick(0),
        events,
        end,
    })
}

// ---- the edge --------------------------------------------------------------

/// The one place winnow's error becomes ours.
fn error_of(e: &ContextError<Fault>) -> Error {
    let faults: Vec<Fault> = e.context().copied().collect();
    if faults.contains(&Fault::Truncated) {
        Error::Truncated
    } else if faults.contains(&Fault::Timecode) {
        Error::MidiUnsupportedTiming
    } else {
        let what = faults.iter().find_map(|f| match f {
            Fault::Malformed(w) => Some(*w),
            _ => None,
        });
        Error::MidiFileParse(what.unwrap_or("unexpected bytes").into())
    }
}

/// Parses SMF bytes.
///
/// # Errors
///
/// - [`Error::Truncated`] if the file is cut off.
/// - [`Error::MidiUnsupportedTiming`] for an SMPTE division.
/// - [`Error::MidiFileParse`] for anything else malformed.
pub(crate) fn read(bytes: &[u8]) -> Result<SmfParts> {
    let (header, bodies) = smf.parse(bytes).map_err(|e| error_of(e.inner()))?;
    let declared = usize::from(header.tracks);
    // Fewer than declared is a cut file; more, or a format 0 with other
    // than one, disagrees with its own header.
    if bodies.len() < declared {
        return Err(Error::Truncated);
    }
    if bodies.len() > declared {
        return Err(Error::MidiFileParse(
            "more tracks than the header declares".into(),
        ));
    }
    if header.format == SmfFormat::SingleTrack && declared != 1 {
        return Err(Error::MidiFileParse("a format 0 file has one track".into()));
    }
    let tracks = bodies
        .iter()
        .map(|body| track(body))
        .collect::<Res<Vec<_>>>()
        .map_err(|e| error_of(&e))?;
    Ok(SmfParts {
        format: header.format,
        ticks_per_quarter: header.ticks_per_quarter,
        tracks,
    })
}
