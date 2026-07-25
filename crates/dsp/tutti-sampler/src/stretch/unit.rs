//! Time-stretching audio unit wrapper.

use std::sync::Arc;
use tutti_core::{
    AtomicF32, AudioUnit, BufferMut, BufferRef, Cents, Ordering, SignalFrame, StretchFactor,
};

use tutti_core::RtScratch;

use super::phase_vocoder::PhaseVocoderProcessor;
use super::types::{Algorithm, FftSize};

enum Processor {
    PhaseVocoder(PhaseVocoderProcessor),
}

impl Processor {
    fn latency_samples(&self) -> usize {
        match self {
            Self::PhaseVocoder(p) => p.latency_samples(),
        }
    }

    fn reset(&mut self) {
        match self {
            Self::PhaseVocoder(p) => p.reset(),
        }
    }

    fn set_sample_rate(&mut self, sr: tutti_core::SampleRate) {
        match self {
            Self::PhaseVocoder(p) => p.set_sample_rate(sr),
        }
    }

    fn push_input(&mut self, samples: &[f32]) {
        match self {
            Self::PhaseVocoder(p) => p.push_input(samples),
        }
    }

    fn process(&mut self, stretch: f32, pitch_ratio: f32) {
        match self {
            Self::PhaseVocoder(p) => p.process(stretch, pitch_ratio),
        }
    }

    fn pop_output(&mut self, output: &mut [f32]) -> usize {
        match self {
            Self::PhaseVocoder(p) => p.pop_output(output),
        }
    }
}

impl Clone for Processor {
    fn clone(&self) -> Self {
        match self {
            Self::PhaseVocoder(p) => Self::PhaseVocoder(p.clone()),
        }
    }
}

/// Maximum buffer size for pre-allocation (covers all common audio interfaces)
const MAX_BUFFER_SIZE: usize = 8192;

/// Real-time time-stretching and pitch-shifting unit.
///
/// A pure frame-in → frame-out **filter**: it owns NO source. The caller ticks
/// the real clip source itself and feeds the resulting stereo frame in as this
/// unit's `input`; the phase-vocoder latent state (the two processors, the
/// scratch buffers, the atomics) is what lives here. This removes the former
/// second copy of the clip source (a boxed clone of it) and the
/// coherence machinery that kept it in sync with the direct-read source.
/// # Channels
///
/// One phase-vocoder [`Processor`] per channel, plus one in/out [`RtScratch`]
/// pair each. The processors are **independent** — there is no phase locking
/// between them, so a correlated source can drift channel-to-channel. That was
/// already true of the stereo pair; widening does not make it worse, and fixing
/// it is a separate question from width.
pub struct Unit {
    /// One per channel; `processors.len()` **is** the unit's width, so the
    /// scratch vectors below are always the same length.
    processors: Vec<Processor>,
    stretch_factor: Arc<AtomicF32>,
    pitch_cents: Arc<AtomicF32>,
    enabled: bool,
    algorithm: Algorithm,
    sample_rate: f64,
    scratch_in: Vec<RtScratch<f32>>,
    scratch_out: Vec<RtScratch<f32>>,
}

// Hand-rolled: holds non-`Debug` phase-vocoder `Processor`s and `RtScratch`
// buffers. Print the algorithm + live stretch/pitch atomics + enabled flag.
impl std::fmt::Debug for Unit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Unit")
            .field("algorithm", &self.algorithm)
            .field("enabled", &self.enabled)
            .field("sample_rate", &self.sample_rate)
            .field(
                "stretch_factor",
                &self.stretch_factor.load(Ordering::Acquire),
            )
            .field("pitch_cents", &self.pitch_cents.load(Ordering::Acquire))
            .finish_non_exhaustive()
    }
}

impl Unit {
    /// Create with phase vocoder algorithm (default)
    pub fn new(sample_rate: impl Into<tutti_core::SampleRate>) -> Self {
        Self::with_fft_size(sample_rate, FftSize::default())
    }

