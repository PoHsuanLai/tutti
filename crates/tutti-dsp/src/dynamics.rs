//! Dynamics algorithms: the gain curves, the compressor, gate, limiter, brickwall and
//! multiband families, and the limiter's lookahead ring.

mod brickwall;
mod compressor;
mod curves;
mod gate;
mod key_source;
mod limiter;
mod lookahead;
mod multiband;
mod window;

pub use brickwall::{BrickwallControls, BrickwallFamily, BrickwallMember, BrickwallRecurrence};
pub use compressor::{
    CompressorControls, CompressorFamily, CompressorMember, CompressorRecurrence, GAIN_GLIDE,
};
pub use curves::{
    LimiterCurve, amplitude_to_db, compute_compressor_gain_reduction, compute_limiter_gain,
};
pub use gate::{GateControls, GateFamily, GateMember, GateRecurrence};
pub use key_source::KeySource;
pub use limiter::{
    LimiterControls, LimiterFamily, LimiterMember, LimiterRecurrence, Lookahead, LookaheadError,
};
pub use multiband::{
    BandReductions, BandSettings, MultibandFamily, MultibandMember, MultibandRecurrence,
    MultibandSettings, MultibandState,
};
