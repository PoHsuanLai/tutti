//! DSP nodes for the Tutti audio engine.
//!
//! Every node here is a graph node (`tutti_graph::Node`): it is inserted into
//! a graph (`tutti_graph::{GraphBuilder, Editor}`), wired, prepared and
//! rendered. Nothing in this crate is fallible — there is no `Error` type — so
//! a node is ready to insert the moment it is built.
//!
//! # Live control values live in shared storage
//!
//! A value a user can change **while the node is rendering** is a [`Param<U>`]
//! cell (or an `Arc`'d atomic), reached through the controls or the
//! `ParamSet` the node was inserted with, and its setter takes `&self`. The
//! graph never clones a node to commit it, so a node's own fields are the
//! running node's; the shared cell is how the control thread reaches them
//! without a lock. (Under fundsp's `Net`, deleted in design doc 013 Phase 5,
//! the frontend held clones of every vertex, so a control stored by value was
//! silently lost on a live node; the rule predates the graph and still holds.)
//!
//! A [`Param<U>`] carries one `f32` with its unit; anything wider (a layout, a
//! table) is a typed control of its own, not a param.
//!
//! # The graph supplies the rate
//!
//! Every time constant in this crate is derived from a sample rate: delay taps
//! and ring lengths from [`Seconds`], filter coefficients from a cutoff in
//! [`Hz`] against Nyquist, envelope attack/release coefficients, LFO phase
//! increments. A constructor that needs one seeds [`SampleRate::DEFAULT`], and
//! the graph calls the node's `prepare` with the device rate before its first
//! block, so there is no path on which a node in a graph runs at the
//! placeholder. A test drives one prepared (`tutti_graph::contract::prepared`,
//! or a `tutti_graph::Solo`).
//!
//! Nodes carrying no rate-dependent quantity — [`BusStripNode`],
//! [`ChannelSumNode`], [`DownmixNode`], [`DistortionNode`] — have nothing for a
//! wrong rate to skew. The placeholder is also what makes a rate-free
//! constructor representable at all: [`ModDelayNode::chorus`] takes only a
//! width, yet builds delay lines.
//!
//! The quick start, the mechanism table and the features are in the crate
//! README, included below.
//!
//! [`SampleRate::DEFAULT`]: tutti_core::SampleRate::DEFAULT
#![doc = include_str!("../README.md")]

// NOTE: this crate has no fallible operation and therefore no `Error` type.
// Speaker-layout construction was the only fallible thing here, and it lives in
// `tutti-spatial` with the panners.

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
// `tutti-mod` through `lfo` so existing `use tutti_nodes::LfoShape` sites are
// untouched.
pub use lfo::{Lfo, LfoMode, LfoNode, LfoShape, Modulator, ModulatorNode};

mod delay;
pub use delay::{DelayLine, DelayLineNode, InterpolationMode, DELAY_PARAMS};

mod distortion;
pub use distortion::{DistortionNode, ShapeKind, DISTORTION_PARAMS};

mod filter;
pub use filter::{
    compute_ladder_coeffs, compute_svf_coeffs, BandState, EqBandNode, LadderCoeffs,
    LadderFilterNode, LadderType, SvfCoeffs, SvfFilterNode, SvfType, LADDER_PARAMS, SVF_PARAMS,
};

mod dynamics;
pub use dynamics::{
    BrickwallLimiterNode, CompressorNode, GateNode, LimiterNode, BRICKWALL_PARAMS,
    COMPRESSOR_PARAMS, GATE_PARAMS, LIMITER_PARAMS,
};

// The shaping an audio-rate modulation edge authors; the edge itself is the
// graph's (design doc 013 item 6).
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
// `N`-wide output. Ungated on purpose — it is arity arithmetic, not geometry, so
// gating it under `spatial` made a VBAP dependency the price of summing two
// stereo signals. `spatial`'s `build_vbap_mix` is one consumer, not the only
// one.
mod mix_bus;
pub use mix_bus::ChannelSumNode;

mod downmix_unit;
pub use downmix_unit::DownmixNode;

// The mixer strip: volume, stereo balance, mute. Ungated for the same reason as
// `mix_bus` — a fader is not a spatial concept.
mod strip;
pub use strip::{BusStripNode, STRIP_PARAMS};

// NOTE: the spatial panners (`VbapPannerNode`, the HRTF binaural pair) and
// `build_vbap_mix` moved to the `tutti-spatial` crate. They were the crate's
// only *geometry* — azimuth, elevation, speaker layouts — where everything left
// here is per-channel signal processing. `tutti-spatial` depends on this crate
// (its mix builder is assembled from `ChannelSumNode` + `SvfFilterNode`), so the
// arrow points geometry → DSP and nothing here names it.

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
// test, example or bench wires a graph out of. They replace the fundsp
// one-liners (`dc`, `sine_hz`, `pass`, `split`, `sink`, …) that
// `tutti_core::dsp` used to forward for the same job.
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

// NOTE: the spatial-panner graph binding (`spatial_graph`) and the automation
// graph binding (`automation::graph`) moved host-side — they bound DAW
// `Volume`/`Pan`/`PluginParam` components, which are not this engine's
// vocabulary. This crate keeps only the pure DSP: the spatial panner nodes
// (`spatial/`) + the automation `AudioUnit` (`automation::{lane, recording}`).
