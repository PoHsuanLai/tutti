#![doc = include_str!("../README.md")]

mod node_id;

mod error;
mod mic;
mod recorder;
mod tap_in;
mod wav_out;

pub use error::{Error, Result};
pub use mic::{share_mic_ring, MicMonitorNode, MicRing};
pub use recorder::{
    FinalizeStatus, ManualDriver, ManualPump, PumpDriver, PumpLoop, PumpPass, Recorder,
    RunningPump, ThreadDriver,
};
pub use tap_in::TapIn;
// `MAX_WAV_FOLD_CHANNELS` is the widest input `WavOut` will fold, so a caller
// sizing a buffer for it has to name the same ceiling.
pub use wav_out::{WavOut, MAX_WAV_FOLD_CHANNELS};

// The traits this crate implements, re-exported so a consumer reaches the
// vocabulary and its live impls from one place.
pub use tutti_core::io::{pump, AudioIn, AudioOut, OnEmpty};
pub use tutti_core::pcm::BitDepth;
