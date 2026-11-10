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
//! This module models the file as a flat list of `(delta_ticks, Ump)`,
//! which is all a Type-0-like clip needs; the caller supplies the DCTPQ.

use std::vec::Vec;

use midi2::Data;
use midi2::ump_stream::{EndOfClip, StartOfClip};
use midi2::utility::{DeltaClockstamp, DeltaClockstampTpq};
use midi2::ux::u20;

use tutti_midi_protocol::channel_voice;
use tutti_types::{Beat, BeatDuration, Bpm};
use tutti_ump::{KeyEvent, Message, MidiChannel, MidiGroup, Ump, Utility, Voice2};

/// The 8-byte file header: ASCII "SMF2CLIP" (M2-116 §5).
pub const CLIP_FILE_MAGIC: [u8; 8] = *b"SMF2CLIP";

/// A timed UMP event in a clip: `delta_ticks` since the previous event, then the
/// event itself.
///
/// This is the file's on-wire timing model (relative ticks). If you think in
/// beats — as most callers do — prefer [`write_clip_file_from_beats`] and
/// [`ParsedClipFile::iter`], which own the beat↔tick↔delta conversion so you
/// never build these by hand.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClipEvent {
    /// Ticks since the *previous* event, not since the clip start. Absolute
    /// position is the running sum, which is why a dropped event shifts every
    /// event after it.
    pub delta_ticks: u32,
    /// The event itself. Only its UMP words are written; position in the clip
    /// comes from `delta_ticks`.
    pub event: Ump,
}

impl ClipEvent {
    /// A timed event: `delta_ticks` since the previous event, then the event.
    #[inline]
    #[must_use]
    pub const fn new(delta_ticks: u32, event: Ump) -> Self {
        Self { delta_ticks, event }
    }
}

impl From<(u32, Ump)> for ClipEvent {
    /// `(delta_ticks, event)` — lets a caller write the tuple form.
    #[inline]
    fn from((delta_ticks, event): (u32, Ump)) -> Self {
        Self { delta_ticks, event }
    }
}

/// The musical context a clip declares up front: tempo and time signature.
///
/// M2-116 §7.1.1 and §7.1.2 say the sequence *should* open with a Set Tempo and
/// a Set Time Signature, in that order, immediately after Start of Clip. Without
/// them a reader has no tempo map, so [`ParsedClipFile::iter`]'s beats cannot be
/// converted to seconds by any importer.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ClipHeader {
    /// Quarter-notes per minute.
    pub tempo_bpm: Bpm,
    /// Beats per bar and the beat unit, e.g. `(4, 4)`.
    pub time_signature: (u8, u8),
}

impl Default for ClipHeader {
    /// 120 BPM, 4/4 — the conventional default when a caller has no better idea.
    fn default() -> Self {
        Self {
            tempo_bpm: Bpm::of(120.0),
            time_signature: (4, 4),
        }
    }
}

/// Serializes a MIDI Clip File (M2-116) from a tick-per-quarter unit and a flat
/// list of timed events. Emits header + DCTPQ + Start-of-Clip + (DCS·UMP)* +
/// End-of-Clip, big-endian.
///
/// This writes no Set Tempo or Set Time Signature, which §7.1.1/§7.1.2 recommend
/// — use [`write_clip_file_with_header`] when you know the clip's musical
/// context, so the file carries its own tempo map.
pub fn write_clip_file(ticks_per_quarter: u16, events: &[ClipEvent]) -> Vec<u8> {
    write_clip(ticks_per_quarter, None, events)
}

