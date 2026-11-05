//! Monophonic audio to notes.
//!
//! A YIN pitch track ([`yin_track`]) and the onsets ([`detect_onsets`]) are cut
//! into notes: each has a [`Note`], a bend in [`Cents`], a span in [`Samples`]
//! and a peak level. Mapping that to a MIDI event is the caller's: the crate
//! holds no MIDI types, and [`VelocityRange`] is the one documented way to a
//! 1..=127 velocity.
//!
//! # Time
//!
//! YIN compares the first half of its [`YinConfig::buffer_size`] with a lagged
//! copy, so a frame starting at `i * hop` vouches for
//! `i * hop .. i * hop + buffer_size / 2`. A note spans the frames it holds:
//! from its first frame's start to its last frame's start plus that half.
//! Silence matches its own lag, so a frame mostly past a release stays voiced
//! and an end runs late by up to the half frame; a start is within a hop and a period (see
//! [`NoteConfig::timing_tolerance`]). Where one note hands over to the next
//! the boundary is put where the change is: at the onset, or, for a pitch
//! change, midway between the centres of the last old and first new frame.
//!
//! # Segmentation
//!
//! 1. A frame is *voiced* when YIN found a pitch and its RMS is at or above
//!    the [floor](NoteConfig::with_floor). The floor is absolute: YIN reads
//!    periodicity, not level, so without one a buffer of rounding noise has a
//!    pitch.
//! 2. Voiced frames join into a run across at most
//!    [`hold`](NoteConfig::with_hold) of dropout. Longer is a rest. The hold is
//!    capped below the shortest possible true rest, so it never joins two
//!    notes.
//! 3. An onset inside a run splits it, so re-articulating one pitch gives two
//!    notes.
//! 4. A pitch change splits it where the median pitch of the
//!    [`smoothing`](NoteConfig::with_smoothing) span after a frame differs from
//!    the median of the span before by more than the
//!    [split](NoteConfig::with_pitch_split). The medians reject a
//!    single-frame octave error, and a slide or vibrato narrower than the
//!    split stays one note. A change within one smoothing span of an onset is
//!    that onset's.
//! 5. A piece shorter than the [minimum length](NoteConfig::with_min_length) is
//!    dropped.
//!
//! The note is the median pitch of its frames, as the nearest [`Note`] and the
//! signed [`Cents`] from it. Its level is the loudest frame RMS wholly inside
//! the note, in dBFS: a sine of peak amplitude `a` reads `20 log10(a / sqrt 2)`.
//!
//! These are offline or UI-thread calls: they allocate the pitch track, the
//! levels and the notes.

use tutti_types::{Amplitude, Cents, Db, Note, SampleRate, Samples, Seconds};

use crate::error::{Error, Result};
use crate::fft::FftScratch;
use crate::onset::{DetectionFunction, OnsetConfig, detect_onsets};
use crate::tempo::onset_lag;
use crate::yin::{PitchEstimate, YinConfig, yin_track};

/// The longest YIN frame accepted. YIN allocates several buffers of about this
/// size, so an absurd `min_freq` would otherwise ask for gigabytes.
const MAX_FRAME: usize = 1 << 18;

/// The most frames one call analyses, bounding the track, level and voicing
/// tables (about 40 bytes a frame between them).
const MAX_FRAMES: usize = 1 << 22;

/// The most hops in one YIN frame. Frame levels sum a whole frame per hop, so
/// this bounds the cost of a tiny hop.
const MAX_OVERLAP: usize = 64;

/// The most frames in one smoothing span, bounding the median's cost.
const MAX_SMOOTHING_FRAMES: usize = 1024;

/// The longest time setting accepted, in seconds.
const MAX_SECONDS: f32 = 60.0;

/// Input whose peak is below this is silence, as in tempo estimation.
const SILENCE_FLOOR: Db = Db(-90.0);

/// What [`transcribe`] and [`notes_from_track`] segment with. Immutable once
/// built.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NoteConfig {
    yin: YinConfig,
    onset: OnsetConfig,
    hop: Samples,
    min_length: Samples,
    pitch_split: Cents,
    smoothing: Samples,
    hold: Samples,
    floor: Db,
}

impl NoteConfig {
    /// Segments `yin`'s pitch track and `onset`'s onsets with the defaults: a
    /// hop of an eighth of the YIN frame, a 60 ms minimum note, a 50 cent
    /// split, 30 ms of smoothing, 10 ms of hold and a -60 dBFS floor.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidNoteParameter`] when the YIN frame is longer than
    /// 262144 samples (a `min_freq` that low is not a note), when `onset` is
    /// not [`DetectionFunction::SpectralFlux`] (the only function whose peak
    /// has a known lag behind its attack), or when the two sample rates differ.
    pub fn new(yin: YinConfig, onset: OnsetConfig) -> Result<Self> {
        let frame = yin.buffer_size().get();
        if frame > MAX_FRAME {
            return Err(Error::InvalidNoteParameter("yin: analysis frame too long"));
        }
        if onset.function() != DetectionFunction::SpectralFlux {
            return Err(Error::InvalidNoteParameter("onset: not spectral flux"));
        }
        if onset.geometry().sample_rate() != yin.sample_rate() {
            return Err(Error::InvalidNoteParameter(
                "onset: sample rate differs from yin's",
            ));
        }
        let rate = yin.sample_rate();
        Ok(Self {
            yin,
            onset,
            hop: Samples((frame / 8).max(1)),
            min_length: seconds(0.06, rate),
            pitch_split: Cents(50.0),
            smoothing: seconds(0.03, rate),
            hold: seconds(0.01, rate),
            floor: Db(-60.0),
        })
    }

    /// How far the pitch track advances per frame.
    ///
    /// # Errors
    ///
    /// [`Error::ZeroHop`] for zero; [`Error::InvalidNoteParameter`] for a hop
    /// wider than the YIN frame (frames would leave gaps) or under 1/64 of it.
    pub fn with_hop(mut self, hop: impl Into<Samples>) -> Result<Self> {
        let hop = hop.into();
        let frame = self.frame().get();
        if hop.is_zero() {
            return Err(Error::ZeroHop);
        }
        if hop.get() > frame {
            return Err(Error::InvalidNoteParameter("hop: wider than the yin frame"));
        }
        if frame.div_ceil(hop.get()) > MAX_OVERLAP {
            return Err(Error::InvalidNoteParameter(
                "hop: under 1/64 of the yin frame",
            ));
        }
        self.hop = hop;
        Ok(self)
    }

    /// Notes shorter than this are dropped. Zero keeps every piece.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidNoteParameter`] unless finite and within `0..=60` s.
    pub fn with_min_length(mut self, length: impl Into<Seconds>) -> Result<Self> {
        self.min_length = checked_seconds(length.into(), self.yin.sample_rate(), "min length")?;
        Ok(self)
    }

