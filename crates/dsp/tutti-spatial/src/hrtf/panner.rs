//! The binaural convolver: overlap-save FFT convolution against the HRIR
//! blended for the source's direction.
//!
//! ## Steps
//!
//! Input accumulates into steps of [`STEP`] frames. When a step is full it is
//! rendered in one go and its output drains over the next step, so output
//! lags input by exactly [`LATENCY`] = `STEP` frames whatever the block size.
//! A step costs one forward FFT of the input history and one inverse FFT of
//! the product with the blended spectrum (both ears at once: see
//! [`HrtfSet`]), plus a second inverse FFT while the direction moves.
//!
//! ## Direction changes
//!
//! The commanded position is de-zippered by an [`AngleSmoother`] stepped
//! once per step. Each step's direction is blended across the sphere face it
//! crosses (barycentric weights over the face's three vertices). When the
//! blend differs from the previous step's, the step is rendered through both
//! filters and crossfaded linearly across it, so the output moves from one
//! filter to the next with no jump. Overlap-save keeps no filter state
//! between steps (each step's output is computed from the input history
//! alone), so both renders are exact and the crossfade is all there is to a
//! change.

use std::sync::Arc;

use rustfft::num_complex::Complex32;
use tutti_core::{fold_frame_to_mono, Azimuth, Elevation, Mix, Radians, SampleRate, Samples};

use super::sphere::{HrirSphere, HrtfSet, Weights};
use crate::AngleSmoother;

/// Frames per convolution step, and the crossfade length of a direction
/// change.
pub(crate) const STEP: usize = 128;

/// How many frames output lags input: what the node reports to PDC.
///
/// A sample written at position `k` of a step leaves when position `k` of the
/// next step is read, `STEP` frames later. The HRIR's own onset (the
/// ear-to-ear delay that *is* the spatial cue) is signal, like a convolution
/// reverb's pre-delay, and is not latency.
pub(crate) const LATENCY: usize = STEP;

