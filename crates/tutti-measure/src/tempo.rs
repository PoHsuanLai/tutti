//! Tempo estimation and beat tracking.
//!
//! The tempo is the strongest peak of the novelty curve's autocorrelation,
//! weighted by a log-Gaussian prior over tempo (Ellis 2007; the same shape as
//! librosa's `beat_track`), refined to a fraction of a frame by a parabola
//! through the peak. The beats come from [`crate::beat`]'s dynamic program,
//! run at that tempo.
//!
//! # Time resolution
//!
//! Everything works on the novelty curve, one value per hop. The default
//! [`TempoConfig::geometry`] is a 1024-sample window with a 512-sample hop:
//! 11.6 ms at 44.1 kHz. Beats sit on the hop grid, shifted by where flux peaks in a frame, so each is
//! within one hop of the onset it marks. A tempo is a period of whole frames
//! refined by a parabola, so its error grows with tempo: measured, 0.3 BPM at
//! 90 BPM and 1.1 at 174 at this hop. A smaller hop buys resolution at the cost
//! of more frames.
//!
//! # Octaves
//!
//! A pulse at `T` also repeats at `2T`, so the autocorrelation peaks at every
//! multiple of the beat period. The prior picks among them: it centres on
//! 120 BPM one octave wide, so a train that only repeats at 60 BPM is reported
//! as 60, while one with a half-strength pulse between its beats is reported as
//! 120 and 60 appears among the [`Tempo::alternatives`]. Move the centre or
//! narrow the range to settle it for a known genre.
//!
//! These are offline or UI-thread calls: each allocates the novelty curve, its
//! autocorrelation and the tracker's tables.

use tutti_types::{Bpm, Confidence, Db, SampleRate, Samples, Seconds};

use crate::beat;
use crate::onset::{DetectionFunction, OnsetConfig, OnsetState, onset_lag, step_onset};
use crate::xcorr::cross_correlate;
use tutti_dsp::FftScratch;
use tutti_dsp::StftGeometry;
use tutti_dsp::{Error, Result};

/// The default analysis window, in samples.
const WINDOW: usize = 1024;
/// The default hop, in samples.
const HOP: usize = 512;

/// The slowest and fastest tempo [`TempoConfig::with_range`] accepts. Outside
/// them a period is not a musical beat, and the lag it maps to is absurd.
const MIN_BPM: f64 = 1.0;
const MAX_BPM: f64 = 1000.0;

/// The most runner-up tempi kept.
const MAX_ALTERNATIVES: usize = 16;

/// Input whose peak is below this is treated as silence. The audio entry
/// points work on the spectral flux of the samples, which is relative, so
/// without a floor the rounding noise of a near-silent buffer would still read
/// as a pulse.
const SILENCE_FLOOR: Db = Db(-90.0);

/// A curve with less relative spread than this is flat: its peak-to-peak range
/// is within rounding of its own level.
const FLAT: f32 = f32::EPSILON;

/// What [`estimate_tempo`] and [`track_beats`] search and prefer.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TempoConfig {
    min: Bpm,
    max: Bpm,
    centre: Bpm,
    // Octaves: a ratio between two tempi, which no unit type covers.
    spread: f32,
    tightness: f32,
    alternatives: usize,
}

impl Default for TempoConfig {
    /// 60 to 200 BPM, a prior centred on 120 BPM one octave wide, a tightness
    /// of 100 and two alternatives.
    fn default() -> Self {
        Self {
            min: Bpm::of(60.0),
            max: Bpm::of(200.0),
            centre: Bpm::of(120.0),
            spread: 1.0,
            tightness: 100.0,
            alternatives: 2,
        }
    }
}

impl TempoConfig {
    /// The defaults; see [`Default`].
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The window and hop the audio entry points analyse with: 1024 and 512
    /// samples, 11.6 ms at 44.1 kHz.
    ///
    /// # Errors
    ///
    /// [`Error::NonPositiveSampleRate`] for a rate that is zero, negative or
    /// NaN.
    pub fn geometry(sample_rate: impl Into<SampleRate>) -> Result<StftGeometry> {
        StftGeometry::new(sample_rate, Samples(WINDOW), Samples(HOP))
    }

    /// Search tempi from `min` to `max` inclusive. The estimate never leaves
    /// the range. A range that holds no whole-frame lag at the hop in use, or
    /// needs more curve than the input has, is refused when the tempo is
    /// estimated.
    ///
    /// # Errors
    ///
    /// [`Error::EmptyTempoRange`] unless `1 <= min < max <= 1000`.
    pub fn with_range(mut self, min: Bpm, max: Bpm) -> Result<Self> {
        let ok = |b: Bpm| (MIN_BPM..=MAX_BPM).contains(&b.get());
        if !(ok(min) && ok(max) && min < max) {
            return Err(Error::EmptyTempoRange { min, max });
        }
        self.min = min;
        self.max = max;
        Ok(self)
    }

    /// Prefer tempi near `centre`, falling to `exp(-1/2)` of the weight one
    /// `spread` away, in octaves (a factor of two in tempo per octave).
    ///
    /// # Errors
    ///
    /// [`Error::InvalidTempoParameter`] unless `spread` is finite and above
    /// zero.
    pub fn with_prior(mut self, centre: Bpm, spread_octaves: f32) -> Result<Self> {
        if !(spread_octaves.is_finite() && spread_octaves > 0.0) {
            return Err(Error::InvalidTempoParameter("prior spread"));
        }
        self.centre = centre;
        self.spread = spread_octaves;
        Ok(self)
    }

    /// How hard the beat tracker holds to the tempo: the weight of the penalty
    /// on a gap's log ratio to the beat period, against a novelty score of unit
    /// standard deviation. Zero ignores the period and picks the strongest
    /// onsets; large values give an evenly spaced grid.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidTempoParameter`] unless `tightness` is finite and not
    /// negative.
    pub fn with_tightness(mut self, tightness: f32) -> Result<Self> {
        if !(tightness.is_finite() && tightness >= 0.0) {
            return Err(Error::InvalidTempoParameter("tightness"));
        }
        self.tightness = tightness;
        Ok(self)
    }

    /// How many runner-up tempi [`Tempo::alternatives`] holds, at most; capped
    /// at 16.
    #[must_use]
    pub fn with_alternatives(mut self, count: usize) -> Self {
        self.alternatives = count.min(MAX_ALTERNATIVES);
        self
    }

