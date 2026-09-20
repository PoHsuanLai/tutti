//! DSP nodes for the Tutti audio engine.
//!
//! Every node here is an `AudioUnit`: it goes into a [`Net`](tutti_core::dsp::Net),
//! gets wired, and renders. Nothing in this crate is fallible — there is no
//! `Error` type — so a node is ready to run the moment it is built.
//!
//! # Live control values must live in shared storage (MANDATORY)
//!
//! > A value a user can change **while the node is rendering** lives behind an
//! > `Arc` — a [`Param<U>`], an `Arc<AtomicBool>`, an `Arc<AtomicU8>` — and its
//! > setter takes **`&self`**. A `&mut self` setter on an `AudioUnit` means
//! > exactly one thing: *restructure me, and expect a respawn.*
//!
//! `Net`'s frontend holds **clones** of its vertices, and `Net::migrate` swaps
//! the backend's unit back over any vertex it considers unchanged — so a control
//! stored **by value** cannot be changed on a live node, and the failure is
//! silent. `&self` is necessary but not sufficient: a plain `AtomicBool` field
//! also permits `&self` and is still lost, because `Clone` copies the atomic
//! rather than sharing it. The property that matters is **shared across clones**.
//!
//! [`Param<U>`] stops where [`Setting`](tutti_core::Setting) stops: its
//! payload is one `f32`, so anything wider leaves the `set()` path entirely.
//! That is a property of the transport, not a limitation of `Param`.
//!
//! [`AudioUnit::set`] has an **empty default body**, which is the first of the
//! three silent layers the README lists; the counted fourth is
//! `Net::take_unaddressed_settings`.
//!
//! # Rate-dependent nodes are born at a placeholder rate (MANDATORY)
//!
//! > A node whose constructor doc says it **starts at [`DEFAULT_SAMPLE_RATE`]**
//! > is *not* ready to run. Call [`AudioUnit::set_sample_rate`] with the real
//! > device rate before the first `process`, or the node renders **silently
//! > wrong-rate audio**.
//!
//! Every time constant in this crate is derived from a sample rate: delay taps
//! and ring lengths from [`Seconds`], filter coefficients from a cutoff in
//! [`Hz`] against Nyquist, envelope attack/release coefficients, LFO phase
//! increments. None can be computed until the rate is known, and the rate is a
//! property of the *device*, not of the code — so these constructors seed
//! [`DEFAULT_SAMPLE_RATE`] and are corrected afterwards.
//!
//! **The failure is neither a panic nor silence.** At 48 kHz an uncorrected
//! node is off by the 44100/48000 ratio — every delay time and filter cutoff
//! lands ~8.8% away from what was asked for. A 500 ms echo returns at 459 ms; a
//! 1 kHz cutoff sits at 1088 Hz. It sounds like plausible audio, which is why
//! nothing downstream catches it. Same hazard and same ratio that
//! `bevy_tutti`'s engine builder documents on the MIDI port manager.
//!
//! In practice the correction arrives through the graph:
//! [`Net`](tutti_core::dsp::Net)'s own [`AudioUnit::set_sample_rate`] forwards
//! to every unit it holds, and the engine calls it once the device is open. A
//! node driven directly — a test, a bench, an offline render assembled by hand
//! — has no such host and must make the call itself.
//!
//! Three of these constructors **allocate** against the placeholder rate (the
//! delay lines, and the limiter's lookahead ring), so the corrective
//! `set_sample_rate` reallocates. That is why the RT no-alloc suites call it
//! outside their no-alloc gate rather than inside it.
//!
//! Nodes carrying no rate-dependent quantity — [`BusStripNode`],
//! [`ChannelSumNode`], [`DownmixNode`], [`DistortionNode`] — are exempt and say
//! nothing, because a wrong rate has nothing to skew. That is also why
//! [`ChorusNode`] and [`FlangerNode`] can still implement [`Default`] while
//! being rate-dependent: the placeholder is what makes a no-argument
//! constructor representable at all.
//!
//! The quick start, the mechanism table, the full silent-failure ladder and the
//! features are in the crate README, included below.
//!
//! [`AudioUnit::set`]: tutti_core::AudioUnit::set
//! [`AudioUnit::set_sample_rate`]: tutti_core::AudioUnit::set_sample_rate
//! [`DEFAULT_SAMPLE_RATE`]: tutti_core::dsp::DEFAULT_SAMPLE_RATE
#![doc = include_str!("../README.md")]

