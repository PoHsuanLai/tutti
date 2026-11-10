//! [`Param<U>`](Param), a lock-free parameter cell typed by its unit.
//!
//! Wraps an `Arc` of an atomic float with a `PhantomData<U>` marker so
//! different units are distinct types at compile time, at zero runtime cost
//! over a bare `Arc<AtomicF32>`. The atomic follows the unit's raw
//! representation ([`ParamRaw`]): [`AtomicF32`] for an `f32` unit,
//! [`AtomicF64`] for an `f64` one ([`Seconds`](super::Seconds)). `SampleRate`
//! is `f64`-backed and is not modulated at audio rate: construct it directly,
//! don't wrap it in `Param`.
//!
//! RT-safety: `load`/`store` never clone the `Arc`. Only `handle()` does, and
//! it is intended for control-thread construction, not the audio path.
//!
//! [`ParamCell`] is a cell with its width erased: the value a host that
//! addresses params without their unit types (a modulation lane's knob,
//! `tutti_graph::ParamSet`) reads and writes as `f32`.
//!
//! [`Param64<U>`](Param64) is a different thing: an inline, unshared `f64`
//! cell, for a hot loop's own state.

use core::marker::PhantomData;
use core::sync::atomic::{AtomicU64, Ordering};

use super::units::Unit;
use atomic_float::{AtomicF32, AtomicF64};
use std::sync::Arc;

mod sealed {
    pub trait Sealed {}
    impl Sealed for f32 {}
    impl Sealed for f64 {}
}

/// A raw float a [`Param`] cell can hold: `f32` (an [`AtomicF32`]) or `f64`
/// (an [`AtomicF64`]). Sealed: the two widths are the whole set.
pub trait ParamRaw: sealed::Sealed + Copy {
    /// The atomic that holds this width.
    type Atomic: Send + Sync + 'static;
    /// A fresh atomic holding `self`.
    fn new_atomic(self) -> Self::Atomic;
    /// Loads from `cell`.
    fn load(cell: &Self::Atomic, order: Ordering) -> Self;
    /// Stores `self` into `cell`.
    fn store(self, cell: &Self::Atomic, order: Ordering);
    /// This raw as the `f32` the modulation path carries (narrowed, for `f64`).
    fn to_f32(self) -> f32;
    /// A raw from that `f32` (widened exactly, for `f64`).
    fn from_f32(v: f32) -> Self;
}

impl ParamRaw for f32 {
    type Atomic = AtomicF32;
    #[inline]
    fn new_atomic(self) -> AtomicF32 {
        AtomicF32::new(self)
    }
    #[inline]
    fn load(cell: &AtomicF32, order: Ordering) -> f32 {
        cell.load(order)
    }
    #[inline]
    fn store(self, cell: &AtomicF32, order: Ordering) {
        cell.store(self, order);
    }
    #[inline]
    fn to_f32(self) -> f32 {
        self
    }
    #[inline]
    fn from_f32(v: f32) -> f32 {
        v
    }
}

impl ParamRaw for f64 {
    type Atomic = AtomicF64;
    #[inline]
    fn new_atomic(self) -> AtomicF64 {
        AtomicF64::new(self)
    }
    #[inline]
    fn load(cell: &AtomicF64, order: Ordering) -> f64 {
        cell.load(order)
    }
    #[inline]
    fn store(self, cell: &AtomicF64, order: Ordering) {
        cell.store(self, order);
    }
    #[inline]
    fn to_f32(self) -> f32 {
        self as f32
    }
    #[inline]
    fn from_f32(v: f32) -> f64 {
        f64::from(v)
    }
}

/// A [`Param`]'s cell with its width erased, read and written as `f32`.
///
/// What a host that addresses params by kind rather than by unit holds: the
/// modulation path (control lanes, `ParamSet`) works in `f32` values, whatever
/// the unit's own raw. An `f64` cell is **widened** on a write (exact) and
/// **narrowed** on a read (`as f32`): the one place a [`Seconds`](super::Seconds)
/// param loses precision, and a control time an `f32` carries to well under a
/// microsecond for any value a modulation lane produces. The node that owns
/// the [`Param`] reads its own cell at full width.
#[derive(Clone, Debug)]
pub enum ParamCell {
    /// An `f32` unit's cell.
    F32(Arc<AtomicF32>),
    /// An `f64` unit's cell.
    F64(Arc<AtomicF64>),
}