    /// The slowest and fastest tempo searched.
    #[inline]
    #[must_use]
    pub fn range(&self) -> (Bpm, Bpm) {
        (self.min, self.max)
    }

    /// The tempo the prior favours.
    #[inline]
    #[must_use]
    pub fn centre(&self) -> Bpm {
        self.centre
    }

    /// The beat tracker's tightness.
    #[inline]
    #[must_use]
    pub fn tightness(&self) -> f32 {
        self.tightness
    }
}

/// A tempo other than the winner.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TempoCandidate {
    /// The tempo, in [`Bpm`].
    pub bpm: Bpm,
    /// How periodic the novelty is at this tempo, as for [`Tempo::confidence`].
    pub strength: Confidence,
}

/// A tempo estimate.
#[derive(Debug, Clone, PartialEq)]
pub struct Tempo {
    /// The estimated tempo, in [`Bpm`], inside the configured range.
    pub bpm: Bpm,
    /// How periodic the novelty is at that tempo: the autocorrelation
    /// coefficient of the mean-removed curve at the tempo's lag, in `0..=1`
    /// (a negative one reads zero). One is a curve that repeats exactly over
    /// the whole clip, and noise reads near zero. It is not a probability, and
    /// it does not measure octave ambiguity: a train that repeats at 60 and
    /// 120 BPM reads high for both, so check
    /// [`alternatives`](Self::alternatives) when the octave matters. It is
    /// normalised by the whole curve, not the overlap at the lag, so a clip
    /// short against the period reads lower, not higher.
    pub confidence: Confidence,
    /// The next strongest tempi after the prior's weighting, best first. Each
    /// is a separate peak of the autocorrelation, so a half or double of
    /// [`bpm`](Self::bpm) shows up here when the signal supports it.
    pub alternatives: Vec<TempoCandidate>,
}

/// Beat positions and the tempo they were placed at.
#[derive(Debug, Clone, PartialEq)]
pub struct BeatGrid {
    /// The beats, ascending, in [`Samples`] from the start of the analysed
    /// buffer, on the hop grid.
    pub beats: Vec<Samples>,
    /// The tempo the beats were placed at, in [`Bpm`]: the estimate, or the
    /// one the caller supplied.
    pub bpm: Bpm,
}

impl BeatGrid {
    /// Beat `index` in [`Seconds`], resolved against `sample_rate`, or `None`
    /// past the last beat.
    #[must_use]
    pub fn time(&self, index: usize, sample_rate: impl Into<SampleRate>) -> Option<Seconds> {
        self.beats
            .get(index)
            .map(|b| Seconds(b.get() as f64 / sample_rate.into().get()))
    }
}

/// Estimates the tempo of `samples`.
///
/// Takes the spectral novelty of `samples` on `geometry` (see
/// [`TempoConfig::geometry`]) and runs [`estimate_tempo_from_novelty`] on it.
/// The first frame has no predecessor to differ from, so its novelty is zero.
///
/// # Errors
///
/// [`Error::NonFiniteInput`] for a NaN or infinite sample; [`Error::NoSignal`]
/// for input whose peak is under -90 dBFS or whose novelty is flat; [`Error::InsufficientInput`]
/// when the input is shorter than one window, or holds fewer than two periods
/// of the slowest tempo; [`Error::EmptyTempoRange`] when no whole-frame lag
/// falls in the range at this hop.
pub fn estimate_tempo(
    cfg: &TempoConfig,
    geometry: StftGeometry,
    samples: &[f32],
    fft: &mut FftScratch,
) -> Result<Tempo> {
    let novelty = novelty(geometry, samples, fft)?;
    estimate_tempo_from_novelty(cfg, &novelty, geometry.sample_rate(), geometry.hop(), fft)
}

/// Estimates the tempo from a novelty curve of one value per `hop` samples.
///
/// For a caller's own detection function. The curve is read as strength per
/// frame, and only its shape matters: any positive gain gives the same
/// result. Its level is not judged: only an exactly flat curve (or one flat to
/// rounding) is [`Error::NoSignal`], so a curve from near-silent audio is the
/// caller's to gate.
///
/// # Errors
///
/// As [`estimate_tempo`] (except that a silence floor applies to samples
/// only), with [`Error::NonFiniteInput`] for a NaN or infinite
/// value in `novelty` and [`Error::ZeroHop`] for a zero `hop`.
pub fn estimate_tempo_from_novelty(
    cfg: &TempoConfig,
    novelty: &[f32],
    sample_rate: impl Into<SampleRate>,
    hop: Samples,
    fft: &mut FftScratch,
) -> Result<Tempo> {
    let rate = frame_rate(sample_rate.into(), hop)?;
    let curve = Periodicity::new(cfg, novelty, rate, hop, fft)?;
    Ok(curve.tempo(cfg))
}

/// Places the beats of `samples` at its estimated tempo.
///
/// Takes the novelty as [`estimate_tempo`] does, and places beats with
/// [`track_beats_from_novelty`]; each beat is `frame * hop` plus the lag that spectral
/// flux peaks behind its onset, `3 * window / 4 - hop / 2`.
///
/// # Errors
///
/// As [`estimate_tempo`].
pub fn track_beats(
    cfg: &TempoConfig,
    geometry: StftGeometry,
    samples: &[f32],
    fft: &mut FftScratch,
) -> Result<BeatGrid> {
    let novelty = novelty(geometry, samples, fft)?;
    let mut grid = track_beats_from_novelty(
        cfg,
        &novelty,
        geometry.sample_rate(),
        geometry.hop(),
        None,
        fft,
    )?;
    let shift = onset_lag(geometry);
    for beat in &mut grid.beats {
        *beat = Samples(beat.get() + shift);
    }
    Ok(grid)
}

