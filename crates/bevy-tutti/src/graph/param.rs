//! One component per modulatable parameter, reconciled generically.
//!
//! [`AudioParam<U, const P: u16>`] is a scalar param on a node entity: `U` is
//! its unit, `P` its [`UnitParam`] address. Registering one with
//! [`add_audio_param`](AudioParamAppExt::add_audio_param) adds a system that
//! pushes changes into the graph — so a new param is one line, not a new
//! component type plus a `QueryData` field plus an `Or<Changed<…>>` arm plus an
//! `if let` branch.
//!
//! ```rust,ignore
//! app.add_audio_param::<Hz, { UnitParam::Cutoff as u16 }>()
//!    .add_audio_param::<Q, { UnitParam::Q as u16 }>();
//!
//! commands.entity(filter).insert(Cutoff::new(Hz(1000.0)));
//! ```
//!
//! # Why this can be generic at all
//!
//! Pushing a param needs no node-type dispatch: `Net::set` carries a
//! `(param, value)` pair to the addressed node, and the unit's own `set`
//! decodes it — a unit ignores params it does not own. That is the opposite of
//! resolving a *modulation target*, which needs a concrete downcast (see
//! [`modulation::target`](crate::modulation::target)). Reconciling is uniform;
//! resolving is not.
//!
//! # Modulated params
//!
//! A param the modulation driver owns must not be written here — modulation
//! flushes `base + Σ offsets` into the same atomic every frame, so a plain
//! write would be reverted within a frame and the fader would look stuck. The
//! reconciler asks
//! [`ModulationMatrix::is_modulated`](crate::modulation::ModulationMatrix::is_modulated)
//! and routes the authored value to the accumulator's *base* instead, which is
//! what makes the two writers one.

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
    pub value: U,
}

impl<U: Unit<Raw = f32>, const P: u16> AudioParam<U, P> {
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
/// is two branches, not one, and both must be taken together:
///
/// - a **modulated** param has a second writer, so the authored value goes to
///   the accumulator's *base* and rides under the modulation;
/// - an **unmodulated** one goes straight to the node's own atomic.
///
/// Taking only the first silently drops every write to an unmodulated param —
/// which is why [`ModulationMatrix::set_base`] is crate-private.
///
/// Taking only the second is worse than it looks, and worth stating precisely
/// because a test will not show it. [`drive`](crate::modulation::drive) and the
/// param reconcilers are **both in `GraphReconcileSystems::Params` with no
/// ordering between them**, and `drive` writes `base + Σ offsets` to the same
/// atomic every frame. So a direct write to a modulated param does not merely
/// get overwritten on the *next* frame — it races the flush within the current
/// one, and which value survives depends on a system order Bevy does not
/// promise. At steady state the two branches are observationally identical,
/// which is exactly why the hazard is invisible until it is intermittent.
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
    entity: Entity,
    node: &AudioNode,
    param: UnitParam,
    value: f32,
) {
    // `Net::set` is an `AudioUnit` method; the trait must be in scope to call
    // it, and nothing else here needs it.
    use tutti_core::dsp::AudioUnit as _;

    #[cfg(feature = "modulation")]
    if matrix.set_base(entity, ParamAddr::Unit(param), value) {
        return;
    }
    #[cfg(not(feature = "modulation"))]
    let _ = entity;

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
