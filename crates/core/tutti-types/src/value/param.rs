//! [`Param<U>`](Param), a lock-free parameter cell typed by its unit.
//!
//! Wraps `Arc<AtomicF32>` with a `PhantomData<U>` marker so different units
//! are distinct types at compile time, at zero runtime cost over a bare
//! `Arc<AtomicF32>`.
//!
//! Only meaningful for units whose raw representation is `f32` (see the
//! `Unit<Raw = f32>` bound). `SampleRate` is `f64`-backed and is not
//! modulated at audio rate — construct it directly, don't wrap in `Param`.
//!
//! RT-safety: `load`/`store` never clone the `Arc`. Only `handle()` does, and
//! it is intended for control-thread construction, not the audio path.
//!
//! [`Param64<U>`](Param64) is its `f64` neighbour: the same `load`/`store`,
//! for the units whose raw representation is `f64`, held inline rather than
//! shared through an `Arc`.

use core::marker::PhantomData;
use core::sync::atomic::{AtomicU64, Ordering};

use super::units::Unit;
use atomic_float::AtomicF32;
use std::sync::Arc;

/// A shared, lock-free scalar parameter carrying its unit in the type.
///
/// Cloning shares the cell rather than copying the value, so a control thread
/// and the audio thread hold the same `Param` and neither allocates to read or
/// write it. [`load`](Self::load) and [`store`](Self::store) are safe on the
/// audio thread; [`new`](Self::new), [`handle`](Self::handle) and
/// [`detach`](Self::detach) are for the control thread.
///
/// Only units whose raw representation is `f32` fit (`Unit<Raw = f32>`).
/// [`SampleRate`](super::SampleRate) is `f64`-backed and is not modulated at
/// audio rate, so it is used directly rather than through a `Param`.
///
/// # Examples
///
/// ```
/// use tutti_types::{Hz, Param};
///
/// let cutoff = Param::new(Hz(1_000.0)); // control thread: allocates
/// let on_audio_thread = cutoff.handle(); // shares the cell
///
/// cutoff.store(Hz(2_000.0));
/// assert_eq!(on_audio_thread.load(), Hz(2_000.0));
/// ```
pub struct Param<U: Unit<Raw = f32>> {
    inner: Arc<AtomicF32>,
    _unit: PhantomData<U>,
}

impl<U: Unit<Raw = f32>> Param<U> {
    /// Allocates a new cell holding `v`.
    ///
    /// Control-thread only: this is the one constructor that allocates.
    #[inline]
    pub fn new(v: U) -> Self {
        Self {
            inner: Arc::new(AtomicF32::new(v.to_raw())),
            _unit: PhantomData,
        }
    }

    /// Loads the current value with `Acquire` ordering, pairing with `store`'s
    /// `Release`.
    ///
    /// Audio-thread safe: no clone, no allocation.
    #[inline]
    pub fn load(&self) -> U {
        U::from_raw(self.inner.load(Ordering::Acquire))
    }

    /// Stores a new value with `Release` ordering.
    ///
    /// Audio-thread safe; every holder of the cell sees it.
    #[inline]
    pub fn store(&self, v: U) {
        self.inner.store(v.to_raw(), Ordering::Release);
    }

    /// Returns another handle on the same cell, for control-thread automation.
    ///
    /// Clones the `Arc`, so keep it off the audio path. Same as `clone`, but
    /// named so hot-path code visibly avoids it.
    #[inline]
    pub fn handle(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
            _unit: PhantomData,
        }
    }

    /// Stops sharing the cell, replacing it with a fresh one that holds the
    /// *current* value.
    ///
    /// Every other holder of the old cell (the live node, a UI handle, a mod
    /// target) keeps it; this copy no longer sees their writes, and they no
    /// longer see this copy's.
    ///
    /// A node's fork for an offline render (`tutti-graph`'s
    /// `ParamNode::fork_fresh`) calls this on each control the node reads, so
    /// the fork renders a snapshot of the controls taken at fork time rather
    /// than following live knob moves while it runs. Keeping the value is the
    /// point: a fork that snapped to a default would render something nobody
    /// heard. Control-thread only: it allocates, like [`Param::new`].
    #[inline]
    pub fn detach(&mut self) {
        *self = Self::new(self.load());
    }

    /// Returns the shared raw atomic, for APIs that take an `Arc<AtomicF32>`
    /// and address parameters without their unit types.
    ///
    /// Clones the `Arc`; do not call on the audio thread.
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

