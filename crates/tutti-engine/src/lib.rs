#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]
#![cfg_attr(docsrs, feature(doc_cfg))]

mod error;
pub use error::{Error, Result};

mod engine;
mod hearing;
mod output;
pub use engine::{DEFAULT_BLOCK_CAPACITY, Engine, GraphEngineError};
pub use hearing::{HeardTapError, HeardTaps};

/// What a host driving the engine names, in one import.
///
/// `Result` is left out: it would shadow a name a consumer already has.
pub mod prelude {
    pub use crate::{Engine, Error};
}
