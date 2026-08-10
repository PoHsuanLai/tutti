//! The short-time Fourier transform and its inverse.
//!
//! Three result types, so "can this be inverted?" is a question the compiler
//! answers:
//!
//! - [`Stft`] — complex bins, as the transform produces them.
//! - [`StftPolar`] — the same information in magnitude/phase form. Lossless.
//! - [`StftMagnitude`] — phase **discarded**. Lossy, and has no inverse.
//!
//! The first two are the same data in two coordinate systems and both
//! reconstruct exactly. The third is a real capability boundary. Collapsing all
//! three into one struct is how a phase-less result reached the inverse and
//! panicked, and how a decimated display transform reached it and produced a
//! comb of islands separated by silence.

use tutti_core::SampleRate;
use tutti_types::{Amplitude, Samples};

use crate::error::{AnalysisError, Result};
use crate::fft::FftScratch;
use crate::geometry::StftGeometry;
use crate::grid::{BinIndex, FrameCount, FrameIndex, Grid};
use crate::Complex;

/// Linear magnitudes, in the units the transform produced. **The invertible
/// form.**
///
/// Distinct from [`NormalizedMagnitudes`] on purpose. As bare `Vec<f32>` both
/// reach the inverse interchangeably, and passing the normalized set compiles
/// and yields audio scaled by `1/peak` — quiet, plausible, and hard to notice.
#[derive(Debug, Clone, PartialEq)]
pub struct RawMagnitudes(Grid<f32>);

impl RawMagnitudes {
    /// Wrap a `frames x bins` grid of linear magnitudes.
    pub fn new(grid: Grid<f32>) -> Self {
        Self(grid)
    }

    /// The underlying `frames x bins` grid, borrowed.
    #[inline]
    pub fn grid(&self) -> &Grid<f32> {
        &self.0
    }

    /// Move the grid out, dropping the "raw" marker with it.
    #[inline]
    pub fn into_grid(self) -> Grid<f32> {
        self.0
    }

    /// The largest magnitude anywhere in the grid.
    pub fn peak(&self) -> Amplitude {
        Amplitude(
            self.0
                .as_slice()
                .iter()
                .copied()
                .fold(0.0f32, |a, b| a.max(b)),
        )
    }

    /// Scale to `0..1` against the peak, returning the display-only form.
    ///
    /// Lazy on purpose. Normalizing eagerly at construction means storing both
    /// forms — roughly 84 MB of duplication for a 4-minute file — when no
    /// consumer reads both, so the caller decides whether to pay for it.
    pub fn normalize(&self) -> NormalizedMagnitudes {
        let peak = self.peak();
        let values = if peak.get() > 0.0 {
            let inv = 1.0 / peak.get();
            self.0.as_slice().iter().map(|m| m * inv).collect()
        } else {
            self.0.as_slice().to_vec()
        };
        NormalizedMagnitudes {
            // Same shape by construction, so the invariant cannot fail.
            values: Grid::new(values, self.0.frames(), self.0.bins())
                .expect("normalize preserves shape"),
            peak,
        }
    }
}

/// Peak-normalized magnitudes plus the peak they were divided by. Display
/// only — there is no path from here back to audio.
#[derive(Debug, Clone, PartialEq)]
pub struct NormalizedMagnitudes {
    values: Grid<f32>,
    peak: Amplitude,
}

impl NormalizedMagnitudes {
    /// The `frames x bins` grid of `0..1` values, borrowed.
    #[inline]
    pub fn grid(&self) -> &Grid<f32> {
        &self.values
    }

    /// Move the `0..1` grid out, dropping the peak that scaled it.
    #[inline]
    pub fn into_grid(self) -> Grid<f32> {
        self.values
    }

    /// The peak the values were scaled against — enough to recover the raw
    /// magnitudes if needed.
    #[inline]
    pub fn peak(&self) -> Amplitude {
        self.peak
    }
}

/// Complex bins, `DC..=Nyquist` per frame.
///
/// Array-of-structs on purpose: the spectral edit path multiplies each bin by
/// a complex mask, which touches magnitude and phase together. Splitting them
/// would force a deinterleave per edit.
#[derive(Debug, Clone, PartialEq)]
pub struct Stft {
    bins: Grid<Complex>,
    geometry: StftGeometry,
}

