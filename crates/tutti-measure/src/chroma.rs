//! Chroma: the energy of a signal folded into the twelve pitch classes.
//!
//! The harmonic pitch class profile of Gomez (2006). Each frame of a
//! [`StftGeometry`] is windowed and transformed, the local maxima of the
//! magnitude spectrum between [`min_freq`](ChromaConfig::min_freq) and
//! [`max_freq`](ChromaConfig::max_freq) are located to a fraction of a bin by
//! parabolic interpolation of the log magnitude, and each peak's energy (its
//! squared amplitude) is added to the pitch class of its frequency and of its
//! subharmonics `f/2 .. f/n`, each weighted by [`harmonic_decay`] to the power
//! of its order less one. A peak thereby also votes for the notes it may be
//! a partial of. A subharmonic below the band's floor is not a candidate
//! fundamental and does not vote.
//!
//! Column `k` of a frame is `PitchClass::ALL[k]`, so column 0 is C. A frame is
//! divided by its largest column, so each reads `0..=1` with a peak of exactly
//! 1: a plain `f32` rather than a unit, because no unit covers a relative
//! energy and `Confidence` is an estimator's certainty. A frame whose strongest
//! in-band partial is under -90 dBFS holds no pitch and reads all zero,
//! never NaN; the floor is absolute because a relative one would call the
//! rounding noise of a silent buffer a signal. Within a frame, a peak under
//! [`peak_floor`](ChromaConfig::peak_floor) of the frame's strongest is
//! ignored, which keeps window sidelobes out of the profile.
//!
//! # Tuning
//!
//! A peak `f` is `s = 12 log2(f / 440)` semitones from A4; its distance from
//! the nearest equal-tempered pitch is `s - round(s)`. The tuning is the
//! energy-weighted circular mean of that distance (a full turn per semitone),
//! in [`Cents`] in `-50..=50`: a recording that sits 30 cents sharp reads +30,
//! and folding subtracts it, so its notes land on their own classes. It is
//! the estimate over the whole buffer ([`chroma`] runs a first pass to find
//! it), or a [fixed](Tuning::Fixed) value. A peak is folded to the nearest
//! class: a signal tuned near a quarter tone is ambiguous, whatever the
//! estimate.
//!
//! The partials of a harmonic tone are not all on the grid: the third sits 2
//! cents sharp of an equal-tempered fifth, the fifth 14 cents flat of a major
//! third. The estimate is therefore a few cents off for a tone rich in
//! partials, which only matters near the quarter-tone ambiguity.
//!
//! # Streaming
//!
//! [`step_chroma`] folds one frame at a time into a [`ChromaState`] that keeps
//! a running mean. Its buffers are built when the state adopts a config, so a
//! step allocates nothing after the first, but the first transform of an
//! [`FftScratch`] plans lazily. With [`Tuning::Estimate`] a step can only use
//! the peaks seen so far, so early frames fold with a rougher tuning than
//! [`chroma`]'s whole-buffer one; with a fixed tuning the two agree to the bit.
//!
//! [`harmonic_decay`]: ChromaConfig::harmonic_decay

use tutti_types::{Cents, Db, Hz, Note, Samples, Semitones};

use crate::{Error, Result};
use tutti_dsp::Complex;
use tutti_dsp::FftScratch;
use tutti_dsp::StftGeometry;
use tutti_dsp::{BinCount, Grid};

/// The pitch classes in a chroma frame.
pub(crate) const CLASSES: usize = 12;

/// The smallest and largest window accepted. The FFT works on the window, so
/// an absurd one would ask for gigabytes, and a tiny one resolves no pitch.
const MIN_WINDOW: usize = 64;
const MAX_WINDOW: usize = 1 << 18;

/// The most frames one call analyses, bounding the output (48 bytes a frame).
pub(crate) const MAX_FRAMES: usize = 1 << 19;

/// The most hops in one window. Each frame is a whole transform, so this
/// bounds the cost of a tiny hop.
const MAX_OVERLAP: usize = 64;

/// The most subharmonics one peak votes for.
const MAX_HARMONICS: usize = 16;

/// A frame whose strongest in-band partial is below this holds no pitch, as in
/// note transcription.
const SILENCE_FLOOR: Db = Db(-90.0);

/// The lowest `peak_floor` accepted: the f32 range of a magnitude ratio.
const MIN_PEAK_FLOOR: Db = Db(-120.0);

/// Smallest positive magnitude, standing in for an empty neighbour bin before
/// its logarithm is taken.
const TINY: f64 = 1e-30;

/// How the tuning a frame is folded with is found.
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub enum Tuning {
    /// The circular mean of the peaks' distance from the equal-tempered grid.
    Estimate,
    /// This offset from A4 = 440 Hz, within `-50..=50` cents.
    Fixed(Cents),
}