/// Places beats on a novelty curve of one value per `hop` samples.
///
/// With `tempo` of `None` the tempo is [`estimate_tempo_from_novelty`]'s;
/// with `Some` it is that tempo, whatever the range, and the curve need only
/// be long enough to track. Beat `i` is at `frame * hop` samples, the position
/// of its novelty value.
///
/// # Errors
///
/// As [`estimate_tempo_from_novelty`] when estimating. With a supplied tempo,
/// [`Error::NonFiniteInput`] for a NaN or infinite value in `novelty`, and
/// [`Error::InvalidTempoParameter`] for a tempo whose period is not between two
/// frames and the curve's length; a flat curve gives no beats.
pub fn track_beats_from_novelty(
    cfg: &TempoConfig,
    novelty: &[f32],
    sample_rate: impl Into<SampleRate>,
    hop: Samples,
    tempo: Option<Bpm>,
    fft: &mut FftScratch,
) -> Result<BeatGrid> {
    let sample_rate = sample_rate.into();
    let rate = frame_rate(sample_rate, hop)?;
    let bpm = match tempo {
        Some(bpm) => {
            if novelty.iter().any(|v| !v.is_finite()) {
                return Err(Error::NonFiniteInput);
            }
            // A period under two frames cannot be tracked, and one longer than
            // the curve holds at most one beat.
            let period = 60.0 * rate / bpm.get();
            if !(2.0..=novelty.len() as f64).contains(&period) {
                return Err(Error::InvalidTempoParameter("tempo"));
            }
            bpm
        }
        None => estimate_tempo_from_novelty(cfg, novelty, sample_rate, hop, fft)?.bpm,
    };
    let period = 60.0 * rate / bpm.get();
    let beats = beat::track(novelty, period, cfg.tightness)
        .into_iter()
        .map(|frame| Samples(frame * hop.get()))
        .collect();
    Ok(BeatGrid { beats, bpm })
}

/// Frames per second of a curve with one value per `hop`.
fn frame_rate(sample_rate: SampleRate, hop: Samples) -> Result<f64> {
    if hop.is_zero() {
        return Err(Error::ZeroHop);
    }
    if sample_rate.get().is_nan() || sample_rate.get() <= 0.0 {
        return Err(Error::NonPositiveSampleRate);
    }
    Ok(sample_rate.get() / hop.get() as f64)
}

/// The detection curve of `samples`, with the first frame zeroed.
fn novelty(geometry: StftGeometry, samples: &[f32], fft: &mut FftScratch) -> Result<Vec<f32>> {
    if samples.iter().any(|s| !s.is_finite()) {
        return Err(Error::NonFiniteInput);
    }
    let window = geometry.window().get();
    if samples.len() < window {
        return Err(Error::InsufficientInput {
            needed: Samples(window),
            got: Samples(samples.len()),
        });
    }
    let peak = samples.iter().fold(0.0f32, |m, s| m.max(s.abs()));
    if peak < SILENCE_FLOOR.to_amplitude().get() {
        return Err(Error::NoSignal);
    }
    let onset = OnsetConfig::new(geometry, DetectionFunction::SpectralFlux);
    let mut state = OnsetState::new();
    for frame in geometry.frames_for(Samples(samples.len())).indices() {
        let offset = frame.get() * geometry.hop().get();
        step_onset(
            &onset,
            &mut state,
            frame,
            &samples[offset..offset + window],
            fft,
        );
    }
    let mut curve: Vec<f32> = state.novelty().iter().map(|&(_, v)| v).collect();
    if let Some(first) = curve.first_mut() {
        *first = 0.0;
    }
    Ok(curve)
}

/// The normalised autocorrelation of a novelty curve over the searched lags.
struct Periodicity {
    /// Normalised autocorrelation at lags `first_lag..=last_lag`, including
    /// the one lag either side that the refinement reads.
    values: Vec<f32>,
    /// The first lag inside the tempo range.
    first_lag: usize,
    last_lag: usize,
    frame_rate: f64,
}

impl Periodicity {
    fn new(
        cfg: &TempoConfig,
        novelty: &[f32],
        frame_rate: f64,
        hop: Samples,
        fft: &mut FftScratch,
    ) -> Result<Self> {
        if novelty.iter().any(|v| !v.is_finite()) {
            return Err(Error::NonFiniteInput);
        }
        // Lag 1 has no neighbour below it for the refinement, so start at 2.
        let first_lag = ((60.0 * frame_rate / cfg.max.get()).ceil() as usize).max(2);
        // In floating point first: a slow tempo at a fast frame rate maps to a
        // lag that does not fit a `usize` after the arithmetic below.
        let longest = (60.0 * frame_rate / cfg.min.get()).floor();
        if longest < first_lag as f64 {
            return Err(Error::EmptyTempoRange {
                min: cfg.min,
                max: cfg.max,
            });
        }
        let n = novelty.len();
        // Two periods of the slowest tempo, so the longest lag still overlaps
        // half the curve.
        let needed = 2.0 * (longest + 2.0);
        if (n as f64) < needed {
            return Err(Error::InsufficientInput {
                needed: Samples((needed * hop.get() as f64).min(usize::MAX as f64) as usize),
                got: Samples(n * hop.get()),
            });
        }
        let last_lag = longest as usize;

        let (lo, hi) = novelty
            .iter()
            .fold((f32::MAX, f32::MIN), |(lo, hi), &v| (lo.min(v), hi.max(v)));
        if hi - lo <= FLAT * hi.abs().max(lo.abs()) {
            return Err(Error::NoSignal);
        }
        let mean = novelty.iter().map(|&v| f64::from(v)).sum::<f64>() / n as f64;
        let centred: Vec<f32> = novelty
            .iter()
            .map(|&v| (f64::from(v) - mean) as f32)
            .collect();
        let correlation = cross_correlate(&centred, &centred, fft);
        let power = correlation.at(0).unwrap_or(0.0) / n as f32;
        if power <= 0.0 {
            return Err(Error::NoSignal);
        }
        // Over the whole curve's power, not the overlap at the lag: a coefficient
        // in -1..=1 (Cauchy-Schwarz), where a long lag has fewer terms and
        // reads lower.
        let values = (first_lag - 1..=last_lag + 1)
            .map(|lag| correlation.at(lag as isize).unwrap_or(0.0) / n as f32 / power)
            .collect();
        Ok(Self {
            values,
            first_lag,
            last_lag,
            frame_rate,
        })
    }

    /// The tempo of lag `lag`, in BPM.
    fn bpm(&self, lag: f64) -> f64 {
        60.0 * self.frame_rate / lag
    }

