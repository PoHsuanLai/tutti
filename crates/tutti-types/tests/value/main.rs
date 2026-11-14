//! The value types' range contracts and the fold's allocation gate. The binary
//! installs `AllocDisabler`, so an allocation inside an `assert_no_alloc`
//! closure aborts.

use assert_no_alloc::AllocDisabler;

#[global_allocator]
static ALLOC: AllocDisabler = AllocDisabler;

mod beats_per_cycle_range;
mod bpm_range;
mod fold_no_alloc;
mod sample_rate_range;
