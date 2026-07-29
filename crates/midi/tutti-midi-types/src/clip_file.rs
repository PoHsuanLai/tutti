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

use std::vec::Vec;

use midi2::ump_stream::{EndOfClip, StartOfClip};
use midi2::utility::{DeltaClockstamp, DeltaClockstampTpq};
use midi2::ux::u20;
use midi2::Data;

use crate::ump::MidiEvent;

/// The 8-byte file header: ASCII "SMF2CLIP" (M2-116 §5).
pub const CLIP_FILE_MAGIC: [u8; 8] = *b"SMF2CLIP";

/// A timed UMP event in a clip: `delta_ticks` since the previous event, then the
/// event itself.
///
/// This is the file's on-wire timing model (relative ticks). If you think in
/// beats — as most callers do — prefer [`write_clip_file_from_beats`] and
/// [`ParsedClipFile::timed`], which own the beat↔tick↔delta conversion so you
/// never build these by hand.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClipEvent {
    pub delta_ticks: u32,
    pub event: MidiEvent,
}

impl ClipEvent {
    /// A timed event: `delta_ticks` since the previous event, then the event.
    #[inline]
    pub const fn new(delta_ticks: u32, event: MidiEvent) -> Self {
        Self { delta_ticks, event }
    }
}

impl From<(u32, MidiEvent)> for ClipEvent {
    /// `(delta_ticks, event)` — lets a caller write the tuple form.
    #[inline]
    fn from((delta_ticks, event): (u32, MidiEvent)) -> Self {
        Self { delta_ticks, event }
    }
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
    // A DCS field is only 20 bits, so a delta beyond `DCS_MAX` is expressed as
    // DCS·NOOP restarts (M2-116 §3.2.2) — see `push_delta`.
    for ev in events {
        push_delta(&mut out, ev.delta_ticks);
        push_words(&mut out, ev.event.data_words());
    }

    // End of Clip (preceded by a zero DCS).
    push_words(&mut out, delta_clockstamp(0).data_words());
    push_words(&mut out, end_of_clip().data_words());
    out
}

/// Serialize a MIDI Clip File from **beat-positioned** events — the ergonomic
/// entry point when you think in beats rather than delta ticks.
///
/// Each `(beat, event)` carries an *absolute* beat position (quarter notes from
/// the clip start). This quantizes them to `ticks_per_quarter` and computes the
/// inter-event delta ticks for you, so you never accumulate deltas by hand.
/// Input need not be sorted — events are ordered by beat first; a zero or
/// negative gap (simultaneous or slightly out-of-order events) becomes a
/// zero-tick delta.
///
/// ```
/// # use tutti_midi_types::{write_clip_file_from_beats, read_clip_file, MidiEvent};
/// let bytes = write_clip_file_from_beats(96, [
///     (0.0, MidiEvent::note_on(0, 0, 60, 0x8000)),
///     (2.0, MidiEvent::note_off(0, 0, 60, 0)),
/// ]);
/// let clip = read_clip_file(&bytes).unwrap();
/// assert_eq!(clip.timed().count(), 2);
/// ```
pub fn write_clip_file_from_beats(
    ticks_per_quarter: u16,
    events: impl IntoIterator<Item = (f64, MidiEvent)>,
) -> Vec<u8> {
    let tpq = f64::from(ticks_per_quarter);
    let mut timed: Vec<(f64, MidiEvent)> = events.into_iter().collect();
    timed.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(core::cmp::Ordering::Equal));

    let mut prev_tick: i64 = 0;
    let clip_events: Vec<ClipEvent> = timed
        .into_iter()
        .map(|(beat, event)| {
            let tick = (beat * tpq).round() as i64;
            let delta = (tick - prev_tick).max(0) as u32;
            prev_tick = tick;
            ClipEvent::new(delta, event)
        })
        .collect();
    write_clip_file(ticks_per_quarter, &clip_events)
}

/// A parsed MIDI Clip File.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedClipFile {
    pub ticks_per_quarter: u16,
    pub events: Vec<ClipEvent>,
}

