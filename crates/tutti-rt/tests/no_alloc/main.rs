//! The runtime primitives' allocation gates. The binary installs `AllocDisabler`,
//! so an allocation inside an `assert_no_alloc` closure aborts.

use assert_no_alloc::AllocDisabler;

#[global_allocator]
static ALLOC: AllocDisabler = AllocDisabler;

mod rt_no_alloc;
