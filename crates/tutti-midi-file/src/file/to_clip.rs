//! [`MidiFile::to_clip`]: a Standard MIDI File as a MIDI 2.0 Clip File.
//!
//! M2-116 section 3.3: the SMF 1 "Meta Events" "have been replaced by new
//! MIDI messages in the UMP Format", mostly Flex Data (message type D), and
//! Appendix B lists them. This module is that replacement as a function.
//! What has no replacement is returned beside the clip, never dropped
//! silently.

use tutti_ump::{
    Flex, FlexAddress, FlexTextKind, KeySignature, Letter, Message, Metronome, MidiGroup,
    SharpsFlats, SysexMessage, TimeSignature, Ump, decode, flex_text_packets,
};

use unicode_normalization::UnicodeNormalization;

use crate::{FileEvent, Meta, MidiFile, Result, SmfFormat, TextKind, Tick, Track};

/// Why a file cannot be a clip.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ToClipError {
    /// A format 2 file holds independent sequences, each on its own
    /// timeline; a clip is one sequence (M2-116 section 7: one Start of Clip
    /// and one End of Clip), so there is no one clip to make of them.
    #[error("a format 2 file holds independent sequences; a clip is one")]
    Sequential,
    /// The file is a Clip File already.
    #[error("the file is a Clip File already")]
    AlreadyClip,
}

/// Why an SMF event has no place in the clip.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum DropReason {
    /// No UMP message replaces it (M2-116 Appendix B lists none): Sequence
    /// Number, Marker, Cue Point, Instrument, Program and Device Name,
    /// Sequencer Specific, any other meta event, and an `F7` escape.
    NoSuccessor,
    /// SMPTE Offset. M2-116 Appendix B names an SMPTE Offset message as its
    /// successor, "defined in" M2-104, but M2-104 v1.1.2 defines no such
    /// message, so there is nothing to write it as yet.
    SuccessorUndefined,
    /// Channel Prefix and MIDI Port Prefix. They address the meta events
    /// that follow to a channel or port, which the UMP does in the packet: a
    /// Flex Data message carries its own address and a group names the port.
    /// This conversion addresses every meta message to the whole group, so
    /// the prefix has nothing left to say.
    ReplacedByAddressing,
    /// The name of a track that is not the sequence's: a clip has no tracks
    /// to name (SMF 1.0 section 3.3: only the first track of a format 1 file
    /// names the sequence).
    TrackName,
    /// A text that is not UTF-8. Flex Data text is UTF-8 (M2-104 sections
    /// 1.8 and 7.5.9) and the SMF text is in no declared encoding, so it
    /// cannot be carried as it is.
    TextNotUtf8,
    /// A text with a NUL, which Flex Data text uses as its padding.
    TextHasNul,
    /// A Key Signature past 7 sharps or flats, which the Flex message does
    /// not hold.
    KeySignatureOutOfRange,
    /// A Time Signature with a denominator power of 0 (a whole note): the
    /// Flex field reads 0 as "non-standard" (M2-104 section 7.5.4).
    WholeNoteDenominator,
    /// A Time Signature with a numerator of 0, a bar of no beats: the Flex
    /// field holds "a value from 1 to 256" (M2-104 section 7.5.4).
    ZeroNumerator,
    /// A System Exclusive event that is not one complete message of 7-bit
    /// bytes from `F0` to `F7`: a divided message (its `F0` part has no `F7`)
    /// or one with an 8-bit byte. SysEx7 carries only the complete one.
    SysexNotCompleteSevenBit,
    /// A MIDI-CI Property Exchange message (Universal System Exclusive
    /// `7E`, sub-ID#1 `0D`, sub-ID#2 `30` to `3F`, M2-101 section 8):
    /// "Property Data for PE Resources shall not be included in a MIDI Clip
    /// File" (M2-116 section 6.4), and the sequence data may hold "any MIDI
    /// messages in UMP Format, except Property Exchange messages" (7.2).
    PropertyExchange,
    /// A text event with no bytes. SMF gives it no meaning, and the Flex
    /// message it would become does: a Complete Lyric packet "filled with the
    /// value 0x00 ... specifies a melisma" (M2-104 section 7.5.10.1).
    EmptyText,
}

/// One event of the SMF that is not in the clip.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Dropped {
    /// The track the event was in.
    pub track: usize,
    /// Where it was.
    pub tick: Tick,
    /// The event.
    pub event: FileEvent,
    /// Why it has no place.
    pub reason: DropReason,
}