impl ParsedClipFile {
    /// Iterate events as `(beat, event)` with **absolute** beat positions —
    /// quarter notes from the clip start — undoing the file's relative delta
    /// ticks. The inverse of [`write_clip_file_from_beats`]: the natural way to
    /// consume a clip when you schedule in beats.
    ///
    /// Beats are `delta_ticks` summed and divided by [`Self::ticks_per_quarter`].
    pub fn timed(&self) -> impl Iterator<Item = (f64, MidiEvent)> + '_ {
        let tpq = f64::from(self.ticks_per_quarter);
        let mut abs_tick: u64 = 0;
        self.events.iter().map(move |ce| {
            abs_tick += u64::from(ce.delta_ticks);
            (abs_tick as f64 / tpq, ce.event)
        })
    }

    /// The clip's musical length in beats: the absolute beat of the last event
    /// (0.0 for an empty clip). Note-offs are events too, so this reflects where
    /// the last message lands, not necessarily where sound stops.
    pub fn duration_beats(&self) -> f64 {
        let total_ticks: u64 = self.events.iter().map(|ce| u64::from(ce.delta_ticks)).sum();
        total_ticks as f64 / f64::from(self.ticks_per_quarter)
    }
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
    // The delta declared by the most recent DCS, and the total closed off by
    // NOOP restarts before it. See the DCS/NOOP arms below.
    let mut pending_delta: u32 = 0;
    let mut banked_delta: u32 = 0;

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
                    // A DCS is the count since the last event or Null message —
                    // it *replaces* the current count rather than adding to it
                    // (M2-116 §3.2.2). Only `banked_delta`, closed off by a
                    // NOOP below, carries over from earlier restarts.
                    pending_delta = word0 & DCS_MAX;
                    continue;
                }
                0x3 => {
                    ticks_per_quarter = Some((word0 & 0xFFFF) as u16);
                    continue;
                }
                0x0 => {
                    // NOOP = the Null message that restarts the delta count
                    // (M2-116 §3.2.2). Bank what the preceding DCS declared;
                    // the next DCS counts from here.
                    banked_delta = banked_delta.saturating_add(pending_delta);
                    pending_delta = 0;
                    continue;
                }
                _ => continue, // JR clock / timestamp / other utility — ignore
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
        // A real event: its delta is the current count plus everything banked
        // by NOOP restarts since the previous event.
        events.push(ClipEvent {
            delta_ticks: banked_delta.saturating_add(pending_delta),
            event: ev,
        });
        pending_delta = 0;
        banked_delta = 0;
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