    /// A pitch change above this splits a note. Under a semitone it reads
    /// vibrato as notes; over it, a step as one.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidNoteParameter`] unless finite and in `(0, 1200]` cents.
    pub fn with_pitch_split(mut self, split: impl Into<Cents>) -> Result<Self> {
        let split = split.into();
        if !(split.get() > 0.0 && split.get() <= 1200.0) {
            return Err(Error::InvalidNoteParameter("pitch split"));
        }
        self.pitch_split = split;
        Ok(self)
    }

    /// How long a pitch change must last to split a note: the median span each
    /// side of a candidate. Shorter blips, and vibrato whose half period is
    /// shorter, do not split. At least one frame is used.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidNoteParameter`] unless finite and within `0..=60` s.
    pub fn with_smoothing(mut self, span: impl Into<Seconds>) -> Result<Self> {
        self.smoothing = checked_seconds(span.into(), self.yin.sample_rate(), "smoothing")?;
        Ok(self)
    }

    /// The longest pitch dropout bridged inside a note, capped for the hop in
    /// use below the shortest true rest, so a rest is never bridged.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidNoteParameter`] unless finite and within `0..=60` s.
    pub fn with_hold(mut self, hold: impl Into<Seconds>) -> Result<Self> {
        self.hold = checked_seconds(hold.into(), self.yin.sample_rate(), "hold")?;
        Ok(self)
    }

    /// Frames whose RMS is below this are unvoiced. Absolute, in dBFS, so a
    /// note quieter than it is lost; scale the input rather than lower it
    /// below the noise.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidThreshold`] for a NaN or infinite floor.
    pub fn with_floor(mut self, floor: impl Into<Db>) -> Result<Self> {
        let floor = floor.into();
        if !floor.get().is_finite() {
            return Err(Error::InvalidThreshold { threshold: floor });
        }
        self.floor = floor;
        Ok(self)
    }

    /// The pitch detector.
    #[inline]
    pub fn yin(&self) -> &YinConfig {
        &self.yin
    }

    /// The onset detector.
    #[inline]
    pub fn onset(&self) -> &OnsetConfig {
        &self.onset
    }

    /// How far the pitch track advances per frame, in [`Samples`].
    #[inline]
    pub fn hop(&self) -> Samples {
        self.hop
    }

    /// The span one pitch frame covers, in [`Samples`]: YIN's buffer size.
    #[inline]
    pub fn frame(&self) -> Samples {
        self.yin.buffer_size()
    }

    /// Notes shorter than this are dropped, in [`Samples`].
    #[inline]
    pub fn min_length(&self) -> Samples {
        self.min_length
    }

    /// The pitch change that splits a note.
    #[inline]
    pub fn pitch_split(&self) -> Cents {
        self.pitch_split
    }

    /// The pitch-change persistence, in [`Samples`].
    #[inline]
    pub fn smoothing(&self) -> Samples {
        self.smoothing
    }

    /// The dropout bridged inside a note, in [`Samples`], before the cap.
    #[inline]
    pub fn hold(&self) -> Samples {
        self.hold
    }

    /// The level below which a frame is unvoiced, in dBFS.
    #[inline]
    pub fn floor(&self) -> Db {
        self.floor
    }

    /// How far a note's start or end may sit from the true attack or release
    /// of a steady note: one hop for the frame grid plus the integration
    /// window, half the YIN frame.
    ///
    /// The window is the bound because YIN reads silence as periodic: a frame
    /// whose window is mostly past the release is still voiced, so an end can
    /// run late by up to a window. A start cannot (silence before an attack
    /// does not match its lagged copy), so it is within a hop and a period. An end is never
    /// early by more than a period and a hop, and a period is within a window.
    /// An onset-split boundary is as exact as the onset, about one onset
    /// window.
    pub fn timing_tolerance(&self) -> Samples {
        Samples(self.hop.get() + span_of(self))
    }

    /// The most frames of dropout bridged: the hold in hops, below the fewest
    /// unvoiced frames a true rest leaves. A rest of `R` samples leaves
    /// `ceil((frame + R) / hop) - 1` frames none of which lies wholly in a
    /// note, so at least `ceil(frame / hop) - 1`, and the cap is one under that.
    fn bridge_frames(&self) -> usize {
        let hold = self.hold.get() / self.hop.get();
        let rest = (self.frame().get() / 2)
            .div_ceil(self.hop.get())
            .saturating_sub(1);
        hold.min(rest.saturating_sub(1))
    }

    /// Frames in the smoothing span, at least one.
    fn smoothing_frames(&self) -> usize {
        self.smoothing.get().div_ceil(self.hop.get()).max(1)
    }

    /// Where an onset's attack is, from the position it reports.
    fn attack(&self, position: Samples) -> Samples {
        Samples(position.get() + onset_lag(self.onset.geometry()))
    }
}

fn seconds(s: f32, rate: SampleRate) -> Samples {
    Seconds(s).to_samples(rate)
}

fn checked_seconds(s: Seconds, rate: SampleRate, what: &'static str) -> Result<Samples> {
    if !(s.get() >= 0.0 && s.get() <= MAX_SECONDS) {
        return Err(Error::InvalidNoteParameter(what));
    }
    Ok(s.to_samples(rate))
}

/// One note.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NoteEvent {
    /// The nearest equal-tempered note to the median pitch.
    pub note: Note,
    /// The median pitch's distance from `note`, `-50..=50` [`Cents`]: the
    /// pitch-bend amount.
    pub cents: Cents,
    /// The first sample of the note.
    pub start: Samples,
    /// One past the last sample of the note; `end > start`.
    pub end: Samples,
    /// The loudest frame RMS wholly inside the note, in dBFS. See
    /// [`VelocityRange`] for a MIDI velocity.
    pub velocity: Db,
}

impl NoteEvent {
    /// The note's length in [`Samples`].
    #[inline]
    pub fn len(&self) -> Samples {
        Samples(self.end.get() - self.start.get())
    }

    /// Whether the note spans nothing; never true of a transcribed note.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.end <= self.start
    }
}

/// A linear-in-dB map from a note's level to a MIDI velocity: `floor` and
/// anything quieter is 1, `ceiling` and anything louder 127.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VelocityRange {
    floor: Db,
    ceiling: Db,
}

impl Default for VelocityRange {
    /// -60 dBFS to -3 dBFS, the RMS of a full-scale sine.
    fn default() -> Self {
        Self {
            floor: Db(-60.0),
            ceiling: Db(-3.0),
        }
    }
}

impl VelocityRange {
    /// Levels from `floor` to `ceiling`.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidThreshold`] for a bound that is NaN or infinite;
    /// [`Error::InvalidNoteParameter`] unless `floor < ceiling`.
    pub fn new(floor: impl Into<Db>, ceiling: impl Into<Db>) -> Result<Self> {
        let (floor, ceiling) = (floor.into(), ceiling.into());
        for bound in [floor, ceiling] {
            if !bound.get().is_finite() {
                return Err(Error::InvalidThreshold { threshold: bound });
            }
        }
        if floor >= ceiling {
            return Err(Error::InvalidNoteParameter("velocity range"));
        }
        Ok(Self { floor, ceiling })
    }

