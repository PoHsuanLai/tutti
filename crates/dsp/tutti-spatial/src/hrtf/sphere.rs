//! The measured HRIR sphere: parsed once from its bytes ([`HrirSphere`]),
//! then prepared per sample rate into the spectra and face table the
//! convolver reads ([`HrtfSet`]).
//!
//! The byte format is the one `hrir_sphere_builder` writes (and the `hrtf`
//! crate read): `"HRIR"`, then little-endian `u32` source rate, response
//! length, vertex count and index count, the face indices, and per vertex its
//! position followed by its left and right responses as `f32`s.

use std::sync::Arc;

use rustfft::num_complex::Complex32;
use rustfft::{Fft, FftPlanner};

use super::panner::{HrtfBinauralError, MAX_SPHERE_RATE, MIN_SPHERE_RATE};

/// The range a vertex's distance from the origin must fall in. Closer, its
/// direction does not survive normalization in `f32`.
const MIN_DISTANCE: f64 = 1e-6;
const MAX_DISTANCE: f64 = 1e6;

/// The largest response sample magnitude accepted. A spectrum sums up to
/// `fft_len` of them, so a sample near `f32::MAX` would overflow it.
const MAX_SAMPLE: f32 = 1e6;

/// The parsed dataset, at the rate it was measured at. Immutable once
/// parsed: every renderer, clone and fork built from the same bytes shares
/// one behind an `Arc`.
pub(crate) struct HrirSphere {
    /// The file's own rate, an integral Hz as the format stores it.
    rate: u32,
    /// Response length in frames at `rate`, the same for every vertex.
    len: usize,
    positions: Vec<[f32; 3]>,
    faces: Vec<[u32; 3]>,
    /// Per vertex `v`, its left response at `2v * len` and its right at
    /// `(2v + 1) * len`.
    responses: Vec<f32>,
}

/// A little-endian cursor that reports a short read as an error, never a
/// panic.
struct Cursor<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Cursor<'_> {
    fn word(&mut self) -> Result<[u8; 4], HrtfBinauralError> {
        let word = self
            .bytes
            .get(self.at..self.at + 4)
            .and_then(|w| <[u8; 4]>::try_from(w).ok())
            .ok_or(HrtfBinauralError::Truncated {
                needed: self.at + 4,
                found: self.bytes.len(),
            })?;
        self.at += 4;
        Ok(word)
    }

    fn u32(&mut self) -> Result<u32, HrtfBinauralError> {
        self.word().map(u32::from_le_bytes)
    }

    fn f32(&mut self) -> Result<f32, HrtfBinauralError> {
        self.word().map(f32::from_le_bytes)
    }
}

