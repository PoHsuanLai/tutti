//! Level-dependent gain processors: compressor, gate, limiter and the
//! multiband compressor.
//!
//! All three share one shape — measure a level, decide a gain, apply it — so
//! they share an envelope follower (`tutti_dsp::EnvelopeFollower`), a [`params`] vocabulary
//! (attack/release, threshold/knee) and the dB conversions and gain curves of `tutti-dsp`. What
//! differs is only the gain decision, which is why each keeps its own file and
//! nothing here is a trait. The compressor, the gate and the brickwall have moved onto the
//! family model (`FamilyNode` over `CompressorFamily`, `GateFamily` and `BrickwallFamily`,
//! which hold the recurrence); the limiter and the multiband are still hand-written.
//!
//! [`CompressorNode`] and [`GateNode`] take a typed [`Key`](tutti_dsp::Key): an
//! **external sidechain** (their level detector reads separate inputs, so the signal
//! being measured need not be the signal being shaped) or their own signal. [`MultibandCompressorNode`] runs one compressor core per
//! band of a [`Crossover`](tutti_dsp::Crossover), each detecting on its own band.

mod brickwall;
mod compressor;
mod compressor_core;
mod gate;
mod limiter;
mod multiband;

mod params;

pub use brickwall::BrickwallLimiterNode;
pub use compressor::CompressorNode;
pub use gate::GateNode;
pub use limiter::LimiterNode;
pub use multiband::{BAND_PARAMS, MultibandCompressorNode, MultibandControls};