    /// The MIDI velocity of `level`, `1..=127`. A NaN level is 1.
    pub fn midi(&self, level: Db) -> u8 {
        let t = (level.get() - self.floor.get()) / (self.ceiling.get() - self.floor.get());
        if t.is_nan() {
            return 1;
        }
        1 + (t.clamp(0.0, 1.0) * 126.0).round() as u8
    }
}

/// Transcribes the notes of a mono buffer.
///
/// Runs [`yin_track`] at the config's hop, [`frame_levels`] and
/// [`detect_onsets`], and segments them with [`notes_from_track`]. Onsets are
/// placed at `position + 3 * window / 4 - hop / 2` of the onset geometry, where
/// spectral flux peaks behind an attack.
///
/// # Errors
///
/// [`Error::NonFiniteInput`] for a NaN or infinite sample;
/// [`Error::InsufficientInput`] when shorter than one YIN frame;
/// [`Error::NoSignal`] when the peak is under -90 dBFS;
/// [`Error::InvalidNoteParameter`] when the hop would make more than 4194304
/// frames. Noise gives no notes rather than an error.
pub fn transcribe(
    cfg: &NoteConfig,
    samples: &[f32],
    fft: &mut FftScratch,
) -> Result<Vec<NoteEvent>> {
    if samples.iter().any(|s| !s.is_finite()) {
        return Err(Error::NonFiniteInput);
    }
    let frame = cfg.frame();
    if samples.len() < frame.get() {
        return Err(Error::InsufficientInput {
            needed: frame,
            got: Samples(samples.len()),
        });
    }
    let peak = samples.iter().fold(0.0f32, |m, s| m.max(s.abs()));
    if peak < SILENCE_FLOOR.to_amplitude().get() {
        return Err(Error::NoSignal);
    }
    if (samples.len() - frame.get()) / cfg.hop.get() >= MAX_FRAMES {
        return Err(Error::InvalidNoteParameter("hop: too many frames"));
    }

    let track = yin_track(&cfg.yin, samples, cfg.hop)?;
    let levels = frame_levels(cfg, samples);
    let onsets: Vec<Samples> = detect_onsets(&cfg.onset, samples, fft)?
        .iter()
        .map(|o| cfg.attack(o.position))
        .collect();
    notes_from_track(cfg, &track, &levels, &onsets)
}

/// The RMS of every pitch frame of `samples`: frame `i` covers
/// `i * hop .. i * hop + frame`, as [`yin_track`]'s does, so the result lines
/// up with a track made at the config's hop. Empty for input shorter than a
/// frame.
pub fn frame_levels(cfg: &NoteConfig, samples: &[f32]) -> Vec<Amplitude> {
    let frame = cfg.frame().get();
    if samples.len() < frame {
        return Vec::new();
    }
    let frames = (samples.len() - frame) / cfg.hop.get() + 1;
    (0..frames)
        .map(|i| {
            let start = i * cfg.hop.get();
            let energy: f64 = samples[start..start + frame]
                .iter()
                .map(|&s| f64::from(s) * f64::from(s))
                .sum();
            Amplitude((energy / frame as f64).sqrt() as f32)
        })
        .collect()
}

/// Segments a pitch track into notes.
///
/// `track` is [`yin_track`] at the config's hop and YIN config, `levels` the
/// [`frame_levels`] of the same audio, and `onsets` the sample positions of the
/// attacks, in any order. For onsets from [`detect_onsets`] add the lag
/// described at [`transcribe`].
///
/// # Errors
///
/// [`Error::InvalidNoteParameter`] when `levels` and `track` differ in length,
/// when a level is negative, when the smoothing span is over 1024 frames or
/// when the track is over 4194304 frames; [`Error::NonFiniteInput`] for a
/// non-finite level or a non-finite or non-positive voiced frequency.
pub fn notes_from_track(
    cfg: &NoteConfig,
    track: &[PitchEstimate],
    levels: &[Amplitude],
    onsets: &[Samples],
) -> Result<Vec<NoteEvent>> {
    if track.len() != levels.len() {
        return Err(Error::InvalidNoteParameter(
            "levels: length differs from the pitch track",
        ));
    }
    if track.len() > MAX_FRAMES {
        return Err(Error::InvalidNoteParameter("track: too many frames"));
    }
    if cfg.smoothing_frames() > MAX_SMOOTHING_FRAMES {
        return Err(Error::InvalidNoteParameter("smoothing: too many frames"));
    }
    let frames = voiced_frames(track, levels, cfg.floor.to_amplitude())?;
    let mut onset_frames: Vec<(usize, usize)> = onsets
        .iter()
        .map(|t| (t.get().div_ceil(cfg.hop.get()), t.get()))
        .collect();
    onset_frames.sort_unstable();

    let mut notes = Vec::new();
    for run in runs(&frames, cfg.bridge_frames()) {
        let pieces = split_run(cfg, &run, &frames, &onset_frames);
        for (i, piece) in pieces.iter().enumerate() {
            let next_split = pieces.get(i + 1).and_then(|p| p.split);
            if let Some(note) = build_note(cfg, &frames, &run, piece, next_split)
                && note.len() >= cfg.min_length
                && !note.is_empty()
            {
                notes.push(note);
            }
        }
    }
    Ok(notes)
}

/// The samples a voiced frame vouches for, from its start: the integration
/// window YIN compares, half the buffer.
fn span_of(cfg: &NoteConfig) -> usize {
    cfg.frame().get() / 2
}

/// What a voiced frame contributes.
#[derive(Debug, Clone, Copy)]
struct Voiced {
    /// Pitch against A4, in [`Cents`].
    cents: Cents,
    level: Amplitude,
}

/// The voiced frames of a track, `None` where unvoiced or under `floor`.
fn voiced_frames(
    track: &[PitchEstimate],
    levels: &[Amplitude],
    floor: Amplitude,
) -> Result<Vec<Option<Voiced>>> {
    let a4 = Note::A4.frequency().get();
    track
        .iter()
        .zip(levels)
        .map(|(pitch, level)| {
            if !level.get().is_finite() {
                return Err(Error::NonFiniteInput);
            }
            if level.get() < 0.0 {
                return Err(Error::InvalidNoteParameter("level: negative"));
            }
            let Some(pitch) = pitch.pitch() else {
                return Ok(None);
            };
            let frequency = pitch.frequency.get();
            if !(frequency.is_finite() && frequency > 0.0) {
                return Err(Error::NonFiniteInput);
            }
            Ok((*level >= floor).then(|| Voiced {
                cents: Cents::from_pitch_ratio(frequency / a4),
                level: *level,
            }))
        })
        .collect()
}

/// The voiced frame indices of each run: frames joined across at most
/// `bridge` unvoiced ones.
fn runs(frames: &[Option<Voiced>], bridge: usize) -> Vec<Vec<usize>> {
    let mut out: Vec<Vec<usize>> = Vec::new();
    for (i, _) in frames.iter().enumerate().filter(|(_, f)| f.is_some()) {
        match out.last_mut() {
            Some(run) if i - run[run.len() - 1] - 1 <= bridge => run.push(i),
            _ => out.push(vec![i]),
        }
    }
    out
}