impl HrirSphere {
    /// Parses and validates a sphere. Every index, length and value is
    /// checked here, so nothing downstream (and nothing on the audio
    /// thread) has a malformed sphere to trip over.
    pub(crate) fn parse(bytes: &[u8]) -> Result<Self, HrtfBinauralError> {
        if bytes.get(..4) != Some(b"HRIR".as_slice()) {
            return Err(HrtfBinauralError::NotASphere);
        }
        let mut cur = Cursor { bytes, at: 4 };
        let rate = cur.u32()?;
        let len = cur.u32()? as usize;
        let vertices = cur.u32()? as usize;
        let indices = cur.u32()? as usize;
        if !(MIN_SPHERE_RATE..=MAX_SPHERE_RATE).contains(&rate) {
            return Err(HrtfBinauralError::Rate { rate });
        }
        if len == 0 {
            return Err(HrtfBinauralError::Header("the response length is zero"));
        }
        let max = max_response(rate);
        if len > max {
            return Err(HrtfBinauralError::ResponseTooLong { len, max });
        }
        if vertices == 0 {
            return Err(HrtfBinauralError::Header("the sphere has no vertices"));
        }
        if !indices.is_multiple_of(3) {
            return Err(HrtfBinauralError::Header(
                "the index count is not a whole number of triangles",
            ));
        }
        // Size the whole file before allocating anything, so a header that
        // claims billions of vertices is refused rather than attempted.
        let needed = vertices
            .checked_mul(len)
            .and_then(|n| n.checked_mul(2))
            .and_then(|n| n.checked_add(vertices.checked_mul(3)?))
            .and_then(|n| n.checked_add(indices))
            .and_then(|n| n.checked_mul(4))
            .and_then(|n| n.checked_add(20))
            .unwrap_or(usize::MAX);
        if needed > bytes.len() {
            return Err(HrtfBinauralError::Truncated {
                needed,
                found: bytes.len(),
            });
        }
        if needed < bytes.len() {
            return Err(HrtfBinauralError::Trailing {
                size: needed,
                found: bytes.len(),
            });
        }

        let mut faces = Vec::with_capacity(indices / 3);
        for face in 0..indices / 3 {
            let mut tri = [0u32; 3];
            for vertex in &mut tri {
                *vertex = cur.u32()?;
                if *vertex as usize >= vertices {
                    return Err(HrtfBinauralError::FaceIndex {
                        face,
                        vertex: *vertex,
                        vertices,
                    });
                }
            }
            faces.push(tri);
        }

        let mut positions = Vec::with_capacity(vertices);
        let mut responses = Vec::with_capacity(vertices * len * 2);
        for vertex in 0..vertices {
            let pos = [cur.f32()?, cur.f32()?, cur.f32()?];
            let start = responses.len();
            for _ in 0..2 * len {
                responses.push(cur.f32()?);
            }
            // NaN fails both comparisons, so a non-finite value is refused
            // too.
            let distance = pos
                .iter()
                .map(|&c| f64::from(c).powi(2))
                .sum::<f64>()
                .sqrt();
            let placed = (MIN_DISTANCE..=MAX_DISTANCE).contains(&distance);
            let bounded = responses[start..].iter().all(|s| s.abs() <= MAX_SAMPLE);
            if !placed || !bounded {
                return Err(HrtfBinauralError::Vertex { vertex });
            }
            positions.push(pos);
        }
        if !faces.iter().any(|&f| Face::new(f, &positions).is_some()) {
            return Err(HrtfBinauralError::NoFace);
        }

        Ok(Self {
            rate,
            len,
            positions,
            faces,
            responses,
        })
    }
}

/// The longest response a sphere at `rate` may carry: 50 ms of frames,
/// rounded down. Measured HRIRs last a few milliseconds (IRCAM's 512 taps at
/// 44.1 kHz are 11.6 ms); 50 ms leaves room for a set with its onset delay
/// kept and its tail undecayed. Resampling keeps a response's duration, so
/// a set prepared at any rate holds at most 50 ms of frames there, rounded
/// up: the bound on what `HrtfSet::build` allocates and computes per vertex.
pub(crate) fn max_response(rate: u32) -> usize {
    rate as usize / 20
}

/// The most response samples [`resample_into`] copies tap-major at once:
/// 4 MB of `f32`, so its scratch stays bounded whatever the file's size.
const RESAMPLE_BATCH: usize = 1 << 20;