/// Magnitude and phase, in separate grids.
///
/// Structure-of-arrays on purpose, and the opposite choice from [`Stft`] for
/// the opposite reason: consumers read magnitudes without phase — one uploads
/// the magnitude grid straight to the GPU as a texture — so interleaving would
/// force a deinterleave on every read.
#[derive(Debug, Clone, PartialEq)]
pub struct StftPolar {
    magnitudes: RawMagnitudes,
    phases: Grid<f32>,
    geometry: StftGeometry,
}

/// Magnitudes with phase discarded. **Not invertible.**
///
/// What a decimated display transform produces. Its own type rather than an
/// invertible result carrying an empty phase vector: that shape is statically
/// indistinguishable from a real one, and reaches the inverse as a panic.
#[derive(Debug, Clone, PartialEq)]
pub struct StftMagnitude {
    magnitudes: RawMagnitudes,
    geometry: StftGeometry,
}

impl Stft {
    /// The complex bins as a `frames x bins` grid, `DC..=Nyquist` per frame.
    #[inline]
    pub fn bins(&self) -> &Grid<Complex> {
        &self.bins
    }

    /// The window, hop and sample rate this transform was taken at.
    #[inline]
    pub fn geometry(&self) -> StftGeometry {
        self.geometry
    }

    /// How many frames the transform produced.
    #[inline]
    pub fn frames(&self) -> FrameCount {
        self.bins.frames()
    }

    /// `(frames, bins)` as plain counts — the shape a mask or display grid
    /// must match.
    #[inline]
    pub fn dims(&self) -> (usize, usize) {
        (self.bins.frames().get(), self.bins.bins().get())
    }

    /// Whether the transform produced no frames at all.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.bins.is_empty()
    }

    /// The [`Amplitude`] at one bin — the complex bin's modulus, phase
    /// discarded for this read only.
    #[inline]
    pub fn magnitude_at(&self, frame: FrameIndex, bin: BinIndex) -> Amplitude {
        Amplitude(self.bins.at(frame, bin).norm())
    }

    /// The magnitude a mask would leave at this bin.
    ///
    /// Offered because the unmasked accessor alone forces callers to redo the
    /// multiply; the spectral view currently pools masked magnitudes itself.
    ///
    /// # Errors
    /// Returns [`AnalysisError::GridShapeDisagreement`] if `mask` does not
    /// match this transform's `frames x bins` shape.
    pub fn masked_magnitude_at(
        &self,
        mask: &Grid<Complex>,
        frame: FrameIndex,
        bin: BinIndex,
    ) -> Result<Amplitude> {
        if !self.bins.same_shape_as(mask) {
            return Err(AnalysisError::GridShapeDisagreement);
        }
        Ok(Amplitude(
            (self.bins.at(frame, bin) * mask.at(frame, bin)).norm(),
        ))
    }

    /// Convert to magnitude/phase. Lossless.
    pub fn to_polar(&self) -> StftPolar {
        let (mut magnitudes, mut phases) = (
            Vec::with_capacity(self.bins.as_slice().len()),
            Vec::with_capacity(self.bins.as_slice().len()),
        );
        for bin in self.bins.as_slice() {
            magnitudes.push(bin.norm());
            phases.push(bin.arg());
        }
        let (frames, bins) = (self.bins.frames(), self.bins.bins());
        StftPolar {
            magnitudes: RawMagnitudes(
                Grid::new(magnitudes, frames, bins).expect("shape preserved"),
            ),
            phases: Grid::new(phases, frames, bins).expect("shape preserved"),
            geometry: self.geometry,
        }
    }

    /// Discard phase. One-way, and the return type says so.
    pub fn to_magnitude(&self) -> StftMagnitude {
        let magnitudes: Vec<f32> = self.bins.as_slice().iter().map(|b| b.norm()).collect();
        StftMagnitude {
            magnitudes: RawMagnitudes(
                Grid::new(magnitudes, self.bins.frames(), self.bins.bins())
                    .expect("shape preserved"),
            ),
            geometry: self.geometry,
        }
    }

    /// Resynthesize to audio.
    pub fn resynthesize(&self, fft: &mut FftScratch) -> Vec<f32> {
        istft(self, fft)
    }

    /// Apply a complex mask and resynthesize — the edit path's whole operation
    /// in one call, so no caller hand-writes the zip plus the inverse.
    ///
    /// # Errors
    /// Returns [`AnalysisError::GridShapeDisagreement`] if `mask` does not
    /// match this transform's `frames x bins` shape.
    pub fn resynthesize_masked(
        &self,
        mask: &Grid<Complex>,
        fft: &mut FftScratch,
    ) -> Result<Vec<f32>> {
        if !self.bins.same_shape_as(mask) {
            return Err(AnalysisError::GridShapeDisagreement);
        }
        let masked: Vec<Complex> = self
            .bins
            .as_slice()
            .iter()
            .zip(mask.as_slice())
            .map(|(b, m)| b * m)
            .collect();
        let masked = Stft {
            bins: Grid::new(masked, self.bins.frames(), self.bins.bins()).expect("shape preserved"),
            geometry: self.geometry,
        };
        Ok(istft(&masked, fft))
    }
}

