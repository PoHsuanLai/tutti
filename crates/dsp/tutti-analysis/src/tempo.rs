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
//! 11.6 ms at 44.1 kHz. Beats sit on the hop grid at frame centres, so each is
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

use tutti_types::{Bpm, Confidence, SampleRate, Samples, Seconds};

use crate::beat;
use crate::error::{Error, Result};
use crate::fft::FftScratch;
use crate::geometry::StftGeometry;
use crate::onset::{DetectionFunction, OnsetConfig, OnsetState, step_onset};
use crate::xcorr::cross_correlate;

/// The default analysis window, in samples.
const WINDOW: usize = 1024;
/// The default hop, in samples.
const HOP: usize = 512;

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
    function: DetectionFunction,
}

impl Default for TempoConfig {
    /// 60 to 200 BPM, a prior centred on 120 BPM one octave wide, a tightness
    /// of 100, two alternatives and spectral flux.
    fn default() -> Self {
        Self {
            min: Bpm(60.0),
            max: Bpm(200.0),
            centre: Bpm(120.0),
            spread: 1.0,
            tightness: 100.0,
            alternatives: 2,
            function: DetectionFunction::SpectralFlux,
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
    /// the range.
    ///
    /// # Errors
    ///
    /// [`Error::EmptyTempoRange`] unless `0 < min < max` and both are finite.
    pub fn with_range(mut self, min: impl Into<Bpm>, max: impl Into<Bpm>) -> Result<Self> {
        let (min, max) = (min.into(), max.into());
        let ok = |b: Bpm| b.get().is_finite() && b.get() > 0.0;
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
    /// [`Error::InvalidTempoParameter`] unless `centre` and `spread` are
    /// finite and above zero.
    pub fn with_prior(mut self, centre: impl Into<Bpm>, spread_octaves: f32) -> Result<Self> {
        let centre = centre.into();
        if !(centre.get().is_finite() && centre.get() > 0.0) {
            return Err(Error::InvalidTempoParameter("prior centre"));
        }
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

    /// How many runner-up tempi [`Tempo::alternatives`] holds, at most.
    #[must_use]
    pub fn with_alternatives(mut self, count: usize) -> Self {
        self.alternatives = count;
        self
    }

    /// The detection function the audio entry points take novelty from.
    #[must_use]
    pub fn with_function(mut self, function: DetectionFunction) -> Self {
        self.function = function;
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
    /// How periodic the novelty is at that tempo: its normalised
    /// autocorrelation there, from zero for no repetition to one for an exact
    /// one. A reading of periodicity, not a probability, and blind to octave
    /// ambiguity: a train that repeats at 60 and 120 BPM reads high for both,
    /// so check [`alternatives`](Self::alternatives) when the octave matters.
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
            .map(|b| Seconds((b.get() as f64 / sample_rate.into().get()) as f32))
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
/// for silence or any input whose novelty is flat; [`Error::InsufficientInput`]
/// when the input is shorter than one window, or holds fewer than two periods
/// of the slowest tempo; [`Error::EmptyTempoRange`] when no whole-frame lag
/// falls in the range at this hop.
pub fn estimate_tempo(
    cfg: &TempoConfig,
    geometry: StftGeometry,
    samples: &[f32],
    fft: &mut FftScratch,
) -> Result<Tempo> {
    let novelty = novelty(cfg, geometry, samples, fft)?;
    estimate_tempo_from_novelty(cfg, &novelty, geometry.sample_rate(), geometry.hop(), fft)
}

/// Estimates the tempo from a novelty curve of one value per `hop` samples.
///
/// For a caller's own detection function. The curve is read as strength per
/// frame, and only its shape matters: any positive gain gives the same
/// result.
///
/// # Errors
///
/// As [`estimate_tempo`], with [`Error::NonFiniteInput`] for a NaN or infinite
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
/// [`track_beats_from_novelty`]; each beat is the centre of its analysis frame,
/// `frame * hop + window / 2`.
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
    let novelty = novelty(cfg, geometry, samples, fft)?;
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
/// [`Error::NonFiniteInput`] for a NaN or infinite value in `novelty`, or for a
/// tempo that is not finite and above zero; a flat curve gives no beats.
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
            if !(bpm.get().is_finite() && bpm.get() > 0.0) || novelty.iter().any(|v| !v.is_finite())
            {
                return Err(Error::NonFiniteInput);
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

/// Where a beat sits in the frame whose novelty it raises: the frame's centre.
fn onset_lag(geometry: StftGeometry) -> usize {
    geometry.window().get() / 2
}

/// The detection curve of `samples`, with the first frame zeroed.
fn novelty(
    cfg: &TempoConfig,
    geometry: StftGeometry,
    samples: &[f32],
    fft: &mut FftScratch,
) -> Result<Vec<f32>> {
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
    let onset = OnsetConfig::new(geometry, cfg.function);
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
    /// The lag of `values[1]`: the first lag inside the tempo range.
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
        let last_lag = (60.0 * frame_rate / cfg.min.get()).floor() as usize;
        if last_lag < first_lag {
            return Err(Error::EmptyTempoRange {
                min: cfg.min,
                max: cfg.max,
            });
        }
        let n = novelty.len();
        // Two periods of the slowest tempo, so the longest lag still overlaps
        // half the curve.
        let needed = 2 * (last_lag + 2);
        if n < needed {
            return Err(Error::InsufficientInput {
                needed: Samples(needed * hop.get()),
                got: Samples(n * hop.get()),
            });
        }

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
        // Divided by the overlap, so a long lag is not penalised for having
        // fewer terms.
        let values = (first_lag - 1..=last_lag + 1)
            .map(|lag| {
                let overlap = (n - lag) as f32;
                correlation.at(lag as isize).unwrap_or(0.0) / overlap / power
            })
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

    /// The normalised autocorrelation at an in-range or neighbouring lag.
    fn at(&self, lag: usize) -> f32 {
        self.values[lag + 1 - self.first_lag]
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
            bpm: Bpm(self
                .bpm(self.refined(lag))
                .clamp(cfg.min.get(), cfg.max.get())),
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

    /// Click trains read at their own tempo, within 1.5 BPM: the hop is 512
    /// samples, so the period is a whole number of frames to within a
    /// parabola's reach, and the error grows with tempo (about 1.1 BPM at 174).
    /// 120 BPM reads as strongly periodic.
    ///
    /// Mutation checked: skipping the parabolic refinement (`refined` returning
    /// `lag as f64`) moves cases out of tolerance, as does reading the frame
    /// rate off a fixed 1024 window instead of the hop.
    #[test]
    fn click_trains_read_at_their_tempo() {
        for bpm in [90.0, 120.0, 128.0, 174.0] {
            let tempo = tempo_of(&clicks(bpm, 0.3, 20.0));
            assert!(
                (tempo.bpm.get() - bpm).abs() < 1.5,
                "{bpm} BPM read as {}",
                tempo.bpm
            );
        }
        let strong = tempo_of(&clicks(120.0, 0.3, 20.0));
        assert!(strong.confidence.get() > 0.9, "{:?}", strong.confidence);
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
            assert!((grid.bpm.get() - bpm).abs() < 1.5);
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
        let cfg = TempoConfig::new().with_range(Bpm(40.0), Bpm(80.0)).unwrap();
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
            .with_range(Bpm(60.0), Bpm(120.0))
            .unwrap();
        let tempo =
            estimate_tempo_from_novelty(&cfg, &curve, RATE, Samples(441), &mut FftScratch::new())
                .unwrap();
        assert_eq!(tempo.bpm, Bpm(120.0));
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

        let centred = TempoConfig::new().with_prior(Bpm(60.0), 1.0).unwrap();
        let low = estimate_tempo(&centred, geometry(), &half, &mut fft).unwrap();
        assert!((low.bpm.get() - 60.0).abs() < 1.5, "{}", low.bpm);

        let narrow = TempoConfig::new().with_prior(Bpm(120.0), 0.1).unwrap();
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
                    Some(Bpm(120.0)),
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
            Some(Seconds(0.4 * (grid.beats[1].get() / (40 * 441)) as f32))
        );
        assert_eq!(grid.time(grid.beats.len(), RATE), None);

        // A supplied tempo is used as given, even at 100 BPM against a curve
        // that holds 150: the beats follow the period, not the curve.
        let forced =
            track_beats_from_novelty(&cfg, &curve, RATE, Samples(441), Some(Bpm(100.0)), &mut fft)
                .unwrap();
        assert_eq!(forced.bpm, Bpm(100.0));
    }

    /// Unstructured noise has no clear tempo: the confidence is low.
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
        for (min, max) in [(100.0, 100.0), (120.0, 90.0), (0.0, 90.0), (-5.0, 90.0)] {
            assert!(matches!(
                cfg.with_range(Bpm(min), Bpm(max)),
                Err(Error::EmptyTempoRange { .. })
            ));
        }
        assert!(cfg.with_range(Bpm(f64::NAN), Bpm(90.0)).is_err());
        assert!(cfg.with_range(Bpm(60.0), Bpm(f64::INFINITY)).is_err());
        assert!(cfg.with_prior(Bpm(0.0), 1.0).is_err());
        assert!(cfg.with_prior(Bpm(120.0), 0.0).is_err());
        assert!(cfg.with_prior(Bpm(120.0), f32::NAN).is_err());
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
        let narrow = cfg.with_range(Bpm(100.0), Bpm(101.0)).unwrap();
        let mut long = vec![0.0f32; 4000];
        long[3] = 1.0;
        assert!(matches!(
            estimate_tempo_from_novelty(&narrow, &long, RATE, Samples(512), &mut fft),
            Err(Error::EmptyTempoRange { .. })
        ));
    }
}
