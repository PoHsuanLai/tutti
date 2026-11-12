//! Level-dependent gain processors: compressor, gate, limiter and the
//! multiband compressor.
//!
//! All of them share one shape — measure a level, decide a gain, apply it — and the dB
//! conversions and gain curves of `tutti-dsp`. What differs is only the gain decision, which is
//! why each keeps its own file and nothing here is a trait. They have moved onto the family
//! model (`FamilyNode` over `CompressorFamily`, `GateFamily`, `LimiterFamily` and
//! `BrickwallFamily`, which hold the recurrence; the multiband is a hand-written node over
//! `Realized<MultibandRecurrence>`, see its file).
//!
//! [`CompressorNode`] and [`GateNode`] take a typed [`KeySource`](tutti_dsp::KeySource): an
//! **external sidechain** (their level detector reads separate inputs, so the signal
//! being measured need not be the signal being shaped) or their own signal. [`MultibandCompressorNode`] runs one compressor per
//! band of a Linkwitz-Riley crossover, each detecting on its own band.

mod brickwall;
mod compressor;
mod gate;
mod limiter;
mod multiband;

pub use brickwall::BrickwallLimiterNode;
pub use compressor::CompressorNode;
pub use gate::GateNode;
pub use limiter::LimiterNode;
pub use multiband::{BAND_PARAMS, MultibandCompressorNode, MultibandControls};
