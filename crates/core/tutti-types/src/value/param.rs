//! `Param<U>` — a modulatable audio-thread parameter typed by its unit.
//!
//! Wraps `Arc<AtomicF32>` with a `PhantomData<U>` marker so different unit
//! aliases are distinct types at compile time — zero runtime cost over a bare
//! `Arc<AtomicF32>`.
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

/// A shared, lock-free scalar parameter carrying its unit in the type.
///
/// Cloning shares the cell rather than copying the value, so a control thread
/// and the audio thread hold the same `Param` and neither allocates to read or
/// write it. See the [module docs](self) for the `Unit<Raw = f32>` restriction.
pub struct Param<U: Unit<Raw = f32>> {
    inner: Arc<AtomicF32>,
    _unit: PhantomData<U>,
}

impl<U: Unit<Raw = f32>> Param<U> {
    /// Allocates a new cell holding `v`. Control-thread only — this is the one
    /// constructor that allocates.
    #[inline]
    pub fn new(v: U) -> Self {
        Self {
            inner: Arc::new(AtomicF32::new(v.to_raw())),
            _unit: PhantomData,
        }
    }

    /// Load the current value with `Acquire` ordering, pairing with `store`'s
    /// `Release`. Audio-thread safe: no clone, no allocation.
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

    /// Stop sharing the cell: replace it with a fresh one holding the
    /// *current* value. Every other holder of the old cell (the live node, a
    /// UI handle, a mod target) keeps it; this copy no longer sees their
    /// writes, and they no longer see this copy's.
    ///
    /// This is what `AudioUnit::isolate` calls on each control a node reads,
    /// so an offline fork renders a snapshot of the controls taken at fork
    /// time rather than following live knob moves while it runs. Keeping the
    /// value is the point — a fork that snapped to a default would render
    /// something nobody heard. Control-thread only: it allocates, like
    /// [`Param::new`].
    #[inline]
    pub fn detach(&mut self) {
        *self = Self::new(self.load());
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

#[cfg(test)]
mod tests {
    use super::Param;
    use crate::value::units::Hz;

    /// `detach` keeps the value and cuts the cell both ways.
    ///
    /// Mutation: make `detach` a no-op → the live write reaches the detached
    /// copy and the second `assert_eq!` fails; make it
    /// `Self::new(U::default())` → the value is lost and the first fails.
    #[test]
    fn detach_keeps_the_value_and_severs_the_cell() {
        let live = Param::new(Hz(440.0));
        let mut copy = live.handle();
        copy.detach();
        assert_eq!(copy.load(), Hz(440.0), "detach keeps the current value");

        live.store(Hz(880.0));
        assert_eq!(copy.load(), Hz(440.0), "a live write must not reach it");
        copy.store(Hz(110.0));
        assert_eq!(
            live.load(),
            Hz(880.0),
            "its write must not reach the live cell"
        );
    }
}