/// The error returned when an [`HrtfBinauralNode`](crate::HrtfBinauralNode)
/// cannot be built from its HRIR data.
#[derive(Debug, thiserror::Error)]
pub enum HrtfBinauralError {
    /// The bytes do not start with the `HRIR` magic.
    #[error("not an HRIR sphere: the bytes do not start with `HRIR`")]
    NotASphere,
    /// The bytes end before the sphere their header describes.
    #[error("HRIR sphere truncated: {needed} bytes needed, {found} present")]
    Truncated {
        /// Bytes the header implies (saturating at `usize::MAX`).
        needed: usize,
        /// Bytes given.
        found: usize,
    },
    /// The bytes run past the sphere their header describes.
    #[error("HRIR sphere has trailing bytes: {size} bytes describe it, {found} present")]
    Trailing {
        /// Bytes the header implies.
        size: usize,
        /// Bytes given.
        found: usize,
    },
    /// The header describes no usable sphere: a zero length or vertex count,
    /// or an index count that is not a whole number of triangles.
    #[error("bad HRIR sphere header: {0}")]
    Header(&'static str),
    /// The sphere's sample rate is outside [`MIN_SPHERE_RATE`] to
    /// [`MAX_SPHERE_RATE`] Hz. Resampling scales the response length by the
    /// ratio of the rates, so the bound keeps a file from asking for a
    /// response thousands of times its own size.
    #[error("HRIR sphere rate {rate} Hz is outside {MIN_SPHERE_RATE}..={MAX_SPHERE_RATE} Hz")]
    Rate {
        /// The rate the header gives, in Hz.
        rate: u32,
    },
    /// The responses last longer than 50 ms at the sphere's rate
    /// (`len > rate / 20`). Measured HRIRs last a few milliseconds. The
    /// bound keeps what preparing the sphere allocates and computes per
    /// vertex in proportion to an HRIR: a set prepared at any rate holds at
    /// most 50 ms of frames there, rounded up.
    #[error("HRIR responses are {len} frames, past the {max} (50 ms) the sphere's rate allows")]
    ResponseTooLong {
        /// The response length the header gives, in frames.
        len: usize,
        /// The longest the sphere's rate allows, in frames.
        max: usize,
    },
    /// A face names a vertex the sphere does not have.
    #[error("face {face} names vertex {vertex}, but the sphere has {vertices} vertices")]
    FaceIndex {
        /// The face's index.
        face: usize,
        /// The vertex index it names.
        vertex: u32,
        /// How many vertices the sphere has.
        vertices: usize,
    },
    /// A vertex's distance from the origin is outside `1e-6..=1e6`, or one
    /// of its response samples is non-finite or larger in magnitude than
    /// `1e6`. (A position too close to the origin normalizes to NaN; a
    /// sample near `f32::MAX` overflows the FFT.)
    #[error("vertex {vertex} is too near or far from the origin, or holds an out-of-range value")]
    Vertex {
        /// The vertex's index.
        vertex: usize,
    },
    /// No face is a usable triangle: there are none, or each one's plane
    /// passes through the origin, so no direction can be blended.
    #[error("the HRIR sphere has no usable face")]
    NoFace,
}

/// The lowest sphere rate [`HrtfBinauralNode::new`](crate::HrtfBinauralNode::new)
/// accepts, in Hz.
pub const MIN_SPHERE_RATE: u32 = 8_000;

/// The highest sphere rate [`HrtfBinauralNode::new`](crate::HrtfBinauralNode::new)
/// accepts, in Hz.
pub const MAX_SPHERE_RATE: u32 = 384_000;

/// The highest rate the sphere is prepared at, in Hz. A node built or
/// prepared above it renders with the set prepared at this rate, so a
/// prepared response is at most `MAX_PREPARED_RATE / 20` frames (50 ms
/// here) whatever rate the graph asks for: an unbounded one (`1e10`) once
/// sized each vertex's spectrum at 2^26 values and aborted on allocation.
/// 768 kHz is twice the highest rate audio hardware runs at. Above it the
/// HRIR is applied time-compressed by `rate / 768 kHz`, which shifts its
/// spectral cues up by that factor; the de-zipper still glides in the
/// graph's own time.
pub const MAX_PREPARED_RATE: u32 = 768_000;

/// The sphere's rate vocabulary: an integral Hz, as the file stores it. It is
/// also the identity a rebuild compares on (an integer equality, not a float
/// one); no HRIR dataset ships at a fractional rate. At least 1 Hz, so a
/// degenerate rate still prepares a (useless but well-formed) set, and at
/// most [`MAX_PREPARED_RATE`]. A NaN rate prepares at 1 Hz.
fn rate_key(sample_rate: SampleRate) -> u32 {
    // `max` before `min`, not `clamp`: `max` takes 1 over a NaN.
    sample_rate
        .get()
        .round()
        .max(1.0)
        .min(f64::from(MAX_PREPARED_RATE)) as u32
}

/// The streaming convolver for one source.
pub(crate) struct HrtfBinaural {
    sphere: Arc<HrirSphere>,
    set: Arc<HrtfSet>,
    /// The input history, `fft_len` frames: the last `fft_len - STEP` frames
    /// already rendered, then the step being filled.
    history: Vec<f32>,
    /// Frames of the current step filled so far, `0..STEP`.
    fill: usize,
    /// The last rendered step, drained while the next one fills: the wet
    /// pair and the mono input it was rendered from (the dry path, equally
    /// late).
    wet_left: Vec<f32>,
    wet_right: Vec<f32>,
    dry: Vec<f32>,
    spectrum: Vec<Complex32>,
    /// The previous filter's product while a direction change crossfades.
    spectrum_prev: Vec<Complex32>,
    scratch: Vec<Complex32>,
    /// The blend the last step ended on.
    current: Weights,
    face_hint: usize,
    aim: AngleSmoother,
}

impl HrtfBinaural {
    /// Parses `hrir_bytes` and prepares the sphere at `sample_rate`. Control
    /// thread.
    pub(crate) fn new(
        hrir_bytes: &[u8],
        sample_rate: SampleRate,
    ) -> Result<Self, HrtfBinauralError> {
        let sphere = Arc::new(HrirSphere::parse(hrir_bytes)?);
        let set = Arc::new(HrtfSet::build(&sphere, rate_key(sample_rate), STEP));
        Ok(Self::over(
            sphere,
            set,
            AngleSmoother::new(sample_rate, Samples(STEP)),
        ))
    }

    /// A renderer over a prepared set, with cleared streaming state, aimed
    /// wherever `aim` is.
    fn over(sphere: Arc<HrirSphere>, set: Arc<HrtfSet>, aim: AngleSmoother) -> Self {
        let n = set.fft_len;
        let zero = Complex32::new(0.0, 0.0);
        let mut face_hint = 0;
        let (az, el) = aim.current();
        let current = set.weights(direction_from_degrees(az, el), &mut face_hint);
        Self {
            history: vec![0.0; n],
            fill: 0,
            wet_left: vec![0.0; STEP],
            wet_right: vec![0.0; STEP],
            dry: vec![0.0; STEP],
            spectrum: vec![zero; n],
            spectrum_prev: vec![zero; n],
            scratch: vec![zero; set.scratch_len],
            current,
            face_hint,
            aim,
            sphere,
            set,
        }
    }

