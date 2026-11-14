#![doc = include_str!("../README.md")]
//! The Tutti audio engine's built-in DSP nodes: filters, delays, dynamics,
//! modulation effects, mixing and automation.
//!
//! Every node here is a graph node (`tutti_graph::Node`), and a node file holds
//! only the adapter: shape, prepare, process, params and fork. The algorithm each
//! one runs is a pure function or struct over slices in `tutti-dsp`: the filter
//! coefficients and steps (`SvfCoeffs`, `LadderCoeffs`, `CrossoverModel`), the dynamics
//! curves and recurrences, `Convolver`, `DelayLine` and the waveshaping curves. Build one, insert it
//! into a graph (`tutti_graph::{GraphBuilder, Editor}`), wire it, and the graph
//! prepares it at the device rate and renders it. Inserting a node hands back its
//! controls, usually a `tutti_graph::ParamSet` addressed by `UnitParam` (a
//! `UnitAddr` for a node with several params of a kind), which change the running
//! node without a lock.
//!
//! - **Filters** — `SvfFilterNode`, `LadderFilterNode` (both any width, one
//!   coefficient solve shared across the channels) and `EqBandNode`.
//! - **Crossover** — `CrossoverNode`, which makes `bands × channels` outputs from
//!   `tutti_dsp::CrossoverFamily`, a 2 to 6 band Linkwitz-Riley 4th-order split (the
//!   bands sum to flat magnitude; IIR, so not linear-phase and no latency). Each
//!   crossover is an indexed `Cutoff` param (`UnitAddr::new(UnitParam::Cutoff, i)`).
//! - **Delay** — `DelayNode` (any width, with a checked cross-feedback `Routing`),
//!   a `FamilyNode` over `tutti_dsp::DelayFamily`; the time and feedback are read
//!   per frame when driven.
//! - **Dynamics** — `CompressorNode`, `GateNode`, `BrickwallLimiterNode`,
//!   `LimiterNode`, and `MultibandCompressorNode` (2 to 4 bands, a linked
//!   compressor per band of a Linkwitz-Riley crossover, a hand-written node over
//!   `Realized<MultibandRecurrence>`). Every setting is an indexed param: a
//!   threshold, knee, ratio, attack, release and make-up (`GainDb`) per band, a
//!   `Cutoff` per crossover (27 ports at 4 bands), and a `Bypass` and `Solo`
//!   toggle per band (`>= 0.5` is on, as every toggle; cells without a port). The
//!   per-band gain reduction meters are readings on `MultibandControls`.
//! - **Modulation effects** — `ModDelayNode` (chorus and flanger are
//!   its two `Sweep` presets, over `tutti_dsp::ModDelayFamily`; the General MIDI chorus a third,
//!   with no feedback and a finite tail) and `PhaserNode` (over `tutti_dsp::PhaserFamily`, its `Stages`
//!   and `SweepRange` checked; the rate is floored at 0.01 Hz, a lane's value and a cell's alike), both any width.
//! - **Distortion** — `DistortionNode` and its `ShapeKind` waveshapers.
//! - **Modulation sources** — `ModulatorNode<M>` (aliased `LfoNode`), the per-sample
//!   adapter over a pure `Modulator` (an LFO's is `tutti_modulation::Waveform`, the wave a
//!   control lane evaluates): an audio generator, whose output feeds a node's
//!   audio input. Its `LfoClock` is free (a rate in `Hz`, a param) or synced to the
//!   transport beat (a span fixed at build); its depth and rate are read per frame.
//! - **Oscillator** — [`OscillatorNode`], a mono source: a sine, or a saw, square or triangle
//!   band-limited by `tutti_dsp::Oscillator`; its frequency (`Frequency`) and level (`Volume`) are read per frame.
//! - **Dry/wet** — the convolver, delay, chorus/flanger, phaser and the two reverbs output their wet signal only; blend with the input through the graph's `set_mix`.
//! - **Mixing** — `BusStripNode` (volume / balance; a mute or bypass is the graph's `NodeMode`).
//!   Summing is the graph's: a port or an output channel sums its sources; so is
//!   folding between layouts (a bus edge between declared buses).
//! - **Spatial** — `VbapPannerNode` (stereo in, one output per speaker of a 2, 4,
//!   6, 8 or 12 channel layout) and `HrtfBinauralNode` (two outputs, by FFT
//!   convolution against a caller-supplied HRIR sphere), both steered by their
//!   params (bearing, height, spread and width, or the blend), de-zippered. A mix of several panned sources
//!   is plain fan-in: wire each panner's outputs to the same graph ports.
//! - **Param ports** — each modulatable node declares its params once, with its
//!   cells (`tutti_graph::declare_params!`), as param ports, filled per
//!   frame from one event source each (read through `Io::param`).
//! - **Modulation** — the `automation` module's `ControlLaneNode`, which
//!   evaluates a `tutti_modulation::ControlProgram` into a param port,
//!   the signal sources that feed a lane (`SourceNode<S>`, the one sender of a
//!   `tutti_modulation::ControlSource`: `EnvelopeFollowerNode`,
//!   `NoteEnvelopeNode`), and `Modulation`, the reconciler that builds them from
//!   `tutti_modulation` declarations.
//! - **Automation / convolution** — the `automation` module's `AutomationLaneNode`
//!   (a curve as audio) and recording units, and `ConvolverNode` with its IR
//!   generators (behind a feature).
//!
//! A `*Node` is the graph-facing node; the inner object some are built from,
//! `Modulator`, carries no suffix, because it is not a node and
//! can be used on its own. The algorithms are `tutti-dsp`'s (`tutti_dsp::DelayLine`,
//! `tutti_dsp::CrossoverFamily`, …); this crate re-exports only the types in a node's
//! signature: `SvfType`, `LadderType`, `Real`, `CrossoverError`,
//! `InterpolationMode` and `ShapeKind`.
//!
//! Nothing in this crate can fail: there is no `Error` type, and a node is ready
//! to insert the moment it is built.
//!
//! ## Scope
//!
//! - **No room simulation and no HRIR data.** The panners place a source; the
//!   HRIR dataset is supplied by the caller as bytes.
//! - **No sample playback and no synthesis.** Those are `tutti-sampler`,
//!   `tutti-polysynth` and `tutti-soundfont`.
//!
//! ## Quick start
//!
//! ```rust
//! use tutti_types::MaxBlock;
//! use tutti_types::{Hz, Q, UnitParam};
//! use tutti_graph::{Prepare, Solo};
//! use tutti_nodes::{SvfFilterNode, SvfType};
//! use tutti_types::{SampleRate, Samples};
//!
//! // A lowpass alone in a graph, fed by the graph's input. The filter is a
//! // graph node: inserted, it hands back its `ParamSet` — cutoff, Q and
//! // gain by `UnitParam` — and the graph prepares it at the graph's rate.
//! let filter = SvfFilterNode::<f32>::new(SvfType::LowPass, Hz(800.0), Q(0.707));
//! let mut solo = Solo::new(filter, Prepare::new(SampleRate::SR_48K, MaxBlock::of(Samples(64))));
//! let out = solo.render_input(&[&[1.0; 64]]);
//!
//! // Sweeping the cutoff on the *running* node: the set writes the `Param`
//! // cell the node reads, at the start of its next block.
//! assert!(solo.controls().set(UnitParam::Cutoff, 2_000.0));
//! let out = solo.render_input(&[&[1.0; 64]]);
//! ```
//!
//! ## Changing a running node
//!
//! Once a node is in a graph it belongs to the executor, so the control thread
//! reaches it only through what it shares: its `Param<U>` cells and atomics,
//! reached through the controls it was inserted with. A setter that takes
//! `&self` writes such a cell; it is lock-free and lands on the node's next
//! block. A setter that takes `&mut self` restructures the node and is meant for
//! before insertion (or for a replacement node). Clones of a node share its
//! cells, so a control handle stays valid for the node it came from.
//!
//! `ParamSet::set` returns `false` for a param the node was not inserted with,
//! so a write to the wrong address is visible at the call site. A host pushing
//! one value to many nodes can ignore the `false` from the ones that do not own
//! it.
//!
//! ### Writing your own node's controls
//!
//! The same rule applies to a node you write: a value that changes while the
//! node renders lives behind an `Arc`, shared across clones.
//!
//! | the value | mechanism |
//! |---|---|
//! | one `f32` with a unit newtype | `Param<U>` + `&self` setter + its `UnitParam` address in the node's `ParamNode::param_set` |
//! | one `bool`, or a small `Copy` enum | `Arc<AtomicBool>` / `Arc<AtomicU8>` + `&self` setter. A bool rides `UnitParam`'s `>= 0.5` encoding, since `ParamSet::set` carries an `f32`. |
//! | a multi-field struct, or anything heap-backed | a command queue: flatten the struct into scalar fields, allocate sender-side, drain in the callback |
//!
//! A plain `AtomicBool` field is not enough: `Clone` copies the atomic rather
//! than sharing it, so writes to one clone never reach another. `ArcLend` is
//! for moving a *deallocation* off the audio thread (routing tables, delay
//! vectors), not for sharing a small `Copy` value.
//!
//! ## Real-time behaviour
//!
//! The nodes' `process` paths allocate nothing and take no lock, at any block
//! length up to the graph's prepared maximum; buffers are sized at construction
//! and in `prepare`, on the control thread. The recursive nodes (filters, delays,
//! modulation effects, dynamics) read a non-finite control value (NaN, ±∞) as
//! unchanged, so it never reaches their state.
//!
//! ## Where it sits
//!
//! The `tutti` crate re-exports this one as `tutti::nodes`;
//! `tutti-polysynth`, `tutti-export` and `tutti-plugin` build on
//! it. It re-exports the unit types (`Hz`, `Db`, `Seconds`, …) and `Param` from
//! `tutti-types`.
//!
//! ## Features
//!
//! `default = []`. The FFT convolution reverb is not a feature: `ConvolverNode` at any width, with
//! `IrChannelConfig`, over `tutti_dsp::Convolver`, whose FFT is
//! `tutti_dsp::RealFft` (scalar, so the output is CPU-independent). An IR's
//! spectra are stored once (`tutti_dsp::IrSpectra`, behind an `Arc`) and shared
//! by every channel and every fork of the graph.
//!
//! - `testing` — stimulus and plumbing nodes for tests, examples and benches
//!   (`Const`, `Osc`, `Through`, `Split`, `Sink`). Enable it from
//!   `[dev-dependencies]` only.
#![cfg_attr(docsrs, feature(doc_cfg))]
#![forbid(unsafe_code)]
//!
//! ## Sample rate
//!
//! Every time constant here is derived from a sample rate: delay taps and ring
//! lengths from [`Seconds`], filter coefficients from a cutoff in [`Hz`]
//! against Nyquist, envelope attack and release coefficients, LFO phase
//! increments. A node built over a family ([`FamilyNode`]: the filters, delays, dynamics,
//! modulation effects and reverbs) has no rate until `prepare` gives it one: it builds its
//! realization, its models and every figure derived from the rate there, so an unprepared
//! node holds none. Before `prepare` it declares no latency and an unknown tail, which the
//! graph reads again after it, and renders silence. A setting that names a frequency is
//! checked for what a rate does not change (finite, above 0, in order), and a frequency
//! past the prepared rate's Nyquist is held under it, not refused. The nodes outside the
//! families ([`LfoNode`], [`ConvolverNode`]) take their rate from `prepare` too. To drive a node
//! outside a graph, prepare it first (`tutti_graph::Solo`, or
//! `tutti_graph::contract::prepared` in tests).
//!
//! ## Items
//!
//! - Filters: [`SvfFilterNode`] ([`SvfType`]), [`LadderFilterNode`]
//!   ([`LadderType`]), [`EqBandNode`]; the coefficient solvers
//!   [`SvfCoeffs::new`](tutti_dsp::SvfCoeffs::new) and
//!   [`LadderCoeffs::new`](tutti_dsp::LadderCoeffs::new); [`Real`], the
//!   `f32`/`f64` state type.
//! - Crossover: [`CrossoverNode`], over `tutti_dsp::CrossoverFamily`, a 2 to 6 band
//!   Linkwitz-Riley 4th-order split (`f64` state, solved on the stream's 16-frame grid), each
//!   crossover frequency an indexed [`Cutoff`](tutti_types::UnitParam::Cutoff) param, with
//!   [`CrossoverError`].
//! - Delay: [`DelayNode`] over `tutti_dsp::DelayFamily`, with [`InterpolationMode`] and
//!   [`MaxDelay`].
//! - Dynamics: [`CompressorNode`], [`GateNode`], [`LimiterNode`],
//!   [`BrickwallLimiterNode`], and [`MultibandCompressorNode`] (2 to 4
//!   bands over a Linkwitz-Riley crossover, every setting an indexed param: see
//!   [`BAND_PARAMS`]) with its [`MultibandControls`].
//! - Modulation effects: [`ModDelayNode`] (over `tutti_dsp::ModDelayFamily`, its [`Sweep`] and
//!   [`Stagger`] checked) and [`PhaserNode`] (over `tutti_dsp::PhaserFamily`, its [`Stages`] and
//!   [`SweepRange`] checked); [`DistortionNode`] with [`ShapeKind`].
//! - Modulation sources: [`LfoNode`] and [`ModulatorNode`], over a
//!   [`Modulator`] (an LFO's is `tutti-modulation`'s [`Waveform`]).
//! - Signal-driven modulation sources, feeding a
//!   control lane's event input: [`EnvelopeFollowerNode`] and
//!   [`NoteEnvelopeNode`].
//! - Spatial: [`VbapPannerNode`] ([`VbapError`]) and [`HrtfBinauralNode`],
//!   steered by their params (bearing, height, and the VBAP spread and width
//!   or the HRTF blend).
//! - Mixing: [`BusStripNode`]. Summing is the graph's (a port or an output
//!   channel sums its sources), not a node's, and so is folding between
//!   channel layouts (a bus edge between declared buses).
//! - Param ports: the params each node declares once, with its cells
//!   (`tutti_graph::declare_params!`), which a control lane drives
//!   ([`ControlLaneNode`](automation::ControlLaneNode)).
//! - Modules: [`automation`] (lanes and recording).
//!

