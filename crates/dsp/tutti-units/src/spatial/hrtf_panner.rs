//! Real HRTF binaural rendering backed by the `hrtf` crate.
//!
//! FFT convolution against a measured HRIR sphere: real spectral cues,
//! front/back and elevation disambiguation. This replaced the earlier crude
//! Woodworth ITD/ILD panner.
//!
//! ## The block-size bridge
//!
//! `hrtf::HrtfProcessor` is *block-based* — it consumes exactly
//! `interpolation_steps * block_len` mono samples per call and accumulates a
//! stereo result. tutti drives nodes at arbitrary `process` sizes (and a
//! 1-sample `tick`), so [`FrameBridge`] owns a pre-allocated input accumulator
//! and a stereo output queue. Input samples fill the accumulator; once a full
//! HRTF frame is buffered it is processed in one shot and the stereo result is
//! drained sample-by-sample. All buffers are allocated up front and only
//! indexed on the audio path, so processing stays allocation-free.
//!
//! The tradeoff is **latency**: output lags input by one HRTF frame
//! (`interpolation_steps * block_len` samples). The frame is kept small so the
//! delay is a few ms at typical rates.
//!
//! ## Composition
//!
//! [`HrtfBinaural`] is a thin orchestrator over four single-purpose parts:
//! [`HrirSource`] (the dataset + how to build a processor from it),
//! [`FrameBridge`] (the accumulate-then-drain ring), [`OverlapTails`] (the
//! per-frame convolution carry-over), and [`PositionSmoother`] (target → a
//! de-zippered direction vector).

use hrtf::{HrirSphere, HrtfContext, HrtfProcessor, Vec3};
use tutti_core::SampleRate;

use super::smoothing::{ExponentialSmoother, DEFAULT_POSITION_SMOOTH_TIME};

/// Samples per `hrtf` convolution block. Small: latency is
/// `INTERPOLATION_STEPS * BLOCK_LEN` samples (~10ms at 48kHz).
const BLOCK_LEN: usize = 128;
/// Cross-fade sub-steps the processor uses to de-click fast source motion.
const INTERPOLATION_STEPS: usize = 4;
/// Full mono frame the processor requires per `process_samples` call.
pub(crate) const FRAME_LEN: usize = INTERPOLATION_STEPS * BLOCK_LEN;

/// Errors constructing an HRTF renderer (bad or wrong-rate HRIR data).
#[derive(Debug, thiserror::Error)]
pub enum HrtfBinauralError {
    #[error("failed to load HRIR sphere: {0}")]
    Sphere(String),
}

/// The HRIR dataset plus the sample rate it is resampled to. Owns the one job
/// of turning bytes into an [`HrtfProcessor`] (which is itself not `Clone`),
/// so both construction and a sample-rate change go through [`Self::build`].
#[derive(Clone)]
struct HrirSource {
    bytes: Vec<u8>,
    sample_rate: u32,
}

impl HrirSource {
    fn new(bytes: &[u8], sample_rate: u32) -> Self {
        Self {
            bytes: bytes.to_vec(),
            sample_rate,
        }
    }

    fn build(&self) -> Result<HrtfProcessor, HrtfBinauralError> {
        let sphere = HrirSphere::new(&self.bytes[..], self.sample_rate)
            .map_err(|e| HrtfBinauralError::Sphere(format!("{e:?}")))?;
        Ok(HrtfProcessor::new(sphere, INTERPOLATION_STEPS, BLOCK_LEN))
    }
}

/// Bridges tutti's arbitrary-size / per-sample calls to `hrtf`'s fixed
/// [`FRAME_LEN`] blocks: accumulate a full mono frame in, drain the stereo
/// frame out. Buffers are allocated once; the audio path only indexes them.
///
/// Invariants: `in_fill <= FRAME_LEN` and `out_cursor <= out.len()`.
struct FrameBridge {
    /// Mono input accumulated toward the next full frame.
    input: Vec<f32>,
    in_fill: usize,
    /// Stereo output of the last processed frame, drained per sample.
    output: Vec<(f32, f32)>,
    out_cursor: usize,
}

