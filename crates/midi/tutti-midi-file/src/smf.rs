//! Standard MIDI File reading and writing — the MIDI **1.0** half of this crate.
//!
//! Two read shapes over the same bytes: [`ParsedMidiFile`] is the flat, merged
//! event stream, and [`tracks`] is the higher-level view that pairs note-on/off
//! into whole [`SmfNote`]s and keeps SMF tracks separate. An importer generally
//! wants the second.
//!
//! # Everything here is 7-bit
//!
//! An SMF cannot express MIDI 2.0 resolution. Key, velocity and controller
//! values are `u8` in 0..=127 and pitch bend is 14-bit — they are protocol
//! integers on the wire, not measurements, and stay `u8` for that reason. The
//! MIDI 2.0 side of the crate ([`crate::clip`], 16-bit velocity and 32-bit
//! controllers) is a *different format*, not a wider encoding of this one.
//!
//! Musical time is the exception, deliberately: positions are [`Beat`] and spans
//! are [`BeatDuration`], because a position and a span are transposable at a call
//! site when both are bare `f64`.
//!
//! # Metrical timing only
//!
//! Beats come from the file's division. An SMPTE/timecode-divided file has no
//! beat grid to read them from and returns [`Error::MidiUnsupportedTiming`]
//! rather than a guess. The tempo map is not applied either — beat positions are
//! as the file states them, and `tempo_bpm` is reported separately for a caller
//! that wants to convert.

use crate::{Error, Result};
// NOTE: `midly::MidiMessage` is the *MIDI 1.0 7-bit* SMF message — a different
// type from `tutti_midi_hardware::MidiMessage` (the decoded MIDI-2 view). It is
// imported under the alias `SmfMessage` so this SMF-1.0 codec never shadows the
// engine's `MidiMessage` in a `use tutti_midi_hardware::*` context.
use midly::{Format, Header, MetaMessage, Smf, Timing, Track, TrackEvent, TrackEventKind};
use tutti_core::{Beat, BeatDuration};

/// The MIDI 1.0 7-bit channel-voice message carried by [`SmfTimedEvent`] — a
/// re-export of `midly::MidiMessage`, aliased so an SMF caller never confuses it
/// with the engine's MIDI-2 `MidiMessage` in `tutti_midi_hardware`.
pub use midly::MidiMessage as SmfMessage;
use std::path::Path;
use tracing::debug;

/// A MIDI voice event positioned in musical time (beats from file start).
#[derive(Debug, Clone, Copy)]
pub struct SmfTimedEvent {
    /// Absolute position from the start of the file.
    pub time_beats: Beat,
    /// MIDI channel (0-15).
    pub channel: u8,
    /// The channel-voice message. This is a **MIDI 1.0** `midly` message
    /// (re-exported as [`SmfMessage`]), not the engine's MIDI-2 `MidiMessage`.
    pub msg: SmfMessage,
}

/// An SMF read as one flat event stream, merged across all tracks.
///
/// The low-level read shape. For per-track notes with durations, use [`tracks`].
#[derive(Debug, Clone)]
pub struct ParsedMidiFile {
    /// Every channel-voice event in the file, merged across tracks and sorted by
    /// [`Beat`]. Meta events (tempo, track name) are not included — tempo is
    /// surfaced as `tempo_bpm` instead.
    pub events: Vec<SmfTimedEvent>,
    /// The file's division: SMF ticks per quarter note, from the header. Never
    /// zero — a zero division is rejected at parse.
    pub ticks_per_beat: u16,
    /// Default tempo in BPM (from first tempo event, or 120 if none).
    pub tempo_bpm: f64,
    /// Where the last event lands, measured from the start of the file.
    ///
    /// A span rather than a position, because that is how every caller reads
    /// it — "how long is this file" — even though it is derived from the final
    /// event's [`Beat`]. The subtraction from the origin says so.
    pub duration_beats: BeatDuration,
}