    /// Frames that outlive a silent input once the [`LATENCY`] is
    /// compensated: the response spreads a sample `ir_len - 1` frames past
    /// the frame it leaves at. A tail excludes latency, so the step's delay
    /// is not counted here. The length is fixed at a rate, so this is exact.
    pub(crate) fn ring_out(&self) -> usize {
        self.set.ir_len - 1
    }

    /// Prepares the sphere at a new rate and clears the streaming state,
    /// seating the de-zipper on `target`. At the set's current rate it only
    /// retunes the de-zipper (allocation-free), so a re-prepare for a new
    /// block size keeps the tail in flight. Two graph rates above
    /// [`MAX_PREPARED_RATE`] share one set but not one glide coefficient.
    /// Allocates on a new rate: control thread only.
    pub(crate) fn set_sample_rate(
        &mut self,
        sample_rate: SampleRate,
        target: (Azimuth, Elevation),
    ) {
        let rate = rate_key(sample_rate);
        if rate == self.set.rate {
            self.aim.set_sample_rate(sample_rate);
            return;
        }
        let set = HrtfSet::build(&self.sphere, rate, STEP);
        let mut aim = self.aim.clone();
        aim.set_sample_rate(sample_rate);
        aim.reset_to_target(target.0, target.1);
        *self = Self::over(self.sphere.clone(), Arc::new(set), aim);
    }

    /// Where the de-zipper is aimed now: the direction the last step
    /// rendered.
    #[cfg(test)]
    pub(crate) fn aim(&self) -> (Azimuth, Elevation) {
        self.aim.current()
    }

    /// Zero the streaming buffers and seat the de-zipper on `target`, so the
    /// next step renders there with no crossfade in from elsewhere.
    pub(crate) fn reset_state(&mut self, target: (Azimuth, Elevation)) {
        self.history.fill(0.0);
        self.fill = 0;
        self.wet_left.fill(0.0);
        self.wet_right.fill(0.0);
        self.dry.fill(0.0);
        self.aim.reset_to_target(target.0, target.1);
        let (az, el) = self.aim.current();
        self.current = self
            .set
            .weights(direction_from_degrees(az, el), &mut self.face_hint);
    }

    /// Renders a block: the input pair is folded to mono, and each output is
    /// `blend` between the delayed dry mono and the binaural pair.
    /// `target` is the commanded position, read once per block.
    /// Allocation-free.
    pub(crate) fn process(
        &mut self,
        target: (Azimuth, Elevation),
        blend: Mix,
        input: (&[f32], &[f32]),
        output: (&mut [f32], &mut [f32]),
    ) {
        let frames = output.0.len();
        let start = self.history.len() - STEP;
        let mut done = 0;
        while done < frames {
            let n = (frames - done).min(STEP - self.fill);
            let at = self.fill;
            let ins = (&input.0[done..done + n], &input.1[done..done + n]);
            let outs = (&mut output.0[done..done + n], &mut output.1[done..done + n]);
            let wet = (&self.wet_left[at..at + n], &self.wet_right[at..at + n]);
            let dry = &self.dry[at..at + n];
            let fresh = &mut self.history[start + at..start + at + n];
            for i in 0..n {
                outs.0[i] = blend.blend(dry[i], wet.0[i]);
                outs.1[i] = blend.blend(dry[i], wet.1[i]);
                fresh[i] = fold_frame_to_mono(&[ins.0[i], ins.1[i]]);
            }
            self.fill += n;
            done += n;
            if self.fill == STEP {
                self.render_step(target);
                self.fill = 0;
            }
        }
    }

