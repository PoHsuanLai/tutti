//! The declarations: sources, routes, ranges and automation as components,
//! keyed by entities (doc 014 §8.1).
//!
//! Each is plain data. [`reconcile`](super::reconcile) collects them every
//! frame into a `tutti_mod::Declarations` whose keys are the entities' bits
//! (a `SourceKey` is a source entity's, a `RouteKey` a route entity's, a
//! `TargetKey` the target entity's with the param), so no table maps an
//! entity to a key: `Entity::to_bits` is the key and `Entity::try_from_bits`
//! reads it back.

use std::sync::Arc;

use bevy_ecs::prelude::*;
use bevy_ecs::system::SystemParam;

use tutti_mod::{Adsr, Clock, Curve, CurveType, LfoShape, Polarity, Source, Wave};
use tutti_nodes::automation::{RecordMode, Recorder};
use tutti_types::{BeatDuration, Depth, Hz, ParamAddr, PhaseIncrement, Seconds, UnitParam};

/// One modulatable parameter: an entity and the param's address on its
/// node. A native param is a [`UnitParam`]; a hosted plugin's is its own
/// numeric id ([`plugin`](Self::plugin)).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ParamRef {
    /// The entity whose [`AudioNode`](tutti_core::AudioNode) has the param.
    pub entity: Entity,
    /// The param's address on that node.
    pub param: ParamAddr,
}

impl ParamRef {
    /// `param` on `entity`'s node.
    pub fn new(entity: Entity, param: impl Into<ParamAddr>) -> Self {
        Self {
            entity,
            param: param.into(),
        }
    }

    /// A native param of `entity`'s node.
    pub const fn unit(entity: Entity, param: UnitParam) -> Self {
        Self {
            entity,
            param: ParamAddr::Unit(param),
        }
    }

    /// Parameter `id` of the plugin hosted on `entity` (its CLAP or VST3 id,
    /// its AU id, or a VST2 index).
    pub const fn plugin(entity: Entity, id: u32) -> Self {
        Self {
            entity,
            param: ParamAddr::Id(id),
        }
    }

    /// The reconciler's key: the entity's bits and the param.
    pub(crate) fn key(self) -> tutti_mod::TargetKey {
        tutti_mod::TargetKey {
            node: self.entity.to_bits(),
            param: self.param,
        }
    }

    /// The `ParamRef` a key names, `None` for one no entity could have.
    pub(crate) fn from_key(key: tutti_mod::TargetKey) -> Option<Self> {
        Some(Self {
            entity: Entity::try_from_bits(key.node)?,
            param: key.param,
        })
    }
}

/// What feeds a signal source's node: the declaration's half of
/// `tutti_nodes::Feed`, naming entities.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModFeed {
    /// Every audio output of the entity's node: a follower's input.
    Audio(Entity),
    /// Event output `port` of the entity's node: a note envelope's MIDI.
    Events {
        /// The entity whose node plays the MIDI (a clip node).
        entity: Entity,
        /// Its event output.
        port: u16,
    },
}

/// A kind of modulation source. The built-in kinds are [`Lfo`], [`Macro`],
/// [`Follower`] and [`NoteEnvelope`]; a host's own declares itself as one of
/// `tutti_mod::Source`'s.
pub trait ModSourceKind: Send + Sync + 'static {
    /// The source this kind is now.
    fn declare(&self) -> Source;

    /// What feeds its node, for a signal source (a follower, a note
    /// envelope). `None`, the default, for a time function (an LFO, a
    /// macro).
    fn feed(&self) -> Option<ModFeed> {
        None
    }
}

/// A modulation source entity: one [`ModSourceKind`]. Replace the component
/// to change it (a macro's value, an LFO's rate): an edit of a time function
/// republishes the lanes it reaches, with no commit and no phase reset.
#[derive(Component, Clone)]
pub struct ModSource {
    /// What it is.
    pub kind: Arc<dyn ModSourceKind>,
}

impl ModSource {
    /// A source of `kind`.
    pub fn new(kind: impl ModSourceKind) -> Self {
        Self {
            kind: Arc::new(kind),
        }
    }
}

impl std::fmt::Debug for ModSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModSource")
            .field("kind", &self.kind.declare())
            .finish()
    }
}