/// Emit `ticks` as Delta Clockstamps, restarting the count with a NOOP each
/// time a gap exceeds the 20-bit DCS field.
///
/// M2-116 §3.2.2 (identically M2-104-UM §7.2.3.2): "If no MIDI message has
/// occurred during the previous 1,048,575 ticks, then the application which
/// creates the MIDI Clip File shall insert a Delta Clockstamp followed by a
/// Null message to restart the delta time count. Then the next Delta Clockstamp
/// in the file declares the ticks since the previous Null message."
///
/// So consecutive DCSs do **not** accumulate — each one is the count since the
/// last event, and a NOOP is what makes a long gap expressible as a sequence of
/// restarts. `0` still emits exactly one (zero) DCS.
fn push_delta(out: &mut Vec<u8>, mut ticks: u32) {
    while ticks > DCS_MAX {
        push_words(out, delta_clockstamp(DCS_MAX).data_words());
        push_words(out, MidiEvent::noop().data_words());
        ticks -= DCS_MAX;
    }
    push_words(out, delta_clockstamp(ticks).data_words());
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

    /// Big-endian words of a clip file body, for byte-level assertions.
    fn body_words(bytes: &[u8]) -> Vec<u32> {
        bytes[8..]
            .chunks_exact(4)
            .map(|c| u32::from_be_bytes([c[0], c[1], c[2], c[3]]))
            .collect()
    }

    #[test]
    fn large_delta_writes_dcs_noop_restarts() {
        // M2-116 §3.2.2: a gap beyond the 20-bit DCS field is written as
        // `DCS(max) · NOOP` restarts, then the remainder — NOT as adjacent DCSs
        // that a reader is expected to sum. This asserts the bytes, not a
        // round-trip: writer and reader agreeing with each other is exactly the
        // thing that hid this bug.
        let big = 3_000_000; // 2×DCS_MAX + 902_850
        let bytes = write_clip_file(
            480,
            &[ClipEvent {
                delta_ticks: big,
                event: MidiEvent::note_on(0, 0, 60, 0x8000),
            }],
        );
        let words = body_words(&bytes);

        // Skip DCS(0)+DCTPQ and DCS(0)+StartOfClip (1+1+1+4 words).
        let stream = &words[7..];
        let dcs_max_word = 0x0040_0000 | DCS_MAX; // utility, status 0x4
        let noop_word = 0x0000_0000; // utility, status 0x0

        assert_eq!(stream[0], dcs_max_word, "first chunk is a full DCS field");
        assert_eq!(stream[1], noop_word, "restart marker must be a NOOP");
        assert_eq!(stream[2], dcs_max_word, "second full chunk");
        assert_eq!(stream[3], noop_word, "second restart marker");
        assert_eq!(
            stream[4],
            0x0040_0000 | (big - 2 * DCS_MAX),
            "remainder counts from the last NOOP"
        );
        // Then the note-on itself (MT 0x4, 2 words).
        assert_eq!(stream[5] >> 28, 0x4);
    }

    #[test]
    fn reader_restarts_on_noop_and_replaces_on_bare_dcs() {
        // Hand-built conformant fixture — an independent oracle for the reader,
        // so a matching writer bug cannot mask a reader bug.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&CLIP_FILE_MAGIC);
        let mut push = |w: u32| bytes.extend_from_slice(&w.to_be_bytes());
        push(0x0040_0000); // DCS(0)
        push(0x0030_01E0); // DCTPQ = 480
        push(0x0040_0000); // DCS(0)
        for w in [0xF020_0000, 0, 0, 0] {
            push(w); // Start of Clip
        }
        // A gap of DCS_MAX + 1000, spelled the conformant way.
        push(0x0040_0000 | DCS_MAX); // DCS(max)
        push(0x0000_0000); // NOOP → restart
        push(0x0040_0000 | 1000); // DCS(1000) counts from the NOOP
        push(0x4090_3C00); // NoteOn word 0
        push(0x8000_0000); // NoteOn word 1

        let parsed = read_clip_file(&bytes).expect("parses");
        assert_eq!(parsed.ticks_per_quarter, 480);
        assert_eq!(parsed.events.len(), 1);
        assert_eq!(
            parsed.events[0].delta_ticks,
            DCS_MAX + 1000,
            "NOOP banks the preceding DCS; the next DCS adds on top"
        );
    }

    #[test]
    fn bare_consecutive_dcs_replaces_rather_than_sums() {
        // Two DCSs with no NOOP between them: §3.2.2 makes each the count since
        // the last event, so the second REPLACES the first. Summing them (our
        // old behaviour) would give 1500 and place the event late.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&CLIP_FILE_MAGIC);
        let mut push = |w: u32| bytes.extend_from_slice(&w.to_be_bytes());
        push(0x0040_0000);
        push(0x0030_01E0); // DCTPQ = 480
        push(0x0040_0000 | 500); // DCS(500)
        push(0x0040_0000 | 1000); // DCS(1000) — replaces, not adds
        push(0x4090_3C00);
        push(0x8000_0000);

        let parsed = read_clip_file(&bytes).expect("parses");
        assert_eq!(parsed.events.len(), 1);
        assert_eq!(parsed.events[0].delta_ticks, 1000);
    }

    #[test]
    fn large_delta_round_trips() {
        // The round-trip must still hold — but it is now a consequence of both
        // halves being spec-correct, not of them sharing a bug.
        let events = [
            ClipEvent {
                delta_ticks: 3_000_000,
                event: MidiEvent::note_on(0, 0, 60, 0x8000),
            },
            ClipEvent {
                delta_ticks: DCS_MAX, // exactly one full field — no restart needed
                event: MidiEvent::note_off(0, 0, 60, 0),
            },
        ];
        let bytes = write_clip_file(480, &events);
        let parsed = read_clip_file(&bytes).expect("parses");
        assert_eq!(parsed.events, events);
    }

    #[test]
    fn exactly_max_delta_needs_no_restart() {
        // Boundary: DCS_MAX fits one field, so no NOOP should be emitted.
        let bytes = write_clip_file(
            480,
            &[ClipEvent {
                delta_ticks: DCS_MAX,
                event: MidiEvent::note_on(0, 0, 60, 0x8000),
            }],
        );
        let stream = body_words(&bytes)[7..].to_vec();
        assert_eq!(stream[0], 0x0040_0000 | DCS_MAX);
        assert_eq!(stream[1] >> 28, 0x4, "note-on follows immediately, no NOOP");
    }

    #[test]
    fn beats_round_trip_through_the_beat_facade() {
        // Absolute beats in → delta ticks on disk → absolute beats out.
        let bytes = write_clip_file_from_beats(
            96,
            [
                (0.0, MidiEvent::note_on(0, 0, 60, 0x8000)),
                (2.0, MidiEvent::note_off(0, 0, 60, 0)),
                (2.5, MidiEvent::note_on(0, 1, 64, 0xFFFF)),
            ],
        );
        let clip = read_clip_file(&bytes).expect("parses");
        let timed: Vec<(f64, MidiEvent)> = clip.timed().collect();
        assert_eq!(timed.len(), 3);
        assert!((timed[0].0 - 0.0).abs() < 1e-9);
        assert!((timed[1].0 - 2.0).abs() < 1e-9);
        assert!((timed[2].0 - 2.5).abs() < 1e-9);
        assert_eq!(timed[2].1, MidiEvent::note_on(0, 1, 64, 0xFFFF));
        assert!((clip.duration_beats() - 2.5).abs() < 1e-9);
    }

    #[test]
    fn beat_facade_orders_unsorted_input() {
        // Out-of-order input is sorted by beat; deltas never go negative.
        let bytes = write_clip_file_from_beats(
            480,
            [
                (4.0, MidiEvent::note_off(0, 0, 60, 0)),
                (0.0, MidiEvent::note_on(0, 0, 60, 0x8000)),
            ],
        );
        let clip = read_clip_file(&bytes).expect("parses");
        let beats: Vec<f64> = clip.timed().map(|(b, _)| b).collect();
        assert_eq!(beats.len(), 2);
        assert!(beats[0] < beats[1]);
        assert!((beats[0] - 0.0).abs() < 1e-9);
        assert!((beats[1] - 4.0).abs() < 1e-9);
    }

    #[test]
    fn clip_event_tuple_and_new_are_equivalent() {
        let ev = MidiEvent::note_on(0, 0, 60, 0x8000);
        assert_eq!(ClipEvent::new(240, ev), ClipEvent::from((240, ev)));
    }

    #[test]
    fn errors_distinguish_failure_modes() {
        // Not a clip file at all.
        assert_eq!(
            read_clip_file(b"NOTACLIP\0\0\0\0"),
            Err(ClipFileError::BadMagic)
        );
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
