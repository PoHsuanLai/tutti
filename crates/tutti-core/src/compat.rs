//! Compatibility layer for no_std + alloc.

pub use parking_lot::{Mutex, RwLock};

pub use alloc::{
    boxed::Box,
    collections::VecDeque,
    string::{String, ToString},
    sync::Arc,
    vec,
    vec::Vec,
};

pub use core::{
    any,
    cell::UnsafeCell,
    sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering},
};

pub use hashbrown::HashMap;
