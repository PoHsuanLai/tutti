//! Standard MIDI File bytes: the wire half under [`MidiFile`](crate::MidiFile),
//! over `midly`, which stays private.
//!
//! `midly` parses and frames the chunks; everything it hands back is copied
//! into this crate's own types ([`FileEvent`], [`Meta`]), so none of its types
//! (which borrow the input) is in a signature.
//!
//! # Everything here is 7-bit
//!
//! An SMF cannot express MIDI 2.0 resolution: a channel message is a
//! [`Voice1`] on group 0, and the MIDI 2.0 side of the crate (the Clip File)
//! is a *different format*, not a wider encoding of this one.
//!
//! # Truncation
//!
//! By default `midly` plows through a cut file, keeping what it could read.
//! The `strict` feature is on instead, so a chunk that runs past the end of the
//! file, a track that ends mid-event and a track count that disagrees with the
//! header are errors ([`Error::MidiFileParse`]); [`scan`] reports the first of
//! these, a file cut off, as [`Error::Truncated`] before `midly` runs.

use midly::num::{u4, u7, u14, u24, u28};
use midly::{Format, Header, MetaMessage, MidiMessage, Smf, Timing, TrackEvent, TrackEventKind};
use tutti_ump::{Address, CCNumber, Message, MidiChannel, NoteNumber, U7, U14, Voice1};

use crate::file::{FileEvent, Meta, SmfFormat, TextKind, Tick, Track};
use crate::{Error, Result};

/// What an SMF holds, read: the header's format and division and the tracks.
pub(crate) struct SmfParts {
    pub(crate) format: SmfFormat,
    pub(crate) ticks_per_quarter: u16,
    pub(crate) tracks: Vec<Track>,
}

/// `midly`'s refusal as this crate's.
fn parse_error(e: midly::Error) -> Error {
    Error::MidiFileParse(e.to_string())
}

/// Whether `bytes` is cut off: a chunk header short of 8 bytes, a chunk whose
/// declared length runs past the end, or fewer track chunks than the header
/// declares. SMF 1.0 section 2: a chunk is its type, its length and that many
/// bytes, and the header's `ntrks` says how many `MTrk` chunks follow.
///
/// # Errors
///
/// [`Error::Truncated`] for the first of these.
fn scan(bytes: &[u8]) -> Result<()> {
    let mut rest = bytes;
    let mut declared = None;
    let mut found = 0u32;
    while !rest.is_empty() {
        let [a, b, c, d, l0, l1, l2, l3, ..] = *rest else {
            return Err(Error::Truncated);
        };
        let len = u32::from_be_bytes([l0, l1, l2, l3]) as usize;
        let body = rest.get(8..).unwrap_or_default();
        if body.len() < len {
            return Err(Error::Truncated);
        }
        match &[a, b, c, d] {
            b"MThd" if declared.is_none() => {
                // `ntrks` is the second 16-bit field of the header's body.
                let ntrks = body.get(2..4).ok_or(Error::Truncated)?;
                declared = Some(u32::from(u16::from_be_bytes([ntrks[0], ntrks[1]])));
            }
            b"MTrk" => found += 1,
            _ => {}
        }
        rest = &body[len..];
    }
    match declared {
        Some(n) if found < n => Err(Error::Truncated),
        _ => Ok(()),
    }
}

/// Parses SMF bytes.
///
/// # Errors
///
/// - [`Error::Truncated`] if the file is cut off ([`scan`]).
/// - [`Error::MidiUnsupportedTiming`] if the header declares SMPTE/timecode
///   division: a timecode file has no beat grid.
/// - [`Error::MidiFileParse`] if the division is zero, or `midly` rejects the
///   bytes.
pub(crate) fn read(bytes: &[u8]) -> Result<SmfParts> {
    scan(bytes)?;
    let smf = Smf::parse(bytes).map_err(parse_error)?;
    let ticks_per_quarter = match smf.header.timing {
        Timing::Metrical(tpq) => tpq.as_int(),
        Timing::Timecode(..) => return Err(Error::MidiUnsupportedTiming),
    };
    // The divisor of every beat in the file: a zero would put every event at
    // infinity.
    if ticks_per_quarter == 0 {
        return Err(Error::MidiFileParse("zero ticks-per-beat".into()));
    }
    let format = match smf.header.format {
        Format::SingleTrack => SmfFormat::SingleTrack,
        Format::Parallel => SmfFormat::Parallel,
        Format::Sequential => SmfFormat::Sequential,
    };
    Ok(SmfParts {
        format,
        ticks_per_quarter,
        tracks: smf.tracks.iter().map(|t| track_of(t)).collect(),
    })
}