/// Serializes a MIDI Clip File that opens with its tempo and time signature, per
/// M2-116 §7.1.1 / §7.1.2 — the shape an importer needs to place the clip in
/// real time.
///
/// **`header` replaces a header the events already carry.** Tempo and time
/// signature are ordinary events in this format, so a clip that came from
/// [`ParsedClipFile::from_bytes`] already has its pair sitting at the front of
/// `events`; any such leading zero-delta pair is dropped in favour of
/// `header`. Without that, re-encoding a parsed clip grew a duplicate pair
/// on every cycle. A Set Tempo later in the stream is a mid-clip tempo
/// change and is left alone.
///
/// Re-encode a parsed clip with [`ParsedClipFile::header`]:
///
/// ```
/// # use tutti_midi_types::{ParsedClipFile, write_clip_file_with_header, ClipHeader};
/// # let first = write_clip_file_with_header(96, ClipHeader::default(), &[]);
/// let clip = ParsedClipFile::from_bytes(&first).unwrap();
/// let again = write_clip_file_with_header(
///     clip.ticks_per_quarter,
///     clip.header().unwrap(),
///     &clip.events,
/// );
/// assert_eq!(first, again, "a round trip is byte-stable");
/// ```
pub fn write_clip_file_with_header(
    ticks_per_quarter: u16,
    header: ClipHeader,
    events: &[ClipEvent],
) -> Vec<u8> {
    write_clip(ticks_per_quarter, Some(header), events)
}

/// How many events at the head of `events` sit in the **header position** — the
/// run of zero-delta Set Tempo / Set Time Signature messages that M2-116
/// §7.1.1 / §7.1.2 place immediately after Start of Clip.
///
/// Only the *leading* run counts, and that is the whole point: a Set Tempo
/// further along the stream is a mid-clip tempo change, which is musical
/// content and must survive a round trip untouched.
fn leading_header_len(events: &[ClipEvent]) -> usize {
    events
        .iter()
        .take_while(|ce| {
            ce.delta_ticks == 0
                && (crate::ump::flex_tempo_bpm(&ce.event).is_some()
                    || crate::ump::flex_time_signature(&ce.event).is_some())
        })
        .count()
}

fn write_clip(ticks_per_quarter: u16, header: Option<ClipHeader>, events: &[ClipEvent]) -> Vec<u8> {
    // A supplied header REPLACES one the events already carry rather than being
    // prepended to it. Tempo and time signature are ordinary events in this
    // format, so `ParsedClipFile::from_bytes` hands them back inside `events` — and writing
    // the header on top of them would make a parse -> write -> parse cycle
    // grow a duplicate pair every time round. Stripping is confined to the
    // leading zero-delta run, so a mid-clip tempo change is not touched.
    let events = if header.is_some() {
        &events[leading_header_len(events)..]
    } else {
        events
    };
    let mut out = Vec::with_capacity(8 + (events.len() + 3) * 8);
    out.extend_from_slice(&CLIP_FILE_MAGIC);

    // DCTPQ declares the tick unit (preceded by a zero DCS per §3.2.1).
    push_words(&mut out, delta_clockstamp(0).words());
    push_words(&mut out, dctpq(ticks_per_quarter).words());

    // Start of Clip (preceded by a zero DCS — bar 1 begins here).
    push_words(&mut out, delta_clockstamp(0).words());
    push_words(&mut out, start_of_clip().words());

    // Set Tempo then Set Time Signature, in that order and at the same (zero)
    // clockstamp as Start of Clip — M2-116 §7.1.1: the first Set Tempo "should
    // use the Delta Clockstamp which precedes the Start of Clip message";
    // §7.1.2 puts the time signature "immediately following the Start of Clip
    // and Set Tempo messages".
    if let Some(h) = header {
        let (numerator, denominator) = h.time_signature;
        push_words(&mut out, delta_clockstamp(0).words());
        push_words(
            &mut out,
            crate::ump::flex_set_tempo(MidiGroup::FIRST, h.tempo_bpm).words(),
        );
        push_words(&mut out, delta_clockstamp(0).words());
        push_words(
            &mut out,
            // 8 thirty-second notes per quarter — the standard value.
            crate::ump::flex_set_time_signature(MidiGroup::FIRST, numerator, denominator, 8)
                .words(),
        );
    }

    // The timed event stream: each UMP preceded by its delta clockstamp(s).
    // A DCS field is only 20 bits, so a delta beyond `DCS_MAX` is expressed as
    // DCS·NOOP restarts (M2-116 §3.2.2) — see `push_delta`.
    for ev in events {
        push_delta(&mut out, ev.delta_ticks);
        push_words(&mut out, ev.event.words());
    }

    // End of Clip (preceded by a zero DCS).
    push_words(&mut out, delta_clockstamp(0).words());
    push_words(&mut out, end_of_clip().words());
    out
}