impl ParsedMidiFile {
    /// Reads and parses the SMF at `path`.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] if the file cannot be read; otherwise as [`Self::parse`].
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let data = std::fs::read(path.as_ref())?;
        Self::parse(&data)
    }

    /// Parses SMF bytes into a merged, beat-positioned event stream.
    ///
    /// # Errors
    ///
    /// - [`Error::MidiUnsupportedTiming`] if the header declares SMPTE/timecode
    ///   division rather than metrical.
    /// - [`Error::MidiFileParse`] if the division is zero, or `midly` rejects the
    ///   bytes.
    pub fn parse(data: &[u8]) -> Result<Self> {
        let smf = Smf::parse(data)?;

        let ticks_per_beat = match smf.header.timing {
            Timing::Metrical(tpb) => tpb.as_int(),
            Timing::Timecode(_, _) => return Err(Error::MidiUnsupportedTiming),
        };
        // Both readers of this field reject a zero: it is the divisor for every
        // `time_beats` in the file, so letting one through returns a parse in
        // which every event sits at infinity. `tracks()` makes the same check.
        if ticks_per_beat == 0 {
            return Err(Error::MidiFileParse("zero ticks-per-beat".into()));
        }

        debug!(
            "Parsing MIDI file: {} tracks, {} tpb",
            smf.tracks.len(),
            ticks_per_beat
        );

        let mut all_events = Vec::new();
        let mut tempo_bpm = 120.0;
        let mut found_tempo = false;

        for track in &smf.tracks {
            all_events.extend(parse_track(track, ticks_per_beat));
            if !found_tempo {
                if let Some(t) = extract_tempo(track) {
                    tempo_bpm = t;
                    found_tempo = true;
                    debug!("Found tempo: {} BPM", tempo_bpm);
                }
            }
        }

        sort_by_time(&mut all_events);
        let duration_beats = all_events
            .last()
            .map(|e| e.time_beats - Beat(0.0))
            .unwrap_or(BeatDuration(0.0));
        debug!(
            "Parsed {} events, {:.2} beats",
            all_events.len(),
            duration_beats.get()
        );

        Ok(Self {
            events: all_events,
            ticks_per_beat,
            tempo_bpm,
            duration_beats,
        })
    }

    /// Events whose onset lands in `[start_beats, end_beats)` — start inclusive,
    /// end exclusive, so adjacent ranges tile without double-counting.
    ///
    /// Both bounds are [`Beat`] rather than `f64` because transposing them yields
    /// an empty slice rather than an error, which is silent at a call site.
    /// Binary search over the sorted `events`; no allocation.
    pub fn get_events_in_range(&self, start_beats: Beat, end_beats: Beat) -> &[SmfTimedEvent] {
        let start = self.events.partition_point(|e| e.time_beats < start_beats);
        let end = self.events[start..].partition_point(|e| e.time_beats < end_beats) + start;
        &self.events[start..end]
    }
}

// --- Parsing helpers ---

fn parse_track(track: &Track, ticks_per_beat: u16) -> Vec<SmfTimedEvent> {
    let mut events = Vec::new();
    let mut tick = 0u64;

    for event in track.iter() {
        tick += u64::from(event.delta.as_int());
        if let TrackEventKind::Midi { channel, message } = &event.kind {
            events.push(SmfTimedEvent {
                time_beats: Beat(tick as f64 / f64::from(ticks_per_beat)),
                channel: channel.as_int(),
                msg: *message,
            });
        }
    }

    events
}

/// Microseconds in a minute — the numerator of the SMF tempo relation, where
/// a Set Tempo meta event carries microseconds per quarter note.
///
/// Named rather than spelled as a literal because the same relation appears in
/// both directions here; the MIDI-2 twin is
/// `ump::flex_data::TEN_NS_UNITS_PER_MINUTE`.
const US_PER_MINUTE: f64 = 60_000_000.0;

/// The first Set Tempo in `track` as BPM, or `None` if it declares none.
///
/// A zero microseconds-per-quarter is wire-representable and yields `None` rather
/// than an infinite BPM — an infinity here would become
/// `ParsedMidiFile::tempo_bpm` and reach every consumer of the parse. The MIDI-2
/// inverse (`ten_ns_per_quarter_to_bpm`) guards the identical condition.
///
/// Stays `f64`: BPM is a rate a caller may convert against long durations, and
/// `Bpm` is f32.
fn extract_tempo(track: &Track) -> Option<f64> {
    track.iter().find_map(|e| match &e.kind {
        TrackEventKind::Meta(MetaMessage::Tempo(t)) => match t.as_int() {
            0 => None,
            us_per_quarter => Some(US_PER_MINUTE / f64::from(us_per_quarter)),
        },
        _ => None,
    })
}