/// Resamples every response in `responses` (back to back, `len` frames
/// each, a vertex's left then its right) from `from` to `to` Hz by
/// band-limited interpolation, keeping its frequency response, writing
/// `out_len` frames of each straight into `spectra` (`fft_len` values per
/// vertex): an even response into the real parts of its vertex's run, an
/// odd one into the imaginary parts. Nothing else is written.
///
/// Each output tap is the response evaluated between its source taps
/// through a Blackman-windowed sinc, low-passed below the lower of the two
/// Nyquist rates (5 % short of it, so the window's transition band stays
/// under it). The kernel is centred, so nothing is delayed. The taps are then
/// scaled by `from / to`: a response's taps are its continuous form times the
/// sample period, so at twice the rate each tap carries half the weight.
///
/// The responses go in batches of up to [`RESAMPLE_BATCH`] samples, copied
/// tap-major so a weight is applied to the whole batch in one contiguous
/// pass. The kernel's weights depend only on the output tap, so each tap's
/// are evaluated once per batch: the `sin` and `cos` calls cost `out_len`
/// kernel widths per batch, and each response costs only its multiply-adds.
/// A response's sum runs in tap order whatever the batch, so batching does
/// not change a bit.
///
/// The kernel is truncated at the response's ends (taps outside it count as
/// zero), so the first and last few output taps of a response that has not
/// decayed there carry the cut's ripple.
fn resample_into(
    responses: &[f32],
    len: usize,
    from: u32,
    to: u32,
    out_len: usize,
    spectra: &mut [Complex32],
    fft_len: usize,
) {
    /// Kernel half-width, in zero crossings.
    const HALF_WIDTH: f64 = 32.0;
    let step = f64::from(from) / f64::from(to);
    let cutoff = 0.95 * step.recip().min(1.0);
    let half = HALF_WIDTH / cutoff;
    let kernel = |t: f64| {
        let x = std::f64::consts::PI * cutoff * t;
        let sinc = if x == 0.0 { 1.0 } else { x.sin() / x };
        let u = std::f64::consts::PI * t / half;
        let window = 0.42 + 0.5 * u.cos() + 0.08 * (2.0 * u).cos();
        cutoff * sinc * window
    };
    let per_batch = (RESAMPLE_BATCH / len).max(1);
    let mut taps = Vec::new();
    let mut weights = Vec::new();
    let mut sums = Vec::new();
    for (batch, chunk) in responses.chunks(per_batch * len).enumerate() {
        let count = chunk.len() / len;
        let first = batch * per_batch;
        // Tap-major: tap `m` of every response in the batch at
        // `m * count..(m + 1) * count`.
        taps.clear();
        taps.resize(chunk.len(), 0.0f32);
        for (r, response) in chunk.chunks_exact(len).enumerate() {
            for (m, &s) in response.iter().enumerate() {
                taps[m * count + r] = s;
            }
        }
        sums.resize(count, 0.0f64);
        for n in 0..out_len {
            let t = n as f64 * step;
            let end = ((t + half).floor() as usize).saturating_add(1).min(len);
            let lo = ((t - half).ceil().max(0.0) as usize).min(end);
            weights.clear();
            weights.extend((lo..end).map(|m| kernel(t - m as f64)));
            sums.fill(0.0);
            for (row, &w) in taps[lo * count..end * count]
                .chunks_exact(count)
                .zip(&weights)
            {
                for (sum, &s) in sums.iter_mut().zip(row) {
                    *sum += f64::from(s) * w;
                }
            }
            for (r, &sum) in sums.iter().enumerate() {
                let r = first + r;
                let value = &mut spectra[(r / 2) * fft_len + n];
                let tap = (sum * step) as f32;
                if r % 2 == 0 {
                    value.re = tap;
                } else {
                    value.im = tap;
                }
            }
        }
    }
}

/// One face prepared for lookup: its vertices and the inverse of the matrix
/// whose columns are their positions.
struct Face {
    verts: [u32; 3],
    inverse: [[f32; 3]; 3],
}

impl Face {
    /// `None` for a face whose plane passes through the origin: no ray from
    /// the centre crosses it at a single point.
    fn new(verts: [u32; 3], positions: &[[f32; 3]]) -> Option<Self> {
        let [a, b, c] = verts.map(|v| positions[v as usize].map(f64::from));
        // Columns a, b, c; the inverse is the adjugate over the determinant.
        let cross = |u: [f64; 3], v: [f64; 3]| {
            [
                u[1] * v[2] - u[2] * v[1],
                u[2] * v[0] - u[0] * v[2],
                u[0] * v[1] - u[1] * v[0],
            ]
        };
        let (bc, ca, ab) = (cross(b, c), cross(c, a), cross(a, b));
        let det = a[0] * bc[0] + a[1] * bc[1] + a[2] * bc[2];
        let scale = [a, b, c]
            .iter()
            .map(|p| p.iter().map(|x| x * x).sum::<f64>().sqrt())
            .product::<f64>();
        // Also refuses a NaN determinant.
        if det.abs().partial_cmp(&(1e-9 * scale)) != Some(std::cmp::Ordering::Greater) {
            return None;
        }
        let row = |r: [f64; 3]| r.map(|x| (x / det) as f32);
        Some(Self {
            verts,
            inverse: [row(bc), row(ca), row(ab)],
        })
    }