    /// Convolve the full step into the wet buffers and slide the history.
    fn render_step(&mut self, target: (Azimuth, Elevation)) {
        let (az, el) = self.aim.step(target.0, target.1);
        let next = self
            .set
            .weights(direction_from_degrees(az, el), &mut self.face_hint);
        let set = &*self.set;
        let start = set.fft_len - STEP;
        self.dry.copy_from_slice(&self.history[start..]);

        for (s, &x) in self.spectrum.iter_mut().zip(&self.history) {
            *s = Complex32::new(x, 0.0);
        }
        set.forward
            .process_with_scratch(&mut self.spectrum, &mut self.scratch);

        let [a, b, c] = next.verts.map(|v| set.spectrum(v));
        let [wa, wb, wc] = next.w;
        if next == self.current {
            for (k, s) in self.spectrum.iter_mut().enumerate() {
                *s *= a[k] * wa + b[k] * wb + c[k] * wc;
            }
            set.inverse
                .process_with_scratch(&mut self.spectrum, &mut self.scratch);
            let out = &self.spectrum[start..];
            for ((l, r), y) in self.wet_left.iter_mut().zip(&mut self.wet_right).zip(out) {
                *l = y.re;
                *r = y.im;
            }
        } else {
            let [pa, pb, pc] = self.current.verts.map(|v| set.spectrum(v));
            let [qa, qb, qc] = self.current.w;
            for (k, (s, p)) in self
                .spectrum
                .iter_mut()
                .zip(&mut self.spectrum_prev)
                .enumerate()
            {
                let x = *s;
                *p = x * (pa[k] * qa + pb[k] * qb + pc[k] * qc);
                *s = x * (a[k] * wa + b[k] * wb + c[k] * wc);
            }
            set.inverse
                .process_with_scratch(&mut self.spectrum, &mut self.scratch);
            set.inverse
                .process_with_scratch(&mut self.spectrum_prev, &mut self.scratch);
            let new = &self.spectrum[start..];
            let old = &self.spectrum_prev[start..];
            let step = 1.0 / STEP as f32;
            for (i, ((l, r), (y, z))) in self
                .wet_left
                .iter_mut()
                .zip(&mut self.wet_right)
                .zip(new.iter().zip(old))
                .enumerate()
            {
                // Reaches the new filter on the step's last frame, so the
                // next step continues from it.
                let t = (i + 1) as f32 * step;
                *l = z.re + (y.re - z.re) * t;
                *r = z.im + (y.im - z.im) * t;
            }
            self.current = next;
        }
        self.history.copy_within(STEP.., 0);
    }
}

impl Clone for HrtfBinaural {
    /// Shares the parsed sphere and its prepared set (nothing is reparsed or
    /// re-planned), with cleared streaming state and the same de-zipper
    /// position. Allocates the streaming buffers: control thread.
    fn clone(&self) -> Self {
        Self::over(self.sphere.clone(), self.set.clone(), self.aim.clone())
    }
}

/// Map azimuth/elevation in degrees to a unit direction, in the sphere's
/// right-handed frame: +x right, +y up, +z toward the listener's back.
///
/// Convention (matching the rest of `spatial`): azimuth 0 = front, +90 = left;
/// elevation 0 = ear level, +90 = up. Takes the typed angles rather than two
/// bare `f32`s: transposed, they would compile and put the source somewhere
/// else on the sphere.
#[inline]
fn direction_from_degrees(azimuth: Azimuth, elevation: Elevation) -> [f32; 3] {
    let az = Radians::from(azimuth);
    let el = Radians::from(elevation);
    let cos_el = el.cos();
    // +90° azimuth (left) → -x; front (0°) → -z.
    [-az.sin() * cos_el, el.sin(), -az.cos() * cos_el]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tetrahedron with delta responses: the smallest sphere that parses.
    fn tetrahedron(rate: u32) -> Vec<u8> {
        let mut b = b"HRIR".to_vec();
        for word in [rate, 4, 4, 12] {
            b.extend_from_slice(&word.to_le_bytes());
        }
        for idx in [0u32, 1, 2, 0, 3, 1, 0, 2, 3, 1, 3, 2] {
            b.extend_from_slice(&idx.to_le_bytes());
        }
        for v in [
            [1.0f32, 1.0, 1.0],
            [-1.0, -1.0, 1.0],
            [-1.0, 1.0, -1.0],
            [1.0, -1.0, -1.0],
        ] {
            for x in v
                .into_iter()
                .chain([1.0, 0.0, 0.0, 0.0, 0.5, 0.0, 0.0, 0.0])
            {
                b.extend_from_slice(&x.to_le_bytes());
            }
        }
        b
    }

    /// A clone (the fork's template, and every fork) shares the parsed
    /// sphere and its prepared spectra; only a new rate prepares a new set,
    /// and a re-prepare at the same rate keeps the one it has.
    ///
    /// Mutations (run): `Clone` preparing its own set from the shared sphere
    /// → the first set `ptr_eq` fails; dropping `set_sample_rate`'s
    /// same-rate early return → the same-rate assertion fails.
    #[test]
    fn a_clone_shares_the_sphere_and_its_spectra() {
        let rate = SampleRate(48_000.0);
        let front = (Azimuth::FRONT, Elevation::LEVEL);
        let mut a = HrtfBinaural::new(&tetrahedron(48_000), rate).expect("parses");
        let b = a.clone();
        assert!(Arc::ptr_eq(&a.sphere, &b.sphere));
        assert!(Arc::ptr_eq(&a.set, &b.set));

        a.set_sample_rate(rate, front);
        assert!(Arc::ptr_eq(&a.set, &b.set), "a same-rate prepare rebuilt");
        a.set_sample_rate(SampleRate(96_000.0), front);
        assert!(Arc::ptr_eq(&a.sphere, &b.sphere));
        assert!(!Arc::ptr_eq(&a.set, &b.set));
        assert_eq!(a.set.rate, 96_000);
    }

    /// Two graph rates above [`MAX_PREPARED_RATE`] share one prepared set,
    /// but the de-zipper still glides in the graph's own time: built at
    /// 1.536 MHz and prepared at 1.024 MHz, 50 ms is 51 200 samples, exactly
    /// 400 steps, and leaves `e^-1` of a move. Left at 1.536 MHz's
    /// coefficient (600 steps per 50 ms) it would leave `e^-(2/3)` = 0.51.
    ///
    /// Mutation (run): drop `self.aim.set_sample_rate(sample_rate)` from
    /// `set_sample_rate`'s same-set branch → 0.513 of the move is left and
    /// the glide assertion fails.
    #[test]
    fn a_prepare_above_the_ceiling_retunes_the_glide() {
        assert_eq!(STEP, 128, "the rates below assume 128-sample steps");
        let front = (Azimuth::FRONT, Elevation::LEVEL);
        let mut a =
            HrtfBinaural::new(&tetrahedron(48_000), SampleRate(1_536_000.0)).expect("parses");
        let set = a.set.clone();
        a.set_sample_rate(SampleRate(1_024_000.0), front);
        assert!(Arc::ptr_eq(&a.set, &set), "both rates key to one set");
        assert_eq!(a.set.rate, MAX_PREPARED_RATE);

        let e_inv = (-1.0f32).exp();
        let left = remaining_after(&mut a.aim, 400);
        assert!(
            (left - e_inv).abs() < 1e-4,
            "after 50 ms at 1.024 MHz {left} of the move is left, expected e^-1 = {e_inv}"
        );
    }

    /// Remaining fraction of a step from 0 to 90 degrees of height after
    /// `steps` rendered steps.
    fn remaining_after(aim: &mut AngleSmoother, steps: usize) -> f32 {
        aim.reset_to_target(Azimuth::FRONT, Elevation(0.0));
        for _ in 0..steps {
            aim.step(Azimuth::FRONT, Elevation(90.0));
        }
        let (_, el) = aim.current();
        (90.0 - el.get()) / 90.0
    }

    /// The de-zipper's time constant is 50 ms of audio: after 50 ms of
    /// steps a move has `e^-1` of its distance left. At 51.2 kHz, 50 ms is
    /// 2560 samples, exactly twenty 128-sample steps; at 102.4 kHz (after a
    /// retune) it is forty.
    ///
    /// Mutations (run):
    /// - build with a stride of one (`AngleSmoother::new(sample_rate,
    ///   Samples(1))`) → after twenty steps almost all of the move is left
    ///   and the assertion fails.
    /// - drop `self.stride` from `ExponentialSmoother::set_sample_rate`
    ///   (retune with a stride of one) → the 102.4 kHz half fails.
    #[test]
    fn the_glide_has_a_50_ms_time_constant_in_audio_time() {
        assert_eq!(STEP, 128, "the rates below assume 128-sample steps");
        let e_inv = (-1.0f32).exp();

        let mut aim = AngleSmoother::new(SampleRate(51_200.0), Samples(STEP));
        let left = remaining_after(&mut aim, 20);
        assert!(
            (left - e_inv).abs() < 1e-4,
            "after 50 ms at 51.2 kHz {left} of the move is left, expected e^-1 = {e_inv}"
        );

        aim.set_sample_rate(SampleRate(102_400.0));
        let left = remaining_after(&mut aim, 40);
        assert!(
            (left - e_inv).abs() < 1e-4,
            "after 50 ms at 102.4 kHz {left} of the move is left, expected e^-1 = {e_inv}"
        );
    }
}
