//! Level monitoring: what the audio callback measures and what other threads
//! read.
//!
//! [`MasterMeter`] holds the output's peak and RMS, [`AudioTap`] hands the
//! callback's output to an analysis consumer, and [`LoadMeter`] reports how
//! much of each block's time budget the render used. [`meter_output`] feeds
//! them from one block.

#![forbid(unsafe_code)]
#![cfg_attr(docsrs, feature(doc_cfg))]

mod amplitude;
mod load;
mod rt;
mod tap;

pub use amplitude::{AtomicAmplitude, MasterMeter, MeterReading};
pub use load::{LOAD_MEAN_TIME_CONSTANT, LoadMeter, LoadReading};
pub use rt::{MeteringContext, meter_output};
pub use tap::{AudioTap, TapBusy, TapCons};
