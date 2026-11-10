//! Standard MIDI File reading and writing — the MIDI **1.0** half of this crate.
//!
//! Two read shapes over the same bytes: [`ParsedMidiFile`] is the flat, merged
//! event stream, and [`SmfFile::tracks`](crate::SmfFile::tracks) is the higher-level view that pairs note-on/off
//! into whole [`SmfNote`]s and keeps SMF tracks separate. An importer generally
//! wants the second.
//!
//! # Everything here is 7-bit
//!
//! An SMF cannot express MIDI 2.0 resolution. Key, velocity and controller
//! values are 7-bit (`U7`, `NoteNumber`, `CCNumber`) and pitch bend is 14-bit
//! (`U14`): a message is a [`Voice1`], the MIDI 1.0 channel-voice message of
//! `tutti-ump`, and no `midly` type is in this module's signatures. The
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
use midly::MidiMessage;
use midly::num::{u7, u14, u24};
use midly::{Format, Header, MetaMessage, Smf, Timing, Track, TrackEvent, TrackEventKind};
use tutti_types::{Beat, BeatDuration, Bpm};
use tutti_ump::{CCNumber, MidiChannel, NoteNumber, U7, U14, Voice1};

use std::path::Path;
use tracing::debug;

/// A MIDI voice event positioned in musical time (beats from file start).
#[derive(Debug, Clone, Copy)]
pub struct SmfTimedEvent {
    /// Absolute position from the start of the file.
    pub time_beats: Beat,
    /// The message's channel.
    pub channel: MidiChannel,
    /// The channel-voice message, MIDI 1.0 (7-bit) as the file holds it. A
    /// note-on at velocity 0 is kept as written: it is a note-off by the
    /// MIDI 1.0 rule, which [`SmfFile`](crate::SmfFile) and
    /// [`MidiFile::iter`](crate::MidiFile::iter) apply, not this stream.
    pub msg: Voice1,
}

/// A `midly` message as the `Voice1` it is. `midly`'s numbers are already
/// framed to 7 and 14 bits, so `masked` drops nothing.
fn voice_of(message: MidiMessage) -> Voice1 {
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

/// An SMF read as one flat event stream, merged across all tracks.
///
/// The low-level read shape. For per-track notes with durations, use [`SmfFile`](crate::SmfFile).
#[derive(Debug, Clone)]
pub struct ParsedMidiFile {
    /// Every channel-voice event in the file, merged across tracks and sorted by
    /// [`Beat`]. Meta events (tempo, track name) are not included — tempo is
    /// surfaced as `tempo_bpm` and `tempo_changes` instead.
    pub events: Vec<SmfTimedEvent>,
    /// The file's division: SMF ticks per quarter note, from the header. Never
    /// zero — a zero division is rejected at parse.
    pub ticks_per_beat: u16,
    /// The tempo at beat 0: a Set Tempo at tick 0 (the first in track order if
    /// several tracks set one there), otherwise the 120 BPM the SMF
    /// specification assumes until a Set Tempo says otherwise. A Set Tempo
    /// later in the file does not govern the beats before it; every change is
    /// in [`tempo_changes`](Self::tempo_changes).
    pub tempo_bpm: Bpm,
    /// Every Set Tempo in the file as `(position, tempo)`, merged across
    /// tracks in time order (ties in track order). A zero
    /// microseconds-per-quarter is skipped rather than read as an infinite
    /// tempo. Empty for a file that declares none.
    pub tempo_changes: Vec<(Beat, Bpm)>,
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
    /// [`Error::Io`] if the file cannot be read; otherwise as [`Self::from_bytes`].
    pub fn read(path: impl AsRef<Path>) -> Result<Self> {
        let data = std::fs::read(path.as_ref())?;
        Self::from_bytes(&data)
    }

    /// Parses SMF bytes into a merged, beat-positioned event stream.
    ///
    /// # Errors
    ///
    /// - [`Error::MidiUnsupportedTiming`] if the header declares SMPTE/timecode
    ///   division rather than metrical.
    /// - [`Error::MidiFileParse`] if the division is zero, or `midly` rejects the
    ///   bytes.
    pub fn from_bytes(data: &[u8]) -> Result<Self> {
        Self::from_smf(&Smf::parse(data)?)
    }

    /// [`from_bytes`](Self::from_bytes) over an SMF `midly` already parsed, so a reader
    /// that wants tracks too parses the bytes once.
    pub(crate) fn from_smf(smf: &Smf) -> Result<Self> {
        let ticks_per_beat = metrical_division(smf)?;

        debug!(
            "Parsing MIDI file: {} tracks, {} tpb",
            smf.tracks.len(),
            ticks_per_beat
        );

        let mut all_events = Vec::new();
        let mut changes: Vec<(u64, Bpm)> = Vec::new();
        for track in &smf.tracks {
            all_events.extend(parse_track(track, ticks_per_beat));
            changes.extend(track_tempos(track));
        }
        // Stable: two tracks setting a tempo on one tick keep track order.
        changes.sort_by_key(|&(tick, _)| tick);
        let tempo_bpm = match changes.first() {
            Some(&(0, tempo)) => tempo,
            // Nothing at tick 0: the spec's 120 governs until the first change.
            _ => Bpm::of(120.0),
        };
        debug!("Tempo at beat 0: {} BPM", tempo_bpm);
        let tempo_changes = changes
            .into_iter()
            .map(|(tick, tempo)| (Beat(tick as f64 / f64::from(ticks_per_beat)), tempo))
            .collect();

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
            tempo_changes,
            duration_beats,
        })
    }

    /// Events whose onset lands in `[start_beats, end_beats)` — start inclusive,
    /// end exclusive, so adjacent ranges tile without double-counting.
    ///
    /// Both bounds are [`Beat`] rather than `f64` because transposing them yields
    /// an empty slice rather than an error, which is silent at a call site.
    /// Binary search over the sorted `events`; no allocation.
    pub fn events_in_range(&self, start_beats: Beat, end_beats: Beat) -> &[SmfTimedEvent] {
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
                channel: MidiChannel::masked(channel.as_int()),
                msg: voice_of(*message),
            });
        }
    }

    events
}