/// How an [`Lfo`] keeps time.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum LfoRate {
    /// One cycle per this many beats: bar-locked, follows seeks, holds while
    /// the transport is stopped.
    Beats(BeatDuration),
    /// Cycles per second on the graph's play clock, running while stopped.
    Hz(Hz),
}

/// A low-frequency oscillator: a closed-form wave on a clock, no node.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Lfo {
    /// The wave.
    pub shape: LfoShape,
    /// The random shapes' seed; ignored by the others.
    pub seed: u64,
    /// Its clock.
    pub rate: LfoRate,
    /// Added to the cycle position.
    pub phase: PhaseIncrement,
}

impl Lfo {
    /// `shape` at one cycle per second.
    pub const fn new(shape: LfoShape) -> Self {
        Self {
            shape,
            seed: 0,
            rate: LfoRate::Hz(Hz(1.0)),
            phase: PhaseIncrement(0.0),
        }
    }

    /// A sine.
    pub const fn sine() -> Self {
        Self::new(LfoShape::Sine)
    }

    /// A triangle.
    pub const fn triangle() -> Self {
        Self::new(LfoShape::Triangle)
    }

    /// A square.
    pub const fn square() -> Self {
        Self::new(LfoShape::Square)
    }

    /// A rising saw.
    pub const fn saw() -> Self {
        Self::new(LfoShape::Sawtooth)
    }

    /// One cycle per `per_cycle` beats.
    pub const fn rate(mut self, per_cycle: BeatDuration) -> Self {
        self.rate = LfoRate::Beats(per_cycle);
        self
    }

    /// `rate` cycles per second.
    pub const fn hz(mut self, rate: Hz) -> Self {
        self.rate = LfoRate::Hz(rate);
        self
    }

    /// Starting `phase` cycles in.
    pub const fn phase(mut self, phase: PhaseIncrement) -> Self {
        self.phase = phase;
        self
    }

    /// The random shapes' seed.
    pub const fn seed(mut self, seed: u64) -> Self {
        self.seed = seed;
        self
    }
}

impl ModSourceKind for Lfo {
    fn declare(&self) -> Source {
        let clock = match self.rate {
            LfoRate::Beats(per_cycle) => Clock::Beats {
                per_cycle,
                phase: self.phase,
            },
            LfoRate::Hz(rate) => Clock::Seconds {
                rate,
                phase: self.phase,
                anchor: None,
            },
        };
        Source::Lfo {
            wave: Wave::Shape {
                shape: self.shape,
                seed: self.seed,
            },
            clock,
        }
    }
}

/// A constant the user moves: a raw value, saturating past ±1.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Macro(pub f32);

impl ModSourceKind for Macro {
    fn declare(&self) -> Source {
        Source::Macro { value: self.0 }
    }
}

/// An envelope follower of an entity's audio: a node fed every audio output
/// of `input`'s node.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Follower {
    /// The entity it follows.
    pub input: Entity,
    /// Rise time constant.
    pub attack: Seconds,
    /// Fall time constant.
    pub release: Seconds,
}

impl Follower {
    /// Follows `input` with a 5 ms attack and a 100 ms release.
    pub const fn new(input: Entity) -> Self {
        Self {
            input,
            attack: Seconds(0.005),
            release: Seconds(0.1),
        }
    }

    /// Rises with `attack`.
    pub const fn attack(mut self, attack: Seconds) -> Self {
        self.attack = attack;
        self
    }

    /// Falls with `release`.
    pub const fn release(mut self, release: Seconds) -> Self {
        self.release = release;
        self
    }
}

impl ModSourceKind for Follower {
    fn declare(&self) -> Source {
        Source::Follower {
            attack: self.attack,
            release: self.release,
        }
    }

    fn feed(&self) -> Option<ModFeed> {
        Some(ModFeed::Audio(self.input))
    }
}

/// An ADSR per note: a node on event output `port` of `midi`'s node.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NoteEnvelope {
    /// The entity whose node plays the notes.
    pub midi: Entity,
    /// Its event output.
    pub port: u16,
    /// The stages.
    pub adsr: Adsr,
}

impl NoteEnvelope {
    /// `adsr` on `midi`'s event output 0.
    pub const fn new(midi: Entity, adsr: Adsr) -> Self {
        Self {
            midi,
            port: 0,
            adsr,
        }
    }
}

