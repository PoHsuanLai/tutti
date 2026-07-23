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
//! 1-sample `tick`), so this type owns a pre-allocated input accumulator and a
//! stereo output queue. Input samples fill the accumulator; once a full HRTF
//! frame is buffered it is processed in one shot and the stereo result is
//! drained sample-by-sample. All buffers are allocated in [`HrtfBinaural::new`]
//! and only indexed on the audio path, so processing stays allocation-free.
//!
//! The tradeoff is **latency**: output lags input by one HRTF frame
//! (`interpolation_steps * block_len` samples). The frame is kept small so the
//! delay is a few ms at typical rates.

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

/// FFT-convolution binaural renderer with a block-size bridge.
pub(crate) struct HrtfBinaural {
    processor: HrtfProcessor,
    /// Original HRIR bytes, kept so [`Clone`] and sample-rate changes can
    /// rebuild the processor (which is not itself `Clone`).
    hrir_bytes: Vec<u8>,
    sample_rate: u32,

    /// Mono input accumulated toward the next full `FRAME_LEN` frame.
    in_accum: Vec<f32>,
    in_fill: usize,
    /// Stereo output produced by the last processed frame, drained per sample.
    out_queue: Vec<(f32, f32)>,
    out_cursor: usize,

    /// Overlap-add tails the processor carries between frames (caller-owned).
    prev_left: Vec<f32>,
    prev_right: Vec<f32>,
    /// Direction of the *previous* frame, so the processor cross-fades motion.
    prev_dir: Vec3,

    /// Smoothed azimuth/elevation (degrees) → de-zippered direction per frame.
    azimuth_smoother: ExponentialSmoother,
    elevation_smoother: ExponentialSmoother,
    target_azimuth: f32,
    target_elevation: f32,
}

impl HrtfBinaural {
    pub(crate) fn new(hrir_bytes: &[u8], sample_rate: f32) -> Result<Self, HrtfBinauralError> {
        let sample_rate = sample_rate as u32;
        let processor = Self::build_processor(hrir_bytes, sample_rate)?;
        Ok(Self {
            processor,
            hrir_bytes: hrir_bytes.to_vec(),
            sample_rate,
            in_accum: vec![0.0; FRAME_LEN],
            in_fill: 0,
            // Pre-fill one frame of silent output so early `tick`s read 0.0
            // rather than starving; keeps the node causal from sample 0.
            out_queue: vec![(0.0, 0.0); FRAME_LEN],
            out_cursor: 0,
            prev_left: vec![0.0; BLOCK_LEN],
            prev_right: vec![0.0; BLOCK_LEN],
            prev_dir: forward(),
            azimuth_smoother: ExponentialSmoother::new(
                DEFAULT_POSITION_SMOOTH_TIME,
                SampleRate(sample_rate as f64),
            ),
            elevation_smoother: ExponentialSmoother::new(
                DEFAULT_POSITION_SMOOTH_TIME,
                SampleRate(sample_rate as f64),
            ),
            target_azimuth: 0.0,
            target_elevation: 0.0,
        })
    }

    fn build_processor(
        hrir_bytes: &[u8],
        sample_rate: u32,
    ) -> Result<HrtfProcessor, HrtfBinauralError> {
        let sphere = HrirSphere::new(hrir_bytes, sample_rate)
            .map_err(|e| HrtfBinauralError::Sphere(format!("{e:?}")))?;
        Ok(HrtfProcessor::new(sphere, INTERPOLATION_STEPS, BLOCK_LEN))
    }

    /// Lock-free-ish position update (called from the audio path via the node).
    #[inline]
    pub(crate) fn set_position(&mut self, azimuth: f32, elevation: f32) {
        self.target_azimuth = azimuth;
        self.target_elevation = elevation;
    }

    pub(crate) fn set_sample_rate(&mut self, sample_rate: f32) {
        let sample_rate = sample_rate as u32;
        if sample_rate == self.sample_rate {
            return;
        }
        // Rebuilding is a non-RT reconfigure (mirrors the old node rebuilding
        // its whole panner in `set_sample_rate`); resampling the sphere here
        // rather than on the audio path keeps the callback clean.
        if let Ok(processor) = Self::build_processor(&self.hrir_bytes, sample_rate) {
            self.processor = processor;
            self.sample_rate = sample_rate;
            self.azimuth_smoother
                .set_sample_rate(SampleRate(sample_rate as f64));
            self.elevation_smoother
                .set_sample_rate(SampleRate(sample_rate as f64));
            self.reset_state();
        }
    }

    /// Zero the streaming buffers without rebuilding the processor.
    pub(crate) fn reset_state(&mut self) {
        self.in_accum.iter_mut().for_each(|s| *s = 0.0);
        self.in_fill = 0;
        self.out_queue.iter_mut().for_each(|s| *s = (0.0, 0.0));
        self.out_cursor = 0;
        self.prev_left.iter_mut().for_each(|s| *s = 0.0);
        self.prev_right.iter_mut().for_each(|s| *s = 0.0);
        self.prev_dir = forward();
    }

    /// Push one mono input sample, return the current delayed stereo output.
    #[inline]
    pub(crate) fn process_sample(&mut self, mono: f32) -> (f32, f32) {
        self.in_accum[self.in_fill] = mono;
        self.in_fill += 1;
        if self.in_fill >= FRAME_LEN {
            self.render_frame();
            self.in_fill = 0;
            self.out_cursor = 0;
        }

        // Advance the smoothers per output sample so the ramp is rate-correct.
        let out = self
            .out_queue
            .get(self.out_cursor)
            .copied()
            .unwrap_or((0.0, 0.0));
        if self.out_cursor < self.out_queue.len() {
            self.out_cursor += 1;
        }
        out
    }

    /// Convolve the accumulated frame; result lands in `out_queue`.
    fn render_frame(&mut self) {
        // One direction per frame from the smoothed position. Stepping the
        // smoother once per output sample would be finer, but `hrtf` takes a
        // single (prev, new) direction pair per frame, so we step it `FRAME_LEN`
        // times' worth here by advancing to the target with a per-frame coeff.
        let smoothed_az = self.azimuth_smoother.process(self.target_azimuth);
        let smoothed_el = self.elevation_smoother.process(self.target_elevation);
        let new_dir = direction_from_degrees(smoothed_az, smoothed_el);

        // hrtf ACCUMULATES into output — zero it first.
        self.out_queue.iter_mut().for_each(|s| *s = (0.0, 0.0));

        let context = HrtfContext {
            source: &self.in_accum,
            output: &mut self.out_queue,
            new_sample_vector: new_dir,
            prev_sample_vector: self.prev_dir,
            prev_left_samples: &mut self.prev_left,
            prev_right_samples: &mut self.prev_right,
            new_distance_gain: 1.0,
            prev_distance_gain: 1.0,
        };
        self.processor.process_samples(context);
        self.prev_dir = new_dir;
    }
}

impl Clone for HrtfBinaural {
    fn clone(&self) -> Self {
        // Rebuild from the retained bytes; a fresh renderer with cleared
        // streaming state is the correct clone (state is per-voice, not shared).
        Self::new(&self.hrir_bytes, self.sample_rate as f32)
            .expect("HRIR bytes validated at first construction")
    }
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