// The fallible constructions are the crossover's (`CrossoverControls::new`,
// `CrossoverNode::new`, `MultibandCompressorNode::new`), all `CrossoverError`;
// the rest of this crate has no `Error` type.

// Per-block control reads and the ramps that keep them from stepping.
mod buses;
#[macro_use]
mod params;
mod ramp;

// Rendering helpers for the width-generic nodes' unit tests.
#[cfg(test)]
mod test_support;

pub use tutti_types::{
    Amplitude, ArcDegrees, Azimuth, Bpm, Cents, CentsSpan, CompressionRatio, Db, Depth, Drive,
    Elevation, Feedback, Hz, Mix, Param, Q, Resonance, SampleRate, Seconds, Semitones, Spread,
    StereoWidth, Unit,
};

// The boundary this crate holds: pure DSP graph nodes plus the `ParamSet` /
// controls surface a host drives them through, and the modulation reconciler
// (`Modulation`) that builds this crate's lanes and signal nodes from
// `tutti_modulation` declarations. DAW-param policy is the host's, and
// deliberately outside this workspace.

mod click;
pub use click::{ClickNode, ClickSettings, ClickState, MetronomeMode, VolumeOutOfRange};

mod lfo;
// The `Modulator` trait the modulation node runs, and its impl for
// `tutti-modulation`'s `Waveform` (the one wave function both tiers evaluate).
mod modulator;
// `LfoNode` is `ModulatorNode<Waveform>` — the graph node over a pure
// `Modulator`. `LfoShape` and `Waveform` are re-exported from `tutti-modulation` through
// `lfo`, so `use tutti_nodes::LfoShape` works.
pub use lfo::{LfoClock, LfoNode, LfoShape, ModulatorNode, Waveform};
pub use modulator::Modulator;

