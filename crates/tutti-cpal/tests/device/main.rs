//! The device layer without a sound card: the output callback, the driver
//! lifecycle and the callback's allocation gate. The binary installs
//! `AllocDisabler`, so an allocation inside an `assert_no_alloc` closure aborts.

use assert_no_alloc::AllocDisabler;

#[global_allocator]
static ALLOC: AllocDisabler = AllocDisabler;

mod support;

mod driver_lifecycle;
mod output_block;
mod rt_no_alloc;
