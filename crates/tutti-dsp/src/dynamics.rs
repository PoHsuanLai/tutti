//! Dynamics algorithms: the envelope followers, the gain curves, the
//! compressor, gate and limiter kernels, and the limiter's lookahead ring.

mod brickwall;
mod compressor;
mod compressor_kernel;
mod curves;
mod envelope;
mod gate;
mod limiter;
mod lookahead;
mod window;

pub use brickwall::{BrickwallControls, BrickwallFamily, BrickwallMember, BrickwallRecurrence};
pub use compressor::{
    CompressorControls, CompressorFamily, CompressorMember, CompressorRecurrence, GAIN_GLIDE,
};
pub use compressor_kernel::{CompressorBlock, CompressorKernel};
pub use curves::{
    LimiterCurve, amplitude_to_db, compute_compressor_gain_reduction, compute_limiter_gain, ramp_db,
};
pub use envelope::{EnvelopeFollower, GateEnvelopeFollower};
pub use gate::{GateBlock, GateKernel};
pub use limiter::{LimiterBlock, LimiterKernel};
pub use lookahead::{LookaheadRing, fold_peak};