mod oscillator;
pub use oscillator::OscillatorNode;

mod delay;
pub use delay::DelayNode;
// `InterpolationMode` and the delay's controls are in `DelayNode::new`'s signature;
// the ring itself (`DelayLine`) is `tutti-dsp`'s only.
pub use tutti_dsp::{
    ChannelTimes, ChannelTimesError, DelayControls, DelayFamily, InterpolationMode, MaxDelay,
    MaxDelayError, Routing, RoutingError,
};

mod distortion;
pub use distortion::DistortionNode;
// `ShapeKind` is in `DistortionNode::new`'s signature; the curves behind it are
// `tutti-dsp`'s.
pub use tutti_dsp::ShapeKind;

mod filter;
pub use filter::{CrossoverNode, EqBandNode, LadderFilterNode, NodeRecurrence, SvfFilterNode};
// The types in the filter nodes' signatures stay reachable from here; the
// algorithms behind them (`SvfCoeffs`, `LadderCoeffs`, `CrossoverModel`) are
// `tutti-dsp`'s only.
pub use tutti_dsp::{
    CrossoverControls, CrossoverError, LadderControls, LadderType, Real, SvfControls, SvfType,
};

mod dynamics;
pub use dynamics::{
    BAND_PARAMS, BrickwallLimiterNode, CompressorNode, GateNode, LimiterNode,
    MultibandCompressorNode, MultibandControls,
};
// The types in the dynamics nodes' constructors: the controls a node is built at, and the
// key a compressor or a gate listens to.
pub use tutti_dsp::{CompressorControls, GateControls, KeySource, LimiterControls, Lookahead};

