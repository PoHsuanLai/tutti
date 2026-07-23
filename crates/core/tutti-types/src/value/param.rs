//! `Param<U>` — a modulatable audio-thread parameter typed by its unit.
//!
//! Wraps `Arc<AtomicF32>` with a `PhantomData<U>` marker so different unit
//! aliases are distinct types at compile time. Zero runtime cost versus the
//! raw `Arc<AtomicF32>` pattern it replaces.
//!
//! Only meaningful for units whose raw representation is `f32` (see the
//! `Unit<Raw = f32>` bound). `SampleRate` is `f64`-backed and is not
//! modulated at audio rate — construct it directly, don't wrap in `Param`.
//!
//! RT-safety: `load`/`store` never clone the `Arc`. Only `handle()` does, and
//! it is intended for control-thread construction, not the audio path.

use core::marker::PhantomData;
use core::sync::atomic::Ordering;

use super::units::Unit;
use atomic_float::AtomicF32;
use std::sync::Arc;

pub struct Param<U: Unit<Raw = f32>> {
    inner: Arc<AtomicF32>,
    _unit: PhantomData<U>,
}

impl<U: Unit<Raw = f32>> Param<U> {
    #[inline]
    pub fn new(v: U) -> Self {
        Self {
            inner: Arc::new(AtomicF32::new(v.to_raw())),
            _unit: PhantomData,
        }
    }

    /// Load the current value with `Acquire` ordering (pairs with `store`'s
    /// `Release` — matches the pattern every call site here already used).
    #[inline]
    pub fn load(&self) -> U {
        U::from_raw(self.inner.load(Ordering::Acquire))
    }

    /// Store a new value with `Release` ordering.
    #[inline]
    pub fn store(&self, v: U) {
        self.inner.store(v.to_raw(), Ordering::Release);
    }

    /// Clone the shared atomic for control-thread automation. Named so
    /// hot-path code avoids it.
    #[inline]
    pub fn handle(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
            _unit: PhantomData,
        }
    }

    /// Expose the raw atomic for compatibility with existing external APIs
    /// that accept `Arc<AtomicF32>`. This clones the `Arc`; do not call on
    /// the audio thread.
    #[inline]
    pub fn as_atomic(&self) -> Arc<AtomicF32> {
        Arc::clone(&self.inner)
    }
}

impl<U: Unit<Raw = f32>> Clone for Param<U> {
    #[inline]
    fn clone(&self) -> Self {
        self.handle()
    }
}

impl<U: Unit<Raw = f32> + Default> Default for Param<U> {
    #[inline]
    fn default() -> Self {
        Self::new(U::default())
    }
}

impl<U: Unit<Raw = f32>> core::fmt::Debug for Param<U> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Param")
            .field("value", &self.load())
            .finish()
    }
}
