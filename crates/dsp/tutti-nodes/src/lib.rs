#![doc = include_str!("../README.md")]
//!
//! ## Sample rate
//!
//! Every time constant here is derived from a sample rate: delay taps and ring
//! lengths from [`Seconds`], filter coefficients from a cutoff in [`Hz`]
//! against Nyquist, envelope attack and release coefficients, LFO phase
//! increments. A constructor that needs a rate seeds [`SampleRate::DEFAULT`],
//! and the graph calls the node's `prepare` with the device rate before its
//! first block, so a node in a graph never renders at the placeholder. To
//! drive a node outside a graph, prepare it first (`tutti_graph::Solo`, or
//! `tutti_graph::contract::prepared` in tests).
//!
//! ## Items
//!
//! - Filters: [`SvfFilterNode`] ([`SvfType`]), [`LadderFilterNode`]
//!   ([`LadderType`]), [`EqBandNode`]; the coefficient solvers
//!   [`compute_svf_coeffs`] and [`compute_ladder_coeffs`]; [`Real`], the
//!   `f32`/`f64` state type.
//! - Delay: [`DelayLineNode`] over [`DelayLine`], with [`InterpolationMode`].
//! - Dynamics: [`CompressorNode`], [`GateNode`], [`LimiterNode`],
//!   [`BrickwallLimiterNode`].
//! - Modulation effects: [`ModDelayNode`] ([`ModDelayConfig`]) and
//!   [`PhaserNode`]; [`DistortionNode`] with [`ShapeKind`].
//! - Modulation sources: [`LfoNode`] and [`ModulatorNode`], over [`Lfo`] and
//!   [`Modulator`].
//! - Mixing: [`ChannelSumNode`], [`DownmixNode`], [`BusStripNode`].
//! - Param modulation: each node's `*_PARAMS` port list (for example
//!   [`SVF_PARAMS`]) and [`ParamModShaping`].
//! - Modules: [`automation`] (lanes and recording), [`buffer`] (ring buffers),
//!   [`param_mod`].
//!
//! [`SampleRate::DEFAULT`]: tutti_core::SampleRate::DEFAULT

// This crate has no fallible operation and therefore no `Error` type.

// Per-block control reads and the ramps that keep them from stepping.
mod ramp;

// Rendering helpers for the width-generic nodes' unit tests.
#[cfg(test)]
mod test_support;

pub use tutti_core::{
    Amplitude, ArcDegrees, Azimuth, Bpm, Cents, CompressionRatio, Db, Depth, Drive, Elevation,
    Feedback, Hz, Mix, Param, Resonance, SampleRate, Seconds, Semitones, Spread, StereoWidth, Unit,
    Q,
};

// The boundary this crate holds: pure DSP graph nodes plus the `ParamSet` /
// controls surface a host drives them through. DAW-param ECS policy — the
// shared param pool, node authoring markers, spawners, reconcilers, deferred
// convolver load — is the host's, and deliberately outside this workspace.

pub mod buffer;

mod lfo;
// `LfoNode` is `ModulatorNode<Lfo>` — the graph node over a pure
// `tutti_mod::Modulator`. `LfoShape`/`Lfo`/`Modulator` are re-exported from
// `tutti-mod` through `lfo`, so `use tutti_nodes::LfoShape` works.
pub use lfo::{Lfo, LfoMode, LfoNode, LfoShape, Modulator, ModulatorNode};

mod delay;
pub use delay::{DelayLine, DelayLineNode, InterpolationMode, DELAY_PARAMS};

mod distortion;
pub use distortion::{DistortionNode, ShapeKind, DISTORTION_PARAMS};

mod filter;
pub use filter::{
    compute_ladder_coeffs, compute_svf_coeffs, BandState, EqBandNode, LadderCoeffs,
    LadderFilterNode, LadderType, Real, SvfCoeffs, SvfFilterNode, SvfType, LADDER_PARAMS,
    SVF_PARAMS,
};

mod dynamics;
pub use dynamics::{
    BrickwallLimiterNode, CompressorNode, GateNode, LimiterNode, BRICKWALL_PARAMS,
    COMPRESSOR_PARAMS, GATE_PARAMS, LIMITER_PARAMS,
};

// The shaping an audio-rate modulation edge authors; the edge itself is the
// graph's.
pub mod param_mod;
pub use param_mod::ParamModShaping;

// A node's control-rate modulation surface is its `ParamSet` (a host
// resolves a route on its cells). Re-export the `ModParams` trait +
// modulation *target* surface from tutti-mod so
// downstream crates (e.g. tutti-plugin implementing `ModParams`) reach it here
// alongside `Lfo`, without a separate tutti-mod dep. The routing feature is on
// (tutti-nodes deps tutti-mod with `features = ["routing"]`).
pub use tutti_mod::{
    AtomicTarget, BeatLfo, CurveModulator, LayerKey, LayeredCurve, ModParams, ModTarget,
};

// The fan-in every mixer needs: `K` sources × `N` channels summed into one
// `N`-wide output. Ungated on purpose — it is arity arithmetic, not geometry.
// `tutti-spatial`'s `build_vbap_mix` is one consumer, not the only one.
mod mix_bus;
pub use mix_bus::ChannelSumNode;

mod downmix_unit;
pub use downmix_unit::DownmixNode;

// The mixer strip: volume, stereo balance, mute. Ungated for the same reason as
// `mix_bus` — a fader is not a spatial concept.
mod strip;
pub use strip::{BusStripNode, STRIP_PARAMS};

// Spatial panners (VBAP, binaural) are `tutti-spatial`'s, which depends on
// this crate; nothing here names it.

mod modulation;
pub use modulation::{ModDelayConfig, ModDelayNode, PhaserNode};

#[cfg(feature = "convolution")]
mod convolution;
#[cfg(feature = "convolution")]
pub use convolution::{
    generate_room_ir, generate_room_ir_into, generate_test_ir, generate_test_ir_into, Convolver,
    ConvolverNode, IrChannelConfig, IrSpectra, WetDry,
};

// Test and stimulus nodes (`Const`, `Osc`, `Through`, `Split`, `Sink`): what a
// test, example or bench wires a graph out of.
//
// Behind the `testing` feature, not `cfg(test)`: the consumers are *other*
// crates' tests, which a `cfg(test)` module is invisible to. The feature keeps
// it out of the production API — a crate turns it on from its
// `[dev-dependencies]` only, the pattern `tutti-sampler`'s `test-support`
// set. This crate reaches its own through a self dev-dependency.
#[cfg(any(test, feature = "testing"))]
pub mod testing;

// No `///` here on purpose: a doc comment on a `pub mod` line shadows the
// module's own `//!` and re-resolves its intra-doc links in this scope, which
// breaks every link the module makes to its own items. See `automation/mod.rs`.
pub mod automation;

// ECS bindings for automation and panning are host-side: they bind DAW
// `Volume`/`Pan`/`PluginParam` components, which are not this engine's
// vocabulary.
