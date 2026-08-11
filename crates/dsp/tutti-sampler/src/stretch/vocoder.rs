//! The phase vocoder itself: analysis, phase unwrapping, synthesis.
//!
//! One [`Vocoder`] per channel. It owns no source and no policy — the caller
//! picks the hop geometry, which is what makes stretch and pitch separable here
//! (see [`super::unit`]).

use std::sync::Arc;

use super::buffers::{OverlapAdd, SampleFifo};
// `Bank` is referenced only by the intra-doc link on `Unit::clone` below;
// rustdoc needs the name in scope to resolve it. `FftSize` and
// `MAX_BUFFER_SIZE` were dead and are gone.
#[allow(unused_imports)]
use super::Bank;
use tutti_analysis::StftGeometry;
use tutti_core::{inverse_fft, real_fft, Complex32, Radians};

/// One channel of phase-vocoder state.
///
/// Sized once, at construction; `process` allocates nothing.
pub(super) struct Vocoder {
    pub(super) geometry: StftGeometry,
    /// The analysis/synthesis window, taken from the grid.
    ///
    /// Whatever shape `geometry.window_fn()` names — it is no longer assumed to
    /// be Hann, because the overlap-add normalization no longer assumes it
    /// either (see [`OverlapAdd`]).
    ///
    /// `Arc` because it is immutable for the vocoder's lifetime and identical for
    /// every channel and every clone — and because building it costs `size`
    /// `cos()` calls, which `Net::commit`'s deep clone was paying per channel per
    /// node. Sharing turns that into a refcount bump.
    pub(super) window: Arc<Vec<f32>>,

    /// Real scratch handed to [`real_fft`], which transforms it in place.
    pub(super) fft_buffer: Vec<f32>,
    /// Full spectrum: `DC..=Nyquist` written by analysis, the conjugate half
    /// rebuilt before the inverse transform.
    pub(super) spectrum: Vec<Complex32>,
    /// Per-bin synthesis phase, accumulated across frames.
    pub(super) phase_accumulator: Vec<Radians>,
    /// Per-bin analysis phase from the previous frame.
    pub(super) last_phase: Vec<Radians>,
    /// Per-bin phase advance produced by **one sample** of analysis hop.
    /// Multiplied by the frame's analysis hop, which varies with stretch.
    ///
    /// `Arc` for the same reason as `window`: a function of the geometry alone,
    /// immutable for the vocoder's lifetime, and identical across every clone.
    pub(super) phase_per_sample: Arc<Vec<Radians>>,

    /// Whether a frame has been analysed yet on this stream.
    ///
    /// The first frame has no previous phase to unwrap against, so it must
    /// **seed** the accumulator with the phase it observes rather than
    /// accumulate onto zero. Skipping that leaves every bin carrying a
    /// permanent, per-bin-varying offset, which is a phase error the stream
    /// never recovers from: the partials of one tone stop lining up and the
    /// overlap-added frames cancel instead of summing. Measured at -11.9 dB at
    /// 2x stretch and -17.5 dB at 4x, with unity clean because there the offset
    /// is identically zero.
    ///
    /// Cleared by [`reset`](Self::reset) — a flushed stream is a new stream and
    /// must seed again.
    pub(super) primed: bool,

    pub(super) input: SampleFifo,
    pub(super) output: OverlapAdd,
}

impl Vocoder {
    /// Allocate every buffer this vocoder will ever need, on `geometry`'s grid.
    ///
    /// The only allocating entry point besides [`clone_fresh`](Self::clone_fresh)
    /// — `process` and `process_frame` run on the audio thread and touch nothing
    /// but what is sized here.
    pub(super) fn new(geometry: StftGeometry) -> Self {
        let size = geometry.window().get();
        let bins = geometry.bins_per_frame().get();
        // 2π·k/size — the phase bin `k` advances **per sample** of analysis hop.
        // No sample-rate term: it is a ratio of sample counts, which is why
        // changing the rate does not invalidate it.
        //
        // Stored per-sample rather than per-hop because the analysis hop varies
        // with the stretch factor (see `process_frame`). Multiplying by the
        // frame's actual hop is one multiply on a table read that already
        // happens.
        let phase_per_sample = (0..bins)
            .map(|k| Radians(Radians::TAU.get() * k as f32 / size as f32))
            .collect();

        Self {
            geometry,
            window: Arc::new(geometry.window_coefficients()),
            fft_buffer: vec![0.0; size],
            spectrum: vec![Complex32::new(0.0, 0.0); size],
            phase_accumulator: vec![Radians(0.0); bins],
            last_phase: vec![Radians(0.0); bins],
            phase_per_sample: Arc::new(phase_per_sample),
            // 4x the window: three frames of overlap-add tail plus the frame
            // being written.
            primed: false,
            input: SampleFifo::new(size * 4),
            output: OverlapAdd::new(size * 4),
        }
    }

