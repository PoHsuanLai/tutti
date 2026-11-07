//! Level monitoring: what the audio callback measures and what other threads
//! read.
//!
//! [`MasterMeter`] holds the output's peak and RMS (the audio thread holds its
//! [`MeterWriter`]), and [`LoadMeter`] reports how much of each block's time
//! budget the render used (the audio thread holds its [`LoadRecorder`]).
//! `tutti-engine`'s `Engine` feeds both.

#![forbid(unsafe_code)]
#![cfg_attr(docsrs, feature(doc_cfg))]

mod amplitude;
mod load;

pub use amplitude::{MasterMeter, MeterReading, MeterWriter};
pub use load::{LOAD_MEAN_TIME_CONSTANT, LoadMeter, LoadReading, LoadRecorder};
