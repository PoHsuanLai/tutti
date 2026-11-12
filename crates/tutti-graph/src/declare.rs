//! A node's params, declared once ([`declare_params!`](crate::declare_params)),
//! and the typed vocabulary they are declared in: [`ParamUnit`], [`HasParam`].
//!
//! A node with params says three things about them: which the graph may
//! modulate (its [`Shape`](crate::Shape)'s ports), which a host reaches by
//! address (its [`ParamSet`]), and what unit each is in (what a typed key
//! hands a caller). The declaration states them once, next to the node's
//! `Param<U>` cells, and generates all three, so they cannot drift and the
//! unit is checked against the cell's own type.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use tutti_types::{
    Amplitude, AtomicF32, Azimuth, Cents, CompressionRatio, Db, Depth, Drive, Elevation, Feedback,
    Hz, Mix, Pan, Param, ParamCell, ParamRaw, Q, Resonance, Seconds, Spread, StereoWidth, Unit,
    UnitAddr,
};

use crate::controls::{ParamSet, Params};
use crate::param::{MAX_PARAM_PORTS, ParamPorts};

mod sealed {
    pub trait Sealed {}
}

/// What a param's value is: a unit tutti defines ([`Scalar`]), or a `bool` for
/// a switch (`Bypass`, `Solo`, `Freeze`: a cell at or above 0.5 is on). Sealed:
/// the engine's units are listed, and a new one joins them.
pub trait ParamUnit: Copy + sealed::Sealed {
    /// The value as the `f32` the param's cell carries (a
    /// [`Seconds`] is narrowed: the `ParamSet` and a lane's value are `f32`).
    fn to_cell(self) -> f32;

    /// The value a cell's `f32` stands for: the inverse of
    /// [`to_cell`](Self::to_cell) (a `Seconds` is widened exactly).
    fn from_cell(cell: f32) -> Self;
}

/// A [`ParamUnit`] that is a number in a unit (not a switch): what a range
/// (`Mods::range`) is a range of. Sealed.
pub trait Scalar: ParamUnit + Unit<Raw: ParamRaw> {}

macro_rules! scalar_units {
    ($($unit:ty),+ $(,)?) => {$(
        impl sealed::Sealed for $unit {}
        impl ParamUnit for $unit {
            fn to_cell(self) -> f32 {
                <<$unit as Unit>::Raw as ParamRaw>::to_f32(<$unit as Unit>::to_raw(self))
            }
            fn from_cell(cell: f32) -> Self {
                <$unit as Unit>::from_raw(<<$unit as Unit>::Raw as ParamRaw>::from_f32(cell))
            }
        }
        impl Scalar for $unit {}
    )+};
}

scalar_units!(
    Amplitude,
    Azimuth,
    Cents,
    CompressionRatio,
    Db,
    Depth,
    Drive,
    Elevation,
    Feedback,
    Hz,
    Mix,
    Pan,
    Q,
    Resonance,
    Seconds,
    Spread,
    StereoWidth,
);

impl sealed::Sealed for bool {}
impl ParamUnit for bool {
    fn to_cell(self) -> f32 {
        f32::from(u8::from(self))
    }

    fn from_cell(cell: f32) -> Self {
        cell >= 0.5
    }
}

/// That the node `Self` has a native param of the kind `P` (a packed
/// [`UnitAddr`] at index 0: `UnitParam::Cutoff as u16`), whose value is in
/// `Unit`. What a typed key's param method for that kind needs, and what
/// [`declare_params!`](crate::declare_params) writes from the node's own
/// cells.
pub trait HasParam<const P: u16> {
    /// The unit the param's value is in.
    type Unit: ParamUnit;
}

/// That the node `Self` has several params of the kind `P`, numbered from 0:
/// what a typed key's indexed method (`threshold_at`) needs. Whether index `i`
/// exists is the graph's to say when it is used.
pub trait HasParams<const P: u16>: HasParam<P> {}

/// A [`Params`] node has the params its node does.
impl<N: HasParam<P>, const P: u16> HasParam<P> for Params<N> {
    type Unit = N::Unit;
}
impl<N: HasParams<P>, const P: u16> HasParams<P> for Params<N> {}

