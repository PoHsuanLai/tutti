//! Guides: how to use the cell, what it does not do, what it costs, and how
//! it works.
//!
//! Nothing in here is code; each module is a page.
//!
//! - [`patterns`]: the shapes it is meant for: registering readers, keeping
//!   a reader's iteration short, nesting, reclaiming.
//! - [`limitations`]: where memory can grow, what the reader cannot do, and
//!   where a free can still happen.
//! - [`performance`]: what each operation does, and what it measured
//!   against the alternatives.
//! - [`internal`]: the protocol, step by step, and the memory-ordering
//!   argument that it is sound.

pub mod internal;
pub mod limitations;
pub mod patterns;
pub mod performance;
