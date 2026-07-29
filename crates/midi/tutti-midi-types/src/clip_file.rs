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

/// The musical context a clip declares up front: tempo and time signature.
///
/// M2-116 §7.1.1 and §7.1.2 say the sequence *should* open with a Set Tempo and
/// a Set Time Signature, in that order, immediately after Start of Clip. Without
/// them a reader has no tempo map, so [`ParsedClipFile::timed`]'s beats cannot be
/// converted to seconds by any importer.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ClipHeader {
    /// Quarter-notes per minute.
    pub tempo_bpm: f64,
    /// Beats per bar and the beat unit, e.g. `(4, 4)`.
    pub time_signature: (u8, u8),
}

impl Default for ClipHeader {
    /// 120 BPM, 4/4 — the conventional default when a caller has no better idea.
    fn default() -> Self {
        Self {
            tempo_bpm: 120.0,
            time_signature: (4, 4),
        }
    }
}

/// Serialize a MIDI Clip File (M2-116) from a tick-per-quarter unit and a flat
/// list of timed events. Emits header + DCTPQ + Start-of-Clip + (DCS·UMP)* +
/// End-of-Clip, big-endian.
///
/// This writes no Set Tempo or Set Time Signature, which §7.1.1/§7.1.2 recommend
/// — use [`write_clip_file_with_header`] when you know the clip's musical
/// context, so the file carries its own tempo map.
pub fn write_clip_file(ticks_per_quarter: u16, events: &[ClipEvent]) -> Vec<u8> {
    write_clip(ticks_per_quarter, None, events)
}

/// Serialize a MIDI Clip File that opens with its tempo and time signature, per
/// M2-116 §7.1.1 / §7.1.2 — the shape an importer needs to place the clip in
/// real time.
pub fn write_clip_file_with_header(
    ticks_per_quarter: u16,
    header: ClipHeader,
    events: &[ClipEvent],
) -> Vec<u8> {
    write_clip(ticks_per_quarter, Some(header), events)
}

