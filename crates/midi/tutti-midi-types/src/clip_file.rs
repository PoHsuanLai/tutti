//! MIDI 2.0 **Clip File** (M2-116) reader/writer.
//!
//! A MIDI Clip File is the MIDI-2 analogue of a Standard MIDI File Type 0: one
//! sequence of Universal MIDI Packets. Layout (M2-116 §4–7, all big-endian):
//!
//! ```text
//! ["SMF2CLIP" 8 bytes] [DCS=0] [DCTPQ] [ (DCS · UMP)* ] [DCS] [StartOfClip]
//!   [ (DCS · UMP)* ] [DCS] [EndOfClip]
//! ```
//!
//! Every UMP is preceded by a **Delta Clockstamp** (DCS) giving the ticks since
//! the previous event; the tick unit is declared once by the **DCTPQ** (Delta
//! Clockstamp Ticks Per Quarter Note). The stream is bracketed by Start-of-Clip
//! and End-of-Clip UMP Stream messages.
//!
//! This module models the file as a flat list of `(delta_ticks, MidiEvent)`,
//! which is all a Type-0-like clip needs; the caller supplies the DCTPQ.

extern crate alloc;
use alloc::vec::Vec;

use midi2::ump_stream::{EndOfClip, StartOfClip};
use midi2::utility::{DeltaClockstamp, DeltaClockstampTpq};
use midi2::ux::u20;
use midi2::Data;

use crate::ump::MidiEvent;

/// The 8-byte file header: ASCII "SMF2CLIP" (M2-116 §5).
pub const CLIP_FILE_MAGIC: [u8; 8] = *b"SMF2CLIP";

/// A timed UMP event in a clip: `delta_ticks` since the previous event, then the
/// event itself.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ClipEvent {
    pub delta_ticks: u32,
    pub event: MidiEvent,
}

/// Serialize a MIDI Clip File (M2-116) from a tick-per-quarter unit and a flat
/// list of timed events. Emits header + DCTPQ + Start-of-Clip + (DCS·UMP)* +
/// End-of-Clip, big-endian.
pub fn write_clip_file(ticks_per_quarter: u16, events: &[ClipEvent]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + (events.len() + 3) * 8);
    out.extend_from_slice(&CLIP_FILE_MAGIC);

    // DCTPQ declares the tick unit (preceded by a zero DCS per §3.2.1).
    push_words(&mut out, delta_clockstamp(0).data_words());
    push_words(&mut out, dctpq(ticks_per_quarter).data_words());

    // Start of Clip (preceded by a zero DCS — bar 1 begins here).
    push_words(&mut out, delta_clockstamp(0).data_words());
    push_words(&mut out, start_of_clip().data_words());

    // The timed event stream: each UMP preceded by its delta clockstamp(s).
    // A DCS field is only 20 bits, so a delta beyond `DCS_MAX` is expressed as a
    // chain of DCS messages that accumulate (M2-116 §3.2.3).
    for ev in events {
        push_delta(&mut out, ev.delta_ticks);
        push_words(&mut out, ev.event.data_words());
    }

    // End of Clip (preceded by a zero DCS).
    push_words(&mut out, delta_clockstamp(0).data_words());
    push_words(&mut out, end_of_clip().data_words());
    out
}

/// A parsed MIDI Clip File.
#[derive(Clone, Debug, PartialEq)]
pub struct ParsedClipFile {
    pub ticks_per_quarter: u16,
    pub events: Vec<ClipEvent>,
}

/// Why a byte stream failed to parse as a MIDI Clip File (M2-116). Distinguishes
/// "this isn't a clip file" from "this clip file is malformed" so a caller can
/// react differently (e.g. try another importer vs. report corruption).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClipFileError {
    /// Fewer than 8 bytes, or the leading 8 bytes are not [`CLIP_FILE_MAGIC`].
    BadMagic,
    /// The body length is not a whole number of 32-bit UMP words.
    Unaligned,
    /// A UMP claims more words than remain in the stream.
    Truncated,
    /// No DCTPQ (tick-unit declaration) was seen before the events.
    MissingDctpq,
}

