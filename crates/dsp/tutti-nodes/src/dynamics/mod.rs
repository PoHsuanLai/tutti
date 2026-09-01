//! Level-dependent gain processors: compressor, gate and limiter.
//!
//! All three share one shape — measure a level, decide a gain, apply it — so
//! they share an [`envelope`] follower, a [`params`] vocabulary
//! (attack/release, threshold/knee) and the dB conversions in [`utils`]. What
//! differs is only the gain decision, which is why each keeps its own file and
//! nothing here is a trait.
//!
//! [`CompressorNode`] and [`GateNode`] take an **external sidechain**: their level
//! detector reads separate inputs, so the signal being measured need not be the
//! signal being shaped.

mod envelope;
mod utils;

mod compressor;
mod gate;
mod limiter;

mod params;

pub use compressor::CompressorNode;
pub use gate::GateNode;
pub use limiter::{BrickwallLimiterNode, LimiterNode};