    /// The normalised autocorrelation at a lag in `first_lag - 1..=last_lag + 1`
    /// (the searched lags and the one either side that refinement reads);
    /// `values[0]` is lag `first_lag - 1`.
    fn at(&self, lag: usize) -> f32 {
        debug_assert!((self.first_lag - 1..=self.last_lag + 1).contains(&lag));
        self.values[lag - (self.first_lag - 1)]
    }

    /// The prior's weight at `lag`, from 1 at the centre down.
    fn weight(&self, cfg: &TempoConfig, lag: usize) -> f32 {
        let octaves = (self.bpm(lag as f64) / cfg.centre.get()).log2() / f64::from(cfg.spread);
        (-0.5 * octaves * octaves).exp() as f32
    }

    /// The lag refined to a fraction of a frame by a parabola through its
    /// value and its neighbours', or `lag` when they do not bend down.
    fn refined(&self, lag: usize) -> f64 {
        let (a, b, c) = (self.at(lag - 1), self.at(lag), self.at(lag + 1));
        let curvature = a - 2.0 * b + c;
        if curvature >= 0.0 {
            return lag as f64;
        }
        lag as f64 + f64::from((0.5 * (a - c) / curvature).clamp(-0.5, 0.5))
    }

    fn tempo(&self, cfg: &TempoConfig) -> Tempo {
        let score = |lag: usize| self.at(lag).max(0.0) * self.weight(cfg, lag);
        // Local maxima of the weighted score; an edge of the range counts when
        // the score rises towards it.
        let mut peaks: Vec<usize> = (self.first_lag..=self.last_lag)
            .filter(|&lag| {
                (lag == self.first_lag || score(lag) > score(lag - 1))
                    && (lag == self.last_lag || score(lag) >= score(lag + 1))
            })
            .collect();
        // Best first; on a tie, the shorter lag (higher tempo).
        peaks.sort_by(|&a, &b| score(b).total_cmp(&score(a)).then(a.cmp(&b)));
        let candidate = |lag: usize| TempoCandidate {
            bpm: Bpm::new(self.bpm(self.refined(lag)))
                .unwrap_or(cfg.min)
                .clamp(cfg.min, cfg.max),
            strength: Confidence::new_clamped(self.at(lag)),
        };
        let best = candidate(peaks[0]);
        Tempo {
            bpm: best.bpm,
            confidence: best.strength,
            alternatives: peaks[1..]
                .iter()
                .take(cfg.alternatives)
                .map(|&lag| candidate(lag))
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_types::Db;

    const RATE: f64 = 44100.0;

    /// A 10 ms decaying 1 kHz burst at each of `times` (seconds), scaled by the
    /// paired gain, over `secs` seconds.
    fn burst_train(times: &[(f64, f32)], secs: f64) -> Vec<f32> {
        let mut out = vec![0.0f32; (secs * RATE) as usize];
        for &(t, gain) in times {
            let start = (t * RATE).round() as usize;
            for i in 0..441 {
                if let Some(s) = out.get_mut(start + i) {
                    let phase = 2.0 * std::f64::consts::PI * 1000.0 * i as f64 / RATE;
                    *s += gain * (phase.sin() * (-(i as f64) / 100.0).exp()) as f32;
                }
            }
        }
        out
    }

    /// Clicks at a constant tempo, the first at `offset` seconds.
    fn clicks(bpm: f64, offset: f64, secs: f64) -> Vec<f32> {
        let period = 60.0 / bpm;
        let times: Vec<_> = (0..)
            .map(|k| offset + k as f64 * period)
            .take_while(|&t| t < secs - 0.1)
            .map(|t| (t, 1.0))
            .collect();
        burst_train(&times, secs)
    }

    fn geometry() -> StftGeometry {
        TempoConfig::geometry(RATE).unwrap()
    }

    /// Positions in samples of the clicks of a train.
    fn click_samples(bpm: f64, offset: f64, secs: f64) -> Vec<f64> {
        let period = 60.0 / bpm;
        (0..)
            .map(|k| (offset + k as f64 * period) * RATE)
            .take_while(|&t| t < (secs - 0.1) * RATE)
            .collect()
    }

    /// The distance from each beat to its nearest click, in samples.
    fn miss(grid: &BeatGrid, clicks: &[f64]) -> Vec<f64> {
        grid.beats
            .iter()
            .map(|b| {
                clicks
                    .iter()
                    .map(|c| (b.get() as f64 - c).abs())
                    .fold(f64::MAX, f64::min)
            })
            .collect()
    }

    fn tempo_of(x: &[f32]) -> Tempo {
        estimate_tempo(&TempoConfig::new(), geometry(), x, &mut FftScratch::new()).unwrap()
    }

    const FRAME_RATE: f64 = RATE / 512.0;

    /// The tempo the prior should pick for a click train at `bpm`, as the
    /// tempi it may read as: the train's own and its integer divisions down to
    /// the range floor, best prior weight first, plus the runner-up when its
    /// weight is within 25% of the best: the prior cannot separate them, because the
    /// autocorrelation at the longer lag is lower by the curve's overlap and by
    /// where the period falls between frames, each worth several percent.
    fn expected_readings(bpm: f64) -> Vec<f64> {
        let weight = |b: f64| (-0.5 * (b / 120.0).log2().powi(2)).exp();
        let mut options: Vec<f64> = (1..=3)
            .map(|m| bpm / m as f64)
            .filter(|&b| b >= 60.0)
            .collect();
        options.sort_by(|a, b| weight(*b).total_cmp(&weight(*a)));
        let keep = if options.len() > 1 && weight(options[1]) > 0.75 * weight(options[0]) {
            2
        } else {
            1
        };
        options.truncate(keep);
        options
    }

    /// The error a reading may have: a third of a frame of period. The tempo is
    /// `60 * frame_rate / lag`, so a lag error `d` frames moves it by
    /// `bpm^2 * d / (60 * frame_rate)`. A parabola through a peak sampled
    /// between two frames is good to about 0.2 frames at worst (measured over
    /// the sweep below), which leaves this bound a margin of about 40%.
    fn tolerance(bpm: f64) -> f64 {
        bpm * bpm / (3.0 * 60.0 * FRAME_RATE)
    }

    /// Click trains from 60 to 200 BPM in steps of 7, and 90, 120, 128 and 174,
    /// read at their tempo, or at the octave the prior prefers, within a third
    /// of a frame of period (0.2 to 1.5 BPM across the range; the measured
    /// worst is 0.2 frames). A confidence is a coefficient: at most one, and
    /// high for a steady train.
    ///
    /// Mutation checked: skipping the parabolic refinement (`refined` returning
    /// `lag as f64`) moves several tempi out of tolerance; reading the frame
    /// rate off a fixed 1024 window instead of the hop fails every one.
    #[test]
    fn click_trains_read_at_their_tempo() {
        let mut tempi: Vec<f64> = (60..=200).step_by(7).map(f64::from).collect();
        tempi.extend([90.0, 120.0, 128.0, 174.0]);
        for bpm in tempi {
            let tempo = tempo_of(&clicks(bpm, 0.3, 20.0));
            let readings = expected_readings(bpm);
            assert!(
                readings
                    .iter()
                    .any(|&r| (tempo.bpm.get() - r).abs() <= tolerance(r)),
                "{bpm} BPM read as {}, wanted one of {readings:?}",
                tempo.bpm
            );
            assert!(tempo.confidence.get() > 0.4 && tempo.confidence.get() <= 1.0);
        }
        let strong = tempo_of(&clicks(120.0, 0.3, 20.0));
        assert!(strong.confidence.get() > 0.9, "{:?}", strong.confidence);
    }

    /// The confidence is normalised by the whole curve, so a clip short against
    /// the period reads lower than a long one of the same train.
    ///
    /// Mutation checked: dividing each lag by its overlap again (the old
    /// estimator) reads both near one and fails.
    #[test]
    fn a_short_clip_reads_less_confident() {
        let short = tempo_of(&clicks(60.0, 0.3, 4.0)).confidence.get();
        let long = tempo_of(&clicks(60.0, 0.3, 20.0)).confidence.get();
        assert!(short < 0.85 && long > 0.9 && short < long, "{short} {long}");
    }

    /// Every beat lands within one hop of a click, and no click is missed by
    /// more than the two the tracker may trim at the ends. The test grids
    /// start at 0.3 s, so the first beat is a real one.
    ///
    /// Mutation checked: dropping the frame-centre shift (`onset_lag` returning
    /// 0) puts every beat about 512 samples early and fails the hop bound.
    #[test]
    fn beats_land_within_a_hop_of_the_clicks() {
        let hop = geometry().hop().get() as f64;
        for bpm in [90.0, 120.0, 128.0, 174.0] {
            let clicks_at = click_samples(bpm, 0.3, 20.0);
            let x = clicks(bpm, 0.3, 20.0);
            let grid =
                track_beats(&TempoConfig::new(), geometry(), &x, &mut FftScratch::new()).unwrap();
            let worst = miss(&grid, &clicks_at).into_iter().fold(0.0, f64::max);
            assert!(worst <= hop, "{bpm} BPM: a beat is {worst} samples off");
            assert!(
                grid.beats.len() + 2 >= clicks_at.len(),
                "{bpm} BPM: {} beats for {} clicks",
                grid.beats.len(),
                clicks_at.len()
            );
            assert!(
                expected_readings(bpm)
                    .iter()
                    .any(|&r| (grid.bpm.get() - r).abs() <= tolerance(r)),
                "{bpm}: {}",
                grid.bpm
            );
        }
    }

    /// 90 BPM clicks searched in 40..80 BPM read as 45: the period doubled, the
    /// nearest multiple the range holds. The estimate and every alternative stay
    /// inside the range.
    ///
    /// Mutation checked: taking the lag range from the defaults (60 to 200)
    /// instead of the config returns 90 and fails; removing the final clamp
    /// leaves this test passing (the refinement never leaves the range here),
    /// which is why the clamp has a test of its own below.
    #[test]
    fn the_estimate_stays_inside_the_range() {
        let cfg = TempoConfig::new()
            .with_range(Bpm::of(40.0), Bpm::of(80.0))
            .unwrap();
        let x = clicks(90.0, 0.3, 24.0);
        let tempo = estimate_tempo(&cfg, geometry(), &x, &mut FftScratch::new()).unwrap();
        assert!((tempo.bpm.get() - 45.0).abs() < 1.0, "{}", tempo.bpm);
        for bpm in std::iter::once(tempo.bpm).chain(tempo.alternatives.iter().map(|a| a.bpm)) {
            assert!((40.0..=80.0).contains(&bpm.get()), "{bpm} is outside");
        }
    }

    /// A tempo the parabola pushes past the edge of the range is held at it:
    /// impulses 24.8 frames apart at 100 frames per second are 120.97 BPM, and
    /// the search stops at 120.
    ///
    /// Mutation checked: removing the `clamp` in `candidate` returns 120.63 and
    /// fails.
    #[test]
    fn a_refined_tempo_is_clamped_to_the_range() {
        let mut curve = vec![0.0f32; 1500];
        let mut at = 3.0;
        while (at as usize) < curve.len() {
            curve[at as usize] = 1.0;
            at += 24.8;
        }
        let cfg = TempoConfig::new()
            .with_range(Bpm::of(60.0), Bpm::of(120.0))
            .unwrap();
        let tempo =
            estimate_tempo_from_novelty(&cfg, &curve, RATE, Samples(441), &mut FftScratch::new())
                .unwrap();
        assert_eq!(tempo.bpm, Bpm::of(120.0));
    }

    /// A 60 BPM train with a half-strength click between its beats is read as
    /// 120 BPM, the octave the prior favours, with 60 among the alternatives. A
    /// 0.1-strength click is too weak to make the 120 BPM period worth its
    /// extra weight, so the read stays at 60. Centring the prior on 60, or
    /// narrowing it, settles the first case either way.
    ///
    /// Mutation checked: giving the prior a weight of one makes the first case
    /// 60 and fails; ignoring `spread` (a fixed one octave) fails the narrow
    /// case; ignoring `centre` (fixing it at 120) fails the centred case;
    /// removing `take(cfg.alternatives)` fails the zero-alternatives case.
    #[test]
    fn the_prior_settles_the_octave() {
        let train = |soft: f32| {
            let mut times: Vec<(f64, f32)> = (0..20).map(|k| (0.3 + k as f64, 1.0)).collect();
            times.extend((0..19).map(|k| (0.8 + k as f64, soft)));
            burst_train(&times, 21.0)
        };
        let mut fft = FftScratch::new();
        let half = train(0.5);

        let by_default = estimate_tempo(&TempoConfig::new(), geometry(), &half, &mut fft).unwrap();
        assert!(
            (by_default.bpm.get() - 120.0).abs() < 1.5,
            "{}",
            by_default.bpm
        );
        assert!(
            by_default
                .alternatives
                .iter()
                .any(|a| (a.bpm.get() - 60.0).abs() < 1.5),
            "{:?}",
            by_default.alternatives
        );

        let weak = estimate_tempo(&TempoConfig::new(), geometry(), &train(0.1), &mut fft).unwrap();
        assert!((weak.bpm.get() - 60.0).abs() < 1.5, "{}", weak.bpm);

        let centred = TempoConfig::new().with_prior(Bpm::of(60.0), 1.0).unwrap();
        let low = estimate_tempo(&centred, geometry(), &half, &mut fft).unwrap();
        assert!((low.bpm.get() - 60.0).abs() < 1.5, "{}", low.bpm);

        let narrow = TempoConfig::new().with_prior(Bpm::of(120.0), 0.1).unwrap();
        let high = estimate_tempo(&narrow, geometry(), &train(0.1), &mut fft).unwrap();
        assert!((high.bpm.get() - 120.0).abs() < 1.5, "{}", high.bpm);

        let none = TempoConfig::new().with_alternatives(0);
        let bare = estimate_tempo(&none, geometry(), &half, &mut fft).unwrap();
        assert!(bare.alternatives.is_empty());
    }

    /// Extra loud clicks 0.2 s after every third beat pull an untethered
    /// tracker off the grid and leave a tight one on it.
    ///
    /// Mutation checked: removing the gap penalty (`penalty` all zero) puts a
    /// beat on a distractor and fails the tight case; the loose case, tightness
    /// zero, then has to be off the grid, which pins the knob's direction.
    #[test]
    fn tightness_holds_the_grid_against_distractors() {
        let mut times: Vec<(f64, f32)> = (0..38).map(|k| (0.3 + 0.5 * k as f64, 1.0)).collect();
        times.extend((0..12).map(|k| (0.5 + 1.5 * k as f64, 1.5)));
        let x = burst_train(&times, 20.0);
        let grid_clicks = click_samples(120.0, 0.3, 20.0);
        let hop = geometry().hop().get() as f64;
        let run = |tightness: f32| {
            let cfg = TempoConfig::new().with_tightness(tightness).unwrap();
            track_beats(&cfg, geometry(), &x, &mut FftScratch::new()).unwrap()
        };
        let tight = miss(&run(100.0), &grid_clicks);
        assert!(tight.iter().all(|&m| m <= hop), "{tight:?}");
        let loose = miss(&run(0.0), &grid_clicks);
        assert!(loose.iter().any(|&m| m > hop), "{loose:?}");
    }

    /// Silence has no tempo, nor does a level that never changes, nor does a
    /// curve with no spread.
    ///
    /// Mutation checked: removing the `FLAT` test lets the constant curve
    /// through on rounding residue; not zeroing the first frame lets the
    /// constant input through on its onset spike.
    #[test]
    fn a_signal_without_change_has_no_tempo() {
        let cfg = TempoConfig::new();
        let mut fft = FftScratch::new();
        for x in [vec![0.0f32; 441_000], vec![0.5f32; 441_000]] {
            assert_eq!(
                estimate_tempo(&cfg, geometry(), &x, &mut fft),
                Err(Error::NoSignal)
            );
            assert_eq!(
                track_beats(&cfg, geometry(), &x, &mut fft),
                Err(Error::NoSignal)
            );
        }
        // A constant, and a constant with one ulp of rounding on every 7th value.
        let flat = vec![0.3f32; 2000];
        let mut ulp = vec![1.0f32; 2000];
        for v in ulp.iter_mut().step_by(7) {
            *v += f32::EPSILON;
        }
        for curve in [flat, ulp] {
            assert_eq!(
                estimate_tempo_from_novelty(&cfg, &curve, RATE, Samples(512), &mut fft),
                Err(Error::NoSignal)
            );
        }
    }

    /// A NaN or infinity in the input is refused, in samples or in a curve.
    ///
    /// Mutation checked: removing the sample check lets the NaN through
    /// (spectral flux's `max(0)` drops it); removing the curve check fails the
    /// curve cases.
    #[test]
    fn non_finite_input_is_refused() {
        let cfg = TempoConfig::new();
        let mut fft = FftScratch::new();
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let mut x = clicks(120.0, 0.3, 10.0);
            x[20_000] = bad;
            assert_eq!(
                estimate_tempo(&cfg, geometry(), &x, &mut fft),
                Err(Error::NonFiniteInput)
            );
            assert_eq!(
                track_beats(&cfg, geometry(), &x, &mut fft),
                Err(Error::NonFiniteInput)
            );
            let mut curve = vec![0.0f32; 1000];
            curve[10] = 1.0;
            curve[500] = bad;
            assert_eq!(
                estimate_tempo_from_novelty(&cfg, &curve, RATE, Samples(512), &mut fft),
                Err(Error::NonFiniteInput)
            );
            assert_eq!(
                track_beats_from_novelty(
                    &cfg,
                    &curve,
                    RATE,
                    Samples(512),
                    Some(Bpm::of(120.0)),
                    &mut fft
                ),
                Err(Error::NonFiniteInput)
            );
        }
    }

    /// The tracker follows a tempo that rises from 120 to 122 BPM over the clip,
    /// where a fixed grid at the mean tempo would end 1800 samples out; the
    /// estimate is the mean.
    ///
    /// Mutation checked: scaling the penalty by 1000 (a rigid grid) fails the
    /// hop bound.
    #[test]
    fn a_slow_drift_is_followed() {
        let mut t = 0.3;
        let mut times = Vec::new();
        while t < 19.9 {
            times.push((t, 1.0f32));
            t += 60.0 / (120.0 + 2.0 * t / 20.0);
        }
        let x = burst_train(&times, 20.0);
        let truth: Vec<f64> = times.iter().map(|&(t, _)| t * RATE).collect();
        let cfg = TempoConfig::new();
        let mut fft = FftScratch::new();
        let grid = track_beats(&cfg, geometry(), &x, &mut fft).unwrap();
        let hop = geometry().hop().get() as f64;
        let worst = miss(&grid, &truth).into_iter().fold(0.0, f64::max);
        assert!(worst <= hop, "a beat is {worst} samples off");
        assert!((grid.bpm.get() - 121.0).abs() < 1.5, "{}", grid.bpm);
    }

    /// The tempo, confidence and beats do not depend on the signal's level.
    ///
    /// Mutation checked: dropping the division by the standard deviation in
    /// `local_score` changes the beats at 0.01 (the penalty outweighs a
    /// quiet curve) and fails; dropping the division by `power` changes the
    /// confidence.
    #[test]
    fn the_result_does_not_depend_on_gain() {
        let x = clicks(128.0, 0.3, 20.0);
        let cfg = TempoConfig::new();
        let mut fft = FftScratch::new();
        let reference = track_beats(&cfg, geometry(), &x, &mut fft).unwrap();
        let tempo = estimate_tempo(&cfg, geometry(), &x, &mut fft).unwrap();
        for gain in [0.01f32, 8.0] {
            let y: Vec<f32> = x.iter().map(|s| s * gain).collect();
            let other = estimate_tempo(&cfg, geometry(), &y, &mut fft).unwrap();
            assert!((other.bpm.get() - tempo.bpm.get()).abs() < 1e-3);
            assert!((other.confidence.get() - tempo.confidence.get()).abs() < 1e-3);
            let grid = track_beats(&cfg, geometry(), &y, &mut fft).unwrap();
            assert_eq!(grid.beats, reference.beats, "gain {gain}");
        }
    }

    /// An impulse every 40 frames at 100 frames per second is 0.4 s: exactly
    /// 150 BPM. The beats sit on the impulses, `frame * hop` samples in.
    ///
    /// Mutation checked: positioning a beat at `frame` rather than
    /// `frame * hop` fails the spacing; computing the frame rate as
    /// `sample_rate * hop` fails the tempo.
    #[test]
    fn a_caller_curve_is_read_in_its_own_frames() {
        let mut curve = vec![0.0f32; 1000];
        for i in (0..1000).step_by(40) {
            curve[i] = 1.0;
        }
        let cfg = TempoConfig::new();
        let mut fft = FftScratch::new();
        let tempo =
            estimate_tempo_from_novelty(&cfg, &curve, RATE, Samples(441), &mut fft).unwrap();
        assert!((tempo.bpm.get() - 150.0).abs() < 0.01, "{}", tempo.bpm);

        let grid =
            track_beats_from_novelty(&cfg, &curve, RATE, Samples(441), None, &mut fft).unwrap();
        assert!(grid.beats.len() >= 20);
        assert!(grid.beats.iter().all(|b| b.get() % (40 * 441) == 0));
        assert!(
            grid.beats
                .windows(2)
                .all(|w| w[1].get() - w[0].get() == 40 * 441)
        );
        assert!((grid.bpm.get() - 150.0).abs() < 0.01);
        assert_eq!(
            grid.time(1, RATE),
            Some(Seconds(0.4 * (grid.beats[1].get() / (40 * 441)) as f64))
        );
        assert_eq!(grid.time(grid.beats.len(), RATE), None);

        // A supplied tempo is used as given, even at 100 BPM against a curve
        // that holds 150: the beats follow the period, not the curve.
        let forced = track_beats_from_novelty(
            &cfg,
            &curve,
            RATE,
            Samples(441),
            Some(Bpm::of(100.0)),
            &mut fft,
        )
        .unwrap();
        assert_eq!(forced.bpm, Bpm::of(100.0));
    }

    /// Unstructured noise has no clear tempo: the confidence is low. White
    /// novelty over about 860 frames has an autocorrelation coefficient of
    /// standard deviation `1 / sqrt(860)`, 0.034, at each lag beyond the
    /// window's overlap; the strongest of the 60 searched lags is under
    /// four deviations (0.14), so a bound of 0.3 is eight deviations out and
    /// does not rest on the seed.
    ///
    /// Mutation checked: reporting the weighted score's maximum scaled to one
    /// (or a constant) as the confidence fails the bound.
    #[test]
    fn noise_has_a_low_confidence() {
        use rand::{Rng, SeedableRng, rngs::StdRng};
        let mut rng = StdRng::seed_from_u64(7);
        let x: Vec<f32> = (0..441_000).map(|_| rng.gen_range(-0.5..0.5)).collect();
        let tempo = tempo_of(&x);
        assert!(tempo.confidence.get() < 0.3, "{:?}", tempo.confidence);
    }

    /// Settings and inputs that cannot work say so.
    ///
    /// Mutation checked: loosening each comparison in `with_range`,
    /// `with_prior`, `with_tightness`, `frame_rate`, `Periodicity::new` fails
    /// its case here.
    #[test]
    fn unusable_settings_are_refused() {
        let cfg = TempoConfig::new();
        let mut fft = FftScratch::new();
        for (min, max) in [(100.0, 100.0), (120.0, 90.0)] {
            assert!(matches!(
                cfg.with_range(Bpm::of(min), Bpm::of(max)),
                Err(Error::EmptyTempoRange { .. })
            ));
        }
        // A zero, negative, NaN or infinite bound, or a centre, is not a
        // `Bpm`: the config is never handed one.
        for raw in [0.0, -5.0, f64::NAN, f64::INFINITY] {
            assert!(Bpm::new(raw).is_none(), "{raw}");
        }
        assert!(cfg.with_prior(Bpm::of(120.0), 0.0).is_err());
        assert!(cfg.with_prior(Bpm::of(120.0), f32::NAN).is_err());
        assert!(cfg.with_tightness(-1.0).is_err());
        assert!(cfg.with_tightness(f32::NAN).is_err());
        assert!(cfg.with_tightness(0.0).is_ok());
        assert!(TempoConfig::geometry(0.0).is_err());

        let curve = vec![0.0f32; 1000];
        assert_eq!(
            estimate_tempo_from_novelty(&cfg, &curve, RATE, Samples(0), &mut fft),
            Err(Error::ZeroHop)
        );
        assert_eq!(
            estimate_tempo_from_novelty(&cfg, &curve, 0.0, Samples(512), &mut fft),
            Err(Error::NonPositiveSampleRate)
        );
        // Under one window, and under two periods of the slowest tempo.
        assert!(matches!(
            estimate_tempo(&cfg, geometry(), &[0.0; 100], &mut fft),
            Err(Error::InsufficientInput { .. })
        ));
        let mut short = vec![0.0f32; 100];
        short[3] = 1.0;
        assert!(matches!(
            estimate_tempo_from_novelty(&cfg, &short, RATE, Samples(512), &mut fft),
            Err(Error::InsufficientInput { .. })
        ));
        // At a hop this wide no whole-frame lag falls in 100 to 101 BPM.
        let narrow = cfg.with_range(Bpm::of(100.0), Bpm::of(101.0)).unwrap();
        let mut long = vec![0.0f32; 4000];
        long[3] = 1.0;
        assert!(matches!(
            estimate_tempo_from_novelty(&narrow, &long, RATE, Samples(512), &mut fft),
            Err(Error::EmptyTempoRange { .. })
        ));
    }

    /// A window much wider than the hop still puts beats within one hop of the
    /// clicks: at 4096/512 flux peaks `3 * 4096 / 4 - 256 = 2816` samples after
    /// a frame starts, where the old `window / 2` was 768 samples off. The
    /// bound is one hop: half for placing the peak on the frame grid, half for
    /// the click's own width (its energy is within the first 300 samples, and
    /// an onset's rise can peak a few hundred samples late at a window this
    /// wide).
    ///
    /// Mutation checked: `onset_lag` returning `window / 2` puts every beat
    /// 768 samples early and fails.
    #[test]
    fn a_wide_window_still_places_beats_within_a_hop() {
        let wide = StftGeometry::new(RATE, Samples(4096), Samples(512)).unwrap();
        let clicks_at = click_samples(120.0, 0.3, 20.0);
        let grid = track_beats(
            &TempoConfig::new(),
            wide,
            &clicks(120.0, 0.3, 20.0),
            &mut FftScratch::new(),
        )
        .unwrap();
        let worst = miss(&grid, &clicks_at).into_iter().fold(0.0, f64::max);
        assert!(worst <= 512.0, "a beat is {worst} samples off");
        assert!(grid.beats.len() + 2 >= clicks_at.len());
    }

    /// Near-silence has no tempo even though flux is relative: a click train
    /// at -100 dBFS is refused, at -80 dBFS it reads.
    ///
    /// Mutation checked: removing the floor reads the -100 dBFS train;
    /// raising it to -60 dBFS refuses the -80 dBFS one.
    #[test]
    fn near_silence_has_no_tempo() {
        let quiet = |db: f32| -> Vec<f32> {
            let gain = Db(db).to_amplitude().get();
            clicks(120.0, 0.3, 20.0).iter().map(|s| s * gain).collect()
        };
        let cfg = TempoConfig::new();
        let mut fft = FftScratch::new();
        assert_eq!(
            estimate_tempo(&cfg, geometry(), &quiet(-100.0), &mut fft),
            Err(Error::NoSignal)
        );
        assert_eq!(
            track_beats(&cfg, geometry(), &quiet(-100.0), &mut fft),
            Err(Error::NoSignal)
        );
        let tempo = estimate_tempo(&cfg, geometry(), &quiet(-80.0), &mut fft).unwrap();
        assert!((tempo.bpm.get() - 120.0).abs() < 1.5);
    }

    /// A supplied tempo whose period no curve can hold is an
    /// `InvalidTempoParameter`, not an allocation of the period's size and not
    /// an empty grid; a usable one tracks.
    ///
    /// Mutation checked: removing the period check lets `1e-6` BPM reach the
    /// tracker (the kernel table is sized by the period) and `1e12` return an
    /// empty grid; each case here fails.
    #[test]
    fn a_supplied_tempo_must_fit_the_curve() {
        let mut curve = vec![0.0f32; 1000];
        for i in (0..1000).step_by(40) {
            curve[i] = 1.0;
        }
        let cfg = TempoConfig::new();
        let mut fft = FftScratch::new();
        let hop = Samples(441);
        // 100 frames per second: a curve of 1000 frames spans 10 s, so the
        // slowest tempo is 6 BPM (a period of the whole curve) and the fastest
        // 3000 (a period of two frames).
        for bad in [1e-6, 5.0, 3001.0, 1e12] {
            assert_eq!(
                track_beats_from_novelty(&cfg, &curve, RATE, hop, Some(Bpm::of(bad)), &mut fft),
                Err(Error::InvalidTempoParameter("tempo")),
                "{bad}"
            );
        }
        for good in [6.0, 150.0, 3000.0] {
            assert!(
                track_beats_from_novelty(&cfg, &curve, RATE, hop, Some(Bpm::of(good)), &mut fft)
                    .is_ok(),
                "{good}"
            );
        }
    }

    /// A range too slow or too fast to be a tempo is refused when it is set,
    /// and one the curve cannot hold is refused when it is read, not
    /// overflowed.
    ///
    /// Mutation checked: lowering `MIN_BPM` to `1e-300` lets the first case
    /// through; raising `MAX_BPM` to infinity lets the second through.
    #[test]
    fn an_absurd_range_is_refused() {
        let cfg = TempoConfig::new();
        for (min, max) in [(1e-300, 90.0), (0.5, 90.0), (60.0, 1e9), (60.0, 1001.0)] {
            assert!(
                matches!(
                    cfg.with_range(Bpm::of(min), Bpm::of(max)),
                    Err(Error::EmptyTempoRange { .. })
                ),
                "{min}..{max}"
            );
        }
        // The slowest range allowed needs 2 * (5168 + 2) frames of curve.
        let slow = cfg.with_range(Bpm::of(1.0), Bpm::of(200.0)).unwrap();
        let curve: Vec<f32> = (0..2000).map(|i| (i % 43) as f32).collect();
        assert!(matches!(
            estimate_tempo_from_novelty(&slow, &curve, RATE, Samples(512), &mut FftScratch::new()),
            Err(Error::InsufficientInput { .. })
        ));
    }

    /// The alternatives are capped.
    ///
    /// Mutation checked: removing the `min` in `with_alternatives` lets
    /// the count through unclipped and fails.
    #[test]
    fn the_alternative_count_is_capped() {
        let cfg = TempoConfig::new().with_alternatives(usize::MAX);
        assert_eq!(cfg.alternatives, MAX_ALTERNATIVES);
    }
}
