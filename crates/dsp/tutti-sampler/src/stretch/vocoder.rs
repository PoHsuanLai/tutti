//! The phase vocoder itself: analysis, phase unwrapping, synthesis.
//!
//! One [`Vocoder`] per channel. It owns no source and no policy — the caller
//! picks the hop geometry, which is what makes stretch and pitch separable here
//! (see [`super::unit`]).

use std::sync::Arc;

use super::buffers::{OverlapAdd, SampleFifo};
use super::{Bank, FftSize, MAX_BUFFER_SIZE};
use tutti_analysis::{window::hann, StftGeometry};
use tutti_core::{inverse_fft, real_fft, Complex32, Radians};

/// One channel of phase-vocoder state.
///
/// Sized once, at construction; `process` allocates nothing.
pub(super) struct Vocoder {
    pub(super) geometry: StftGeometry,
    /// The Hann analysis/synthesis window.
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
    pub(super) fn new(geometry: StftGeometry) -> Self {
        let size = geometry.window().get();
        let bins = geometry.bins_per_frame().get();
        // 2π·k/size — the phase bin `k` advances **per sample** of analysis hop.
        // No sample-rate term: it is a ratio of sample counts, which is why
        // changing the rate does not invalidate it.
        //
        // Stored per-sample rather than per-hop because the analysis hop is no
        // longer fixed — see `process_frame`. Multiplying by the frame's actual
        // hop is one multiply on a table read that already happens.
        let phase_per_sample = (0..bins)
            .map(|k| Radians(Radians::TAU.get() * k as f32 / size as f32))
            .collect();

        Self {
            geometry,
            window: Arc::new(hann(size)),
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
    /// A clone starts with clean phase history (see [`Unit::clone`]), so no state
    /// is copied — only the *shapes* carry. The Hann window and the per-bin phase
    /// table are both functions of the geometry alone, so they are shared rather
    /// than rebuilt; rebuilding cost `size` `cos()` calls per vocoder per commit.
    ///
    /// # What this costs on a graph commit
    ///
    /// This runs from `Net::commit`, once per channel per node, and the clone is
    /// **kept** — `commit_inner` clones the net, `core::mem::swap`s the vertex
    /// vectors so the ORIGINALS ship to the backend ("necessary if the nodes
    /// contain any backends, which cannot be cloned effectively"), and the
    /// freshly-built clones stay on the frontend as the next generation's source.
    /// So the allocation is not waste; it is the price of double-buffering, paid
    /// once per commit per vocoder: ~100 KB of mutable state, 64% of it the two
    /// `size * 4` rings.
    ///
    /// Profiled (`examples/profile_stretch_clone.rs`, run under `samply`), the
    /// cost splits **~42% allocator, ~37% `memset`** — allocating the buffers and
    /// zeroing them, in nearly equal measure. Kernel time is 1.3%, so this is
    /// real work rather than the paging artifact an earlier wall-clock benchmark
    /// suggested. That benchmark's figures (18.5 ms / 628 ms, quoted in earlier
    /// revisions of this comment) also measured two live generations at once,
    /// which is 5-14x more expensive than the one-at-a-time shape `commit_inner`
    /// actually produces — so they overstated a commit by about an order of
    /// magnitude.
    ///
    /// **That 37% is why a buffer pool was built here and then removed.** A pool
    /// recycles the allocation but a recycled buffer still has to be cleared, and
    /// the clear is the same `memset` as a fresh `vec![0.0; n]` — so pooling can
    /// only address the allocator's 42%, and only when the pool is non-empty.
    /// Here it never is: `commit_inner` clones *before* it retires the previous
    /// generation, so nothing has been returned at the moment the clone asks.
    /// Measured, `Buffers::new` and a pooled hit came out identical within noise.
    ///
    /// **What did work: not cloning what carries nothing.** The block scratch
    /// (`scratch_in`/`scratch_out`, 64 KB per channel — 40% of a unit's bytes)
    /// is overwritten every block before it is read, so a clone leaves it empty
    /// and [`AudioUnit::allocate`] sizes it. That removes both halves of the
    /// cost for those bytes, because a buffer never allocated is also never
    /// zeroed. Re-profiled, the clone phase fell 461 → 215 samples (-53%) while
    /// `fresh_construction`, which still allocates eagerly, held at 180 → 179 —
    /// the control that says the drop is this change and not the machine.
    ///
    /// # This is no longer on the commit path
    ///
    /// `Unit::clone` shares the vocoder bank by refcount (see [`Bank`]), so a
    /// graph commit does not reach this function at all. It runs only from
    /// [`AudioUnit::isolate`], where an offline render needs private state.
    ///
    /// The history is worth keeping, because it is what the design was measured
    /// against. When a commit *did* deep-clone: 201.8 MB per commit at stereo and
    /// 604.6 MB at six channels, median 70-135 ms and 393-488 ms against a 2 ms
    /// budget. Sharing the bank took that to 81.5 / 243.8 MB, and moving the
    /// block scratch onto the bank as well took it to **1.3 / 3.3 MB** — a ~180x
    /// reduction, with both widths committing in ~0.2 ms. What remains is
    /// fundsp's own per-`Vertex` bookkeeping, not this state.
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
    /// between bins — line for line, bin `k` in is bin `k` out. Scaling the phase
    /// advance by a pitch ratio, which this function used to do, therefore
    /// transposes nothing; it only decorrelates each bin's phase from its
    /// magnitude.
    ///
    /// That was measurably *worse than omitting it*. Feeding 440 Hz and asking
    /// for ±1200 cents, the scaling produced 411 Hz and 408 Hz — the same wrong
    /// answer in both directions, so not even a wrong-ratio bug — at 6 dB down.
    /// Held alongside the correct read-rate fix it still cost 8.7 dB at +1200 and
    /// 12.5 dB at +700, pulling exact pitch off by up to 47 Hz.
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
        // `fft_roundtrip_is_the_identity`). The original code divided anyway,
        // attenuating the stretched signal by the FFT size — 60 dB at 1024, 66
        // at 2048. It read as "stretching mutes the voice" rather than as a
        // gain bug, which is why it survived: every test asserted only that
        // output was non-zero, and 0.0004 is non-zero.
        inverse_fft(&mut self.spectrum);
        for i in 0..size {
            self.output
                .add_at(i, self.spectrum[i].re * self.window[i] * COLA_GAIN);
        }

        // Zero the span the next frame will accumulate into, which this one has
        // already scrolled past.
        for i in 0..synthesis_hop {
            self.output.clear_at(size + i);
        }
        self.output.advance(synthesis_hop);
    }
}

/// Wrap a phase into (-π, π].
///
/// Arithmetic, not a `while` loop: the loop it replaces ran once per 2π of
/// input, so a large accumulated phase cost unbounded iterations on the audio
/// thread.
#[inline]
pub(super) fn wrap_phase(phase: Radians) -> Radians {
    let tau = Radians::TAU.get();
    let p = phase.get();
    Radians(p - tau * ((p + std::f32::consts::PI) / tau).floor())
}

/// Overlap-add normalization for a Hann analysis/synthesis pair at 75% overlap.
///
/// Windowing twice — once on analysis, once on synthesis — means the overlapped
/// frames sum to `Σ hann²` per sample rather than to unity. At a hop of
/// `window / 4` that sum is exactly `4 × mean(hann²) = 4 × 3/8 = 1.5`, so
/// synthesis divides it back out. Without this the vocoder is 3.5 dB hot.
///
/// Only correct at 75% overlap, which is why [`FftSize::hop`] is fixed at
/// `size / 4` rather than configurable.
const COLA_GAIN: f32 = 1.0 / 1.5;
