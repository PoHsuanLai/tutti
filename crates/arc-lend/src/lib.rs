#![doc = include_str!("../README.md")]
#![deny(clippy::undocumented_unsafe_blocks, clippy::missing_safety_doc)]

mod cell;
mod current;
mod reader;
mod slots;
mod sync;

pub mod docs;

pub use cell::ArcLend;
pub use reader::{Loan, Reader};

// Under `--cfg loom` the atomics are loom's, which panic outside a
// `loom::model`, so these ordinary tests are compiled out; the loom model is
// `tests/loom.rs`.
#[cfg(all(test, not(loom)))]
mod tests;