    /// A fresh vocoder on the same grid, sharing everything immutable.
    ///
    /// A clone starts with clean phase history (see [`Unit::clone`]), so no
    /// state is copied — only the *shapes* carry. The Hann window and the
    /// per-bin phase table are both functions of the geometry alone, so they are
    /// shared by `Arc` rather than rebuilt; rebuilding costs `size` `cos()`
    /// calls per vocoder.
    ///
    /// # Not on the commit path
    ///
    /// `Unit::clone` shares the whole vocoder bank by refcount (see [`Bank`]),
    /// so a graph commit does not reach here. This runs only from
    /// [`AudioUnit::isolate`], where an offline render needs private state, and
    /// it allocates ~100 KB per vocoder — 64% of it the two `size * 4` rings.
    /// Control thread only.
    ///
    /// # Why the deep clone was worth removing
    ///
    /// Profiled under `samply` (`examples/profile_stretch_clone.rs`), this
    /// function's cost splits **~42% allocator, ~37% `memset`** — allocating the
    /// buffers and zeroing them in nearly equal measure. Kernel time is 1.3%, so
    /// it is real work rather than a paging artifact.
    ///
    /// **That 37% is why a buffer pool was built here and then removed.** A pool
    /// recycles the allocation but a recycled buffer still has to be cleared,
    /// and the clear is the same `memset` as a fresh `vec![0.0; n]` — so pooling
    /// can only address the allocator's 42%, and only when the pool is
    /// non-empty. On this path it never is: `commit_inner` clones *before* it
    /// retires the previous generation, so nothing has been returned at the
    /// moment the clone asks. Measured, a fresh build and a pooled hit came out
    /// identical within noise.
    ///
    /// What worked instead was not cloning what carries nothing. Against a 2 ms
    /// commit budget, a deep-cloning commit moved 201.8 MB at stereo and 604.6
    /// MB at six channels. Sharing the bank took that to 81.5 / 243.8 MB;
    /// moving the block scratch onto the bank as well — it is overwritten every
    /// block before it is read, so it need never be copied — took it to **1.3 /
    /// 3.3 MB**, a ~180x reduction with both widths committing in ~0.2 ms. What
    /// remains is fundsp's own per-`Vertex` bookkeeping, not this state.
    pub(super) fn clone_fresh(&self) -> Self {
        let size = self.geometry.window().get();
        let bins = self.geometry.bins_per_frame().get();
        Self {
            geometry: self.geometry,
            window: Arc::clone(&self.window),
            fft_buffer: vec![0.0; size],
            spectrum: vec![Complex32::new(0.0, 0.0); size],
            phase_accumulator: vec![Radians(0.0); bins],
            last_phase: vec![Radians(0.0); bins],
            phase_per_sample: Arc::clone(&self.phase_per_sample),
            primed: false,
            input: SampleFifo::new(size * 4),
            output: OverlapAdd::new(size * 4),
        }
    }

    /// Clear every buffer and the phase history, keeping the grid and the shared
    /// tables. Allocation-free, so it is safe from the audio thread.
    pub(super) fn reset(&mut self) {
        self.fft_buffer.fill(0.0);
        self.spectrum.fill(Complex32::new(0.0, 0.0));
        self.phase_accumulator.fill(Radians(0.0));
        self.last_phase.fill(Radians(0.0));
        self.input.reset();
        self.output.reset();
        // A flushed stream is a new stream: it must seed its phase again, or it
        // carries the pre-flush offset into the new material.
        self.primed = false;
    }

    /// Drain every whole frame the input holds.
    ///
    /// `analysis_hop` is how far through the SOURCE each frame steps;
    /// `synthesis_hop` is how much finished output each frame publishes. Their
    /// ratio is the time-scaling, and which one varies depends on who drives the
    /// rate — see [`Unit::tick`].
    pub(super) fn process(&mut self, analysis_hop: usize, synthesis_hop: usize) {
        while self.input.available() >= self.geometry.window().get() {
            self.process_frame(analysis_hop, synthesis_hop);
        }
    }

