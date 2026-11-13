#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]
#![cfg_attr(docsrs, feature(doc_cfg))]

mod error;
pub use error::{Error, Result};

mod block_clock;
mod engine;
mod hearing;
mod output;
pub use block_clock::{BlockClockReader, BlockReading, Playhead};
pub use engine::{DEFAULT_BLOCK_CAPACITY, Engine, GraphEngineError};
pub use hearing::{HeardOpen, HeardTapError, HeardTaps};
pub use output::OutputControl;