/// Serializes a MIDI Clip File from **beat-positioned** events — the ergonomic
/// entry point when you think in beats rather than delta ticks.
///
/// Each `(beat, event)` carries an *absolute* beat position (quarter notes from
/// the clip start). This quantizes them to `ticks_per_quarter` and computes the
/// inter-event delta ticks for you, so you never accumulate deltas by hand.
/// Input need not be sorted — events are ordered by beat first; a zero or
/// negative gap (simultaneous or slightly out-of-order events) becomes a
/// zero-tick delta. A non-finite beat has no tick: it is written with a
/// zero-tick delta wherever it sorts (`+inf` and a NaN after every other
/// event, `-inf` and a NaN whose sign bit is set before them). A gap longer
/// than `u32::MAX` ticks is clamped to it.
///
/// ```
/// # use tutti_midi_types::{write_clip_file_from_beats, ParsedClipFile};
/// # use tutti_midi_types::Beat;
/// # use tutti_ump::{MidiChannel, MidiGroup};
/// use tutti_fixtures::midi::{note_off, note_on};
/// let bytes = write_clip_file_from_beats(96, [
///     (Beat(0.0), note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x8000)),
///     (Beat(2.0), note_off(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0)),
/// ]);
/// let clip = ParsedClipFile::from_bytes(&bytes).unwrap();
/// assert_eq!(clip.iter().count(), 2);
/// ```
pub fn write_clip_file_from_beats(
    ticks_per_quarter: u16,
    events: impl IntoIterator<Item = (Beat, Ump)>,
) -> Vec<u8> {
    let tpq = f64::from(ticks_per_quarter);
    let mut timed: Vec<(Beat, Ump)> = events.into_iter().collect();
    // `total_cmp`: a total order even with a NaN beat, which a partial
    // order made `Equal` to everything (an inconsistent order the sort may
    // panic on).
    timed.sort_by(|a, b| a.0.get().total_cmp(&b.0.get()));

    let mut prev_tick: i64 = 0;
    let clip_events: Vec<ClipEvent> = timed
        .into_iter()
        .map(|(beat, event)| {
            let pos = (beat.get() * tpq).round();
            // A non-finite beat has no tick: write it at the previous
            // event's tick. `as i64` would saturate ±inf to `i64::MAX`/`MIN`
            // and the subtraction below would overflow on the next event.
            if !pos.is_finite() {
                return ClipEvent::new(0, event);
            }
            // `as i64` saturates a huge finite position; the gap saturates
            // too, and a gap past `u32::MAX` ticks is clamped rather than
            // truncated (`push_delta` splits any `u32` into DCS restarts).
            let tick = pos as i64;
            let delta = tick.saturating_sub(prev_tick).clamp(0, i64::from(u32::MAX)) as u32;
            prev_tick = tick;
            ClipEvent::new(delta, event)
        })
        .collect();
    write_clip_file(ticks_per_quarter, &clip_events)
}

/// A parsed MIDI Clip File.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedClipFile {
    /// Ticks per quarter note — the resolution every `delta_ticks` in `events`
    /// is denominated in. Needed to convert any of them to `Beat`.
    pub ticks_per_quarter: u16,
    /// Events in file order. Their positions are relative, so the order is
    /// load-bearing; use [`iter`](Self::iter) to get absolute `Beat`s.
    pub events: Vec<ClipEvent>,
}