/// Microseconds in a minute — the numerator of the SMF tempo relation, where
/// a Set Tempo meta event carries microseconds per quarter note.
///
/// Named rather than spelled as a literal because the same relation appears in
/// both directions here; the MIDI-2 twin is `flex::TEN_NS_UNITS_PER_MINUTE`.
const US_PER_MINUTE: f64 = 60_000_000.0;

/// Every Set Tempo in `track` as `(absolute tick, Bpm)`, in track order.
///
/// A zero microseconds-per-quarter is wire-representable and is skipped: it
/// divides to an infinity, which `Bpm::new` refuses, and an infinity here would
/// become `ParsedMidiFile::tempo_bpm` and reach every consumer of the parse.
/// The MIDI-2 inverse (`ten_ns_per_quarter_to_bpm`) skips the same condition.
/// Any other value is a tempo, however slow (about 0.014 BPM) or fast (60
/// million BPM at 1 us): the file's tempo is read as written, with no policy.
fn track_tempos(track: &Track) -> Vec<(u64, Bpm)> {
    let mut tick = 0u64;
    let mut out = Vec::new();
    for event in track.iter() {
        tick = tick.saturating_add(u64::from(event.delta.as_int()));
        if let TrackEventKind::Meta(MetaMessage::Tempo(t)) = &event.kind
            && let Some(bpm) = Bpm::new(US_PER_MINUTE / f64::from(t.as_int()))
        {
            out.push((tick, bpm));
        }
    }
    out
}

/// The file's division in ticks per quarter note.
///
/// # Errors
///
/// [`Error::MidiUnsupportedTiming`] for an SMPTE/timecode division, and
/// [`Error::MidiFileParse`] for a zero one: it is the divisor for every
/// `Beat` in the file, so letting one through returns a parse in which every
/// event sits at infinity.
fn metrical_division(smf: &Smf) -> Result<u16> {
    let ticks_per_beat = match smf.header.timing {
        Timing::Metrical(tpb) => tpb.as_int(),
        Timing::Timecode(_, _) => return Err(Error::MidiUnsupportedTiming),
    };
    if ticks_per_beat == 0 {
        return Err(Error::MidiFileParse("zero ticks-per-beat".into()));
    }
    Ok(ticks_per_beat)
}

/// Parsed times come from integer ticks, so they are always finite;
/// `total_cmp` is the total order regardless.
fn sort_by_time(events: &mut [SmfTimedEvent]) {
    events.sort_by(|a, b| a.time_beats.get().total_cmp(&b.time_beats.get()));
}

// --- Per-track paired notes ---
//
// A higher-level read view than [`ParsedMidiFile`]'s flat event stream:
// note-on/off are paired into whole notes, kept separated per SMF track, with
// each track's name. This is the shape an importer wants (one clip per track,
// notes with durations).
//
// The key and velocity are `NoteNumber` and `U7`, the range types of the MIDI
// 1.0 wire; the musical-time fields are a position and a span, `Beat` and
// `BeatDuration`, which as two bare `f64`s were transposable at every reader.