/// The result of [`MidiFile::to_clip`]: the clip and what it left out.
///
/// A conversion that returned the clip alone would lose events without a word
/// and one that failed on the first would make a file with one marker
/// unconvertible. `dropped` lists each event the clip lacks, in the order the
/// clip's sequence would have had them (by tick, then track, then the track's
/// own order), so a caller can show it, or refuse a file that loses
/// something it cares about.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Converted {
    /// The Clip File.
    pub clip: MidiFile,
    /// What has no UMP form.
    pub dropped: Vec<Dropped>,
}

/// 10 ns units in a microsecond.
const TEN_NS_PER_MICRO: u32 = 100;

/// Where in a tick's events a message goes: M2-116 sections 7.1.1 and 7.1.2
/// put the first Set Tempo, then the first Set Time Signature, at the Start
/// of Clip.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Rank {
    Tempo,
    Signature,
    Other,
}

fn event(ump: Ump) -> FileEvent {
    match decode(ump) {
        Ok(message) => FileEvent::Message(message),
        Err(unknown) => FileEvent::Unknown(unknown),
    }
}

fn flex(flex: Flex) -> FileEvent {
    FileEvent::Message(Message::Flex(MidiGroup::FIRST, flex))
}

fn tempo(micros_per_quarter: u32) -> FileEvent {
    // 24 bits of microseconds times 100 is under 2^31.
    flex(Flex::SetTempo(micros_per_quarter * TEN_NS_PER_MICRO))
}

fn signature(
    numerator: u8,
    denominator: u8,
    thirty_seconds: u8,
    clocks_per_click: u8,
) -> [FileEvent; 2] {
    [
        flex(Flex::SetTimeSignature(TimeSignature {
            numerator,
            denominator,
            thirty_seconds,
        })),
        // SMF has the clocks per click only, so the bar is not divided:
        // "If the bar is not divided, then the value of Bar Accent Part 1
        // shall be equal to the number of beats in the bar, Bar Accent Part 2
        // shall be set to zero, and Bar Accent Part 3 shall be set to zero"
        // (M2-104 section 7.5.5). "Set a value of 0 to not use any
        // Subdivision Clicks" (same section).
        flex(Flex::SetMetronome(Metronome {
            clocks_per_click,
            accent_1: numerator,
            accent_2: 0,
            accent_3: 0,
            subdivision_1: 0,
            subdivision_2: 0,
        })),
    ]
}

/// The key signature of `sharps` and `minor`: the sharps or flats, and the
/// tonic's letter (the field has no accidental: the sharps and flats say it).
///
/// A major key's tonic goes round the circle of fifths F C G D A E B as the
/// sharps rise from -1 (C at 0, G at 1); a minor key is the relative minor of
/// the major key 3 sharps higher (A minor, no sharps, shares its signature
/// with C major and the key 3 sharps up is A major), so its tonic is that
/// key's.
fn key_signature(sharps: i8, minor: bool) -> Option<KeySignature> {
    const CIRCLE: [Letter; 7] = [
        Letter::F,
        Letter::C,
        Letter::G,
        Letter::D,
        Letter::A,
        Letter::E,
        Letter::B,
    ];
    let sharps_flats = SharpsFlats::new(sharps)?;
    let major = i16::from(sharps) + if minor { 3 } else { 0 };
    Some(KeySignature {
        sharps_flats: Some(sharps_flats),
        tonic: Some(CIRCLE[(major + 1).rem_euclid(7) as usize]),
    })
}

/// A text as its Flex packets, or why it has none.
///
/// "MIDI messages only use normalized UTF-8 encoded Unicode characters,
/// following the NFC (Normalization Form C) standard" and "shall not include
/// a Byte Order Mark" (M2-104 section 1.8): a leading BOM goes and the rest is
/// composed to NFC, which changes how the text is spelled, never what it says.
fn text(kind: FlexTextKind, bytes: &[u8]) -> std::result::Result<Vec<FileEvent>, DropReason> {
    if bytes.is_empty() {
        return Err(DropReason::EmptyText);
    }
    let text = std::str::from_utf8(bytes).map_err(|_| DropReason::TextNotUtf8)?;
    let text: String = text
        .strip_prefix('\u{FEFF}')
        .unwrap_or(text)
        .nfc()
        .collect();
    let packets = flex_text_packets(MidiGroup::FIRST, FlexAddress::Group, kind, &text)
        .ok_or(DropReason::TextHasNul)?;
    Ok(packets.map(event).collect())
}