impl ParsedClipFile {
    /// Parses a MIDI Clip File (M2-116) from its bytes.
    ///
    /// Delta Clockstamps set the delta of the following UMP; Start/End-of-Clip and
    /// the DCTPQ are structural and not returned as events. Unknown/utility
    /// messages between a DCS and a real event are tolerated.
    ///
    /// # Errors
    ///
    /// Returns a [`ClipFileError`] naming the first structural problem: a bad
    /// magic, a body that is not whole UMP words, a truncated UMP, a missing or
    /// zero DCTPQ, a missing Start or End of Clip, or data after the End of Clip.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, ClipFileError> {
        parse_clip(bytes)
    }

    /// Re-encodes the clip as Clip File bytes, events as they stand (a tempo
    /// and time signature the file declared are events, so they are kept).
    pub fn to_bytes(&self) -> Vec<u8> {
        write_clip_file(self.ticks_per_quarter, &self.events)
    }

    /// Iterate events as `(beat, event)` with **absolute** beat positions —
    /// quarter notes from the clip start — undoing the file's relative delta
    /// ticks. The inverse of [`write_clip_file_from_beats`]: the natural way to
    /// consume a clip when you schedule in beats.
    ///
    /// Beats are `delta_ticks` summed and divided by [`Self::ticks_per_quarter`].
    pub fn iter(&self) -> impl Iterator<Item = (Beat, Ump)> + '_ {
        let tpq = f64::from(self.ticks_per_quarter);
        let mut abs_tick: u64 = 0;
        self.events.iter().map(move |ce| {
            abs_tick += u64::from(ce.delta_ticks);
            (Beat(abs_tick as f64 / tpq), ce.event)
        })
    }

    /// The clip's own tempo in BPM — the first Flex Data **Set Tempo** in the
    /// sequence (M2-116 §7.1.1), or `None` if the file declares none.
    ///
    /// A clip file carries its own tempo map, and it is not the project's: an
    /// importer that ignores this places every note at the wrong wall-clock
    /// time, silently. Read it and decide explicitly whether to adopt it or
    /// keep the project tempo — do not let it default by omission.
    pub fn tempo_bpm(&self) -> Option<Bpm> {
        self.events
            .iter()
            .find_map(|ce| crate::ump::flex_tempo_bpm(&ce.event))
    }

    /// The clip's musical context as a [`ClipHeader`] — the value to hand back
    /// to [`write_clip_file_with_header`] when re-encoding this clip.
    ///
    /// `Some` only when the file declares **both** a tempo and a time
    /// signature. A clip carrying one and not the other has no complete
    /// header, and substituting [`ClipHeader::default`]'s 120/4-4 for the
    /// missing half is a decision for the caller — doing it here would write a
    /// tempo the file never claimed and make it indistinguishable from one it
    /// did.
    pub fn header(&self) -> Option<ClipHeader> {
        Some(ClipHeader {
            tempo_bpm: self.tempo_bpm()?,
            time_signature: self.time_signature()?,
        })
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
    pub fn duration_beats(&self) -> BeatDuration {
        let total_ticks: u64 = self.events.iter().map(|ce| u64::from(ce.delta_ticks)).sum();
        BeatDuration(total_ticks as f64 / f64::from(self.ticks_per_quarter))
    }

    /// Pair the event stream into whole notes with durations — the shape an
    /// importer wants, and the MIDI-2 analogue of `tutti_midi_file::SmfFile::tracks`.
    ///
    /// Velocity stays 16-bit ([`ClipNote::velocity`]): pairing here rather than
    /// in a caller is what keeps a clip file's full-resolution velocity from
    /// being narrowed to 7 bits on the way in.
    ///
    /// Pairing is keyed on `(group, channel, note)`, so a Note Off only closes a
    /// Note On on the same group *and* channel; overlapping identical notes
    /// close in LIFO order; notes still open at End of Clip are dropped (their
    /// duration is unknowable). MIDI 1.0 velocity-0 Note On counts as a Note Off
    /// — [`Ump::is_note_off`] already folds that in.
    pub fn notes(&self) -> Vec<ClipNote> {
        use std::collections::BTreeMap;

        // `Beat`, so `beat - start` below is a `BeatDuration` by construction.
        let mut held: BTreeMap<(MidiGroup, MidiChannel, u8), Vec<(Beat, u16)>> = BTreeMap::new();
        let mut out: Vec<ClipNote> = Vec::new();

        for (beat, event) in self.iter() {
            // `key_event` reads the status, group, channel and note bits of a
            // MIDI 1.0 or 2.0 channel-voice packet and counts a MIDI 1.0 Note On
            // at velocity 0 as an off, as `Ump::is_note_off` does.
            let Some(key_event) = event.key_event() else {
                continue;
            };
            let (KeyEvent::On(key) | KeyEvent::Off(key)) = key_event else {
                continue;
            };
            let lane = key.lane();
            let group = MidiGroup::masked(lane >> 4);
            let channel = MidiChannel::masked(lane & 0x0F);
            let note = key.note();
            let map_key = (group, channel, note);
            if matches!(key_event, KeyEvent::Off(_)) {
                if let Some((start, velocity)) = held.get_mut(&map_key).and_then(Vec::pop) {
                    out.push(ClipNote {
                        group,
                        channel,
                        note,
                        velocity,
                        start_beats: start,
                        duration_beats: (beat - start).max(BeatDuration(0.0)),
                    });
                }
            } else {
                // A Note On at velocity 0 is only an off in MIDI 1.0; a MIDI 2.0
                // one is neither a start nor an end here.
                // A MIDI 1.0 Note On widens (`channel_voice`), a MIDI 2.0 one
                // keeps its 16 bits.
                let velocity = match channel_voice(event) {
                    Some((_, Voice2::NoteOn { velocity, .. })) => velocity,
                    _ => 0,
                };
                if velocity > 0 {
                    held.entry(map_key).or_default().push((beat, velocity));
                }
            }
        }

        // Onset order, then key, so simultaneous notes have a stable order
        // rather than one that depends on when their Note Offs arrived.
        out.sort_by(|a, b| {
            a.start_beats
                .get()
                .total_cmp(&b.start_beats.get())
                .then_with(|| (a.group, a.channel, a.note).cmp(&(b.group, b.channel, b.note)))
        });
        out
    }
}

