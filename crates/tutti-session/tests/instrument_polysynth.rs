//! The instrument contract (`instrument_support`) against Poly.

#[macro_use]
mod instrument_support;

#[global_allocator]
static ALLOC: assert_no_alloc::AllocDisabler = assert_no_alloc::AllocDisabler;

contract!(instrument_support::subjects::Poly, gaps = 0);