impl ModSourceKind for NoteEnvelope {
    fn declare(&self) -> Source {
        Source::NoteEnvelope { adsr: self.adsr }
    }

    fn feed(&self) -> Option<ModFeed> {
        Some(ModFeed::Events {
            entity: self.midi,
            port: self.port,
        })
    }
}

/// A route: `source` into `target`, by `depth` of the target's range
/// ([`ModRange`]). Its offset is `shape(raw, depth, polarity, curve)` times
/// the range's span, added to the knob (or the automation) and clamped to
/// the range (doc 014 §2).
///
/// A route whose source or target entity is gone is left out of the
/// declaration (and loses its layer); a disabled one too.
#[derive(Component, Clone, Copy, Debug, PartialEq)]
pub struct ModRoute {
    /// The [`ModSource`] entity.
    pub source: Entity,
    /// The param it modulates.
    pub target: ParamRef,
    /// A fraction of the target's range.
    pub depth: Depth,
    /// Both ways or one.
    pub polarity: Polarity,
    /// The response curve on the magnitude.
    pub curve: CurveType,
    /// Whether it is declared at all.
    pub enabled: bool,
}

impl ModRoute {
    /// `source` into `target` at full depth, bipolar, linear.
    pub fn new(source: Entity, target: ParamRef) -> Self {
        Self {
            source,
            target,
            depth: Depth(1.0),
            polarity: Polarity::Bipolar,
            curve: CurveType::Linear,
            enabled: true,
        }
    }

    /// At `depth`.
    pub fn with_depth(mut self, depth: Depth) -> Self {
        self.depth = depth;
        self
    }

    /// With `polarity`.
    pub fn with_polarity(mut self, polarity: Polarity) -> Self {
        self.polarity = polarity;
        self
    }

    /// With `curve`.
    pub fn with_curve(mut self, curve: CurveType) -> Self {
        self.curve = curve;
        self
    }
}

/// A modulatable param's range, `min..=max`: what a route's depth is a
/// fraction of, and what the value is clamped to. There is no base: the
/// base is the param's knob (its [`AudioParam`](crate::graph::AudioParam),
/// `write_param`, a plugin's own value) or its [`Automation`].
///
/// A route needs one on its target, and a target takes one: a second
/// `ModRange` on the same param is refused (`ModulationRefused`,
/// `DuplicateTarget`). Edit it in place: a range edit is a store into the
/// lane, never a plan.
#[derive(Component, Clone, Copy, Debug, PartialEq)]
pub struct ModRange {
    /// The param.
    pub param: ParamRef,
    /// The lowest value.
    pub min: f32,
    /// The highest value.
    pub max: f32,
}

impl ModRange {
    /// `param` over `min..=max`.
    pub fn new(param: ParamRef, min: f32, max: f32) -> Self {
        Self { param, min, max }
    }
}

/// A param's automation: `curve` is its base where it has a value, in place
/// of the knob (doc 014 §2). The curve must be frozen (`Curve::frozen` is
/// `None`: an envelope, not a cell something writes).
///
/// A target with automation and no [`ModRange`] is unclamped. A target
/// takes one `Automation`: a second on the same param is refused
/// (`ModulationRefused`, `DuplicateTarget`).
#[derive(Component, Clone)]
pub struct Automation {
    /// The param.
    pub target: ParamRef,
    /// The curve, by beat.
    pub curve: Arc<dyn Curve>,
}

impl Automation {
    /// `target` follows `curve`.
    pub fn new(target: ParamRef, curve: Arc<dyn Curve>) -> Self {
        Self { target, curve }
    }
}

impl std::fmt::Debug for Automation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Automation")
            .field("target", &self.target)
            .finish_non_exhaustive()
    }
}

/// A recording take on an [`Automation`]'s entity (doc 014 §8.2,
/// "Recording"). While it is on the entity the param declares no
/// automation, so its knob (the gesture) drives it; when the take closes
/// the finished envelope becomes the entity's automation curve and the
/// component goes.
///
/// It closes on [`close`](Self::close), or, in [`RecordMode::Touch`], when
/// the recorder is released after a touch. Feed the recorder through
/// [`recorder_mut`](Self::recorder_mut) as the gesture moves.
#[derive(Component, Debug)]
pub struct AutomationTake {
    recorder: Recorder,
    touched: bool,
    closed: bool,
}

