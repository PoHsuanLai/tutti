//! Typed params of a node: [`TypedParamRef`], what a [`Key`]'s param methods
//! return (`lp.cutoff()`), and the declaration those methods are generated
//! from ([`HasParam`], [`declare_params!`](crate::declare_params)).
//!
//! A node's params are declared once, as a kind and a unit
//! (`UnitParam::Cutoff => Hz`), by the macro that makes the node a
//! [`SessionNode`](crate::SessionNode) (the engine's) or by `declare_params!`
//! (a host's). A [`Key<N>`] then has a method per kind the node declares:
//! `Key<SvfFilterNode<f32>>::cutoff()` is a [`TypedParamRef<Hz>`], and
//! `gate.cutoff()` on a gate does not compile. A node with several params of
//! one kind (a multiband compressor's threshold per band) also has the
//! indexed method, `mb.threshold_at(2)`.
//!
//! The kind and the unit are the declaration's; whether the node *has* the
//! param now (a free-running LFO has a rate, a beat-synced one does not) is
//! the graph's, and [`Graph::set_param`](crate::Graph::set_param) refuses a
//! param the node lacks.

use std::fmt;
use std::marker::PhantomData;

use thiserror::Error;
use tutti_graph::NodeKey;
use tutti_types::{
    Amplitude, Azimuth, Cents, CompressionRatio, Db, Depth, Drive, Elevation, Feedback, Hz, Mix,
    Pan, ParamAddr, ParamRaw, Q, Resonance, Seconds, Spread, StereoWidth, Unit, UnitAddr,
    UnitParam,
};

use crate::Key;

/// One native param of a node in the session's graph, with the unit its value
/// is in: what `key.cutoff()` returns. The typed sibling of
/// `ParamRef` (feature `modulation`), which it converts
/// into, so `mods.route(src, lp.cutoff(), depth)` takes it as it takes the
/// untyped one; a plugin's param stays a `ParamRef`.
///
/// `Copy`, and equal when node and address are.
pub struct TypedParamRef<U> {
    node: NodeKey,
    addr: UnitAddr,
    unit: PhantomData<fn() -> U>,
}

impl<U> TypedParamRef<U> {
    /// `addr` on `node`, whose value is in `U`. A [`Key`]'s param methods
    /// are the way to make one that the node's declaration checked.
    #[must_use]
    pub fn new(node: impl Into<NodeKey>, addr: UnitAddr) -> Self {
        Self {
            node: node.into(),
            addr,
            unit: PhantomData,
        }
    }

    /// The node with the param.
    #[must_use]
    pub const fn node(self) -> NodeKey {
        self.node
    }

    /// The param's address on that node.
    #[must_use]
    pub const fn addr(self) -> UnitAddr {
        self.addr
    }

    /// The same kind of param at `index` among the node's params of that
    /// kind (`ParamRef::unit_at`).
    #[must_use]
    pub const fn at(self, index: u8) -> Self {
        Self {
            node: self.node,
            addr: UnitAddr::new(self.addr.param, index),
            unit: PhantomData,
        }
    }
}

// Written out: the derives would demand `U: Clone`/`Eq`/..., and the float
// units cannot be `Eq`. The reference's identity is its node and address.
impl<U> Clone for TypedParamRef<U> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<U> Copy for TypedParamRef<U> {}
impl<U> PartialEq for TypedParamRef<U> {
    fn eq(&self, other: &Self) -> bool {
        self.node == other.node && self.addr == other.addr
    }
}
impl<U> Eq for TypedParamRef<U> {}
impl<U> std::hash::Hash for TypedParamRef<U> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.node.hash(state);
        self.addr.hash(state);
    }
}
impl<U> fmt::Debug for TypedParamRef<U> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "TypedParamRef<{}>({}, {:?})",
            std::any::type_name::<U>(),
            self.node.0,
            self.addr
        )
    }
}

impl<U> From<TypedParamRef<U>> for ParamAddr {
    fn from(param: TypedParamRef<U>) -> Self {
        ParamAddr::unit(param.addr)
    }
}