fn sort_by_time(events: &mut [SmfTimedEvent]) {
    events.sort_by(|a, b| a.time_beats.partial_cmp(&b.time_beats).unwrap());
}

// --- Per-track paired notes ---
//
// A higher-level read view than [`ParsedMidiFile`]'s flat event stream:
// note-on/off are paired into whole notes, kept separated per SMF track, with
// each track's name. This is the shape an importer wants (one clip per track,
// notes with durations).
//
// The MIDI-wire fields stay `u8` — a channel nibble and a 7-bit key are
// protocol integers, not measurements. The musical-time fields do not: they are
// a position and a span, and as two bare `f64`s they were transposable at every
// reader. `tutti-sampler` already spells the same pair `Beat`/`BeatDuration`.

/// One note, paired from its NoteOn/NoteOff, in beats from track start.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SmfNote {
    /// MIDI channel, 0..=15. Pairing is per-channel: a NoteOff only closes a
    /// NoteOn on the *same* channel, which is what multi-channel tracks
    /// (the norm for General MIDI Type-0 files) require.
    pub channel: u8,
    /// MIDI key number, 0..=127.
    pub key: u8,
    /// NoteOn velocity, 1..=127 (velocity-0 NoteOn is treated as NoteOff).
    pub velocity: u8,
    /// Onset, relative to the start of the file.
    pub start_beats: Beat,
    /// Length (NoteOff beat − NoteOn beat, clamped to ≥ 0).
    ///
    /// A [`BeatDuration`] beside a [`Beat`]: a span and a position, which is
    /// what makes the pair un-transposable now. Subtracting the two onsets
    /// yields exactly this type, so the clamp below reads as the `max` of a
    /// span rather than of a float that happens to be one.
    pub duration_beats: BeatDuration,
}

/// One SMF track's paired notes plus its track-name meta event (if any).
#[derive(Debug, Clone, Default)]
pub struct SmfTrack {
    /// The track's `TrackName` meta event, trimmed; `None` if it declares none or
    /// declares an empty one.
    pub name: Option<String>,
    /// Paired notes in onset order, then by `(channel, key)` so simultaneous
    /// notes have a stable order rather than one set by NoteOff arrival.
    pub notes: Vec<SmfNote>,
}

/// Parse an SMF into per-track paired notes (one [`SmfTrack`] per SMF track,
/// note-on/off paired into [`SmfNote`]s, time in beats from the file division).
///
/// Tracks with no notes are still returned (callers skip empties as they see
/// fit). Only metrical (ticks-per-beat) timing is supported — SMPTE-timed files
/// return [`Error::MidiUnsupportedTiming`]. The tempo map is not applied; beats
/// come from the division, matching [`ParsedMidiFile`].
pub fn tracks(data: &[u8]) -> Result<Vec<SmfTrack>> {
    let smf = Smf::parse(data)?;
    let ticks_per_beat = match smf.header.timing {
        Timing::Metrical(tpb) => f64::from(tpb.as_int()),
        Timing::Timecode(_, _) => return Err(Error::MidiUnsupportedTiming),
    };
    if ticks_per_beat == 0.0 {
        return Err(Error::MidiFileParse("zero ticks-per-beat".into()));
    }

    Ok(smf
        .tracks
        .iter()
        .map(|track| SmfTrack {
            name: track_name(track),
            notes: pair_notes(track, ticks_per_beat),
        })
        .collect())
}

/// Parse an SMF file at `path` into per-track paired notes. See [`tracks`].
pub fn tracks_from_path(path: impl AsRef<Path>) -> Result<Vec<SmfTrack>> {
    tracks(&std::fs::read(path.as_ref())?)
}

