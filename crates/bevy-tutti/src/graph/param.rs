//! One component per modulatable parameter, reconciled generically.
//!
//! [`AudioParam<U, const P: u16>`] is a scalar param on a node entity: `U` is
//! its unit, `P` its [`UnitParam`] address. Registering one with
//! [`add_audio_param`](AudioParamAppExt::add_audio_param) adds a system that
//! pushes changes into the graph — so a new param is one line, not a new
//! component type plus a `QueryData` field plus an `Or<Changed<…>>` arm plus an
//! `if let` branch.
//!
//! ```rust
//! use bevy_app::prelude::*;
//! use bevy_ecs::prelude::*;
//! use bevy_tutti::graph::{AudioGraphRes, AudioParam, AudioParamAppExt, GraphReconcilePlugin};
//! use bevy_tutti::AudioEngineState;
//! use tutti_core::dsp::{AudioUnit as _, Net};
//! use tutti_core::{AudioNode, SampleRate};
//! use tutti_types::{Drive, UnitParam};
//! use tutti_nodes::{DistortionNode, ShapeKind};
//!
//! /// One line per param — the unit and the address, both load-bearing.
//! type DriveParam = AudioParam<Drive, { UnitParam::Drive as u16 }>;
//!
//! let mut net = Net::new(0, 1);
//! let node = net.push(Box::new(DistortionNode::new(ShapeKind::Tanh, 1.0)));
//! net.set_sample_rate(SampleRate(48_000.0));
//!
//! let mut app = App::new();
//! app.insert_resource(AudioGraphRes(net));
//! app.insert_resource(AudioEngineState::Running);
//! app.add_plugins(GraphReconcilePlugin);
//! // Under the `modulation` feature the reconciler asks the matrix whether a
//! // param has a second writer, so the plugin that owns it must be present.
//! #[cfg(feature = "modulation")]
//! app.add_plugins(bevy_tutti::modulation::TuttiModulationPlugin);
//! app.add_audio_param::<Drive, { UnitParam::Drive as u16 }>();
//!
//! let entity = app.world_mut().spawn((AudioNode(node), DriveParam::new(Drive(4.0)))).id();
//! app.update();
//!
//! // Read the node's own atomic — the cell the DSP reads, not the component.
//! let graph = app.world().resource::<AudioGraphRes>();
//! let live = graph.0.node_as::<DistortionNode>(node).unwrap().drive();
//! assert_eq!(live.load(std::sync::atomic::Ordering::Acquire), 4.0);
//! ```
//!
//! # Why this can be generic at all
//!
//! Pushing a param needs no node-type dispatch: `Net::set` carries a
//! `(param, value)` pair to the addressed node, and the unit's own `set`
//! decodes it — a unit ignores params it does not own. That is the opposite of
//! resolving a *modulation target*, which needs a concrete downcast (see
//! `modulation::target`). Reconciling is uniform; resolving is not.
//!
//! # Modulated params
//!
//! A param the modulation driver owns must not be written here — modulation
//! flushes `base + Σ layers` into the same atomic every frame, so a plain write
//! is overwritten by the next flush and the fader snaps back. The reconciler
//! asks `ModulationMatrix::is_modulated` and routes the authored value to the
//! accumulator's *base* instead, which is what makes the two writers one.

use bevy_app::{App, Update};
use bevy_ecs::prelude::*;

use tutti_core::AudioNode;
// `ParamAddr` only appears in the modulation-gated arm of the reconciler, which
// is where an authored value is handed to the accumulator's base instead of
// being written straight to the node.
#[cfg(feature = "modulation")]
use tutti_types::ParamAddr;
use tutti_types::{Unit, UnitParam};

use crate::graph::{engine_ready, AudioGraphRes, GraphReconcileSystems};

/// A scalar parameter on a node entity: unit `U`, address `P`.
///
/// `P` is a [`UnitParam`] discriminant rather than the enum itself because
/// const generics cannot yet be arbitrary enums. Spell it
/// `{ UnitParam::Cutoff as u16 }` at the use site; [`param`](Self::param)
/// converts back.
///
/// Both parameters are load-bearing. `U` keeps a cutoff in `Hz` from being
/// assigned seconds, and `P` keeps two params of the same unit — a filter's
/// cutoff and an LFO's rate are both `Hz` — from being confused for one
/// another. Either alone would let one of those through.
#[derive(Component, Debug, Clone, Copy, PartialEq)]
pub struct AudioParam<U: Unit<Raw = f32>, const P: u16> {
    /// The authored value, in `U`. Written by the host; read by the reconciler,
    /// which decides where it lands (see [`write_param`]).
    pub value: U,
}