impl ParamCell {
    /// Loads the value as `f32`.
    #[inline]
    pub fn load(&self, order: Ordering) -> f32 {
        match self {
            Self::F32(c) => c.load(order),
            Self::F64(c) => c.load(order).to_f32(),
        }
    }

    /// Stores an `f32` value (widened, for an `f64` cell).
    #[inline]
    pub fn store(&self, value: f32, order: Ordering) {
        match self {
            Self::F32(c) => c.store(value, order),
            Self::F64(c) => c.store(f64::from(value), order),
        }
    }

    /// Whether both are the same cell.
    #[must_use]
    pub fn ptr_eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::F32(a), Self::F32(b)) => Arc::ptr_eq(a, b),
            (Self::F64(a), Self::F64(b)) => Arc::ptr_eq(a, b),
            _ => false,
        }
    }
}

impl From<Arc<AtomicF32>> for ParamCell {
    fn from(c: Arc<AtomicF32>) -> Self {
        Self::F32(c)
    }
}

impl From<Arc<AtomicF64>> for ParamCell {
    fn from(c: Arc<AtomicF64>) -> Self {
        Self::F64(c)
    }
}

/// A shared, lock-free scalar parameter carrying its unit in the type.
///
/// Cloning shares the cell rather than copying the value, so a control thread
/// and the audio thread hold the same `Param` and neither allocates to read or
/// write it. [`load`](Self::load) and [`store`](Self::store) are safe on the
/// audio thread; [`new`](Self::new), [`handle`](Self::handle) and
/// [`detach`](Self::detach) are for the control thread.
///
/// The cell holds the unit's raw at its own width ([`ParamRaw`]): `f32` for
/// most units, `f64` for [`Seconds`](super::Seconds).
/// [`SampleRate`](super::SampleRate) is `f64`-backed too but is not modulated
/// at audio rate, so it is used directly rather than through a `Param`.
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
pub struct Param<U: Unit>
where
    U::Raw: ParamRaw,
{
    inner: Arc<<U::Raw as ParamRaw>::Atomic>,
    _unit: PhantomData<U>,
}

impl<U: Unit> Param<U>
where
    U::Raw: ParamRaw,
{
    /// Allocates a new cell holding `v`.
    ///
    /// Control-thread only: this is the one constructor that allocates.
    #[inline]
    pub fn new(v: U) -> Self {
        Self {
            inner: Arc::new(v.to_raw().new_atomic()),
            _unit: PhantomData,
        }
    }

    /// Loads the current value with `Acquire` ordering, pairing with `store`'s
    /// `Release`.
    ///
    /// Audio-thread safe: no clone, no allocation.
    #[inline]
    pub fn load(&self) -> U {
        U::from_raw(<U::Raw as ParamRaw>::load(&self.inner, Ordering::Acquire))
    }

    /// Stores a new value with `Release` ordering.
    ///
    /// Audio-thread safe; every holder of the cell sees it.
    #[inline]
    pub fn store(&self, v: U) {
        v.to_raw().store(&self.inner, Ordering::Release);
    }

    /// Returns another handle on the same cell, for control-thread automation.
    ///
    /// Clones the `Arc`, so keep it off the audio path. Same as `clone`, but
    /// named so hot-path code visibly avoids it.
    #[inline]
    #[must_use]
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
    /// A node's fork for an offline render calls this on each control the node
    /// reads, so the fork renders a snapshot of the controls taken at fork time rather
    /// than following live knob moves while it runs. Keeping the value is the
    /// point: a fork that snapped to a default would render something nobody
    /// heard. Control-thread only: it allocates, like [`Param::new`].
    #[inline]
    pub fn detach(&mut self) {
        *self = Self::new(self.load());
    }

    /// Returns the shared raw atomic (an `Arc<AtomicF32>` for an `f32` unit,
    /// an `Arc<AtomicF64>` for an `f64` one), for APIs that address
    /// parameters without their unit types.
    ///
    /// Clones the `Arc`; do not call on the audio thread.
    #[inline]
    #[must_use]
    pub fn as_atomic(&self) -> Arc<<U::Raw as ParamRaw>::Atomic> {
        Arc::clone(&self.inner)
    }

    /// The shared cell as a width-erased [`ParamCell`].
    ///
    /// Clones the `Arc`; do not call on the audio thread.
    #[inline]
    #[must_use]
    pub fn cell(&self) -> ParamCell
    where
        Arc<<U::Raw as ParamRaw>::Atomic>: Into<ParamCell>,
    {
        self.as_atomic().into()
    }
}