    /// The barycentric weights of the point where the ray along `dir` meets
    /// this face, or `None` if it misses.
    #[inline]
    fn hit(&self, dir: [f32; 3]) -> Option<[f32; 3]> {
        // `dir = M · coeffs`: the coefficients of `dir` in the face's
        // vertices. The ray hits the face iff all three are non-negative,
        // and the hit point's barycentric weights are them normalized.
        let c = self
            .inverse
            .map(|r| r[0] * dir[0] + r[1] * dir[1] + r[2] * dir[2]);
        let sum = c[0] + c[1] + c[2];
        // Relative slack, so a ray along a shared edge hits both faces.
        let slack = -1e-5 * sum;
        if sum > 0.0 && c.iter().all(|&x| x >= slack) {
            let c = c.map(|x| x.max(0.0));
            let sum = c[0] + c[1] + c[2];
            Some(c.map(|x| x / sum))
        } else {
            None
        }
    }
}

/// Which vertices a direction blends, and by how much.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Weights {
    pub(crate) verts: [u32; 3],
    pub(crate) w: [f32; 3],
}

/// The sphere prepared at one sample rate for the convolver: each vertex's
/// two responses as one packed spectrum, the face table, and the FFT plans.
/// Built on the control thread; shared, read-only, by every renderer at that
/// rate.
pub(crate) struct HrtfSet {
    /// The rate the responses were resampled to.
    pub(crate) rate: u32,
    /// Response length in frames at `rate`.
    pub(crate) ir_len: usize,
    /// The FFT length: the power of two at or above `step + ir_len - 1`.
    pub(crate) fft_len: usize,
    pub(crate) forward: Arc<dyn Fft<f32>>,
    pub(crate) inverse: Arc<dyn Fft<f32>>,
    /// Scratch the two plans need, in complex values.
    pub(crate) scratch_len: usize,
    /// Per vertex, `(FFT(left) + i·FFT(right)) / fft_len`. Both responses
    /// are real, so one inverse FFT of `X · packed` returns the left output
    /// in its real part and the right in its imaginary part.
    spectra: Vec<Complex32>,
    /// Vertex directions, normalized, for the nearest-vertex fallback.
    directions: Vec<[f32; 3]>,
    faces: Vec<Face>,
}

impl HrtfSet {
    /// Prepares `sphere` at `rate` for steps of `step` frames. Allocates,
    /// resamples and plans: control thread only.
    ///
    /// The peak memory is the spectra this returns, `vertices × fft_len`
    /// complex values, plus the resampler's scratch (at most
    /// [`RESAMPLE_BATCH`] samples): the resampled responses are written
    /// straight into the spectra and transformed in place. Time and memory
    /// scale with the file's size times `rate / sphere.rate`; the vertex
    /// count is bounded only by the file's size.
    pub(crate) fn build(sphere: &HrirSphere, rate: u32, step: usize) -> Self {
        let ir_len = if rate == sphere.rate {
            sphere.len
        } else {
            (sphere.len as u64 * u64::from(rate)).div_ceil(u64::from(sphere.rate)) as usize
        };
        let fft_len = (step + ir_len - 1).next_power_of_two();
        let mut planner = FftPlanner::<f32>::new();
        let forward = planner.plan_fft_forward(fft_len);
        let inverse = planner.plan_fft_inverse(fft_len);
        let scratch_len = forward
            .get_inplace_scratch_len()
            .max(inverse.get_inplace_scratch_len());

        let vertices = sphere.positions.len();
        // The time-domain pairs go straight into the spectra, zero-padded,
        // then each is transformed in place: no other copy of the responses
        // at `rate` exists.
        let mut spectra = vec![Complex32::new(0.0, 0.0); vertices * fft_len];
        if rate == sphere.rate {
            for (spectrum, pair) in spectra
                .chunks_exact_mut(fft_len)
                .zip(sphere.responses.chunks_exact(2 * ir_len))
            {
                let (left, right) = pair.split_at(ir_len);
                for ((s, l), r) in spectrum.iter_mut().zip(left).zip(right) {
                    *s = Complex32::new(*l, *r);
                }
            }
        } else {
            resample_into(
                &sphere.responses,
                sphere.len,
                sphere.rate,
                rate,
                ir_len,
                &mut spectra,
                fft_len,
            );
        }
        let mut scratch = vec![Complex32::new(0.0, 0.0); scratch_len];
        let norm = 1.0 / fft_len as f32;
        for spectrum in spectra.chunks_exact_mut(fft_len) {
            forward.process_with_scratch(spectrum, &mut scratch);
            spectrum.iter_mut().for_each(|s| *s *= norm);
        }

        let directions = sphere
            .positions
            .iter()
            .map(|p| {
                let n = (p[0] * p[0] + p[1] * p[1] + p[2] * p[2]).sqrt();
                p.map(|c| c / n)
            })
            .collect();
        let faces = sphere
            .faces
            .iter()
            .filter_map(|&f| Face::new(f, &sphere.positions))
            .collect();

        Self {
            rate,
            ir_len,
            fft_len,
            forward,
            inverse,
            scratch_len,
            spectra,
            directions,
            faces,
        }
    }