/// A lock-free scalar cell for a unit whose raw representation is `f64`
/// (`Unit<Raw = f64>`): [`Param`]'s neighbour, not a mode of it.
///
/// The width is the reason it exists. [`ReadRate`](super::ReadRate) and
/// [`SamplePosition`](super::SamplePosition) are `f64` because a disk voice adds
/// the rate into its read position once per sample for the length of the file.
/// A cell that stored the rate as `f32` and widened it on load would put that
/// rounding back into the sum on every load, and the error does not average
/// out: it integrates. [`Param`] is `Unit<Raw = f32>` only, so a unit that
/// needs the width needs this cell.
///
/// [`SampleRate`](super::SampleRate) is also `f64` and does not get a cell:
/// nothing modulates it per sample.
///
/// Unlike [`Param`], nothing is shared by cloning: the cell is an inline
/// [`AtomicU64`] holding the value's bits, construction does not allocate, and
/// another thread reaches it through `&self` on whatever already shares its
/// owner (a voice's `RtState`, a memory source). One type for both widths would
/// hide two implementations behind one name, and a call site would no longer
/// see which load it pays for. There is no `Default`: a default
/// [`ReadRate`](super::ReadRate) is `0.0`, a stopped voice, so the initial
/// value is always named.
///
/// [`load`](Self::load) and [`store`](Self::store) are audio-thread safe, with
/// `Param`'s `Acquire`/`Release` pairing. The bits round-trip exactly, a `NaN`
/// payload included.
///
/// # Examples
///
/// ```
/// use tutti_types::{Param64, ReadRate};
///
/// let stretch_rate = Param64::new(ReadRate::UNITY); // no allocation
/// stretch_rate.store(ReadRate::new(0.75));
/// assert_eq!(stretch_rate.load(), ReadRate::new(0.75));
/// ```
///
/// An `f64` unit is not a [`Param`]:
///
/// ```compile_fail
/// use tutti_types::{Param, ReadRate};
/// struct Voice {
///     stretch_rate: Param<ReadRate>, // `Param64<ReadRate>`
/// }
/// ```
///
/// Nor is an `f32` unit a `Param64`:
///
/// ```compile_fail
/// use tutti_types::{Hz, Param64};
/// struct Filter {
///     cutoff: Param64<Hz>, // `Param<Hz>`
/// }
/// ```
// Mutations (run) for the two `compile_fail` doctests: `Raw = f32` dropped from
// `Param`'s struct bound (its impls keep it) → the first compiles → fails;
// `Raw = f64` dropped from `Param64`'s struct bound → the second compiles →
// fails.
pub struct Param64<U: Unit<Raw = f64>> {
    inner: AtomicU64,
    _unit: PhantomData<U>,
}

impl<U: Unit<Raw = f64>> Param64<U> {
    /// A cell holding `v`. Does not allocate.
    #[inline]
    pub fn new(v: U) -> Self {
        Self {
            inner: AtomicU64::new(v.to_raw().to_bits()),
            _unit: PhantomData,
        }
    }

    /// Loads the current value with `Acquire` ordering, pairing with `store`'s
    /// `Release`.
    ///
    /// Audio-thread safe.
    #[inline]
    pub fn load(&self) -> U {
        U::from_raw(f64::from_bits(self.inner.load(Ordering::Acquire)))
    }

    /// Stores a new value with `Release` ordering.
    ///
    /// Audio-thread safe.
    #[inline]
    pub fn store(&self, v: U) {
        self.inner.store(v.to_raw().to_bits(), Ordering::Release);
    }
}

impl<U: Unit<Raw = f64>> core::fmt::Debug for Param64<U> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Param64")
            .field("value", &self.load())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::{Param, Param64};
    use crate::value::units::{Hz, ReadRate, SamplePosition};

    /// Both `f64` units round-trip through the cell bit for bit: a value `f32`
    /// cannot hold, an hour's position at 48 kHz plus a fraction, and a `NaN`
    /// whose payload bits survive `to_bits`/`from_bits`.
    ///
    /// Mutation (run): `store` through `v.to_raw() as f32 as f64` → the
    /// precision and position cases lose their low bits and the payload `NaN`
    /// comes back with its payload truncated → fails.
    #[test]
    fn param64_round_trips_its_bits() {
        let nan = f64::from_bits(0x7ff8_0000_dead_beef);
        assert!(nan.is_nan());

        let rates = [1.0 + f64::EPSILON, 0.1, 17.4, nan];
        let cell = Param64::new(ReadRate::UNITY);
        for want in rates {
            cell.store(ReadRate::new(want));
            assert_eq!(cell.load().get().to_bits(), want.to_bits(), "{want}");
        }
        assert_eq!(
            Param64::new(ReadRate::new(nan)).load().get().to_bits(),
            nan.to_bits()
        );

        let positions = [172_800_000.0 + 0.25, 1.0 / 3.0, -0.0, nan];
        let cell = Param64::new(SamplePosition::new(0.0));
        for want in positions {
            cell.store(SamplePosition::new(want));
            assert_eq!(cell.load().get().to_bits(), want.to_bits(), "{want}");
        }
    }

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