impl<U: Unit> Clone for Param<U>
where
    U::Raw: ParamRaw,
{
    #[inline]
    fn clone(&self) -> Self {
        self.handle()
    }
}

impl<U: Unit + Default> Default for Param<U>
where
    U::Raw: ParamRaw,
{
    #[inline]
    fn default() -> Self {
        Self::new(U::default())
    }
}

impl<U: Unit> core::fmt::Debug for Param<U>
where
    U::Raw: ParamRaw,
{
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Param")
            .field("value", &self.load())
            .finish()
    }
}

/// A lock-free scalar cell for a unit whose raw representation is `f64`
/// (`Unit<Raw = f64>`), held inline: [`Param`]'s neighbour for a hot loop's own
/// state, not a mode of it.
///
/// The width is the reason it exists. [`ReadRate`](super::ReadRate) and
/// [`SamplePosition`](super::SamplePosition) are `f64` because a disk voice adds
/// the rate into its read position once per sample for the length of the file.
/// A cell that stored the rate as `f32` and widened it on load would put that
/// rounding back into the sum on every load, and the error does not average
/// out: it integrates. (A `Param<ReadRate>` would hold the full width too, but
/// shared through an `Arc`; this cell is for state nothing else holds.)
///
/// Unlike [`Param`], nothing is shared by cloning: the cell is an inline
/// [`AtomicU64`] holding the value's bits, construction does not allocate, and
/// another thread reaches it through `&self` on whatever already shares its
/// owner (a voice's `RtState`, a memory source). It is not what a node's
/// control is: those are `Param`s, which a host addresses by `UnitAddr`. There
/// is no `Default`: a default [`ReadRate`](super::ReadRate) is `0.0`, a stopped
/// voice, so the initial value is always named.
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
/// An `f32` unit is not a `Param64`:
///
/// ```compile_fail
/// use tutti_types::{Hz, Param64};
/// struct Filter {
///     cutoff: Param64<Hz>, // `Param<Hz>`
/// }
/// ```
// Mutation (run) for the `compile_fail` doctest: `Raw = f64` dropped from
// `Param64`'s struct bound → it compiles → fails.
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
    use core::sync::atomic::Ordering;

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

    /// An `f64` unit's `Param` holds the full width, shares one cell across
    /// handles, and a `ParamCell` reads it as `f32` and writes it widened.
    ///
    /// Mutation (run): `ParamRaw for f64` stores `self as f32 as f64` → the
    /// first assert loses the low bits and fails; `ParamCell::store` writing
    /// into a fresh cell instead of the shared one → the last assert fails.
    #[test]
    fn an_f64_param_keeps_its_width_and_its_cell_is_shared() {
        use crate::value::units::Seconds;
        let long = Seconds(3600.0 + 1.0e-9); // not an f32
        let a = Param::new(Seconds(0.0));
        let b = a.handle();
        a.store(long);
        assert_eq!(b.load().0.to_bits(), long.0.to_bits());

        let cell = a.cell();
        assert_eq!(cell.load(Ordering::Acquire), long.0 as f32);
        cell.store(0.25, Ordering::Release);
        assert_eq!(b.load(), Seconds(0.25));
        assert!(cell.ptr_eq(&b.cell()));
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
