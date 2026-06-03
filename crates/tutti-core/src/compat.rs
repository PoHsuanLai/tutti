//! Shared re-exports for the core data structures.
//!
//! Historically a no_std + alloc polyfill; tutti-core is now a std crate (it
//! carries hard Bevy deps for the ECS hub), so these resolve through `std`.
//! The *choices* are deliberate and kept: `parking_lot` locks (not `std`) and
//! `hashbrown` maps (specific hasher) match the RT/DSP code's expectations —
//! do not swap these for `std`/Bevy equivalents without auditing lock and
//! hash behavior.

pub use parking_lot::{Mutex, RwLock};

pub use std::{
    any,
    boxed::Box,
    cell::UnsafeCell,
    collections::VecDeque,
    string::{String, ToString},
    sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering},
    sync::Arc,
    vec,
    vec::Vec,
};

pub use hashbrown::HashMap;
