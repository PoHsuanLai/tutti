//! Longer documentation, one page per module.
//!
//! - [`guide`]: how to use the cell well, and what to watch for.
//! - [`design`]: how it works, why we believe it is safe, and what each
//!   operation costs.
//!
//! Measurements against other cells are in
//! [BENCHMARKS.md](https://docs.rs/crate/arc-lend/latest/source/BENCHMARKS.md).

#[doc = include_str!("design.md")]
pub mod design {}
pub mod guide;