/// What one SMF event becomes: its clip events (in order) and where among its
/// tick's they go, or the reason it has none.
fn convert(
    track: usize,
    format: SmfFormat,
    ev: &FileEvent,
) -> std::result::Result<(Rank, Vec<FileEvent>), DropReason> {
    let other = |events| Ok((Rank::Other, events));
    match ev {
        // Already a UMP: a MIDI 1.0 channel message on group 0.
        FileEvent::Message(_) | FileEvent::Unknown(_) => other(vec![ev.clone()]),
        FileEvent::SysEx(body) => {
            if let [0x7E, _, 0x0D, 0x30..=0x3F, ..] = **body {
                return Err(DropReason::PropertyExchange);
            }
            let whole: Vec<u8> = std::iter::once(0xF0).chain(body.iter().copied()).collect();
            let message = SysexMessage::new(&whole).ok_or(DropReason::SysexNotCompleteSevenBit)?;
            other(message.packets(MidiGroup::FIRST).map(event).collect())
        }
        FileEvent::Escape(_) => Err(DropReason::NoSuccessor),
        FileEvent::Meta(meta) => match meta {
            Meta::Tempo { micros_per_quarter } => {
                Ok((Rank::Tempo, vec![tempo(*micros_per_quarter)]))
            }
            Meta::TimeSignature {
                numerator,
                denominator_power,
                clocks_per_click,
                thirty_seconds_per_quarter,
            } => {
                if *denominator_power == 0 {
                    return Err(DropReason::WholeNoteDenominator);
                }
                if *numerator == 0 {
                    return Err(DropReason::ZeroNumerator);
                }
                Ok((
                    Rank::Signature,
                    signature(
                        *numerator,
                        *denominator_power,
                        *thirty_seconds_per_quarter,
                        *clocks_per_click,
                    )
                    .to_vec(),
                ))
            }
            Meta::KeySignature { sharps, minor } => {
                let key =
                    key_signature(*sharps, *minor).ok_or(DropReason::KeySignatureOutOfRange)?;
                other(vec![flex(Flex::SetKeySignature(FlexAddress::Group, key))])
            }
            Meta::Text { kind, bytes } => {
                let kind = match kind {
                    // SMF 1.0 section 3.3: "In a format 0 track, or the first
                    // track in a format 1 file, the name of the Sequence.
                    // Otherwise, the name of the Track." The sequence is the
                    // Composition (Song) Name of Appendix B.
                    TextKind::TrackName if track == 0 && format != SmfFormat::Sequential => {
                        FlexTextKind::CompositionName
                    }
                    TextKind::TrackName => return Err(DropReason::TrackName),
                    TextKind::Copyright => FlexTextKind::Copyright,
                    TextKind::Lyric => FlexTextKind::Lyrics,
                    // "Any text" is what the Unknown Metadata Text message is
                    // for (M2-104 Table 16, bank 1 status 0).
                    TextKind::Text => FlexTextKind::UnknownMetadata,
                    TextKind::InstrumentName
                    | TextKind::Marker
                    | TextKind::CuePoint
                    | TextKind::ProgramName
                    | TextKind::DeviceName => return Err(DropReason::NoSuccessor),
                };
                other(text(kind, bytes)?)
            }
            Meta::Other { kind, .. } => Err(match kind {
                0x20 | 0x21 => DropReason::ReplacedByAddressing,
                0x54 => DropReason::SuccessorUndefined,
                _ => DropReason::NoSuccessor,
            }),
        },
    }
}