impl StftPolar {
    /// The magnitude half, in the units the transform produced.
    #[inline]
    pub fn magnitudes(&self) -> &RawMagnitudes {
        &self.magnitudes
    }

    /// The phase half, in radians — same `frames x bins` shape as the
    /// magnitudes, and required to reconstruct.
    #[inline]
    pub fn phases(&self) -> &Grid<f32> {
        &self.phases
    }

    /// The window, hop and sample rate this transform was taken at.
    #[inline]
    pub fn geometry(&self) -> StftGeometry {
        self.geometry
    }

    /// Back to complex bins. Lossless.
    pub fn to_rectangular(&self) -> Stft {
        let bins: Vec<Complex> = self
            .magnitudes
            .grid()
            .as_slice()
            .iter()
            .zip(self.phases.as_slice())
            .map(|(&m, &p)| Complex::from_polar(m, p))
            .collect();
        Stft {
            bins: Grid::new(bins, self.phases.frames(), self.phases.bins())
                .expect("shape preserved"),
            geometry: self.geometry,
        }
    }

    /// Move both grids out without cloning — consumers copy these into their
    /// own asset types.
    pub fn into_grids(self) -> (Grid<f32>, Grid<f32>) {
        (self.magnitudes.into_grid(), self.phases)
    }

    /// Resynthesize to audio, converting back to rectangular first. Lossless
    /// against the transform this came from.
    pub fn resynthesize(&self, fft: &mut FftScratch) -> Vec<f32> {
        istft(&self.to_rectangular(), fft)
    }
}

impl StftMagnitude {
    /// The magnitudes, in the units the transform produced. There is no phase
    /// counterpart — that is what makes this type one-way.
    #[inline]
    pub fn magnitudes(&self) -> &RawMagnitudes {
        &self.magnitudes
    }

    /// The window, hop and sample rate this transform was taken at. A decimated
    /// hop generally breaks COLA, which is why this result cannot invert.
    #[inline]
    pub fn geometry(&self) -> StftGeometry {
        self.geometry
    }

    /// How many frames the transform produced.
    #[inline]
    pub fn frames(&self) -> FrameCount {
        self.magnitudes.grid().frames()
    }

    /// `(frames, bins)` as plain counts.
    #[inline]
    pub fn dims(&self) -> (usize, usize) {
        let grid = self.magnitudes.grid();
        (grid.frames().get(), grid.bins().get())
    }

    /// Whether the transform produced no frames at all.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.magnitudes.grid().is_empty()
    }
}

/// How the hop between frames is chosen.
///
/// A named policy rather than an `Option<usize>` doing enum duty, where `None`
/// and `Some(n)` have to carry "use my hop" and "override it to hit n frames"
/// between them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HopPolicy {
    /// Use exactly this hop, in [`Samples`].
    Fixed(Samples),
    /// Widen the hop to produce roughly `frames` frames, never going below
    /// `min_hop`.
    ///
    /// Decimation is a display concern: the resulting hop generally breaks
    /// COLA, so this yields [`StftMagnitude`], which cannot be inverted.
    TargetFrames {
        /// Roughly how many frames to land on.
        frames: FrameCount,
        /// Floor on the widened hop, in [`Samples`].
        min_hop: Samples,
    },
}

/// Which part of the buffer to analyze.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleRange {
    /// The whole buffer.
    All,
    /// Half-open `[start, end)`, clamped to the buffer.
    Span {
        /// First sample analyzed, clamped to the buffer length.
        start: Samples,
        /// One past the last sample analyzed, clamped to `start..=len`.
        end: Samples,
    },
}

impl SampleRange {
    fn resolve(self, len: usize) -> (usize, usize) {
        match self {
            Self::All => (0, len),
            Self::Span { start, end } => {
                let start = start.get().min(len);
                (start, end.get().clamp(start, len))
            }
        }
    }
}