// The mixer strip: volume, stereo balance, mute. Ungated: a fader is not a
// spatial concept.
mod strip;
pub use strip::BusStripNode;

// The reverb: any `tutti_dsp::Family` as a node, and the Freeverb send (mono in,
// stereo wet out) the SoundFont player's General MIDI setup uses.
mod reverb;
pub use params::{At, Control, ControlRead, ControlSet, ControlValid, Params};
pub use reverb::{FamilyNode, FreeverbNode, NodeFamily, Rings, TailCeiling, TailCeilingError};

// The feedback delay network as a `FamilyNode`.
mod reverb_fdn;
pub use reverb_fdn::FdnNode;

// The families and controls that sit in a `FamilyNode<F>`'s signature.
pub use tutti_dsp::{FdnControls, FdnFamily, FreeverbControls, FreeverbFamily, SvfFamily};

// The spatial panners: VBAP speaker panning and binaural HRTF rendering.
mod spatial;
pub use spatial::{HrtfBinauralNode, VbapError, VbapPannerNode};

mod modulation;
pub use modulation::{ModDelayNode, PhaserNode};
// The sweep, its stagger and the controls are in `ModDelayNode::new`'s and `PhaserNode::new`'s
// signatures.
pub use tutti_dsp::{
    ModDelayControls, ModDelayFamily, PhaserControls, PhaserFamily, Stages, StagesError, Stagger,
    StaggerError, Sweep, SweepError, SweepRange, SweepRangeError,
};

