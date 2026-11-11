//! The writer: the reader's inverse, a fold of pure functions.
//!
//! An event is a pure function to its bytes ([`encode`]); a track body is a
//! fold of events over the running status ([`body`]), which elides a channel
//! status byte equal to the one before it (SMF 1.0 section 3: running status;
//! sysex and meta cancel it); a chunk's length is computed from the body it
//! frames ([`chunk`]), never patched in afterwards.

use tutti_ump::{Message, Midi1Bytes};

use crate::file::{FileEvent, Meta, SmfFormat, TextKind, Tick, Track};
use crate::{ConfigError, Error, Result};

fn too_large() -> Error {
    ConfigError::TooLarge.into()
}

/// The widest value a variable-length quantity holds: 28 bits.
const VLQ_MAX: u32 = 0x0FFF_FFFF;

/// The variable-length quantity of `n` (SMF 1.0 section 1.1), `None` past 28
/// bits: seven bits a byte, most significant first, the top bit set on all
/// but the last.
fn vlq(n: u32) -> Option<impl Iterator<Item = u8>> {
    (n <= VLQ_MAX).then(|| {
        (1..4u32)
            .rev()
            .filter(move |i| n >> (7 * i) != 0)
            .chain(std::iter::once(0))
            .map(move |i| (n >> (7 * i)) as u8 & 0x7F | if i > 0 { 0x80 } else { 0 })
    })
}

/// An event's bytes, status byte first.
enum Encoded {
    /// A channel message: the only kind running status applies to.
    Channel(Midi1Bytes),
    /// Sysex, escape or meta.
    Other(Vec<u8>),
}

impl Encoded {
    fn bytes(&self) -> &[u8] {
        match self {
            Self::Channel(b) => b.as_slice(),
            Self::Other(v) => v,
        }
    }
}

/// `lead`, the length of `data` as a vlq, then `data`.
fn counted(lead: &[u8], data: &[u8]) -> Result<Vec<u8>> {
    let length = u32::try_from(data.len())
        .ok()
        .and_then(vlq)
        .ok_or_else(too_large)?;
    Ok(lead
        .iter()
        .copied()
        .chain(length)
        .chain(data.iter().copied())
        .collect())
}

/// A meta event's type byte and data.
fn meta_parts(meta: &Meta) -> (u8, Vec<u8>) {
    match meta {
        Meta::Text { kind, bytes } => (
            match kind {
                TextKind::Text => 0x01,
                TextKind::Copyright => 0x02,
                TextKind::TrackName => 0x03,
                TextKind::InstrumentName => 0x04,
                TextKind::Lyric => 0x05,
                TextKind::Marker => 0x06,
                TextKind::CuePoint => 0x07,
                TextKind::ProgramName => 0x08,
                TextKind::DeviceName => 0x09,
            },
            bytes.to_vec(),
        ),
        // Checked to fit 24 bits at construction: the high byte is zero.
        Meta::Tempo {
            micros_per_quarter,
            extra,
        } => (0x51, {
            let [_, a, b, c] = micros_per_quarter.to_be_bytes();
            [&[a, b, c][..], extra].concat()
        }),
        Meta::TimeSignature {
            numerator,
            denominator_power,
            clocks_per_click,
            thirty_seconds_per_quarter,
            extra,
        } => (
            0x58,
            [
                &[
                    *numerator,
                    *denominator_power,
                    *clocks_per_click,
                    *thirty_seconds_per_quarter,
                ][..],
                extra,
            ]
            .concat(),
        ),
        Meta::KeySignature {
            sharps,
            minor,
            extra,
        } => (
            0x59,
            [&[sharps.cast_unsigned(), u8::from(*minor)][..], extra].concat(),
        ),
        Meta::Other { kind, data } => (*kind, data.to_vec()),
    }
}

