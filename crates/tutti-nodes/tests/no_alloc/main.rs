//! The nodes' allocation gates: each node's audio-thread path, alone and through
//! an engine. The binary installs `AllocDisabler`, so an allocation inside an
//! `assert_no_alloc` closure aborts.

use assert_no_alloc::AllocDisabler;

#[global_allocator]
static ALLOC: AllocDisabler = AllocDisabler;

mod rt_no_alloc;
mod rt_no_alloc_crossover;
mod rt_no_alloc_dynamics;
mod rt_no_alloc_engine;
mod rt_no_alloc_filter;
mod rt_no_alloc_modulation;
mod spatial_rt_no_alloc;
