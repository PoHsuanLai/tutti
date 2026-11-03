#![doc = include_str!("../README.md")]

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