#[cfg(feature = "modulation")]
impl<U> From<TypedParamRef<U>> for crate::ParamRef {
    fn from(param: TypedParamRef<U>) -> Self {
        Self::new(param.node, ParamAddr::unit(param.addr))
    }
}

mod sealed {
    pub trait Sealed {}
}

/// What a param's value is: a unit tutti defines ([`Scalar`]), or a `bool` for
/// a switch (`Bypass`, `Solo`, `Freeze`: a cell at or above 0.5 is on). Sealed:
/// the engine's units are listed, and a new one joins them.
pub trait ParamValue: Copy + sealed::Sealed {
    /// The value as the `f32` the param's cell carries (a
    /// [`Seconds`] is narrowed: the `ParamSet` and a lane's value are `f32`).
    fn to_cell(self) -> f32;
}

/// A [`ParamValue`] that is a number in a unit (not a switch): what a range
/// (`Mods::range`) is a range of. Sealed.
pub trait Scalar: ParamValue {}

macro_rules! scalar_values {
    ($($unit:ty),+ $(,)?) => {$(
        impl sealed::Sealed for $unit {}
        impl ParamValue for $unit {
            fn to_cell(self) -> f32 {
                <$unit as Unit>::to_raw(self).to_f32()
            }
        }
        impl Scalar for $unit {}
    )+};
}

scalar_values!(
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
impl ParamValue for bool {
    fn to_cell(self) -> f32 {
        f32::from(u8::from(self))
    }
}

/// That the node `Self` has a native param of the kind `P` (a packed
/// [`UnitAddr`] at index 0: `UnitParam::Cutoff as u16`), whose value is in
/// `Unit`. What a [`Key`]'s param method for that kind needs, and what
/// [`declare_params!`](crate::declare_params) writes.
pub trait HasParam<const P: u16> {
    /// The unit the param's value is in.
    type Unit: ParamValue;
}

/// That the node `Self` has several params of the kind `P`, numbered from 0:
/// what a [`Key`]'s indexed method (`threshold_at`) needs. Whether index `i`
/// exists is the graph's to say when it is used.
pub trait HasParams<const P: u16>: HasParam<P> {}

/// A [`Params`](tutti_graph::Params) node has the params its node does.
impl<N: HasParam<P>, const P: u16> HasParam<P> for tutti_graph::Params<N> {
    type Unit = N::Unit;
}
impl<N: HasParams<P>, const P: u16> HasParams<P> for tutti_graph::Params<N> {}

/// The kinds a node declares with [`declare_params!`](crate::declare_params),
/// for a test to hold against the node's own `ParamSet`.
#[doc(hidden)]
pub trait Declared {
    /// Each declared kind, and whether the node has several of it.
    const KINDS: &'static [(UnitParam, bool)];
}

/// Declares the native params of a node, each a kind and the unit its value
/// is in, so a [`Key`] of it has the param methods:
///
/// ```
/// use tutti_session::{Graph, declare_params};
/// use tutti_types::{Hz, Db, UnitParam};
/// # struct MyFilter;
///
/// declare_params!(MyFilter {
///     UnitParam::Cutoff => Hz,
///     // Several of a kind, numbered from 0: `key.threshold_at(i)`.
///     UnitParam::Threshold => [Db],
/// });
/// ```
///
/// It states what the node declares as modulatable (its `Shape`'s params and
/// its `ParamSet`) a second time, at the type level: keep them in step. A
/// switch (`Bypass`, `Solo`, `Freeze`) is declared as `bool`.
#[macro_export]
macro_rules! declare_params {
    ($ty:ty { $($kind:path => $unit:tt),* $(,)? }) => {
        $( $crate::declare_params!(@one $ty, $kind, $unit); )*
        impl $crate::__Declared for $ty {
            const KINDS: &'static [($crate::__UnitParam, bool)] =
                &[$( ($kind, $crate::declare_params!(@many $unit)) ),*];
        }
    };
    (@many [$unit:ty]) => { true };
    (@many $unit:ty) => { false };
    (@one $ty:ty, $kind:path, [$unit:ty]) => {
        $crate::declare_params!(@one $ty, $kind, $unit);
        impl $crate::HasParams<{ $kind as u16 }> for $ty {}
    };
    (@one $ty:ty, $kind:path, $unit:ty) => {
        impl $crate::HasParam<{ $kind as u16 }> for $ty {
            type Unit = $unit;
        }
    };
}

/// Why [`Graph::set_param`](crate::Graph::set_param) refused; nothing was
/// written.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum ParamError {
    /// The node is not in the graph.
    #[error("{0:?} is not in the graph")]
    NoSuchNode(NodeKey),
    /// The node has no param addressable by its set at that address: it does
    /// not declare it (a beat-synced LFO has no rate), or it has no
    /// params at all.
    #[error("{node:?} has no param at {addr:?}")]
    NoSuchParam {
        /// The node.
        node: NodeKey,
        /// The address asked for.
        addr: UnitAddr,
    },
}

