#![doc = include_str!("../README.md")]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![deny(unsafe_code)]
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
//!   [`SvfCoeffs::new`](tutti_dsp::SvfCoeffs::new) and
//!   [`LadderCoeffs::new`](tutti_dsp::LadderCoeffs::new); [`Real`], the
//!   `f32`/`f64` state type.
//! - Crossover: [`Crossover`](tutti_dsp::Crossover), a 2 to 6 band Linkwitz-Riley 4th-order split
//!   (a plain struct, `f64` state, ramped frequency changes), and
//!   [`CrossoverNode`] over it, each crossover frequency an indexed
//!   [`Cutoff`](tutti_types::UnitParam::Cutoff) param, with [`CrossoverError`].
//! - Delay: [`DelayLineNode`] over `tutti_dsp::DelayLine`, with [`InterpolationMode`].
//! - Dynamics: [`CompressorNode`], [`GateNode`], [`LimiterNode`],
//!   [`BrickwallLimiterNode`], and [`MultibandCompressorNode`] (2 to 4
//!   bands over a [`Crossover`](tutti_dsp::Crossover), every setting an indexed param: see
//!   [`BAND_PARAMS`]) with its [`MultibandControls`].
//! - Modulation effects: [`ModDelayNode`] ([`ModDelayConfig`]) and
//!   [`PhaserNode`]; [`DistortionNode`] with [`ShapeKind`].
//! - Modulation sources: [`LfoNode`] and [`ModulatorNode`], over a
//!   [`Modulator`] (an LFO's is `tutti-modulation`'s [`Wave`]).
//! - Signal-driven modulation sources (design doc 014 §6), feeding a
//!   control lane's event input: [`EnvelopeFollowerNode`] and
//!   [`NoteEnvelopeNode`].
//! - Spatial: [`VbapPannerNode`] ([`VbapError`]) and [`HrtfBinauralNode`],
//!   steered by their params ([`VBAP_PANNER_PARAMS`], [`HRTF_BINAURAL_PARAMS`]:
//!   bearing, height, and the VBAP spread and width or the HRTF blend).
//! - Mixing: [`DownmixNode`], [`BusStripNode`]. Summing is the graph's
//!   (a port or an output channel sums its sources), not a node's.
//! - Param ports: each node's `*_PARAMS` port list (for example
//!   [`SVF_PARAMS`]), which a control lane drives
//!   ([`ControlLaneNode`](automation::ControlLaneNode)).
//! - Modules: [`automation`] (lanes and recording).
//!
//! [`SampleRate::DEFAULT`]: tutti_types::SampleRate::DEFAULT

// The fallible constructions are the crossover's (`Crossover::new`,
// `CrossoverNode::new`, `MultibandCompressorNode::new`), all `CrossoverError`;
// the rest of this crate has no `Error` type.

// Per-block control reads and the ramps that keep them from stepping.
mod ramp;

// Rendering helpers for the width-generic nodes' unit tests.
#[cfg(test)]
mod test_support;

pub use tutti_types::{
    Amplitude, ArcDegrees, Azimuth, Bpm, Cents, CompressionRatio, Db, Depth, Drive, Elevation,
    Feedback, Hz, Mix, Param, Q, Resonance, SampleRate, Seconds, Semitones, Spread, StereoWidth,
    Unit,
};

// The boundary this crate holds: pure DSP graph nodes plus the `ParamSet` /
// controls surface a host drives them through, and the Bevy-free modulation
// reconciler (`Modulation`) that builds this crate's lanes and signal nodes
// from `tutti_modulation` declarations. DAW-param ECS policy — the shared param
// pool, node authoring markers, spawners, ECS reconcilers, deferred
// convolver load — is the host's, and deliberately outside this workspace.

mod click;
pub use click::{ClickNode, ClickSettings, ClickState, MetronomeMode};

mod lfo;
// The `Modulator` trait the modulation node runs, and its impl for
// `tutti-modulation`'s `Wave` (the one wave function both tiers evaluate).
mod modulator;
// `LfoNode` is `ModulatorNode<Wave>` — the graph node over a pure
// `Modulator`. `LfoShape` and `Wave` are re-exported from `tutti-modulation` through
// `lfo`, so `use tutti_nodes::LfoShape` works.
pub use lfo::{LFO_PARAMS, LfoMode, LfoNode, LfoShape, ModulatorNode, Wave};
pub use modulator::Modulator;