// NOTE: this crate has no fallible operation and therefore no `Error` type.
// Speaker-layout construction was the only fallible thing here, and it lives in
// `tutti-spatial` with the panners.

mod node_id;

pub use tutti_core::{
    Amplitude, ArcDegrees, Azimuth, Bpm, Cents, CompressionRatio, Db, Depth, Drive, Elevation,
    Feedback, Hz, Mix, Param, Resonance, SampleRate, Seconds, Semitones, Spread, StereoWidth, Unit,
    Q,
};

// The boundary this crate holds: pure DSP unit types plus the `set(UnitParam)`
// surface a host drives them through. DAW-param ECS policy — the shared param
// pool, node authoring markers, spawners, reconcilers, deferred convolver load —
// is the host's, and deliberately outside this workspace. Engine Bevy is the Net
// pump only.

pub mod buffer;

mod lfo;
// `LfoNode` is now `ModulatorNode<Lfo>` — the fundsp adapter over a pure
// `tutti_mod::Modulator`. `LfoShape`/`Lfo`/`Modulator` are re-exported from
// `tutti-mod` through `lfo` so existing `use tutti_nodes::LfoShape` sites are
// untouched.
pub use lfo::{Lfo, LfoMode, LfoNode, LfoShape, Modulator, ModulatorNode};

mod delay;
pub use delay::{DelayLine, DelayLineNode, InterpolationMode, StereoDelayLineNode, StereoPair};

mod distortion;
pub use distortion::{DistortionNode, ShapeKind};

mod filter;
pub use filter::{
    BandState, EqBandNode, LadderFilterNode, LadderType, StereoLadderFilterNode,
    StereoSvfFilterNode, SvfFilterNode, SvfType,
};

mod dynamics;
pub use dynamics::{BrickwallLimiterNode, CompressorNode, GateNode, LimiterNode};

// A node declares its own audio-rate param-input ports (cutoff, drive, …). No
// Bevy dependency — pure node capability.
mod param_ports;
pub use param_ports::ParamPorts;

pub mod param_mod;
pub use param_mod::{
    build_param_mod, wire_param_mod, AtomicSourceNode, ClampBounds, ParamModChain, ParamModShaping,
    ParamShaperNode, ParamSumNode,
};

// The native `ModParams` impls (the trait itself lives in tutti-mod).
mod mod_params;

// Re-export the `ModParams` trait + modulation *target* surface from tutti-mod so
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
pub use strip::BusStripNode;

// NOTE: the spatial panners (`VbapPannerNode`, the HRTF binaural pair) and
// `build_vbap_mix` moved to the `tutti-spatial` crate. They were the crate's
// only *geometry* — azimuth, elevation, speaker layouts — where everything left
// here is per-channel signal processing. `tutti-spatial` depends on this crate
// (its mix builder is assembled from `ChannelSumNode` + `SvfFilterNode`), so the
// arrow points geometry → DSP and nothing here names it.

mod modulation;
pub use modulation::{ChorusNode, FlangerNode, PhaserNode, StereoPhaserNode};

#[cfg(feature = "convolution")]
mod convolution;
#[cfg(feature = "convolution")]
pub use convolution::{
    generate_room_ir, generate_room_ir_into, generate_test_ir, generate_test_ir_into, Convolver,
    ConvolverNode, IrChannelConfig, StereoConvolverNode, WetDry,
};

// No `///` here on purpose: a doc comment on a `pub mod` line shadows the
// module's own `//!` and re-resolves its intra-doc links in this scope, which
// breaks every link the module makes to its own items. See `automation/mod.rs`.
pub mod automation;

// NOTE: the spatial-panner graph binding (`spatial_graph`) and the automation
// graph binding (`automation::graph`) moved host-side — they bound DAW
// `Volume`/`Pan`/`PluginParam` components, which are not this engine's
// vocabulary. This crate keeps only the pure DSP: the spatial panner nodes
// (`spatial/`) + the automation `AudioUnit` (`automation::{lane, recording}`).