/// Why `Mods::range` refused a range; nothing was
/// stored.
#[derive(Clone, Copy, Debug, PartialEq, Error)]
#[non_exhaustive]
pub enum RangeError {
    /// A bound is NaN, so the range has no order.
    #[error("a range needs numbers, not NaN: {min}..{max}")]
    NotANumber {
        /// The lower bound given.
        min: f32,
        /// The upper bound given.
        max: f32,
    },
    /// The lower bound is above the upper: `min > max`.
    #[error("the range is backwards: min {min} is above max {max}")]
    MinAboveMax {
        /// The lower bound given.
        min: f32,
        /// The upper bound given.
        max: f32,
    },
}

/// Generates a [`Key`]'s param methods, one pair per kind, from the kind's
/// name.
macro_rules! param_methods {
    ($($name:ident, $at:ident => $kind:ident;)+) => {
        impl<N: ?Sized> Key<N> {$(
            #[doc = concat!(
                "The node's `", stringify!($kind), "` param, in the unit the node declares it in ",
                "(only a node that declares it has this method)."
            )]
            #[must_use]
            pub fn $name(self) -> TypedParamRef<<N as HasParam<{ UnitParam::$kind as u16 }>>::Unit>
            where
                N: HasParam<{ UnitParam::$kind as u16 }>,
            {
                TypedParamRef::new(self.erase(), UnitAddr::first(UnitParam::$kind))
            }

            #[doc = concat!(
                "The node's `", stringify!($kind), "` param at `index` among its params of ",
                "that kind (`", stringify!($name), "()` is index 0). Whether the node has that ",
                "index is the graph's to say when the reference is used."
            )]
            #[must_use]
            pub fn $at(self, index: u8) -> TypedParamRef<<N as HasParam<{ UnitParam::$kind as u16 }>>::Unit>
            where
                N: HasParams<{ UnitParam::$kind as u16 }>,
            {
                TypedParamRef::new(self.erase(), UnitAddr::new(UnitParam::$kind, index))
            }
        )+}
    };
}

param_methods! {
    cutoff, cutoff_at => Cutoff;
    q, q_at => Q;
    gain_db, gain_db_at => GainDb;
    wet, wet_at => Wet;
    feedback, feedback_at => Feedback;
    delay_time, delay_time_at => DelayTime;
    rate, rate_at => Rate;
    depth, depth_at => Depth;
    room_size, room_size_at => RoomSize;
    damping, damping_at => Damping;
    threshold, threshold_at => Threshold;
    ratio, ratio_at => Ratio;
    attack, attack_at => Attack;
    release, release_at => Release;
    ceiling, ceiling_at => Ceiling;
    drive, drive_at => Drive;
    makeup, makeup_at => Makeup;
    volume, volume_at => Volume;
    detune, detune_at => Detune;
    stereo_spread, stereo_spread_at => StereoSpread;
    pan, pan_at => Pan;
    knee, knee_at => Knee;
    decay, decay_at => Decay;
    diffusion, diffusion_at => Diffusion;
    azimuth, azimuth_at => Azimuth;
    elevation, elevation_at => Elevation;
    spread, spread_at => Spread;
    stereo_width, stereo_width_at => StereoWidth;
    bypass, bypass_at => Bypass;
    solo, solo_at => Solo;
    freeze, freeze_at => Freeze;
}