impl<U: Unit<Raw = f32>, const P: u16> AudioParam<U, P> {
    /// A param holding `value`.
    pub fn new(value: U) -> Self {
        Self { value }
    }

    /// This param's address, or `None` if `P` is not a known [`UnitParam`].
    ///
    /// `None` is unreachable for a param declared with the
    /// `{ UnitParam::X as u16 }` idiom; it stays total rather than panicking so
    /// a stale discriminant degrades to "this param never reaches the graph"
    /// instead of taking the app down.
    pub fn param(self) -> Option<UnitParam> {
        UnitParam::try_from(P).ok()
    }
}

impl<U: Unit<Raw = f32> + Default, const P: u16> Default for AudioParam<U, P> {
    fn default() -> Self {
        Self::new(U::default())
    }
}

/// Write one authored scalar to `param` on `node`, respecting modulation.
///
/// **The single home for "an authored value reaches the graph".** A param write
/// is three branches, not one, and the order is fixed:
///
/// 1. an **audio-rate** param's port is fed by a `ParamSumNode`, and a node with
///    a wired param port never reads its own atomic — so the value goes to the
///    sum's *base cell*;
/// 2. a **control-rate modulated** param has a second writer, so the value goes
///    to the accumulator's *base* and rides under the modulation;
/// 3. an **unmodulated** one goes straight to the node's own atomic.
///
/// The three are mutually exclusive by construction — `ModDelivery` is one axis,
/// and `a_per_sample_route_is_not_also_claimed_by_the_driver` pins that the
/// driver does not claim an audio-rate param. Ordering them anyway makes that
/// independent of the invariant rather than dependent on it.
///
/// Taking only the last two loses every write to an audio-rate param, and it is
/// the quietest of the three failures: the write lands on the node's atomic,
/// which is a real cell that a debugger and a `node_as` read both show holding
/// the new value — while the DSP reads the port and hears the old one.
/// `tutti-nodes`' `a_wired_param_port_makes_the_node_ignore_its_atomic` is the
/// engine-level statement of it.
///
/// Taking only the first silently drops every write to an unmodulated param —
/// which is why `ModulationMatrix::set_base` is crate-private.
///
/// Taking only the second loses every write to a *modulated* param, and the
/// loss is **deterministic rather than racy** — worth stating precisely, because
/// the shape of the failure decides how you would find it.
/// `modulation::drive` mirrors `clamp(base + Σ layers)` into the node's atomic
/// every frame, so a direct write to that cell is overwritten by the next flush
/// unconditionally. The symptom is a control that snaps back, reproducible on
/// demand.
///
/// **System ordering is not what saves this, so do not try to fix it with a
/// `.before()`.** `drive` and the param reconcilers do share
/// `GraphReconcileSystems::Params` with no ordering between them, but the two
/// writers never contend: `tutti_mod::AtomicTarget` holds a mutex-guarded
/// `LayeredCurve`, and they touch *different fields* of it — `set_base` the
/// base, `accumulate` a keyed layer — each recomputing the composite under the
/// same lock. The writes commute and both survive in either order
/// (`AtomicTarget`'s own `set_base_re_mirrors` pins exactly that). Adding an
/// ordering constraint here would buy nothing and imply a hazard that is not
/// there.
///
/// The engine tests the property that matters — that an authored write to a
/// modulated param lands on the base and therefore *survives* — in
/// `set_base_moves_a_modulated_param_without_fighting_the_driver`. This
/// function's job is to route the write to the right field; the accumulator
/// handles the rest.
///
/// # Why this is a free function and not a method on a component
///
/// [`AudioParam<U, P>`] is the statically-addressed carrier, and it is a good
/// one: `U` stops a cutoff being assigned seconds and `P` stops a filter cutoff
/// and an LFO rate — both `Hz` — being confused. But a const generic can only
/// carry an address that is a property of the **code**, and most params here are
/// a property of the **data**: a processor kind decides its key set at load
/// time, a hosted plugin at instantiation. (The same limit that removed
/// `AudioIn<S, const CH: usize>`; see `CLAUDE.md`.)
///
/// So three call sites had to route around the component, and each re-derived
/// this write — one of them without the modulation branch at all. Extracting the
/// write rather than generalising the component keeps `AudioParam`'s type safety
/// for the params that genuinely have static addresses, and gives the runtime
/// ones a door that is not a reimplementation.
///
/// **Hosted plugin parameters do not belong here.** They are runtime-discovered
/// `u32` ids reached over a different transport (`set_parameter_rt` across the
/// IPC bridge), not `Net::set` — a different write, not a different address for
/// the same one.
pub fn write_param(
    graph: &mut AudioGraphRes,
    #[cfg(feature = "modulation")] matrix: &crate::modulation::ModulationMatrix,
    #[cfg(feature = "modulation")] chains: &crate::modulation::audio_rate::AudioRateChains,
    entity: Entity,
    node: &AudioNode,
    param: UnitParam,
    value: f32,
) {
    // `Net::set` is an `AudioUnit` method; the trait must be in scope to call
    // it, and nothing else here needs it.
    use tutti_core::AudioUnit as _;

    // 1. Audio rate: the node reads its port, not its atomic.
    #[cfg(feature = "modulation")]
    if let Some(cell) = chains.base_cell(entity, ParamAddr::Unit(param)) {
        cell.store(value, tutti_core::Ordering::Release);
        return;
    }

    // 2. Control rate: the driver owns the atomic, so the value rides the base.
    #[cfg(feature = "modulation")]
    if matrix.set_base(entity, ParamAddr::Unit(param), value) {
        return;
    }
    #[cfg(not(feature = "modulation"))]
    let _ = entity;

    // 3. Unmodulated: the node's own atomic is the value.
    graph
        .0
        .set(tutti_core::unit_param::node_setting(node.0, param, value));
}

