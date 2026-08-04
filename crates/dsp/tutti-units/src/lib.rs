//! DSP nodes for the Tutti audio engine.
//!
//! # Live control values must live in shared storage (MANDATORY)
//!
//! > A value a user can change **while the node is rendering** lives behind an
//! > `Arc` — a [`Param<U>`], an `Arc<AtomicBool>`, an `Arc<AtomicU8>` — and its
//! > setter takes **`&self`**. A `&mut self` setter on an `AudioUnit` means
//! > exactly one thing: *restructure me, and expect a respawn.*
//!
//! This is not style. `Net`'s frontend holds **clones** of its vertices, and
//! `Net::migrate` swaps the backend's unit back over any vertex it considers
//! unchanged. A control stored **by value** therefore cannot be changed on a
//! live node: the write lands on a clone the next commit discards. There is no
//! error and no diagnostic — the fader moves on screen and not in the sound.
//! `tests/live_value_survives_commit.rs` is that mechanism as three assertions.
//!
//! **`&self` is necessary, not sufficient.** A plain `AtomicBool` field also
//! permits `&self` and is *still* lost, because `Clone` copies the atomic
//! rather than sharing it (`tutti_sampler`'s `MemorySource` is the cautionary
//! example: `trigger`/`play`/`stop` all take `&self` and all evaporate). The
//! property that matters is **shared across clones**; `&self` is how you get
//! there, not proof that you did.
//!
//! ## Which mechanism, by what the value is
//!
//! | the value | mechanism |
//! |---|---|
//! | one `f32` with a unit newtype | [`Param<U>`] + `&self` setter + a [`UnitParam`](tutti_core::UnitParam) arm in `AudioUnit::set` |
//! | one `bool`, or a small `Copy` enum | `Arc<AtomicBool>` / `Arc<AtomicU8>` + `&self` setter. A bool rides `UnitParam`'s documented `>= 0.5` encoding — `Setting` carries an `f32`, so it has to. |
//! | a multi-field struct, or anything heap-backed | a command queue: flatten the struct into scalar fields, allocate sender-side, drain in the callback |
//!
//! `Param<U>` stops where [`Setting`](tutti_core::dsp::Setting) stops: its
//! payload is one `f32`, so anything wider leaves the `set()` path entirely.
//! That is a property of the transport, not a limitation of `Param`.
//!
//! `RtPublish` is **not** on this ladder. It exists to move a *deallocation*
//! off the audio thread — routing tables, PDC vectors, meter maps. Reaching for
//! it to share a 16-byte `Copy` struct pays two `SeqCst` loads, a thread-local
//! lookup and a scarce guard slot for none of its benefit.
//!
//! ## The failure is silent at four layers
//!
//! Worth knowing before assuming a setting arrived. [`AudioUnit::set`] has an
//! **empty default body**, so a unit that does not implement it swallows every
//! setting; a unit that does implement it ignores params it does not own (which
//! is deliberate — it is what lets a host push without dispatching on node
//! type); `from_setting` answers `None` for an unknown id; and `Net::set` drops
//! a misaddressed setting with no `else`. The only counter that exists,
//! `Net::take_dropped_settings`, measures **queue-full alone**.

// The crate's only fallible operation is VBAP speaker-layout construction, so
// `Error` / `Result` exist only under `spatial` (without it `Error` would be an
// uninhabited enum with no users).
#[cfg(feature = "spatial")]
mod error;
#[cfg(feature = "spatial")]
pub use error::{Error, Result};

mod node_id;

pub use tutti_core::{
    params, Amplitude, ArcDegrees, Azimuth, Bpm, Cents, CompressionRatio, Db, Depth, Drive,
    Elevation, Feedback, Hz, Mix, Param, Resonance, SampleRate, Seconds, Semitones, Spread,
    StereoWidth, Unit, Q,
};