impl core::fmt::Display for ClipFileError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let msg = match self {
            Self::BadMagic => "not a MIDI Clip File (bad or missing SMF2CLIP magic)",
            Self::Unaligned => "clip body is not 32-bit-word aligned",
            Self::Truncated => "clip file is truncated mid-message",
            Self::MissingDctpq => "clip file has no DCTPQ tick-unit declaration",
        };
        f.write_str(msg)
    }
}

#[cfg(feature = "std")]
impl std::error::Error for ClipFileError {}

/// Parse a MIDI Clip File (M2-116). Delta Clockstamps set the delta of the
/// following UMP; Start/End-of-Clip and the DCTPQ are structural and not
/// returned as events. Unknown/utility messages between a DCS and a real event
/// are tolerated. See [`ClipFileError`] for the failure cases.
pub fn read_clip_file(bytes: &[u8]) -> Result<ParsedClipFile, ClipFileError> {
    if bytes.len() < 8 || bytes[..8] != CLIP_FILE_MAGIC {
        return Err(ClipFileError::BadMagic);
    }
    let mut words = WordReader::new(&bytes[8..]).ok_or(ClipFileError::Unaligned)?;

    let mut ticks_per_quarter = None;
    let mut events = Vec::new();
    let mut pending_delta: u32 = 0;

    while let Some(word0) = words.peek() {
        let mt = (word0 >> 28) as u8;
        let n = crate::ump::ump_word_count(mt);
        let raw = words.take(n).ok_or(ClipFileError::Truncated)?;
        let ev = MidiEvent::from_ump(0, raw);

        // Classify by message type + status.
        if mt == 0x0 {
            // Utility: Delta Clockstamp (status 0x4) or DCTPQ (status 0x3).
            let status = ((word0 >> 20) & 0x0F) as u8;
            match status {
                0x4 => {
                    // Delta Clockstamps accumulate: a chain of them (each ≤ 20
                    // bits) expresses a delta larger than one field can hold.
                    pending_delta = pending_delta.saturating_add(word0 & DCS_MAX);
                    continue;
                }
                0x3 => {
                    ticks_per_quarter = Some((word0 & 0xFFFF) as u16);
                    continue;
                }
                _ => continue, // NoOp / other utility — ignore
            }
        }
        if mt == 0xF {
            // UMP Stream: Start of Clip (0x020) / End of Clip (0x021).
            let status = (word0 >> 16) & 0x03FF;
            if status == 0x020 {
                continue; // start of clip — structural
            }
            if status == 0x021 {
                break; // end of clip — done
            }
        }
        // A real event: attach the pending delta.
        events.push(ClipEvent {
            delta_ticks: pending_delta,
            event: ev,
        });
        pending_delta = 0;
    }

    Ok(ParsedClipFile {
        ticks_per_quarter: ticks_per_quarter.ok_or(ClipFileError::MissingDctpq)?,
        events,
    })
}

// --- helpers ---------------------------------------------------------------

fn push_words(out: &mut Vec<u8>, words: &[u32]) {
    for w in words {
        out.extend_from_slice(&w.to_be_bytes());
    }
}

/// Widest value a single 20-bit Delta Clockstamp field can carry.
const DCS_MAX: u32 = 0x000F_FFFF;

fn delta_clockstamp(ticks: u32) -> MidiEvent {
    let mut m = DeltaClockstamp::<[u32; 1]>::new();
    m.set_time_data(u20::new(ticks & DCS_MAX));
    MidiEvent::from_ump(0, m.data())
}

/// Emit `ticks` as one or more chained Delta Clockstamps. A single DCS field is
/// 20 bits; a larger delta is split into full-width chunks that the reader sums
/// (M2-116 §3.2.3). `0` still emits exactly one (zero) DCS.
fn push_delta(out: &mut Vec<u8>, mut ticks: u32) {
    loop {
        let chunk = ticks.min(DCS_MAX);
        push_words(out, delta_clockstamp(chunk).data_words());
        ticks -= chunk;
        if ticks == 0 {
            break;
        }
    }
}

fn dctpq(ticks_per_quarter: u16) -> MidiEvent {
    let mut m = DeltaClockstampTpq::<[u32; 1]>::new();
    m.set_time_data(ticks_per_quarter);
    MidiEvent::from_ump(0, m.data())
}

fn start_of_clip() -> MidiEvent {
    MidiEvent::from_ump(0, StartOfClip::<[u32; 4]>::new().data())
}

