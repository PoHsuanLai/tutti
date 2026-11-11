#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]
#![cfg_attr(docsrs, feature(doc_cfg))]
//!
//! ## Items
//!
//! - FFT: [`RealFft`], a real-input FFT on rustfft's scalar planner whose
//!   arithmetic does not depend on the CPU; [`StagedRealFft`], the same
//!   transform in steps; [`FftScratch`], reusable plans and buffers.
//! - Filters: [`SvfCoeffs`] and the SVF as a design ([`SvfDesign`], [`SvfType`]); [`LadderCoeffs`] and the ladder as a design
//!   ([`LadderDesign`], [`LadderType`]); the Linkwitz-Riley crossover as a design ([`CrossoverDesign`]) and as a state over
//!   one channel ([`Crossover`]); and
//!   [`Real`], the `f32` or `f64` their state runs in; their models ([`SvfModel`],
//!   [`LadderModel`], [`CrossoverModel`]) and the [`Biquad`] section they share.
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
//! - The model (design 024): the traits every family of processor answers to.
//!   A [`Design`] maps a family's controls to its [`Model`](Design::Model); a
//!   [`System`] is what every model answers (its [`latency`](System::latency) and
//!   [`tail`](System::tail)) and [`Lti`] is the class of a linear time-invariant one
//!   (its [`frequency_response`](Lti::frequency_response), [`spectral_radius`](Lti::spectral_radius),
//!   [`l1_norm`](Lti::l1_norm) and [`hinf_norm`](Lti::hinf_norm), with [`Complex64`]);
//!   a [`Realization`] runs a model (one of several), its block shape an [`Effect`],
//!   prepared from a [`Setup`] and the design's [`Sizes`] and moved to each model by
//!   `retarget`. [`Ceiling`] (with a family's [`Range`], [`MaxDecay`] for a reverb),
//!   [`Effort`] and [`Cadence`] are the engineering that sizes and schedules it.
//! - Reverb model (design 022): a [`Network`] of [`Stage`]s, a feedback delay
//!   network as a plain fixed-capacity value, a [`System`] and an [`Lti`], with
//!   analytic queries ([`Network::rt60`], [`tail`](System::tail),
//!   [`is_stable`](Lti::is_stable), the densities) and a [`Bounds`] worst case;
//!   [`FreeverbDesign`] and [`FdnDesign`] map controls to one, for [`Freeverb`] and
//!   [`FdnKernel`]; [`loop_gain`] and [`rt60_from_gain`] are the RT60 formula both
//!   ways.
//! - Spatial: [`SpeakerLayout`], the pairs or triplets of a speaker array and Pulkki's
//!   vector base amplitude panning solve over it ([`SpeakerLayout::gains_into`]), with a
//!   [`SpeakerGeometryError`] for a layout that cannot be built; [`VbapKernel`] and [`VbapGains`],
//!   the block's gains at the two ends of a ramp and one channel rendered between them; and
//!   [`HrtfBinaural`].
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
//!
//! ## Feature flags
//!
//! - `contract` (off by default): the `contract` module, the cases a [`Design`] and
//!   its [`Realization`] are held to, with `design_contract!` and
//!   `realization_contract!`. Test support: enable it from a dev-dependency.
//!
//! ## Import paths
//!
//! Every public item is re-exported at the crate root; import from there
//! (`tutti_dsp::RealFft`). The only public module is `contract`, which has paths of
//! its own.

mod allpass;
mod balance;
mod buffer;
#[cfg(feature = "contract")]
pub mod contract;
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
mod model;
mod network;
mod resample;
mod reverb;
mod reverb_design;
mod smoother;
mod spatial;
#[cfg(test)]
mod test_support;

pub use allpass::{allpass_chain, allpass_coeff, sweep_hz};
pub use balance::balance_gains;
pub use buffer::{CircularBuffer, MonotonicMinDeque};
pub use control::{COEFF_CADENCE, COEFF_INTERVAL, LANES, Ramp, lane_group};
pub use convolver::{Convolver, IrSpectra};
pub use delay::{DelayLine, InterpolationMode, delay_step, fractional_samples};
pub use dither::{DitherNoise, DitherShape};
pub use dynamics::{
    CompressorBlock, CompressorKernel, EnvelopeFollower, GateBlock, GateEnvelopeFollower,
    GateKernel, LimiterBlock, LimiterCurve, LimiterKernel, LookaheadRing, amplitude_to_db,
    brickwall_clip, compute_compressor_gain_reduction, compute_gate_gain, compute_limiter_gain,
    db_to_amplitude, fold_peak, gate_gain_at, ramp_db, ramp_is_at_end,
};
pub use error::{Error, Result};
pub use fdn::FdnKernel;
pub use fdn_design::{FdnControls, FdnDesign, MAX_PRE_DELAY, MIN_DECAY};
pub use fft::{Complex32, FftScratch, RealFft, StagedRealFft};
pub use filter::{
    Biquad, ChannelKernel, Crossover, CrossoverControls, CrossoverDesign, CrossoverError,
    CrossoverModel, CrossoverTree, DirectForm1, Filter, FilterSizes, Ladder, LadderCoeffs,
    LadderControls, LadderDesign, LadderModel, LadderType, MAX_SPLITS, PerChannel, Real,
    SOLVE_CEILING, Svf, SvfCoeffs, SvfControls, SvfDesign, SvfModel, SvfType, fit_to_rate,
};
pub use geometry::StftGeometry;
pub use grid::{BinCount, BinIndex, FrameCount, FrameIndex, Grid};
pub use interp::cubic_hermite;
pub use model::{
    Anchor, Approach, Cadence, Ceiling, Channels, Design, Effect, Effort, Glide, Kernel, Lerp, Lti,
    MaxDecay, MaxDecayError, Period, Range, Realization, Realized, Ring, Setup, Sizes, Step,
    System,
};
pub use network::{
    Absorption, AllPass, AllPassForm, Band, Bounds, Fdn, FirSummary, Full, Gains, Inline, Line,
    MAX_CHANNELS, MAX_DENSE, MAX_LINES, MAX_STAGES, MAX_TAPS, Matrix, Modulation, Network,
    OutputMap, Stage, Tap, Taps, loop_gain, pre_delay_frames, rt60_from_gain,
};
pub use resample::{ChunkSize, FftResampler, ResampleError, StreamingResampler, WindowedSinc};
pub use reverb::Freeverb;
pub use reverb_design::{FreeverbControls, FreeverbDesign};
pub use rustfft::num_complex::Complex64;
pub use smoother::Smoother;
pub use spatial::{
    HRTF_LATENCY, HRTF_STEP, HrtfBinaural, HrtfBinauralError, MAX_PREPARED_RATE, MAX_SPEAKERS,
    MAX_SPHERE_RATE, MIN_SPHERE_RATE, PanningMode, SpeakerGeometryError, SpeakerLayout, Tuple,
    VbapGains, VbapKernel,
};
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