fn track_of(track: &[TrackEvent<'_>]) -> Track {
    let mut events = Vec::with_capacity(track.len());
    let mut tick = 0u64;
    let mut end = None;
    for (i, event) in track.iter().enumerate() {
        tick += u64::from(event.delta.as_int());
        let kind = match event.kind {
            TrackEventKind::Midi { channel, message } => FileEvent::Message(Message::Midi1(
                Address::first(MidiChannel::masked(channel.as_int())),
                voice_of(message),
            )),
            TrackEventKind::SysEx(data) => FileEvent::SysEx(data.into()),
            TrackEventKind::Escape(data) => FileEvent::Escape(data.into()),
            // The last End of Track is the track's length, not an event.
            TrackEventKind::Meta(MetaMessage::EndOfTrack) if i + 1 == track.len() => {
                end = Some(tick);
                continue;
            }
            TrackEventKind::Meta(meta) => FileEvent::Meta(meta_of(meta)),
        };
        events.push((Tick(tick), kind));
    }
    Track {
        end: Tick(end.unwrap_or(tick)),
        events,
    }
}

fn voice_of(message: MidiMessage) -> Voice1 {
    // `midly`'s numbers are already framed to 7 and 14 bits, so `masked`
    // drops nothing.
    let note = |k: u7| NoteNumber::masked(k.as_int());
    let seven = |v: u7| U7::masked(v.as_int());
    match message {
        MidiMessage::NoteOff { key, vel } => Voice1::NoteOff {
            note: note(key),
            velocity: seven(vel),
        },
        MidiMessage::NoteOn { key, vel } => Voice1::NoteOn {
            note: note(key),
            velocity: seven(vel),
        },
        MidiMessage::Aftertouch { key, vel } => Voice1::PolyPressure {
            note: note(key),
            pressure: seven(vel),
        },
        MidiMessage::Controller { controller, value } => Voice1::ControlChange {
            controller: CCNumber::masked(controller.as_int()),
            value: seven(value),
        },
        MidiMessage::ProgramChange { program } => Voice1::ProgramChange(seven(program)),
        MidiMessage::ChannelAftertouch { vel } => Voice1::ChannelPressure(seven(vel)),
        MidiMessage::PitchBend { bend } => Voice1::PitchBend(U14::masked(bend.0.as_int())),
    }
}

/// The inverse of [`voice_of`].
fn midly_of(voice: Voice1) -> MidiMessage {
    let note = |n: NoteNumber| u7::from(n.get());
    let seven = |v: U7| u7::from(v.get());
    match voice {
        Voice1::NoteOff { note: n, velocity } => MidiMessage::NoteOff {
            key: note(n),
            vel: seven(velocity),
        },
        Voice1::NoteOn { note: n, velocity } => MidiMessage::NoteOn {
            key: note(n),
            vel: seven(velocity),
        },
        Voice1::PolyPressure { note: n, pressure } => MidiMessage::Aftertouch {
            key: note(n),
            vel: seven(pressure),
        },
        Voice1::ControlChange { controller, value } => MidiMessage::Controller {
            controller: u7::from(controller.get()),
            value: seven(value),
        },
        Voice1::ProgramChange(program) => MidiMessage::ProgramChange {
            program: seven(program),
        },
        Voice1::ChannelPressure(pressure) => MidiMessage::ChannelAftertouch {
            vel: seven(pressure),
        },
        Voice1::PitchBend(bend) => MidiMessage::PitchBend {
            bend: midly::PitchBend(u14::from(bend.get())),
        },
    }
}

fn meta_of(meta: MetaMessage<'_>) -> Meta {
    let text = |kind, bytes: &[u8]| Meta::Text {
        kind,
        bytes: bytes.into(),
    };
    let other = |kind: u8, data: &[u8]| Meta::Other {
        kind,
        data: data.into(),
    };
    match meta {
        MetaMessage::Text(b) => text(TextKind::Text, b),
        MetaMessage::Copyright(b) => text(TextKind::Copyright, b),
        MetaMessage::TrackName(b) => text(TextKind::TrackName, b),
        MetaMessage::InstrumentName(b) => text(TextKind::InstrumentName, b),
        MetaMessage::Lyric(b) => text(TextKind::Lyric, b),
        MetaMessage::Marker(b) => text(TextKind::Marker, b),
        MetaMessage::CuePoint(b) => text(TextKind::CuePoint, b),
        MetaMessage::ProgramName(b) => text(TextKind::ProgramName, b),
        MetaMessage::DeviceName(b) => text(TextKind::DeviceName, b),
        MetaMessage::Tempo(t) => Meta::Tempo {
            micros_per_quarter: t.as_int(),
        },
        MetaMessage::TimeSignature(n, d, c, t) => Meta::TimeSignature {
            numerator: n,
            denominator_power: d,
            clocks_per_click: c,
            thirty_seconds_per_quarter: t,
        },
        MetaMessage::KeySignature(sharps, minor) => Meta::KeySignature { sharps, minor },
        // The rest are kept as their bytes: no consumer here reads them, and
        // the bytes are what a write must give back.
        MetaMessage::TrackNumber(n) => match n {
            Some(n) => other(0x00, &n.to_be_bytes()),
            None => other(0x00, &[]),
        },
        MetaMessage::MidiChannel(c) => other(0x20, &[c.as_int()]),
        MetaMessage::MidiPort(p) => other(0x21, &[p.as_int()]),
        // Only a mid-track End of Track gets here: the last is the track's end.
        MetaMessage::EndOfTrack => other(0x2F, &[]),
        MetaMessage::SmpteOffset(t) => other(
            0x54,
            &[
                t.hour() | (fps_code(t.fps()) << 5),
                t.minute(),
                t.second(),
                t.frame(),
                t.subframe(),
            ],
        ),
        MetaMessage::SequencerSpecific(b) => other(0x7F, b),
        MetaMessage::Unknown(kind, b) => other(kind, b),
    }
}

/// The 2-bit frame-rate code of an SMPTE offset (SMF 1.0 section 3.2.?: 0 is
/// 24, 1 is 25, 2 is 29.97 drop-frame, 3 is 30).
fn fps_code(fps: midly::Fps) -> u8 {
    match fps {
        midly::Fps::Fps24 => 0,
        midly::Fps::Fps25 => 1,
        midly::Fps::Fps29 => 2,
        midly::Fps::Fps30 => 3,
    }
}

/// Writes `tracks` as an SMF of `format` and division `ticks_per_quarter`.
///
/// Each track ends with an End of Track at its `end`. A caller has checked the
/// tracks ([`MidiFile`](crate::MidiFile)'s constructor): events in order, a
/// delta that fits the wire's 28 bits, only group-0 MIDI 1.0 messages.
///
/// # Errors
///
/// [`Error::MidiFileParse`] if `midly` refuses to serialise (more than 65535
/// tracks, an event of 256 MB).
pub(crate) fn write(
    format: SmfFormat,
    ticks_per_quarter: u16,
    tracks: &[Track],
) -> Result<Vec<u8>> {
    let format = match format {
        SmfFormat::SingleTrack => Format::SingleTrack,
        SmfFormat::Parallel => Format::Parallel,
        SmfFormat::Sequential => Format::Sequential,
    };
    let header = Header::new(format, Timing::Metrical(ticks_per_quarter.into()));
    let mut smf = Smf::new(header);
    smf.tracks = tracks.iter().map(midly_track).collect();
    let mut out = Vec::new();
    smf.write_std(&mut out)
        .map_err(|e| Error::MidiFileParse(format!("failed to encode MIDI: {e}")))?;
    Ok(out)
}

fn midly_track(track: &Track) -> Vec<TrackEvent<'_>> {
    let delta = |from: Tick, to: Tick| u28::from((to.0 - from.0) as u32);
    let mut out = Vec::with_capacity(track.events.len() + 1);
    let mut at = Tick(0);
    for (tick, event) in &track.events {
        let kind = match event {
            FileEvent::Message(Message::Midi1(address, voice)) => TrackEventKind::Midi {
                channel: u4::from(address.channel.get()),
                message: midly_of(*voice),
            },
            FileEvent::SysEx(data) => TrackEventKind::SysEx(data),
            FileEvent::Escape(data) => TrackEventKind::Escape(data),
            FileEvent::Meta(meta) => TrackEventKind::Meta(midly_meta(meta)),
            // Refused at construction: not an SMF event.
            FileEvent::Message(_) | FileEvent::Unknown(_) => continue,
        };
        out.push(TrackEvent {
            delta: delta(at, *tick),
            kind,
        });
        at = *tick;
    }
    out.push(TrackEvent {
        delta: delta(at, track.end),
        kind: TrackEventKind::Meta(MetaMessage::EndOfTrack),
    });
    out
}

