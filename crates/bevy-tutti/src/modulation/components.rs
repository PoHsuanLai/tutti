//! The ECS declaration of a modulation graph: sources, routes, and the address
//! of a modulatable parameter.
//!
//! These carry no `Arc`s and no minted ids — they are plain data describing
//! *what should be modulated by what*. Turning that into a live
//! [`ModMatrix`](tutti_mod::ModMatrix) is [`rebuild`](super::rebuild)'s job, and
//! the ids it mints stay inside the resource it builds.

use bevy_ecs::prelude::*;
use bevy_reflect::prelude::*;

use tutti_types::{Depth, Hz, ParamAddr, PhaseIncrement};

/// A modulation source: one LFO shape, running at one [`ModRate`].
///
/// The shape is `tutti_mod`'s, so this component names the engine's waveform
/// vocabulary directly rather than mirroring it — a `From`-bridged copy is what
/// the app layer needs when it has its own authored enum, not what the adapter
/// needs.
///
/// One of possibly several *kinds* of source (see
/// [`ModSourceKind`](super::ModSourceKind)); it is the built-in one, and the
/// only kind [`TuttiModulationPlugin`](super::TuttiModulationPlugin) registers
/// on its own. A source entity carries exactly one kind component plus a
/// [`ModRate`].
#[derive(Component, Reflect, Debug, Clone, Copy, Default, PartialEq)]
#[reflect(Component, Debug, Default)]
#[require(ModRate)]
pub struct ModSource {
    pub shape: LfoShape,
}

impl ModSource {
    pub fn new(shape: LfoShape) -> Self {
        Self { shape }
    }
}

impl super::ModSourceKind for ModSource {
    type Source = tutti_mod::Lfo;

    fn build(&self) -> Self::Source {
        // Depth is the *route's* property, not the source's — one LFO feeding
        // two params at different depths is the ordinary case — so the
        // modulator is built at full depth and each `ModEdge` scales it.
        tutti_mod::Lfo::new(self.shape)
    }

    /// Every LFO shape has a beat-derived form, including the stepped ones —
    /// see [`BeatLfo`](tutti_mod::BeatLfo), which keys their randomness on the
    /// cycle index rather than a threaded stepper.
    fn build_curve(
        &self,
        beats_per_cycle: f32,
        edge: tutti_mod::EdgeShape,
    ) -> Option<std::sync::Arc<dyn tutti_mod::Curve>> {
        Some(tutti_mod::ShapedCurve::erased(
            tutti_mod::BeatLfo::new(self.shape, beats_per_cycle),
            edge,
        ))
    }
}

/// How a [`ModSource`] derives its phase from the transport.
///
/// Mirrors [`SourceRate`](tutti_mod::SourceRate) as a component. It is a
/// separate component rather than a field on `ModSource` so a rate change and a
/// shape change are independently change-detectable — and because rate is the
/// half a UI moves continuously.
#[derive(Component, Reflect, Debug, Clone, Copy, PartialEq)]
#[reflect(Component, Debug, Default)]
pub struct ModRate {
    /// Beat-synced: **beats per cycle** — `2.0` is one cycle every two beats,
    /// not two cycles per beat (phase is `beat / frequency`). Free-running:
    /// cycles per second. Mirrors [`SourceRate::frequency`](tutti_mod::SourceRate::frequency).
    pub frequency: Hz,
    /// A displacement applied after phase generation — negative is meaningful,
    /// which is why it is not a `Phase`.
    pub phase_offset: PhaseIncrement,
    /// Locked to the transport beat, or free-running against elapsed time.
    pub beat_synced: bool,
}

impl Default for ModRate {
    fn default() -> Self {
        Self {
            frequency: Hz(1.0),
            phase_offset: PhaseIncrement(0.0),
            beat_synced: false,
        }
    }
}