/// Push every changed [`AudioParam<U, P>`] into its node.
///
/// Change-detection-gated, so a steady frame does no work at all. Values reach
/// the audio thread through `Net::set`, which enqueues rather than mutating —
/// the RT-correct path, and the reason no downcast is needed.
///
/// The write itself is [`write_param`]'s; this system's job is the query and the
/// `P` → [`UnitParam`] conversion.
#[allow(
    clippy::type_complexity,
    reason = "Bevy queries are tuple-shaped by design"
)]
pub fn reconcile_audio_param<U: Unit<Raw = f32> + Send + Sync + 'static, const P: u16>(
    mut graph: ResMut<AudioGraphRes>,
    #[cfg(feature = "modulation")] matrix: Res<crate::modulation::ModulationMatrix>,
    #[cfg(feature = "modulation")] chains: Res<crate::modulation::audio_rate::AudioRateChains>,
    changed: Query<(Entity, &AudioNode, &AudioParam<U, P>), Changed<AudioParam<U, P>>>,
) {
    let Ok(param) = UnitParam::try_from(P) else {
        return;
    };
    for (entity, node, value) in &changed {
        write_param(
            &mut graph,
            #[cfg(feature = "modulation")]
            &matrix,
            #[cfg(feature = "modulation")]
            &chains,
            entity,
            node,
            param,
            value.value.to_raw(),
        );
    }
}

/// Which `AudioParam<U, P>` reconcilers are already scheduled.
///
/// `add_systems` does not deduplicate, so without this a param declared by both
/// a host and a library plugin would push its value twice per frame. Harmless
/// for an idempotent store, but it doubles the per-frame cost of every shared
/// param and makes the schedule depend on how many callers happened to ask.
#[derive(Resource, Default)]
struct RegisteredAudioParams(std::collections::HashSet<(core::any::TypeId, u16)>);

/// Registers the reconciler for one [`AudioParam`] type.
pub trait AudioParamAppExt {
    /// Reconcile `AudioParam<U, P>` into the graph every frame it changes.
    ///
    /// Idempotent — registering the same `(U, P)` twice schedules one system —
    /// so a host and a library plugin can both declare a param they share.
    fn add_audio_param<U: Unit<Raw = f32> + Send + Sync + 'static, const P: u16>(
        &mut self,
    ) -> &mut Self;
}

impl AudioParamAppExt for App {
    fn add_audio_param<U: Unit<Raw = f32> + Send + Sync + 'static, const P: u16>(
        &mut self,
    ) -> &mut Self {
        let key = (core::any::TypeId::of::<U>(), P);
        if !self
            .world_mut()
            .get_resource_or_init::<RegisteredAudioParams>()
            .0
            .insert(key)
        {
            return self;
        }
        self.add_systems(
            Update,
            reconcile_audio_param::<U, P>
                .in_set(GraphReconcileSystems::Params)
                .run_if(engine_ready),
        )
    }
}