fn write_clip(ticks_per_quarter: u16, header: Option<ClipHeader>, events: &[ClipEvent]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + (events.len() + 3) * 8);
    out.extend_from_slice(&CLIP_FILE_MAGIC);

    // DCTPQ declares the tick unit (preceded by a zero DCS per §3.2.1).
    push_words(&mut out, delta_clockstamp(0).data_words());
    push_words(&mut out, dctpq(ticks_per_quarter).data_words());

    // Start of Clip (preceded by a zero DCS — bar 1 begins here).
    push_words(&mut out, delta_clockstamp(0).data_words());
    push_words(&mut out, start_of_clip().data_words());

    // Set Tempo then Set Time Signature, in that order and at the same (zero)
    // clockstamp as Start of Clip — M2-116 §7.1.1: the first Set Tempo "should
    // use the Delta Clockstamp which precedes the Start of Clip message";
    // §7.1.2 puts the time signature "immediately following the Start of Clip
    // and Set Tempo messages".
    if let Some(h) = header {
        let (numerator, denominator) = h.time_signature;
        push_words(&mut out, delta_clockstamp(0).data_words());
        push_words(
            &mut out,
            MidiEvent::flex_set_tempo(0, h.tempo_bpm).data_words(),
        );
        push_words(&mut out, delta_clockstamp(0).data_words());
        push_words(
            &mut out,
            // 8 thirty-second notes per quarter — the standard value.
            MidiEvent::flex_set_time_signature(0, numerator, denominator, 8).data_words(),
        );
    }

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

    /// The clip's own tempo in BPM — the first Flex Data **Set Tempo** in the
    /// sequence (M2-116 §7.1.1), or `None` if the file declares none.
    ///
    /// A clip file carries its own tempo map, and it is not the project's: an
    /// importer that ignores this places every note at the wrong wall-clock
    /// time, silently. Read it and decide explicitly whether to adopt it or
    /// keep the project tempo — do not let it default by omission.
    pub fn tempo_bpm(&self) -> Option<f64> {
        self.events
            .iter()
            .find_map(|ce| crate::ump::flex_tempo_bpm(&ce.event))
    }

    /// The clip's first Flex Data **Set Time Signature** as
    /// `(numerator, denominator)` (M2-116 §7.1.2), or `None` if unset.
    pub fn time_signature(&self) -> Option<(u8, u8)> {
        self.events
            .iter()
            .find_map(|ce| crate::ump::flex_time_signature(&ce.event))
    }

    /// The clip's musical length in beats: the absolute beat of the last event
    /// (0.0 for an empty clip). Note-offs are events too, so this reflects where
    /// the last message lands, not necessarily where sound stops.
    pub fn duration_beats(&self) -> f64 {
        let total_ticks: u64 = self.events.iter().map(|ce| u64::from(ce.delta_ticks)).sum();
        total_ticks as f64 / f64::from(self.ticks_per_quarter)
    }

    /// Pair the event stream into whole notes with durations — the shape an
    /// importer wants, and the MIDI-2 analogue of `tutti_midi_io::smf::tracks`.
    ///
    /// Velocity stays 16-bit ([`ClipNote::velocity`]): pairing here rather than
    /// in a caller is what keeps a clip file's full-resolution velocity from
    /// being narrowed to 7 bits on the way in.
    ///
    /// Pairing is keyed on `(group, channel, note)`, so a Note Off only closes a
    /// Note On on the same group *and* channel; overlapping identical notes
    /// close in LIFO order; notes still open at End of Clip are dropped (their
    /// duration is unknowable). MIDI 1.0 velocity-0 Note On counts as a Note Off
    /// — [`MidiEvent::is_note_off`] already folds that in.
    pub fn notes(&self) -> Vec<ClipNote> {
        use std::collections::BTreeMap;

        let mut held: BTreeMap<(u8, u8, u8), Vec<(f64, u16)>> = BTreeMap::new();
        let mut out: Vec<ClipNote> = Vec::new();

        for (beat, event) in self.timed() {
            let (Some(note), Some(channel)) = (event.note(), event.channel()) else {
                continue;
            };
            let key = (event.group(), channel, note);
            // Order matters: a velocity-0 MIDI 1.0 Note On satisfies both
            // predicates' shapes, and `is_note_off` is the one that claims it.
            if event.is_note_off() {
                if let Some((start, velocity)) = held.get_mut(&key).and_then(Vec::pop) {
                    out.push(ClipNote {
                        group: key.0,
                        channel,
                        note,
                        velocity,
                        start_beats: start,
                        duration_beats: (beat - start).max(0.0),
                    });
                }
            } else if event.is_note_on() {
                let velocity = event.velocity_u16().unwrap_or(0);
                held.entry(key).or_default().push((beat, velocity));
            }
        }

        // Onset order, then key, so simultaneous notes have a stable order
        // rather than one that depends on when their Note Offs arrived.
        out.sort_by(|a, b| {
            a.start_beats
                .partial_cmp(&b.start_beats)
                .unwrap_or(core::cmp::Ordering::Equal)
                .then_with(|| (a.group, a.channel, a.note).cmp(&(b.group, b.channel, b.note)))
        });
        out
    }
}

/// One note from a clip file, paired from its Note On / Note Off, in beats from
/// the clip start. The MIDI-2 counterpart of `tutti_midi_io::smf::SmfNote` —
/// same shape, but velocity keeps all 16 bits and the UMP group is carried.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ClipNote {
    /// UMP group, 0..=15. Part of the pairing key: groups are independent
    /// 16-channel spaces, so the same channel+note in two groups is two notes.
    pub group: u8,
    /// MIDI channel, 0..=15.
    pub channel: u8,
    /// Note number, 0..=127.
    pub note: u8,
    /// Note On velocity at full MIDI 2.0 width. A MIDI 1.0 note in the clip is
    /// upscaled by Min-Center-Max, so this is lossless in both directions.
    pub velocity: u16,
    /// Onset in beats (quarter notes) from the clip start.
    pub start_beats: f64,
    /// Duration in beats (Note Off beat − Note On beat, clamped to ≥ 0).
    pub duration_beats: f64,
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
    /// No Start of Clip message. M2-116 §7: "A Clip Sequence Data shall include
    /// one Start of Clip message … as the first UMP message."
    MissingStartOfClip,
    /// No End of Clip message. §7: "A Clip Sequence Data shall include one End
    /// of Clip message as the last UMP message."
    MissingEndOfClip,
    /// Bytes follow the End of Clip. §7.3: "A MIDI Clip File shall not have any
    /// data following the End of Clip message." There is no multi-clip clip
    /// file — that is what the MIDI Container File is for.
    TrailingData,
}