/// What [`chroma`] and [`step_chroma`] fold with. Immutable once built.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ChromaConfig {
    geometry: StftGeometry,
    min_freq: Hz,
    max_freq: Hz,
    harmonics: usize,
    harmonic_decay: f32,
    peak_floor: Db,
    tuning: Tuning,
}

impl ChromaConfig {
    /// Folds the peaks of `geometry`'s frames between 100 Hz and 5 kHz (or
    /// Nyquist, if lower), each voting for itself and three subharmonics
    /// weighted `0.6^(order - 1)`, ignoring peaks 40 dB under the frame's
    /// strongest, with an estimated tuning.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidTonalParameter`] for a window under 64 or over 262144
    /// samples, or a hop under a sixty-fourth of the window;
    /// [`Error::EmptyFrequencyRange`] when the band holds fewer than three bins.
    pub fn new(geometry: StftGeometry) -> Result<Self> {
        let window = geometry.window().get();
        if !(MIN_WINDOW..=MAX_WINDOW).contains(&window) {
            return Err(Error::InvalidTonalParameter("window: outside 64..=262144"));
        }
        if window.div_ceil(geometry.hop().get()) > MAX_OVERLAP {
            return Err(Error::InvalidTonalParameter(
                "hop: more than 64 frames per window",
            ));
        }
        let nyquist = geometry.sample_rate().get() as f32 / 2.0;
        let config = Self {
            geometry,
            min_freq: Hz(100.0),
            max_freq: Hz(5000.0f32.min(nyquist)),
            harmonics: 4,
            harmonic_decay: 0.6,
            peak_floor: Db(-40.0),
            tuning: Tuning::Estimate,
        };
        config.band()?;
        Ok(config)
    }

    /// Folds only peaks from `min_freq` to `max_freq`.
    ///
    /// # Errors
    ///
    /// [`Error::EmptyFrequencyRange`] when a bound is not finite and positive,
    /// `min_freq` is not below `max_freq`, or fewer than three bins fall in it;
    /// [`Error::AboveNyquist`] when `max_freq` exceeds half the sample rate.
    pub fn with_band(mut self, min_freq: impl Into<Hz>, max_freq: impl Into<Hz>) -> Result<Self> {
        (self.min_freq, self.max_freq) = (min_freq.into(), max_freq.into());
        self.band()?;
        Ok(self)
    }

    /// How many partials a peak may be, itself included: 1 folds a peak to its
    /// own class only.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidTonalParameter`] unless within `1..=16`.
    pub fn with_harmonics(mut self, harmonics: usize) -> Result<Self> {
        if !(1..=MAX_HARMONICS).contains(&harmonics) {
            return Err(Error::InvalidTonalParameter("harmonics: outside 1..=16"));
        }
        self.harmonics = harmonics;
        Ok(self)
    }

    /// The weight of a subharmonic, per order past the first.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidTonalParameter`] unless finite and in `(0, 1]`.
    pub fn with_harmonic_decay(mut self, decay: f32) -> Result<Self> {
        if !(decay > 0.0 && decay <= 1.0) {
            return Err(Error::InvalidTonalParameter(
                "harmonic decay: outside (0, 1]",
            ));
        }
        self.harmonic_decay = decay;
        Ok(self)
    }

    /// Peaks this far under the frame's strongest are ignored.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidTonalParameter`] unless finite and within `-120..=0` dB.
    pub fn with_peak_floor(mut self, floor: Db) -> Result<Self> {
        if !(floor.get() >= MIN_PEAK_FLOOR.get() && floor.get() <= 0.0) {
            return Err(Error::InvalidTonalParameter(
                "peak floor: outside -120..=0 dB",
            ));
        }
        self.peak_floor = floor;
        Ok(self)
    }

    /// Folds with a fixed or an estimated tuning.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidTonalParameter`] for a fixed offset that is not finite or
    /// is outside `-50..=50` cents.
    pub fn with_tuning(mut self, tuning: Tuning) -> Result<Self> {
        if let Tuning::Fixed(cents) = tuning
            && (cents.get().is_nan() || cents.get().abs() > 50.0)
        {
            return Err(Error::InvalidTonalParameter(
                "tuning: outside -50..=50 cents",
            ));
        }
        self.tuning = tuning;
        Ok(self)
    }

    /// The window, hop and sample rate frames are taken at.
    #[inline]
    #[must_use]
    pub fn geometry(&self) -> StftGeometry {
        self.geometry
    }

    /// The lowest peak frequency folded.
    #[inline]
    #[must_use]
    pub fn min_freq(&self) -> Hz {
        self.min_freq
    }

    /// The highest peak frequency folded.
    #[inline]
    #[must_use]
    pub fn max_freq(&self) -> Hz {
        self.max_freq
    }