/// A cell a declaration names, whose value is in `U`: what makes the unit a
/// declaration states agree with the cell's own type. Implemented for a
/// `&Param<U>` (any unit but its own does not compile), for a switch's
/// `&Arc<AtomicF32>` (`bool` only), and for an `Option` of either, where the
/// node has the param only sometimes.
#[doc(hidden)]
pub trait Cells<U: ParamUnit> {
    /// The cell's value as an `f32`, or `None` where the node has no such
    /// param.
    fn base(&self) -> Option<f32>;

    /// The cell, shared. Clones the `Arc`: control thread.
    fn cell(&self) -> Option<ParamCell>;
}

impl<U> Cells<U> for &Param<U>
where
    U: Scalar,
    Arc<<U::Raw as ParamRaw>::Atomic>: Into<ParamCell>,
{
    fn base(&self) -> Option<f32> {
        Some(self.load().to_cell())
    }

    fn cell(&self) -> Option<ParamCell> {
        Some(Param::cell(self))
    }
}

impl Cells<bool> for &Arc<AtomicF32> {
    fn base(&self) -> Option<f32> {
        Some(self.load(Ordering::Acquire))
    }

    fn cell(&self) -> Option<ParamCell> {
        Some(ParamCell::F32(Arc::clone(self)))
    }
}

impl<U: ParamUnit, C: Cells<U>> Cells<U> for Option<C> {
    fn base(&self) -> Option<f32> {
        self.as_ref().and_then(Cells::base)
    }

    fn cell(&self) -> Option<ParamCell> {
        self.as_ref().and_then(Cells::cell)
    }
}

/// One declared param, as a declaration visits it.
#[doc(hidden)]
pub struct Row<'a> {
    addr: UnitAddr,
    port: bool,
    base: f32,
    cell: &'a dyn Fn() -> Option<ParamCell>,
}

impl std::fmt::Debug for Row<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Row")
            .field("addr", &self.addr)
            .field("port", &self.port)
            .field("base", &self.base)
            .finish_non_exhaustive()
    }
}