impl core::fmt::Display for ClipFileError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let msg = match self {
            Self::BadMagic => "not a MIDI Clip File (bad or missing SMF2CLIP magic)",
            Self::Unaligned => "clip body is not 32-bit-word aligned",
            Self::Truncated => "clip file is truncated mid-message",
            Self::MissingDctpq => "clip file has no DCTPQ tick-unit declaration",
            Self::MissingStartOfClip => "clip file has no Start of Clip message",
            Self::MissingEndOfClip => "clip file has no End of Clip message",
            Self::TrailingData => "clip file has data following the End of Clip message",
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
    // §7 requires both brackets. Tracked rather than assumed: a file missing
    // either is malformed, and `ClipFileError` exists to say which.
    let mut saw_start_of_clip = false;
    let mut saw_end_of_clip = false;

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
                saw_start_of_clip = true;
                continue; // structural
            }
            if status == 0x021 {
                saw_end_of_clip = true;
                // §7.3: "A MIDI Clip File shall not have any data following the
                // End of Clip message." There is no multi-clip clip file, so
                // anything after this is corruption rather than a second clip.
                if words.peek().is_some() {
                    return Err(ClipFileError::TrailingData);
                }
                break;
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

    // Order matters: report the *earliest* structural thing that is missing, so
    // a caller sees the first reason the file is unusable rather than the last.
    let ticks_per_quarter = ticks_per_quarter.ok_or(ClipFileError::MissingDctpq)?;
    if !saw_start_of_clip {
        return Err(ClipFileError::MissingStartOfClip);
    }
    if !saw_end_of_clip {
        return Err(ClipFileError::MissingEndOfClip);
    }

    Ok(ParsedClipFile {
        ticks_per_quarter,
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
        push(0x0040_0000); // DCS(0)
        for w in [0xF021_0000, 0, 0, 0] {
            push(w); // End of Clip
        }

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
        push(0x0040_0000); // DCS(0)
        for w in [0xF020_0000, 0, 0, 0] {
            push(w); // Start of Clip
        }
        push(0x0040_0000 | 500); // DCS(500)
        push(0x0040_0000 | 1000); // DCS(1000) — replaces, not adds
        push(0x4090_3C00);
        push(0x8000_0000);
        push(0x0040_0000); // DCS(0)
        for w in [0xF021_0000, 0, 0, 0] {
            push(w); // End of Clip
        }

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
    fn header_round_trips_tempo_and_time_signature() {
        // M2-116 §7.1.1/§7.1.2: the sequence opens with Set Tempo then Set Time
        // Signature. A clip carries its own tempo map; an importer that can't
        // read it back places every note at the wrong wall-clock time.
        let header = ClipHeader {
            tempo_bpm: 174.0,
            time_signature: (7, 8),
        };
        let bytes = write_clip_file_with_header(
            480,
            header,
            &[ClipEvent::new(0, MidiEvent::note_on(0, 0, 60, 0x8000))],
        );

        let clip = read_clip_file(&bytes).expect("parses");
        assert!(
            (clip.tempo_bpm().expect("tempo present") - 174.0).abs() < 0.05,
            "tempo survives the round trip"
        );
        assert_eq!(clip.time_signature(), Some((7, 8)));

        // Ordering: tempo comes before the time signature, both before the note.
        let kinds: Vec<_> = clip
            .events
            .iter()
            .map(|ce| {
                (
                    crate::ump::flex_tempo_bpm(&ce.event).is_some(),
                    crate::ump::flex_time_signature(&ce.event).is_some(),
                )
            })
            .collect();
        assert_eq!(kinds[0], (true, false), "Set Tempo first");
        assert_eq!(kinds[1], (false, true), "Set Time Signature second");

        // The note still lands at beat 0 — the header messages share its
        // zero clockstamp rather than pushing it later.
        let timed: Vec<(f64, MidiEvent)> = clip.timed().collect();
        assert!(timed.iter().all(|(b, _)| *b == 0.0));

        // A clip written without a header simply declares none.
        let bare = read_clip_file(&write_clip_file(480, &[])).expect("parses");
        assert_eq!(bare.tempo_bpm(), None);
        assert_eq!(bare.time_signature(), None);
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

    #[test]
    fn structural_shalls_are_enforced() {
        // M2-116 §7 requires both brackets, and §7.3 forbids anything after End
        // of Clip. These were all accepted silently, which defeats the point of
        // ClipFileError distinguishing "not a clip file" from "malformed".
        let mut base = Vec::new();
        base.extend_from_slice(&CLIP_FILE_MAGIC);
        for w in [0x0040_0000u32, 0x0030_01E0] {
            base.extend_from_slice(&w.to_be_bytes()); // DCS(0) + DCTPQ
        }
        let push = |bytes: &mut Vec<u8>, words: &[u32]| {
            for w in words {
                bytes.extend_from_slice(&w.to_be_bytes());
            }
        };

        // DCTPQ but no Start of Clip.
        let mut no_start = base.clone();
        push(&mut no_start, &[0x0040_0000, 0xF021_0000, 0, 0, 0]);
        assert_eq!(
            read_clip_file(&no_start),
            Err(ClipFileError::MissingStartOfClip)
        );

        // Start but no End — a file truncated mid-clip.
        let mut no_end = base.clone();
        push(&mut no_end, &[0x0040_0000, 0xF020_0000, 0, 0, 0]);
        assert_eq!(
            read_clip_file(&no_end),
            Err(ClipFileError::MissingEndOfClip)
        );

        // Data after End of Clip: §7.3 forbids it, and there is no such thing
        // as a multi-clip clip file (that is the MIDI Container File's job).
        let mut trailing = base.clone();
        push(
            &mut trailing,
            &[
                0x0040_0000,
                0xF020_0000,
                0,
                0,
                0, // Start of Clip
                0x0040_0000,
                0xF021_0000,
                0,
                0,
                0, // End of Clip
                0x0040_0000,
                0x4090_3C00,
                0x8000_0000, // …then a stray note-on
            ],
        );
        assert_eq!(read_clip_file(&trailing), Err(ClipFileError::TrailingData));

        // A well-formed file with both brackets still parses.
        assert!(read_clip_file(&write_clip_file(480, &[])).is_ok());
    }

    // --- Note pairing ---

    /// Round-trip a beat-positioned event list and pair it back into notes.
    fn notes_from(events: impl IntoIterator<Item = (f64, MidiEvent)>) -> Vec<ClipNote> {
        let bytes = write_clip_file_from_beats(96, events);
        read_clip_file(&bytes).unwrap().notes()
    }

    #[test]
    fn pairs_notes_and_keeps_full_velocity() {
        let notes = notes_from([
            (0.0, MidiEvent::note_on(0, 0, 60, 0xABCD)),
            (2.5, MidiEvent::note_off(0, 0, 60, 0)),
        ]);

        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].note, 60);
        assert_eq!(notes[0].start_beats, 0.0);
        assert_eq!(notes[0].duration_beats, 2.5);
        // The reason pairing lives here: a caller pairing on `velocity_u7`
        // would have silently narrowed this to 7 bits.
        assert_eq!(notes[0].velocity, 0xABCD);
    }

    #[test]
    fn a_note_off_only_closes_its_own_channel_and_group() {
        // Same note number in three different (group, channel) spaces, closed
        // in a deliberately scrambled order.
        let notes = notes_from([
            (0.0, MidiEvent::note_on(0, 0, 60, 0x1000)),
            (0.0, MidiEvent::note_on(0, 1, 60, 0x2000)),
            (0.0, MidiEvent::note_on(1, 0, 60, 0x3000)),
            (1.0, MidiEvent::note_off(1, 0, 60, 0)),
            (2.0, MidiEvent::note_off(0, 1, 60, 0)),
            (3.0, MidiEvent::note_off(0, 0, 60, 0)),
        ]);

        assert_eq!(notes.len(), 3);
        // Sorted by (group, channel) at equal onset, so the order is stable.
        assert_eq!(
            notes
                .iter()
                .map(|n| (n.group, n.channel))
                .collect::<Vec<_>>(),
            [(0, 0), (0, 1), (1, 0)]
        );
        // Each closed against its own Note Off, not the nearest one.
        assert_eq!(notes[0].duration_beats, 3.0);
        assert_eq!(notes[1].duration_beats, 2.0);
        assert_eq!(notes[2].duration_beats, 1.0);
        assert_eq!(
            notes.iter().map(|n| n.velocity).collect::<Vec<_>>(),
            [0x1000, 0x2000, 0x3000]
        );
    }

    #[test]
    fn overlapping_identical_notes_close_in_lifo_order() {
        let notes = notes_from([
            (0.0, MidiEvent::note_on(0, 0, 60, 0x1000)),
            (1.0, MidiEvent::note_on(0, 0, 60, 0x2000)),
            (2.0, MidiEvent::note_off(0, 0, 60, 0)),
            (4.0, MidiEvent::note_off(0, 0, 60, 0)),
        ]);

        assert_eq!(notes.len(), 2);
        // The first Note Off closes the *most recent* Note On.
        assert_eq!((notes[0].start_beats, notes[0].duration_beats), (0.0, 4.0));
        assert_eq!((notes[1].start_beats, notes[1].duration_beats), (1.0, 1.0));
    }

    #[test]
    fn a_midi1_velocity_zero_note_on_closes_a_note() {
        // MIDI 1.0's running-status idiom: NoteOn with velocity 0 is a NoteOff.
        // If it were treated as an onset instead, this would pair as two open
        // notes and yield nothing. Built from MIDI 1.0 bytes so these are real
        // Channel Voice 1 packets — `note_on_7bit` would widen to CV2 and never
        // reach this path.
        let on = MidiEvent::from_midi1_bytes(0, &[0x90, 60, 100]).expect("midi1 note-on");
        let off = MidiEvent::from_midi1_bytes(0, &[0x90, 60, 0]).expect("midi1 note-on vel 0");
        let notes = notes_from([(0.0, on), (1.5, off)]);

        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].duration_beats, 1.5);
        // A MIDI 1.0 velocity upscales by Min-Center-Max rather than shifting.
        assert_eq!(
            notes[0].velocity,
            crate::convert::midi1_velocity_to_midi2(100)
        );
    }

    #[test]
    fn notes_left_open_at_end_of_clip_are_dropped() {
        let notes = notes_from([
            (0.0, MidiEvent::note_on(0, 0, 60, 0x8000)),
            (1.0, MidiEvent::note_off(0, 0, 60, 0)),
            // Never closed — its duration is unknowable, so it is not a note.
            (2.0, MidiEvent::note_on(0, 0, 64, 0x8000)),
        ]);

        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].note, 60);
    }

    #[test]
    fn non_note_events_do_not_disturb_pairing() {
        let notes = notes_from([
            (0.0, MidiEvent::note_on(0, 0, 60, 0x8000)),
            (0.5, MidiEvent::cc(0, 0, 74, 0x4000)),
            (1.0, MidiEvent::note_off(0, 0, 60, 0)),
        ]);

        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].duration_beats, 1.0);
    }
}