    /// Create with custom FFT size (phase vocoder)
    pub fn with_fft_size(
        sample_rate: impl Into<tutti_core::SampleRate>,
        fft_size: FftSize,
    ) -> Self {
        Self::with_fft_size_and_channels(sample_rate, fft_size, 2)
    }

    /// Create an `channels`-wide stretcher (phase vocoder, default FFT size).
    ///
    /// Width is fixed at construction — it sizes one phase-vocoder processor and
    /// two scratch buffers per channel, all allocated here so the RT path never
    /// does. Callers must pass the width of the source they will feed in: a
    /// narrower stretcher silently truncates the frames handed to `tick`.
    pub fn with_channels(sample_rate: impl Into<tutti_core::SampleRate>, channels: usize) -> Self {
        Self::with_fft_size_and_channels(sample_rate, FftSize::default(), channels)
    }

    /// Full constructor: custom FFT size at a custom width.
    pub fn with_fft_size_and_channels(
        sample_rate: impl Into<tutti_core::SampleRate>,
        fft_size: FftSize,
        channels: usize,
    ) -> Self {
        let sample_rate = sample_rate.into().get();
        // A zero-wide filter has nothing to process and would make `inputs()` /
        // `outputs()` lie to the graph.
        let channels = channels.max(1);
        Self {
            processors: (0..channels)
                .map(|_| Processor::PhaseVocoder(PhaseVocoderProcessor::new(fft_size, sample_rate)))
                .collect(),
            stretch_factor: Arc::new(AtomicF32::new(1.0)),
            pitch_cents: Arc::new(AtomicF32::new(0.0)),
            enabled: true,
            algorithm: Algorithm::PhaseVocoder,
            sample_rate,
            scratch_in: (0..channels)
                .map(|_| RtScratch::new(MAX_BUFFER_SIZE))
                .collect(),
            scratch_out: (0..channels)
                .map(|_| RtScratch::new(MAX_BUFFER_SIZE))
                .collect(),
        }
    }

    /// Channel width — the number of processors, and this unit's in/out arity.
    pub fn channels(&self) -> usize {
        self.processors.len()
    }

    pub fn algorithm(&self) -> Algorithm {
        self.algorithm
    }

    /// Set stretch factor (1.0 = normal, 2.0 = half speed, 0.5 = double speed)
    pub fn set_stretch_factor(&self, factor: StretchFactor) {
        self.stretch_factor
            .store(factor.get().clamp(0.25, 4.0), Ordering::Release);
    }

    pub fn stretch_factor(&self) -> StretchFactor {
        StretchFactor::new(self.stretch_factor.load(Ordering::Acquire))
    }

    /// Get Arc for lock-free external control
    pub fn stretch_factor_arc(&self) -> Arc<AtomicF32> {
        Arc::clone(&self.stretch_factor)
    }

    /// Set pitch shift in cents (only works with PhaseVocoder algorithm)
    pub fn set_pitch_cents(&self, cents: Cents) {
        self.pitch_cents
            .store(cents.get().clamp(-2400.0, 2400.0), Ordering::Release);
    }

    pub fn pitch_cents(&self) -> Cents {
        Cents::new(self.pitch_cents.load(Ordering::Acquire))
    }