    /// How many partials a peak may be.
    #[inline]
    #[must_use]
    pub fn harmonics(&self) -> usize {
        self.harmonics
    }

    /// The weight of a subharmonic per order past the first.
    #[inline]
    #[must_use]
    pub fn harmonic_decay(&self) -> f32 {
        self.harmonic_decay
    }

    /// How far under the frame's strongest a peak may be and still count.
    #[inline]
    #[must_use]
    pub fn peak_floor(&self) -> Db {
        self.peak_floor
    }

    /// How the tuning is found.
    #[inline]
    #[must_use]
    pub fn tuning(&self) -> Tuning {
        self.tuning
    }

    /// The bins a peak may sit on, `lo..=hi`, each with both neighbours inside
    /// the half spectrum; an error unless the band is valid and holds three.
    fn band(&self) -> Result<(usize, usize)> {
        let (min, max) = (self.min_freq, self.max_freq);
        let empty = Error::EmptyFrequencyRange { min, max };
        if !(min.get().is_finite() && max.get().is_finite() && min.get() > 0.0 && min < max) {
            return Err(empty);
        }
        let nyquist = Hz(self.geometry.sample_rate().get() as f32 / 2.0);
        if max > nyquist {
            return Err(Error::AboveNyquist { freq: max, nyquist });
        }
        let width = self.bin_width();
        let lo = ((f64::from(min.get()) / width).ceil() as usize).max(1);
        let hi = ((f64::from(max.get()) / width).floor() as usize)
            .min(self.geometry.window().get() / 2 - 1);
        if hi < lo + 2 {
            return Err(empty);
        }
        Ok((lo, hi))
    }

    /// The spacing of the bins, in hertz.
    fn bin_width(&self) -> f64 {
        self.geometry.sample_rate().get() / self.geometry.window().get() as f64
    }

    /// Calls `visit(frequency, energy)` for each peak of `magnitudes` that
    /// counts, and returns false, calling nothing, for a frame with no pitch.
    fn peaks(&self, magnitudes: &[f32], mut visit: impl FnMut(f64, f32)) -> bool {
        // The band was validated on construction and on every `with_band`.
        let Ok((lo, hi)) = self.band() else {
            return false;
        };
        let top = magnitudes[lo..=hi].iter().copied().fold(0.0f32, f32::max);
        // A sine of amplitude `a` reads `a * magnitude_gain / 2`.
        let amplitude = 2.0 * top / self.geometry.magnitude_gain();
        if amplitude.is_nan() || amplitude < SILENCE_FLOOR.to_amplitude().get() {
            return false;
        }
        let floor = top * self.peak_floor.to_amplitude().get();
        let width = self.bin_width();
        for i in lo..=hi {
            let m = magnitudes[i];
            if !(m > magnitudes[i - 1] && m >= magnitudes[i + 1] && m >= floor) {
                continue;
            }
            let (offset, peak) = interpolate(magnitudes[i - 1], m, magnitudes[i + 1]);
            let frequency = (i as f64 + offset) * width;
            if frequency >= f64::from(self.min_freq.get())
                && frequency <= f64::from(self.max_freq.get())
            {
                visit(frequency, peak * peak);
            }
        }
        true
    }

    /// Adds a peak's distance from the equal-tempered grid to `sums`.
    fn tune(frequency: f64, energy: f32, sums: &mut TuningSums) {
        let off = off_grid(frequency);
        let turn = f64::from(off) * std::f64::consts::TAU;
        let w = f64::from(energy);
        sums.cos += w * turn.cos();
        sums.sin += w * turn.sin();
    }

    /// Folds a peak into the twelve classes at `tuning` semitones sharp.
    fn fold(&self, frequency: f64, energy: f32, tuning: Semitones, frame: &mut [f32; CLASSES]) {
        let mut weight = energy;
        for order in 1..=self.harmonics {
            let sub = frequency / order as f64;
            if sub < f64::from(self.min_freq.get()) {
                break;
            }
            let class = class_of(sub, tuning);
            frame[class] += weight;
            weight *= self.harmonic_decay;
        }
    }
}

/// The half-semitone-wide distance of `frequency` from the nearest
/// equal-tempered pitch, in semitones, `-0.5..=0.5`.
fn off_grid(frequency: f64) -> f32 {
    let s = semitones_from_a4(frequency).get();
    s - s.round()
}

/// `frequency`'s distance from A4.
fn semitones_from_a4(frequency: f64) -> Semitones {
    Semitones::from_pitch_ratio((frequency / f64::from(Note::A4.frequency().get())) as f32)
}

/// The class of `frequency` once `tuning` is taken out, as a column.
fn class_of(frequency: f64, tuning: Semitones) -> usize {
    let a = f32::from(Note::A4.class().semitones_above_c());
    let position = a + semitones_from_a4(frequency).get() - tuning.get();
    (position.round() as i64).rem_euclid(CLASSES as i64) as usize
}

