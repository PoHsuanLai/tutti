#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]
#![cfg_attr(docsrs, feature(doc_cfg))]

mod error;
pub use error::{Error, Result};

mod engine;
pub use engine::{DEFAULT_BLOCK_CAPACITY, Engine, GraphEngineError};
