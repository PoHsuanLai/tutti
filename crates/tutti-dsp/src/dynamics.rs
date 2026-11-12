//! Dynamics algorithms: the envelope followers, the gain curves, the
//! compressor, gate, limiter and brickwall families, and the limiter's lookahead ring.

mod brickwall;
mod compressor;
mod compressor_kernel;
mod curves;
mod envelope;
mod gate;
mod key_source;
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
pub use envelope::EnvelopeFollower;
pub use gate::{GateControls, GateFamily, GateMember, GateRecurrence};
pub use key_source::KeySource;
pub use limiter::{LimiterControls, LimiterFamily, LimiterMember, LimiterRecurrence};