/// A declaration's visitor: called with each param the node has, in the order
/// declared; `false` stops the walk.
#[doc(hidden)]
pub type Visit<'a> = &'a mut dyn FnMut(Row<'_>) -> bool;

/// Visits one declared param of unit `U`.
#[doc(hidden)]
pub fn row<U: ParamUnit, C: Cells<U>>(
    visit: Visit<'_>,
    addr: UnitAddr,
    port: bool,
    cell: C,
) -> bool {
    let Some(base) = cell.base() else {
        return true;
    };
    visit(Row {
        addr,
        port,
        base,
        cell: &|| cell.cell(),
    })
}

/// The ports a declaration gives the node's shape: its params that are
/// ports, in the order declared.
#[doc(hidden)]
pub fn ports(walk: impl FnOnce(Visit<'_>) -> bool) -> ParamPorts {
    let mut addrs = [UnitAddr::first(tutti_types::UnitParam::Cutoff); MAX_PARAM_PORTS];
    let mut n = 0;
    walk(&mut |row| {
        if row.port {
            addrs[n] = row.addr;
            n += 1;
        }
        true
    });
    ParamPorts::of(&addrs[..n])
}

/// The [`ParamSet`] a declaration gives the node: every param it has.
#[doc(hidden)]
pub fn set(walk: impl FnOnce(Visit<'_>) -> bool) -> ParamSet {
    let mut set = ParamSet::builder();
    walk(&mut |row| {
        if let Some(cell) = (row.cell)() {
            set = std::mem::replace(&mut set, ParamSet::builder()).param(row.addr, cell);
        }
        true
    });
    set.build()
}

/// Port `k`'s cell value: what [`Node::param_base`](crate::Node::param_base)
/// answers.
#[doc(hidden)]
pub fn base(walk: impl FnOnce(Visit<'_>) -> bool, k: usize) -> Option<f32> {
    let (mut next, mut found) = (0, None);
    walk(&mut |row| {
        if !row.port {
            return true;
        }
        if next == k {
            found = Some(row.base);
            return false;
        }
        next += 1;
        true
    });
    found
}

/// Declares a node's params once, next to its cells: what a host reaches by
/// address (its [`ParamSet`]), what the graph may modulate (its shape's
/// ports), and the unit each is in (a typed key's
/// [`HasParam`]).
///
/// ```
/// use tutti_graph::declare_params;
/// use tutti_types::{Hz, Param, Q};
///
/// struct Filter {
///     cutoff: Param<Hz>,
///     q: Param<Q>,
/// }
///
/// declare_params!([] Filter, n {
///     Cutoff => Hz = &n.cutoff,
///     Q => Q = &n.q,
/// });
///
/// let filter = Filter { cutoff: Param::new(Hz(1_000.0)), q: Param::new(Q(0.7)) };
/// // The shape's ports, the node's `ParamSet` and the base of port 1:
/// assert_eq!(filter.declared_ports().as_slice().len(), 2);
/// assert!(filter.declared_set().set(tutti_types::UnitParam::Cutoff, 2_000.0));
/// assert_eq!(filter.declared_base(0), Some(2_000.0));
/// ```
///
/// The unit a row states is checked against the cell's own type: a cutoff
/// declared in the wrong unit does not compile.
///
/// ```compile_fail
/// use tutti_graph::declare_params;
/// use tutti_types::{Db, Hz, Param};
///
/// struct Filter {
///     cutoff: Param<Db>,
/// }
///
/// // `cutoff` is a `Param<Db>`, not a `Param<Hz>`.
/// declare_params!([] Filter, n {
///     Cutoff => Hz = &n.cutoff,
/// });
/// ```
///
/// # Form
///
/// `declare_params!([generics] Type, n { rows })`, where `n` names the node in
/// the rows' cell expressions. Each row is `Kind => Unit = cell`, a
/// [`UnitParam`](tutti_types::UnitParam) kind, the unit its value is in, and
/// the cell: a `&Param<Unit>`, a `&Arc<AtomicF32>` for a `bool` switch, or an
/// `Option` of either for a param the node has only sometimes. The rows are
/// the node's ports in this order, and its set in this order.
///
/// - **Several of a kind** are a group, `[i in range] { rows }`: each row is
///   addressed at index `i`, and the group's rows repeat for each `i` before
///   the next (a band's params, then the next band's). The kind then has the
///   indexed `HasParams`.
/// - **A param that is not a port** (set by address, never modulated) follows
///   a `;` in its group, or in the node's rows.
///
/// It generates, on the type, `declared_ports` (a
/// [`ParamPorts`] for the shape's
/// [`with_param_addrs`](crate::Shape::with_param_addrs)), `declared_set` (the
/// node's [`ParamSet`], for [`ParamNode::param_set`](crate::ParamNode::param_set)) and
/// `declared_base` (port `k`'s cell value, for
/// [`Node::param_base`](crate::Node::param_base)), and the type's
/// [`HasParam`] impls.
#[macro_export]
macro_rules! declare_params {
    ([$($g:tt)*] $ty:ty, $n:ident { $($rows:tt)* }) => {
        impl<$($g)*> $ty {
            #[allow(dead_code)]
            fn declared_visit(&self, visit: $crate::__private::Visit<'_>) -> bool {
                let $n = self;
                $crate::declare_params!(@walk $n visit [] true; $($rows)*);
                true
            }

            /// The params the graph may modulate, in port order.
            #[allow(dead_code)]
            fn declared_ports(&self) -> $crate::ParamPorts {
                $crate::__private::ports(|visit| self.declared_visit(visit))
            }

            /// The params a host reaches by address.
            #[allow(dead_code)]
            fn declared_set(&self) -> $crate::ParamSet {
                $crate::__private::set(|visit| self.declared_visit(visit))
            }

            /// Port `k`'s cell value.
            #[allow(dead_code)]
            fn declared_base(&self, k: usize) -> Option<f32> {
                $crate::__private::base(|visit| self.declared_visit(visit), k)
            }
        }
        $crate::declare_params!(@has [$($g)*] $ty; [] $($rows)*);
    };

    // The walk: a row, a group, and the `;` that turns the rest to set-only.
    (@walk $n:ident $v:ident [$($i:ident)?] $port:tt;) => {};
    (@walk $n:ident $v:ident [$($i:ident)?] $port:tt; ; $($rest:tt)*) => {
        $crate::declare_params!(@walk $n $v [$($i)?] false; $($rest)*);
    };
    (@walk $n:ident $v:ident [] $port:tt;
        [$i:ident in $range:expr] { $($body:tt)* } $(, $($rest:tt)*)?) => {
        for $i in $range {
            $crate::declare_params!(@walk $n $v [$i] true; $($body)*);
        }
        $crate::declare_params!(@walk $n $v [] $port; $($($rest)*)?);
    };
    (@walk $n:ident $v:ident [$($i:ident)?] $port:tt;
        $kind:ident => $unit:ty = $cell:expr $(, $($rest:tt)*)?) => {
        $crate::declare_params!(@row $v [$($i)?] $port; $kind, $unit, $cell);
        $crate::declare_params!(@walk $n $v [$($i)?] $port; $($($rest)*)?);
    };
    (@walk $n:ident $v:ident [$($i:ident)?] $port:tt;
        $kind:ident => $unit:ty = $cell:expr ; $($rest:tt)*) => {
        $crate::declare_params!(@row $v [$($i)?] $port; $kind, $unit, $cell);
        $crate::declare_params!(@walk $n $v [$($i)?] false; $($rest)*);
    };

    (@row $v:ident [] $port:tt; $kind:ident, $unit:ty, $cell:expr) => {
        if !$crate::__private::row::<$unit, _>(
            &mut *$v,
            $crate::__private::UnitAddr::first($crate::__private::UnitParam::$kind),
            $port,
            $cell,
        ) {
            return false;
        }
    };
    (@row $v:ident [$i:ident] $port:tt; $kind:ident, $unit:ty, $cell:expr) => {
        if !$crate::__private::row::<$unit, _>(
            &mut *$v,
            $crate::__private::UnitAddr::new($crate::__private::UnitParam::$kind, $i as u8),
            $port,
            $cell,
        ) {
            return false;
        }
    };

    // The typed side: one `HasParam` per kind, `HasParams` for a group's.
    (@has [$($g:tt)*] $ty:ty; [$($grp:ident)?]) => {};
    (@has [$($g:tt)*] $ty:ty; [$($grp:ident)?] ; $($rest:tt)*) => {
        $crate::declare_params!(@has [$($g)*] $ty; [$($grp)?] $($rest)*);
    };
    (@has [$($g:tt)*] $ty:ty; []
        [$i:ident in $range:expr] { $($body:tt)* } $(, $($rest:tt)*)?) => {
        $crate::declare_params!(@has [$($g)*] $ty; [$i] $($body)*);
        $crate::declare_params!(@has [$($g)*] $ty; [] $($($rest)*)?);
    };
    (@has [$($g:tt)*] $ty:ty; [$($grp:ident)?]
        $kind:ident => $unit:ty = $cell:expr $(, $($rest:tt)*)?) => {
        $crate::declare_params!(@one [$($g)*] $ty; [$($grp)?] $kind, $unit);
        $crate::declare_params!(@has [$($g)*] $ty; [$($grp)?] $($($rest)*)?);
    };
    (@has [$($g:tt)*] $ty:ty; [$($grp:ident)?]
        $kind:ident => $unit:ty = $cell:expr ; $($rest:tt)*) => {
        $crate::declare_params!(@one [$($g)*] $ty; [$($grp)?] $kind, $unit);
        $crate::declare_params!(@has [$($g)*] $ty; [$($grp)?] $($rest)*);
    };

    (@one [$($g:tt)*] $ty:ty; [] $kind:ident, $unit:ty) => {
        impl<$($g)*> $crate::HasParam<{ $crate::__private::UnitParam::$kind as u16 }> for $ty {
            type Unit = $unit;
        }
    };
    (@one [$($g:tt)*] $ty:ty; [$i:ident] $kind:ident, $unit:ty) => {
        impl<$($g)*> $crate::HasParam<{ $crate::__private::UnitParam::$kind as u16 }> for $ty {
            type Unit = $unit;
        }
        impl<$($g)*> $crate::HasParams<{ $crate::__private::UnitParam::$kind as u16 }> for $ty {}
    };
}
