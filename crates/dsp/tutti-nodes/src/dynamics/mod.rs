//! Level-dependent gain processors: compressor, gate, limiter and the
//! multiband compressor.
//!
//! All three share one shape — measure a level, decide a gain, apply it — so
//! they share an envelope follower (`tutti_dsp::EnvelopeFollower`), a [`params`] vocabulary
//! (attack/release, threshold/knee) and the dB conversions and gain curves of `tutti-dsp`. What
//! differs is only the gain decision, which is why each keeps its own file and
//! nothing here is a trait.
//!
//! [`CompressorNode`] and [`GateNode`] take an **external sidechain**: their level
//! detector reads separate inputs, so the signal being measured need not be the
//! signal being shaped. [`MultibandCompressorNode`] runs one compressor core per
//! band of a [`Crossover`](crate::Crossover), each detecting on its own band.

mod utils;

mod compressor;
mod gate;
mod limiter;
mod multiband;

mod params;

pub use compressor::{COMPRESSOR_PARAMS, CompressorNode};
pub use gate::{GATE_PARAMS, GateNode};
pub use limiter::{BRICKWALL_PARAMS, BrickwallLimiterNode, LIMITER_PARAMS, LimiterNode};
pub use multiband::{BAND_PARAMS, MultibandCompressorNode, MultibandControls};