impl AutomationTake {
    /// A take recording into `recorder` (a fresh one, or
    /// `Recorder::overdub` over the lane's envelope).
    pub fn new(recorder: Recorder) -> Self {
        Self {
            recorder,
            touched: false,
            closed: false,
        }
    }

    /// A fresh `mode` take over `min..=max`.
    pub fn record(mode: RecordMode, min: f32, max: f32) -> Self {
        Self::new(Recorder::new(mode, min, max))
    }

    /// The recorder.
    pub fn recorder(&self) -> &Recorder {
        &self.recorder
    }

    /// The recorder, to touch, record and release.
    pub fn recorder_mut(&mut self) -> &mut Recorder {
        &mut self.recorder
    }

    /// Ends the take: on the next frame its envelope becomes the automation.
    pub fn close(&mut self) {
        self.closed = true;
    }

    /// Whether the take is done: closed, or a touch take released.
    fn done(&mut self) -> bool {
        if self.recorder.is_taking() {
            self.touched = true;
        }
        self.closed
            || (self.touched
                && !self.recorder.is_taking()
                && self.recorder.mode().stops_on_release())
    }
}

/// Closes the takes that are done: each one's envelope replaces its entity's
/// automation curve, and the take goes. Runs before the reconcile, so the
/// frame that closes a take republishes its finished envelope.
pub fn close_takes(
    mut commands: Commands,
    mut takes: Query<(Entity, &mut AutomationTake, Option<&mut Automation>)>,
) {
    for (entity, mut take, automation) in &mut takes {
        if !take.bypass_change_detection().done() {
            continue;
        }
        let recorder = std::mem::replace(
            &mut take.bypass_change_detection().recorder,
            Recorder::new(RecordMode::Touch, 0.0, 0.0),
        );
        let envelope: Arc<dyn Curve> = Arc::new(recorder.finish());
        if let Some(mut automation) = automation {
            automation.curve = envelope;
        }
        commands.entity(entity).remove::<AutomationTake>();
    }
}

/// Declares modulation: each call spawns an entity, and returns it.
///
/// ```rust
/// use bevy_ecs::prelude::*;
/// use bevy_tutti::modulation::{Lfo, Mods, ParamRef};
/// use tutti_types::{BeatDuration, Depth, UnitParam};
///
/// fn wobble(mut mods: Mods, filter: Single<Entity, With<Name>>) {
///     let cutoff = ParamRef::unit(*filter, UnitParam::Cutoff);
///     mods.range(cutoff, 200.0, 4_000.0);
///     let lfo = mods.lfo(Lfo::sine().rate(BeatDuration(0.5)));
///     mods.route(lfo, cutoff, Depth(0.3));
/// }
/// # let _ = wobble;
/// ```
#[derive(SystemParam)]
pub struct Mods<'w, 's> {
    commands: Commands<'w, 's>,
}

impl Mods<'_, '_> {
    /// A source of any kind.
    pub fn source(&mut self, kind: impl ModSourceKind) -> Entity {
        self.commands.spawn(ModSource::new(kind)).id()
    }

    /// An LFO.
    pub fn lfo(&mut self, lfo: Lfo) -> Entity {
        self.source(lfo)
    }

    /// A macro at `value`.
    pub fn macro_(&mut self, value: f32) -> Entity {
        self.source(Macro(value))
    }

    /// A follower of `input`'s audio, with the default times.
    pub fn follower(&mut self, input: Entity) -> Entity {
        self.source(Follower::new(input))
    }

    /// A note envelope on `midi`'s event output 0.
    pub fn note_envelope(&mut self, midi: Entity, adsr: Adsr) -> Entity {
        self.source(NoteEnvelope::new(midi, adsr))
    }

    /// A route from `source` into `target` at `depth`.
    pub fn route(&mut self, source: Entity, target: ParamRef, depth: Depth) -> Entity {
        self.commands
            .spawn(ModRoute::new(source, target).with_depth(depth))
            .id()
    }

    /// `target`'s range.
    pub fn range(&mut self, target: ParamRef, min: f32, max: f32) -> Entity {
        self.commands.spawn(ModRange::new(target, min, max)).id()
    }

    /// `target`'s automation.
    pub fn automate(&mut self, target: ParamRef, curve: Arc<dyn Curve>) -> Entity {
        self.commands.spawn(Automation::new(target, curve)).id()
    }
}