    /// # Why there is no pitch parameter here
    ///
    /// A phase vocoder cannot transpose. A partial's output frequency is set by
    /// **which bin holds its magnitude**, and this loop never moves magnitude
    /// between bins — line for line, bin `k` in is bin `k` out. Scaling the
    /// phase advance by a pitch ratio therefore transposes nothing; it only
    /// decorrelates each bin's phase from its magnitude.
    ///
    /// That is measurably *worse than omitting it*, which is why the temptation
    /// is worth naming. Feeding 440 Hz and asking for ±1200 cents, the scaling
    /// produces 411 Hz and 408 Hz — the same wrong answer in both directions, so
    /// not even a wrong-ratio bug — at 6 dB down. Held alongside a correct
    /// read-rate resample it still costs 8.7 dB at +1200 and 12.5 dB at +700,
    /// pulling exact pitch off by up to 47 Hz.
    ///
    /// Transposition is a *resampling* operation and lives at the call site: the
    /// caller reads the source at [`Unit::input_rate`], which folds in the pitch
    /// ratio, and [`Unit::effective_stretch`] restores the duration that fast or
    /// slow read cost. This function's only job is time-scaling.
    pub(super) fn process_frame(&mut self, analysis_hop: usize, synthesis_hop: usize) {
        let size = self.geometry.window().get();
        let bins = self.geometry.bins_per_frame().get();

        // 1. Window the frame into the FFT scratch.
        for i in 0..size {
            self.fft_buffer[i] = self.input.peek(i) * self.window[i];
        }
        self.input.consume(analysis_hop);

        // 2. Forward transform. `real_fft` returns size/2 bins and packs
        //    Nyquist into DC's imaginary part, so unpack both before treating
        //    any bin as a magnitude/phase pair — reading bin 0 as-is mixes two
        //    unrelated frequencies into one bogus polar value.
        let packed = real_fft(&mut self.fft_buffer);
        self.spectrum[..packed.len()].copy_from_slice(packed);
        let dc = self.spectrum[0].re;
        let nyquist = self.spectrum[0].im;
        self.spectrum[0] = Complex32::new(dc, 0.0);
        self.spectrum[bins - 1] = Complex32::new(nyquist, 0.0);

        // 3. Per-bin phase advance, re-accumulated at the synthesis rate.
        let hop_ratio = synthesis_hop as f32 / analysis_hop as f32;
        for k in 0..bins {
            let magnitude = self.spectrum[k].norm();
            let phase = Radians(self.spectrum[k].arg());

            // Deviation of the observed advance from the expected one, wrapped
            // into (-π, π] — the unwrapping step that recovers the bin's true
            // instantaneous frequency rather than its aliased one.
            let expected = Radians(self.phase_per_sample[k].get() * analysis_hop as f32);

            if self.primed {
                let deviation = wrap_phase(phase - self.last_phase[k] - expected);
                let true_advance = expected + deviation;
                self.phase_accumulator[k] =
                    wrap_phase(self.phase_accumulator[k] + true_advance * hop_ratio);
            } else {
                // First frame of the stream: adopt the observed phase. There is
                // no previous frame to measure an advance against, and starting
                // the accumulator anywhere else bakes in an offset that never
                // decays — see `primed`.
                self.phase_accumulator[k] = phase;
            }
            self.last_phase[k] = phase;

            self.spectrum[k] = Complex32::from_polar(magnitude, self.phase_accumulator[k].get());
        }

        self.primed = true;

        // 4. Rebuild the conjugate half so the inverse transform is real.
        for i in 1..bins - 1 {
            self.spectrum[size - i] = self.spectrum[i].conj();
        }

        // 5. Inverse transform, then window and overlap-add.
        //
        // No `1 / size` here: microfft's inverse already normalizes, so a
        // forward-then-inverse pair is the identity (pinned by
        // `fft_roundtrip_is_the_identity`). Dividing again attenuates the
        // stretched signal by the FFT size — 60 dB at 1024, 66 at 2048 — and
        // presents as "stretching mutes the voice" rather than as a gain bug.
        // That is the shape a non-zero-output assertion cannot catch: 0.0004 is
        // non-zero.
        inverse_fft(&mut self.spectrum);
        for i in 0..size {
            let w = self.window[i];
            // The windowed sample and the window energy that carried it, summed
            // into the same slot. `drain` divides one by the other — which is
            // what `tutti_analysis::istft` has always done, and what the old
            // `COLA_GAIN` scalar only approximated for one window at one hop.
            self.output.add_at(i, self.spectrum[i].re * w, w * w);
        }

        // Zero the span the next frame will accumulate into, which this one has
        // already scrolled past.
        for i in 0..synthesis_hop {
            self.output.clear_at(size + i);
        }
        self.output.advance(synthesis_hop);
    }
}

/// Wrap a phase into `[-π, π)`.
///
/// A thin alias over [`Radians::wrapped_signed`], which owns the arithmetic.
/// Hand-rolling the wrap in raw `f32` here instead is exactly the escape the
/// unit-type omission ledger exists to catch: an operator [`Radians`] declines
/// to offer means "call the named method", not "drop to the primitive".
///
/// The half-open end is `-π`, not `+π`. They are the same point on the circle,
/// so nothing downstream distinguishes them — the interval is stated precisely
/// only so a reader comparing against a textbook's `(-π, π]` does not go looking
/// for a bug.
#[inline]
pub(super) fn wrap_phase(phase: Radians) -> Radians {
    phase.wrapped_signed()
}

// `COLA_GAIN` used to live here: `1.0 / 1.5`, where `1.5 = 4 × mean(hann²)`.
//
// It was a precomputed scalar, and its own doc said what was wrong with it —
// "only correct at 75% overlap". It was also only correct for Hann, silently:
// point a Hamming window at it and every sample is 0.8 dB hot, a Blackman one
// and it is 0.9 dB shy, with nothing erroring anywhere.
//
// The sum is now accumulated per sample in `OverlapAdd` and divided out at the
// read, which is what `tutti_analysis::istft` has always done. See that type's
// doc for why the two rings are one struct.