/// A slice of a run's frames and the sample where the note holding it begins,
/// when it begins at a split rather than at its first frame.
#[derive(Debug, Clone, Copy)]
struct Piece {
    /// Positions in the run, `from..to`.
    from: usize,
    to: usize,
    /// The split before this piece, in samples, or `None` for the first.
    split: Option<usize>,
}

/// Cuts a run at its onsets and pitch changes.
fn split_run(
    cfg: &NoteConfig,
    run: &[usize],
    frames: &[Option<Voiced>],
    onsets: &[(usize, usize)],
) -> Vec<Piece> {
    let hop = cfg.hop.get();
    let span = cfg.smoothing_frames();

    // An onset at `t` (first whole frame `k`) splits before the first voiced
    // frame at or after `k`, provided something voiced precedes it.
    let mut cuts: Vec<(usize, usize)> = Vec::new();
    for &(k, t) in onsets {
        let pos = run.partition_point(|&f| f < k);
        if pos > 0 && pos < run.len() && cuts.last().is_none_or(|&(p, _)| p < pos) {
            cuts.push((pos, t));
        }
    }

    let cents: Vec<f32> = run
        .iter()
        .map(|&f| frames[f].map_or(0.0, |v| v.cents.get()))
        .collect();
    for pos in pitch_changes(&cents, span, cfg.pitch_split.get()) {
        if cuts.iter().all(|&(p, _)| p.abs_diff(pos) >= span) {
            // Midway between the centres of the last old and first new frame.
            let centre = |f: usize| f * hop + span_of(cfg) / 2;
            let split = (centre(run[pos - 1]) + centre(run[pos])) / 2;
            cuts.push((pos, split));
        }
    }
    cuts.sort_unstable();

    let mut pieces = Vec::with_capacity(cuts.len() + 1);
    let mut from = 0;
    let mut split = None;
    for (pos, t) in cuts {
        pieces.push(Piece {
            from,
            to: pos,
            split,
        });
        from = pos;
        split = Some(t);
    }
    pieces.push(Piece {
        from,
        to: run.len(),
        split,
    });
    pieces
}

/// The positions where the median of the next `span` values differs from the
/// median of the previous `span` by more than `split` cents, one per
/// contiguous stretch of such positions, at its middle.
fn pitch_changes(cents: &[f32], span: usize, split: f32) -> Vec<usize> {
    let n = cents.len();
    let mut changes = Vec::new();
    if n < 2 * span {
        return changes;
    }
    let mut stretch: Option<(usize, usize)> = None;
    for pos in span..=n - span {
        let before = median(&cents[pos - span..pos]);
        let after = median(&cents[pos..pos + span]);
        if (after - before).abs() > split {
            stretch = Some(stretch.map_or((pos, pos), |(first, _)| (first, pos)));
        } else if let Some((first, last)) = stretch.take() {
            changes.push((first + last) / 2);
        }
    }
    if let Some((first, last)) = stretch {
        changes.push((first + last) / 2);
    }
    changes
}

/// The median; the mean of the middle two for an even count.
fn median(values: &[f32]) -> f32 {
    let mut v = values.to_vec();
    v.sort_unstable_by(f32::total_cmp);
    let mid = v.len() / 2;
    if v.len() % 2 == 1 {
        v[mid]
    } else {
        (v[mid - 1] + v[mid]) / 2.0
    }
}