/// One note, paired from its NoteOn/NoteOff, in beats from track start.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SmfNote {
    /// Pairing is per-channel: a NoteOff only closes a NoteOn on the *same*
    /// channel, which multi-channel tracks (General MIDI Type-0) require.
    pub channel: MidiChannel,
    /// The key.
    pub key: NoteNumber,
    /// NoteOn velocity, 1..=127: a velocity-0 NoteOn is a NoteOff (MIDI 1.0
    /// Detailed Specification), so it never opens a note. `U7` holds 0, but
    /// no note here has it.
    pub velocity: U7,
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

/// Per-track paired notes of an SMF `midly` already parsed: one [`SmfTrack`] per
/// SMF track, note-on/off paired into [`SmfNote`]s, time in beats from the file
/// division. Tracks with no notes are still returned. The tempo map is not
/// applied; beats come from the division, matching [`ParsedMidiFile`].
///
/// # Errors
///
/// - [`Error::MidiUnsupportedTiming`] if the file uses SMPTE/timecode division.
/// - [`Error::MidiFileParse`] if the division is zero.
pub(crate) fn tracks_of(smf: &Smf) -> Result<Vec<SmfTrack>> {
    let ticks_per_beat = f64::from(metrical_division(smf)?);

    Ok(smf
        .tracks
        .iter()
        .map(|track| SmfTrack {
            name: track_name(track),
            notes: pair_notes(track, ticks_per_beat),
        })
        .collect())
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
    let mut held: BTreeMap<(MidiChannel, NoteNumber), Vec<(Beat, U7)>> = BTreeMap::new();
    let mut out: Vec<SmfNote> = Vec::new();

    let mut close = |held: &mut BTreeMap<(MidiChannel, NoteNumber), Vec<(Beat, U7)>>,
                     channel: MidiChannel,
                     key: NoteNumber,
                     end: Beat| {
        if let Some(stack) = held.get_mut(&(channel, key))
            && let Some((start, velocity)) = stack.pop()
        {
            out.push(SmfNote {
                channel,
                key,
                velocity,
                start_beats: start,
                duration_beats: (end - start).max(BeatDuration(0.0)),
            });
        }
    };

    for event in track.iter() {
        now_ticks = now_ticks.saturating_add(u64::from(event.delta.as_int()));
        let beat = Beat(now_ticks as f64 / ticks_per_beat);
        if let TrackEventKind::Midi { channel, message } = event.kind {
            let channel = MidiChannel::masked(channel.as_int());
            match voice_of(message) {
                Voice1::NoteOn { note, velocity } => {
                    if velocity.get() == 0 {
                        close(&mut held, channel, note, beat);
                    } else {
                        held.entry((channel, note))
                            .or_default()
                            .push((beat, velocity));
                    }
                }
                Voice1::NoteOff { note, .. } => close(&mut held, channel, note, beat),
                _ => {}
            }
        }
    }

    // Onset order, then (channel, key) so simultaneous notes have a stable
    // order rather than one that depends on NoteOff arrival.
    out.sort_by(|a, b| {
        a.start_beats
            .get()
            .total_cmp(&b.start_beats.get())
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
pub struct MidiWriteConfig {
    /// The header's division — SMF ticks per quarter note. Defaults to 480. Sets
    /// the write resolution: an event's tick is `beat * ticks_per_beat` rounded
    /// to nearest, so a coarse value quantises onsets — to the closest tick,
    /// never systematically early.
    pub ticks_per_beat: u16,
    /// Tempo for the Set Tempo meta event on the first track. `None` writes no
    /// tempo, leaving a reader to assume the SMF default of 120.
    ///
    /// The wire holds 1 to 16 777 215 us per quarter (24 bits): a tempo above
    /// 60 million BPM writes the shortest, 1 (the wire's zero means "no valid
    /// tempo"), and one below about 3.6 BPM the longest.
    pub tempo_bpm: Option<Bpm>,
    /// Time signature as `(numerator, denominator_power_of_two)` — `(4, 2)` is
    /// 4/4, `(6, 3)` is 6/8 — for the first track's meta event. This is the SMF
    /// wire encoding, not a fraction.
    pub time_signature: Option<(u8, u8)>,
}

impl Default for MidiWriteConfig {
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
pub fn write_smf(
    path: impl AsRef<Path>,
    tracks: &[Vec<SmfTimedEvent>],
    config: &MidiWriteConfig,
) -> Result<()> {
    let data = smf_to_bytes(tracks, config)?;
    std::fs::write(path, data)?;
    Ok(())
}

/// Encodes `tracks` to SMF bytes in memory. The in-memory half of
/// [`write_smf`], with the same format rules.
///
/// # Errors
///
/// [`Error::InvalidConfig`] if `tracks` is empty — an SMF must carry at least one
/// track chunk — or if an event's time is not finite: a NaN or infinite beat
/// has no tick to land on. [`Error::MidiFileParse`] if `midly` refuses to
/// serialise.
pub fn smf_to_bytes(tracks: &[Vec<SmfTimedEvent>], config: &MidiWriteConfig) -> Result<Vec<u8>> {
    if tracks.is_empty() {
        return Err(Error::InvalidConfig("No tracks provided".into()));
    }
    for (i, track) in tracks.iter().enumerate() {
        if let Some(e) = track.iter().find(|e| !e.time_beats.get().is_finite()) {
            return Err(Error::InvalidConfig(format!(
                "track {i}: an event at a non-finite beat ({})",
                e.time_beats.get()
            )));
        }
    }

    let format = if tracks.len() == 1 {
        Format::SingleTrack
    } else {
        Format::Parallel
    };
    let header = Header::new(format, Timing::Metrical(config.ticks_per_beat.into()));
    let mut smf = Smf::new(header);

    for (i, track_events) in tracks.iter().enumerate() {
        smf.tracks.push(build_track(track_events, config, i == 0));
    }

    let mut buf = Vec::new();
    smf.write_std(&mut buf)
        .map_err(|e| Error::MidiFileParse(format!("Failed to encode MIDI: {}", e)))?;
    Ok(buf)
}

fn build_track<'a>(
    events: &[SmfTimedEvent],
    config: &MidiWriteConfig,
    include_meta: bool,
) -> Vec<TrackEvent<'a>> {
    let tpb = f64::from(config.ticks_per_beat);
    let mut track: Vec<TrackEvent<'a>> = Vec::new();

    if include_meta {
        if let Some(bpm) = config.tempo_bpm {
            // Converting to the 24-bit field masks, so a slow tempo would wrap
            // to a faster one, and one above 60 million BPM would cast to 0,
            // the wire's "no valid tempo".
            let us = ((US_PER_MINUTE / bpm.get()) as u32).clamp(1, u24::max_value().as_int());
            track.push(meta_event(MetaMessage::Tempo(us.into())));
        }
        if let Some((num, denom_pow)) = config.time_signature {
            track.push(meta_event(MetaMessage::TimeSignature(
                num, denom_pow, 24, 8,
            )));
        }
    }

    let mut sorted: Vec<&SmfTimedEvent> = events.iter().collect();
    sort_by_time_ref(&mut sorted);

    let mut last_tick: u32 = 0;
    for event in &sorted {
        // Rounded to nearest, not truncated. The engine's convention for a
        // *position* conversion is round-to-nearest (`Seconds::to_samples`,
        // whose floor/ceil siblings are separately named precisely so the bare
        // form can mean "nearest"), and a tick is a position.
        //
        // Truncation was not merely half a tick less accurate — it was biased
        // one direction, which is what made it a bug rather than a rounding
        // choice. A beat reached by repeated addition lands just *below* its
        // exact value about half the time (`1/3 * 6` is 1.9999999999999998),
        // and truncation turns every one of those into a tick one early: beat
        // 2.0 written as 959 instead of 960. Over a triplet figure that is 18
        // early onsets in 39. Rounding absorbs the representation error the
        // whole way, since a musical subdivision is exact on a 480-tick grid
        // and the float noise is many orders of magnitude below half a tick.
        //
        // Ties (an exact `.5`, reachable only from a beat genuinely off-grid)
        // round away from zero, `f64::round`'s rule. Which way a tie goes is
        // not musically meaningful — a value landing exactly between two ticks
        // is already past the file's resolution — but it is deterministic, so
        // the same input always writes the same bytes.
        //
        // `f64::round` before the cast, so the saturating cast still clamps a
        // beat beyond `u32::MAX` ticks rather than wrapping.
        let abs_tick = (event.time_beats.get() * tpb).round() as u32;
        let delta = abs_tick.saturating_sub(last_tick);
        last_tick = abs_tick;

        track.push(TrackEvent {
            delta: delta.into(),
            kind: TrackEventKind::Midi {
                channel: event.channel.get().into(),
                message: midly_of(event.msg),
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

/// `to_bytes` has rejected every non-finite time before this runs;
/// `total_cmp` keeps the sort a total order (no panic) regardless.
fn sort_by_time_ref(events: &mut [&SmfTimedEvent]) {
    events.sort_by(|a, b| a.time_beats.get().total_cmp(&b.time_beats.get()));
}

#[cfg(test)]
mod tests;