    /// Vertex `v`'s packed spectrum.
    #[inline]
    pub(crate) fn spectrum(&self, v: u32) -> &[Complex32] {
        let at = v as usize * self.fft_len;
        &self.spectra[at..at + self.fft_len]
    }

    /// The face the ray along `dir` crosses and the blend of its vertices
    /// there. `hint` is the face the last lookup found, tried first (a source
    /// moves smoothly, so usually it hits again) and updated.
    ///
    /// Never fails: a direction no face covers (a sphere that does not
    /// enclose the listener, or a non-finite direction) takes the nearest
    /// vertex, or vertex 0 if no vertex compares. Allocation-free and
    /// lock-free; the scan is linear in the face count, which only a miss of
    /// the hint pays.
    pub(crate) fn weights(&self, dir: [f32; 3], hint: &mut usize) -> Weights {
        if let Some(face) = self.faces.get(*hint) {
            if let Some(w) = face.hit(dir) {
                return Weights {
                    verts: face.verts,
                    w,
                };
            }
        }
        for (i, face) in self.faces.iter().enumerate() {
            if let Some(w) = face.hit(dir) {
                *hint = i;
                return Weights {
                    verts: face.verts,
                    w,
                };
            }
        }
        let mut nearest = 0u32;
        let mut best = f32::NEG_INFINITY;
        for (v, d) in self.directions.iter().enumerate() {
            let dot = d[0] * dir[0] + d[1] * dir[1] + d[2] * dir[2];
            if dot > best {
                best = dot;
                nearest = v as u32;
            }
        }
        Weights {
            verts: [nearest; 3],
            w: [1.0, 0.0, 0.0],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The resampler reads a response's first and last samples. One vertex
    /// whose left response is a unit delta on its first sample and right a
    /// unit delta on its last, neither decayed, is doubled from 8 to 16 kHz.
    /// Each response is resampled alone, so an output tap at `t` is
    /// `step · kernel(t − m)` for its delta at `m`, and the tap that lands on
    /// the delta (`n = 2m`) takes the kernel's centre: sinc and window are 1
    /// there, so it is `step · cutoff = 0.5 · 0.95 = 0.475`, the delta's DC
    /// sum of 1 at the kernel's peak. No libm call is involved but `cos(0)`,
    /// which is exact; the tolerance covers only the `f32` rounding.
    ///
    /// Mutations (run):
    /// - `.min(len)` → `.min(len - 1)` on the window's `end` (drops every
    ///   response's last sample) → the right ear's tap is 0, not 0.475.
    /// - `.max(0.0)` → `.max(1.0)` on its `lo` (drops every response's first
    ///   sample) → the left ear's tap is 0.
    /// - writing an odd response into the real part → the left ear's tap
    ///   is overwritten by the right response's kernel tail.
    #[test]
    fn resampling_reads_a_responses_edge_taps() {
        const LEN: usize = 8;
        let mut responses = vec![0.0f32; 2 * LEN];
        responses[0] = 1.0;
        responses[2 * LEN - 1] = 1.0;
        let out_len = 2 * LEN;
        let fft_len = out_len;
        let mut spectra = vec![Complex32::new(0.0, 0.0); fft_len];
        resample_into(
            &responses,
            LEN,
            8_000,
            16_000,
            out_len,
            &mut spectra,
            fft_len,
        );

        let first = spectra[0].re;
        let last = spectra[2 * (LEN - 1)].im;
        for (ear, tap) in [("left, first sample", first), ("right, last sample", last)] {
            assert!(
                (tap - 0.475).abs() <= 1e-6,
                "{ear}: the tap on the delta is {tap}, expected 0.475"
            );
        }
    }
}