    /// Get Arc for lock-free external control
    pub fn pitch_cents_arc(&self) -> Arc<AtomicF32> {
        Arc::clone(&self.pitch_cents)
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    pub fn is_processing(&self) -> bool {
        if !self.enabled {
            return false;
        }
        let stretch = self.stretch_factor.load(Ordering::Acquire);
        let pitch = self.pitch_cents.load(Ordering::Acquire);
        (stretch - 1.0).abs() > 0.001 || pitch.abs() > 0.5
    }

    /// Processing latency. Every channel's processor reports the same value
    /// (it is a function of the shared FFT size), so channel 0 speaks for all.
    pub fn latency_samples(&self) -> usize {
        self.processors
            .first()
            .map(|p| p.latency_samples())
            .unwrap_or(0)
    }

    #[inline]
    fn pitch_ratio(&self) -> f32 {
        2.0_f32.powf(self.pitch_cents.load(Ordering::Acquire) / 1200.0)
    }
}

impl Clone for Unit {
    fn clone(&self) -> Self {
        Self {
            processors: self.processors.clone(),
            stretch_factor: Arc::new(AtomicF32::new(self.stretch_factor.load(Ordering::Acquire))),
            pitch_cents: Arc::new(AtomicF32::new(self.pitch_cents.load(Ordering::Acquire))),
            enabled: self.enabled,
            algorithm: self.algorithm,
            sample_rate: self.sample_rate,
            scratch_in: self.scratch_in.clone(),
            scratch_out: self.scratch_out.clone(),
        }
    }
}

impl AudioUnit for Unit {
    fn inputs(&self) -> usize {
        // A filter: it consumes the frame the caller feeds in (already tick'd
        // from the real clip source), one channel per processor.
        self.channels()
    }

    fn outputs(&self) -> usize {
        self.channels()
    }

    fn reset(&mut self) {
        for p in &mut self.processors {
            p.reset();
        }
    }

    fn set_sample_rate(&mut self, sample_rate: tutti_core::SampleRate) {
        let sample_rate: f64 = sample_rate.get();
        self.sample_rate = sample_rate;
        for p in &mut self.processors {
            p.set_sample_rate(tutti_core::SampleRate(sample_rate));
        }
    }

    fn tick(&mut self, input: &[f32], output: &mut [f32]) {
        // `input` is the source frame the caller already produced (in-RAM index
        // or streaming ring pop). This unit no longer owns/pulls a source.
        //
        // A short `input` fans channel 0 to the rest, matching the old stereo
        // behaviour (`input.get(1).unwrap_or(src_left)`) — a mono feed into a
        // wider stretcher stays audible on every channel rather than going
        // silent past the first.
        let n = self.channels().min(output.len());
        let src0 = input.first().copied().unwrap_or(0.0);
        let src = |c: usize| input.get(c).copied().unwrap_or(src0);

        if !self.is_processing() {
            for (c, o) in output.iter_mut().enumerate().take(n) {
                *o = src(c);
            }
            return;
        }

        let stretch = self.stretch_factor.load(Ordering::Acquire);
        let pitch_ratio = self.pitch_ratio();

        for (c, p) in self.processors.iter_mut().enumerate() {
            p.push_input(&[src(c)]);
            p.process(stretch, pitch_ratio);
        }

        let mut one = [0.0f32];
        for (c, o) in output.iter_mut().enumerate().take(n) {
            one[0] = 0.0;
            self.processors[c].pop_output(&mut one);
            *o = one[0];
        }
    }

    fn process(&mut self, size: usize, input: &BufferRef, output: &mut BufferMut) {
        // `size` past MAX_BUFFER_SIZE is clamped by `RtScratch::active`; the
        // fixed capacity makes a per-block reallocation impossible.
        //
        // `input` carries the source frames the caller already produced; this
        // unit reads them instead of tick'ing an owned source.
        //
        // One scratch pair per channel (not one flat strided buffer): the
        // phase-vocoder API is per-channel `push_input`/`pop_output` over a
        // contiguous run of samples, so a channel-major layout is what it wants.
        // A strided frame-major buffer would need a de-interleave here and a
        // re-interleave after, for no gain.
        let channels = self.channels();
        let in_ch = input.channels();

        for (c, s) in self.scratch_in.iter_mut().enumerate() {
            let buf = s.active(size);
            if c < in_ch {
                for (i, b) in buf.iter_mut().enumerate().take(size) {
                    *b = input.at_f32(c, i);
                }
            } else {
                // Fewer input channels than processors: mirror `tick`'s
                // fan-from-channel-0 rather than emitting silence.
                for (i, b) in buf.iter_mut().enumerate().take(size) {
                    *b = input.at_f32(0, i);
                }
            }
        }

        let out_ch = output.channels().min(channels);

        if !self.is_processing() {
            for c in 0..out_ch {
                let buf = self.scratch_in[c].active_ref(size);
                for (i, &s) in buf.iter().enumerate().take(size) {
                    output.set_f32(c, i, s);
                }
            }
            return;
        }

        let stretch = self.stretch_factor.load(Ordering::Acquire);
        let pitch_ratio = self.pitch_ratio();

        for (c, p) in self.processors.iter_mut().enumerate() {
            p.push_input(self.scratch_in[c].active_ref(size));
            p.process(stretch, pitch_ratio);
        }

        for c in 0..channels {
            let out = self.scratch_out[c].active(size);
            out.fill(0.0);
            let count = self.processors[c].pop_output(out);
            if c >= out_ch {
                continue;
            }
            for (i, &s) in out.iter().enumerate().take(size) {
                output.set_f32(c, i, if i < count { s } else { 0.0 });
            }
        }
    }