fn end_of_clip() -> MidiEvent {
    MidiEvent::from_ump(0, EndOfClip::<[u32; 4]>::new().data())
}

/// Reads big-endian u32 words from a byte slice.
struct WordReader<'a> {
    words: Vec<u32>,
    pos: usize,
    _marker: core::marker::PhantomData<&'a ()>,
}

impl<'a> WordReader<'a> {
    fn new(bytes: &'a [u8]) -> Option<Self> {
        if !bytes.len().is_multiple_of(4) {
            return None;
        }
        let words = bytes
            .chunks_exact(4)
            .map(|c| u32::from_be_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        Some(Self {
            words,
            pos: 0,
            _marker: core::marker::PhantomData,
        })
    }

    fn peek(&self) -> Option<u32> {
        self.words.get(self.pos).copied()
    }

    fn take(&mut self, n: usize) -> Option<&[u32]> {
        let end = self.pos + n;
        if end > self.words.len() {
            return None;
        }
        let slice = &self.words[self.pos..end];
        self.pos = end;
        Some(slice)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_magic_is_smf2clip() {
        assert_eq!(&CLIP_FILE_MAGIC, b"SMF2CLIP");
        let bytes = write_clip_file(480, &[]);
        assert_eq!(&bytes[..8], b"SMF2CLIP");
    }

    #[test]
    fn empty_clip_round_trips_dctpq() {
        let bytes = write_clip_file(480, &[]);
        let parsed = read_clip_file(&bytes).expect("parses");
        assert_eq!(parsed.ticks_per_quarter, 480);
        assert!(parsed.events.is_empty());
    }

    #[test]
    fn events_round_trip_with_deltas() {
        let events = [
            ClipEvent {
                delta_ticks: 0,
                event: MidiEvent::note_on(0, 0, 60, 0x8000),
            },
            ClipEvent {
                delta_ticks: 240,
                event: MidiEvent::note_off(0, 0, 60, 0),
            },
            ClipEvent {
                delta_ticks: 240,
                event: MidiEvent::note_on(0, 1, 64, 0xFFFF),
            },
        ];
        let bytes = write_clip_file(96, &events);
        let parsed = read_clip_file(&bytes).expect("parses");
        assert_eq!(parsed.ticks_per_quarter, 96);
        assert_eq!(parsed.events.len(), 3);
        assert_eq!(parsed.events[0].delta_ticks, 0);
        assert_eq!(parsed.events[1].delta_ticks, 240);
        assert_eq!(parsed.events[2].delta_ticks, 240);
        // Payloads survive.
        assert_eq!(parsed.events, events);
    }

    #[test]
    fn large_delta_chains_and_round_trips() {
        // A delta well beyond the 20-bit DCS field (0xFFFFF = 1_048_575) must
        // survive via chained clockstamps rather than truncating.
        let big = 3_000_000; // ~2.86 × DCS_MAX
        let events = [
            ClipEvent {
                delta_ticks: big,
                event: MidiEvent::note_on(0, 0, 60, 0x8000),
            },
            ClipEvent {
                delta_ticks: DCS_MAX, // exactly one full field
                event: MidiEvent::note_off(0, 0, 60, 0),
            },
        ];
        let bytes = write_clip_file(480, &events);
        let parsed = read_clip_file(&bytes).expect("parses");
        assert_eq!(parsed.events.len(), 2);
        assert_eq!(parsed.events[0].delta_ticks, big);
        assert_eq!(parsed.events[1].delta_ticks, DCS_MAX);
        assert_eq!(parsed.events, events);
    }

    #[test]
    fn errors_distinguish_failure_modes() {
        // Not a clip file at all.
        assert_eq!(read_clip_file(b"NOTACLIP\0\0\0\0"), Err(ClipFileError::BadMagic));
        assert_eq!(read_clip_file(b"short"), Err(ClipFileError::BadMagic));
        // Right magic, but the body isn't word-aligned.
        assert_eq!(
            read_clip_file(b"SMF2CLIP\x00\x00\x00"),
            Err(ClipFileError::Unaligned)
        );
        // Magic + aligned body, but no DCTPQ before EOF.
        assert_eq!(
            read_clip_file(b"SMF2CLIP"),
            Err(ClipFileError::MissingDctpq)
        );
    }
}