impl FrameBridge {
    fn new() -> Self {
        Self {
            input: vec![0.0; FRAME_LEN],
            in_fill: 0,
            // Pre-fill one frame of silent output so early reads return 0.0
            // rather than starving; keeps the node causal from sample 0.
            output: vec![(0.0, 0.0); FRAME_LEN],
            out_cursor: 0,
        }
    }

    fn reset(&mut self) {
        self.input.iter_mut().for_each(|s| *s = 0.0);
        self.in_fill = 0;
        self.output.iter_mut().for_each(|s| *s = (0.0, 0.0));
        self.out_cursor = 0;
    }

    /// Push one mono sample; returns `true` when a full frame is ready to
    /// render (the caller then fills [`Self::output`] and calls [`Self::rewind`]).
    #[inline]
    fn push(&mut self, mono: f32) -> bool {
        self.input[self.in_fill] = mono;
        self.in_fill += 1;
        self.in_fill >= FRAME_LEN
    }

    /// Reset the fill/drain cursors after a frame has been rendered.
    #[inline]
    fn rewind(&mut self) {
        self.in_fill = 0;
        self.out_cursor = 0;
    }

    /// Borrow the full input frame and a freshly-zeroed output frame together
    /// (a split borrow, so both are live at once). Output is zeroed because
    /// `hrtf` *accumulates* into it and must start clean.
    #[inline]
    fn frames(&mut self) -> (&[f32], &mut [(f32, f32)]) {
        self.output.iter_mut().for_each(|s| *s = (0.0, 0.0));
        (&self.input, &mut self.output)
    }

    /// Drain the next stereo output sample, or silence once the frame is spent.
    #[inline]
    fn drain(&mut self) -> (f32, f32) {
        let out = self
            .output
            .get(self.out_cursor)
            .copied()
            .unwrap_or((0.0, 0.0));
        if self.out_cursor < self.output.len() {
            self.out_cursor += 1;
        }
        out
    }
}

/// The overlap-add carry-over `hrtf` threads between consecutive frames: the
/// left/right convolution tails plus the direction of the previous frame (so
/// the processor can cross-fade motion).
struct OverlapTails {
    left: Vec<f32>,
    right: Vec<f32>,
    prev_dir: Vec3,
}

impl OverlapTails {
    fn new() -> Self {
        Self {
            left: vec![0.0; BLOCK_LEN],
            right: vec![0.0; BLOCK_LEN],
            prev_dir: forward(),
        }
    }

    fn reset(&mut self) {
        self.left.iter_mut().for_each(|s| *s = 0.0);
        self.right.iter_mut().for_each(|s| *s = 0.0);
        self.prev_dir = forward();
    }
}

/// Turns a target azimuth/elevation (degrees) into a de-zippered direction
/// vector: one-pole smoothers on each angle, stepped once per rendered frame.
struct PositionSmoother {
    azimuth: ExponentialSmoother,
    elevation: ExponentialSmoother,
    target_azimuth: f32,
    target_elevation: f32,
}

impl PositionSmoother {
    fn new(sample_rate: u32) -> Self {
        Self {
            azimuth: ExponentialSmoother::new(DEFAULT_POSITION_SMOOTH_TIME, sr(sample_rate)),
            elevation: ExponentialSmoother::new(DEFAULT_POSITION_SMOOTH_TIME, sr(sample_rate)),
            target_azimuth: 0.0,
            target_elevation: 0.0,
        }
    }

    #[inline]
    fn aim_at(&mut self, azimuth: f32, elevation: f32) {
        self.target_azimuth = azimuth;
        self.target_elevation = elevation;
    }

    fn retune(&mut self, sample_rate: u32) {
        self.azimuth.set_sample_rate(sr(sample_rate));
        self.elevation.set_sample_rate(sr(sample_rate));
    }

    /// Advance both smoothers one frame and return the smoothed direction.
    #[inline]
    fn step(&mut self) -> Vec3 {
        let az = self.azimuth.process(self.target_azimuth);
        let el = self.elevation.process(self.target_elevation);
        direction_from_degrees(az, el)
    }
}