/// Parabolic interpolation of the log magnitude through three bins, whose
/// middle one is the peak: its offset from the middle bin, within
/// `-0.5..=0.5`, and the magnitude there.
fn interpolate(below: f32, mid: f32, above: f32) -> (f64, f32) {
    let ln = |m: f32| f64::from(m).max(TINY).ln();
    let (a, b, c) = (ln(below), ln(mid), ln(above));
    let curvature = a - 2.0 * b + c;
    if curvature.is_nan() || curvature >= 0.0 {
        return (0.0, mid);
    }
    let offset = (0.5 * (a - c) / curvature).clamp(-0.5, 0.5);
    (offset, (b - 0.25 * (a - c) * offset).exp() as f32)
}

/// The energy-weighted circular mean of the peaks' off-grid distances, kept as
/// its two components.
#[derive(Debug, Clone, Copy, Default)]
struct TuningSums {
    cos: f64,
    sin: f64,
}

impl TuningSums {
    /// The mean distance, within `-50..=50` cents; 0 with no peak.
    fn cents(self) -> Cents {
        let turns = self.sin.atan2(self.cos) / std::f64::consts::TAU;
        Semitones(turns as f32).to_cents()
    }
}

/// A chromagram: a `frames x 12` grid and the tuning it was folded with.
#[derive(Debug, Clone, PartialEq)]
pub struct Chroma {
    grid: Grid<f32>,
    tuning: Cents,
}

impl Chroma {
    /// One row per frame, one column per pitch class from C, each `0..=1`.
    #[inline]
    #[must_use]
    pub fn grid(&self) -> &Grid<f32> {
        &self.grid
    }

    /// Moves the grid out, dropping the tuning.
    #[inline]
    #[must_use]
    pub fn into_grid(self) -> Grid<f32> {
        self.grid
    }

    /// The tuning the frames were folded with: the estimate over the whole
    /// buffer, or the fixed value the config named.
    #[inline]
    #[must_use]
    pub fn tuning(&self) -> Cents {
        self.tuning
    }

    /// The mean of the frames, per class; all zero for no frames.
    #[must_use]
    pub fn mean(&self) -> [f32; CLASSES] {
        mean_of_rows(&self.grid)
    }
}

/// The per-column mean of a `frames x 12` grid; all zero for no frames.
pub(crate) fn mean_of_rows(grid: &Grid<f32>) -> [f32; CLASSES] {
    let mut sum = [0.0f64; CLASSES];
    for row in grid.rows() {
        for (s, &v) in sum.iter_mut().zip(row) {
            *s += f64::from(v);
        }
    }
    mean_of(&sum, grid.frames().get())
}

fn mean_of(sum: &[f64; CLASSES], frames: usize) -> [f32; CLASSES] {
    let n = frames.max(1) as f64;
    sum.map(|s| (s / n) as f32)
}

/// What a run of [`step_chroma`] carries between frames: a running mean and
/// the tuning sums, and the buffers a frame is transformed in.
#[derive(Debug, Clone, Default)]
pub struct ChromaState {
    /// The config this carry belongs to; a different one resets it.
    config: Option<ChromaConfig>,
    /// The geometry the buffers below were built for.
    built: Option<StftGeometry>,
    window: Vec<f32>,
    spectrum: Vec<Complex>,
    magnitudes: Vec<f32>,
    sum: [f64; CLASSES],
    frames: usize,
    sums: TuningSums,
}

impl ChromaState {
    /// An empty carry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Forgets the carried frames and tuning, keeping the buffers: the next
    /// step allocates nothing if its geometry is the last one's. Reads like a
    /// fresh state: no frames, a zero mean and a zero tuning.
    pub fn reset(&mut self) {
        self.config = None;
        self.sum = [0.0; CLASSES];
        self.frames = 0;
        self.sums = TuningSums::default();
    }

    /// How many frames have been folded.
    #[inline]
    #[must_use]
    pub fn frames_seen(&self) -> usize {
        self.frames
    }

    /// The mean of the frames folded so far, per class; all zero before the
    /// first.
    #[must_use]
    pub fn mean(&self) -> [f32; CLASSES] {
        mean_of(&self.sum, self.frames)
    }

    /// The tuning in use: the config's fixed value, or the estimate from the
    /// peaks seen so far (0 before any).
    #[must_use]
    pub fn tuning(&self) -> Cents {
        match self.config.map(|c| c.tuning) {
            Some(Tuning::Fixed(cents)) => cents,
            _ => self.sums.cents(),
        }
    }