    audio_unit_boilerplate!(id = crate::node_id::TIME_STRETCH_ID);

    fn route(&mut self, input: &SignalFrame, _frequency: f64) -> SignalFrame {
        // As a filter, the incoming `input` frame IS the source signal. Width
        // must track `outputs()` or fundsp mis-plans this node's latency.
        let channels = self.channels();
        let mut out = SignalFrame::new(channels);
        let latency = self.latency_samples() as f64;
        let first = input.at(0).delay(latency);
        for c in 0..channels {
            let sig = if c < input.len() {
                input.at(c).delay(latency)
            } else {
                first
            };
            out.set(c, sig);
        }
        out
    }

    fn footprint(&self) -> usize {
        std::mem::size_of::<Self>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_phase_vocoder_creation() {
        let unit = Unit::new(44100.0);
        assert_eq!(unit.algorithm(), Algorithm::PhaseVocoder);
        assert_eq!(unit.inputs(), 2);
        assert_eq!(unit.outputs(), 2);
    }

    #[test]
    fn test_set_parameters() {
        let unit = Unit::new(44100.0);

        unit.set_stretch_factor(StretchFactor::new(2.0));
        assert!((unit.stretch_factor().get() - 2.0).abs() < 0.001);

        unit.set_pitch_cents(Cents::new(-200.0));
        assert!((unit.pitch_cents().get() - (-200.0)).abs() < 0.001);
    }

    #[test]
    fn test_parameter_clamping() {
        let unit = Unit::new(44100.0);

        unit.set_stretch_factor(StretchFactor::new(10.0));
        assert!((unit.stretch_factor().get() - 4.0).abs() < 0.001);

        unit.set_stretch_factor(StretchFactor::new(0.1));
        assert!((unit.stretch_factor().get() - 0.25).abs() < 0.001);
    }

    #[test]
    fn test_passthrough_mode() {
        // No stretch/pitch → the fed source frame passes straight through.
        let mut unit = Unit::new(44100.0);
        assert!(!unit.is_processing());

        let mut output = [0.0f32; 2];
        unit.tick(&[0.5, 0.5], &mut output);
        assert!((output[0] - 0.5).abs() < 0.001);
        assert!((output[1] - 0.5).abs() < 0.001);
    }

    #[test]
    fn test_enabled_flag() {
        let mut unit = Unit::new(44100.0);

        unit.set_stretch_factor(StretchFactor::new(2.0));
        assert!(unit.is_processing());

        unit.set_enabled(false);
        assert!(!unit.is_processing());

        unit.set_enabled(true);
        assert!(unit.is_processing());
    }

    #[test]
    fn test_clone() {
        let unit1 = Unit::new(44100.0);
        unit1.set_stretch_factor(StretchFactor::new(1.5));

        let unit2 = unit1.clone();
        assert!((unit2.stretch_factor().get() - 1.5).abs() < 0.001);

        unit1.set_stretch_factor(StretchFactor::new(2.0));
        assert!((unit1.stretch_factor().get() - 2.0).abs() < 0.001);
        assert!((unit2.stretch_factor().get() - 1.5).abs() < 0.001);
    }

    /// Width is declared, not inferred: `new` stays stereo so every existing
    /// call site keeps its arity, and `with_channels` is the explicit opt-in.
    #[test]
    fn new_is_stereo_and_with_channels_is_explicit() {
        let two = Unit::new(44_100.0);
        assert_eq!(two.channels(), 2);
        assert_eq!(two.inputs(), 2);
        assert_eq!(two.outputs(), 2);

        let six = Unit::with_channels(44_100.0, 6);
        assert_eq!(six.channels(), 6);
        assert_eq!(six.inputs(), 6);
        assert_eq!(six.outputs(), 6);
    }

    /// A zero-wide filter would make `inputs()`/`outputs()` lie to the graph.
    #[test]
    fn zero_width_is_clamped_to_one() {
        assert_eq!(Unit::with_channels(44_100.0, 0).channels(), 1);
    }

    /// `route` must agree with `outputs()`. If it does not, fundsp mis-plans this
    /// node's latency — which corrupts PDC without crashing or obviously
    /// mis-routing audio, so nothing else in the suite would notice.
    #[test]
    fn route_width_tracks_outputs_at_every_width() {
        for w in [1usize, 2, 6, 8] {
            let mut u = Unit::with_channels(44_100.0, w);
            let out = u.route(&SignalFrame::new(w), 44_100.0);
            assert_eq!(
                out.len(),
                u.outputs(),
                "route width {} != outputs {} at channels={w}",
                out.len(),
                u.outputs()
            );
        }
    }

    /// Bypass (`is_processing() == false`) must pass every channel through
    /// untouched, not just the front pair.
    #[test]
    fn six_channel_bypass_passes_all_channels_through() {
        let mut u = Unit::with_channels(44_100.0, 6);
        u.set_stretch_factor(StretchFactor::new(1.0));
        u.set_pitch_cents(Cents::new(0.0));
        assert!(!u.is_processing(), "unity stretch/pitch should bypass");

        let input: Vec<f32> = (0..6).map(|c| (c + 1) as f32).collect();
        let mut output = [0.0f32; 6];
        u.tick(&input, &mut output);

        for (c, &got) in output.iter().enumerate() {
            assert_eq!(
                got,
                (c + 1) as f32,
                "channel {c} did not pass through: {output:?}"
            );
        }
    }

    /// With stretching active, every channel must reach the output — a 6-channel
    /// clip through a stretcher that only ran two processors would silently lose
    /// four channels, and no stereo test can see that.
    ///
    /// The phase vocoder has FFT latency, so the first blocks are legitimately
    /// silent; this drives enough blocks to fill the pipeline and asserts that
    /// *some* energy arrives on every channel, not on an exact value.
    #[test]
    fn six_channel_stretch_reaches_every_channel() {
        let mut u = Unit::with_channels(44_100.0, 6);
        u.set_stretch_factor(StretchFactor::new(2.0));
        assert!(u.is_processing());

        let mut seen = [false; 6];
        let mut output = [0.0f32; 6];
        for n in 0..8192 {
            // Distinct per-channel tone so a cross-channel leak is not mistaken
            // for a correct read.
            let input: Vec<f32> = (0..6)
                .map(|c| ((n as f32) * 0.01 * (c + 1) as f32).sin())
                .collect();
            u.tick(&input, &mut output);
            for (c, &s) in output.iter().enumerate() {
                if s.abs() > 1e-6 {
                    seen[c] = true;
                }
            }
            if seen.iter().all(|&b| b) {
                break;
            }
        }
        assert!(
            seen.iter().all(|&b| b),
            "channels {:?} never produced output",
            seen.iter()
                .enumerate()
                .filter(|(_, &b)| !b)
                .map(|(c, _)| c)
                .collect::<Vec<_>>()
        );
    }

    /// A clone must carry the width, not silently reset to stereo — clones happen
    /// on every graph commit and per clip slot.
    #[test]
    fn clone_preserves_width() {
        let u = Unit::with_channels(44_100.0, 6);
        assert_eq!(u.clone().channels(), 6);
    }
}