/// The note of one piece, or `None` when its span is empty.
fn build_note(
    cfg: &NoteConfig,
    frames: &[Option<Voiced>],
    run: &[usize],
    piece: &Piece,
    next_split: Option<usize>,
) -> Option<NoteEvent> {
    let hop = cfg.hop.get();
    let frame = cfg.frame().get();
    let mine = &run[piece.from..piece.to];
    let (first, last) = (*mine.first()?, *mine.last()?);

    let covered_start = first * hop;
    let covered_end = last * hop + span_of(cfg);
    let start = piece.split.map_or(covered_start, |s| covered_start.max(s));
    let end = next_split.map_or(covered_end, |s| covered_end.min(s));
    if end <= start {
        return None;
    }

    let voiced = |f: usize| frames[f].expect("a run holds voiced frames");
    let cents: Vec<f32> = mine.iter().map(|&f| voiced(f).cents.get()).collect();
    let pitch = Cents(median(&cents));
    let (note, bend) = Note::nearest_to(Note::A4.frequency() * pitch.to_pitch_ratio())?;

    // The loudest frame wholly inside the note; one that straddles a neighbour
    // reads that neighbour's level too. A note shorter than a frame has none.
    let inside = |&&f: &&usize| f * hop >= start && f * hop + frame <= end;
    let whole: Vec<usize> = mine.iter().filter(inside).copied().collect();
    let level = if whole.is_empty() { mine } else { &whole }
        .iter()
        .map(|&f| voiced(f).level)
        .fold(Amplitude::SILENT, |a, b| a.max(b));

    Some(NoteEvent {
        note,
        cents: bend,
        start: Samples(start),
        end: Samples(end),
        velocity: level.to_db(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::StftGeometry;
    use crate::yin::Pitch;
    use rand::{Rng, SeedableRng, rngs::StdRng};
    use tutti_types::{Confidence, Hz};

    const RATE: f64 = 44100.0;
    const TAU: f64 = std::f64::consts::TAU;

    fn config(rate: f64, min_freq: f32, hop_div: usize, onset_window: usize) -> NoteConfig {
        let yin = YinConfig::new(rate, Hz(min_freq), Hz(1000.0)).unwrap();
        let g = StftGeometry::new(rate, Samples(onset_window), Samples(onset_window / 4)).unwrap();
        NoteConfig::new(yin, OnsetConfig::new(g, DetectionFunction::SpectralFlux))
            .unwrap()
            .with_hop(Samples(yin.buffer_size().get() / hop_div))
            .unwrap()
    }

    fn cfg() -> NoteConfig {
        config(RATE, 80.0, 8, 1024)
    }

    /// A tone from `a` to `b` seconds at `rate`: `pitch(t)` in Hz and
    /// `amp(t)`, `t` seconds into the tone, with continuous phase.
    struct Tone {
        a: f64,
        b: f64,
        pitch: Box<dyn Fn(f64) -> f64>,
        amp: Box<dyn Fn(f64) -> f64>,
    }

    fn tone(a: f64, b: f64, hz: f64, amp: f64) -> Tone {
        Tone {
            a,
            b,
            pitch: Box::new(move |_| hz),
            amp: Box::new(move |_| amp),
        }
    }

    fn render(rate: f64, tones: &[Tone], secs: f64) -> Vec<f32> {
        let mut out = vec![0.0f32; (secs * rate) as usize];
        for t in tones {
            let (a, b) = ((t.a * rate) as usize, (t.b * rate) as usize);
            let mut phase = 0.0f64;
            for (i, s) in out[a..b].iter_mut().enumerate() {
                let at = i as f64 / rate;
                *s = ((t.amp)(at) * phase.sin()) as f32;
                phase += TAU * (t.pitch)(at) / rate;
            }
        }
        out
    }

    fn cents_up(hz: f64, cents: f64) -> f64 {
        hz * (cents / 1200.0).exp2()
    }

    fn run(cfg: &NoteConfig, x: &[f32]) -> Vec<NoteEvent> {
        transcribe(cfg, x, &mut FftScratch::new()).unwrap()
    }

    fn secs(s: Samples) -> f64 {
        s.get() as f64 / RATE
    }

    fn near(got: Samples, want: f64, rate: f64, tol: Samples) -> bool {
        (got.get() as f64 - want * rate).abs() <= tol.get() as f64
    }

    /// A hand-made track: `Some(hz)` voiced, `None` not.
    fn track(freqs: &[Option<f32>]) -> Vec<PitchEstimate> {
        freqs
            .iter()
            .map(|f| match f {
                None => PitchEstimate::Unvoiced,
                Some(hz) => {
                    let (note, cents_offset) =
                        Note::nearest_to(Hz(*hz)).unwrap_or((Note::A4, Cents(0.0)));
                    PitchEstimate::Voiced(Pitch {
                        frequency: Hz(*hz),
                        confidence: Confidence::CERTAIN,
                        note,
                        cents_offset,
                    })
                }
            })
            .collect()
    }

    fn loud(n: usize) -> Vec<Amplitude> {
        vec![Amplitude(0.3); n]
    }

    /// A4, C5 and E5, each 250 ms, with gaps, come out as exactly those notes
    /// at several YIN frame lengths, hops, onset windows and sample rates.
    /// Starts are within a hop and a period of the true attack, and starts and
    /// ends within the config's [`timing_tolerance`](NoteConfig::timing_tolerance).
    ///
    /// Mutation checked: dropping the `+ span_of` from the end (ending a note
    /// at its last frame's start) fails the late-end side at every config;
    /// returning `covered_start + hop` for the start fails the early side;
    /// reporting `Note::A4` for every note fails the pitch check.
    #[test]
    fn a_melody_gives_its_notes_at_every_geometry() {
        let melody = [
            (0.2, 0.45, 440.0, 69),
            (0.6, 0.85, 523.2511, 72),
            (1.0, 1.25, 659.2551, 76),
        ];
        for (rate, min_freq, hop_div, onset_window) in [
            (44100.0, 80.0, 8, 1024),
            (44100.0, 50.0, 4, 2048),
            (44100.0, 50.0, 16, 512),
            (48000.0, 100.0, 2, 1024),
            (22050.0, 60.0, 32, 512),
        ] {
            let c = config(rate, min_freq, hop_div, onset_window);
            let tones: Vec<_> = melody
                .iter()
                .map(|&(a, b, hz, _)| tone(a, b, hz, 0.5))
                .collect();
            let notes = transcribe(&c, &render(rate, &tones, 1.5), &mut FftScratch::new()).unwrap();
            assert_eq!(notes.len(), 3, "{notes:?} at {rate} {min_freq} {hop_div}");
            for (n, &(a, b, hz, midi)) in notes.iter().zip(&melody) {
                assert_eq!(u8::try_from(n.note), Ok(midi));
                assert!(n.cents.get().abs() < 1.0, "{}", n.cents);
                let tol = c.timing_tolerance();
                let period = Samples((rate / hz) as usize);
                let tight = Samples(c.hop().get() + period.get());
                assert!(
                    near(n.start, a, rate, tight),
                    "start {:?} vs {a} ({tight:?})",
                    n.start
                );
                assert!(
                    near(n.start, a, rate, tol),
                    "start {:?} vs {a} ({tol:?})",
                    n.start
                );
                assert!(
                    near(n.end, b, rate, tol),
                    "end {:?} vs {b} ({tol:?})",
                    n.end
                );
            }
        }
    }

    /// A tone 30 cents sharp is A4 plus 30 cents; 49 cents either way still
    /// rounds to A4, and 51 to the neighbour.
    ///
    /// Mutation checked: negating the bend, or taking the note by truncating
    /// the semitone count rather than `Note::nearest_to`, fails the -49 case.
    #[test]
    fn a_detuned_tone_reports_its_bend() {
        for (cents, midi, bend) in [
            (30.0, 69, 30.0),
            (-30.0, 69, -30.0),
            (49.0, 69, 49.0),
            (-49.0, 69, -49.0),
            (51.0, 70, -49.0),
        ] {
            let x = render(RATE, &[tone(0.1, 0.5, cents_up(440.0, cents), 0.5)], 0.7);
            let notes = run(&cfg(), &x);
            assert_eq!(notes.len(), 1, "{cents}: {notes:?}");
            assert_eq!(u8::try_from(notes[0].note), Ok(midi), "{cents}");
            assert!(
                (notes[0].cents.get() - bend).abs() < 1.5,
                "{cents}: {}",
                notes[0].cents
            );
        }
    }

    /// One pitch struck twice, with a 30 ms dip between, is two notes. Without
    /// the onsets the same track is one: the split is the onset's.
    ///
    /// Mutation checked: skipping the onset cuts in `split_run` fails the first
    /// assertion; ordering by `floor(t / hop)` rather than `ceil` is caught in
    /// `an_onset_splits_at_its_first_whole_frame`.
    #[test]
    fn a_re_articulated_note_is_two_notes() {
        let c = cfg();
        let dip = |t: f64| if (0.2..0.23).contains(&t) { 0.02 } else { 0.5 };
        let x = render(
            RATE,
            &[Tone {
                a: 0.2,
                b: 0.7,
                pitch: Box::new(|_| 440.0),
                amp: Box::new(dip),
            }],
            0.9,
        );
        let notes = run(&c, &x);
        assert_eq!(notes.len(), 2, "{notes:?}");
        assert!(notes.iter().all(|n| u8::try_from(n.note) == Ok(69)));
        assert!(notes[0].end <= notes[1].start || notes[0].end.get() - notes[1].start.get() == 0);
        let attack = 0.2 + 0.2; // the dip starts 0.2 s into the tone, at 0.4 s
        assert!(near(
            notes[1].start,
            attack + 0.03,
            RATE,
            c.timing_tolerance()
        ));

        let track = yin_track(c.yin(), &x, c.hop()).unwrap();
        let levels = frame_levels(&c, &x);
        assert_eq!(notes_from_track(&c, &track, &levels, &[]).unwrap().len(), 1);
    }

    /// Vibrato of +/-30 cents at 5 Hz and a 40 cent glide are one note each;
    /// a 100 cent step with no gap is two, split where the pitch changes.
    ///
    /// Mutation checked: a 20 cent split fails the vibrato and glide cases.
    /// The step case is also split by the onset the change makes, so disabling
    /// the pitch split does not fail it; the pitch cut is pinned on a
    /// hand-made track, with no onsets, in
    /// `a_pitch_step_splits_at_the_frame_midpoint_and_blips_do_not`.
    #[test]
    fn pitch_changes_split_only_beyond_the_threshold() {
        let c = cfg();
        let vib = Tone {
            a: 0.1,
            b: 1.1,
            pitch: Box::new(|t| cents_up(440.0, 30.0 * (TAU * 5.0 * t).sin())),
            amp: Box::new(|_| 0.5),
        };
        let glide = Tone {
            a: 0.1,
            b: 1.1,
            pitch: Box::new(|t| cents_up(440.0, 40.0 * t)),
            amp: Box::new(|_| 0.5),
        };
        for t in [vib, glide] {
            let notes = run(&c, &render(RATE, &[t], 1.3));
            assert_eq!(notes.len(), 1, "{notes:?}");
            assert_eq!(u8::try_from(notes[0].note), Ok(69));
        }
        let step = Tone {
            a: 0.1,
            b: 0.7,
            pitch: Box::new(|t| {
                if t < 0.3 {
                    440.0
                } else {
                    cents_up(440.0, 100.0)
                }
            }),
            amp: Box::new(|_| 0.5),
        };
        let notes = run(&c, &render(RATE, &[step], 0.9));
        assert_eq!(notes.len(), 2, "{notes:?}");
        assert_eq!(u8::try_from(notes[0].note), Ok(69));
        assert_eq!(u8::try_from(notes[1].note), Ok(70));
        // The onset the change makes cuts first, so the notes meet within a hop.
        assert!(notes[0].end <= notes[1].start);
        assert!(notes[1].start.get() - notes[0].end.get() <= c.hop().get());
        assert!(
            near(notes[0].end, 0.4, RATE, c.timing_tolerance()),
            "{:?}",
            notes[0].end
        );
    }

    /// A tone shorter than the minimum is dropped, and kept when the minimum is
    /// lowered; a zero minimum keeps everything.
    ///
    /// Mutation checked: deleting the `min_length` filter keeps the short note
    /// and fails the first assertion.
    #[test]
    fn a_note_shorter_than_the_minimum_is_dropped() {
        let x = render(
            RATE,
            &[tone(0.1, 0.16, 440.0, 0.5), tone(0.4, 0.7, 523.2511, 0.5)],
            0.9,
        );
        let c = cfg().with_min_length(Seconds(0.1)).unwrap();
        let notes = run(&c, &x);
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert_eq!(u8::try_from(notes[0].note), Ok(72));
        let kept = run(&cfg().with_min_length(Seconds(0.02)).unwrap(), &x);
        assert_eq!(kept.len(), 2, "{kept:?}");
        assert_eq!(u8::try_from(kept[0].note), Ok(69));
    }

    /// Silence is `NoSignal`, noise gives no notes, a NaN or infinity and a
    /// buffer shorter than a frame are refused.
    ///
    /// Mutation checked: removing the peak test returns `Ok(vec![])` for
    /// silence and fails the first. Removing the sample check alone survives
    /// for a NaN inside a frame (the level check refuses it with the same
    /// error), so the last case puts it in the tail no frame covers, where
    /// only that check sees it and removing it fails.
    #[test]
    fn silence_noise_and_bad_input_are_defined() {
        let c = cfg();
        let mut fft = FftScratch::new();
        assert_eq!(
            transcribe(&c, &vec![0.0; 44100], &mut fft),
            Err(Error::NoSignal)
        );
        assert_eq!(
            transcribe(&c, &vec![1e-6; 44100], &mut fft),
            Err(Error::NoSignal)
        );
        let mut rng = StdRng::seed_from_u64(7);
        let noise: Vec<f32> = (0..88200).map(|_| rng.gen_range(-0.3..0.3)).collect();
        assert_eq!(transcribe(&c, &noise, &mut fft), Ok(Vec::new()));
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let mut x = render(RATE, &[tone(0.1, 0.5, 440.0, 0.5)], 0.7);
            x[10_000] = bad;
            assert_eq!(transcribe(&c, &x, &mut fft), Err(Error::NonFiniteInput));
        }
        let n = c.frame().get() + 10 * c.hop().get() + c.hop().get() / 2;
        let mut tail: Vec<f32> = (0..n)
            .map(|i| (TAU * 440.0 * i as f64 / RATE).sin() as f32 * 0.5)
            .collect();
        *tail.last_mut().unwrap() = f32::NAN;
        assert_eq!(transcribe(&c, &tail, &mut fft), Err(Error::NonFiniteInput));
        assert!(matches!(
            transcribe(&c, &[0.5; 100], &mut fft),
            Err(Error::InsufficientInput { .. })
        ));
    }

    /// Louder notes read louder: the level of a sine is its RMS in dBFS, to a
    /// tenth of a decibel, and the MIDI velocity follows it. A note that
    /// swells reads at its peak, not its start.
    ///
    /// Mutation checked: taking the first frame's level, or the minimum,
    /// reads the swell at its quiet start and fails the swell check; using the
    /// peak amplitude rather than the RMS is 3 dB off and fails the figure.
    #[test]
    fn velocity_follows_amplitude() {
        let amps = [0.05, 0.2, 0.8];
        let tones: Vec<_> = amps
            .iter()
            .enumerate()
            .map(|(i, &a)| {
                tone(
                    0.1 + 0.5 * i as f64,
                    0.4 + 0.5 * i as f64,
                    440.0 * 1.2f64.powi(i as i32),
                    a,
                )
            })
            .collect();
        let notes = run(&cfg(), &render(RATE, &tones, 1.8));
        assert_eq!(notes.len(), 3, "{notes:?}");
        let range = VelocityRange::default();
        for (n, &a) in notes.iter().zip(&amps) {
            let want = 20.0 * (a / 2f64.sqrt()).log10();
            assert!(
                (f64::from(n.velocity.get()) - want).abs() < 0.1,
                "{} vs {want}",
                n.velocity
            );
        }
        assert!(notes[0].velocity < notes[1].velocity && notes[1].velocity < notes[2].velocity);
        let v: Vec<u8> = notes.iter().map(|n| range.midi(n.velocity)).collect();
        assert!(v[0] < v[1] && v[1] < v[2], "{v:?}");

        let swell = Tone {
            a: 0.1,
            b: 0.7,
            pitch: Box::new(|_| 440.0),
            amp: Box::new(|t| 0.1 + 0.7 * t / 0.6),
        };
        let n = run(&cfg(), &render(RATE, &[swell], 0.9));
        assert_eq!(n.len(), 1, "{n:?}");
        assert!(n[0].velocity.get() > -8.0, "{}", n[0].velocity);
    }

    /// The map is linear in dB between its bounds, clamps outside them, and
    /// reads NaN as the quietest.
    ///
    /// Mutation checked: `126.0` to `127.0` makes the ceiling 128 and fails
    /// the ceiling; dropping the `1 +` fails the floor; returning 127 for a
    /// NaN fails the NaN case.
    #[test]
    fn the_velocity_map_is_linear_and_clamped() {
        let r = VelocityRange::new(Db(-60.0), Db(-20.0)).unwrap();
        assert_eq!(r.midi(Db(-60.0)), 1);
        assert_eq!(r.midi(Db(-20.0)), 127);
        assert_eq!(r.midi(Db(-40.0)), 64);
        assert_eq!(r.midi(Db(-100.0)), 1);
        assert_eq!(r.midi(Db(0.0)), 127);
        assert_eq!(r.midi(Db(f32::NEG_INFINITY)), 1);
        assert_eq!(r.midi(Db(f32::NAN)), 1);
        assert!(VelocityRange::new(Db(-20.0), Db(-20.0)).is_err());
        assert!(VelocityRange::new(Db(-20.0), Db(-30.0)).is_err());
        assert!(VelocityRange::new(Db(f32::NAN), Db(0.0)).is_err());
        assert!(VelocityRange::new(Db(-60.0), Db(f32::INFINITY)).is_err());
    }

    /// Scaling the input by a power of two (exact in floating point) changes
    /// nothing: pitch and timing are the same, bit for bit, and only the level
    /// moves, by the gain in dB. Below the floor the note is gone.
    ///
    /// Mutation checked: gating frames on a fixed amplitude of 0.05 rather than
    /// the floor fails the 1/32 case; an absolute threshold anywhere in the
    /// timing fails the equality.
    #[test]
    fn gain_changes_only_the_level() {
        let c = cfg();
        let base = render(
            RATE,
            &[tone(0.1, 0.4, 440.0, 0.25), tone(0.6, 0.9, 523.2511, 0.25)],
            1.1,
        );
        let want = run(&c, &base);
        assert_eq!(want.len(), 2);
        for (gain, db) in [(2.0f32, 6.0206), (0.25, -12.0412), (0.125, -18.0618)] {
            let scaled: Vec<f32> = base.iter().map(|s| s * gain).collect();
            let got = run(&c, &scaled);
            assert_eq!(got.len(), 2, "{gain}");
            for (g, w) in got.iter().zip(&want) {
                assert_eq!(
                    (g.note, g.cents, g.start, g.end),
                    (w.note, w.cents, w.start, w.end),
                    "{gain}"
                );
                assert!(
                    (g.velocity.get() - w.velocity.get() - db).abs() < 0.01,
                    "{gain}"
                );
            }
        }
        // -60 dBFS RMS is an amplitude of 0.00141; this is under it.
        let quiet: Vec<f32> = base.iter().map(|s| s * 0.001).collect();
        assert_eq!(run(&c, &quiet), Vec::new());
        let lowered = c.with_floor(Db(-80.0)).unwrap();
        assert_eq!(run(&lowered, &quiet).len(), 2);
    }

    /// A dropout of up to the bridge is inside one note; one more frame is a
    /// rest. On real audio a rest as short as 20 ms is never bridged, even
    /// with a minute of hold.
    ///
    /// Mutation checked: `hold` ignored (bridge 0) fails the first case;
    /// removing the `.min(rest...)` cap fails the real-audio case; an off by
    /// one in `runs` (`<` for `<=`) fails the boundary pair.
    #[test]
    fn dropouts_are_bridged_and_rests_are_not() {
        let c = cfg();
        let bridge = c.bridge_frames();
        assert_eq!(bridge, 3);
        let with_gap = |gap: usize| {
            let mut t = vec![Some(440.0); 40];
            t.extend(vec![None; gap]);
            t.extend(vec![Some(440.0); 40]);
            notes_from_track(&c, &track(&t), &loud(80 + gap), &[]).unwrap()
        };
        assert_eq!(with_gap(bridge).len(), 1);
        assert_eq!(with_gap(bridge + 1).len(), 2);

        let x = render(
            RATE,
            &[tone(0.1, 0.4, 440.0, 0.5), tone(0.42, 0.7, 440.0, 0.5)],
            0.9,
        );
        let long = c.with_hold(Seconds(60.0)).unwrap();
        let t = yin_track(long.yin(), &x, long.hop()).unwrap();
        let notes = notes_from_track(&long, &t, &frame_levels(&long, &x), &[]).unwrap();
        assert_eq!(notes.len(), 2, "{notes:?}");
    }

    /// An onset at `t` cuts before the first whole frame at or after `t`: the
    /// earlier note ends at `t` and the later starts on that frame. Onsets in
    /// any order, repeated, or at the run's own start give the same cut.
    ///
    /// Mutation checked: `floor` in place of `div_ceil` starts the later note
    /// at `t` and fails the start; clamping the end with `max` fails the end;
    /// not sorting the onsets fails the reversed case.
    #[test]
    fn an_onset_splits_at_its_first_whole_frame() {
        let c = cfg().with_min_length(Seconds(0.0)).unwrap();
        let hop = c.hop().get();
        let t = track(&vec![Some(440.0); 40]);
        let at = 17 * hop + 50;
        let notes = notes_from_track(&c, &t, &loud(40), &[Samples(at)]).unwrap();
        assert_eq!(notes.len(), 2);
        assert_eq!(notes[0].start, Samples(0));
        assert_eq!(notes[0].end, Samples(at));
        assert_eq!(notes[1].start, Samples(18 * hop));
        assert_eq!(notes[1].end, Samples(39 * hop + c.frame().get() / 2));

        let extra = [Samples(30 * hop - 3), Samples(0), Samples(at), Samples(at)];
        let mut reversed = extra;
        reversed.reverse();
        let a = notes_from_track(&c, &t, &loud(40), &extra).unwrap();
        let b = notes_from_track(&c, &t, &loud(40), &reversed).unwrap();
        assert_eq!(a, b);
        assert_eq!(a.len(), 3);
        // An onset at the very start, or past the last frame, cuts nothing.
        let none = [Samples(0), Samples(1000 * hop)];
        assert_eq!(notes_from_track(&c, &t, &loud(40), &none).unwrap().len(), 1);
    }

    /// A pitch step in a hand-made track splits at the midpoint of the two
    /// frames' centres; a single octave-error frame and a short blip do not.
    ///
    /// Mutation checked: median replaced by the first value of the span fails
    /// the octave-error case; using `first` rather than the stretch middle
    /// fails the boundary; a `>=` for `>` on the split is not distinguishable
    /// here and is not claimed.
    #[test]
    fn a_pitch_step_splits_at_the_frame_midpoint_and_blips_do_not() {
        let c = cfg();
        let (hop, half) = (c.hop().get(), c.frame().get() / 2);
        let mut step = vec![Some(440.0); 20];
        step.extend(vec![Some(523.2511); 20]);
        let notes = notes_from_track(&c, &track(&step), &loud(40), &[]).unwrap();
        assert_eq!(notes.len(), 2);
        let boundary = ((19 * hop + half / 2) + (20 * hop + half / 2)) / 2;
        assert_eq!(
            (notes[0].end.get(), notes[1].start.get()),
            (boundary, boundary)
        );
        assert_eq!(u8::try_from(notes[1].note), Ok(72));

        let mut octave = vec![Some(440.0); 40];
        octave[20] = Some(880.0);
        for blip in 1..c.smoothing_frames() / 2 {
            octave[20..20 + blip].fill(Some(880.0));
            let notes = notes_from_track(&c, &track(&octave), &loud(40), &[]).unwrap();
            assert_eq!(notes.len(), 1, "{blip}: {notes:?}");
            assert_eq!(u8::try_from(notes[0].note), Ok(69));
        }
    }

    /// The earlier note of an onset split reads its own level: frames that
    /// reach past the cut see the louder note and are not counted.
    ///
    /// Mutation checked: taking the maximum over every frame of the piece
    /// reads 0.8 and fails.
    #[test]
    fn a_note_does_not_read_its_neighbours_level() {
        let c = cfg();
        let (hop, frame) = (c.hop().get(), c.frame().get());
        let k = 20;
        let mut levels = vec![Amplitude(0.1); 40];
        for l in &mut levels[k - frame / hop + 1..] {
            *l = Amplitude(0.8);
        }
        let t = track(&vec![Some(440.0); 40]);
        let notes = notes_from_track(&c, &t, &levels, &[Samples(k * hop)]).unwrap();
        assert_eq!(notes.len(), 2);
        assert!((notes[0].velocity.get() - Amplitude(0.1).to_db().get()).abs() < 1e-3);
        assert!((notes[1].velocity.get() - Amplitude(0.8).to_db().get()).abs() < 1e-3);
    }

    /// Every setting is validated, so absurd ones cannot allocate or overflow.
    ///
    /// Mutation checked: each assertion names a guard; removing that guard
    /// (the `MAX_FRAME` test, the hop bounds, the `MAX_SECONDS` range, the
    /// sample-rate and function checks, the length check, the smoothing cap,
    /// the level checks) fails its line.
    #[test]
    fn settings_and_inputs_are_validated() {
        let g = StftGeometry::new(RATE, Samples(1024), Samples(256)).unwrap();
        let flux = OnsetConfig::new(g, DetectionFunction::SpectralFlux);
        let bad = |e: Error| matches!(e, Error::InvalidNoteParameter(_));
        let huge = YinConfig::new(RATE, Hz(0.01), Hz(1000.0)).unwrap();
        assert!(bad(NoteConfig::new(huge, flux).unwrap_err()));
        let yin = YinConfig::new(RATE, Hz(80.0), Hz(1000.0)).unwrap();
        let energy = OnsetConfig::new(g, DetectionFunction::Energy);
        assert!(bad(NoteConfig::new(yin, energy).unwrap_err()));
        let other = StftGeometry::new(48000.0, Samples(1024), Samples(256)).unwrap();
        let other = OnsetConfig::new(other, DetectionFunction::SpectralFlux);
        assert!(bad(NoteConfig::new(yin, other).unwrap_err()));

        let c = cfg();
        let frame = c.frame().get();
        assert_eq!(c.with_hop(Samples(0)).unwrap_err(), Error::ZeroHop);
        assert!(bad(c.with_hop(Samples(frame + 1)).unwrap_err()));
        assert!(bad(c.with_hop(Samples(frame / 64 - 1)).unwrap_err()));
        assert!(c.with_hop(Samples(frame)).is_ok());
        assert!(c.with_hop(Samples(frame.div_ceil(64))).is_ok());
        for v in [-1.0, f32::NAN, f32::INFINITY, 61.0] {
            assert!(bad(c.with_min_length(Seconds(v)).unwrap_err()), "{v}");
            assert!(bad(c.with_smoothing(Seconds(v)).unwrap_err()), "{v}");
            assert!(bad(c.with_hold(Seconds(v)).unwrap_err()), "{v}");
        }
        for v in [0.0, -5.0, f32::NAN, 1201.0, f32::INFINITY] {
            assert!(bad(c.with_pitch_split(Cents(v)).unwrap_err()), "{v}");
        }
        assert!(c.with_pitch_split(Cents(1200.0)).is_ok());
        for v in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            assert!(matches!(
                c.with_floor(Db(v)),
                Err(Error::InvalidThreshold { .. })
            ));
        }

        let t = track(&[Some(440.0); 10]);
        assert!(bad(notes_from_track(&c, &t, &loud(9), &[]).unwrap_err()));
        let mut neg = loud(10);
        neg[3] = Amplitude(-0.1);
        assert!(bad(notes_from_track(&c, &t, &neg, &[]).unwrap_err()));
        neg[3] = Amplitude(f32::NAN);
        assert_eq!(
            notes_from_track(&c, &t, &neg, &[]),
            Err(Error::NonFiniteInput)
        );
        let zero = track(&[Some(0.0); 10]);
        assert_eq!(
            notes_from_track(&c, &zero, &loud(10), &[]),
            Err(Error::NonFiniteInput)
        );
        let nan = track(&[Some(f32::NAN); 10]);
        assert_eq!(
            notes_from_track(&c, &nan, &loud(10), &[]),
            Err(Error::NonFiniteInput)
        );
        let wide = c.with_smoothing(Seconds(60.0)).unwrap();
        assert!(bad(notes_from_track(&wide, &t, &loud(10), &[]).unwrap_err()));
        assert!(notes_from_track(&c, &[], &[], &[]).unwrap().is_empty());
        assert!(
            notes_from_track(&c, &track(&[None; 10]), &loud(10), &[])
                .unwrap()
                .is_empty()
        );
    }

    /// A hop so small the input would make over 4 M frames is refused before
    /// anything is allocated.
    ///
    /// Mutation checked: removing the frame-count test in `transcribe` lets it
    /// run (and this test times out rather than returning the error).
    #[test]
    fn too_many_frames_are_refused() {
        let yin = YinConfig::new(RATE, Hz(5000.0), Hz(10000.0)).unwrap();
        let g = StftGeometry::new(RATE, Samples(1024), Samples(256)).unwrap();
        let c = NoteConfig::new(yin, OnsetConfig::new(g, DetectionFunction::SpectralFlux))
            .unwrap()
            .with_hop(Samples(1))
            .unwrap();
        assert!(c.frame().get() < 64, "{:?}", c.frame());
        let x = vec![0.1f32; MAX_FRAMES + c.frame().get() + 1];
        assert_eq!(
            transcribe(&c, &x, &mut FftScratch::new()),
            Err(Error::InvalidNoteParameter("hop: too many frames"))
        );
    }

    /// The default config's figures, and `secs` for reading them.
    #[test]
    fn the_defaults_are_the_documented_ones() {
        let c = cfg();
        assert_eq!(c.min_length(), Samples(2646));
        assert_eq!(c.smoothing(), Samples(1323));
        assert_eq!(c.hold(), Samples(441));
        assert_eq!(c.pitch_split(), Cents(50.0));
        assert_eq!(c.floor(), Db(-60.0));
        assert!((secs(c.min_length()) - 0.06).abs() < 1e-6);
        let d = NoteConfig::new(*c.yin(), *c.onset()).unwrap();
        assert_eq!(d.hop().get(), d.frame().get() / 8);
    }
}
