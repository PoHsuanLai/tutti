//! The executor, the worker pool, the sample-accuracy contract and the reader ends of taps and inlets.
//!
//! The binary installs `AllocDisabler`, so an allocation inside an `assert_no_alloc`
//! closure aborts.

use assert_no_alloc::AllocDisabler;

#[global_allocator]
static ALLOC: AllocDisabler = AllocDisabler;

mod by_hand;
mod chase;
mod contract;
mod executor;
mod inlet_no_alloc;
mod inlet_retire;
mod meter;
mod no_denormals;
mod parallel;
mod pool_spawn_refusal;
mod rt_no_alloc;
mod tap;
mod tap_audio_in;
mod wake;
