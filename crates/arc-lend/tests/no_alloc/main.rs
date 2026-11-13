//! The reader side's allocation gate. The binary installs `AllocDisabler`, so an
//! allocation inside an `assert_no_alloc` closure aborts.

// Under `--cfg loom` the cell's atomics are loom's, which panic outside a
// `loom::model`.
#![cfg(not(loom))]

use assert_no_alloc::AllocDisabler;

#[global_allocator]
static ALLOC: AllocDisabler = AllocDisabler;

mod no_alloc;
