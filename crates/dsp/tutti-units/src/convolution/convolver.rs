//! Mono FFT convolver primitive.
//!
//! Wraps [`fft_convolver::FFTConvolver`] with a `FftPartitionState`
//! scratch block so `process_sample` is allocation-free after `new()`.

use fft_convolver::FFTConvolver;

/// Default FFT block size when the caller doesn't pick one. Must be a
/// power of two; `FFTConvolver::init` rejects anything else.
pub(crate) const DEFAULT_BLOCK_SIZE: usize = 512;

/// Linear scratch blocks the partitioned-FFT convolver drains/fills
/// once per `block_size` samples.
///
/// Not a ring — the input block fills linearly, gets passed wholesale
/// to `FFTConvolver::process`, and resets. The output block is read
/// once per sample and rewritten in full on the next FFT cycle. This
/// shape is distinct enough from [`crate::buffer::CircularBuffer`]
/// that reusing the ring would be a forced fit.
#[derive(Clone)]
struct FftPartitionState {
    block_size: usize,
    input: Vec<f32>,
    input_fill: usize,
    output: Vec<f32>,
    output_cursor: usize,
}

impl FftPartitionState {
    fn new(block_size: usize) -> Self {
        Self {
            block_size,
            input: vec![0.0; block_size],
            input_fill: 0,
            output: vec![0.0; block_size],
            // Start drained — first `block_size` samples read zero (fill-up
            // latency).
            output_cursor: block_size,
        }
    }

    /// Next sample from the already-computed output block, or `0.0` if
    /// the block has been drained.
    #[inline]
    fn next_output(&mut self) -> f32 {
        if self.output_cursor < self.block_size {
            let s = self.output[self.output_cursor];
            self.output_cursor += 1;
            s
        } else {
            0.0
        }
    }

    /// Write `sample` into the input-accumulation block. Returns `true`
    /// when the block is full and `drain` must be run.
    #[inline]
    fn push_input(&mut self, sample: f32) -> bool {
        self.input[self.input_fill] = sample;
        self.input_fill += 1;
        self.input_fill >= self.block_size
    }

    /// Hand the filled input block to `fft`, overwrite the output
    /// block with the result, and reset both cursors.
    #[inline]
    fn drain_through(&mut self, fft: &mut FFTConvolver<f32>) {
        // Buffers are pre-sized to `block_size`, so `process` cannot fail here.
        // On the audio thread we still degrade to silence rather than panic
        // across the callback boundary; `debug_assert` catches a sizing
        // regression in tests.
        let result = fft.process(&self.input, &mut self.output);
        debug_assert!(
            result.is_ok(),
            "FFTConvolver::process failed on pre-sized buffers"
        );
        if result.is_err() {
            self.output.fill(0.0);
        }
        self.input_fill = 0;
        self.output_cursor = 0;
    }

    fn clear(&mut self) {
        self.input.fill(0.0);
        self.input_fill = 0;
        self.output.fill(0.0);
        self.output_cursor = self.block_size;
    }

    #[inline]
    fn footprint(&self) -> usize {
        (self.input.capacity() + self.output.capacity()) * core::mem::size_of::<f32>()
    }
}

/// Real-time-safe mono convolver.
///
/// Composes three pieces:
/// - the `fft-convolver` FFT engine,
/// - a `FftPartitionState` scratch block sized at construction,
/// - the IR length (for reporting only).
///
/// The hot path (`process_sample`) never allocates.
#[derive(Clone)]
pub struct Convolver {
    fft: FFTConvolver<f32>,
    partition: FftPartitionState,
    ir_length: usize,
}

impl Convolver {
    /// Create a new convolver from an impulse response.
    ///
    /// `block_size` is rounded up to the next power of two.
    pub fn new(ir: &[f32], block_size: usize) -> Self {
        let block_size = block_size.next_power_of_two().max(2);

        let mut fft = FFTConvolver::default();
        fft.init(block_size, ir)
            .expect("FFTConvolver::init rejected a power-of-two block size");

        Self {
            fft,
            partition: FftPartitionState::new(block_size),
            ir_length: ir.len(),
        }
    }

    /// Create with the default block size (`512`).
    pub fn with_ir(ir: &[f32]) -> Self {
        Self::new(ir, DEFAULT_BLOCK_SIZE)
    }

    /// IR length in samples.
    pub fn ir_length(&self) -> usize {
        self.ir_length
    }

    /// Latency introduced by the partitioned FFT (== block size).
    pub fn latency(&self) -> usize {
        self.partition.block_size
    }

    /// Produce one convolved sample. Returns `0.0` for the first
    /// `block_size` samples (fill-up latency).
    #[inline]
    pub fn process_sample(&mut self, input: f32) -> f32 {
        let out = self.partition.next_output();
        if self.partition.push_input(input) {
            self.partition.drain_through(&mut self.fft);
        }
        out
    }

    /// Zero the working buffers without discarding the IR. Safe to
    /// call on a stream discontinuity.
    pub fn reset(&mut self) {
        self.partition.clear();
    }

    /// Scratch-buffer memory footprint (excludes the FFT engine's own
    /// internal state, which `fft-convolver` does not expose).
    pub(crate) fn scratch_footprint(&self) -> usize {
        self.partition.footprint()
    }
}