impl ModRate {
    /// Locked to the transport at `frequency` **beats per cycle** — see
    /// [`frequency`](Self::frequency).
    pub fn beat_synced(frequency: impl Into<Hz>) -> Self {
        Self {
            frequency: frequency.into(),
            beat_synced: true,
            ..Default::default()
        }
    }

    /// Free-running at `frequency` Hz.
    pub fn free_running(frequency: impl Into<Hz>) -> Self {
        Self {
            frequency: frequency.into(),
            beat_synced: false,
            ..Default::default()
        }
    }

    pub fn with_phase_offset(mut self, offset: impl Into<PhaseIncrement>) -> Self {
        self.phase_offset = offset.into();
        self
    }
}

/// One edge of the mod matrix: `source` drives `param` on `target` at `depth`.
///
/// An entity of its own rather than a list on the source. A route is the thing
/// a user creates, deletes and drags a depth slider on, so it gets `Changed`
/// detection and despawn semantics per edge — a `Vec<Route>` on the source
/// would rebuild every edge whenever any one of them moved.
///
/// `source` and `target` are `Entity`, not indices: index assignment into the
/// driver's source registry is a build-step concern, and an `Entity` survives
/// the rebuild that reassigns them.
#[derive(Component, Reflect, Debug, Clone, Copy, PartialEq)]
#[reflect(Component, Debug)]
pub struct ModRoute {
    /// The entity carrying the [`ModSource`].
    pub source: Entity,
    /// The entity carrying the modulatable node.
    pub target: Entity,
    /// Which of the target's params to drive.
    pub param: ParamAddr,
    /// Bipolar. Negative inverts the source rather than attenuating it.
    pub depth: Depth,
    pub polarity: Polarity,
    pub curve: CurveType,
    /// A disabled route keeps its declaration but contributes nothing, and its
    /// layer is cleared from the target — the difference between muting a route
    /// and deleting it.
    pub enabled: bool,
    /// How often this route's value reaches the param. See [`ModDelivery`].
    pub delivery: ModDelivery,
}

/// How often a route's value reaches the param it drives.
///
/// One axis, three points — **not** a set of flags. Delivery is a choice among
/// alternatives, and the earlier pair of independent bools
/// (`deliver_as_curve` + `at_audio_rate`) could express a fourth state that
/// means nothing: both at once produced a graph chain *and* a per-frame driver
/// edge, two writers racing over one param. That is the hazard
/// [`ModulationMatrix`](super::ModulationMatrix)'s claim set exists to prevent,
/// so it should not be representable one level out either.
///
/// The names say what actually differs — the **rate** — rather than naming an
/// implementation ("curve") or a tier only an audio person parses ("audio
/// rate"). They line up with the three sampling rates `tutti-mod` documents:
/// one `Curve`, read at whatever rate the consumer needs.
///
/// Every tier **falls back to [`PerFrame`](Self::PerFrame)** when its
/// preconditions do not hold. Falling back is always correct — a coarser
/// staircase, never silence — but the two finer tiers differ in how visible the
/// fallback is, and that is worth knowing before choosing one.
#[derive(Reflect, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ModDelivery {
    /// **Per frame.** The driver samples the source once a frame and writes a
    /// scalar into the param's accumulator.
    ///
    /// The default, and the only tier with no preconditions: it is always
    /// available and always correct. Resolution is your framerate, which shows
    /// up as a staircase on a fast modulator into a sharp-responding param.
    #[default]
    PerFrame,
    /// **Per block.** The source is installed as a
    /// [`Curve`](tutti_mod::Curve) that the *sink* evaluates at whatever rate
    /// it reads.
    ///
    /// A **request**: honoured only if the source kind has a curve form *and*
    /// the sink accepts curve layers. The fallback is **silent** — you asked
    /// for a ramp and got a staircase, with nothing to observe.
    ///
    /// Worth asking for when the sink reads faster than the frame rate, such as
    /// a plugin's per-block parameter producer. Pointless for a native param:
    /// [`AtomicTarget`](tutti_mod::AtomicTarget) collapses at a fixed beat, so
    /// a curve stored there would never move and the route falls back.
    PerBlock,
    /// **Per sample.** Materialises `source → shaper → sum → the sink's param
    /// port` as real graph nodes; the driver never touches the param.
    ///
    /// A different *mechanism* rather than a request, and its outcome is
    /// **observable**: the chain either appears in
    /// [`AudioRateChains`](super::audio_rate::AudioRateChains) or it does not.
    /// It falls back when the sink exposes no audio-rate port for the param —
    /// a fact about the node, not an error.
    ///
    /// Worth asking for when the staircase is audible: a fast LFO on a filter
    /// cutoff or a distortion drive. It costs two idle graph nodes per
    /// modulated param (~0.1% of a block at four effects, ~2% at sixty-four),
    /// so it is opt-in rather than the default.
    PerSample,
}