mod delay;
pub use delay::{DELAY_PARAMS, DelayLineNode};
// `InterpolationMode` is in `DelayLineNode::with_interpolation`'s signature;
// the ring itself (`DelayLine`) is `tutti-dsp`'s only.
pub use tutti_dsp::InterpolationMode;

mod distortion;
pub use distortion::{DISTORTION_PARAMS, DistortionNode};
// `ShapeKind` is in `DistortionNode::new`'s signature; the curves behind it are
// `tutti-dsp`'s.
pub use tutti_dsp::ShapeKind;

mod filter;
pub use filter::{
    BandState, CrossoverNode, EqBandNode, LADDER_PARAMS, LadderFilterNode, SVF_PARAMS,
    SvfFilterNode,
};
// The types in the filter nodes' signatures stay reachable from here; the
// algorithms behind them (`SvfCoeffs`, `LadderCoeffs`, `Crossover`) are
// `tutti-dsp`'s only.
pub use tutti_dsp::{CrossoverError, LadderType, Real, SvfControls, SvfType};

mod dynamics;
pub use dynamics::{
    BAND_PARAMS, BRICKWALL_PARAMS, BrickwallLimiterNode, COMPRESSOR_PARAMS, CompressorNode,
    GATE_PARAMS, GateNode, LIMITER_PARAMS, LimiterNode, MultibandCompressorNode, MultibandControls,
};

mod downmix_unit;
pub use downmix_unit::DownmixNode;

// The mixer strip: volume, stereo balance, mute. Ungated: a fader is not a
// spatial concept.
mod strip;
pub use strip::{BusStripNode, STRIP_PARAMS};

// The dry path's delay, shared by the nodes that blend a processed signal with
// their input.
mod dry_align;

// What a `DesignNode` does with its input around the realization: the dry path
// and its mix (`DryWet`), or nothing (`NoBlend`).
mod blend;
pub use blend::{Blend, DryWet, NoBlend};

// The reverb: any `tutti_dsp::Design` as a node, and the Freeverb send (mono in,
// stereo wet out) the SoundFont player's General MIDI setup uses.
mod reverb;
pub use reverb::{ControlParams, DesignNode, FreeverbNode, FreeverbParams, NodeDesign};

// The feedback delay network as a `DesignNode`, and the choice between the two.
mod reverb_fdn;
pub use reverb_fdn::{FdnNode, FdnParams, ReverbKind, SpawnedReverb};

// The designs and controls that sit in a `DesignNode<D>`'s signature.
pub use tutti_dsp::{FdnControls, FdnDesign, FreeverbControls, FreeverbDesign};

// The spatial panners: VBAP speaker panning and binaural HRTF rendering.
mod spatial;
pub use spatial::{
    HRTF_BINAURAL_PARAMS, HrtfBinauralNode, VBAP_PANNER_PARAMS, VbapError, VbapPannerNode,
};

mod modulation;
pub use modulation::{MOD_DELAY_PARAMS, ModDelayConfig, ModDelayNode, PHASER_PARAMS, PhaserNode};

// The signal-driven sources a `tutti_modulation::compile` declaration describes
// (`SourceNode`): nodes that send a raw value into control lanes.
mod signal;
pub use signal::{
    EnvelopeFollowerNode, FOLLOWER_EVENT_CAPACITY, NOTE_ENVELOPE_EVENT_CAPACITY, NoteEnvelopeNode,
};

// The reconcile step (doc 014 §8.2): declarations compiled and applied to
// an `Editor` as lanes, signal-source nodes and edges, with no Bevy. It
// lives here because this crate owns the lane and the signal nodes and
// already names `tutti-modulation` and `tutti-graph`.
mod reconcile;
pub use reconcile::{Applied, Feed, ModHost, Modulation, ReconcileError, Target};

#[cfg(feature = "convolution")]
mod convolution;
#[cfg(feature = "convolution")]
pub use convolution::{
    CONVOLVER_PARAMS, ConvolverNode, IrChannelConfig, WetDry, generate_room_ir,
    generate_room_ir_into, generate_test_ir, generate_test_ir_into,
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
