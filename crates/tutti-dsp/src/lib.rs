#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]
#![cfg_attr(docsrs, feature(doc_cfg))]
//!
//! ## Items
//!
//! - FFT: [`RealFft`], a real-input FFT on rustfft's scalar planner whose
//!   arithmetic does not depend on the CPU; [`StagedRealFft`], the same
//!   transform in steps; [`FftScratch`], reusable plans and buffers.
//! - Filters: [`SvfCoeffs`] and [`SvfCoefficients`] with [`svf_step`]
//!   ([`SvfType`]); [`LadderCoeffs`] and [`LadderCoefficients`] with
//!   [`ladder_step`] ([`LadderType`]); the Linkwitz-Riley [`Crossover`]; and
//!   [`Real`], the `f32` or `f64` their state runs in.
//! - Dynamics: [`EnvelopeFollower`] and [`GateEnvelopeFollower`]; the gain
//!   curves [`compute_compressor_gain_reduction`], [`compute_gate_gain`],
//!   [`compute_limiter_gain`] and [`LimiterCurve`]; [`CompressorKernel`], [`LimiterKernel`] and
//!   [`GateKernel`] with their [`CompressorBlock`] and [`GateBlock`]; the
//!   limiter's [`LookaheadRing`]; and the ring buffers [`CircularBuffer`] and
//!   [`MonotonicMinDeque`].
//! - Convolution: [`Convolver`] over a shared [`IrSpectra`].
//! - Delay: [`DelayLine`] with an [`InterpolationMode`], and [`SweptLines`], the
//!   chorus and flanger kernel.
//! - Interpolation: [`cubic_hermite`], the four-tap Catmull-Rom kernel the delay
//!   line and the sampler's readers share.
//! - Reverb: [`Freeverb`], mono in and stereo out, a damped comb bank and
//!   all-pass chain whose comb feedback and damping are set through its
//!   [`Realization`] impl.
//! - Reverb model (design 022): a [`Network`] of [`Stage`]s, a feedback delay
//!   network as a plain fixed-capacity value, with analytic queries
//!   ([`Network::rt60`], [`tail`](Network::tail), [`is_stable`](Network::is_stable),
//!   the densities) and a [`Bounds`] worst case; a [`Design`] maps its controls
//!   to one ([`FreeverbDesign`] for [`Freeverb`]) and names the [`Realization`] that runs
//!   it, prepared from a [`Setup`] and moved to each network by `retarget`; [`loop_gain`] and
//!   [`rt60_from_gain`] are the RT60 formula both ways.
//! - Waveshaping: [`ShapeKind`] and [`Shaper`].
//! - Frame-by-bin data: [`StftGeometry`] (window, hop, shape and rate, validated
//!   together), and [`Grid`] with its typed indices; [`enum@Error`] and [`Result`].
//! - Windows: [`CosineWindow`] (Hann, Hamming, Blackman) and the [`Window`] trait,
//!   shared by the analyzers' STFT geometry and the sampler's vocoder.
//! - Resampling: [`WindowedSinc`] (the band-limited interpolation kernel) and [`FftResampler`], a fixed-input FFT block resampler on the
//!   same scalar FFT, failing with a [`ResampleError`]; and
//!   [`StreamingResampler`], which feeds it a block of any size at a time and
//!   compensates its delay and tail, sized by a [`ChunkSize`].
//! - Dither: [`DitherNoise`], the seeded rectangular or triangular ([`DitherShape`])
//!   noise that decorrelates quantization, continuous across blocks.

mod allpass;
mod balance;
mod buffer;
#[cfg(test)]
mod contract;
mod control;
mod convolver;
mod delay;
mod dither;
mod dynamics;
mod error;
mod fft;
mod filter;
mod geometry;
mod grid;
mod interp;
mod swept_delay;
mod waveshape;
mod window;

mod fdn;
mod fdn_design;
mod network;
mod resample;
mod reverb;
mod reverb_design;
#[cfg(test)]
mod test_support;

pub use allpass::{allpass_chain, allpass_coeff, sweep_hz};
pub use balance::balance_gains;
pub use buffer::{CircularBuffer, MonotonicMinDeque};
pub use control::{COEFF_INTERVAL, LANES, Ramp, lane_group, segments};
pub use convolver::{Convolver, IrSpectra};
pub use delay::{DelayLine, InterpolationMode, delay_step, fractional_samples};
pub use dither::{DitherNoise, DitherShape};
pub use dynamics::{
    CompressorBlock, CompressorKernel, EnvelopeFollower, GateBlock, GateEnvelopeFollower,
    GateKernel, LimiterBlock, LimiterCurve, LimiterKernel, LookaheadRing, amplitude_to_db,
    brickwall_clip, compute_compressor_gain_reduction, compute_gate_gain, compute_limiter_gain,
    db_to_amplitude, fold_peak, gate_gain_at, ramp_db, ramp_is_at_end, smooth_envelope,
};
pub use error::{Error, Result};
pub use fdn::FdnKernel;
pub use fdn_design::{FdnControls, FdnDesign, MAX_PRE_DELAY, MIN_DECAY};
pub use fft::{Complex32, FftScratch, RealFft, StagedRealFft};
pub use filter::{
    Crossover, CrossoverError, LadderCoefficients, LadderCoeffs, LadderType, MAX_SPLITS, Real,
    SOLVE_CEILING, SvfCoefficients, SvfCoeffs, SvfType, fit_to_rate, ladder_step, svf_step,
};
pub use geometry::StftGeometry;
pub use grid::{BinCount, BinIndex, FrameCount, FrameIndex, Grid};
pub use interp::cubic_hermite;
pub use network::{
    Absorption, AllPass, AllPassForm, Band, Bounds, Ceiling, Channels, Design, Effort, Fdn,
    FirSummary, Full, Gains, Inline, Line, MAX_CHANNELS, MAX_DENSE, MAX_LINES, MAX_STAGES,
    MAX_TAPS, Matrix, Modulation, Network, OutputMap, Realization, Ring, Setup, Stage, Tap, Taps,
    loop_gain, pre_delay_frames, rt60_from_gain,
};
pub use resample::{ChunkSize, FftResampler, ResampleError, StreamingResampler, WindowedSinc};
pub use reverb::Freeverb;
pub use reverb_design::{FreeverbControls, FreeverbDesign};
pub use swept_delay::SweptLines;
pub use tutti_types::ChannelLayout;
pub use waveshape::{ShapeKind, Shaper};
pub use window::{CosineWindow, Window};

/// A single frequency bin: a rectangular complex number.
///
/// `num_complex::Complex<f32>`, the type [`RealFft`] works in, so
/// bins cross the FFT boundary without conversion. The vocoder in
/// `tutti-sampler` and the [`Convolver`] work in the same type, so values cross
/// freely between them.
pub use Complex32 as Complex;
