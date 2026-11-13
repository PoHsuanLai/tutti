//! The live border: device inlets, the ingest, tap and clock nodes, and their cost classes.

/// Inert outside `assert_no_alloc`, so every test here may also gate a
/// closure on allocating nothing.
#[global_allocator]
static A: assert_no_alloc::AllocDisabler = assert_no_alloc::AllocDisabler;

#[path = "../common/mod.rs"]
mod common;

mod border_no_alloc;
mod cost_classes;
mod device_inlets;
mod edge_nodes;