    /// Takes on `cfg`, discarding a carry that belongs to another. The
    /// buffers are rebuilt only for a different geometry, so a change of
    /// tuning, band or weights allocates nothing.
    fn adopt(&mut self, cfg: &ChromaConfig) {
        if self.config != Some(*cfg) {
            self.reset();
            self.config = Some(*cfg);
        }
        if self.built != Some(cfg.geometry) {
            self.built = Some(cfg.geometry);
            self.window = cfg.geometry.window_coefficients();
            let window = cfg.geometry.window().get();
            self.spectrum = vec![Complex::default(); window / 2 + 1];
            self.magnitudes = vec![0.0; window / 2 + 1];
        }
    }

    /// Leaves the magnitudes of `samples`, one adopted frame, in
    /// `self.magnitudes`.
    fn transform(&mut self, samples: &[f32], fft: &mut FftScratch) {
        fft.forward(samples, &self.window, &mut self.spectrum);
        for (m, bin) in self.magnitudes.iter_mut().zip(&self.spectrum) {
            *m = bin.norm();
        }
    }
}

/// Folds one frame of `samples` (exactly one window) and adds it to the
/// running mean. Returns the frame's chroma, `0..=1` per class, zero for a
/// frame with no pitch.
///
/// # Errors
///
/// [`Error::InvalidTonalParameter`] when `samples` is not one window long;
/// [`Error::NonFiniteInput`] when it holds a NaN or infinity.
pub fn step_chroma(
    cfg: &ChromaConfig,
    state: &mut ChromaState,
    samples: &[f32],
    fft: &mut FftScratch,
) -> Result<[f32; CLASSES]> {
    // Checked before the state adopts the config, so a bad frame leaves the
    // carry as it was.
    if samples.len() != cfg.geometry.window().get() {
        return Err(Error::InvalidTonalParameter(
            "frame: length differs from the window",
        ));
    }
    if samples.iter().any(|s| !s.is_finite()) {
        return Err(Error::NonFiniteInput);
    }
    state.adopt(cfg);
    state.transform(samples, fft);

    let mut frame = [0.0f32; CLASSES];
    let tuning = match cfg.tuning {
        Tuning::Fixed(cents) => cents,
        Tuning::Estimate => {
            let sums = &mut state.sums;
            cfg.peaks(&state.magnitudes, |f, e| ChromaConfig::tune(f, e, sums));
            sums.cents()
        }
    };
    let tuning = tuning.to_semitones();
    cfg.peaks(&state.magnitudes, |f, e| cfg.fold(f, e, tuning, &mut frame));

    let max = frame.iter().copied().fold(0.0f32, f32::max);
    if max > 0.0 {
        frame.iter_mut().for_each(|v| *v /= max);
    } else {
        frame = [0.0; CLASSES];
    }
    for (s, &v) in state.sum.iter_mut().zip(&frame) {
        *s += f64::from(v);
    }
    state.frames += 1;
    Ok(frame)
}