/// Pair NoteOn / NoteOff events in one track into whole notes. Velocity-0
/// NoteOn is treated as NoteOff; pairing is keyed on `(channel, key)`, so a
/// NoteOff only closes a NoteOn on its own channel; overlapping notes on the
/// same channel+key close in LIFO order; notes left open at end-of-track are
/// dropped.
fn pair_notes(track: &Track, ticks_per_beat: f64) -> Vec<SmfNote> {
    use std::collections::BTreeMap;

    let mut now_ticks: u64 = 0;
    // The held onset is a `Beat`, so `end - start` below *is* a `BeatDuration`
    // rather than a float that has to be trusted to be one.
    let mut held: BTreeMap<(u8, u8), Vec<(Beat, u8)>> = BTreeMap::new();
    let mut out: Vec<SmfNote> = Vec::new();

    let mut close =
        |held: &mut BTreeMap<(u8, u8), Vec<(Beat, u8)>>, channel: u8, key: u8, end: Beat| {
            if let Some(stack) = held.get_mut(&(channel, key)) {
                if let Some((start, velocity)) = stack.pop() {
                    out.push(SmfNote {
                        channel,
                        key,
                        velocity,
                        start_beats: start,
                        duration_beats: (end - start).max(BeatDuration(0.0)),
                    });
                }
            }
        };

    for event in track.iter() {
        now_ticks = now_ticks.saturating_add(u64::from(event.delta.as_int()));
        let beat = Beat(now_ticks as f64 / ticks_per_beat);
        if let TrackEventKind::Midi { channel, message } = event.kind {
            let channel = channel.as_int();
            match message {
                SmfMessage::NoteOn { key, vel } => {
                    let (key, vel) = (key.as_int(), vel.as_int());
                    if vel == 0 {
                        close(&mut held, channel, key, beat);
                    } else {
                        held.entry((channel, key)).or_default().push((beat, vel));
                    }
                }
                SmfMessage::NoteOff { key, .. } => close(&mut held, channel, key.as_int(), beat),
                _ => {}
            }
        }
    }

    // Onset order, then (channel, key) so simultaneous notes have a stable
    // order rather than one that depends on NoteOff arrival.
    out.sort_by(|a, b| {
        a.start_beats
            .partial_cmp(&b.start_beats)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| (a.channel, a.key).cmp(&(b.channel, b.key)))
    });
    out
}

/// First non-empty `TrackName` meta event in a track, trimmed.
fn track_name(track: &Track) -> Option<String> {
    track.iter().find_map(|e| match e.kind {
        TrackEventKind::Meta(MetaMessage::TrackName(bytes)) => std::str::from_utf8(bytes)
            .ok()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        _ => None,
    })
}

// --- Writing ---

/// How to encode an SMF: division, plus the optional meta events for track 0.
#[derive(Debug, Clone)]
pub struct MidiWriteOptions {
    /// The header's division — SMF ticks per quarter note. Defaults to 480. Sets
    /// the write resolution: an event's tick is `beat * ticks_per_beat`
    /// truncated, so a coarse value quantises onsets.
    pub ticks_per_beat: u16,
    /// Tempo for the Set Tempo meta event on the first track. `None` writes no
    /// tempo, leaving a reader to assume the SMF default of 120.
    ///
    /// `f64` rather than `Bpm`, matching what the parse returns. A non-positive
    /// value writes the wire's own "no valid tempo" zero.
    pub tempo_bpm: Option<f64>,
    /// Time signature as `(numerator, denominator_power_of_two)` — `(4, 2)` is
    /// 4/4, `(6, 3)` is 6/8 — for the first track's meta event. This is the SMF
    /// wire encoding, not a fraction.
    pub time_signature: Option<(u8, u8)>,
}

impl Default for MidiWriteOptions {
    fn default() -> Self {
        Self {
            ticks_per_beat: 480,
            tempo_bpm: None,
            time_signature: None,
        }
    }
}

/// Encodes `tracks` and writes the result to `path`.
///
/// A single track produces a format 0 file; multiple tracks produce format 1.
/// Only the first track carries the tempo and time-signature meta events.
///
/// # Errors
///
/// [`Error::InvalidConfig`] if `tracks` is empty, or [`Error::Io`] if the write
/// fails.
pub fn write_midi_file(
    path: impl AsRef<Path>,
    tracks: &[Vec<SmfTimedEvent>],
    options: &MidiWriteOptions,
) -> Result<()> {
    let data = encode_midi_file(tracks, options)?;
    std::fs::write(path, data)?;
    Ok(())
}