/// An analysis request: what to analyze, at what resolution.
#[derive(Debug, Clone, Copy)]
pub struct StftRequest {
    /// Rate the buffer is denominated at, used to resolve bin frequencies.
    pub sample_rate: SampleRate,
    /// Analysis window, in [`Samples`]. Also the FFT size.
    pub window: Samples,
    /// How far to advance between frames — fixed, or decimated to a frame count.
    pub hop: HopPolicy,
    /// Which span of the buffer to transform.
    pub range: SampleRange,
}

impl StftRequest {
    /// A request over the whole buffer at a fixed hop.
    pub fn new(
        sample_rate: impl Into<SampleRate>,
        window: impl Into<Samples>,
        hop: impl Into<Samples>,
    ) -> Self {
        Self {
            sample_rate: sample_rate.into(),
            window: window.into(),
            hop: HopPolicy::Fixed(hop.into()),
            range: SampleRange::All,
        }
    }

    /// Decimate to roughly `frames` frames — a display transform.
    pub fn decimated_to(mut self, frames: FrameCount, min_hop: impl Into<Samples>) -> Self {
        self.hop = HopPolicy::TargetFrames {
            frames,
            min_hop: min_hop.into(),
        };
        self
    }

    /// Restrict the request to the half-open span `[start, end)`, clamped to
    /// the buffer at resolve time.
    pub fn over(mut self, start: impl Into<Samples>, end: impl Into<Samples>) -> Self {
        self.range = SampleRange::Span {
            start: start.into(),
            end: end.into(),
        };
        self
    }

    /// The geometry this request resolves to over `len` samples.
    ///
    /// Only ever an overlapping grid — use [`resolve_cola`](Self::resolve_cola)
    /// when the result must invert.
    ///
    /// # Errors
    /// Returns an [`AnalysisError`] for a zero window or hop, a hop wider than
    /// the window, or a non-positive sample rate.
    pub fn resolve(&self, len: Samples) -> Result<StftGeometry> {
        let hop = self.effective_hop(len.get());
        StftGeometry::new(self.sample_rate, self.window, hop)
    }

    /// The geometry, additionally required to be invertible.
    ///
    /// # Errors
    /// Everything [`resolve`](Self::resolve) rejects, plus
    /// [`AnalysisError::NotColaCompliant`] when the hop does not divide the
    /// window at 4x overlap.
    pub fn resolve_cola(&self, len: Samples) -> Result<StftGeometry> {
        let hop = self.effective_hop(len.get());
        StftGeometry::cola(self.sample_rate, self.window, hop)
    }

    fn effective_hop(&self, len: usize) -> Samples {
        match self.hop {
            HopPolicy::Fixed(hop) => hop,
            HopPolicy::TargetFrames { frames, min_hop } => {
                if frames.get() == 0 || len <= self.window.get() {
                    return min_hop;
                }
                let span = len - self.window.get();
                Samples((span / frames.get()).max(min_hop.get()).max(1))
            }
        }
    }
}

/// Compute the complex STFT.
///
/// The geometry must be COLA-compliant, because this result can be inverted.
pub fn stft(samples: &[f32], request: StftRequest, fft: &mut FftScratch) -> Result<Stft> {
    let (start, end) = request.range.resolve(samples.len());
    let slice = &samples[start..end];
    let geometry = request.resolve_cola(Samples(slice.len()))?;
    Ok(compute(slice, geometry, fft))
}

/// Compute the STFT in magnitude/phase form. Also invertible.
pub fn stft_polar(
    samples: &[f32],
    request: StftRequest,
    fft: &mut FftScratch,
) -> Result<StftPolar> {
    Ok(stft(samples, request, fft)?.to_polar())
}

/// Compute a magnitude-only STFT — the display path.
///
/// Accepts any overlapping geometry, including decimated hops that break COLA,
/// because the result cannot be inverted anyway. That is the whole reason this
/// returns a different type.
pub fn stft_magnitude(
    samples: &[f32],
    request: StftRequest,
    fft: &mut FftScratch,
) -> Result<StftMagnitude> {
    let (start, end) = request.range.resolve(samples.len());
    let slice = &samples[start..end];
    let geometry = request.resolve(Samples(slice.len()))?;
    Ok(compute(slice, geometry, fft).to_magnitude())
}