/// The chromagram of a whole buffer.
///
/// With [`Tuning::Estimate`] the buffer is transformed twice, once to find the
/// tuning of all its peaks and once to fold with it; the second pass is
/// [`step_chroma`] with that tuning fixed, so streaming a buffer at a fixed
/// tuning gives these frames bit for bit. Frame `i` covers samples
/// `i * hop .. i * hop + window`.
///
/// # Errors
///
/// [`Error::NonFiniteInput`] for a NaN or infinite sample;
/// [`Error::InsufficientInput`] for less than one window;
/// [`Error::InvalidTonalParameter`] for more than 524288 frames.
pub fn chroma(samples: &[f32], cfg: &ChromaConfig, fft: &mut FftScratch) -> Result<Chroma> {
    if samples.iter().any(|s| !s.is_finite()) {
        return Err(Error::NonFiniteInput);
    }
    let window = cfg.geometry.window().get();
    if samples.len() < window {
        return Err(Error::InsufficientInput {
            needed: Samples(window),
            got: Samples(samples.len()),
        });
    }
    let frames = cfg.geometry.frames_for(Samples(samples.len()));
    if frames.get() > MAX_FRAMES {
        return Err(Error::InvalidTonalParameter("input: too many frames"));
    }
    let frame_at = |i: usize| {
        let at = i * cfg.geometry.hop().get();
        &samples[at..at + window]
    };

    let tuning = match cfg.tuning {
        Tuning::Fixed(cents) => cents,
        Tuning::Estimate => {
            let mut state = ChromaState::new();
            state.adopt(cfg);
            for i in 0..frames.get() {
                state.transform(frame_at(i), fft);
                let sums = &mut state.sums;
                cfg.peaks(&state.magnitudes, |f, e| ChromaConfig::tune(f, e, sums));
            }
            state.sums.cents()
        }
    };

    let fixed = ChromaConfig {
        tuning: Tuning::Fixed(tuning),
        ..*cfg
    };
    let mut state = ChromaState::new();
    let mut data = Vec::with_capacity(frames.get() * CLASSES);
    for i in 0..frames.get() {
        data.extend_from_slice(&step_chroma(&fixed, &mut state, frame_at(i), fft)?);
    }
    Ok(Chroma {
        grid: Grid::new(data, frames, BinCount(CLASSES))?,
        tuning,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_types::SampleRate;

    fn geometry(window: usize, hop: usize) -> StftGeometry {
        StftGeometry::new(SampleRate::new(22_050.0), Samples(window), Samples(hop)).unwrap()
    }

    fn sine(freq: f64, len: usize, amp: f64) -> Vec<f32> {
        (0..len)
            .map(|i| (amp * (std::f64::consts::TAU * freq * i as f64 / 22_050.0).sin()) as f32)
            .collect()
    }

    /// Largest error of the interpolated peak position, in bins, over a sweep
    /// of a sine across one bin.
    fn worst_offset_error(interp: impl Fn(f32, f32, f32) -> (f64, f32)) -> f64 {
        let g = geometry(4096, 4096);
        let (window, mut fft) = (g.window_coefficients(), FftScratch::new());
        let mut worst = 0.0f64;
        for step in 0..40 {
            let bin = 100.0 + f64::from(step) / 40.0;
            let x = sine(bin * 22_050.0 / 4096.0, 4096, 0.5);
            let mut spectrum = vec![Complex::default(); 2049];
            fft.forward(&x, &window, &mut spectrum);
            let mags: Vec<f32> = spectrum.iter().map(|b| b.norm()).collect();
            let top = (1..2048).fold(1, |b, i| if mags[i] > mags[b] { i } else { b });
            let (offset, _) = interp(mags[top - 1], mags[top], mags[top + 1]);
            worst = worst.max((top as f64 + offset - bin).abs());
        }
        worst
    }

    /// Parabolic interpolation of the log magnitude places a Hann peak within
    /// 0.02 bin (measured worst 0.016) wherever it falls in a bin; the raw
    /// peak bin is up to half a bin off.
    #[test]
    fn interpolation_places_a_peak_to_a_fiftieth_of_a_bin() {
        assert!(worst_offset_error(interpolate) <= INTERP_BIAS_BINS);
        assert!(worst_offset_error(|_, m, _| (0.0, m)) > 10.0 * INTERP_BIAS_BINS);
    }

    /// The bin error bound above, as a fraction of a bin.
    const INTERP_BIAS_BINS: f64 = 0.02;

    fn run(x: &[f32], cfg: &ChromaConfig) -> Chroma {
        chroma(x, cfg, &mut FftScratch::new()).unwrap()
    }

    fn cfg() -> ChromaConfig {
        ChromaConfig::new(geometry(4096, 1024)).unwrap()
    }

    /// Silence is all zero and finite, a sine under -90 dBFS is silence and one
    /// above is not, and no frame is NaN.
    #[test]
    fn a_silent_frame_is_zero_not_nan() {
        let silent = run(&vec![0.0; 8192], &cfg());
        assert!(silent.grid().as_slice().iter().all(|&v| v == 0.0));
        assert_eq!(silent.tuning(), Cents(0.0));
        let quiet = run(&sine(440.0, 8192, 1e-5), &cfg());
        assert!(quiet.grid().as_slice().iter().all(|&v| v == 0.0));
        let audible = run(&sine(440.0, 8192, 1e-3), &cfg());
        assert!(audible.grid().as_slice().iter().any(|&v| v > 0.0));
    }

    /// A pitched frame's largest column is exactly 1, whatever the level.
    #[test]
    fn a_frame_is_normalized_by_its_largest_column() {
        for level in [0.1, 0.9] {
            let c = run(&sine(440.0, 8192, level), &cfg());
            for row in c.grid().rows() {
                assert_eq!(row.iter().copied().fold(0.0, f32::max), 1.0);
                assert!(row.iter().all(|&v| (0.0..=1.0).contains(&v)));
            }
        }
    }

    /// A sine at G4 (392 Hz) votes for G (itself and its octave, 196 Hz) and
    /// C (a third of it, 130.7 Hz); the fourth subharmonic, 98 Hz, is under the
    /// band's 100 Hz floor and does not vote. With decay 0.6 the energies are
    /// G = 1 + 0.6, C = 0.6^2, so C reads `0.36 / 1.6 = 0.225`.
    #[test]
    fn a_peak_votes_for_its_subharmonics_down_to_the_band_floor() {
        let c = run(&sine(392.0, 8192, 0.5), &cfg()).mean();
        assert!((c[7] - 1.0).abs() < 1e-6);
        assert!((c[0] - 0.225).abs() < 0.01, "{c:?}");
        for (i, &v) in c.iter().enumerate() {
            if i != 0 && i != 7 {
                assert!(v < 0.01, "{c:?}");
            }
        }
    }

    /// Partials outside the band do not fold.
    #[test]
    fn peaks_outside_the_band_are_not_folded() {
        for freq in [80.0, 6000.0] {
            let c = run(&sine(freq, 8192, 0.5), &cfg());
            assert!(c.grid().as_slice().iter().all(|&v| v == 0.0), "{freq}");
        }
    }

    /// A sine's tuning is its distance from the grid, within the interpolation
    /// bias at its frequency: 0.02 bin is `1200 log2(1 + 0.02 w / f)` cents,
    /// `w = 22050 / 4096` Hz. The mean wraps: +45 and -55 are the same.
    #[test]
    fn the_tuning_of_a_sine_is_its_distance_from_the_grid() {
        let width = 22_050.0 / 4096.0;
        let tolerance = 1200.0 * (1.0 + INTERP_BIAS_BINS * width / 440.0).log2();
        for cents in [-40.0, -20.0, 0.0, 20.0, 40.0, 55.0] {
            let freq = 440.0 * (cents / 1200.0f64).exp2();
            let got = run(&sine(freq, 8192, 0.5), &cfg()).tuning().get();
            let want = if cents > 50.0 { cents - 100.0 } else { cents };
            assert!((f64::from(got) - want).abs() <= tolerance, "{cents}: {got}");
        }
    }

    /// The tuning is subtracted before a peak is rounded to a class: a sine
    /// 30 cents above A folds to A at +30 and to A# at -30 (60 cents up).
    #[test]
    fn a_fixed_tuning_moves_the_fold() {
        let x = sine(440.0 * (30.0f64 / 1200.0).exp2(), 8192, 0.5);
        let at = |cents: f32| {
            let cfg = cfg().with_tuning(Tuning::Fixed(Cents(cents))).unwrap();
            let c = run(&x, &cfg);
            assert_eq!(c.tuning(), Cents(cents));
            c.mean()
        };
        assert_eq!(at(30.0)[9], 1.0);
        assert_eq!(at(-30.0)[10], 1.0);
    }

    /// Streaming at the batch's tuning gives the batch's frames to the bit and
    /// the same mean; the carry counts frames and resets.
    #[test]
    fn streaming_at_a_fixed_tuning_is_the_batch() {
        let x: Vec<f32> = sine(440.0, 12_000, 0.5)
            .iter()
            .zip(sine(660.0, 12_000, 0.3))
            .map(|(a, b)| a + b)
            .collect();
        let batch = run(&x, &cfg());
        let fixed = cfg().with_tuning(Tuning::Fixed(batch.tuning())).unwrap();
        let (mut state, mut fft) = (ChromaState::new(), FftScratch::new());
        for (i, row) in batch.grid().rows().enumerate() {
            let at = i * 1024;
            let frame = step_chroma(&fixed, &mut state, &x[at..at + 4096], &mut fft).unwrap();
            assert_eq!(frame.as_slice(), row);
        }
        assert_eq!(state.frames_seen(), batch.grid().frames().get());
        assert_eq!(state.mean(), batch.mean());
        assert_eq!(state.tuning(), batch.tuning());
        state.reset();
        assert_eq!(state.frames_seen(), 0);
        assert_eq!(state.mean(), [0.0; CLASSES]);
    }

    /// With an estimated tuning the state reads the peaks seen so far.
    #[test]
    fn a_running_estimate_follows_the_peaks_seen() {
        let x = sine(440.0 * (25.0f64 / 1200.0).exp2(), 4096, 0.5);
        let (mut state, mut fft) = (ChromaState::new(), FftScratch::new());
        assert_eq!(state.tuning(), Cents(0.0));
        step_chroma(&cfg(), &mut state, &x, &mut fft).unwrap();
        assert!((state.tuning().get() - 25.0).abs() < 1.0);
    }

    /// A settings change resets the carry rather than mixing two grids.
    #[test]
    fn a_new_config_resets_the_carry() {
        let (mut state, mut fft) = (ChromaState::new(), FftScratch::new());
        let x = sine(440.0, 4096, 0.5);
        step_chroma(&cfg(), &mut state, &x, &mut fft).unwrap();
        let other = cfg().with_harmonics(1).unwrap();
        step_chroma(&other, &mut state, &x, &mut fft).unwrap();
        assert_eq!(state.frames_seen(), 1);
    }

    /// A frame that is refused leaves the carry alone, even under a config
    /// that differs, and `reset` reads like a fresh state.
    #[test]
    fn a_refused_frame_keeps_the_carry_and_reset_reads_fresh() {
        let (mut state, mut fft) = (ChromaState::new(), FftScratch::new());
        let x = sine(440.0 * (25.0f64 / 1200.0).exp2(), 4096, 0.5);
        step_chroma(&cfg(), &mut state, &x, &mut fft).unwrap();
        let (mean, tuning) = (state.mean(), state.tuning());
        let other = cfg().with_harmonics(1).unwrap();
        let mut nan = x.clone();
        nan[7] = f32::NAN;
        assert!(step_chroma(&other, &mut state, &nan, &mut fft).is_err());
        assert!(step_chroma(&other, &mut state, &x[..100], &mut fft).is_err());
        assert_eq!(state.frames_seen(), 1);
        assert_eq!((state.mean(), state.tuning()), (mean, tuning));

        state.reset();
        assert_eq!(state.frames_seen(), 0);
        assert_eq!(state.mean(), [0.0; CLASSES]);
        assert_eq!(state.tuning(), Cents(0.0));
    }

    /// Every setting is validated, NaN and infinity included.
    #[test]
    fn settings_are_validated() {
        let bad = |r: Result<ChromaConfig>| matches!(r, Err(Error::InvalidTonalParameter(_)));
        assert!(bad(ChromaConfig::new(geometry(32, 8))));
        assert!(bad(ChromaConfig::new(geometry(1 << 19, 1 << 17))));
        assert!(bad(ChromaConfig::new(geometry(4096, 32))));
        assert!(ChromaConfig::new(geometry(4096, 64)).is_ok());
        for harmonics in [0, 17] {
            assert!(bad(cfg().with_harmonics(harmonics)));
        }
        for decay in [0.0, -0.1, 1.5, f32::NAN, f32::INFINITY] {
            assert!(bad(cfg().with_harmonic_decay(decay)), "{decay}");
        }
        for floor in [1.0, -121.0, f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            assert!(bad(cfg().with_peak_floor(Db(floor))), "{floor}");
        }
        for cents in [51.0, -51.0, f32::NAN, f32::INFINITY] {
            assert!(
                bad(cfg().with_tuning(Tuning::Fixed(Cents(cents)))),
                "{cents}"
            );
        }
        let empty = |r: Result<ChromaConfig>| matches!(r, Err(Error::EmptyFrequencyRange { .. }));
        for (lo, hi) in [
            (500.0, 100.0),
            (0.0, 1000.0),
            (f32::NAN, 1000.0),
            (100.0, f32::NAN),
            (100.0, f32::INFINITY),
            (100.0, 110.0),
        ] {
            assert!(empty(cfg().with_band(Hz(lo), Hz(hi))), "{lo} {hi}");
        }
        assert!(matches!(
            cfg().with_band(Hz(100.0), Hz(12_000.0)),
            Err(Error::AboveNyquist { .. })
        ));
        // A low rate clamps the default band to Nyquist instead of failing.
        let low = StftGeometry::new(SampleRate::new(8_000.0), Samples(1024), Samples(256)).unwrap();
        assert_eq!(ChromaConfig::new(low).unwrap().max_freq(), Hz(4000.0));
    }

    /// Bad input is an error, and a refused call computes nothing.
    #[test]
    fn bad_input_is_refused() {
        let mut fft = FftScratch::new();
        let mut x = sine(440.0, 8192, 0.5);
        x[100] = f32::NAN;
        assert_eq!(
            chroma(&x, &cfg(), &mut fft).unwrap_err(),
            Error::NonFiniteInput
        );
        x[100] = f32::INFINITY;
        assert_eq!(
            chroma(&x, &cfg(), &mut fft).unwrap_err(),
            Error::NonFiniteInput
        );
        x[100] = 0.25;
        assert_eq!(
            chroma(&x[..4095], &cfg(), &mut fft).unwrap_err(),
            Error::InsufficientInput {
                needed: Samples(4096),
                got: Samples(4095)
            }
        );
        let tiny = ChromaConfig::new(geometry(64, 1)).unwrap();
        let long = vec![0.0; MAX_FRAMES + 64];
        assert!(matches!(
            chroma(&long, &tiny, &mut fft),
            Err(Error::InvalidTonalParameter(_))
        ));
        // A NaN between two frames is in no window but is still refused.
        let sparse = ChromaConfig::new(geometry(64, 128)).unwrap();
        let mut gap = sine(440.0, 512, 0.5);
        gap[100] = f32::NAN;
        assert_eq!(
            chroma(&gap, &sparse, &mut fft).unwrap_err(),
            Error::NonFiniteInput
        );
        let mut state = ChromaState::new();
        assert!(matches!(
            step_chroma(&cfg(), &mut state, &x[..4000], &mut fft),
            Err(Error::InvalidTonalParameter(_))
        ));
        x[100] = f32::NAN;
        assert_eq!(
            step_chroma(&cfg(), &mut state, &x[..4096], &mut fft).unwrap_err(),
            Error::NonFiniteInput
        );
        assert_eq!(state.frames_seen(), 0);
    }
}