// The control sources' node (`SourceNode<S: ControlSource>`) and the
// signal-driven sources a `tutti_modulation::compile` declaration describes
// (`SignalSource`): nodes that send a raw value into control lanes.
mod signal;
pub use signal::{
    EnvelopeFollowerNode, NoteEnvelopeNode, SOURCE_EVENT_CAPACITY, SourceNode, SourcePorts,
};

// Declarations compiled and applied to an `Editor` as lanes, signal-source
// nodes and edges.
mod reconcile;
pub use reconcile::{Applied, Feed, ModHost, Modulation, ReconcileError, Target};
mod convolution;
pub use convolution::{
    ConvolverNode, IrChannelConfig, generate_room_ir, generate_room_ir_into, generate_test_ir,
    generate_test_ir_into,
};

// Test and stimulus nodes (`Const`, `Osc`, `Through`, `Split`, `Sink`): what a
// test, example or bench wires a graph out of.
//
// Behind the `testing` feature, not `cfg(test)`: the consumers are *other*
// crates' tests, which a `cfg(test)` module is invisible to.
#[cfg(any(test, feature = "testing"))]
pub mod testing;

// No `///` here on purpose: a doc comment on a `pub mod` line shadows the
// module's own `//!` and re-resolves its intra-doc links in this scope, which
// breaks every link the module makes to its own items. See `automation/mod.rs`.
pub mod automation;

// ECS bindings for automation and panning are host-side: they bind DAW
// `Volume`/`Pan`/`PluginParam` components, which are not this engine's
// vocabulary.
