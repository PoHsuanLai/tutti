#![doc = include_str!("../README.md")]
#![cfg(target_os = "macos")]
#![cfg_attr(docsrs, feature(doc_cfg))]
// Every `unsafe` block says why it is sound, and every `unsafe fn` what it
// asks of its caller.
#![deny(clippy::undocumented_unsafe_blocks, clippy::missing_safety_doc)]
#![warn(unsafe_op_in_unsafe_fn)]

mod host_clock;
mod source;

pub use host_clock::{Timebase, host_ticks};
pub use source::{Error, UmpSource};
