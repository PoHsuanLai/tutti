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
//!   [`Family`] maps a family's controls to its [`Member`](Family::Member) and states its
//!   buses ([`Channels`]); a [`System`] is what every member answers (its
//!   [`latency`](System::latency) and [`tail`](System::tail)), and [`Lti`] the class of
//!   a linear time-invariant one ([`frequency_response`](Lti::frequency_response),
//!   [`spectral_radius`](Lti::spectral_radius), [`l1_norm`](Lti::l1_norm),
//!   [`hinf_norm`](Lti::hinf_norm), with [`Complex64`]). A [`Recurrence`] runs a member:
//!   its [`coefficients_into`](Recurrence::coefficients_into) asks for the coefficient point
//!   of a member, its [`Approach`] (a [`Glide`]) moves the coefficients there, and its
//!   [`run`](Recurrence::run) is the per-frame math over a [`Feed`] (the audio, a side-chain
//!   key, and up to [`MAX_LANES`] per-frame control lanes); [`Realized`] is the one runner of
//!   them. A family whose state grows with its member is also [`Grows`]: its
//!   [`reach`](Grows::reach) over a [`Reach`] (with a family's [`Domain`], [`MaxDecay`] for a
//!   reverb) is a [`Capacity`], what its recurrence allocates; [`Effort`] and [`Cadence`]
//!   are the engineering that schedules it.
//! - **Filters** (`filter/`): the SVF ([`SvfFamily`], [`SvfModel`], [`SvfType`]), the
//!   ladder ([`LadderFamily`], [`LadderModel`], [`LadderType`]) and the Linkwitz-Riley
//!   crossover ([`CrossoverFamily`], [`CrossoverModel`]), each a [`ChannelRecurrence`] under the one
//!   [`Filter`] family, run by [`PerChannel`]; [`Real`], the `f32` or `f64` their state
//!   runs in, and the [`Biquad`] section they share.
//! - **Dynamics** (`dynamics/`): the
//!   gain curves ([`LimiterCurve`] and the `compute_*` functions); the lookahead limiter as a family ([`LimiterFamily`], run by
//!   [`LimiterRecurrence`], its threshold and ceiling lanes of the [`Feed`], its lookahead
//!   the member's latency); the brickwall clipper as a family
//!   ([`BrickwallFamily`], run by [`BrickwallRecurrence`], its ceiling a per-frame lane of
//!   the [`Feed`]); and the compressor ([`CompressorFamily`], run by
//!   [`CompressorRecurrence`], keyed, its threshold a lane, its makeup gliding over
//!   [`GAIN_GLIDE`]) and the noise gate ([`GateFamily`], run by [`GateRecurrence`], its
//!   range gliding likewise). Both read a [`KeySource`]: a side-chain bus or the signal itself.
//!   The multiband compressor is a family too ([`MultibandFamily`], run by
//!   [`MultibandRecurrence`]): a crossover tree per channel and the compressor's frame step per
//!   band, each band's makeup, bypass and solo gliding over [`GAIN_GLIDE`].
//! - **Reverb** (`reverb/`, design 022): the model, a [`Network`] of [`Stage`]s with
//!   analytic queries ([`Network::rt60`], [`tail`](System::tail),
//!   [`is_stable`](Lti::is_stable), the densities) and a [`NetworkCapacity`] worst case; and two
//!   families over it, [`FreeverbFamily`] (run by [`FreeverbRecurrence`]) and [`FdnFamily`]
//!   (run by [`FdnRecurrence`]). [`loop_gain`] and [`rt60_from_gain`] are the RT60 formula
//!   both ways.
//! - **Convolution**: [`Convolver`] over a shared [`IrSpectra`].
//! - **Delay and modulation** (`delay.rs`, `modulation/`): [`DelayLine`] with an
//!   [`InterpolationMode`]; the feedback delay as a family ([`DelayFamily`], run by
//!   [`DelayRecurrence`], a [`Grows`] family over a [`MaxDelay`], its cross-feedback a
//!   checked [`Routing`], its time and feedback lanes of the [`Feed`]); [`SweptLines`], the chorus and flanger kernel; the phaser's
//!   all-pass chain ([`allpass_chain`]).
//! - **Spatial** (`spatial/`): VBAP ([`SpeakerLayout`], Pulkki's pairs and triplets and
//!   the gain solve over them, [`SpeakerGeometryError`]; [`VbapKernel`] and
//!   [`VbapGains`], a block's gains and one channel ramped between two) and the HRTF
//!   convolver ([`HrtfBinaural`]).
//! - **FFT** (`fft/`): [`RealFft`], a real-input FFT whose arithmetic does not depend on
//!   the CPU (the convolver also runs it in steps); [`FftScratch`], reusable
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
//! - `contract` (off by default): the `contract` module, the cases a [`Family`] and
//!   its [`Realized`] runner are held to, with `family_contract!` and
//!   `recurrence_contract!`. Test support: enable it from a dev-dependency.
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
pub use control::{COEFF_CADENCE, COEFF_INTERVAL, LANES, Ramp};
pub use convolver::{Convolver, IrSpectra};
pub use delay::{
    ChannelTimes, ChannelTimesError, DELAY_SLEW, DelayCapacity, DelayControls, DelayFamily,
    DelayLine, DelayMember, DelayRecurrence, InterpolationMode, MAX_DELAY_CHANNELS, MaxDelay,
    MaxDelayError, Routing, RoutingError, delay_step, fractional_samples,
};
pub use dither::sink::{DitherSink, DitherSpec};
pub use dither::{DitherNoise, DitherShape};
pub use dynamics::{
    BandReductions, BandSettings, BrickwallControls, BrickwallFamily, BrickwallMember,
    BrickwallRecurrence, CompressorControls, CompressorFamily, CompressorMember,
    CompressorRecurrence, GAIN_GLIDE, GateControls, GateFamily, GateMember, GateRecurrence,
    KeySource, LimiterControls, LimiterCurve, LimiterFamily, LimiterMember, LimiterRecurrence,
    Lookahead, LookaheadError, MultibandFamily, MultibandMember, MultibandRecurrence,
    MultibandSettings, MultibandState, amplitude_to_db, compute_compressor_gain_reduction,
    compute_limiter_gain,
};
pub use fft::{Complex32, FftScratch, RealFft};
pub use filter::{
    Biquad, ChannelRecurrence, CrossoverControls, CrossoverError, CrossoverFamily, CrossoverModel,
    CrossoverTree, DirectForm1, Filter, Ladder, LadderCoefficients, LadderCoeffs, LadderControls,
    LadderFamily, LadderModel, LadderType, Lanes, MAX_SPLITS, PerChannel, Real, SOLVE_CEILING, Svf,
    SvfCoefficients, SvfCoeffs, SvfControls, SvfFamily, SvfModel, SvfType, fit_to_rate,
    ladder_step, svf_step,
};
pub use interp::cubic_hermite;
pub use model::{
    Anchor, Approach, Cadence, Capacity, Channels, Domain, Effort, Family, Feed, Glide, Grows,
    Lerp, Lti, MAX_LANES, MaxDecay, MaxDecayError, Period, Reach, Readout, Realized, Recurrence,
    Ring, Setup, Start, Step, Steps, System, TAIL_FLOOR,
};
pub use modulation::allpass::{allpass_chain, allpass_coeff, sweep_hz};
pub use modulation::swept::SweptLines;
pub use resample::{ChunkSize, FftResampler, ResampleError, StreamingResampler, WindowedSinc};
pub use reverb::fdn::design::{FdnControls, FdnFamily, MAX_PRE_DELAY, MIN_DECAY};
pub use reverb::fdn::recurrence::FdnRecurrence;
pub use reverb::freeverb::design::{FreeverbControls, FreeverbFamily};
pub use reverb::freeverb::recurrence::{Freeverb, FreeverbRecurrence};
pub use reverb::network::{
    Absorption, AllPass, AllPassForm, Band, Fdn, FirSummary, Full, Gains, Inline, Line,
    MAX_CHANNELS, MAX_DENSE, MAX_LINES, MAX_STAGES, MAX_TAPS, Matrix, Modulation, Network,
    NetworkCapacity, OutputMap, Stage, Tap, Taps, loop_gain, pre_delay_frames, rt60_from_gain,
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
pub use waveshape::{ShapeKind, Shaper};