fn midly_meta(meta: &Meta) -> MetaMessage<'_> {
    match meta {
        Meta::Text { kind, bytes } => match kind {
            TextKind::Text => MetaMessage::Text(bytes),
            TextKind::Copyright => MetaMessage::Copyright(bytes),
            TextKind::TrackName => MetaMessage::TrackName(bytes),
            TextKind::InstrumentName => MetaMessage::InstrumentName(bytes),
            TextKind::Lyric => MetaMessage::Lyric(bytes),
            TextKind::Marker => MetaMessage::Marker(bytes),
            TextKind::CuePoint => MetaMessage::CuePoint(bytes),
            TextKind::ProgramName => MetaMessage::ProgramName(bytes),
            TextKind::DeviceName => MetaMessage::DeviceName(bytes),
        },
        // Checked to fit 24 bits at construction.
        Meta::Tempo { micros_per_quarter } => MetaMessage::Tempo(u24::from(*micros_per_quarter)),
        Meta::TimeSignature {
            numerator,
            denominator_power,
            clocks_per_click,
            thirty_seconds_per_quarter,
        } => MetaMessage::TimeSignature(
            *numerator,
            *denominator_power,
            *clocks_per_click,
            *thirty_seconds_per_quarter,
        ),
        Meta::KeySignature { sharps, minor } => MetaMessage::KeySignature(*sharps, *minor),
        // `Unknown` writes its kind and bytes verbatim, whatever the kind:
        // the typed variants `midly` would parse them to have the same bytes.
        Meta::Other { kind, data } => MetaMessage::Unknown(*kind, data),
    }
}