/// Invert a complex STFT by overlap-add.
///
/// Weighted overlap-add with per-sample window-sum normalization, which is
/// exact when the geometry is COLA — and [`Stft`] can only be built on one.
pub fn istft(transform: &Stft, fft: &mut FftScratch) -> Vec<f32> {
    let geometry = transform.geometry;
    let frames = transform.bins.frames();
    let window_len = geometry.window().get();

    let out_len = geometry.output_len(frames).get();
    if out_len == 0 {
        return Vec::new();
    }

    let window = geometry.hann();
    let mut out = vec![0.0f32; out_len];
    let mut window_sum = vec![0.0f32; out_len];
    let mut frame_buf = vec![0.0f32; window_len];

    for frame in frames.indices() {
        fft.inverse(transform.bins.row(frame), &mut frame_buf);
        let offset = frame.get() * geometry.hop().get();
        for i in 0..window_len {
            out[offset + i] += frame_buf[i] * window[i];
            window_sum[offset + i] += window[i] * window[i];
        }
    }

    for (sample, &sum) in out.iter_mut().zip(&window_sum) {
        if sum > 1e-8 {
            *sample /= sum;
        }
    }
    out
}

/// The shared forward pass. Both entry points differ only in what they accept
/// and what they return.
fn compute(samples: &[f32], geometry: StftGeometry, fft: &mut FftScratch) -> Stft {
    let frames = geometry.frames_for(Samples(samples.len()));
    let bins_per_frame = geometry.bins_per_frame();
    let window = geometry.hann();
    let window_len = geometry.window().get();

    let mut bins = vec![Complex::default(); frames.get() * bins_per_frame.get()];
    for frame in frames.indices() {
        let offset = frame.get() * geometry.hop().get();
        let start = frame.get() * bins_per_frame.get();
        fft.forward(
            &samples[offset..offset + window_len],
            &window,
            &mut bins[start..start + bins_per_frame.get()],
        );
    }

    Stft {
        bins: Grid::new(bins, frames, bins_per_frame).expect("shape built to match"),
        geometry,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(len: usize, freq: f32, sample_rate: f32) -> Vec<f32> {
        (0..len)
            .map(|i| (2.0 * core::f32::consts::PI * freq * i as f32 / sample_rate).sin() * 0.5)
            .collect()
    }

    #[test]
    fn a_tone_lands_in_the_expected_bin() {
        let samples = tone(8192, 440.0, 44100.0);
        let mut fft = FftScratch::new();
        let result = stft(
            &samples,
            StftRequest::new(44100.0, 2048usize, 512usize),
            &mut fft,
        )
        .unwrap();

        let peak = (0..result.bins().bins().get())
            .max_by(|&a, &b| {
                result
                    .magnitude_at(FrameIndex(1), BinIndex(a))
                    .get()
                    .partial_cmp(&result.magnitude_at(FrameIndex(1), BinIndex(b)).get())
                    .unwrap()
            })
            .unwrap();

        let expected = result.geometry().bin_frequency(BinIndex(peak));
        assert!(
            (expected.get() - 440.0).abs() < 25.0,
            "peak bin maps to {expected}, expected ~440 Hz"
        );
    }

    /// The round trip the type split exists to protect.
    #[test]
    fn stft_round_trips_through_istft() {
        let samples = tone(8192, 440.0, 44100.0);
        let mut fft = FftScratch::new();
        let transform = stft(
            &samples,
            StftRequest::new(44100.0, 2048usize, 512usize),
            &mut fft,
        )
        .unwrap();
        let resynth = transform.resynthesize(&mut fft);

        // Compare the steady-state interior: the edges ramp in and out because
        // fewer windows overlap there.
        let window = transform.geometry().window().get();
        for i in window..resynth.len() - window {
            assert!(
                (samples[i] - resynth[i]).abs() < 1e-3,
                "sample {i}: {} != {}",
                samples[i],
                resynth[i]
            );
        }
    }

    #[test]
    fn polar_and_rectangular_are_the_same_information() {
        let samples = tone(4096, 300.0, 44100.0);
        let mut fft = FftScratch::new();
        let rect = stft(
            &samples,
            StftRequest::new(44100.0, 1024usize, 256usize),
            &mut fft,
        )
        .unwrap();
        let back = rect.to_polar().to_rectangular();

        for (a, b) in rect.bins().as_slice().iter().zip(back.bins().as_slice()) {
            assert!((a.re - b.re).abs() < 1e-3 && (a.im - b.im).abs() < 1e-3);
        }
    }

    /// A decimated request breaks COLA, so the invertible entry point refuses
    /// it — where the old code silently produced non-overlapping frames and
    /// let them reach the inverse.
    #[test]
    fn a_decimated_request_is_refused_by_the_invertible_path() {
        let samples = tone(200_000, 440.0, 44100.0);
        let mut fft = FftScratch::new();
        let request =
            StftRequest::new(44100.0, 2048usize, 512usize).decimated_to(FrameCount(64), 1usize);

        // The hop this resolves to is far wider than the window.
        assert!(matches!(
            stft(&samples, request, &mut fft),
            Err(AnalysisError::HopExceedsWindow { .. })
                | Err(AnalysisError::NotColaCompliant { .. })
        ));

        // The display path accepts it and yields a type with no inverse.
        let display = stft_magnitude(&samples, request, &mut fft).unwrap();
        assert!(display.frames().get() > 0);
    }

    #[test]
    fn normalized_magnitudes_carry_their_peak() {
        let samples = tone(4096, 440.0, 44100.0);
        let mut fft = FftScratch::new();
        let polar = stft_polar(
            &samples,
            StftRequest::new(44100.0, 1024usize, 256usize),
            &mut fft,
        )
        .unwrap();

        let raw = polar.magnitudes();
        let normalized = raw.normalize();

        assert_eq!(normalized.peak(), raw.peak());
        let max = normalized
            .grid()
            .as_slice()
            .iter()
            .copied()
            .fold(0.0f32, f32::max);
        assert!((max - 1.0).abs() < 1e-5, "normalized peak should be 1.0");
    }

    #[test]
    fn a_range_analyzes_only_its_span() {
        let samples = tone(16384, 440.0, 44100.0);
        let mut fft = FftScratch::new();

        let whole = stft(
            &samples,
            StftRequest::new(44100.0, 2048usize, 512usize),
            &mut fft,
        )
        .unwrap();
        let part = stft(
            &samples,
            StftRequest::new(44100.0, 2048usize, 512usize).over(0usize, 8192usize),
            &mut fft,
        )
        .unwrap();

        assert!(part.frames().get() < whole.frames().get());
        // A span past the end clamps rather than panicking.
        let clamped = stft(
            &samples,
            StftRequest::new(44100.0, 2048usize, 512usize).over(0usize, 99_999usize),
            &mut fft,
        )
        .unwrap();
        assert_eq!(clamped.frames(), whole.frames());
    }

    #[test]
    fn masking_requires_a_matching_shape() {
        let samples = tone(8192, 440.0, 44100.0);
        let mut fft = FftScratch::new();
        let transform = stft(
            &samples,
            StftRequest::new(44100.0, 2048usize, 512usize),
            &mut fft,
        )
        .unwrap();

        let wrong = Grid::filled(Complex::default(), FrameCount(2), crate::grid::BinCount(3));
        assert_eq!(
            transform.resynthesize_masked(&wrong, &mut fft),
            Err(AnalysisError::GridShapeDisagreement)
        );

        // A neutral mask is the identity.
        let neutral = Grid::filled(
            Complex::new(1.0, 0.0),
            transform.bins().frames(),
            transform.bins().bins(),
        );
        let masked = transform.resynthesize_masked(&neutral, &mut fft).unwrap();
        let plain = transform.resynthesize(&mut fft);
        for (a, b) in masked.iter().zip(&plain) {
            assert!((a - b).abs() < 1e-6);
        }
    }

    #[test]
    fn an_empty_mask_erases_the_signal() {
        let samples = tone(8192, 440.0, 44100.0);
        let mut fft = FftScratch::new();
        let transform = stft(
            &samples,
            StftRequest::new(44100.0, 2048usize, 512usize),
            &mut fft,
        )
        .unwrap();

        let silence = Grid::filled(
            Complex::default(),
            transform.bins().frames(),
            transform.bins().bins(),
        );
        let erased = transform.resynthesize_masked(&silence, &mut fft).unwrap();
        for s in erased {
            assert!(s.abs() < 1e-6);
        }
    }

    #[test]
    fn input_shorter_than_one_window_yields_no_frames() {
        let samples = tone(100, 440.0, 44100.0);
        let mut fft = FftScratch::new();
        let result = stft(
            &samples,
            StftRequest::new(44100.0, 2048usize, 512usize),
            &mut fft,
        )
        .unwrap();

        assert_eq!(result.frames(), FrameCount(0));
        assert!(result.resynthesize(&mut fft).is_empty());
    }
}