/// FFT-convolution binaural renderer: a thin orchestrator over the four parts
/// above. `set_position`/`process_sample` are the audio-path surface.
pub(crate) struct HrtfBinaural {
    processor: HrtfProcessor,
    source: HrirSource,
    bridge: FrameBridge,
    tails: OverlapTails,
    aim: PositionSmoother,
}

impl HrtfBinaural {
    pub(crate) fn new(hrir_bytes: &[u8], sample_rate: f32) -> Result<Self, HrtfBinauralError> {
        let source = HrirSource::new(hrir_bytes, sample_rate as u32);
        let processor = source.build()?;
        Ok(Self {
            processor,
            aim: PositionSmoother::new(source.sample_rate),
            source,
            bridge: FrameBridge::new(),
            tails: OverlapTails::new(),
        })
    }

    /// Lock-free-ish position update (called from the audio path via the node).
    #[inline]
    pub(crate) fn set_position(&mut self, azimuth: f32, elevation: f32) {
        self.aim.aim_at(azimuth, elevation);
    }

    pub(crate) fn set_sample_rate(&mut self, sample_rate: f32) {
        let sample_rate = sample_rate as u32;
        if sample_rate == self.source.sample_rate {
            return;
        }
        // Rebuilding is a non-RT reconfigure (mirrors the old node rebuilding
        // its whole panner in `set_sample_rate`); resampling the sphere here
        // rather than on the audio path keeps the callback clean.
        let candidate = HrirSource::new(&self.source.bytes, sample_rate);
        if let Ok(processor) = candidate.build() {
            self.processor = processor;
            self.source = candidate;
            self.aim.retune(sample_rate);
            self.reset_state();
        }
    }

    /// Zero the streaming buffers without rebuilding the processor.
    pub(crate) fn reset_state(&mut self) {
        self.bridge.reset();
        self.tails.reset();
    }

    /// Push one mono input sample, return the current delayed stereo output.
    #[inline]
    pub(crate) fn process_sample(&mut self, mono: f32) -> (f32, f32) {
        if self.bridge.push(mono) {
            self.render_frame();
            self.bridge.rewind();
        }
        self.bridge.drain()
    }

    /// Convolve the accumulated frame; result lands in the bridge's output.
    fn render_frame(&mut self) {
        // One direction per frame from the smoothed position — `hrtf` takes a
        // single (prev, new) direction pair per `process_samples` call.
        let new_dir = self.aim.step();
        let (source, output) = self.bridge.frames();

        let context = HrtfContext {
            source,
            output,
            new_sample_vector: new_dir,
            prev_sample_vector: self.tails.prev_dir,
            prev_left_samples: &mut self.tails.left,
            prev_right_samples: &mut self.tails.right,
            new_distance_gain: 1.0,
            prev_distance_gain: 1.0,
        };
        self.processor.process_samples(context);
        self.tails.prev_dir = new_dir;
    }
}

impl Clone for HrtfBinaural {
    fn clone(&self) -> Self {
        // Rebuild from the retained source; a fresh renderer with cleared
        // streaming state is the correct clone (state is per-voice, not shared).
        Self::new(&self.source.bytes, self.source.sample_rate as f32)
            .expect("HRIR bytes validated at first construction")
    }
}

#[inline]
fn sr(sample_rate: u32) -> SampleRate {
    SampleRate(sample_rate as f64)
}

/// Unit vector straight ahead (listener forward), in the sphere's right-handed
/// frame: +x right, +y up, +z toward the listener's back.
#[inline]
fn forward() -> Vec3 {
    Vec3 {
        x: 0.0,
        y: 0.0,
        z: -1.0,
    }
}

/// Map azimuth/elevation in degrees to a unit direction `Vec3`.
///
/// Convention (matching the rest of `spatial`): azimuth 0 = front, +90 = left;
/// elevation 0 = ear level, +90 = up. Right-handed output frame.
#[inline]
fn direction_from_degrees(azimuth_deg: f32, elevation_deg: f32) -> Vec3 {
    let az = azimuth_deg.to_radians();
    let el = elevation_deg.to_radians();
    let cos_el = el.cos();
    Vec3 {
        // +90° azimuth (left) → -x; front (0°) → -z.
        x: -az.sin() * cos_el,
        y: el.sin(),
        z: -az.cos() * cos_el,
    }
}
