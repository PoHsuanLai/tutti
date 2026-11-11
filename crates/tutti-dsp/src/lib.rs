#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]
#![cfg_attr(docsrs, feature(doc_cfg))]
//!
//! ## Items
//!
//! By family, as the source is laid out (each family's design, model and kernel side
//! by side):
//!
//! - **The model** (design 024, `model/`): the traits every family answers to. A
//!   [`Design`] maps a family's controls to its [`Model`](Design::Model) and states its
//!   buses ([`Channels`]); a [`System`] is what every model answers (its
//!   [`latency`](System::latency) and [`tail`](System::tail)), and [`Lti`] the class of
//!   a linear time-invariant one ([`frequency_response`](Lti::frequency_response),
//!   [`spectral_radius`](Lti::spectral_radius), [`l1_norm`](Lti::l1_norm),
//!   [`hinf_norm`](Lti::hinf_norm), with [`Complex64`]). A [`Realization`] runs a model;
//!   the one in use is [`Realized`] over a [`Kernel`] (the per-frame math), whose
//!   coefficients approach each new model by a [`Glide`]. [`Ceiling`] (with a family's
//!   [`Range`], [`MaxDecay`] for a reverb), [`Effort`] and [`Cadence`] are the
//!   engineering that sizes and schedules it.
//! - **Filters** (`filter/`): the SVF ([`SvfDesign`], [`SvfModel`], [`SvfType`]), the
//!   ladder ([`LadderDesign`], [`LadderModel`], [`LadderType`]) and the Linkwitz-Riley
//!   crossover ([`CrossoverDesign`], [`CrossoverModel`]; [`Crossover`], the block
//!   processor the multiband node still runs), each a [`ChannelKernel`] under the one
//!   [`Filter`] design, run by [`PerChannel`]; [`Real`], the `f32` or `f64` their state
//!   runs in, and the [`Biquad`] section they share.
//! - **Dynamics** (`dynamics/`): [`EnvelopeFollower`] and [`GateEnvelopeFollower`]; the
//!   gain curves ([`LimiterCurve`] and the `compute_*` functions); [`CompressorKernel`],
//!   [`GateKernel`] and [`LimiterKernel`] with their blocks; the limiter's
//!   [`LookaheadRing`].
//! - **Reverb** (`reverb/`, design 022): the model, a [`Network`] of [`Stage`]s with
//!   analytic queries ([`Network::rt60`], [`tail`](System::tail),
//!   [`is_stable`](Lti::is_stable), the densities) and a [`Bounds`] worst case; and two
//!   designs over it, [`FreeverbDesign`] (run by [`FreeverbKernel`]) and [`FdnDesign`]
//!   (run by [`FdnKernel`]). [`loop_gain`] and [`rt60_from_gain`] are the RT60 formula
//!   both ways.
//! - **Convolution**: [`Convolver`] over a shared [`IrSpectra`].
//! - **Delay and modulation** (`delay.rs`, `modulation/`): [`DelayLine`] with an
//!   [`InterpolationMode`]; [`SweptLines`], the chorus and flanger kernel; the phaser's
//!   all-pass chain ([`allpass_chain`]).
//! - **Spatial** (`spatial/`): VBAP ([`SpeakerLayout`], Pulkki's pairs and triplets and
//!   the gain solve over them, [`SpeakerGeometryError`]; [`VbapKernel`] and
//!   [`VbapGains`], a block's gains and one channel ramped between two) and the HRTF
//!   convolver ([`HrtfBinaural`]).
//! - **FFT** (`fft/`): [`RealFft`], a real-input FFT whose arithmetic does not depend on
//!   the CPU; [`StagedRealFft`], the same transform in steps; [`FftScratch`], reusable
//!   plans and buffers.
//! - **STFT vocabulary** (`stft/`): [`CosineWindow`] and the [`Window`] trait,
//!   [`StftGeometry`] (window, hop, shape and rate, validated together), and [`Grid`]
//!   with its typed indices; [`enum@Error`] and [`Result`] are what they refuse with.
//! - **Resampling** (`resample/`): [`WindowedSinc`], [`FftResampler`] (failing with a
//!   [`ResampleError`]) and [`StreamingResampler`], sized by a [`ChunkSize`].
//! - **Dither** (`dither/`): [`DitherNoise`] ([`DitherShape`]) and [`DitherSink`], the
//!   `tutti_types::Sink` adapter export and recording share.
//! - **Small kernels**: [`cubic_hermite`], [`ShapeKind`] and [`Shaper`], [`Smoother`],
//!   [`balance_gains`].
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

