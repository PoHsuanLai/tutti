//! Typed params of a node: [`TypedParamRef`], what a [`Key`]'s param methods
//! return (`lp.cutoff()`), generated from the node's own declaration
//! ([`HasParam`], which `tutti_graph::declare_params!` writes from the node's
//! cells).
//!
//! A node's params are declared once, next to its cells, as a kind and the
//! unit its cell is in (`Cutoff => Hz = &n.cutoff`). A [`Key<N>`] then has a
//! method per kind the node declares:
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
use tutti_graph::{HasParam, HasParams, NodeKey};
use tutti_types::{ParamAddr, UnitAddr, UnitParam};

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
    /// `addr` on `node`, whose value is in `U`. Only a [`Key`]'s param
    /// methods make one, so the unit is always the one the node declares.
    #[must_use]
    pub(crate) fn new(node: impl Into<NodeKey>, addr: UnitAddr) -> Self {
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