impl ModRoute {
    /// A full-depth bipolar linear route — the common case.
    pub fn new(source: Entity, target: Entity, param: ParamAddr) -> Self {
        Self {
            source,
            target,
            param,
            depth: Depth::FULL,
            polarity: Polarity::Bipolar,
            curve: CurveType::Linear,
            enabled: true,
            delivery: ModDelivery::PerFrame,
        }
    }

    /// Choose how often this route's value reaches the param.
    ///
    /// One setter for one axis, so picking a tier cannot leave a previous
    /// choice standing beside it.
    pub fn deliver(mut self, delivery: ModDelivery) -> Self {
        self.delivery = delivery;
        self
    }

    /// Sugar for [`ModDelivery::PerBlock`].
    pub fn per_block(self) -> Self {
        self.deliver(ModDelivery::PerBlock)
    }

    /// Sugar for [`ModDelivery::PerSample`].
    pub fn per_sample(self) -> Self {
        self.deliver(ModDelivery::PerSample)
    }

    pub fn with_depth(mut self, depth: impl Into<Depth>) -> Self {
        self.depth = depth.into();
        self
    }

    pub fn with_polarity(mut self, polarity: Polarity) -> Self {
        self.polarity = polarity;
        self
    }

    pub fn with_curve(mut self, curve: CurveType) -> Self {
        self.curve = curve;
        self
    }
}

/// The authored `(base, min, max)` for a modulatable param on this entity.
///
/// Modulation is `clamp(base + Σ offsets, [min, max])`, and none of those three
/// numbers is the engine's to invent: a cutoff's sensible range is a decision
/// about the *product*, not about DSP. A host that wants a param modulated
/// declares its range here, and [`rebuild`](super::rebuild) reads it when it
/// resolves the target.
///
/// The values are bare floats because they are in the param's own units — Hz
/// for a cutoff, linear gain for a fader — the same reason
/// [`ModEdge`](tutti_mod::ModEdge)'s bounds are.
#[derive(Component, Reflect, Debug, Clone, PartialEq, Default)]
#[reflect(Component, Debug, Default)]
pub struct ModParamRange {
    /// One entry per modulatable param on this entity.
    pub params: Vec<ParamRange>,
}

/// One param's authored base and bounds. See [`ModParamRange`].
#[derive(Reflect, Debug, Clone, Copy, PartialEq)]
pub struct ParamRange {
    pub param: ParamAddr,
    pub base: f32,
    pub min: f32,
    pub max: f32,
}

impl ModParamRange {
    /// Declare `param` modulatable over `[min, max]`, sitting at `base`.
    pub fn with(mut self, param: ParamAddr, base: f32, min: f32, max: f32) -> Self {
        self.params.push(ParamRange {
            param,
            base,
            min,
            max,
        });
        self
    }

    pub fn get(&self, param: ParamAddr) -> Option<&ParamRange> {
        self.params.iter().find(|p| p.param == param)
    }
}

// The engine's own vocabulary, re-exported so a host writing these components
// imports one module rather than three crates.
pub use tutti_mod::{CurveType, LfoShape, Polarity};
