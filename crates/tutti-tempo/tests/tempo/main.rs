//! The tempo map's analytic figures, its cell grid and its range contract, and
//! the audio-thread calls' allocation gate. The binary installs `AllocDisabler`,
//! so an allocation inside an `assert_no_alloc` closure aborts.

use assert_no_alloc::AllocDisabler;

#[global_allocator]
static ALLOC: AllocDisabler = AllocDisabler;

#[path = "../common/mod.rs"]
mod common;

mod analytic;
mod cells;
mod no_alloc;
mod range;