mod balance;
#[cfg(feature = "contract")]
pub mod contract;
mod control;
mod convolver;
mod delay;
mod dither;
mod dynamics;
mod fft;
mod filter;
mod interp;
mod modulation;
mod waveshape;

mod model;
mod resample;
mod reverb;
mod smoother;
mod spatial;
mod stft;
#[cfg(test)]
mod testing;

pub use balance::balance_gains;
pub use control::{COEFF_CADENCE, COEFF_INTERVAL, LANES, Ramp, lane_group};
pub use convolver::{Convolver, IrSpectra};
pub use delay::{DelayLine, InterpolationMode, delay_step, fractional_samples};
pub use dither::sink::{DitherSink, DitherSpec};
pub use dither::{DitherNoise, DitherShape};
pub use dynamics::{
    CompressorBlock, CompressorKernel, EnvelopeFollower, GateBlock, GateEnvelopeFollower,
    GateKernel, LimiterBlock, LimiterCurve, LimiterKernel, LookaheadRing, amplitude_to_db,
    brickwall_clip, compute_compressor_gain_reduction, compute_gate_gain, compute_limiter_gain,
    db_to_amplitude, fold_peak, gate_gain_at, ramp_db, ramp_is_at_end,
};
pub use fft::{Complex32, FftScratch, RealFft, StagedRealFft};
pub use filter::{
    Biquad, ChannelKernel, Crossover, CrossoverControls, CrossoverDesign, CrossoverError,
    CrossoverModel, CrossoverTree, DirectForm1, Filter, FilterSizes, Ladder, LadderCoeffs,
    LadderControls, LadderDesign, LadderModel, LadderType, MAX_SPLITS, PerChannel, Real,
    SOLVE_CEILING, Svf, SvfCoeffs, SvfControls, SvfDesign, SvfModel, SvfType, fit_to_rate,
};
pub use interp::cubic_hermite;
pub use model::{
    Anchor, Approach, Cadence, Ceiling, Channels, Design, Effect, Effort, Glide, Kernel, Lerp, Lti,
    MaxDecay, MaxDecayError, Period, Range, Realization, Realized, Ring, Setup, Sizes, Step, Steps,
    System,
};
pub use modulation::allpass::{allpass_chain, allpass_coeff, sweep_hz};
pub use modulation::swept::SweptLines;
pub use resample::{ChunkSize, FftResampler, ResampleError, StreamingResampler, WindowedSinc};
pub use reverb::fdn::design::{FdnControls, FdnDesign, MAX_PRE_DELAY, MIN_DECAY};
pub use reverb::fdn::kernel::FdnKernel;
pub use reverb::freeverb::design::{FreeverbControls, FreeverbDesign};
pub use reverb::freeverb::kernel::{Freeverb, FreeverbKernel};
pub use reverb::network::{
    Absorption, AllPass, AllPassForm, Band, Bounds, Fdn, FirSummary, Full, Gains, Inline, Line,
    MAX_CHANNELS, MAX_DENSE, MAX_LINES, MAX_STAGES, MAX_TAPS, Matrix, Modulation, Network,
    OutputMap, Stage, Tap, Taps, loop_gain, pre_delay_frames, rt60_from_gain,
};
pub use rustfft::num_complex::Complex64;
pub use smoother::Smoother;
pub use spatial::{
    HRTF_LATENCY, HRTF_STEP, HrtfBinaural, HrtfBinauralError, MAX_PREPARED_RATE, MAX_SPEAKERS,
    MAX_SPHERE_RATE, MIN_SPHERE_RATE, PanningMode, SpeakerGeometryError, SpeakerLayout, Tuple,
    VbapGains, VbapKernel,
};
pub use stft::error::{Error, Result};
pub use stft::geometry::StftGeometry;
pub use stft::grid::{BinCount, BinIndex, FrameCount, FrameIndex, Grid};
pub use stft::window::{CosineWindow, Window};
pub use tutti_types::ChannelLayout;
pub use waveshape::{ShapeKind, Shaper};

/// A single frequency bin: a rectangular complex number.
///
/// `num_complex::Complex<f32>`, the type [`RealFft`] works in, so
/// bins cross the FFT boundary without conversion. The vocoder in
/// `tutti-sampler` and the [`Convolver`] work in the same type, so values cross
/// freely between them.
pub use Complex32 as Complex;