/// Encodes `tracks` to SMF bytes in memory. The in-memory half of
/// [`write_midi_file`], with the same format rules.
///
/// # Errors
///
/// [`Error::InvalidConfig`] if `tracks` is empty — an SMF must carry at least one
/// track chunk. [`Error::MidiFileParse`] if `midly` refuses to serialise.
pub fn encode_midi_file(
    tracks: &[Vec<SmfTimedEvent>],
    options: &MidiWriteOptions,
) -> Result<Vec<u8>> {
    if tracks.is_empty() {
        return Err(Error::InvalidConfig("No tracks provided".into()));
    }

    let format = if tracks.len() == 1 {
        Format::SingleTrack
    } else {
        Format::Parallel
    };
    let header = Header::new(format, Timing::Metrical(options.ticks_per_beat.into()));
    let mut smf = Smf::new(header);

    for (i, track_events) in tracks.iter().enumerate() {
        smf.tracks.push(build_track(track_events, options, i == 0));
    }

    let mut buf = Vec::new();
    smf.write_std(&mut buf)
        .map_err(|e| Error::MidiFileParse(format!("Failed to encode MIDI: {}", e)))?;
    Ok(buf)
}

fn build_track<'a>(
    events: &[SmfTimedEvent],
    options: &MidiWriteOptions,
    include_meta: bool,
) -> Vec<TrackEvent<'a>> {
    let tpb = f64::from(options.ticks_per_beat);
    let mut track: Vec<TrackEvent<'a>> = Vec::new();

    if include_meta {
        if let Some(bpm) = options.tempo_bpm {
            // Guarded like its MIDI-2 twin `bpm_to_ten_ns_per_quarter`: a
            // non-positive BPM divided to `inf`, and the saturating cast wrote
            // `u32::MAX` microseconds per quarter — about 71 minutes a beat —
            // into the file. Zero is the wire's own "no valid tempo".
            let us = if bpm > 0.0 {
                (US_PER_MINUTE / bpm) as u32
            } else {
                0
            };
            track.push(meta_event(MetaMessage::Tempo(us.into())));
        }
        if let Some((num, denom_pow)) = options.time_signature {
            track.push(meta_event(MetaMessage::TimeSignature(
                num, denom_pow, 24, 8,
            )));
        }
    }

    let mut sorted: Vec<&SmfTimedEvent> = events.iter().collect();
    sort_by_time_ref(&mut sorted);

    let mut last_tick: u32 = 0;
    for event in &sorted {
        let abs_tick = (event.time_beats.get() * tpb) as u32;
        let delta = abs_tick.saturating_sub(last_tick);
        last_tick = abs_tick;

        track.push(TrackEvent {
            delta: delta.into(),
            kind: TrackEventKind::Midi {
                channel: (event.channel & 0x0F).into(),
                message: event.msg,
            },
        });
    }

    track.push(meta_event(MetaMessage::EndOfTrack));
    track
}

