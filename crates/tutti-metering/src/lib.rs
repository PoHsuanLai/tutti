//! Load monitoring: how much of each block's time budget the render used.
//!
//! [`LoadMeter`] reports the render time against the budget (the audio thread
//! holds its [`LoadRecorder`]); `tutti-engine`'s `Engine` feeds it. Level
//! metering is not here: it is an op in the compiled plan
//! (`tutti_graph::MeterReader`, declared with `Editor::open_meter`), so it
//! meters any port of the graph, offline too.

#![forbid(unsafe_code)]
#![cfg_attr(docsrs, feature(doc_cfg))]

mod load;

pub use load::{LOAD_MEAN_TIME_CONSTANT, LoadMeter, LoadReading, LoadRecorder};
