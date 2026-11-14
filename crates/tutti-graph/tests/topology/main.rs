//! The topology value, the compiler and the editor: building, validating, compiling, replacing and forking.
//!
//! The binary installs `AllocDisabler`, so an allocation inside an `assert_no_alloc`
//! closure aborts.

use assert_no_alloc::AllocDisabler;

#[global_allocator]
static ALLOC: AllocDisabler = AllocDisabler;

mod builder;
mod bus_edges;
mod bus_no_alloc;
mod buses;
mod compile_passes;
mod cost_model;
mod fork;
mod graph_value;
mod limits;
mod replace;
mod reprepare;
mod revision;
mod set_latency;
mod set_tail;
