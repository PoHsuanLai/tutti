//! The engine's allocation gates and its load meter. The binary installs
//! `AllocDisabler`, so an allocation inside an `assert_no_alloc` closure aborts.

use assert_no_alloc::AllocDisabler;

#[global_allocator]
static ALLOC: AllocDisabler = AllocDisabler;

#[path = "../support/mod.rs"]
mod support;

mod load_meter;
mod rt_no_alloc_engine;
