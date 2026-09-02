//! The node contract: what it means to be a node in the Tutti audio graph.
//!
//! This crate owns [`AudioUnit`], the planar block buffers it processes into,
//! the numeric tower those are generic over, and the [`Signal`] vocabulary
//! [`AudioUnit::route`] speaks. It is the **floor** of the engine — it names no
//! other `tutti` crate, so everything above it (`fundsp-tutti` included) can
//! depend on it without a cycle.
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

#![cfg_attr(not(feature = "std"), no_std)]
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