/// One event as bytes. `Ok(None)` for what an SMF cannot hold (refused at
/// construction, so not reached).
fn encode(event: &FileEvent) -> Result<Option<Encoded>> {
    Ok(match event {
        FileEvent::Message(Message::Midi1(address, voice)) => {
            Some(Encoded::Channel(voice.bytes(address.channel)))
        }
        FileEvent::SysEx(data) => Some(Encoded::Other(counted(&[0xF0], data)?)),
        FileEvent::Escape(data) => Some(Encoded::Other(counted(&[0xF7], data)?)),
        FileEvent::Meta(meta) => {
            let (kind, data) = meta_parts(meta);
            Some(Encoded::Other(counted(&[0xFF, kind], &data)?))
        }
        FileEvent::Message(_) | FileEvent::Unknown(_) => None,
    })
}

/// A track body's fold state: the bytes so far, the running status, and the
/// tick of the last event.
type Fold = (Vec<u8>, Option<u8>, Tick);

/// The delta from `from` to `to` as vlq bytes.
fn delta(from: Tick, to: Tick) -> Result<impl Iterator<Item = u8>> {
    to.0.checked_sub(from.0)
        .and_then(|d| u32::try_from(d).ok())
        .and_then(vlq)
        .ok_or_else(too_large)
}

/// Folds one event onto the body: its delta, then its bytes without a status
/// byte the running status already holds.
fn append((mut out, running, at): Fold, (tick, event): &(Tick, FileEvent)) -> Result<Fold> {
    let Some(encoded) = encode(event)? else {
        return Ok((out, running, at));
    };
    let (&status, data) = encoded.bytes().split_first().ok_or_else(too_large)?;
    let is_channel = matches!(encoded, Encoded::Channel(_));
    out.extend(delta(at, *tick)?);
    out.extend(
        (!(is_channel && running == Some(status)))
            .then_some(status)
            .into_iter()
            .chain(data.iter().copied()),
    );
    Ok((out, is_channel.then_some(status), *tick))
}

/// A track's body: its events, then End of Track at its `end`.
fn body(track: &Track) -> Result<Vec<u8>> {
    let (out, _, at) = track
        .events
        .iter()
        .try_fold((Vec::new(), None, Tick(0)), append)?;
    Ok(out
        .into_iter()
        .chain(delta(at, track.end)?)
        .chain([0xFF, 0x2F, 0x00])
        .collect())
}

/// A chunk: its type, the length of `body`, the body.
fn chunk(kind: &[u8; 4], body: &[u8]) -> Result<Vec<u8>> {
    let len = u32::try_from(body.len()).map_err(|_| too_large())?;
    Ok(kind
        .iter()
        .copied()
        .chain(len.to_be_bytes())
        .chain(body.iter().copied())
        .collect())
}

/// Writes `tracks` as an SMF of `format` and division `ticks_per_quarter`.
///
/// Each track ends with an End of Track at its `end`. A caller has checked the
/// tracks ([`MidiFile`](crate::MidiFile)'s constructor): events in order, a
/// delta that fits the wire's 28 bits, only group-0 MIDI 1.0 messages.
///
/// # Errors
///
/// [`ConfigError::TooLarge`] if the SMF cannot hold it: more than 65535 tracks,
/// an event or a chunk too long for its length field.
pub(crate) fn write(
    format: SmfFormat,
    ticks_per_quarter: u16,
    tracks: &[Track],
) -> Result<Vec<u8>> {
    let format_word: u16 = match format {
        SmfFormat::SingleTrack => 0,
        SmfFormat::Parallel => 1,
        SmfFormat::Sequential => 2,
    };
    let count = u16::try_from(tracks.len()).map_err(|_| too_large())?;
    let header: Vec<u8> = [format_word, count, ticks_per_quarter]
        .into_iter()
        .flat_map(u16::to_be_bytes)
        .collect();
    let chunks = std::iter::once(chunk(b"MThd", &header))
        .chain(
            tracks
                .iter()
                .map(|t| body(t).and_then(|b| chunk(b"MTrk", &b))),
        )
        .collect::<Result<Vec<_>>>()?;
    Ok(chunks.concat())
}