// NOTE: the shared DSP param pool (`Frequency`/`FilterQ`/`WetMix`/…), the node
// authoring markers (`FilterNode`/`ReverbNode`/…), the generic marker spawner
// (`spawn_dsp_node`/`TuttiDspPlugin`), the param reconcilers
// (`reconcile_unit_params`/`reconcile_reverb_params`/`reconcile_convolver_params`),
// and the deferred convolver load moved OUT of this crate to
// `dawai_model::engine_bind` — they are DAW-param ECS policy, not DSP. This
// crate keeps only the pure DSP unit types + the `set(UnitParam)` surface the
// app drives them through. (Engine Bevy = Net pump only.)

pub mod buffer;

mod lfo;
// `LfoNode` is now `ModulatorNode<Lfo>` — the fundsp adapter over a pure
// `tutti_mod::Modulator`. `LfoShape`/`Lfo`/`Modulator` are re-exported from
// `tutti-mod` through `lfo` so existing `use tutti_units::LfoShape` sites are
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
pub use dynamics::{BrickwallLimiter, Compressor, Gate, LimiterNode};

// A node declares its own audio-rate param-input ports (cutoff, drive, …). No
// Bevy dependency — pure node capability.
mod param_ports;
pub use param_ports::ParamPorts;

pub mod param_mod;
pub use param_mod::{
    build_param_mod, wire_param_mod, AtomicSourceUnit, ClampBounds, ParamModChain, ParamModShaping,
    ParamShaperUnit, ParamSumUnit,
};

// The native `ModParams` impls (the trait itself lives in tutti-mod).
mod mod_params;

// Re-export the `ModParams` trait + modulation *target* surface from tutti-mod so
// downstream crates (e.g. tutti-plugin implementing `ModParams`) reach it here
// alongside `Lfo`, without a separate tutti-mod dep. The routing feature is on
// (tutti-units deps tutti-mod with `features = ["routing"]`).
pub use tutti_mod::{
    AtomicTarget, BeatLfo, CurveModulator, LayerKey, LayeredCurve, ModParams, ModTarget,
};

// The fan-in every mixer needs: `K` sources × `N` channels summed into one
// `N`-wide output. Ungated on purpose — it is arity arithmetic, not geometry, so
// gating it under `spatial` made a VBAP dependency the price of summing two
// stereo signals. `spatial`'s `build_surround_mix` is one consumer, not the only
// one.
mod mix_bus;
pub use mix_bus::ChannelSumUnit;

mod downmix_unit;
pub use downmix_unit::DownmixUnit;

// The mixer strip: volume, stereo balance, mute. Ungated for the same reason as
// `mix_bus` — a fader is not a spatial concept.
mod strip;
pub use strip::BusStripUnit;

#[cfg(feature = "spatial")]
mod spatial;
#[cfg(feature = "spatial")]
pub use spatial::{build_surround_mix, SpatialPannerNode, SurroundSource};
#[cfg(feature = "hrtf")]
pub use spatial::{HrtfBinauralError, HrtfBinauralNode};

mod modulation;
pub use modulation::{ChorusNode, FlangerNode, PhaserNode, StereoPhaserNode};

#[cfg(feature = "convolution")]
mod convolution;
#[cfg(feature = "convolution")]
pub use convolution::{
    generate_room_ir, generate_room_ir_into, generate_test_ir, generate_test_ir_into, Convolver,
    ConvolverNode, IrChannelConfig, StereoConvolverNode, WetDry,
};

/// Transport-driven envelope automation — the playback-side `AutomationLane`
/// `AudioUnit`, the [`Curve`](automation::Curve) trait it evaluates, and the
/// capture-side `Recorder`. See the module docs.
pub mod automation;

// NOTE: the spatial-panner graph binding (`spatial_graph`) and the automation
// graph binding (`automation::graph`) moved app-side to
// `dawai_model::engine_bind::{spatial, automation}` — they bound the DAW
// `Volume`/`Pan`/`PluginParam` components, which left the engine. This crate
// keeps only the pure DSP: the spatial panner nodes (`spatial/`) + the
// automation `AudioUnit` (`automation::{lane, recording}`).