/// One note from a clip file, paired from its Note On / Note Off, in beats from
/// the clip start. The MIDI-2 counterpart of `tutti_midi_file::SmfNote` —
/// same shape, but velocity keeps all 16 bits and the UMP group is carried.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ClipNote {
    /// UMP group. Part of the pairing key: groups are independent 16-channel
    /// spaces, so the same channel+note in two groups is two notes.
    pub group: MidiGroup,
    /// MIDI channel.
    pub channel: MidiChannel,
    /// Note number, 0..=127.
    pub note: u8,
    /// Note On velocity at full MIDI 2.0 width. A MIDI 1.0 note in the clip is
    /// upscaled by Min-Center-Max, so this is lossless in both directions.
    pub velocity: u16,
    /// Onset (quarter notes) from the clip start.
    pub start_beats: Beat,
    /// Length (Note Off beat − Note On beat, clamped to ≥ 0).
    ///
    /// A [`BeatDuration`] beside a [`Beat`] — a span and a position, so the
    /// pair cannot be transposed. The MIDI-wire fields above stay
    /// integers: a channel nibble and a 16-bit velocity are protocol values,
    /// not measurements.
    pub duration_beats: BeatDuration,
}

/// Why a byte stream failed to parse as a MIDI Clip File (M2-116). Distinguishes
/// "this isn't a clip file" from "this clip file is malformed" so a caller can
/// react differently (e.g. try another importer vs. report corruption).
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ClipFileError {
    /// Fewer than 8 bytes, or the leading 8 bytes are not [`CLIP_FILE_MAGIC`].
    #[error("not a MIDI Clip File (bad or missing SMF2CLIP magic)")]
    BadMagic,
    /// The body length is not a whole number of 32-bit UMP words.
    #[error("clip body is not 32-bit-word aligned")]
    Unaligned,
    /// A UMP claims more words than remain in the stream.
    #[error("clip file is truncated mid-message")]
    Truncated,
    /// No DCTPQ (tick-unit declaration) was seen before the events.
    #[error("clip file has no DCTPQ tick-unit declaration")]
    MissingDctpq,
    /// The DCTPQ declared zero ticks per quarter note.
    ///
    /// Distinct from [`MissingDctpq`](Self::MissingDctpq) because the file did
    /// declare one — it is unusable rather than absent, and a caller reporting
    /// corruption wants to say which. Every beat in the file is that tick count
    /// divided by this, so a zero yields infinities rather than a parse error
    /// unless it is caught here.
    #[error("clip file declares zero ticks per quarter note")]
    ZeroDctpq,
    /// No Start of Clip message. M2-116 §7: "A Clip Sequence Data shall include
    /// one Start of Clip message … as the first UMP message."
    #[error("clip file has no Start of Clip message")]
    MissingStartOfClip,
    /// No End of Clip message. §7: "A Clip Sequence Data shall include one End
    /// of Clip message as the last UMP message."
    #[error("clip file has no End of Clip message")]
    MissingEndOfClip,
    /// Bytes follow the End of Clip. §7.3: "A MIDI Clip File shall not have any
    /// data following the End of Clip message." There is no multi-clip clip
    /// file — that is what the MIDI Container File is for.
    #[error("clip file has data following the End of Clip message")]
    TrailingData,
}

