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

/// Push every changed [`AudioParam<U, P>`] into its node.
///
/// Change-detection-gated, so a steady frame does no work at all. Values reach
/// the audio thread through `Net::set`, which enqueues rather than mutating —
/// the RT-correct path, and the reason no downcast is needed.
#[allow(
    clippy::type_complexity,
    reason = "Bevy queries are tuple-shaped by design"
)]
pub fn reconcile_audio_param<U: Unit<Raw = f32> + Send + Sync + 'static, const P: u16>(
    mut graph: ResMut<AudioGraphRes>,
    #[cfg(feature = "modulation")] matrix: Res<crate::modulation::ModulationMatrix>,
    changed: Query<(Entity, &AudioNode, &AudioParam<U, P>), Changed<AudioParam<U, P>>>,
) {
    // `Net::set` is an `AudioUnit` method; the trait must be in scope to call
    // it, and nothing else here needs it.
    use tutti_core::dsp::AudioUnit as _;

    let Ok(param) = UnitParam::try_from(P) else {
        return;
    };
    for (entity, node, value) in &changed {
        let raw = value.value.to_raw();

        // A modulated param has a second writer. Handing the authored value to
        // the accumulator's base lets it ride *under* the modulation instead of
        // being overwritten by the next flush.
        #[cfg(feature = "modulation")]
        if matrix.set_base(entity, ParamAddr::Unit(param), raw) {
            continue;
        }
        #[cfg(not(feature = "modulation"))]
        let _ = entity;

        graph
            .0
            .set(tutti_core::unit_param::node_setting(node.0, param, raw));
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
