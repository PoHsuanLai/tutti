//! Sample-rate conversion: a fixed-input FFT block resampler.

mod fft;

pub use fft::FftResampler;

/// Why an [`FftResampler`] could not be built or fed.
#[derive(Debug, Clone, Copy, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum ResampleError {
    /// A rate that is not a finite value of at least 0.5 Hz and under 2^32 Hz.
    #[error("invalid sample rate: {hz} Hz")]
    InvalidRate {
        /// The rejected rate.
        hz: f64,
    },
    /// A rate of 0 Hz.
    #[error("invalid sample rates, input: {source_hz}, output: {target_hz}")]
    ZeroRate {
        /// The input rate, in Hz.
        source_hz: usize,
        /// The output rate, in Hz.
        target_hz: usize,
    },
    /// No channels, no chunk, or no sub-chunks.
    #[error("a resampler needs channels, a chunk and sub-chunks")]
    EmptyShape,
    /// The rate pair needs an FFT block over the supported size.
    #[error(
        "{source_hz} Hz -> {target_hz} Hz needs a block over {} frames",
        fft::MAX_BLOCK
    )]
    BlockTooLarge {
        /// The input rate, in Hz.
        source_hz: usize,
        /// The output rate, in Hz.
        target_hz: usize,
    },
    /// The planes handed to a call do not hold a chunk in and room for the output.
    #[error(
        "buffers do not fit: {channels} channels of {frames_in} frames in, room for {frames_out} out"
    )]
    BufferMismatch {
        /// The channel count the resampler was built for.
        channels: usize,
        /// Input frames per plane the call takes.
        frames_in: usize,
        /// Output frames per plane the call needs room for.
        frames_out: usize,
    },
}
