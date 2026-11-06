//! Dynamics algorithms: the envelope followers, the gain curves, the
//! compressor and gate kernels, and the limiter's lookahead ring.

mod compressor;
mod curves;
mod envelope;
mod gate;
mod limiter;

pub use compressor::{CompressorBlock, CompressorKernel};
pub use curves::{
    LimiterCurve, amplitude_to_db, compute_compressor_gain_reduction, compute_gate_gain,
    compute_limiter_gain, db_to_amplitude, gate_gain_at, ramp_db, ramp_is_at_end, smooth_envelope,
};
pub use envelope::{EnvelopeFollower, GateEnvelopeFollower};
pub use gate::{GateBlock, GateKernel};
pub use limiter::{LookaheadRing, brickwall_clip, fold_peak};
