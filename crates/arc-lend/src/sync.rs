//! Atomics, fences and the mutex: `std`'s in a normal build, `loom`'s under
//! `--cfg loom`, so the model in `tests/loom.rs` checks *this* code rather
//! than a replica of it.

#[cfg(loom)]
pub(crate) use loom::{
    sync::atomic::{fence, AtomicBool, AtomicPtr, AtomicUsize, Ordering},
    sync::{Mutex, MutexGuard},
};
#[cfg(not(loom))]
pub(crate) use std::{
    sync::atomic::{fence, AtomicBool, AtomicPtr, AtomicUsize, Ordering},
    sync::{Mutex, MutexGuard},
};