fn meta_event(msg: MetaMessage<'static>) -> TrackEvent<'static> {
    TrackEvent {
        delta: 0.into(),
        kind: TrackEventKind::Meta(msg),
    }
}

fn sort_by_time_ref(events: &mut [&SmfTimedEvent]) {
    events.sort_by(|a, b| a.time_beats.partial_cmp(&b.time_beats).unwrap());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_empty_midi() {
        let data = [
            0x4D, 0x54, 0x68, 0x64, 0x00, 0x00, 0x00, 0x06, 0x00, 0x00, 0x00, 0x01, 0x01, 0xE0,
            0x4D, 0x54, 0x72, 0x6B, 0x00, 0x00, 0x00, 0x04, 0x00, 0xFF, 0x2F, 0x00,
        ];

        let file = ParsedMidiFile::parse(&data).unwrap();
        assert_eq!(file.ticks_per_beat, 480);
        assert_eq!(file.events.len(), 0);
    }

    /// Neither direction of the tempo relation may divide by zero.
    ///
    /// A `Tempo(0)` meta event is malformed but wire-representable, and gave an
    /// infinite BPM that became `ParsedMidiFile::tempo_bpm`. Writing a
    /// non-positive BPM did the mirror: `inf` microseconds, saturating to
    /// `u32::MAX` — about 71 minutes per beat — in the file. The MIDI-2 twin
    /// has always guarded both.
    #[test]
    fn a_degenerate_tempo_is_absent_rather_than_infinite() {
        // Header (division 480) + a track holding only Set Tempo = 0.
        let data = [
            0x4D, 0x54, 0x68, 0x64, 0x00, 0x00, 0x00, 0x06, 0x00, 0x00, 0x00, 0x01, 0x01, 0xE0,
            0x4D, 0x54, 0x72, 0x6B, 0x00, 0x00, 0x00, 0x0B, 0x00, 0xFF, 0x51, 0x03, 0x00, 0x00,
            0x00, 0x00, 0xFF, 0x2F, 0x00,
        ];
        let file = ParsedMidiFile::parse(&data).expect("a zero tempo is not a parse failure");
        assert!(
            file.tempo_bpm.is_finite(),
            "zero microseconds-per-quarter must not become an infinite BPM"
        );
        // Falls back to the documented default rather than inventing a rate.
        assert_eq!(file.tempo_bpm, 120.0);

        // And the write direction: a non-positive BPM writes the wire's own
        // "no tempo" zero rather than a saturated u32.
        let opts = MidiWriteOptions {
            ticks_per_beat: 480,
            tempo_bpm: Some(0.0),
            time_signature: None,
        };
        let track = vec![vec![SmfTimedEvent {
            time_beats: Beat(0.0),
            channel: 0,
            msg: SmfMessage::NoteOn {
                key: 60.into(),
                vel: 100.into(),
            },
        }]];
        let bytes = encode_midi_file(&track, &opts).expect("encodes");
        let reparsed = ParsedMidiFile::parse(&bytes).expect("round-trips");
        assert!(reparsed.tempo_bpm.is_finite());
    }

    /// Both readers of the header's division field must reject a zero.
    ///
    /// `tracks()` always did; `parse()` did not, on the same field of the same
    /// file — so which entry point you called decided whether a malformed file
    /// was an error or a set of infinite event times. The two bytes at offset
    /// 12..14 are the division, here zeroed.
    #[test]
    fn either_reader_rejects_a_zero_division() {
        let mut data = [
            0x4D, 0x54, 0x68, 0x64, 0x00, 0x00, 0x00, 0x06, 0x00, 0x00, 0x00, 0x01, 0x01, 0xE0,
            0x4D, 0x54, 0x72, 0x6B, 0x00, 0x00, 0x00, 0x04, 0x00, 0xFF, 0x2F, 0x00,
        ];
        // Sanity: this fixture parses on both paths before the division is
        // zeroed, so the assertions below are about the zero and nothing else.
        assert!(ParsedMidiFile::parse(&data).is_ok());
        assert!(tracks(&data).is_ok());

        data[12] = 0x00;
        data[13] = 0x00;
        assert!(matches!(
            ParsedMidiFile::parse(&data),
            Err(Error::MidiFileParse(_))
        ));
        assert!(matches!(tracks(&data), Err(Error::MidiFileParse(_))));
    }

    #[test]
    fn test_write_and_read_roundtrip() {
        let events = vec![
            SmfTimedEvent {
                time_beats: Beat(0.0),
                channel: 0,
                msg: SmfMessage::NoteOn {
                    key: 60.into(),
                    vel: 100.into(),
                },
            },
            SmfTimedEvent {
                time_beats: Beat(1.0),
                channel: 0,
                msg: SmfMessage::NoteOff {
                    key: 60.into(),
                    vel: 0.into(),
                },
            },
        ];

        let options = MidiWriteOptions {
            ticks_per_beat: 480,
            tempo_bpm: Some(120.0),
            time_signature: Some((4, 2)),
        };

        let data = encode_midi_file(&[events], &options).unwrap();
        let parsed = ParsedMidiFile::parse(&data).unwrap();

        assert_eq!(parsed.ticks_per_beat, 480);
        assert!((parsed.tempo_bpm - 120.0).abs() < 0.1);
        assert_eq!(parsed.events.len(), 2);
        assert!((parsed.events[0].time_beats - Beat(0.0)).abs() < BeatDuration(0.001));
        assert!((parsed.events[1].time_beats - Beat(1.0)).abs() < BeatDuration(0.001));
    }

    #[test]
    fn test_write_empty_tracks_error() {
        let result = encode_midi_file(&[], &MidiWriteOptions::default());
        assert!(result.is_err());
    }

    #[test]
    fn tracks_pairs_note_on_off_with_duration() {
        // One note: on at beat 0, off at beat 1, on a named track.
        let events = vec![
            SmfTimedEvent {
                time_beats: Beat(0.0),
                channel: 0,
                msg: SmfMessage::NoteOn {
                    key: 60.into(),
                    vel: 100.into(),
                },
            },
            SmfTimedEvent {
                time_beats: Beat(1.0),
                channel: 0,
                msg: SmfMessage::NoteOff {
                    key: 60.into(),
                    vel: 0.into(),
                },
            },
        ];
        let data = encode_midi_file(
            &[events],
            &MidiWriteOptions {
                ticks_per_beat: 480,
                ..Default::default()
            },
        )
        .unwrap();

        let parsed = tracks(&data).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].notes.len(), 1);
        let n = parsed[0].notes[0];
        assert_eq!(n.key, 60);
        assert_eq!(n.velocity, 100);
        assert!((n.start_beats - Beat(0.0)).abs() < BeatDuration(1e-6));
        assert!((n.duration_beats - BeatDuration(1.0)).abs() < BeatDuration(1e-3));
    }

    #[test]
    fn tracks_pair_notes_per_channel() {
        // Same key on two channels, overlapping. Channel 0's note runs 0..4;
        // channel 1's runs 1..2. Pairing on key alone would let channel 1's
        // NoteOff at beat 2 close channel 0's note (LIFO), yielding durations
        // 1 (ch1: 1..2 closed by the ch0 off at 4 → no, LIFO gives 1..2→1)
        // and 2 — i.e. the wrong note gets the wrong length.
        let note = |time_beats: f64, channel: u8, on: bool| SmfTimedEvent {
            time_beats: Beat(time_beats),
            channel,
            msg: if on {
                SmfMessage::NoteOn {
                    key: 60.into(),
                    vel: 100.into(),
                }
            } else {
                SmfMessage::NoteOff {
                    key: 60.into(),
                    vel: 0.into(),
                }
            },
        };
        let events = vec![
            note(0.0, 0, true),
            note(1.0, 1, true),
            note(2.0, 1, false),
            note(4.0, 0, false),
        ];
        let data = encode_midi_file(
            &[events],
            &MidiWriteOptions {
                ticks_per_beat: 480,
                ..Default::default()
            },
        )
        .unwrap();

        let parsed = tracks(&data).unwrap();
        assert_eq!(parsed[0].notes.len(), 2);

        let ch0 = parsed[0]
            .notes
            .iter()
            .find(|n| n.channel == 0)
            .expect("channel 0 note");
        let ch1 = parsed[0]
            .notes
            .iter()
            .find(|n| n.channel == 1)
            .expect("channel 1 note");

        assert!(
            (ch0.start_beats - Beat(0.0)).abs() < BeatDuration(1e-3)
                && (ch0.duration_beats - BeatDuration(4.0)).abs() < BeatDuration(1e-3),
            "ch0 note should be closed by its own NoteOff at beat 4, got {ch0:?}"
        );
        assert!(
            (ch1.start_beats - Beat(1.0)).abs() < BeatDuration(1e-3)
                && (ch1.duration_beats - BeatDuration(1.0)).abs() < BeatDuration(1e-3),
            "ch1 note should be closed by its own NoteOff at beat 2, got {ch1:?}"
        );
    }

    #[test]
    fn tracks_treats_velocity_zero_note_on_as_off() {
        let events = vec![
            SmfTimedEvent {
                time_beats: Beat(0.0),
                channel: 0,
                msg: SmfMessage::NoteOn {
                    key: 64.into(),
                    vel: 80.into(),
                },
            },
            SmfTimedEvent {
                time_beats: Beat(2.0),
                channel: 0,
                // Running-status note-off: NoteOn with velocity 0.
                msg: SmfMessage::NoteOn {
                    key: 64.into(),
                    vel: 0.into(),
                },
            },
        ];
        let data = encode_midi_file(
            &[events],
            &MidiWriteOptions {
                ticks_per_beat: 480,
                ..Default::default()
            },
        )
        .unwrap();

        let parsed = tracks(&data).unwrap();
        assert_eq!(
            parsed[0].notes.len(),
            1,
            "vel-0 NoteOn should close the note"
        );
        assert!((parsed[0].notes[0].duration_beats - BeatDuration(2.0)).abs() < BeatDuration(1e-3));
    }
}
