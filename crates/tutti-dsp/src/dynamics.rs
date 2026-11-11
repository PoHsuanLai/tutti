//! Dynamics algorithms: the envelope followers, the gain curves, the
//! compressor, gate and limiter kernels, and the limiter's lookahead ring.

mod compressor;
mod curves;
mod envelope;
mod gate;
mod limiter;
mod lookahead;
mod window;

pub use compressor::{CompressorBlock, CompressorKernel};
pub use curves::{
    LimiterCurve, amplitude_to_db, compute_compressor_gain_reduction, compute_limiter_gain, ramp_db,
};
pub use envelope::{EnvelopeFollower, GateEnvelopeFollower};
pub use gate::{GateBlock, GateKernel};
pub use limiter::{LimiterBlock, LimiterKernel};
pub use lookahead::{LookaheadRing, brickwall_clip, fold_peak};
