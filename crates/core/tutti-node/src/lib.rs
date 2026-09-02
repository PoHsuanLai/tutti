//! The node contract: what it means to be a node in the Tutti audio graph.
//!
//! This crate owns [`AudioUnit`], the planar block buffers it processes into,
//! the numeric tower those are generic over, the [`Signal`] vocabulary
//! [`AudioUnit::route`] speaks, and the [`Setting`](setting::Setting) its `set`
//! takes. It sits **below** `fundsp-tutti`, so a crate can implement a node
//! without depending on the fork.
//!
//! # Why this is a crate and not a module of the fork
//!
//! `fundsp-tutti` is a vendored fork. Owning the contract there means the
//! engine's central abstraction lives in code we treat as third-party and do
//! not restyle, and every crate that implements a node reaches it through a
//! glob re-export wall rather than by naming what it depends on. Lifting the
//! trait *up* into `tutti-core` is a cycle (`tutti-core → fundsp-tutti`), and
//! lifting it into `tutti-types` drags this numeric tower into the vocabulary
//! crate. Putting it *below* the fork is the one direction that is neither.
//!
//! [`Sample`] stays a type parameter with `F32` as its default: the plugin
//! hosts implement `AudioUnit<F64>`, so specializing the trait to `f32` to shed
//! the tower is not available.
//!
//! # What it depends on
//!
//! Numeric crates, and `tutti-types` for exactly two types — [`SampleRate`] and
//! [`Tail`], which the trait's own signatures name. That single edge is the one
//! place this crate is not self-contained; [`value`] states in full why neither
//! could move down with the rest, and what the exception costs. `tutti-types`
//! names no other `tutti` crate, so the dependency graph stays acyclic and
//! `fundsp-tutti` sits on top of both.

#![allow(
    clippy::precedence,
    clippy::type_complexity,
    clippy::float_cmp,
    clippy::len_zero,
    clippy::needless_range_loop,
    clippy::manual_range_contains,
    clippy::too_many_arguments,
    clippy::comparison_chain,
    clippy::unnecessary_cast
)]

extern crate alloc;

pub mod audiounit;
pub mod buffer;
pub mod math;
pub mod num;
pub mod setting;
pub mod signal;
pub mod value;

pub use audiounit::AudioUnit;
pub use math::AttoHash;
pub use setting::{Address, NodeAddr, Parameter, Setting};
pub use signal::{Routing, Signal, SignalFrame};
pub use value::{SampleRate, Samples, Tail};

// Re-exported at the root, because the fork's `lib.rs` defined them there and
// `use fundsp_tutti::*` (which every prelude does) put them in scope
// unqualified. Keeping the root spelling is what makes the relocation a no-op
// for the ~200 sites that name `Float`, `Frame` or `MAX_BUFFER_SIZE`.
pub use num::{
    convert, full_simd_items, full_simd_items_s, simd_items, simd_items_s, Float, Frame, Int, Num,
    Real, Sample, Size, DEFAULT_SR, F32, F32x, F64, F64x, I32x, I64x, MAX_BUFFER_LOG,
    MAX_BUFFER_SIZE, SIMD_C, SIMD_LEN, SIMD_M, SIMD_N, SIMD_S, U32x,
};

// The type-level integers arities are written with (`U1`, `U2`, …) plus the
// `Frame` sequence traits. Re-exported for the same reason the tower is: the
// fork exposed them at its root, and every arity in the engine is spelled
// unqualified.
pub use numeric_array::{
    self,
    generic_array::sequence::{Concat, GenericSequence},
    typenum,
};
pub use wide;
