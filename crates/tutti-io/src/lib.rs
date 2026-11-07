#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]
#![cfg_attr(docsrs, feature(doc_cfg))]

mod error;
mod mic;
mod pump_loop;
mod recorder;
mod tap_in;
mod wav_out;

pub use error::{Error, Result};
pub use mic::{MicMonitorNode, MicRing};
pub use pump_loop::{
    IDLE_PARK, ManualDriver, ManualPump, PumpDriver, PumpLoop, PumpPass, RunningPump, ThreadDriver,
};
pub use recorder::{FinalizeStatus, Recorder};
pub use tap_in::TapIn;
pub use wav_out::WavOut;