fn parse_clip(bytes: &[u8]) -> Result<ParsedClipFile, ClipFileError> {
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
        let ev = crate::ump::packet_from(raw);

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
    // Declared, but unusable: every beat this file yields is a tick count
    // divided by this. Checking presence and not value let a zero through to
    // `timed()`, which returned infinities instead of failing here.
    if ticks_per_quarter == 0 {
        return Err(ClipFileError::ZeroDctpq);
    }
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

fn delta_clockstamp(ticks: u32) -> Ump {
    let mut m = DeltaClockstamp::<[u32; 1]>::new();
    m.set_time_data(u20::new(ticks & DCS_MAX));
    crate::ump::packet_from(m.data())
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
        push_words(out, delta_clockstamp(DCS_MAX).words());
        push_words(out, Ump::from(Message::Utility(Utility::Noop)).words());
        ticks -= DCS_MAX;
    }
    push_words(out, delta_clockstamp(ticks).words());
}

fn dctpq(ticks_per_quarter: u16) -> Ump {
    let mut m = DeltaClockstampTpq::<[u32; 1]>::new();
    m.set_time_data(ticks_per_quarter);
    crate::ump::packet_from(m.data())
}

fn start_of_clip() -> Ump {
    crate::ump::packet_from(StartOfClip::<[u32; 4]>::new().data())
}

fn end_of_clip() -> Ump {
    crate::ump::packet_from(EndOfClip::<[u32; 4]>::new().data())
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
        // `as_chunks` rather than `chunks_exact`: the length is already known to
        // be a multiple of 4 (checked above), so the remainder is empty and each
        // chunk arrives as a `[u8; 4]` that `from_be_bytes` takes directly —
        // no index-by-index rebuild, and no bounds checks to elide.
        let words = bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|c| u32::from_be_bytes(*c))
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
mod tests;