impl MidiFile {
    /// A Standard MIDI File of format 0 or 1 as a MIDI 2.0 Clip File
    /// (M2-116), its meta events replaced by the UMP messages of Appendix B,
    /// with the events that have no replacement beside it.
    ///
    /// The mapping, each row from M2-116 section 3.3 and Appendix B (fields
    /// from M2-104 section 7.5):
    ///
    /// | SMF | clip |
    /// |---|---|
    /// | channel message | the same [`Message::Midi1`] packet, group 0 |
    /// | Set Tempo `FF 51`, us per quarter | Flex Set Tempo, `us * 100` (10 ns units per quarter) |
    /// | Time Signature `FF 58` | Flex Set Time Signature (numerator, denominator power, 32nds per quarter) then Flex Set Metronome (clocks per click; the bar undivided, its one accent part the numerator; no subdivisions) |
    /// | Key Signature `FF 59` | Flex Set Key Signature: the sharps or flats, and the tonic's letter from the circle of fifths (a minor key's from the major key 3 sharps up) |
    /// | Copyright `FF 02` | Flex Copyright Notice |
    /// | Lyric `FF 05` | Flex Lyrics |
    /// | Track Name `FF 03` of format 0, or the first track of format 1 | Flex Composition Name (SMF 1.0 section 3.3: it names the sequence) |
    /// | Text `FF 01` | Flex Unknown Metadata Text |
    /// | `F0` System Exclusive, complete and 7-bit | SysEx7 packets |
    /// | divisions per quarter | the DCTPQ, and ticks carry over |
    /// | End of Track | End of Clip, at the longest track's end |
    ///
    /// Texts are split over Flex packets by [`tutti_ump::flex_text_packets`].
    /// Nothing else is converted: [`DropReason`] lists what has no successor.
    ///
    /// Format 1's tracks merge into the clip's one sequence by tick, ties in
    /// track order and then the track's own, as [`events`](Self::events) does.
    /// Placement follows M2-116 sections 7.1.1 and 7.1.2: at tick 0 the Set
    /// Tempo messages come first and the Set Time Signature messages (each
    /// with its Set Metronome) next, then the rest; a file with no Set Tempo
    /// at tick 0 gets the SMF default, 120 bpm (SMF 1.0 section 3.3: "If no
    /// tempo is specified, 120 beats per minute is assumed"), and one with no
    /// Time Signature 4/4, 24 clocks per click, 8 32nds per quarter, so the
    /// clip says what a SMF reader assumed. A later tempo or time signature
    /// stays on its own tick: the spec's "where MIDI clocks fall" and "at a
    /// bar line" are wishes about the author's grid, and moving an event
    /// would change the music. The [header](Track::header) is empty: the
    /// sequence data carries the tempo, which the spec's section 7.1.1 asks.
    ///
    /// # Errors
    ///
    /// [`Error::ToClip`](crate::Error::ToClip): [`ToClipError::Sequential`] for a format 2 file,
    /// [`ToClipError::AlreadyClip`] for a Clip File. [`Error::InvalidConfig`](crate::Error::InvalidConfig)
    /// or [`Error::Tempo`](crate::Error::Tempo) cannot happen for a valid SMF.
    pub fn to_clip(&self) -> Result<Converted> {
        let format = match self.smf_format() {
            Some(SmfFormat::Sequential) => return Err(ToClipError::Sequential.into()),
            Some(format) => format,
            None => return Err(ToClipError::AlreadyClip.into()),
        };

        // Track order, then the track's own: the stable sort by tick keeps it.
        let mut all: Vec<(Tick, usize, &FileEvent)> = self
            .tracks()
            .iter()
            .enumerate()
            .flat_map(|(i, t)| t.events.iter().map(move |(tick, ev)| (*tick, i, ev)))
            .collect();
        all.sort_by_key(|&(tick, ..)| tick);

        let at_start = |tick: Tick, wanted: fn(&Meta) -> bool| {
            all.iter()
                .any(|&(t, _, ev)| t == tick && matches!(ev, FileEvent::Meta(m) if wanted(m)))
        };
        let has_tempo = at_start(Tick(0), |m| matches!(m, Meta::Tempo { .. }));
        let has_signature = at_start(Tick(0), |m| matches!(m, Meta::TimeSignature { .. }));

        let mut placed: Vec<(Tick, Rank, FileEvent)> = Vec::new();
        if !has_tempo {
            placed.push((Tick(0), Rank::Tempo, tempo(500_000)));
        }
        if !has_signature {
            for ev in signature(4, 2, 8, 24) {
                placed.push((Tick(0), Rank::Signature, ev));
            }
        }
        let mut dropped = Vec::new();
        for (tick, track, ev) in all {
            match convert(track, format, ev) {
                Ok((rank, events)) => {
                    // Only tick 0 is ordered by rank; a later tempo is where it was.
                    let rank = if tick == Tick(0) { rank } else { Rank::Other };
                    placed.extend(events.into_iter().map(|e| (tick, rank, e)));
                }
                Err(reason) => dropped.push(Dropped {
                    track,
                    tick,
                    event: ev.clone(),
                    reason,
                }),
            }
        }
        // Stable: within a tick and rank, the merge order.
        placed.sort_by_key(|&(tick, rank, _)| (tick, rank));

        let end = self
            .tracks()
            .iter()
            .map(|t| t.end)
            .max()
            .unwrap_or_default();
        let sequence = Track {
            header: Vec::new(),
            header_end: Tick(0),
            events: placed.into_iter().map(|(tick, _, e)| (tick, e)).collect(),
            end,
        };
        let clip = MidiFile::clip(self.ticks_per_quarter(), sequence)?;
        Ok(Converted { clip, dropped })
    }
}
